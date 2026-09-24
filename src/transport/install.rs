//! Automatic agent installation.
//!
//! The protocol requires the agent's version to match the controller's
//! exactly, so rather than asking every host in a fleet to be upgraded in
//! lockstep, agents are installed *per version* at a well-known path on the
//! remote host (`~/.autobahn/bin/autobahn-<version>`). The controller always
//! invokes its own version's path; when that fails — a fresh host, or a
//! freshly upgraded controller — the matching agent binary is installed over
//! SSH and the connection retried. Upgrades therefore happen automatically,
//! host by host, on first contact.
//!
//! The binary to install is located by probing the remote platform
//! (`uname -sm`) and searching, in order: the `AUTOBAHN_AGENTS_DIR`
//! environment variable, `~/.autobahn/agents` (where the installer places
//! the bundle), an `agents` directory beside the running executable, and —
//! when the remote platform matches the local one — the running executable
//! itself (the common single-platform-fleet case, which needs no bundle at
//! all).

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use crate::protocol;

/// Returns the remote command invoking this build's agent.
///
/// An agent is named for its version *and its content*:
/// `autobahn-<version>-<digest>`. The version alone let a rebuilt agent
/// at the same version run the old one forever, since the path existed and
/// nothing was ever uploaded again. Named by content, a changed binary is
/// always a missing one, and an unchanged one is never sent twice.
///
/// The controller cannot know the remote platform without asking, and
/// asking is a round trip on every connect, so the command asks for it:
/// a `sh` script maps `uname` to the bundle's naming, as `platform_name`
/// does, and runs the agent this controller would have installed for that
/// platform. A platform it holds no binary for falls back to the version's
/// plain name, which is what every controller before this one installed.
/// A missing agent exits 127, which is what a missing agent always looked
/// like, and the caller installs it.
pub fn versioned_remote_command() -> String {
    let version = protocol::version();
    let mut branches = String::new();
    for (platform, path) in agent_candidates() {
        if let Some(digest) = file_digest(&path) {
            branches.push_str(&format!(
                "{platform}) exec \"$HOME/.autobahn/bin/autobahn-{version}-{digest}\" agent;; "
            ));
        }
    }
    // Run by `sh` whatever the login shell is: the script is POSIX, and a
    // login shell like fish would not parse it. Single-quoted as one word;
    // nothing inside it holds a single quote.
    format!(
        "sh -c 's=$(uname -s | tr A-Z a-z); m=$(uname -m); \
         case $m in arm64) m=aarch64;; amd64) m=x86_64;; esac; \
         case $s-$m in {branches}*) exec \"$HOME/.autobahn/bin/autobahn-{version}\" agent;; esac'"
    )
}

/// The first `digest_length` hex digits of a binary's blake3: enough to
/// tell two builds apart, short enough to read in a file listing.
const DIGEST_LENGTH: usize = 12;

/// Names an agent binary's content.
fn content_digest(content: &[u8]) -> String {
    blake3::hash(content).to_hex()[..DIGEST_LENGTH].to_owned()
}

/// Names a file's content, remembered per path while the file's size and
/// modification time stand, so building the remote command on every
/// connect does not read several megabytes each time.
fn file_digest(path: &std::path::Path) -> Option<String> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    type Seen = HashMap<PathBuf, (std::time::SystemTime, u64, String)>;
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let metadata = fs::metadata(path).ok()?;
    let stamp = (metadata.modified().ok()?, metadata.len());
    let seen = SEEN.get_or_init(Mutex::default);
    if let Some((modified, length, digest)) = seen
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(path)
    {
        if (*modified, *length) == stamp {
            return Some(digest.clone());
        }
    }
    let digest = content_digest(&fs::read(path).ok()?);
    seen.lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(path.to_path_buf(), (stamp.0, stamp.1, digest.clone()));
    Some(digest)
}

