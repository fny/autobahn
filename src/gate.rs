//! What a restricted key may run.
//!
//! A peering key is restricted in `authorized_keys` to one command,
//! `autobahn-gate gate`, so that whoever holds it — another peer that leads,
//! compromised or not — can run autobahn's agent there and nothing else. The
//! gate reads what the connection asked for (`SSH_ORIGINAL_COMMAND`) and
//! runs it only when it is one of:
//!
//! - **The agent**, asked for exactly as a controller asks: the script
//!   [`crate::transport::install::agent_script`] builds. The gate parses the
//!   version and binaries out of it, builds the script again, and requires
//!   the two to match to the byte; then it runs the installed binary itself,
//!   without a shell, marked as gated.
//! - **`autobahn peering attach`**, which the alpha runs on a leader.
//! - **`autobahn gate install <release> <version>`**: an agent installed
//!   from autobahn's signed release, downloaded and checked here
//!   ([`crate::update::install_release_agent`]). A controller behind the
//!   gate names a release; it never sends a binary.
//!
//! Anything else is refused, saying [`REFUSAL`] so that a controller can
//! tell a gate from a broken host. The agent a gate runs knows it is gated
//! ([`GATED_VARIABLE`]) and refuses to manage keys: a peering key cannot
//! widen its own access.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

/// How every refusal begins, so a controller knows it met a gate.
pub const REFUSAL: &str = "autobahn gate:";

/// Set for the agent a gate runs.
pub const GATED_VARIABLE: &str = "AUTOBAHN_GATED";

/// Whether this process was started by a gate.
pub fn gated() -> bool {
    std::env::var_os(GATED_VARIABLE).is_some()
}

/// What a request may run.
#[derive(Debug, PartialEq, Eq)]
pub enum Allowed {
    /// The agent: this binary.
    Agent(PathBuf),
    /// `autobahn peering attach`.
    Attach,
    /// An agent installed from a signed release: the release's tag, the
    /// version asked for, and where it goes.
    Install {
        tag: String,
        version: String,
        target: PathBuf,
    },
}

/// Runs what the connection asked for, if it may. Does not return when it
/// runs the agent or the attachment: the process becomes them.
pub fn run() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let requested = std::env::var("SSH_ORIGINAL_COMMAND").unwrap_or_default();
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("{REFUSAL} no home directory"))?;
    let error = match judge(
        &requested,
        &home,
        &crate::transport::install::local_platform(),
    )? {
        Allowed::Agent(binary) => std::process::Command::new(&binary)
            .arg("agent")
            .env(GATED_VARIABLE, "1")
            .exec(),
        Allowed::Attach => std::process::Command::new(
            std::env::current_exe().context("unable to find this executable")?,
        )
        .args(["peering", "attach"])
        .env(GATED_VARIABLE, "1")
        .exec(),
        Allowed::Install {
            tag,
            version,
            target,
        } => {
            let reported = crate::update::install_release_agent(&tag, &version, &target)?;
            println!(
                "installed autobahn {reported} from {tag} at {}",
                target.display()
            );
            return Ok(());
        }
    };
    Err(error).context("unable to run what the gate allowed")
}

/// Decides what a request may run, with `home` the server user's home and
/// `platform` this machine's, in bundle naming.
pub fn judge(requested: &str, home: &Path, platform: &str) -> Result<Allowed> {
    let bin = home.join(".autobahn").join("bin");
    if requested == "autobahn peering attach" {
        return Ok(Allowed::Attach);
    }
    if let Some(rest) = requested.strip_prefix("autobahn gate install ") {
        let mut words = rest.split(' ');
        if let (Some(tag), Some(version), None) = (words.next(), words.next(), words.next()) {
            if is_tag(tag) && is_name(version) {
                return Ok(Allowed::Install {
                    tag: tag.to_owned(),
                    version: version.to_owned(),
                    target: bin.join(format!("autobahn-{version}")),
                });
            }
        }
    }
    if let Some((version, branches)) = parse_agent_script(requested) {
        if crate::transport::install::agent_script(&version, &branches) == requested {
            let built = branches
                .iter()
                .find(|(branch, _)| branch == platform)
                .map(|(_, digest)| bin.join(format!("autobahn-{version}-{digest}")))
                .filter(|path| path.is_file());
            let binary = built.unwrap_or_else(|| bin.join(format!("autobahn-{version}")));
            if !binary.is_file() {
                bail!(
                    "{REFUSAL} autobahn {version} is not installed here; ask for it with \
                     `autobahn gate install v<release> {version}`"
                );
            }
            return Ok(Allowed::Agent(binary));
        }
    }
    bail!(
        "{REFUSAL} refusing {requested:?}: this key runs only the autobahn agent, `autobahn \
         peering attach`, and `autobahn gate install <release> <version>`"
    )
}

