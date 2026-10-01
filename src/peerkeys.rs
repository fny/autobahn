//! Peering keys: SSH keys between the betas that can only run autobahn.
//!
//! Any beta must be able to take the lead, so each reaches every other.
//! With `manage_keys` on, the alpha sets that up itself over the logins it
//! already has (its sessions with each beta):
//!
//! 1. each beta makes a key pair of its own under its peering directory and
//!    hands back the public half, with its SSH host keys
//!    ([`ensure_key`]); private keys never leave the host that made them;
//! 2. each beta is given every *other* beta's public key, in a marked block
//!    of its `~/.ssh/authorized_keys` that autobahn owns, each line forced
//!    through the gate ([`crate::gate`]); a peering `known_hosts` holding
//!    the other betas' host keys; and the gate itself ([`install_peers`]);
//! 3. a beta leading dials the others with its peering key and that
//!    `known_hosts` ([`ssh_options`]).
//!
//! A gated agent refuses both requests, so a peering key never widens its
//! own access: only the alpha, over the user's own login, manages keys.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The key pair's file, under the peering directory.
pub const KEY_FILE: &str = "id_ed25519";
/// The other betas' host keys, under the peering directory.
pub const KNOWN_HOSTS_FILE: &str = "known_hosts";
/// The gate, under `~/.autobahn/bin`.
pub const GATE_BINARY: &str = "autobahn-gate";
/// The forced command every managed line carries.
pub const FORCED: &str = "restrict,command=\"$HOME/.autobahn/bin/autobahn-gate gate\"";
const BEGIN: &str = "# autobahn peering: managed by autobahn, replaced whole on every change";
const END: &str = "# autobahn peering: end";

/// What a host answers about its keys.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostKeys {
    /// Its peering key's public half, as `ssh-keygen` writes it.
    pub public: String,
    /// Its SSH host keys, as `/etc/ssh/ssh_host_*_key.pub` hold them.
    pub host_keys: Vec<String>,
}

/// Makes this host's peering key pair, unless it has one, and answers with
/// its public half and the host's SSH host keys.
pub fn ensure_key(peering_directory: &Path) -> Result<HostKeys> {
    prepare(peering_directory)?;
    let private = peering_directory.join(KEY_FILE);
    let public = private.with_extension("pub");
    if !private.is_file() || !public.is_file() {
        let _ = std::fs::remove_file(&private);
        let _ = std::fs::remove_file(&public);
        let host = hostname();
        let output = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C"])
            .arg(format!("autobahn-peering@{host}"))
            .arg("-f")
            .arg(&private)
            .stdin(std::process::Stdio::null())
            .output()
            .context("unable to run ssh-keygen, which making a peering key needs")?;
        if !output.status.success() {
            bail!(
                "ssh-keygen could not make a peering key: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    let public = std::fs::read_to_string(&public)
        .with_context(|| format!("unable to read {}", public.display()))?
        .trim()
        .to_owned();
    Ok(HostKeys {
        public,
        host_keys: host_keys(Path::new("/etc/ssh")),
    })
}

/// Makes the peering directory, private, and what holds it.
fn prepare(peering_directory: &Path) -> Result<()> {
    if let Some(parent) = peering_directory.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("unable to create {}", parent.display()))?;
    }
    crate::fsutil::private_dir(peering_directory)
        .with_context(|| format!("unable to prepare {}", peering_directory.display()))
}

/// The host's SSH host keys, each `<type> <key>`, from `ssh_host_*_key.pub`.
fn host_keys(directory: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("ssh_host_") && name.ends_with("_key.pub")
        })
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|text| {
            let mut words = text.split_whitespace();
            Some(format!("{} {}", words.next()?, words.next()?))
        })
        .filter(|key| is_key(key))
        .collect();
    keys.sort();
    keys
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty() && is_word(name))
        .unwrap_or_else(|| "host".to_owned())
}

