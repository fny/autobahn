//! Filesystem location conventions: home-relative path expansion and the
//! default locations of the configuration file and supervisor state.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Expands a leading `~` (the current user's home directory) in a path.
///
/// Only `~` and `~/...` are expanded — `~user` forms are passed through
/// untouched, as is any path that doesn't begin with a tilde. Expansion is
/// against `HOME`, so on a remote agent it resolves against the *agent's*
/// home directory, which is what makes home-relative roots in a group
/// configuration mean the right thing on every host they fan out to. A
/// `HOME` that isn't an absolute path is an error: expanding against it
/// would silently produce a working-directory-relative root.
pub fn expand_tilde(path: &str) -> Result<PathBuf> {
    if path != "~" && !path.starts_with("~/") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var("HOME").context("unable to expand ~ (HOME is not set)")?;
    expand_tilde_in(path, &home)
}

/// Expands a leading `~` against an explicit home directory.
fn expand_tilde_in(path: &str, home: &str) -> Result<PathBuf> {
    if !home.starts_with('/') {
        anyhow::bail!("unable to expand ~: HOME ({home:?}) is not an absolute path");
    }
    if path == "~" {
        return Ok(PathBuf::from(home));
    }
    Ok(PathBuf::from(home).join(&path["~/".len()..]))
}

/// The environment variable that moves everything autobahn keeps. The
/// installer honours it for the agent bundle; the binary honours it for
/// the state root and the configuration, so one setting moves the whole
/// directory and the two never disagree about where the agents are.
pub const HOME_VARIABLE: &str = "AUTOBAHN_HOME";

/// Returns the default configuration file path, `config.toml` under the
/// state root — everything autobahn keeps (configuration, session state,
/// status, installed agents, staging) lives under the one directory, on
/// every machine it touches.
pub fn default_config_path() -> Result<PathBuf> {
    Ok(default_state_root()?.join("config.toml"))
}

/// Returns the default state root, under which session state
/// (`sessions/<id>/`) and supervisor status (`status/`) are kept:
/// `$AUTOBAHN_HOME` when it is set and non-empty, and `~/.autobahn`
/// otherwise.
pub fn default_state_root() -> Result<PathBuf> {
    match home_override()? {
        Some(home) => Ok(home),
        None => Ok(absolute_home()?.join(".autobahn")),
    }
}

/// `$AUTOBAHN_HOME`, when it is set to something. Absolute, for the same
/// reason `HOME` must be: every default path is derived from it, and a
/// relative one would make them all working-directory-dependent — and a
/// login service starts in a working directory nobody chose.
pub fn home_override() -> Result<Option<PathBuf>> {
    match std::env::var(HOME_VARIABLE) {
        Ok(value) => home_override_from(&value),
        Err(_) => Ok(None),
    }
}

fn home_override_from(value: &str) -> Result<Option<PathBuf>> {
    if value.is_empty() {
        return Ok(None);
    }
    if !value.starts_with('/') {
        anyhow::bail!("{HOME_VARIABLE} ({value:?}) is not an absolute path");
    }
    Ok(Some(PathBuf::from(value)))
}

/// The directories under the state root that hold what autobahn keeps
/// about hosts, paths and sessions, and are made private at startup with
/// the root itself.
pub const PRIVATE_SUBDIRECTORIES: [&str; 4] = ["sessions", "status", "staging", "peering"];

/// Makes the state root, and each of [`PRIVATE_SUBDIRECTORIES`] under it,
/// a directory only the current user can use (see
/// [`crate::fsutil::private_dir`]), then sweeps its `tmp/` of old scratch
/// files. Every entry point that touches state calls this first, so the
/// root is private because it was made that way, not as a side effect of
/// whichever command happened to run first.
///
/// Missing parents of the root are created as ordinary directories: they
/// are not autobahn's, and an explicit `--state-root` may name a path
/// that does not exist yet. A root or subdirectory that is a symbolic
/// link or owned by another user is an error, and a looser one is
/// tightened with a warning. A failed sweep is reported and does not stop
/// the command.
pub fn prepare_state_root(root: &std::path::Path) -> Result<()> {
    if let Some(parent) = root.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("unable to create {}", parent.display()))?;
        }
    }
    crate::fsutil::private_dir(root)?;
    for name in PRIVATE_SUBDIRECTORIES {
        crate::fsutil::private_dir(&root.join(name))?;
    }
    if let Err(error) = crate::fsutil::sweep_private_tmp(root, crate::fsutil::TMP_MAX_AGE) {
        crate::complain!("warning: {error:#}");
    }
    Ok(())
}

/// Describes why a file others can write to should not be trusted, when
/// they can: the file, or the directory it is in, is writable by its group
/// or by everyone. `None` when neither is, or when either cannot be
/// inspected (reading the file reports that better).
///
/// The configuration holds commands autobahn runs (`on_alert`,
/// `agent_command`), so whoever can write it can run commands as this
/// user — the check ssh makes of its own configuration.
pub fn loose_write_permissions(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let writable = |path: &std::path::Path| {
        std::fs::metadata(path)
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o7777)
            .filter(|mode| mode & 0o022 != 0)
    };
    if let Some(mode) = writable(path) {
        return Some(format!(
            "{} is mode {mode:04o}, writable by others",
            path.display()
        ));
    }
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => std::path::Path::new("."),
    };
    writable(directory).map(|mode| {
        format!(
            "{} is in {}, which is mode {mode:04o}, writable by others",
            path.display(),
            directory.display()
        )
    })
}

