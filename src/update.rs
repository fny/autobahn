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
//!   the service restarts on purpose. The displaced binary and bundle are
//!   kept beside the new ones until the service is confirmed running the
//!   new build, because a rollback with nothing to roll back to is a
//!   machine with no autobahn on it at all.
//!
//! Nothing here is a new dependency. Downloads shell out to curl or wget
//! and checksums to sha256sum or shasum, exactly as the installer does,
//! so both paths resolve the same assets and refuse on the same grounds.

use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    /// Point a login service registered at another executable at the one
    /// this installs.
    pub retarget: bool,
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

    let places = Places {
        platform,
        bin_dir,
        state_root,
    };
    install(&options, resolved.as_deref(), &places, &source, &Installed)
}

/// Where a run puts things.
struct Places {
    /// This machine's platform, in release asset naming.
    platform: String,
    /// Where the command goes.
    bin_dir: PathBuf,
    /// Where the agent bundle goes, and the downloads meanwhile.
    state_root: PathBuf,
}

/// Where release assets come from: the release itself in a real run, a
/// directory in a test.
trait Fetch {
    /// Downloads one asset to a path.
    fn fetch(&self, asset: &str, destination: &Path) -> Result<()>;
}

/// The login service, as an update sees it: the real one in a real run,
/// a stub in a test.
trait Service {
    /// Whether one is installed, and running.
    fn state(&self) -> ServiceState;
    /// What it is registered to run.
    fn registration(&self) -> Result<Option<service::Registration>>;
    /// Points it at another executable.
    fn retarget(&self, executable: &Path) -> Result<()>;
    /// Asks the service manager to restart it.
    fn restart(&self) -> Result<()>;
    /// Waits for it to report running, and says what it last reported.
    fn wait_running(&self) -> ServiceState;
    /// Which build the supervisor answering under `state_root` is, given
    /// that the one wanted reports `reported`.
    fn running_build(&self, state_root: &Path, reported: &str) -> RunningBuild;
}

/// The build a running supervisor turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RunningBuild {
    /// This build: it answered this command's request.
    This,
    /// Another build, which said which (a `protocol::version()`).
    Other(String),
    /// Another build, from before builds were compared, which cannot say.
    Unknown,
    /// Nothing answered.
    Absent,
}

/// The login service this machine has.
struct Installed;

impl Service for Installed {
    fn state(&self) -> ServiceState {
        service::state().unwrap_or(ServiceState::NotInstalled)
    }

    fn registration(&self) -> Result<Option<service::Registration>> {
        service::registration()
    }

    fn retarget(&self, executable: &Path) -> Result<()> {
        service::retarget(executable)
    }

    fn restart(&self) -> Result<()> {
        service::restart()
    }

