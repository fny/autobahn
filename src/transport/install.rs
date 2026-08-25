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
//! environment variable, an `agents` directory beside the running
//! executable, and — when the remote platform matches the local one — the
//! running executable itself (the common single-platform-fleet case, which
//! needs no bundle at all).

use std::fs;
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
            "no agent binary for {platform} is available (set AUTOBAHN_AGENTS_DIR or place an \
             `agents/autobahn-{platform}` directory beside the executable)"
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
        bail!(
            "unable to probe the platform of {destination}: ssh exited with {}",
            output.status
        );
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
fn local_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Locates the agent binary for a platform: the bundle directory named by
/// `AUTOBAHN_AGENTS_DIR`, an `agents` directory beside the executable, or —
/// for the local platform — the running executable itself.
fn locate_agent_binary(platform: &str) -> Option<PathBuf> {
    let name = format!("autobahn-{platform}");
    if let Ok(directory) = std::env::var("AUTOBAHN_AGENTS_DIR") {
        let candidate = PathBuf::from(directory).join(&name);
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
    // The temporary is uniquified by the remote shell's PID ($$): two
    // controllers bootstrapping the same host concurrently must not stream
    // into one file, or the later `cat` truncates what the earlier one is
    // about to rename into the executable path. With unique temporaries,
    // the final rename is atomic and last-writer-wins with a whole binary.
    let script = format!(
        "mkdir -p ~/.autobahn/bin && \
         tmp=~/.autobahn/bin/.autobahn-tmp-install-{version}-$$ && \
         cat > \"$tmp\" && \
         chmod 755 \"$tmp\" && \
         mv \"$tmp\" ~/.autobahn/bin/autobahn-{version}"
    );
    let mut child = ssh_command(destination, &script)
        .stdin(Stdio::piped())
        .spawn()
        .context("unable to run ssh")?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("ssh standard input unavailable"))?;
    let mut file = fs::File::open(binary)
        .with_context(|| format!("unable to open agent binary {}", binary.display()))?;
    let copy = std::io::copy(&mut file, &mut stdin);
    drop(stdin);
    let status = child.wait().context("unable to wait for ssh")?;
    copy.context("unable to stream the agent binary")?;
    if !status.success() {
        bail!("the installation command exited with {status}");
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
    command.stderr(Stdio::inherit());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

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