/// Resolves a path to its physical identity: fully canonicalized when it
/// exists, and otherwise the canonicalized deepest *existing* ancestor with
/// the missing suffix reappended (lexically normalized).
///
/// This is the path form that session identity and duplicate detection are
/// built on. Textual comparison isn't enough — `/data/.`, a symlink alias,
/// and `~/data` versus its expansion all denote one directory — and plain
/// canonicalization isn't either, because a root that doesn't exist *yet*
/// (one a transition will create) still has an identity: the place it will
/// be created, which lives under its existing ancestors' resolved form.
/// Without ancestor resolution, `/real/new` and `/alias/new` (where `alias`
/// is a symlink to `real`) would get distinct identities while denoting the
/// same future directory.
pub fn resolve_for_identity(path: &std::path::Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    let normalized: PathBuf = path.components().collect();
    let mut prefix = normalized.as_path();
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    while let Some(parent) = prefix.parent() {
        let Some(name) = prefix.file_name() else {
            break;
        };
        suffix.push(name.to_owned());
        if let Ok(resolved) = std::fs::canonicalize(parent) {
            let mut result = resolved;
            for name in suffix.iter().rev() {
                result.push(name);
            }
            return result;
        }
        prefix = parent;
    }
    normalized
}

/// Returns the current user's home directory, requiring it to be absolute:
/// every default path is derived from it, and a relative `HOME` would make
/// them all silently working-directory-dependent.
fn absolute_home() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    if !home.starts_with('/') {
        anyhow::bail!("HOME ({home:?}) is not an absolute path");
    }
    Ok(PathBuf::from(home))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        let home = std::env::var("HOME").expect("HOME should be set in tests");
        assert_eq!(expand_tilde("~").unwrap(), PathBuf::from(&home));
        assert_eq!(
            expand_tilde("~/projects/x").unwrap(),
            PathBuf::from(&home).join("projects/x")
        );
        assert_eq!(
            expand_tilde("/absolute").unwrap(),
            PathBuf::from("/absolute")
        );
        assert_eq!(
            expand_tilde("relative/x").unwrap(),
            PathBuf::from("relative/x")
        );
        // ~user forms are not expanded.
        assert_eq!(expand_tilde("~other/x").unwrap(), PathBuf::from("~other/x"));
    }

    /// `AUTOBAHN_HOME` moves the state root; unset or empty, it is not
    /// there at all; relative, it is refused like a relative `HOME`.
    #[test]
    fn autobahn_home_is_absolute_or_absent() {
        assert_eq!(
            home_override_from("/srv/autobahn").unwrap(),
            Some(PathBuf::from("/srv/autobahn"))
        );
        assert_eq!(home_override_from("").unwrap(), None);
        assert!(home_override_from("state").is_err());
    }

    #[test]
    fn a_non_absolute_home_is_an_error_not_a_relative_root() {
        assert!(expand_tilde_in("~/x", "relative-home").is_err());
        assert!(expand_tilde_in("~", "").is_err());
        assert_eq!(
            expand_tilde_in("~/x", "/home/user").unwrap(),
            PathBuf::from("/home/user/x")
        );
    }

    #[test]
    fn identity_resolution_sees_through_ancestors_of_missing_paths() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let real = keep.path().join("real");
        std::fs::create_dir_all(&real).expect("directory should be creatable");
        let alias = keep.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("symlink should be creatable");

        // An existing path fully canonicalizes.
        assert_eq!(
            resolve_for_identity(&alias),
            std::fs::canonicalize(&real).unwrap()
        );
        // A missing leaf under a symlinked ancestor resolves to the same
        // identity as the direct spelling — this is exactly the case where
        // whole-path canonicalization fails and textual comparison lies.
        assert_eq!(
            resolve_for_identity(&alias.join("new/deeper")),
            resolve_for_identity(&real.join("new/deeper")),
        );
        // Dot components are normalized even when nothing exists.
        assert_eq!(
            resolve_for_identity(std::path::Path::new("/nonexistent/./x")),
            PathBuf::from("/nonexistent/x")
        );
    }

    fn mode(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path)
            .expect("the path should exist")
            .permissions()
            .mode()
            & 0o7777
    }

    #[test]
    fn the_state_root_and_its_subdirectories_are_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("nested").join(".autobahn");
        prepare_state_root(&root).expect("the state root should be preparable");
        assert_eq!(mode(&root), 0o700);
        for name in PRIVATE_SUBDIRECTORIES {
            assert_eq!(mode(&root.join(name)), 0o700, "{name}");
        }
        // One left readable by an older build is tightened.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(
            root.join("sessions"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        prepare_state_root(&root).expect("the state root should be preparable");
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("sessions")), 0o700);
    }

    #[test]
    fn a_state_root_that_is_a_link_is_refused() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let elsewhere = keep.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let root = keep.path().join(".autobahn");
        std::os::unix::fs::symlink(&elsewhere, &root).unwrap();
        assert!(prepare_state_root(&root).is_err());
        assert!(!elsewhere.join("sessions").exists());
    }

    #[test]
    fn a_file_or_directory_others_can_write_is_named() {
        use std::os::unix::fs::PermissionsExt;
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let directory = keep.path().join("home");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = directory.join("config.toml");
        std::fs::write(&config, b"").unwrap();
        for private in [0o600, 0o644] {
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(private)).unwrap();
            assert_eq!(loose_write_permissions(&config), None, "{private:04o}");
        }
        for loose in [0o620, 0o602, 0o666] {
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(loose)).unwrap();
            let warning = loose_write_permissions(&config).expect("a loose file is named");
            assert!(warning.contains(&format!("{loose:04o}")), "{warning}");
        }
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o777)).unwrap();
        let warning = loose_write_permissions(&config).expect("a loose directory is named");
        assert!(
            warning.contains(&directory.display().to_string()),
            "{warning}"
        );
    }
}