/// Every platform this controller could install an agent for, with the
/// binary it would install — the same search `locate_agent_binary` makes,
/// the first place holding a platform winning.
fn agent_candidates() -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for directory in agent_directories() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter(|name| name.starts_with("autobahn-"))
            .collect();
        names.sort();
        for name in names {
            let platform = name["autobahn-".len()..].to_owned();
            if !platform.is_empty()
                && platform
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && !found.iter().any(|(known, _)| *known == platform)
            {
                found.push((platform, directory.join(&name)));
            }
        }
    }
    let local = local_platform();
    if !found.iter().any(|(known, _)| *known == local) {
        if let Ok(executable) = std::env::current_exe() {
            found.push((local, executable));
        }
    }
    found
}

/// Where agent bundles are looked for, in order.
fn agent_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Ok(directory) = std::env::var("AUTOBAHN_AGENTS_DIR") {
        directories.push(PathBuf::from(directory));
    }
    if let Ok(state_root) = crate::paths::default_state_root() {
        directories.push(state_root.join("agents"));
    }
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            directories.push(directory.join("agents"));
        }
    }
    directories
}

/// The file a bundle states its contents in: one line per binary,
/// `autobahn-<platform> <version> <blake3>`, written by the release.
pub const MANIFEST: &str = "MANIFEST";

/// Checks a bundle binary against its bundle's manifest, before anything
/// is sent anywhere. A binary from an older build would be uploaded under
/// this build's name and refused by the handshake, on every host, with a
/// message about the host; this refuses it here, naming the bundle.
///
/// A bundle with no manifest — a local cross-build — is not checked, and
/// the handshake remains the check.
fn check_manifest(binary: &std::path::Path, platform: &str) -> Result<()> {
    let Some(directory) = binary.parent() else {
        return Ok(());
    };
    let Ok(manifest) = fs::read_to_string(directory.join(MANIFEST)) else {
        return Ok(());
    };
    let name = format!("autobahn-{platform}");
    let version = protocol::version();
    let Some(line) = manifest
        .lines()
        .find(|line| line.split_whitespace().next() == Some(name.as_str()))
    else {
        bail!(
            "the agent bundle in {} has a manifest that does not list {name}; \
             reinstall the bundle (autobahn update)",
            directory.display()
        );
    };
    let mut fields = line.split_whitespace().skip(1);
    let (Some(stated), Some(digest)) = (fields.next(), fields.next()) else {
        bail!(
            "the agent bundle manifest in {} is malformed",
            directory.display()
        );
    };
    if stated != version {
        bail!(
            "the agent bundle in {} is for {stated}, and this is {version}; \
             `autobahn update` installs the matching bundle",
            directory.display()
        );
    }
    let content = fs::read(binary)
        .with_context(|| format!("unable to read agent binary {}", binary.display()))?;
    if blake3::hash(&content).to_hex().as_str() != digest {
        bail!(
            "{} is not the binary its bundle's manifest lists; \
             `autobahn update` reinstalls the bundle",
            binary.display()
        );
    }
    Ok(())
}

/// Ensures this version's agent is installed on the remote host: probes the
/// platform, locates a matching binary locally, and streams it into place
/// over a single SSH connection.
///
/// Returns what was installed, which the caller needs to diagnose the one
/// failure this cannot detect for itself: a *stale bundle*. The uploaded
/// file is named for the controller's version, but nothing here can read
/// the version out of a binary built for another platform, so a bundle
/// left over from an older build is published under the new name and the
/// handshake is the first thing to notice. That is safe — the mismatch is
/// refused — but the message says only that the remote is behind, which
/// reads as a host that needs upgrading rather than a bundle that needs
/// rebuilding.
pub fn ensure_agent(destination: &str) -> Result<Installed> {
    let platform = probe_platform(destination)?;
    let binary = locate_agent_binary(&platform).ok_or_else(|| {
        anyhow!(
            "no agent binary for {platform} is available (install the agent bundle into \
             ~/.autobahn/agents, set AUTOBAHN_AGENTS_DIR, or place an `agents` directory \
             containing autobahn-{platform} beside the executable)"
        )
    })?;
    if binary != std::env::current_exe().unwrap_or_default() {
        check_manifest(&binary, &platform)?;
    }
    upload_agent(destination, &binary)
        .with_context(|| format!("unable to install the {platform} agent on {destination}"))?;
    Ok(Installed {
        platform,
        source: binary,
    })
}

