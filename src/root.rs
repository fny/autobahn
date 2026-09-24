//! Refusing to run as root by default.
//!
//! Autobahn is a single-user tool: its state, its configuration and the
//! commands that configuration runs all belong to one user, and the races
//! it accepts (RETAINED §2) are harmless only because nobody with more
//! rights than that user acts on what it scans. Root breaks both halves.
//! Under `sudo` on macOS `$HOME` stays the calling user's, so a root run
//! leaves root-owned state in their `~/.autobahn` that breaks every later
//! run; and a root service reads a configuration the user can edit, whose
//! commands then run as root.
//!
//! So the controller refuses to run as root unless told to, and the agent
//! refuses unless the session sets an owner or group for what it creates,
//! which is the one deployment that needs root and says so in its
//! configuration. Root with a `$HOME` belonging to someone else is the
//! `sudo` case, and is refused whatever was said.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{bail, Result};

use crate::protocol::Initialize;

/// Who is running, as far as these checks care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// The effective uid.
    pub euid: u32,
    /// The owner of `$HOME`, when it is set and exists.
    pub home_owner: Option<u32>,
}

impl Identity {
    /// This process's.
    pub fn current() -> Self {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let euid = unsafe { libc::geteuid() };
        let home_owner = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .and_then(|home| std::fs::metadata(Path::new(&home)).ok())
            .map(|metadata| metadata.uid());
        Self { euid, home_owner }
    }

    /// Refuses root with a home that belongs to another user: `sudo`,
    /// writing root-owned files into that user's state. Nothing excuses it.
    fn refuse_borrowed_home(&self) -> Result<()> {
        if let (0, Some(owner)) = (self.euid, self.home_owner) {
            if owner != 0 {
                bail!(
                    "refusing to run as root with $HOME belonging to uid {owner} (this is what \
                     `sudo` does): the files written would be root's, in that user's \
                     ~/.autobahn, and every later run as that user would fail on them. Run \
                     autobahn as that user instead"
                );
            }
        }
        Ok(())
    }
}

/// The controller's check, for `watch`, `sync`, `resolve`, `install` and
/// `start`. `allowed` is `--allow-root` or `advanced.allow_root`.
pub fn check_controller(identity: Identity, allowed: bool) -> Result<()> {
    identity.refuse_borrowed_home()?;
    if identity.euid == 0 && !allowed {
        bail!(
            "refusing to run as root. Autobahn keeps one user's files in sync and runs the \
             commands in that user's configuration; as root, a configuration anyone else can \
             edit runs as root, and a scanner race becomes a way to write anywhere. Run it as \
             the user whose files these are, or pass --allow-root (or set \
             `advanced.allow_root = true`) if root is really meant"
        );
    }
    Ok(())
}

/// The agent's check, made on every channel it opens: root only when the
/// session asks for an owner or group for what it creates, which only
/// root can give.
pub fn check_agent(identity: Identity, initialize: &Initialize) -> Result<()> {
    identity.refuse_borrowed_home()?;
    if identity.euid == 0
        && initialize.default_owner.is_none()
        && initialize.default_group.is_none()
    {
        bail!(
            "the agent refuses to run as root unless the session sets default_owner or \
             default_group: as root, the races a scanner accepts become a way to write \
             anywhere. Connect as the user whose files these are, or set default_owner in \
             the group's configuration if root is really meant"
        );
    }
    Ok(())
}

/// Whether a configuration file sets `advanced.allow_root = true`, read
/// on its own so that the check does not depend on the rest of the file
/// being valid. A file that cannot be read or parsed does not allow it.
pub fn config_allows_root(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| table.get("advanced")?.get("allow_root")?.as_bool())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: Identity = Identity {
        euid: 0,
        home_owner: Some(0),
    };
    const SUDO: Identity = Identity {
        euid: 0,
        home_owner: Some(501),
    };
    const USER: Identity = Identity {
        euid: 501,
        home_owner: Some(501),
    };

    fn initialize() -> Initialize {
        Initialize {
            root: "/srv".into(),
            session: "0123456789abcdef0123456789abcdef".into(),
            ignores: Vec::new(),
            symlink_mode: crate::scan::SymlinkMode::Raw,
            file_mode: None,
            directory_mode: None,
            side: "beta".into(),
            staging: Default::default(),
            max_file_size: None,
            max_entry_count: None,
            ignore_mounts: true,
            default_owner: None,
            default_group: None,
        }
    }

    #[test]
    fn the_controller_refuses_root_without_the_override() {
        let error = check_controller(ROOT, false).expect_err("refused");
        assert!(format!("{error}").contains("--allow-root"), "{error}");
        check_controller(ROOT, true).expect("allowed when asked");
        check_controller(USER, false).expect("an ordinary user is not asked");
    }

    #[test]
    fn root_with_another_user_s_home_is_refused_even_with_the_override() {
        let error = check_controller(SUDO, true).expect_err("refused");
        assert!(format!("{error}").contains("sudo"), "{error}");
        check_agent(SUDO, &initialize()).expect_err("the agent too");
    }

    #[test]
    fn the_agent_runs_as_root_only_for_a_session_that_sets_an_owner() {
        let plain = initialize();
        let error = check_agent(ROOT, &plain).expect_err("refused");
        assert!(format!("{error}").contains("default_owner"), "{error}");
        check_agent(USER, &plain).expect("an ordinary user is not asked");

        let mut owned = initialize();
        owned.default_owner = Some("www-data".into());
        check_agent(ROOT, &owned).expect("accepted with an owner");
        let mut grouped = initialize();
        grouped.default_group = Some("id:33".into());
        check_agent(ROOT, &grouped).expect("accepted with a group");
    }

    #[test]
    fn the_override_is_read_from_the_configuration() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("config.toml");
        assert!(!config_allows_root(&path), "no file");
        std::fs::write(&path, "[advanced]\nallow_root = true\n").unwrap();
        assert!(config_allows_root(&path));
        std::fs::write(&path, "[advanced]\nallow_root = false\n").unwrap();
        assert!(!config_allows_root(&path));
        std::fs::write(&path, "not toml [").unwrap();
        assert!(!config_allows_root(&path));
    }

    /// This process as it really is, where the tests run as root (a CI
    /// container): the plain agent is refused, the owned one accepted.
    #[test]
    fn as_root_the_agent_without_an_owner_is_refused() {
        let identity = Identity::current();
        if identity.euid != 0 || identity.home_owner.is_some_and(|owner| owner != 0) {
            return;
        }
        check_agent(identity, &initialize()).expect_err("refused as root");
        let mut owned = initialize();
        owned.default_owner = Some("root".into());
        check_agent(identity, &owned).expect("accepted with an owner");
    }
}
