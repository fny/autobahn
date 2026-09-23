//! `autobahn update`: replacing the installed command and the agent
//! bundle from a published release.
//!
//! This is `scripts/install.sh` run from the inside, and it exists
//! because an upgrade of a machine that is *already running* autobahn is
//! not the same job as a first install. Three things have to be true when
//! it ends: the command on the PATH is the new one, the agent bundle
//! beside it is the new one, and the supervisor is running. The order in
//! which they are made true is the whole of this module.
//!
//! Two orderings in particular are not interchangeable:
//!
//! - The bundle is refreshed **before** the service restarts. A
//!   controller that comes back on a new version while the bundle still
//!   holds the old binaries uploads an agent named for the new version
//!   whose bytes are the old one, and every host of a platform other than
//!   this one fails its handshake until somebody notices.
//! - The binary is **renamed** into place, never written over. A running
//!   process holds the inode of the file it was started from. Writing
//!   into that file replaces the pages under a live process and kills it;
//!   renaming a new file over the name leaves the old inode alone until
//!   the service restarts on purpose. The displaced binary is kept beside
//!   the new one, because a rollback with nothing to roll back to is a
//!   machine with no autobahn on it at all.
//!
//! Nothing here is a new dependency. Downloads shell out to curl or wget
//! and checksums to sha256sum or shasum, exactly as the installer does,
//! so both paths resolve the same assets and refuse on the same grounds.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::service::{self, ServiceState};

/// The repository releases come from. The installer's `REPO`, and it must
/// stay the installer's, or an update would quietly move a machine to a
/// different publisher than the one that installed it.
const REPO: &str = "fny/autobahn";

/// The asset carrying the agent bundle.
const AGENTS_ASSET: &str = "autobahn-agents.tar.gz";

/// The asset carrying the checksums of every other asset.
const CHECKSUMS_ASSET: &str = "SHA256SUMS";

/// The platforms a release publishes a build for. A machine outside this
/// list has no asset to download, and saying so up front is better than a
/// 404 attributed to a missing release.
const PLATFORMS: [&str; 4] = [
    "linux-x86_64",
    "linux-aarch64",
    "darwin-x86_64",
    "darwin-aarch64",
];

/// What `update` was asked to do.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The release tag to install (`None` for the latest stable release).
    pub version: Option<String>,
    /// Where the command goes (`None` for `$AUTOBAHN_BIN_DIR`, else
    /// `~/.local/bin`).
    pub bin_dir: Option<PathBuf>,
    /// Leave the agent bundle alone.
    pub no_agents: bool,
    /// Report what would happen and change nothing.
    pub dry_run: bool,
}

/// Downloads a release and puts it in place, restarting the login service
/// if one is installed.
pub fn run(options: Options) -> Result<()> {
    let platform = release_platform()?;
    let bin_dir = resolve_bin_dir(options.bin_dir.clone())?;
    let state_root = resolve_state_root()?;
    let source = Source::discover(options.version.clone());
    let resolved = source.resolve_tag();

    println!(
        "autobahn {} → {} for {platform}",
        env!("CARGO_PKG_VERSION"),
        resolved.as_deref().unwrap_or("the latest release")
    );

    let installed = service::state().unwrap_or(ServiceState::NotInstalled);

    if options.dry_run {
        report_plan(
            &options,
            resolved.as_deref(),
            &platform,
            &bin_dir,
            &state_root,
            installed,
        );
        return Ok(());
    }

    let work = Workspace::new()?;

    // 1. Everything lands in a temporary directory first. Nothing that
    //    follows can be undone halfway through a download.
    let staged_binary = work.path().join(format!("autobahn-{platform}"));
    source
        .fetch(&format!("autobahn-{platform}"), &staged_binary)
        .with_context(|| format!("unable to download the {platform} build"))?;
    let staged_agents = work.path().join(AGENTS_ASSET);
    if !options.no_agents {
        source
            .fetch(AGENTS_ASSET, &staged_agents)
            .context("unable to download the agent bundle")?;
    }
    let staged_sums = work.path().join(CHECKSUMS_ASSET);
    source.fetch(CHECKSUMS_ASSET, &staged_sums).context(
        "unable to download SHA256SUMS. Nothing is installed unverified: retry, or \
         use scripts/install.sh if this release publishes no checksums",
    )?;

    // 2. Verified before anything moves. A checksum checked after the
    //    file is in place is a report, not a guard.
    let sums = std::fs::read_to_string(&staged_sums)
        .with_context(|| format!("unable to read {}", staged_sums.display()))?;
    verify(&staged_binary, &format!("autobahn-{platform}"), &sums)?;
    if !options.no_agents {
        verify(&staged_agents, AGENTS_ASSET, &sums)?;
    }
    println!("  checksums verified");

    // 3. The one check a checksum cannot make: that this binary runs
    //    *here*. A release whose assets were assembled with two platforms
    //    transposed matches its own checksums perfectly.
    let reported = run_reports_version(&staged_binary)?;
    println!("  the downloaded binary reports {reported}");

    // 4. The bundle, before the restart. A controller that comes back new
    //    while the bundle is old installs agents that fail every
    //    handshake on every host of another platform.
    if options.no_agents {
        println!("  skipped the agent bundle (--no-agents)");
    } else {
        let count = refresh_agents(&staged_agents, &state_root)?;
        println!(
            "  refreshed {count} agents in {}",
            state_root.join("agents").display()
        );
    }

    // 5. The command, by rename, with the old one kept beside it.
    let target = bin_dir.join("autobahn");
    let previous = bin_dir.join("autobahn.previous");
    place_binary(&staged_binary, &target, &previous)?;
    println!("  installed {}", target.display());

    // 6 and 7. The service, and the proof that it came back.
    restart_service(installed, &target, &previous)
}

