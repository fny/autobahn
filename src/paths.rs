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

/// Returns the default configuration file path,
/// `~/.autobahn/config.toml` — everything autobahn keeps (configuration,
/// session state, status, installed agents, staging) lives under the one
/// `~/.autobahn` directory, on every machine it touches.
pub fn default_config_path() -> Result<PathBuf> {
    Ok(absolute_home()?.join(".autobahn").join("config.toml"))
}

/// Returns the default state root, under which session state
/// (`sessions/<id>/`) and supervisor status (`status/`) are kept.
pub fn default_state_root() -> Result<PathBuf> {
    Ok(absolute_home()?.join(".autobahn"))
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
}