    /// launchd and systemd both return from a restart before the process
    /// is up, so the first reading after a restart is not evidence of
    /// anything. This polls for a few seconds and reports the last state
    /// it saw, so a slow start is not read as a failed one.
    fn wait_running(&self) -> ServiceState {
        let mut last = ServiceState::NotInstalled;
        for _ in 0..20 {
            last = self.state();
            if last == ServiceState::Running {
                return last;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        last
    }

    /// Asked over the control socket, which a supervisor opens a moment
    /// after it starts: polled for a few seconds before it counts as
    /// absent. An answer from a build other than the one wanted is polled
    /// past too, for as long, in case it is the old process still on its
    /// way out (launchd's `kickstart -k` returns before it has gone); it
    /// is the answer only if nothing replaces it.
    fn running_build(&self, state_root: &Path, reported: &str) -> RunningBuild {
        use crate::supervisor::control::{probe, Probe};
        let mut last = RunningBuild::Absent;
        for _ in 0..40 {
            let build = match probe(state_root) {
                Probe::Answered(_) => RunningBuild::This,
                Probe::Mismatch(Some(version)) => RunningBuild::Other(version),
                Probe::Mismatch(None) => RunningBuild::Unknown,
                // A wedged supervisor cannot say which build it is: polled
                // past, like a socket nobody listens on.
                Probe::Unresponsive | Probe::Absent => RunningBuild::Absent,
            };
            match build_matches(&build, reported) {
                Some(true) => return build,
                None if build == RunningBuild::Unknown => return build,
                _ => {}
            }
            if build != RunningBuild::Absent {
                last = build;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        last
    }
}

/// Everything `run` does once it knows where things go.
fn install(
    options: &Options,
    resolved: Option<&str>,
    places: &Places,
    source: &dyn Fetch,
    service: &dyn Service,
) -> Result<()> {
    let Places {
        platform,
        bin_dir,
        state_root,
    } = places;
    let target = bin_dir.join("autobahn");
    let previous = bin_dir.join("autobahn.previous");
    let installed = service.state();

    // 0. What the service runs. Replacing a file the service does not run
    //    and then restarting it restarts the old version, and the update
    //    would report success with nothing changed.
    let registration = match installed {
        ServiceState::NotInstalled => None,
        _ => service.registration()?,
    };
    if installed != ServiceState::NotInstalled && registration.is_none() {
        println!(
            "  warning: unable to read which executable the login service runs, so it cannot \
             be checked against {}",
            target.display()
        );
    }
    let retarget_from = match &registration {
        Some(registration) if !same_entry(&registration.executable, &target) => {
            if !options.retarget {
                bail!(
                    "the login service runs {}, not {}, so updating {} would leave it on the \
                     old version. Run `autobahn install` from the new binary, or pass \
                     --retarget to point the service at {}. Nothing was changed.",
                    registration.executable.display(),
                    target.display(),
                    target.display(),
                    target.display()
                );
            }
            Some(registration.executable.clone())
        }
        _ => None,
    };

    if options.dry_run {
        report_plan(
            options,
            resolved,
            platform,
            bin_dir,
            state_root,
            installed,
            retarget_from.as_deref(),
        );
        return Ok(());
    }

    let work = workspace(state_root)?;

    // 1. Everything lands in a private temporary directory first. Nothing
    //    that follows can be undone halfway through a download, and
    //    nobody else can reach what is downloaded.
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
        "unable to download SHA256SUMS, so nothing can be verified and nothing was \
         installed. Retry. Only an old release publishes no checksums; installing one \
         means `scripts/install.sh --insecure --version <tag>`, and nothing then checks \
         that its files are the ones that were published",
    )?;

    // 2. Verified before anything moves. A checksum checked after the
    //    file is in place is a report, not a guard. Each download is
    //    opened once, and everything after this reads that handle: the
    //    bytes installed are the bytes checked, whatever happens to the
    //    name in the meantime.
    let sums = std::fs::read_to_string(&staged_sums)
        .with_context(|| format!("unable to read {}", staged_sums.display()))?;
    let binary = Verified::open(&staged_binary, &format!("autobahn-{platform}"), &sums)?;
    let agents = match options.no_agents {
        true => None,
        false => Some(Verified::open(&staged_agents, AGENTS_ASSET, &sums)?),
    };
    println!("  checksums verified");

    // 3. The verified bytes, copied beside the target, where the rename
    //    that installs them happens. Everything that runs the new build
    //    before then runs this copy, never the download.
    let incoming = Incoming::stage(&binary, &target)?;
    drop(binary);

    // 3a. The one check a checksum cannot make: that this binary runs
    //     *here*. A release whose assets were assembled with two platforms
    //     transposed matches its own checksums perfectly.
    let reported = run_reports_version(incoming.path())?;
    println!("  the downloaded binary reports {reported}");

    // 3b. Whether it can read every session's baseline. A build that
    //     cannot rebuilds one from the two sides, which is safe only where
    //     they already match — so where they might not, stop here, before
    //     anything is replaced, and say which to settle.
    check_baselines(incoming.path(), state_root)?;

    // 4. The bundle, before the restart. A controller that comes back new
    //    while the bundle is old installs agents that fail every
    //    handshake on every host of another platform. The old bundle is
    //    kept until the new version is confirmed, because a rollback to
    //    the old binary with the new bundle is the same failure reversed.
    let mut undo = Undo {
        target: target.clone(),
        previous: previous.clone(),
        kept_binary: false,
        bundle: None,
        retarget_from: None,
    };
    match &agents {
        None => println!("  skipped the agent bundle (--no-agents)"),
        Some(agents) => {
            let (count, bundle) = refresh_agents(agents, state_root)?;
            undo.bundle = Some(bundle);
            println!(
                "  refreshed {count} agents in {}",
                state_root.join("agents").display()
            );
        }
    }

    // 5. The command, by rename, with the old one kept beside it.
    match incoming.place(&target, &previous) {
        Ok(kept) => undo.kept_binary = kept,
        Err(error) => {
            undo.restore_bundle();
            return Err(error);
        }
    }
    println!("  installed {}", target.display());

    // 5b. The service, pointed at what was just installed, when it ran
    //     something else and --retarget said to.
    if let Some(from) = retarget_from {
        if let Err(error) = service.retarget(&target) {
            // The definition may have been written before what failed: it
            // goes back to what it named, with the files.
            undo.retarget_from = Some(from.clone());
            if let Err(undone) = undo.roll_back_files() {
                eprintln!("  {undone:#}");
            } else if let Err(back) = service.retarget(&from) {
                eprintln!(
                    "  unable to point the login service back at {}: {back:#}",
                    from.display()
                );
            }
            return Err(error).context(
                "unable to point the login service at the new binary, so the previous files \
                 were restored",
            );
        }
        undo.retarget_from = Some(from);
        println!("  pointed the login service at {}", target.display());
    }

    // 6 and 7. The service, and the proof that it came back on this build.
    let probe_root = registration
        .as_ref()
        .and_then(service::Registration::state_root)
        .unwrap_or_else(|| state_root.clone());
    restart_service(installed, service, &reported, &probe_root, undo)
}

/// Whether a service registered to run `registered` runs whatever is put
/// at `target`: the same directory entry, once the directories on the way
/// are resolved, or a link to it. A registered path that merely resolves
/// to the same file today does not count when `target` is itself a link,
/// because the update replaces the link and not the file.
fn same_entry(registered: &Path, target: &Path) -> bool {
    if registered == target {
        return true;
    }
    let entry = |path: &Path| -> Option<PathBuf> {
        let parent = path.parent()?;
        let parent = match parent.as_os_str().is_empty() {
            true => Path::new("."),
            false => parent,
        };
        Some(parent.canonicalize().ok()?.join(path.file_name()?))
    };
    let Some(target) = entry(target) else {
        return false;
    };
    entry(registered).as_ref() == Some(&target)
        || registered.canonicalize().ok().as_ref() == Some(&target)
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
    retarget_from: Option<&Path>,
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
        "  would install {}, keeping the current one at {} until the new one is confirmed",
        bin_dir.join("autobahn").display(),
        bin_dir.join("autobahn.previous").display()
    );
    if let Some(from) = retarget_from {
        println!(
            "  would point the login service at {} (it runs {} now)",
            bin_dir.join("autobahn").display(),
            from.display()
        );
    }
    match installed {
        ServiceState::NotInstalled => {
            println!("  no login service is installed, so nothing would be restarted")
        }
        ServiceState::Stopped => println!("  would restart the login service (stopped now)"),
        ServiceState::Running => println!("  would restart the login service (running now)"),
    }
    println!("  nothing was changed (--dry-run)");
}

/// What an update has replaced, and how to put it back.
struct Undo {
    /// Where the new binary is.
    target: PathBuf,
    /// Where the binary it replaced is kept.
    previous: PathBuf,
    /// Whether this run put a binary at `previous`. A file there from an
    /// earlier run is nothing to restore.
    kept_binary: bool,
    /// The bundle this run replaced, when it replaced one.
    bundle: Option<Bundle>,
    /// The executable the service ran before this run pointed it at
    /// `target`.
    retarget_from: Option<PathBuf>,
}

impl Undo {
    /// Puts the previous bundle back, reporting (not raising) a failure:
    /// for a new binary that never got as far as being installed.
    fn restore_bundle(&mut self) {
        if let Some(bundle) = self.bundle.take() {
            if let Err(error) = bundle.restore() {
                eprintln!("  {error:#}");
            }
        }
    }

    /// Puts the previous bundle and binary back, bundle first: a
    /// controller that starts on the old binary must find the old agents.
    /// A bundle that cannot be put back stops the rollback there, with
    /// the new bundle in place, so the binary and the bundle still match.
    fn roll_back_files(&mut self) -> Result<()> {
        if let Some(bundle) = self.bundle.take() {
            bundle.restore().context(
                "the previous agent bundle could not be restored, so the new binary and \
                 bundle were left in place",
            )?;
        }
        if self.kept_binary {
            std::fs::rename(&self.previous, &self.target).with_context(|| {
                format!(
                    "restoring {} onto {} failed",
                    self.previous.display(),
                    self.target.display()
                )
            })?;
            self.kept_binary = false;
        } else if self.retarget_from.is_some() {
            // Nothing was here before, and the service goes back to the
            // executable it ran, which this run never touched.
            let _ = std::fs::remove_file(&self.target);
        } else {
            bail!(
                "there is no {} to restore. Fix it by hand: reinstall with scripts/install.sh, \
                 then `autobahn start`",
                self.previous.display()
            );
        }
        Ok(())
    }

    /// The new version is confirmed: what it replaced goes.
    fn commit(self) {
        if self.kept_binary {
            let _ = std::fs::remove_file(&self.previous);
        }
        if let Some(bundle) = self.bundle {
            bundle.discard();
        }
    }
}

/// Restarts the login service and confirms it came back on the new build,
/// restoring the previous binary and bundle when it did not.
///
/// The confirmation is the reason this is not one line. A restart command
/// that returns successfully has said only that the service manager
/// accepted the request; a binary that exits immediately on this machine
/// (a missing library, a configuration the new version refuses) leaves
/// launchd or systemd reporting a service that is registered and dead,
/// which is exactly the state nobody notices until the next edit fails to
/// propagate. And a service that is running is not yet proof: it has to
/// be running the version just installed.
fn restart_service(
    installed: ServiceState,
    service: &dyn Service,
    reported: &str,
    state_root: &Path,
    mut undo: Undo,
) -> Result<()> {
    if installed == ServiceState::NotInstalled {
        undo.commit();
        println!("  no login service is installed; nothing to restart");
        println!("  run `autobahn install` to register one, or `autobahn watch` in a terminal");
        return Ok(());
    }

    let restarted = service.restart();
    if let Err(error) = &restarted {
        eprintln!("  the restart failed: {error}");
    }
    let observed = service.wait_running();
    let build = match (&restarted, observed) {
        (Ok(()), ServiceState::Running) => service.running_build(state_root, reported),
        _ => RunningBuild::Absent,
    };

    match recovery(restarted.is_ok(), observed, build_matches(&build, reported)) {
        Recovery::Keep => {
            match build {
                RunningBuild::Absent | RunningBuild::Unknown => println!(
                    "  restarted the login service (its build could not be confirmed over \
                     the control socket)"
                ),
                _ => println!("  restarted the login service, now on {reported}"),
            }
            undo.commit();
            Ok(())
        }
        Recovery::RollBack => {
            let why = match (&restarted, observed, &build) {
                (Ok(()), ServiceState::Running, RunningBuild::This) => format!(
                    "the login service came back on this build ({}), not {reported}",
                    env!("CARGO_PKG_VERSION")
                ),
                (Ok(()), ServiceState::Running, RunningBuild::Other(version)) => {
                    format!("the login service came back on {version}, not {reported}")
                }
                _ => "the login service did not come back on the new version".to_owned(),
            };
            // The previous binary is the only thing here that is known to
            // have run on this machine, so it goes back before anything
            // else is attempted.
            if let Err(error) = undo.roll_back_files() {
                bail!("{why}, and {error:#}");
            }
            if let Some(from) = undo.retarget_from.take() {
                if let Err(error) = service.retarget(&from) {
                    eprintln!(
                        "  unable to point the login service back at {}: {error:#}",
                        from.display()
                    );
                }
            }
            let second = service.restart();
            let back = service.wait_running();
            match (second.is_ok(), back) {
                (true, ServiceState::Running) => bail!(
                    "{why}, so the previous binary and agent bundle were restored at {} and \
                     the service is running again. The new version is not installed.",
                    undo.target.display()
                ),
                _ => bail!(
                    "{why}. The previous binary and agent bundle were restored at {}, but the \
                     service is still not running: start it with `autobahn start` and read {}",
                    undo.target.display(),
                    service::log_path()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|_| "the service log".to_owned())
                ),
            }
        }
    }
}

/// Whether the running supervisor is the build just installed: `Some`
/// when that can be told, `None` when it cannot.
///
/// A supervisor that answers this command's own request is this build,
/// which is the new one only when the update reinstalled the same
/// version. One from before builds were compared cannot say, and nothing
/// that runs here is that old except, in a downgrade, the new one; so it
/// is not held against the update, and neither is a socket that never
/// answered.
fn build_matches(build: &RunningBuild, reported: &str) -> Option<bool> {
    match build {
        RunningBuild::This => Some(env!("CARGO_PKG_VERSION") == reported),
        RunningBuild::Other(version) => Some(version.split('+').next() == Some(reported)),
        RunningBuild::Unknown | RunningBuild::Absent => None,
    }
}

/// What to do with the binary just installed, once the service manager
/// has been asked to restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recovery {
    /// The service is running on the new version. Keep it.
    Keep,
    /// The service is not running, or not on the new version. Put the
    /// previous binary and bundle back.
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
/// keep a new binary on. A service running another build is a failure
/// too: whatever it runs, it is not what was installed.
fn recovery(restarted: bool, observed: ServiceState, new_build: Option<bool>) -> Recovery {
    if !restarted || new_build == Some(false) {
        return Recovery::RollBack;
    }
    match observed {
        ServiceState::Running => Recovery::Keep,
        ServiceState::Stopped | ServiceState::NotInstalled => Recovery::RollBack,
    }
}

/// The bundle an update replaced, kept until the new version is confirmed.
struct Bundle {
    /// Where the bundle lives.
    agents: PathBuf,
    /// Where the one it replaced is kept.
    previous: PathBuf,
    /// Whether there was one to keep.
    had_previous: bool,
}

impl Bundle {
    /// Puts the replaced bundle back, or, when there was none, removes the
    /// new one: the state before the update, either way.
    fn restore(self) -> Result<()> {
        let aside = self
            .agents
            .with_file_name(format!(".agents.rollback.{}", std::process::id()));
        let moved = self.agents.exists();
        if moved {
            std::fs::rename(&self.agents, &aside).with_context(|| {
                format!(
                    "unable to move the new bundle aside from {}",
                    self.agents.display()
                )
            })?;
        }
        if self.had_previous {
            if let Err(error) = std::fs::rename(&self.previous, &self.agents) {
                // No bundle at all is worse than the new one.
                if moved {
                    let _ = std::fs::rename(&aside, &self.agents);
                }
                return Err(error).with_context(|| {
                    format!(
                        "unable to restore the previous bundle from {}",
                        self.previous.display()
                    )
                });
            }
        }
        let _ = std::fs::remove_dir_all(&aside);
        Ok(())
    }