/// Prints what a real run would do. Every line here names a decision the
/// run would make, because the point of a dry run is to check the
/// decisions, not to read a list of verbs.
fn report_plan(
    options: &Options,
    resolved: Option<&str>,
    platform: &str,
    bin_dir: &Path,
    state_root: &Path,
    installed: ServiceState,
) {
    // The resolved tag where it is known, so a dry run answers the
    // question it is asked most often: which release is "latest" today.
    let tag = resolved.unwrap_or("the latest release");
    println!("  would download autobahn-{platform} and SHA256SUMS from {REPO} ({tag})");
    if options.no_agents {
        println!("  would leave the agent bundle alone (--no-agents)");
    } else {
        println!("  would download {AGENTS_ASSET} and verify it");
        println!(
            "  would replace the agent bundle at {}",
            state_root.join("agents").display()
        );
    }
    println!(
        "  would install {}, keeping the current one at {}",
        bin_dir.join("autobahn").display(),
        bin_dir.join("autobahn.previous").display()
    );
    match installed {
        ServiceState::NotInstalled => {
            println!("  no login service is installed, so nothing would be restarted")
        }
        ServiceState::Stopped => println!("  would restart the login service (stopped now)"),
        ServiceState::Running => println!("  would restart the login service (running now)"),
    }
    println!("  nothing was changed (--dry-run)");
}

/// Restarts the login service and confirms it came back, restoring the
/// previous binary when it did not.
///
/// The confirmation is the reason this is not one line. A restart command
/// that returns successfully has said only that the service manager
/// accepted the request; a binary that exits immediately on this machine
/// (a missing library, a configuration the new version refuses) leaves
/// launchd or systemd reporting a service that is registered and dead,
/// which is exactly the state nobody notices until the next edit fails to
/// propagate.
fn restart_service(installed: ServiceState, target: &Path, previous: &Path) -> Result<()> {
    if installed == ServiceState::NotInstalled {
        println!("  no login service is installed; nothing to restart");
        println!("  run `autobahn install` to register one, or `autobahn watch` in a terminal");
        return Ok(());
    }

    let restarted = service::restart();
    if let Err(error) = &restarted {
        eprintln!("  the restart failed: {error}");
    }
    let observed = confirm_running();

    match recovery(restarted.is_ok(), observed) {
        Recovery::Keep => {
            println!("  restarted the login service");
            Ok(())
        }
        Recovery::RollBack => {
            // The previous binary is the only thing here that is known to
            // have run on this machine, so it goes back before anything
            // else is attempted.
            if !previous.exists() {
                bail!(
                    "the login service did not come back after the update, and there is no \
                     {} to restore. Fix it by hand: reinstall with scripts/install.sh, then \
                     `autobahn start`",
                    previous.display()
                );
            }
            std::fs::rename(previous, target).with_context(|| {
                format!(
                    "the login service did not come back, and restoring {} onto {} failed",
                    previous.display(),
                    target.display()
                )
            })?;
            let second = service::restart();
            let back = confirm_running();
            match (second.is_ok(), back) {
                (true, ServiceState::Running) => bail!(
                    "the login service did not come back on the new version, so the previous \
                     binary was restored at {} and the service is running again. The new \
                     version is not installed.",
                    target.display()
                ),
                _ => bail!(
                    "the login service did not come back on the new version. The previous \
                     binary was restored at {}, but the service is still not running: start \
                     it with `autobahn start` and read {}",
                    target.display(),
                    service::log_path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|_| "the service log".to_owned())
                ),
            }
        }
    }
}