/// What an installation put on the remote host.
pub struct Installed {
    /// The remote platform, in bundle naming form.
    pub platform: String,
    /// The local file the agent was copied from.
    pub source: PathBuf,
}

impl Installed {
    /// How the source file should be described when the agent it produced
    /// turns out to be the wrong version: the path, and how old it is.
    ///
    /// The age is the tell. A bundle built days before the controller is
    /// the whole diagnosis, and it is the one fact a version mismatch
    /// alone never shows.
    pub fn provenance(&self) -> String {
        let age = self
            .source
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .map(|age| format!(", last built {}", describe_age(age)))
            .unwrap_or_default();
        format!("{}{age}", self.source.display())
    }
}

/// A duration in the coarsest unit that still says something, for reporting
/// how old a bundle is.
fn describe_age(age: std::time::Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        0..=90 => "moments ago".to_owned(),
        91..=5_399 => format!("{} minutes ago", seconds / 60),
        5_400..=172_799 => format!("{} hours ago", seconds / 3_600),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

/// Probes the remote host's platform, returning it in bundle naming form
/// (`linux-x86_64`, `darwin-aarch64`, ...).
fn probe_platform(destination: &str) -> Result<String> {
    let output = ssh_command(destination, "uname -sm")
        .stdin(Stdio::null())
        .output()
        .context("unable to run ssh")?;
    if !output.status.success() {
        // ssh's own message is the diagnosis — "Permission denied
        // (publickey)", "Could not resolve hostname", "Connection refused" —
        // and the exit status alone is not. It is folded in here because the
        // speculative first connection no longer prints it.
        let complaint = String::from_utf8_lossy(&output.stderr);
        let complaint = complaint.trim();
        if complaint.is_empty() {
            bail!(
                "unable to reach {destination}: ssh exited with {}",
                output.status
            );
        }
        bail!("unable to reach {destination}: {complaint}");
    }
    let report = String::from_utf8_lossy(&output.stdout);
    let mut parts = report.split_whitespace();
    let (Some(system), Some(machine)) = (parts.next(), parts.next()) else {
        bail!("unable to parse the platform report {report:?} from {destination}");
    };
    Ok(platform_name(system, machine))
}

/// Maps a `uname -sm` report to the bundle platform naming.
fn platform_name(system: &str, machine: &str) -> String {
    let machine = match machine {
        "arm64" => "aarch64",
        "amd64" => "x86_64",
        other => other,
    };
    format!("{}-{machine}", system.to_lowercase())
}

/// Returns the local platform in bundle naming form.
///
/// Rust names the operating system after the vendor ("macos") where
/// `uname -s` names the kernel ("Darwin"), so the constant is translated
/// rather than used directly. Without this the two namings never match on
/// a Mac, and a Mac controller cannot serve as its own agent for another
/// Mac — the case that is supposed to need no bundle at all.
fn local_platform() -> String {
    let system = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{system}-{}", std::env::consts::ARCH)
}

/// Locates the agent binary for a platform, searching in order: the bundle
/// directory named by `AUTOBAHN_AGENTS_DIR`, the `agents` directory in the
/// state root (where the installer puts it, and where everything else
/// autobahn owns already lives), an `agents` directory beside the
/// executable (which keeps a bundle travelling with a relocatable binary),
/// and — for the local platform — the running executable itself, since it
/// *is* an agent for its own platform.
fn locate_agent_binary(platform: &str) -> Option<PathBuf> {
    agent_candidates()
        .into_iter()
        .find(|(candidate, path)| candidate == platform && path.is_file())
        .map(|(_, path)| path)
}

/// Streams a binary to the remote host's versioned agent path over one SSH
/// connection: written to a temporary alongside the destination, made
/// executable, and renamed into place (so a concurrent controller never
/// observes a partial binary).
fn upload_agent(destination: &str, binary: &std::path::Path) -> Result<()> {
    // The binary is read into memory once, so the bytes streamed are
    // exactly the bytes measured — a bundle replaced or truncated mid-read
    // can't smuggle a partial binary past the check.
    let content = fs::read(binary)
        .with_context(|| format!("unable to read agent binary {}", binary.display()))?;
    // Compressed when both ends can: `gzip -1` takes the 6.7 MB binary to
    // 3.0 MB in 0.15 s and unpacks in 0.05, which pays on any link slower
    // than ~150 Mbps — a first contact over a 20 Mbps uplink goes from
    // ~2.7 s to ~1.4 — and costs about a tenth of a second on a gigabit
    // LAN. Without gzip here, or there, the binary goes as it is.
    if let Some(compressed) = gzip(&content) {
        match stream_agent(destination, &content, Some(&compressed)) {
            Err(error) if error.downcast_ref::<NoRemoteGzip>().is_some() => {}
            outcome => return outcome,
        }
    }
    stream_agent(destination, &content, None)
}

/// The remote end has no gzip to unpack with.
#[derive(Debug, thiserror::Error)]
#[error("the remote host has no gzip")]
struct NoRemoteGzip;

/// Compresses with the local `gzip -1`, or `None` when there is none.
fn gzip(content: &[u8]) -> Option<Vec<u8>> {
    let mut child = Command::new("gzip")
        .arg("-1")
        .arg("-c")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let content = content.to_vec();
    let feeder = std::thread::spawn(move || stdin.write_all(&content));
    let output = child.wait_with_output().ok()?;
    feeder.join().ok()?.ok()?;
    output.status.success().then_some(output.stdout)
}

/// Streams a binary to the remote host's agent path over one SSH
/// connection — `compressed` when given, unpacked there — checked against
/// the uncompressed length before anything is published.
fn stream_agent(destination: &str, content: &[u8], compressed: Option<&[u8]>) -> Result<()> {
    let version = protocol::version();
    // The temporary is uniquified by the remote shell's PID ($$): two
    // controllers bootstrapping the same host concurrently must not stream
    // into one file, or the later write truncates what the earlier one is
    // about to rename into the executable path. The remote length check
    // catches a stream cut short (a dropped connection, a killed ssh)
    // before anything is published; the rename is atomic and
    // last-writer-wins with a verified whole binary.
    // Named for the bytes streamed, not for the file's name: what runs
    // under this name is exactly what was measured here.
    let digest = content_digest(content);
    let (probe, receive) = match compressed {
        Some(_) => ("command -v gzip >/dev/null 2>&1 || exit 71; ", "gzip -dc"),
        None => ("", "cat"),
    };
    let script = format!(
        "{probe}mkdir -p ~/.autobahn/bin && \
         tmp=~/.autobahn/bin/.autobahn-tmp-install-{version}-$$ && \
         {receive} > \"$tmp\" && \
         [ \"$(wc -c < \"$tmp\")\" -eq {length} ] || {{ rm -f \"$tmp\"; exit 70; }} && \
         chmod 755 \"$tmp\" && \
         mv \"$tmp\" ~/.autobahn/bin/autobahn-{version}-{digest}",
        length = content.len()
    );
    let mut child = ssh_command(destination, &script)
        .stdin(Stdio::piped())
        .spawn()
        .context("unable to run ssh")?;
    let complaint = child.stderr.take();
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("ssh standard input unavailable"))?;
    let write = stdin.write_all(compressed.unwrap_or(content));
    drop(stdin);
    let status = child.wait().context("unable to wait for ssh")?;
    if status.code() == Some(71) && compressed.is_some() {
        return Err(NoRemoteGzip.into());
    }
    write.context("unable to stream the agent binary")?;
    if !status.success() {
        let detail = complaint
            .map(|mut stderr| {
                use std::io::Read;
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text);
                text.trim().to_owned()
            })
            .filter(|text| !text.is_empty());
        match detail {
            Some(detail) => bail!("the installation command failed: {detail}"),
            None => bail!("the installation command exited with {status}"),
        }
    }
    Ok(())
}