    /// Removes the replaced bundle.
    fn discard(self) {
        if self.had_previous {
            let _ = std::fs::remove_dir_all(&self.previous);
        }
    }
}

/// Replaces the agent bundle, returning how many agents it now holds and
/// the bundle it replaced, which is kept at `agents.previous` until the
/// caller confirms or rolls back the update.
///
/// The archive carries a top-level `agents/` directory, so it is
/// extracted beside the destination and swapped in whole. An extraction
/// straight over the live directory would leave a half-populated bundle
/// behind any interruption, and a half-populated bundle is worse than an
/// old one: the old one installs a stale agent that fails a handshake
/// loudly, while a missing one refuses the host outright.
fn refresh_agents(archive: &Verified, state_root: &Path) -> Result<(usize, Bundle)> {
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
        // Unpacked from the verified handle, on tar's standard input, so
        // what is unpacked is what was checked.
        let status = Command::new("tar")
            .arg("xzf")
            .arg("-")
            .arg("-C")
            .arg(&staging)
            .stdin(archive.reader()?)
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

    // Binaries only: the bundle also carries its MANIFEST.
    let count = std::fs::read_dir(&extracted)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("autobahn-"))
                .count()
        })
        .unwrap_or(0);

    let agents = state_root.join("agents");
    let superseded = state_root.join("agents.previous");
    let _ = std::fs::remove_dir_all(&superseded);
    let had_previous = agents.exists();
    if had_previous {
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
        if had_previous {
            let _ = std::fs::rename(&superseded, &agents);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error)
            .with_context(|| format!("unable to move the new bundle into {}", agents.display()));
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok((
        count,
        Bundle {
            agents,
            previous: superseded,
            had_previous,
        },
    ))
}