/// What to do with the binary just installed, once the service manager
/// has been asked to restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recovery {
    /// The service is running on the new version. Keep it.
    Keep,
    /// The service is not running. Put the previous binary back.
    RollBack,
}

/// The rollback decision, kept separate from the restarting so it can be
/// stated as a table and tested as one.
///
/// A service that is registered but not running is a failure here even
/// though it is an ordinary state elsewhere: this code path has just
/// asked it to run. `NotInstalled` cannot normally be reached — the
/// caller returns early for it — and is treated as a failure because a
/// service that unregistered itself during a restart is not a state to
/// keep a new binary on.
fn recovery(restarted: bool, observed: ServiceState) -> Recovery {
    if !restarted {
        return Recovery::RollBack;
    }
    match observed {
        ServiceState::Running => Recovery::Keep,
        ServiceState::Stopped | ServiceState::NotInstalled => Recovery::RollBack,
    }
}

/// Waits for the service manager to report a running service.
///
/// launchd and systemd both return from a restart before the process is
/// up, so the first reading after a restart is not evidence of anything.
/// This polls for a few seconds and reports the last state it saw, so a
/// slow start is not read as a failed one.
fn confirm_running() -> ServiceState {
    let mut last = ServiceState::NotInstalled;
    for _ in 0..20 {
        last = service::state().unwrap_or(ServiceState::NotInstalled);
        if last == ServiceState::Running {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    last
}

/// Replaces the agent bundle, returning how many agents it now holds.
///
/// The archive carries a top-level `agents/` directory, so it is
/// extracted beside the destination and swapped in whole. An extraction
/// straight over the live directory would leave a half-populated bundle
/// behind any interruption, and a half-populated bundle is worse than an
/// old one: the old one installs a stale agent that fails a handshake
/// loudly, while a missing one refuses the host outright.
fn refresh_agents(archive: &Path, state_root: &Path) -> Result<usize> {
    std::fs::create_dir_all(state_root)
        .with_context(|| format!("unable to create {}", state_root.display()))?;
    // Extracted inside the state root, not in the temporary directory,
    // because the swap that follows is a rename and a rename cannot cross
    // filesystems. /tmp is very often a different one.
    let staging = state_root.join(format!(".agents.update.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("unable to create {}", staging.display()))?;

    let extracted = (|| -> Result<PathBuf> {
        let status = Command::new("tar")
            .arg("xzf")
            .arg(archive)
            .arg("-C")
            .arg(&staging)
            .status()
            .context("unable to run tar")?;
        if !status.success() {
            bail!("tar exited with {status} unpacking the agent bundle");
        }
        let extracted = staging.join("agents");
        if !extracted.is_dir() {
            bail!("the agent bundle has an unexpected layout (no agents/ directory)");
        }
        Ok(extracted)
    })();
    let extracted = match extracted {
        Ok(path) => path,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    };

    let count = std::fs::read_dir(&extracted)
        .map(|entries| entries.flatten().count())
        .unwrap_or(0);

    let agents = state_root.join("agents");
    let superseded = state_root.join("agents.previous");
    let _ = std::fs::remove_dir_all(&superseded);
    if agents.exists() {
        std::fs::rename(&agents, &superseded).with_context(|| {
            format!(
                "unable to move the current bundle aside from {}",
                agents.display()
            )
        })?;
    }
    if let Err(error) = std::fs::rename(&extracted, &agents) {
        // The bundle a controller is about to run with matters more than
        // tidiness: the old one goes back before the error is reported.
        let _ = std::fs::rename(&superseded, &agents);
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error)
            .with_context(|| format!("unable to move the new bundle into {}", agents.display()));
    }
    let _ = std::fs::remove_dir_all(&superseded);
    let _ = std::fs::remove_dir_all(&staging);
    Ok(count)
}

/// Puts the new binary at `target`, keeping whatever was there at
/// `previous`.
///
/// The new file is copied next to the target first, because the download
/// is in a temporary directory that is usually on another filesystem, and
/// then renamed. Rename is what makes this safe for a running process:
/// the old inode is untouched and stays open until the service restarts.
fn place_binary(staged: &Path, target: &Path, previous: &Path) -> Result<()> {
    let directory = target
        .parent()
        .ok_or_else(|| anyhow!("{} has no directory", target.display()))?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("unable to create {}", directory.display()))?;

    let incoming = directory.join(format!(".autobahn.update.{}", std::process::id()));
    std::fs::copy(staged, &incoming)
        .with_context(|| format!("unable to write {}", incoming.display()))?;
    if let Err(error) = make_executable(&incoming) {
        let _ = std::fs::remove_file(&incoming);
        return Err(error);
    }

    // `symlink_metadata`, so a directory entry that is a symlink (a
    // hand-managed install, or a development tree) is kept rather than
    // followed and missed.
    if std::fs::symlink_metadata(target).is_ok() {
        let _ = std::fs::remove_file(previous);
        std::fs::rename(target, previous).with_context(|| {
            format!(
                "unable to keep the current binary at {}",
                previous.display()
            )
        })?;
    }
    if let Err(error) = std::fs::rename(&incoming, target) {
        // Nothing is left half-installed: the old binary goes back under
        // its own name before this returns.
        let _ = std::fs::rename(previous, target);
        let _ = std::fs::remove_file(&incoming);
        return Err(error)
            .with_context(|| format!("unable to move the new binary into {}", target.display()));
    }
    Ok(())
}

/// Makes a staged binary executable by its owner and readable by anyone,
/// which is what the installer's `chmod 755` means.
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("unable to make {} executable", path.display()))
}

