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

/// Returns the remote command invoking this version's installed agent. The
/// path is home-relative and unquoted, expanded by the remote login shell.
pub fn versioned_remote_command() -> String {
    format!("~/.autobahn/bin/autobahn-{} agent", protocol::version())
}

/// Ensures this version's agent is installed on the remote host: probes the
/// platform, locates a matching binary locally, and streams it into place
/// over a single SSH connection.
pub fn ensure_agent(destination: &str) -> Result<()> {
    let platform = probe_platform(destination)?;
    let binary = locate_agent_binary(&platform).ok_or_else(|| {
        anyhow!(
            "no agent binary for {platform} is available (install the agent bundle into \
             ~/.autobahn/agents, set AUTOBAHN_AGENTS_DIR, or place an `agents` directory \
             containing autobahn-{platform} beside the executable)"
        )
    })?;
    upload_agent(destination, &binary)
        .with_context(|| format!("unable to install the {platform} agent on {destination}"))
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
    let name = format!("autobahn-{platform}");
    if let Ok(directory) = std::env::var("AUTOBAHN_AGENTS_DIR") {
        let candidate = PathBuf::from(directory).join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Ok(state_root) = crate::paths::default_state_root() {
        let candidate = state_root.join("agents").join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            let candidate = directory.join("agents").join(&name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if platform == local_platform() {
            return Some(executable);
        }
    }
    None
}

/// Streams a binary to the remote host's versioned agent path over one SSH
/// connection: written to a temporary alongside the destination, made
/// executable, and renamed into place (so a concurrent controller never
/// observes a partial binary).
fn upload_agent(destination: &str, binary: &std::path::Path) -> Result<()> {
    let version = protocol::version();
    // The binary is read into memory once, so the bytes streamed are
    // exactly the bytes measured — a bundle replaced or truncated mid-read
    // can't smuggle a partial binary past the check.
    let content = fs::read(binary)
        .with_context(|| format!("unable to read agent binary {}", binary.display()))?;
    // The temporary is uniquified by the remote shell's PID ($$): two
    // controllers bootstrapping the same host concurrently must not stream
    // into one file, or the later `cat` truncates what the earlier one is
    // about to rename into the executable path. The remote length check
    // catches a stream cut short (a dropped connection, a killed ssh)
    // before anything is published; the rename is atomic and
    // last-writer-wins with a verified whole binary.
    let script = format!(
        "mkdir -p ~/.autobahn/bin && \
         tmp=~/.autobahn/bin/.autobahn-tmp-install-{version}-$$ && \
         cat > \"$tmp\" && \
         [ \"$(wc -c < \"$tmp\")\" -eq {length} ] || {{ rm -f \"$tmp\"; exit 70; }} && \
         chmod 755 \"$tmp\" && \
         mv \"$tmp\" ~/.autobahn/bin/autobahn-{version}",
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
    let write = stdin.write_all(&content);
    drop(stdin);
    let status = child.wait().context("unable to wait for ssh")?;
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

    #[test]
    fn the_versioned_command_names_this_build() {
        let command = versioned_remote_command();
        assert!(command.starts_with("~/.autobahn/bin/autobahn-"));
        assert!(command.ends_with(" agent"));
        assert!(command.contains(&protocol::version()));
    }
}