/// The version and the (platform, digest) branches of an agent script,
/// read without trusting its shape — [`judge`] builds the script again
/// from them and compares.
fn parse_agent_script(requested: &str) -> Option<(String, Vec<(String, String)>)> {
    let fallback = "*) exec \"$HOME/.autobahn/bin/autobahn-";
    let marker = "case $s-$m in ";
    let middle_start = requested.find(marker)? + marker.len();
    let fallback_start = requested.rfind(fallback)?;
    let version = requested[fallback_start + fallback.len()..]
        .strip_suffix("\" agent;; esac'")?
        .to_owned();
    if !is_name(&version) {
        return None;
    }
    let mut branches = Vec::new();
    for branch in requested
        .get(middle_start..fallback_start)?
        .split(";; ")
        .filter(|branch| !branch.is_empty())
    {
        let (platform, command) = branch.split_once(") exec \"$HOME/.autobahn/bin/autobahn-")?;
        let digest = command
            .strip_suffix("\" agent")?
            .strip_prefix(&format!("{version}-"))?;
        let platform_ok = !platform.is_empty()
            && platform
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        let digest_ok = !digest.is_empty() && digest.chars().all(|c| c.is_ascii_hexdigit());
        if !platform_ok || !digest_ok {
            return None;
        }
        branches.push((platform.to_owned(), digest.to_owned()));
    }
    Some((version, branches))
}

/// A version as it names a file: letters, digits, `.`, `+`, `-` and `_`.
fn is_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-' | '_'))
        && !value.starts_with('.')
}

/// A release tag: `v1.2.3`, or `v1.2.3-dev.1`.
fn is_tag(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('v') else {
        return false;
    };
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        && pre.is_none_or(|pre| {
            !pre.is_empty() && pre.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERSION: &str = "0.4.0+e16";

    fn script(branches: &[(&str, &str)]) -> String {
        let branches: Vec<(String, String)> = branches
            .iter()
            .map(|(platform, digest)| (platform.to_string(), digest.to_string()))
            .collect();
        crate::transport::install::agent_script(VERSION, &branches)
    }

    /// The agent, asked for exactly as a controller asks, runs the binary
    /// installed for this platform — the one built for it, or else the
    /// version's plain name — and a request for one not installed says how
    /// to install it.
    #[test]
    fn the_agent_runs_the_binary_installed_for_this_platform() {
        let keep = tempfile::tempdir().unwrap();
        let bin = keep.path().join(".autobahn/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let asked = script(&[("linux-x86_64", "abc123"), ("darwin-aarch64", "def456")]);

        let error = judge(&asked, keep.path(), "linux-x86_64").expect_err("nothing installed");
        assert!(format!("{error:#}").contains("gate install"), "{error:#}");

        std::fs::write(bin.join(format!("autobahn-{VERSION}")), "").unwrap();
        assert_eq!(
            judge(&asked, keep.path(), "linux-x86_64").unwrap(),
            Allowed::Agent(bin.join(format!("autobahn-{VERSION}")))
        );
        std::fs::write(bin.join(format!("autobahn-{VERSION}-abc123")), "").unwrap();
        assert_eq!(
            judge(&asked, keep.path(), "linux-x86_64").unwrap(),
            Allowed::Agent(bin.join(format!("autobahn-{VERSION}-abc123")))
        );
        assert_eq!(
            judge(&script(&[]), keep.path(), "linux-x86_64").unwrap(),
            Allowed::Agent(bin.join(format!("autobahn-{VERSION}")))
        );
    }

    /// Anything but the exact script, the exact attachment, or a
    /// well-formed install is refused, saying it met a gate.
    #[test]
    fn anything_else_is_refused() {
        let keep = tempfile::tempdir().unwrap();
        let bin = keep.path().join(".autobahn/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(format!("autobahn-{VERSION}")), "").unwrap();
        let asked = script(&[("linux-x86_64", "abc123")]);
        for refused in [
            String::new(),
            "id".to_owned(),
            "sh -c 'mkdir -p ~/.autobahn/bin && cat > ~/.autobahn/bin/x'".to_owned(),
            format!("{asked}; id"),
            asked.replace("abc123", "abc123\" agent;; x) exec \"/bin/sh"),
            asked.replace(VERSION, "../../bin/sh"),
            asked.replace("exec", "eval"),
            asked.replacen(' ', "  ", 1),
            "autobahn peering attach; id".to_owned(),
            "autobahn gate install v1.2 0.4.0".to_owned(),
            "autobahn gate install 0.4.0 0.4.0".to_owned(),
            "autobahn gate install v0.4.0 ../x".to_owned(),
            "autobahn gate install v0.4.0;id 0.4.0".to_owned(),
            "autobahn gate install v0.4.0 0.4.0 extra".to_owned(),
        ] {
            let error = judge(&refused, keep.path(), "linux-x86_64").expect_err(&refused);
            assert!(
                format!("{error:#}").starts_with(REFUSAL),
                "{refused:?}: {error:#}"
            );
        }
        assert_eq!(
            judge("autobahn peering attach", keep.path(), "linux-x86_64").unwrap(),
            Allowed::Attach
        );
        assert_eq!(
            judge(
                "autobahn gate install v0.4.0-dev.1 0.4.0+e16",
                keep.path(),
                "linux-x86_64"
            )
            .unwrap(),
            Allowed::Install {
                tag: "v0.4.0-dev.1".into(),
                version: "0.4.0+e16".into(),
                target: bin.join("autobahn-0.4.0+e16"),
            }
        );
    }
}