/// Runs a downloaded binary and returns the version it reports.
///
/// This is the check a checksum cannot make. A release whose assets were
/// assembled with two platforms transposed, or an asset renamed by hand,
/// matches its own SHA256SUMS exactly and still cannot run here.
fn run_reports_version(binary: &Path) -> Result<String> {
    make_executable(binary)?;
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("unable to run the downloaded binary {}", binary.display()))?;
    let reported = String::from_utf8_lossy(&output.stdout);
    match (output.status.success(), reported_version(&reported)) {
        (true, Some(version)) => Ok(version.to_owned()),
        _ => {
            let complaint = String::from_utf8_lossy(&output.stderr);
            let complaint = complaint.trim();
            bail!(
                "the downloaded binary does not run on this machine{}. Nothing was installed. \
                 The download matched its checksum, so this is a release that published the \
                 wrong asset under this platform's name.",
                match complaint.is_empty() {
                    true => String::new(),
                    false => format!(": {complaint}"),
                }
            )
        }
    }
}

/// Picks the version out of what `autobahn --version` prints.
///
/// Deliberately loose about the rest of the line: the point is to tell a
/// binary that ran from one that did not, and a released binary that
/// prints its name differently is not a reason to refuse an update.
fn reported_version(output: &str) -> Option<&str> {
    output.split_whitespace().find(|token| {
        token.starts_with(|character: char| character.is_ascii_digit()) && token.contains('.')
    })
}

/// Verifies one downloaded file against the release's checksums.
fn verify(file: &Path, name: &str, sums: &str) -> Result<()> {
    let expected = expected_checksum(sums, name)
        .ok_or_else(|| anyhow!("SHA256SUMS has no entry for {name}"))?;
    let actual = checksum(file)?;
    if expected != actual {
        bail!(
            "checksum mismatch for {name}\n  expected {expected}\n  actual   {actual}\n\
             Refusing to install. Retry, or report this."
        );
    }
    Ok(())
}

/// Finds one asset's expected checksum in a `SHA256SUMS` file.
///
/// The format is one entry per line, `<hex>  <name>`, and the name is
/// matched whole: `autobahn-linux-x86_64` must never be satisfied by a
/// line for `autobahn-linux-x86_64-debug`. The binary marker (`*name`,
/// which `sha256sum -b` writes) is accepted because the file is produced
/// by whatever ran on the release runner, not by us.
fn expected_checksum<'a>(sums: &'a str, name: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (digest, entry) = line.split_once(char::is_whitespace)?;
        let entry = entry.trim().trim_start_matches('*');
        match entry == name && is_sha256(digest) {
            true => Some(digest),
            false => None,
        }
    })
}

/// Whether a token is a SHA-256 digest in the form these files use.
///
/// Checked rather than assumed, so a line of prose in a malformed
/// SHA256SUMS cannot become an "expected" value that some later file
/// coincidentally fails against with a confusing message.
fn is_sha256(token: &str) -> bool {
    token.len() == 64 && token.chars().all(|character| character.is_ascii_hexdigit())
}