/// What a prune found and did on one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pruned {
    /// The versions removed, oldest first.
    pub removed: Vec<String>,
    /// The versions left in place, including the one in use.
    pub kept: Vec<String>,
}

/// Removes superseded agent binaries from a remote host, keeping the
/// version this controller runs plus `keep` older ones for rollback.
///
/// Agents are installed per version and nothing has ever removed them, so
/// a host contacted by a year of controllers accumulates a year of ~5 MB
/// binaries. They are kept deliberately — an older controller reconnecting
/// finds its agent already there — but "kept deliberately" and "kept
/// forever" are not the same thing.
///
/// The version in use is never removed, whatever `keep` says. Ordering is
/// by the remote file's modification time rather than by parsing the
/// version out of the name: the name carries an epoch as well as a
/// semantic version, so "newest" is a question about when a controller
/// last needed it, which is what the mtime records.
pub fn prune_agents(destination: &str, keep: usize, dry_run: bool) -> Result<Pruned> {
    let current = format!("autobahn-{}", protocol::version());
    // One name per build of this version since agents were named by
    // content: the newest is the one in use, and older builds of the same
    // version are spares like any other.
    // Oldest first, one name per line. `ls -t` is newest first, so this is
    // reversed on arrival rather than trusting a second sort remotely.
    let script = "ls -t ~/.autobahn/bin/ 2>/dev/null | grep '^autobahn-' || true";
    let output = ssh_command(destination, script)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("unable to list the agents on {destination}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        match detail.is_empty() {
            true => bail!("unable to list the agents on {destination}"),
            false => bail!("unable to list the agents on {destination}: {detail}"),
        }
    }

    let newest_first: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        // A half-finished upload is named `.autobahn-tmp-install-…`, which
        // does not match, but an interrupted rename could leave anything;
        // only well-formed agent names are ever considered.
        .filter(|name| name.starts_with("autobahn-") && !name.contains('/'))
        .map(str::to_owned)
        .collect();

    let (kept, mut removed) = select_agents(&newest_first, &current, keep);

    if removed.is_empty() || dry_run {
        removed.reverse();
        return Ok(Pruned { removed, kept });
    }

    // Removed by exact name under the one directory, never by pattern: a
    // glob here would be a remote `rm` whose reach depends on what happens
    // to be on the far side.
    let names = removed
        .iter()
        .map(|name| format!("~/.autobahn/bin/{name}"))
        .collect::<Vec<_>>()
        .join(" ");
    let script = format!("rm -f {names}");
    let output = ssh_command(destination, &script)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("unable to remove the agents on {destination}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        match detail.is_empty() {
            true => bail!("unable to remove the agents on {destination}"),
            false => bail!("unable to remove the agents on {destination}: {detail}"),
        }
    }
    removed.reverse();
    Ok(Pruned { removed, kept })
}