/// A verified download, held open.
///
/// The checksum is computed from this handle, and every later use of the
/// download reads it too, so replacing the file under its name after the
/// check changes nothing that is installed.
struct Verified {
    file: File,
}

impl Verified {
    /// Opens a download and checks it against the release's checksums.
    fn open(path: &Path, name: &str, sums: &str) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("unable to open {}", path.display()))?;
        let verified = Self { file };
        verify(&verified, name, sums)?;
        Ok(verified)
    }

    /// The verified bytes from the start, as a child's standard input.
    fn reader(&self) -> Result<Stdio> {
        Ok(Stdio::from(self.rewound()?))
    }

    /// A second descriptor on the same open file, at its start.
    fn rewound(&self) -> Result<File> {
        let mut file = self
            .file
            .try_clone()
            .context("unable to reopen a download")?;
        file.seek(SeekFrom::Start(0))
            .context("unable to rewind a download")?;
        Ok(file)
    }
}

/// The new binary, copied beside the target and not yet installed.
///
/// Its own file, created fresh and private and made executable only
/// once its bytes are written, so it is the verified content and
/// nothing else. It is removed if it is dropped before it is placed.
struct Incoming {
    path: PathBuf,
    placed: bool,
}

impl Incoming {
    /// Copies the verified bytes next to `target`. The download is in a
    /// temporary directory that may be on another filesystem, and the
    /// rename that installs the binary cannot cross one.
    fn stage(binary: &Verified, target: &Path) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let directory = target
            .parent()
            .ok_or_else(|| anyhow!("{} has no directory", target.display()))?;
        std::fs::create_dir_all(directory)
            .with_context(|| format!("unable to create {}", directory.display()))?;
        let path = directory.join(format!(
            ".autobahn.update.{}",
            crate::fsutil::random_hex(8)?
        ));
        let mut file = crate::fsutil::private_file(&path)?;
        let incoming = Self {
            path,
            placed: false,
        };
        std::io::copy(&mut binary.rewound()?, &mut file)
            .with_context(|| format!("unable to write {}", incoming.path.display()))?;
        // Through the handle, and before it is closed: nothing can be
        // executed from this file before its content is complete.
        file.set_permissions(std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("unable to make {} executable", incoming.path.display()))?;
        // Closed before anything runs it: Linux refuses to execute a file
        // that is open for writing.
        drop(file);
        Ok(incoming)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Puts the new binary at `target`, keeping whatever was there at
    /// `previous`.
    ///
    /// Rename is what makes this safe for a running process: the old
    /// inode is untouched and stays open until the service restarts.
    ///
    /// Returns whether there was a binary to keep.
    fn place(mut self, target: &Path, previous: &Path) -> Result<bool> {
        // `symlink_metadata`, so a directory entry that is a symlink (a
        // hand-managed install, or a development tree) is kept rather than
        // followed and missed.
        let kept = std::fs::symlink_metadata(target).is_ok();
        if kept {
            let _ = std::fs::remove_file(previous);
            std::fs::rename(target, previous).with_context(|| {
                format!(
                    "unable to keep the current binary at {}",
                    previous.display()
                )
            })?;
        }
        if let Err(error) = std::fs::rename(&self.path, target) {
            // Nothing is left half-installed: the old binary goes back under
            // its own name before this returns.
            if kept {
                let _ = std::fs::rename(previous, target);
            }
            return Err(error).with_context(|| {
                format!("unable to move the new binary into {}", target.display())
            });
        }
        self.placed = true;
        Ok(kept)
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        if !self.placed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Runs a downloaded binary and returns the version it reports.
///
/// This is the check a checksum cannot make. A release whose assets were
/// assembled with two platforms transposed, or an asset renamed by hand,
/// matches its own SHA256SUMS exactly and still cannot run here.
fn run_reports_version(binary: &Path) -> Result<String> {
    let output = run_staged(Command::new(binary).arg("--version"))
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

/// Runs the binary just staged, and collects its output.
///
/// The staged copy was written by this process a moment ago. A process
/// forked by another thread in that moment holds the write descriptor
/// until it executes something, and until then Linux refuses to execute
/// the file (`ETXTBSY`). That is brief, and it is retried rather than
/// reported as a binary that does not run.
fn run_staged(command: &mut Command) -> std::io::Result<std::process::Output> {
    let mut attempts = 0;
    loop {
        match command.output() {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempts < 50 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

/// Refuses an upgrade to a build that cannot read some session's baseline
/// while that session is not settled.
///
/// Asked of the downloaded build itself (`autobahn formats`), since only it
/// knows what it reads. A build from before the question existed cannot
/// answer, and is let through as before.
fn check_baselines(binary: &Path, state_root: &Path) -> Result<()> {
    let Some((oldest, newest)) = run_staged(Command::new(binary).arg("formats"))
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| parse_formats(&String::from_utf8_lossy(&output.stdout)))
    else {
        return Ok(());
    };
    let (unreadable, unsettled) = baselines_needing_rebuild(state_root, oldest, newest);
    if unreadable == 0 {
        return Ok(());
    }
    if !unsettled.is_empty() {
        bail!(
            "the new build reads baseline formats {oldest} to {newest}, and {unreadable} \
             session(s) here were written in another. It rebuilds those from their two \
             sides, which is safe only where the sides match, and these do not yet:\n{}\n\
             Settle them (`autobahn doctor <group>` shows how they differ), then run \
             `autobahn update` again. Nothing was installed.",
            unsettled
                .iter()
                .map(|line| format!("  {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    println!(
        "  {unreadable} session(s) will rebuild their baseline on first start; \
         their two sides match, so nothing moves"
    );
    Ok(())
}

/// Reads `ancestor <oldest> <newest>` as `autobahn formats` prints it.
fn parse_formats(text: &str) -> Option<(u16, u16)> {
    let mut words = text.split_whitespace();
    (words.next()? == "ancestor").then_some(())?;
    Some((words.next()?.parse().ok()?, words.next()?.parse().ok()?))
}

/// How many sessions' baselines fall outside `oldest..=newest`, and a line
/// for each of those not settled — anything but synchronized with nothing
/// waiting, or never recorded at all.
fn baselines_needing_rebuild(state_root: &Path, oldest: u16, newest: u16) -> (usize, Vec<String>) {
    let mut unreadable = 0;
    let mut unsettled = Vec::new();
    let Ok(entries) = std::fs::read_dir(state_root.join("sessions")) else {
        return (0, unsettled);
    };
    for entry in entries.flatten() {
        let identifier = entry.file_name().to_string_lossy().into_owned();
        let format = crate::session::ancestor::format_of(&entry.path().join("ancestor"));
        let Ok(Some(format)) = format else {
            continue;
        };
        if (oldest..=newest).contains(&format) {
            continue;
        }
        unreadable += 1;
        match crate::supervisor::read_status(state_root, &identifier) {
            Ok(Some(status))
                if status.state == "synchronized"
                    && status.conflicts.is_empty()
                    && status.blocked.is_empty() => {}
            Ok(Some(status)) => {
                let mut why = vec![status.state.clone()];
                if !status.conflicts.is_empty() {
                    why.push(crate::alerts::plural(status.conflicts.len(), "conflict"));
                }
                if !status.blocked.is_empty() {
                    why.push(crate::alerts::plural(status.blocked.len(), "blocked path"));
                }
                unsettled.push(format!(
                    "{} → {}: {}",
                    status.group,
                    status.host,
                    why.join(", ")
                ));
            }
            _ => unsettled.push(format!("session {identifier}: no status recorded")),
        }
    }
    (unreadable, unsettled)
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
fn verify(file: &Verified, name: &str, sums: &str) -> Result<()> {
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
///
/// The tool reads the open file on its standard input rather than a
/// path, so the digest is of the bytes behind this handle.
fn checksum(file: &Verified) -> Result<String> {
    let (program, arguments): (&str, &[&str]) = if have("sha256sum") {
        ("sha256sum", &[])
    } else if have("shasum") {
        ("shasum", &["-a", "256"])
    } else {
        bail!("neither sha256sum nor shasum is available, so nothing can be verified");
    };
    let output = Command::new(program)
        .args(arguments)
        .stdin(file.reader()?)
        .output()
        .with_context(|| format!("unable to run {program}"))?;
    if !output.status.success() {
        bail!("{program} exited with {}", output.status);
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
}

impl Fetch for Source {
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

/// The private directory a run downloads into, under the state root's
/// `tmp/`, never the shared temporary directory: a download is read again
/// after it is checked, and nobody else may be able to reach it. Removed,
/// with everything in it, when dropped, so every early return in `run`
/// takes the downloads with it.
fn workspace(state_root: &Path) -> Result<crate::fsutil::PrivateTempDir> {
    crate::fsutil::private_tempdir_in(state_root)
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
    // Absolute, because it may be written into the service definition,
    // which is read in a working directory nobody chose.
    let absolute = |path: PathBuf| {
        std::path::absolute(&path).with_context(|| format!("unable to resolve {}", path.display()))
    };
    if let Some(bin_dir) = bin_dir {
        return absolute(bin_dir);
    }
    for variable in ["AUTOBAHN_BIN_DIR", "AUTOBAHN_PREFIX"] {
        if let Ok(bin_dir) = std::env::var(variable) {
            if !bin_dir.is_empty() {
                return absolute(PathBuf::from(bin_dir));
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
        let error = Verified::open(&file, "autobahn-linux-x86_64", wrong)
            .err()
            .expect("a wrong checksum must refuse");
        assert!(format!("{error}").contains("checksum mismatch"), "{error}");

        let absent = "\
0000000000000000000000000000000000000000000000000000000000000000  autobahn-agents.tar.gz
";
        let error = Verified::open(&file, "autobahn-linux-x86_64", absent)
            .err()
            .expect("a missing entry refuses");
        assert!(format!("{error}").contains("no entry"), "{error}");
    }

    /// The same file verifies against the digest the machine's own tool
    /// computes, which is the only way to know the two halves agree.
    #[test]
    fn a_file_verifies_against_its_own_digest() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let file = directory.path().join("autobahn-linux-x86_64");
        std::fs::write(&file, b"content").expect("writes");
        let sums = sums_for(&file, "autobahn-linux-x86_64");
        Verified::open(&file, "autobahn-linux-x86_64", &sums).expect("its own digest verifies");
    }

    /// A `SHA256SUMS` line for `file` as it is now, from the machine's
    /// own tool.
    fn sums_for(file: &Path, name: &str) -> String {
        let handle = Verified {
            file: File::open(file).expect("opens"),
        };
        format!("{}  {name}\n", checksum(&handle).expect("a digest"))
    }

    /// What is installed is what was checked. The name of a download can
    /// be replaced after its checksum passes; the copy that is run and
    /// installed comes from the handle the checksum read.
    #[test]
    fn a_download_swapped_after_verification_is_not_what_is_installed() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let staged = directory.path().join("autobahn-linux-x86_64");
        std::fs::write(&staged, b"the verified binary").expect("writes");
        let sums = sums_for(&staged, "autobahn-linux-x86_64");
        let verified = Verified::open(&staged, "autobahn-linux-x86_64", &sums).expect("verifies");

        // A new file under the name, the way somebody racing the update
        // would replace it, not a write into the checked one.
        let aside = directory.path().join("replacement");
        std::fs::write(&aside, b"somebody else's binary").expect("writes");
        std::fs::rename(&aside, &staged).expect("renames");

        let target = directory.path().join("bin").join("autobahn");
        let previous = directory.path().join("bin").join("autobahn.previous");
        let incoming = Incoming::stage(&verified, &target).expect("stages");
        assert_eq!(
            std::fs::read(incoming.path()).expect("reads"),
            b"the verified binary",
            "what runs --version is the checked bytes"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(incoming.path())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o755);
        incoming.place(&target, &previous).expect("installs");
        assert_eq!(
            std::fs::read(&target).expect("reads"),
            b"the verified binary"
        );
    }

    /// The same for the bundle: tar unpacks the handle, not the name.
    #[test]
    fn a_bundle_swapped_after_verification_is_not_what_is_unpacked() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let archive = directory.path().join(AGENTS_ASSET);
        bundle(
            &directory.path().join("good"),
            b"the verified agent",
            &archive,
        );
        let sums = sums_for(&archive, AGENTS_ASSET);
        let verified = Verified::open(&archive, AGENTS_ASSET, &sums).expect("verifies");

        let evil = directory.path().join("evil.tar.gz");
        bundle(
            &directory.path().join("bad"),
            b"somebody else's agent",
            &evil,
        );
        std::fs::rename(&evil, &archive).expect("renames");

        let state_root = directory.path().join("state");
        refresh_agents(&verified, &state_root).expect("refreshes");
        assert_eq!(
            std::fs::read(state_root.join("agents").join("autobahn-linux-x86_64")).expect("reads"),
            b"the verified agent"
        );
    }

    /// Writes an agent bundle holding one agent with `content`.
    fn bundle(scratch: &Path, content: &[u8], archive: &Path) {
        std::fs::create_dir_all(scratch.join("agents")).expect("directories");
        std::fs::write(
            scratch.join("agents").join("autobahn-linux-x86_64"),
            content,
        )
        .expect("writes");
        let status = Command::new("tar")
            .arg("czf")
            .arg(archive)
            .arg("-C")
            .arg(scratch)
            .arg("agents")
            .status()
            .expect("runs tar");
        assert!(status.success());
    }

    /// Downloads land in a directory only this user can enter, under the
    /// state root, not in the shared temporary directory.
    #[test]
    fn the_update_workspace_is_private_under_the_state_root() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("temporary directory");
        let state_root = directory.path().join("state");
        let work = workspace(&state_root).expect("a workspace");
        assert_eq!(work.path().parent(), Some(state_root.join("tmp").as_path()));
        let mode = std::fs::symlink_metadata(work.path())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o700);
        let path = work.path().to_path_buf();
        drop(work);
        assert!(!path.exists(), "the workspace goes when the run does");
    }

    /// A staged binary that is never placed does not stay behind in the
    /// directory on the PATH.
    #[test]
    fn a_binary_staged_and_not_placed_is_removed() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let staged = directory.path().join("downloaded");
        std::fs::write(&staged, b"the new binary").expect("writes");
        let verified = Verified {
            file: File::open(&staged).expect("opens"),
        };
        let bin_dir = directory.path().join("bin");
        let incoming = Incoming::stage(&verified, &bin_dir.join("autobahn")).expect("stages");
        drop(incoming);
        assert_eq!(std::fs::read_dir(&bin_dir).expect("lists").count(), 0);
    }

    /// Stages and places a download, as `run` does once it is verified.
    fn place(staged: &Path, target: &Path, previous: &Path) {
        let verified = Verified {
            file: File::open(staged).expect("opens"),
        };
        Incoming::stage(&verified, target)
            .expect("stages")
            .place(target, previous)
            .expect("installs");
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
        let running = ServiceState::Running;
        assert_eq!(recovery(true, running, Some(true)), Recovery::Keep);
        assert_eq!(recovery(true, running, None), Recovery::Keep);
        assert_eq!(
            recovery(true, ServiceState::Stopped, None),
            Recovery::RollBack
        );
        assert_eq!(
            recovery(true, ServiceState::NotInstalled, None),
            Recovery::RollBack
        );
        assert_eq!(
            recovery(false, ServiceState::Stopped, None),
            Recovery::RollBack
        );
        // A restart that failed is a rollback even if the service manager
        // reports a running service: what is running is the old process
        // the restart never replaced, and keeping a new binary on that
        // reading would arm the *next* restart to fail with nobody
        // watching.
        assert_eq!(recovery(false, running, None), Recovery::RollBack);
        // Running, and on another build: not what was installed.
        assert_eq!(recovery(true, running, Some(false)), Recovery::RollBack);
    }

    #[test]
    fn the_running_build_is_compared_with_the_installed_version() {
        let this = env!("CARGO_PKG_VERSION");
        assert_eq!(build_matches(&RunningBuild::This, this), Some(true));
        assert_eq!(build_matches(&RunningBuild::This, "999.0.0"), Some(false));
        assert_eq!(
            build_matches(&RunningBuild::Other("999.0.0+e7".into()), "999.0.0"),
            Some(true)
        );
        assert_eq!(
            build_matches(&RunningBuild::Other("0.1.0+e1".into()), "999.0.0"),
            Some(false)
        );
        assert_eq!(build_matches(&RunningBuild::Unknown, "999.0.0"), None);
        assert_eq!(build_matches(&RunningBuild::Absent, "999.0.0"), None);
    }

    /// A release served from a directory: the fake fetcher.
    struct Release(tempfile::TempDir);

    impl Fetch for Release {
        fn fetch(&self, asset: &str, destination: &Path) -> Result<()> {
            std::fs::copy(self.0.path().join(asset), destination)
                .with_context(|| format!("no {asset}"))?;
            Ok(())
        }
    }

    /// The version the fake release's binary reports.
    const NEW: &str = "999.0.0";

    impl Release {
        /// A binary reporting [`NEW`], a bundle whose one agent says
        /// "new agent", and checksums for both.
        fn new() -> Self {
            let release = Self(tempfile::tempdir().expect("temporary directory"));
            let asset = format!("autobahn-{}", release_platform().unwrap());
            let binary = release.0.path().join(&asset);
            // `formats` fails, as a build from before that question does,
            // so no baseline stands in the way.
            std::fs::write(
                &binary,
                format!(
                    "#!/bin/sh\n[ \"$1\" = --version ] && echo 'autobahn {NEW}' && exit 0\nexit 1\n"
                ),
            )
            .unwrap();
            let archive = release.0.path().join(AGENTS_ASSET);
            bundle(&release.0.path().join("scratch"), b"new agent", &archive);
            let sums = sums_for(&binary, &asset) + &sums_for(&archive, AGENTS_ASSET);
            std::fs::write(release.0.path().join(CHECKSUMS_ASSET), sums).unwrap();
            release
        }
    }

    /// A login service that does what a test says, and records what it
    /// was asked.
    struct Stub {
        executable: std::cell::RefCell<PathBuf>,
        restart_fails: bool,
        build: RunningBuild,
        retargeted: std::cell::RefCell<Vec<PathBuf>>,
        restarts: std::cell::Cell<usize>,
    }

    impl Stub {
        fn new(executable: &Path) -> Self {
            Self {
                executable: std::cell::RefCell::new(executable.to_path_buf()),
                restart_fails: false,
                build: RunningBuild::Other(format!("{NEW}+e1")),
                retargeted: Default::default(),
                restarts: Default::default(),
            }
        }
    }

    impl Service for Stub {
        fn state(&self) -> ServiceState {
            ServiceState::Running
        }
        fn registration(&self) -> Result<Option<service::Registration>> {
            Ok(Some(service::Registration {
                executable: self.executable.borrow().clone(),
                arguments: vec!["watch".into()],
                home: None,
            }))
        }
        fn retarget(&self, executable: &Path) -> Result<()> {
            *self.executable.borrow_mut() = executable.to_path_buf();
            self.retargeted.borrow_mut().push(executable.to_path_buf());
            Ok(())
        }
        fn restart(&self) -> Result<()> {
            self.restarts.set(self.restarts.get() + 1);
            // Only the first restart, onto the new version, fails; the one
            // after a rollback works.
            match self.restart_fails && self.restarts.get() == 1 {
                true => bail!("the stub refuses"),
                false => Ok(()),
            }
        }
        fn wait_running(&self) -> ServiceState {
            ServiceState::Running
        }
        fn running_build(&self, _: &Path, _: &str) -> RunningBuild {
            self.build.clone()
        }
    }

    /// A machine with an old binary and an old bundle installed.
    struct Machine {
        _root: tempfile::TempDir,
        places: Places,
    }

    impl Machine {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temporary directory");
            let bin_dir = root.path().join("bin");
            let state_root = root.path().join("state");
            std::fs::create_dir_all(&bin_dir).unwrap();
            std::fs::write(bin_dir.join("autobahn"), b"old binary").unwrap();
            std::fs::create_dir_all(state_root.join("agents")).unwrap();
            std::fs::write(
                state_root.join("agents").join("autobahn-linux-x86_64"),
                b"old agent",
            )
            .unwrap();
            Self {
                _root: root,
                places: Places {
                    platform: release_platform().unwrap(),
                    bin_dir,
                    state_root,
                },
            }
        }

        fn target(&self) -> PathBuf {
            self.places.bin_dir.join("autobahn")
        }

        fn binary(&self) -> Vec<u8> {
            std::fs::read(self.target()).unwrap()
        }

        fn agent(&self) -> Vec<u8> {
            std::fs::read(
                self.places
                    .state_root
                    .join("agents")
                    .join("autobahn-linux-x86_64"),
            )
            .unwrap()
        }

        fn update(&self, release: &Release, service: &Stub, retarget: bool) -> Result<()> {
            let options = Options {
                retarget,
                ..Options::default()
            };
            install(&options, Some("v999.0.0"), &self.places, release, service)
        }

        /// Whether the binary and bundle kept for a rollback are gone.
        fn nothing_kept(&self) -> bool {
            !self.places.bin_dir.join("autobahn.previous").exists()
                && !self.places.state_root.join("agents.previous").exists()
        }
    }

    #[test]
    fn a_confirmed_update_installs_both_and_keeps_neither_previous() {
        let machine = Machine::new();
        let service = Stub::new(&machine.target());
        machine
            .update(&Release::new(), &service, false)
            .expect("updates");
        assert!(machine.binary().starts_with(b"#!/bin/sh"));
        assert_eq!(machine.agent(), b"new agent");
        assert!(machine.nothing_kept());
        assert!(service.retargeted.borrow().is_empty());
    }

    /// M-40: a restart that fails puts back the bundle as well as the
    /// binary, so the old controller does not upload the new agents.
    #[test]
    fn a_failed_restart_rolls_back_the_binary_and_the_bundle() {
        let machine = Machine::new();
        let mut service = Stub::new(&machine.target());
        service.restart_fails = true;
        let error = machine
            .update(&Release::new(), &service, false)
            .expect_err("a failed restart fails the update");
        assert!(format!("{error}").contains("restored"), "{error}");
        assert_eq!(machine.binary(), b"old binary");
        assert_eq!(machine.agent(), b"old agent");
        assert!(machine.nothing_kept());
    }

    /// M-41: a service that runs another file would restart onto the old
    /// version, so the update refuses before changing anything.
    #[test]
    fn a_service_registered_elsewhere_is_refused_without_retarget() {
        let machine = Machine::new();
        let elsewhere = machine.places.state_root.join("elsewhere").join("autobahn");
        let service = Stub::new(&elsewhere);
        let error = machine
            .update(&Release::new(), &service, false)
            .expect_err("refused");
        assert!(format!("{error}").contains("--retarget"), "{error}");
        assert_eq!(machine.binary(), b"old binary");
        assert_eq!(machine.agent(), b"old agent");
        assert_eq!(service.restarts.get(), 0);
    }

    #[test]
    fn with_retarget_the_service_is_pointed_at_the_update_and_confirmed() {
        let machine = Machine::new();
        let elsewhere = machine.places.state_root.join("elsewhere").join("autobahn");
        let service = Stub::new(&elsewhere);
        machine
            .update(&Release::new(), &service, true)
            .expect("updates");
        assert_eq!(*service.retargeted.borrow(), vec![machine.target()]);
        assert_eq!(*service.executable.borrow(), machine.target());
        assert_eq!(machine.agent(), b"new agent");
    }

    /// M-41: running is not enough; the service must be on the new build.
    #[test]
    fn a_service_that_comes_back_on_the_old_build_is_rolled_back() {
        let machine = Machine::new();
        let mut service = Stub::new(&machine.target());
        service.build = RunningBuild::This;
        let error = machine
            .update(&Release::new(), &service, false)
            .expect_err("the old build is a failure");
        assert!(format!("{error}").contains("not 999.0.0"), "{error}");
        assert_eq!(machine.binary(), b"old binary");
        assert_eq!(machine.agent(), b"old agent");
        assert_eq!(service.restarts.get(), 2, "restarted again on the old one");
    }

    /// A retargeted service that fails is pointed back where it was.
    #[test]
    fn a_retargeted_service_that_fails_is_pointed_back() {
        let machine = Machine::new();
        let elsewhere = machine.places.state_root.join("elsewhere").join("autobahn");
        let mut service = Stub::new(&elsewhere);
        service.restart_fails = true;
        machine
            .update(&Release::new(), &service, true)
            .expect_err("fails");
        assert_eq!(*service.executable.borrow(), elsewhere);
        assert_eq!(machine.binary(), b"old binary");
    }

    /// The usual --retarget: nothing at the target yet. A failure puts
    /// the service back on the executable it ran, and takes away the new
    /// binary, rather than leaving the service on a build that failed.
    #[test]
    fn a_first_retarget_that_fails_leaves_the_service_where_it_was() {
        let machine = Machine::new();
        std::fs::remove_file(machine.target()).unwrap();
        let elsewhere = machine.places.state_root.join("elsewhere").join("autobahn");
        let mut service = Stub::new(&elsewhere);
        service.restart_fails = true;
        let error = machine
            .update(&Release::new(), &service, true)
            .expect_err("fails");
        assert!(format!("{error}").contains("restored"), "{error}");
        assert_eq!(*service.executable.borrow(), elsewhere);
        assert!(!machine.target().exists());
        assert_eq!(machine.agent(), b"old agent");
    }

    #[test]
    fn a_registered_path_is_the_target_through_linked_directories() {
        let root = tempfile::tempdir().expect("temporary directory");
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, root.path().join("link")).unwrap();
        std::fs::write(real.join("autobahn"), b"x").unwrap();
        assert!(same_entry(
            &root.path().join("link").join("autobahn"),
            &real.join("autobahn")
        ));
        assert!(!same_entry(
            &root.path().join("other").join("autobahn"),
            &real.join("autobahn")
        ));
        // A target that is itself a link is replaced, not followed: the
        // file it points at is not what the update changes.
        std::os::unix::fs::symlink(real.join("autobahn"), real.join("autobahn-link")).unwrap();
        assert!(!same_entry(
            &real.join("autobahn"),
            &real.join("autobahn-link")
        ));
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

        place(&staged, &target, &previous);

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

        place(&staged, &target, &previous);
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

    #[test]
    fn formats_parse_as_printed() {
        assert_eq!(parse_formats("ancestor 0 2\n"), Some((0, 2)));
        assert_eq!(parse_formats("something else"), None);
        assert_eq!(parse_formats(""), None);
    }

    #[test]
    fn a_baseline_the_new_build_cannot_read_blocks_only_while_unsettled() {
        use crate::supervisor::SessionStatus;
        let root = tempfile::tempdir().expect("a temporary directory");
        let session = root.path().join("sessions").join("abc");
        std::fs::create_dir_all(&session).unwrap();
        // Format 2, which a build reading 3 to 3 cannot.
        let mut checkpoint = b"ABAHNAN2".to_vec();
        checkpoint.extend_from_slice(&2u16.to_le_bytes());
        std::fs::write(session.join("ancestor"), &checkpoint).unwrap();
        let status = |state: &str, conflicts: Vec<String>| SessionStatus {
            group: "work".into(),
            host: "boite".into(),
            alpha: "~/a".into(),
            beta: "boite:~/a".into(),
            mode: "two-way-conflict".into(),
            state: state.into(),
            cycles: 3,
            last_alpha_transitions: 0,
            last_beta_transitions: 0,
            conflicts,
            conflict_details: Vec::new(),
            blocked: Vec::new(),
            error: None,
            updated_at: 1,
            alpha_entries: 0,
            beta_entries: 0,
            moved_files: 0,
            moved_bytes: 0,
            role: String::new(),
            term: 0,
            alert_after_seconds: None,
        };

        // A build that reads it: nothing to say.
        assert_eq!(
            baselines_needing_rebuild(root.path(), 0, 2),
            (0, Vec::new())
        );

        // One that does not, with the session settled: through.
        crate::supervisor::write_status(root.path(), "abc", &status("synchronized", Vec::new()))
            .unwrap();
        assert_eq!(
            baselines_needing_rebuild(root.path(), 3, 3),
            (1, Vec::new())
        );

        // Unsettled: named, with why.
        crate::supervisor::write_status(
            root.path(),
            "abc",
            &status("conflicts", vec!["a.txt".into(), "b.txt".into()]),
        )
        .unwrap();
        let (count, unsettled) = baselines_needing_rebuild(root.path(), 3, 3);
        assert_eq!(count, 1);
        assert_eq!(
            unsettled,
            vec!["work → boite: conflicts, 2 conflicts".to_owned()]
        );
    }
}