/// Computes a file's SHA-256, using whichever of the two standard tools
/// this machine has. The installer makes the same choice, so both refuse
/// on the same machines rather than one silently skipping the check.
fn checksum(file: &Path) -> Result<String> {
    let (program, arguments): (&str, &[&str]) = if have("sha256sum") {
        ("sha256sum", &[])
    } else if have("shasum") {
        ("shasum", &["-a", "256"])
    } else {
        bail!("neither sha256sum nor shasum is available, so nothing can be verified");
    };
    let output = Command::new(program)
        .args(arguments)
        .arg(file)
        .output()
        .with_context(|| format!("unable to run {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} exited with {} for {}",
            output.status,
            file.display()
        );
    }
    let reported = String::from_utf8_lossy(&output.stdout);
    parse_checksum_output(&reported)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("unable to read a digest out of {program}'s output {reported:?}"))
}

/// Reads the digest out of what `sha256sum` or `shasum -a 256` prints.
fn parse_checksum_output(output: &str) -> Option<&str> {
    let token = output.split_whitespace().next()?;
    is_sha256(token).then_some(token)
}

/// Where the release assets come from, and how they are fetched.
///
/// The GitHub CLI is used when it is present and logged in, and the plain
/// release URL otherwise. This is the installer's rule, and it matters
/// for the same reason: `gh` is what reaches a private repository, and
/// the plain URL is what needs no credentials at all.
struct Source {
    /// The tag to install, or `None` for the latest release.
    tag: Option<String>,
    /// Whether `gh` is present and authenticated.
    gh: bool,
}

impl Source {
    fn discover(tag: Option<String>) -> Self {
        let gh = have("gh")
            && Command::new("gh")
                .args(["auth", "status"])
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false);
        Self { tag, gh }
    }

    /// The base URL for plain downloads.
    ///
    /// With no tag this is `releases/latest/download`, which GitHub
    /// points at the last release *not* marked as a prerelease. That is
    /// what keeps a `-dev` or `-rc` build off every machine that did not
    /// ask for it by tag.
    fn base(&self) -> String {
        match &self.tag {
            Some(tag) => format!("https://github.com/{REPO}/releases/download/{tag}"),
            None => format!("https://github.com/{REPO}/releases/latest/download"),
        }
    }

    /// The tag being installed, resolved to a real name where that is
    /// possible without guessing.
    ///
    /// Only reported, never used to build a URL: resolving "latest" and
    /// then downloading that tag would open a window in which a release
    /// published between the two steps changes what is installed.
    fn resolve_tag(&self) -> Option<String> {
        if self.tag.is_some() {
            return self.tag.clone();
        }
        if self.gh {
            let output = Command::new("gh")
                .args([
                    "release", "view", "--repo", REPO, "--json", "tagName", "-q", ".tagName",
                ])
                .output()
                .ok()?;
            if output.status.success() {
                let tag = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if !tag.is_empty() {
                    return Some(tag);
                }
            }
            return None;
        }
        if !have("curl") {
            return None;
        }
        // The latest-release URL redirects to the release's own page, and
        // that page's address carries the tag. A HEAD request, so nothing
        // is downloaded to learn a name.
        let output = Command::new("curl")
            .args(["-fsSLI", "-o", "/dev/null", "-w", "%{url_effective}"])
            .arg(format!("https://github.com/{REPO}/releases/latest"))
            .output()
            .ok()?;
        match output.status.success() {
            true => tag_from_release_url(String::from_utf8_lossy(&output.stdout).trim())
                .map(str::to_owned),
            false => None,
        }
    }

    /// Downloads one asset to a path.
    fn fetch(&self, asset: &str, destination: &Path) -> Result<()> {
        if self.gh {
            let mut command = Command::new("gh");
            command.arg("release").arg("download");
            if let Some(tag) = &self.tag {
                command.arg(tag);
            }
            command
                .args(["--repo", REPO, "--pattern", asset, "--output"])
                .arg(destination)
                .arg("--clobber");
            let output = command.output().context("unable to run gh")?;
            if output.status.success() {
                return Ok(());
            }
            let complaint = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            bail!(match complaint.is_empty() {
                true => format!("gh could not download {asset}"),
                false => format!("gh could not download {asset}: {complaint}"),
            });
        }
        let url = format!("{}/{asset}", self.base());
        // --fail (curl) and the default for wget, so an HTML error page
        // never lands in a file that is about to be checksummed and run.
        let (program, arguments): (&str, Vec<String>) = if have("curl") {
            (
                "curl",
                vec![
                    "-fsSL".to_owned(),
                    "-o".to_owned(),
                    destination.to_string_lossy().into_owned(),
                    url.clone(),
                ],
            )
        } else if have("wget") {
            (
                "wget",
                vec![
                    "-q".to_owned(),
                    "-O".to_owned(),
                    destination.to_string_lossy().into_owned(),
                    url.clone(),
                ],
            )
        } else {
            bail!("neither curl nor wget is available");
        };
        let output = Command::new(program)
            .args(&arguments)
            .output()
            .with_context(|| format!("unable to run {program}"))?;
        if !output.status.success() {
            let _ = std::fs::remove_file(destination);
            let complaint = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            bail!(match complaint.is_empty() {
                true => format!("{program} could not download {url}"),
                false => format!("{program} could not download {url}: {complaint}"),
            });
        }
        Ok(())
    }
}