/// Installs what the alpha sends a beta: `authorized`, the other betas'
/// keys, as the marked block of `~/.ssh/authorized_keys` under `home`,
/// replacing the block there and nothing else; `known_hosts`, the other
/// betas' host keys, as the peering directory's `known_hosts`; and the gate,
/// as a copy of this executable. Every line is checked to be what the alpha
/// makes — a key forced through the gate, a host and its key — so nothing
/// else can be written there this way.
pub fn install_peers(
    home: &Path,
    peering_directory: &Path,
    authorized: &[String],
    known_hosts: &[String],
) -> Result<()> {
    for line in authorized {
        let Some(rest) = line
            .strip_prefix(FORCED)
            .and_then(|rest| rest.strip_prefix(' '))
        else {
            bail!("refusing an authorized key not forced through the gate: {line:?}");
        };
        let mut words = rest.split(' ');
        let (Some(kind), Some(key), comment) = (words.next(), words.next(), words.next()) else {
            bail!("refusing a malformed authorized key: {line:?}");
        };
        if !is_key(&format!("{kind} {key}"))
            || comment.is_some_and(|comment| !is_word(comment))
            || words.next().is_some()
        {
            bail!("refusing a malformed authorized key: {line:?}");
        }
    }
    for line in known_hosts {
        let Some((host, key)) = line.split_once(' ') else {
            bail!("refusing a malformed known host: {line:?}");
        };
        if !is_word(host) || !is_key(key) {
            bail!("refusing a malformed known host: {line:?}");
        }
    }
    write_block(&home.join(".ssh"), authorized)?;
    let mut text = known_hosts.join("\n");
    text.push('\n');
    prepare(peering_directory)?;
    std::fs::write(peering_directory.join(KNOWN_HOSTS_FILE), text)
        .context("unable to write the peering known_hosts")?;
    install_gate(&home.join(".autobahn").join("bin"))
}

/// Replaces the marked block of `authorized_keys` in `ssh` with `lines`,
/// keeping every other line as it was, written whole and renamed into place.
fn write_block(ssh: &Path, lines: &[String]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !ssh.exists() {
        std::fs::create_dir_all(ssh)
            .with_context(|| format!("unable to create {}", ssh.display()))?;
        std::fs::set_permissions(ssh, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = ssh.join("authorized_keys");
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "{} is a symbolic link, managed elsewhere; add these lines yourself:\n{}",
            path.display(),
            lines.join("\n")
        );
    }
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("unable to read {}", path.display()))
        }
    };
    let mut kept = Vec::new();
    let mut inside = false;
    for line in existing.lines() {
        match line {
            BEGIN => inside = true,
            END => inside = false,
            _ if !inside => kept.push(line),
            _ => {}
        }
    }
    let mut text: String = kept.iter().map(|line| format!("{line}\n")).collect();
    if !lines.is_empty() {
        text.push_str(BEGIN);
        text.push('\n');
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        text.push_str(END);
        text.push('\n');
    }
    let temporary = ssh.join(format!(".authorized_keys.autobahn.{}", std::process::id()));
    std::fs::write(&temporary, text)
        .with_context(|| format!("unable to write {}", temporary.display()))?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&temporary, &path).with_context(|| {
        let _ = std::fs::remove_file(&temporary);
        format!(
            "unable to replace {}; add these lines yourself:\n{}",
            path.display(),
            lines.join("\n")
        )
    })
}

/// Puts this executable at `bin/autobahn-gate`, unless it is there already.
fn install_gate(bin: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let source = std::env::current_exe().context("unable to find this executable")?;
    let target = bin.join(GATE_BINARY);
    let wanted =
        std::fs::read(&source).with_context(|| format!("unable to read {}", source.display()))?;
    if std::fs::read(&target).is_ok_and(|held| held == wanted) {
        return Ok(());
    }
    std::fs::create_dir_all(bin).with_context(|| format!("unable to create {}", bin.display()))?;
    let temporary = bin.join(format!(".{GATE_BINARY}.{}", std::process::id()));
    std::fs::write(&temporary, &wanted)
        .with_context(|| format!("unable to write {}", temporary.display()))?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&temporary, &target)
        .with_context(|| format!("unable to install {}", target.display()))
}

/// The line authorizing `public`, a beta's peering key, through the gate.
pub fn authorized_line(public: &str, host: &str) -> String {
    let mut words = public.split_whitespace();
    let kind = words.next().unwrap_or_default();
    let key = words.next().unwrap_or_default();
    format!("{FORCED} {kind} {key} autobahn-peering:{host}")
}

/// What one beta is given: every other beta's peering key, forced through
/// the gate, and every other beta's host keys, by its host name. `keys` is
/// every beta's, by host; the beta's own are left out.
pub fn block_for(host: &str, keys: &BTreeMap<String, HostKeys>) -> (Vec<String>, Vec<String>) {
    let mut authorized = Vec::new();
    let mut known_hosts = Vec::new();
    for (other, held) in keys {
        if other == host {
            continue;
        }
        authorized.push(authorized_line(&held.public, other));
        let name = other
            .rsplit_once('@')
            .map_or(other.as_str(), |(_, name)| name);
        for key in &held.host_keys {
            known_hosts.push(format!("{name} {key}"));
        }
    }
    (authorized, known_hosts)
}