/// Chooses which agent binaries to keep, given the remote directory
/// newest first. Pure, so the policy is testable without a host.
///
/// The version in use is always kept and never spends a `keep` slot:
/// asking to keep one previous version should leave two binaries, not one
/// plus a gap where the working agent used to be.
fn select_agents(
    newest_first: &[String],
    current: &str,
    keep: usize,
) -> (Vec<String>, Vec<String>) {
    let mut kept = Vec::new();
    let mut removed = Vec::new();
    let mut spare = keep;
    let mut in_use = false;
    for name in newest_first {
        // This version's agent, by its plain name or by a build of it: the
        // newest such is the one running.
        let this_version = name == current
            || name
                .strip_prefix(current)
                .is_some_and(|rest| rest.starts_with('-'));
        if this_version && !in_use {
            in_use = true;
            kept.push(name.clone());
        } else if spare > 0 {
            spare -= 1;
            kept.push(name.clone());
        } else {
            removed.push(name.clone());
        }
    }
    (kept, removed)
}

/// Builds an SSH command running `script` on the destination. The option
/// terminator keeps a hostile destination (one beginning with `-`) from
/// being parsed as an SSH option such as `ProxyCommand`.
fn ssh_command(destination: &str, script: &str) -> Command {
    let mut command = Command::new(super::ssh_binary());
    command.args(super::ssh_options());
    command.arg("--");
    command.arg(destination);
    command.arg(script);
    command.stdout(Stdio::piped());
    // Captured rather than inherited: ssh's complaint is folded into the
    // error the caller returns, where it is attributed to a session and a
    // host. Inherited, it arrives as an unattributed line in the middle of
    // a supervisor's output, which for a fan-out is unreadable.
    command.stderr(Stdio::piped());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The installer places the bundle in the state root, so that is where
    /// a bundle must be found — the alternative is an installer whose work
    /// the controller ignores.
    #[test]
    fn a_bundle_in_the_state_root_is_found() {
        // HOME is process-global, so this test owns it for its duration and
        // restores it, rather than running beside another that reads it.
        let keep = tempfile::tempdir().expect("temporary directory");
        let agents = keep.path().join(".autobahn").join("agents");
        std::fs::create_dir_all(&agents).expect("directories");
        // A platform this machine certainly is not, so the local-executable
        // fallback cannot satisfy the lookup and mask the failure.
        let planted = agents.join("autobahn-linux-mips64");
        std::fs::write(&planted, b"an agent").expect("writes");

        let previous = std::env::var("HOME").ok();
        std::env::set_var("HOME", keep.path());
        let found = locate_agent_binary("linux-mips64");
        match previous {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(found.as_deref(), Some(planted.as_path()));
    }

    /// The probe's naming and the local constant's naming must agree, or
    /// the running executable is never recognized as an agent for its own
    /// platform — which is exactly the case that needs no bundle.
    #[test]
    fn the_local_platform_matches_what_probing_this_machine_would_report() {
        let probed = platform_name(
            if cfg!(target_os = "macos") {
                "Darwin"
            } else if cfg!(target_os = "linux") {
                "Linux"
            } else {
                "FreeBSD"
            },
            if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "x86_64"
            },
        );
        assert_eq!(local_platform(), probed);
    }

    #[test]
    fn platform_names_normalize_uname_variants() {
        assert_eq!(platform_name("Linux", "x86_64"), "linux-x86_64");
        assert_eq!(platform_name("Linux", "aarch64"), "linux-aarch64");
        assert_eq!(platform_name("Darwin", "arm64"), "darwin-aarch64");
        assert_eq!(platform_name("Darwin", "x86_64"), "darwin-x86_64");
        assert_eq!(platform_name("FreeBSD", "amd64"), "freebsd-x86_64");
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn the_local_platform_is_expressible_in_bundle_naming() {
        // The fallback that lets a single-platform fleet skip bundles
        // entirely depends on these namings agreeing.
        assert_eq!(local_platform(), platform_name("Linux", "x86_64"));
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    /// The agent in use is never a candidate, whatever it costs. Removing
    /// it would take every session on that host down until the controller
    /// reinstalled it.
    #[test]
    fn the_version_in_use_is_never_removed() {
        let all = names(&[
            "autobahn-0.4.0+e8",
            "autobahn-0.4.0+e9",
            "autobahn-0.4.0+e5",
        ]);
        let (kept, removed) = select_agents(&all, "autobahn-0.4.0+e9", 0);
        assert_eq!(kept, names(&["autobahn-0.4.0+e9"]));
        assert_eq!(
            removed,
            names(&["autobahn-0.4.0+e8", "autobahn-0.4.0+e5"]),
            "everything else goes when no rollback is asked for"
        );
    }

    /// Keeping one previous version must leave two binaries, not one. The
    /// version in use does not spend a rollback slot.
    #[test]
    fn the_current_version_does_not_spend_a_rollback_slot() {
        let all = names(&[
            "autobahn-0.4.0+e9",
            "autobahn-0.4.0+e8",
            "autobahn-0.4.0+e7",
            "autobahn-0.4.0+e6",
        ]);
        let (kept, removed) = select_agents(&all, "autobahn-0.4.0+e9", 1);
        assert_eq!(kept, names(&["autobahn-0.4.0+e9", "autobahn-0.4.0+e8"]));
        assert_eq!(removed, names(&["autobahn-0.4.0+e7", "autobahn-0.4.0+e6"]));
    }

    /// A host this controller has never contacted has no agent of ours in
    /// place. Nothing is in use there, so nothing is protected, but the
    /// rollback allowance still applies.
    #[test]
    fn a_host_without_this_version_still_keeps_its_allowance() {
        let all = names(&["autobahn-0.4.0+e8", "autobahn-0.4.0+e7"]);
        let (kept, removed) = select_agents(&all, "autobahn-0.4.0+e9", 1);
        assert_eq!(kept, names(&["autobahn-0.4.0+e8"]));
        assert_eq!(removed, names(&["autobahn-0.4.0+e7"]));
    }

    /// Fewer agents than the allowance removes nothing at all.
    #[test]
    fn nothing_is_removed_when_there_is_nothing_superseded() {
        let all = names(&["autobahn-0.4.0+e9"]);
        let (kept, removed) = select_agents(&all, "autobahn-0.4.0+e9", 1);
        assert_eq!(kept, all);
        assert!(removed.is_empty());
    }

    #[test]
    fn the_versioned_command_names_this_build() {
        let command = versioned_remote_command();
        assert!(command.contains(&format!("/.autobahn/bin/autobahn-{}", protocol::version())));
        assert!(command.contains("\" agent;;"));
    }

    #[test]
    fn builds_of_the_version_in_use_keep_only_the_newest() {
        let all = vec![
            "autobahn-0.4.0+e9-bbbbbbbbbbbb".to_owned(),
            "autobahn-0.4.0+e9-aaaaaaaaaaaa".to_owned(),
            "autobahn-0.4.0+e9".to_owned(),
            "autobahn-0.4.0+e8".to_owned(),
        ];
        let (kept, removed) = select_agents(&all, "autobahn-0.4.0+e9", 0);
        assert_eq!(kept, vec!["autobahn-0.4.0+e9-bbbbbbbbbbbb".to_owned()]);
        assert_eq!(removed.len(), 3);
        // A version whose name merely begins the same is another version.
        let (kept, _) = select_agents(&["autobahn-0.4.0+e90".to_owned()], "autobahn-0.4.0+e9", 0);
        assert!(kept.is_empty());
    }

    #[test]
    fn the_remote_command_is_one_posix_word_naming_each_build_by_content() {
        let command = versioned_remote_command();
        assert!(
            command.starts_with("sh -c '") && command.ends_with('\''),
            "{command}"
        );
        // One quoted word: nothing inside closes it early.
        assert_eq!(command.matches('\'').count(), 2, "{command}");
        let version = protocol::version();
        // The running executable is always a candidate for its own
        // platform, named by its digest.
        let executable = std::env::current_exe().expect("the test binary");
        let digest = file_digest(&executable).expect("digestible");
        assert_eq!(digest.len(), DIGEST_LENGTH);
        assert!(
            command.contains(&format!("autobahn-{version}-{digest}"))
                || agent_candidates()
                    .iter()
                    .any(|(platform, _)| *platform == local_platform()),
            "{command}"
        );
        // And an unknown platform falls back to the plain versioned name.
        assert!(command.contains(&format!(
            "*) exec \"$HOME/.autobahn/bin/autobahn-{version}\" agent;;"
        )));
    }

    #[test]
    fn the_remote_command_runs_under_sh_and_picks_this_platform() {
        // Run the script as the remote would, with $HOME pointed at a
        // directory holding a stand-in "agent" under the expected name.
        let home = tempfile::tempdir().expect("a temporary directory");
        let bin = home.path().join(".autobahn/bin");
        fs::create_dir_all(&bin).unwrap();
        let (platform, path) = agent_candidates()
            .into_iter()
            .find(|(platform, _)| *platform == local_platform())
            .expect("the local platform is always a candidate");
        let _ = platform;
        let name = format!(
            "autobahn-{}-{}",
            protocol::version(),
            file_digest(&path).unwrap()
        );
        let stand_in = bin.join(&name);
        fs::write(&stand_in, "#!/bin/sh\necho picked \"$1\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stand_in, fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new("sh")
            .arg("-c")
            .arg(versioned_remote_command())
            .env("HOME", home.path())
            .output()
            .expect("sh runs");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "picked agent"
        );
        // Missing, it fails the way a missing agent always has.
        fs::remove_file(&stand_in).unwrap();
        let output = Command::new("sh")
            .arg("-c")
            .arg(versioned_remote_command())
            .env("HOME", home.path())
            .output()
            .expect("sh runs");
        assert_eq!(output.status.code(), Some(127));
    }

    #[test]
    fn a_manifest_for_another_version_is_refused_before_anything_is_sent() {
        let bundle = tempfile::tempdir().expect("a temporary directory");
        let binary = bundle.path().join("autobahn-linux-x86_64");
        fs::write(&binary, b"an agent").unwrap();
        // No manifest: not checked.
        assert!(check_manifest(&binary, "linux-x86_64").is_ok());
        let digest = blake3::hash(b"an agent").to_hex().to_string();
        let version = protocol::version();
        fs::write(
            bundle.path().join(MANIFEST),
            format!("autobahn-linux-x86_64 {version} {digest}\n"),
        )
        .unwrap();
        assert!(check_manifest(&binary, "linux-x86_64").is_ok());
        fs::write(
            bundle.path().join(MANIFEST),
            format!("autobahn-linux-x86_64 0.0.1+e1 {digest}\n"),
        )
        .unwrap();
        let error = check_manifest(&binary, "linux-x86_64").expect_err("another version");
        assert!(
            format!("{error:#}").contains("is for 0.0.1+e1"),
            "{error:#}"
        );
        fs::write(
            bundle.path().join(MANIFEST),
            format!("autobahn-linux-x86_64 {version} {}\n", "0".repeat(64)),
        )
        .unwrap();
        let error = check_manifest(&binary, "linux-x86_64").expect_err("replaced binary");
        assert!(format!("{error:#}").contains("not the binary"), "{error:#}");
    }

    #[test]
    fn a_compressed_agent_unpacks_to_the_same_bytes() {
        let content: Vec<u8> = (0..200_000u32)
            .flat_map(|i| (i % 97).to_le_bytes())
            .collect();
        let Some(compressed) = gzip(&content) else {
            return; // no gzip on this host: the plain upload is used
        };
        assert!(compressed.len() < content.len() / 2);
        let mut child = Command::new("gzip")
            .arg("-dc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let feeder = std::thread::spawn(move || stdin.write_all(&compressed));
        let output = child.wait_with_output().unwrap();
        feeder.join().unwrap().unwrap();
        assert_eq!(output.stdout, content);
    }
}