/// Reads the tag out of a release page's address.
fn tag_from_release_url(url: &str) -> Option<&str> {
    let tag = url.rsplit_once("/releases/tag/")?.1;
    let tag = tag.split(['?', '#']).next()?.trim_end_matches('/');
    match tag.is_empty() {
        true => None,
        false => Some(tag),
    }
}

/// A temporary directory that removes itself.
///
/// Its own type rather than a bare path, so every early return in `run`
/// takes the downloads with it. `tempfile` is a development dependency
/// and is deliberately not made a shipping one for this.
struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "autobahn-update-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path)
            .with_context(|| format!("unable to create {}", path.display()))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether a program is on the PATH, which is `command -v` without a
/// shell.
fn have(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        let candidate = directory.join(program);
        candidate
            .metadata()
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// This machine's platform in release asset naming.
fn release_platform() -> Result<String> {
    let platform = platform_name(std::env::consts::OS, std::env::consts::ARCH);
    if !PLATFORMS.contains(&platform.as_str()) {
        bail!(
            "no release build for {platform} (builds exist for {}). Build from source instead.",
            PLATFORMS.join(", ")
        );
    }
    Ok(platform)
}

/// Maps Rust's own naming to the release's.
///
/// Rust names the operating system after the vendor ("macos") where the
/// release names the kernel ("darwin"), the same translation
/// `transport::install` makes for `uname -s`. Both namings have to agree
/// or a Mac downloads an asset that was never published.
fn platform_name(os: &str, arch: &str) -> String {
    let os = match os {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{arch}")
}

/// Where the command goes: the flag, then `AUTOBAHN_BIN_DIR`, then
/// `~/.local/bin` — the installer's order, so an update lands where the
/// install did. `AUTOBAHN_PREFIX` is the same variable under its earlier
/// name, and the installer honours it too.
fn resolve_bin_dir(bin_dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(bin_dir) = bin_dir {
        return Ok(bin_dir);
    }
    for variable in ["AUTOBAHN_BIN_DIR", "AUTOBAHN_PREFIX"] {
        if let Ok(bin_dir) = std::env::var(variable) {
            if !bin_dir.is_empty() {
                return Ok(PathBuf::from(bin_dir));
            }
        }
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local").join("bin"))
}

/// Where the agent bundle goes: the state root, which is `AUTOBAHN_HOME`
/// when the installer was pointed there and everything autobahn owns
/// otherwise — the binary reads the same variable, so the two agree.
fn resolve_state_root() -> Result<PathBuf> {
    crate::paths::default_state_root()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name is matched whole. A prefix match would accept the entry
    /// for a differently suffixed asset, and the whole point of the
    /// checksum is that it belongs to the file being installed.
    #[test]
    fn a_checksum_entry_belongs_to_exactly_its_own_asset() {
        let sums = "\
1111111111111111111111111111111111111111111111111111111111111111  autobahn-linux-x86_64-debug
2222222222222222222222222222222222222222222222222222222222222222  autobahn-linux-x86_64
3333333333333333333333333333333333333333333333333333333333333333  autobahn-agents.tar.gz
";
        assert_eq!(
            expected_checksum(sums, "autobahn-linux-x86_64"),
            Some("2222222222222222222222222222222222222222222222222222222222222222")
        );
        assert_eq!(
            expected_checksum(sums, "autobahn-agents.tar.gz"),
            Some("3333333333333333333333333333333333333333333333333333333333333333")
        );
        assert_eq!(expected_checksum(sums, "autobahn-darwin-aarch64"), None);
    }

    /// `sha256sum` writes two spaces, `shasum` writes two, and the binary
    /// form writes `*name`. All three are the same release file to us.
    #[test]
    fn the_usual_checksum_spellings_all_parse() {
        let two_spaces =
            "4444444444444444444444444444444444444444444444444444444444444444  autobahn-darwin-aarch64";
        let one_space =
            "4444444444444444444444444444444444444444444444444444444444444444 autobahn-darwin-aarch64";
        let binary =
            "4444444444444444444444444444444444444444444444444444444444444444 *autobahn-darwin-aarch64";
        for sums in [two_spaces, one_space, binary] {
            assert_eq!(
                expected_checksum(sums, "autobahn-darwin-aarch64"),
                Some("4444444444444444444444444444444444444444444444444444444444444444"),
                "{sums}"
            );
        }
    }

    /// A malformed file must produce "no entry", not an expected value
    /// that every real file then fails against.
    #[test]
    fn prose_in_a_checksum_file_is_not_a_checksum() {
        let sums = "\
Not found: autobahn-linux-x86_64
short  autobahn-linux-x86_64
zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz  autobahn-linux-x86_64
";
        assert_eq!(expected_checksum(sums, "autobahn-linux-x86_64"), None);
    }

    #[test]
    fn a_digest_is_read_out_of_either_tool_s_output() {
        let sha256sum =
            "5555555555555555555555555555555555555555555555555555555555555555  /tmp/autobahn\n";
        let shasum =
            "5555555555555555555555555555555555555555555555555555555555555555  /tmp/autobahn\n";
        for output in [sha256sum, shasum] {
            assert_eq!(
                parse_checksum_output(output),
                Some("5555555555555555555555555555555555555555555555555555555555555555")
            );
        }
        assert_eq!(parse_checksum_output(""), None);
        assert_eq!(parse_checksum_output("shasum: no such file\n"), None);
    }

    /// The verification refuses on a mismatch and on a missing entry
    /// alike. A missing entry is not "nothing to check": it is a release
    /// that did not publish a checksum for what is about to be installed.
    #[test]
    fn verification_refuses_a_mismatch_and_a_missing_entry() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let file = directory.path().join("autobahn-linux-x86_64");
        std::fs::write(&file, b"content").expect("writes");

        let wrong = "\
0000000000000000000000000000000000000000000000000000000000000000  autobahn-linux-x86_64
";
        let error = verify(&file, "autobahn-linux-x86_64", wrong)
            .expect_err("a wrong checksum must refuse");
        assert!(format!("{error}").contains("checksum mismatch"), "{error}");

        let absent = "\
0000000000000000000000000000000000000000000000000000000000000000  autobahn-agents.tar.gz
";
        let error =
            verify(&file, "autobahn-linux-x86_64", absent).expect_err("a missing entry refuses");
        assert!(format!("{error}").contains("no entry"), "{error}");
    }

    /// The same file verifies against the digest the machine's own tool
    /// computes, which is the only way to know the two halves agree.
    #[test]
    fn a_file_verifies_against_its_own_digest() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let file = directory.path().join("autobahn-linux-x86_64");
        std::fs::write(&file, b"content").expect("writes");
        let digest = checksum(&file).expect("a digest");
        let sums = format!("{digest}  autobahn-linux-x86_64\n");
        verify(&file, "autobahn-linux-x86_64", &sums).expect("its own digest verifies");
    }

    /// The release publishes `darwin-*`, and Rust calls the same machine
    /// `macos`. Without the translation a Mac asks for an asset that was
    /// never published.
    #[test]
    fn platform_names_follow_the_release_naming() {
        assert_eq!(platform_name("macos", "aarch64"), "darwin-aarch64");
        assert_eq!(platform_name("macos", "x86_64"), "darwin-x86_64");
        assert_eq!(platform_name("linux", "x86_64"), "linux-x86_64");
        assert_eq!(platform_name("linux", "aarch64"), "linux-aarch64");
    }

    /// Every platform this binary can be built for and support must have
    /// a published asset, or `update` is a command that cannot run on a
    /// machine autobahn runs on.
    #[test]
    fn this_machine_has_a_published_build() {
        let platform = release_platform().expect("a supported platform");
        assert!(PLATFORMS.contains(&platform.as_str()), "{platform}");
    }

    /// The names here and the names `transport::install` probes for are
    /// one namespace: the bundle downloaded by `update` is the bundle
    /// that module searches.
    #[test]
    fn the_update_naming_matches_the_agent_bundle_naming() {
        let bundled = format!(
            "autobahn-{}",
            platform_name(std::env::consts::OS, std::env::consts::ARCH)
        );
        assert!(
            PLATFORMS
                .iter()
                .any(|platform| bundled == format!("autobahn-{platform}")),
            "{bundled}"
        );
    }

    #[test]
    fn a_version_is_read_out_of_what_the_binary_prints() {
        assert_eq!(reported_version("autobahn 0.4.0\n"), Some("0.4.0"));
        assert_eq!(
            reported_version("autobahn 0.5.0-dev.1\n"),
            Some("0.5.0-dev.1")
        );
        // Not a run at all: the shell's complaint about an unrunnable
        // file, and a truncated download that is not an executable.
        assert_eq!(reported_version(""), None);
        assert_eq!(reported_version("cannot execute binary file"), None);
    }

    /// The rollback table. A restart the service manager refused, and a
    /// restart it accepted onto a binary that then exited, are the same
    /// outcome for the machine: no supervisor.
    #[test]
    fn a_service_that_did_not_come_back_is_rolled_back() {
        assert_eq!(recovery(true, ServiceState::Running), Recovery::Keep);
        assert_eq!(recovery(true, ServiceState::Stopped), Recovery::RollBack);
        assert_eq!(
            recovery(true, ServiceState::NotInstalled),
            Recovery::RollBack
        );
        assert_eq!(recovery(false, ServiceState::Stopped), Recovery::RollBack);
        // A restart that failed is a rollback even if the service manager
        // reports a running service: what is running is the old process
        // the restart never replaced, and keeping a new binary on that
        // reading would arm the *next* restart to fail with nobody
        // watching.
        assert_eq!(recovery(false, ServiceState::Running), Recovery::RollBack);
    }

    /// The previous binary is kept by rename, so the inode a running
    /// process holds is never written through.
    #[test]
    fn installing_keeps_the_previous_binary_and_replaces_by_rename() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let bin_dir = directory.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("directories");
        let target = bin_dir.join("autobahn");
        let previous = bin_dir.join("autobahn.previous");
        std::fs::write(&target, b"the old binary").expect("writes");
        let live = std::fs::metadata(&target).expect("metadata");

        let staged = directory.path().join("downloaded");
        std::fs::write(&staged, b"the new binary").expect("writes");

        place_binary(&staged, &target, &previous).expect("installs");

        assert_eq!(
            std::fs::read(&target).expect("reads"),
            b"the new binary",
            "the new binary is what the name resolves to"
        );
        assert_eq!(
            std::fs::read(&previous).expect("reads"),
            b"the old binary",
            "and the old one is still there to roll back to"
        );
        // The inode a running process would be holding still carries the
        // old content, which is the whole reason this is a rename.
        use std::os::unix::fs::MetadataExt;
        let kept = std::fs::metadata(&previous).expect("metadata");
        assert_eq!(
            live.ino(),
            kept.ino(),
            "the old inode was moved, not written"
        );
    }

    /// A first install has nothing to keep, and must not refuse for it.
    #[test]
    fn installing_where_nothing_is_installed_works() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let bin_dir = directory.path().join("bin");
        let target = bin_dir.join("autobahn");
        let previous = bin_dir.join("autobahn.previous");
        let staged = directory.path().join("downloaded");
        std::fs::write(&staged, b"the new binary").expect("writes");

        place_binary(&staged, &target, &previous).expect("installs");
        assert_eq!(std::fs::read(&target).expect("reads"), b"the new binary");
        assert!(!previous.exists(), "nothing was displaced");
    }

    #[test]
    fn a_tag_is_read_out_of_a_release_page_address() {
        assert_eq!(
            tag_from_release_url("https://github.com/fny/autobahn/releases/tag/v0.4.0"),
            Some("v0.4.0")
        );
        assert_eq!(
            tag_from_release_url("https://github.com/fny/autobahn/releases/tag/v0.5.0-dev.1"),
            Some("v0.5.0-dev.1")
        );
        assert_eq!(
            tag_from_release_url("https://github.com/fny/autobahn/releases"),
            None
        );
    }

    /// The explicit tag is the tag, and the base URL says so. With no
    /// tag the base is `latest/download`, which GitHub resolves to the
    /// last release not marked as a prerelease — the property the release
    /// workflow's `--prerelease` exists to preserve.
    #[test]
    fn the_download_base_follows_the_requested_tag() {
        let pinned = Source {
            tag: Some("v0.5.0-dev.1".to_owned()),
            gh: false,
        };
        assert!(pinned.base().ends_with("/releases/download/v0.5.0-dev.1"));
        assert_eq!(pinned.resolve_tag().as_deref(), Some("v0.5.0-dev.1"));

        let latest = Source {
            tag: None,
            gh: false,
        };
        assert!(latest.base().ends_with("/releases/latest/download"));
    }
}