/// The ssh options a beta leading from here dials the others with: its
/// peering key, tried first, and the peering `known_hosts` beside the
/// user's own. None when this host has no peering key.
pub fn ssh_options(peering_directory: &Path) -> Option<Vec<String>> {
    let key: PathBuf = peering_directory.join(KEY_FILE);
    if !key.is_file() {
        return None;
    }
    let known = peering_directory.join(KNOWN_HOSTS_FILE);
    let mut options = vec!["-i".to_owned(), key.to_string_lossy().into_owned()];
    if known.is_file() {
        options.push("-o".to_owned());
        options.push(format!(
            "UserKnownHostsFile=\"{}\" ~/.ssh/known_hosts",
            known.display()
        ));
    }
    Some(options)
}

/// `<type> <base64>`, as a key's public half is written.
fn is_key(value: &str) -> bool {
    let Some((kind, key)) = value.split_once(' ') else {
        return false;
    };
    let kind_ok = matches!(kind, "ssh-ed25519" | "ssh-rsa")
        || kind.starts_with("ecdsa-sha2-nistp")
        || kind.starts_with("sk-");
    kind_ok
        && !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
}

/// A host name or a key's comment: no spaces, quotes or control characters.
fn is_word(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | ':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    fn keys(public: &str) -> HostKeys {
        HostKeys {
            public: format!("{public} autobahn-peering@x"),
            host_keys: vec![public.to_owned()],
        }
    }

    /// A beta is given every other beta's key, forced through the gate, and
    /// their host keys by host name — never its own.
    #[test]
    fn a_beta_is_given_every_other_betas_key_through_the_gate() {
        let mut known = BTreeMap::new();
        known.insert("u@one".to_owned(), keys(KEY));
        known.insert("two".to_owned(), keys(&KEY.replace("Jl", "Jm")));
        let (authorized, known_hosts) = block_for("u@one", &known);
        assert_eq!(
            authorized,
            [format!(
                "{FORCED} {} autobahn-peering:two",
                KEY.replace("Jl", "Jm")
            )]
        );
        assert_eq!(known_hosts, [format!("two {}", KEY.replace("Jl", "Jm"))]);
        let (authorized, known_hosts) = block_for("two", &known);
        assert_eq!(
            authorized,
            [format!("{FORCED} {KEY} autobahn-peering:u@one")]
        );
        assert_eq!(known_hosts, [format!("one {KEY}")]);
    }

    /// Installing replaces autobahn's block and nothing else, refuses any
    /// line that is not a key forced through the gate or a host and its
    /// key, and puts the gate in place.
    #[test]
    fn installing_replaces_only_the_managed_block() {
        let keep = tempfile::tempdir().unwrap();
        let home = keep.path();
        let peering = home.join(".autobahn/peering");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/authorized_keys"), "ssh-ed25519 AAAA mine\n").unwrap();
        let line = authorized_line(KEY, "two");
        let host = format!("two {KEY}");
        let (lines, hosts) = (vec![line.clone()], vec![host.clone()]);
        install_peers(home, &peering, &lines, &hosts).unwrap();
        install_peers(home, &peering, &lines, &hosts).unwrap();
        let held = std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap();
        assert_eq!(
            held,
            format!("ssh-ed25519 AAAA mine\n{BEGIN}\n{line}\n{END}\n")
        );
        assert_eq!(
            std::fs::read_to_string(peering.join(KNOWN_HOSTS_FILE)).unwrap(),
            format!("{host}\n")
        );
        assert!(home.join(".autobahn/bin").join(GATE_BINARY).is_file());

        for (authorized, known) in [
            (vec![format!("{KEY} unforced")], vec![]),
            (vec![format!("command=\"sh\" {KEY}")], vec![]),
            (vec![format!("{line}\nssh-ed25519 AAAA smuggled")], vec![]),
            (vec![], vec![format!("two {KEY}\nevil {KEY}")]),
            (vec![], vec!["two not-a-key".to_owned()]),
        ] {
            assert!(
                install_peers(home, &peering, &authorized, &known).is_err(),
                "{authorized:?} {known:?}"
            );
        }
        let held_after = std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap();
        assert_eq!(held_after, held, "a refusal writes nothing");

        install_peers(home, &peering, &[], &[]).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap(),
            "ssh-ed25519 AAAA mine\n"
        );
    }

    /// A host makes its peering key once and keeps it; the options a beta
    /// leading from here dials with name it, once there is one.
    #[test]
    fn a_host_makes_its_key_once() {
        let keep = tempfile::tempdir().unwrap();
        let peering = keep.path().join("peering");
        assert_eq!(ssh_options(&peering), None);
        let first = ensure_key(&peering).expect("ssh-keygen makes a key");
        assert!(first.public.starts_with("ssh-ed25519 "), "{}", first.public);
        assert_eq!(ensure_key(&peering).unwrap().public, first.public);
        let options = ssh_options(&peering).expect("a key");
        assert_eq!(options[0], "-i");
        assert!(options[1].ends_with(KEY_FILE));
    }
}
