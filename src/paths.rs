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

/// Returns the default configuration file path:
/// `$XDG_CONFIG_HOME/autobahn/config.toml`, falling back to
/// `~/.config/autobahn/config.toml` per the XDG base directory
/// specification (which also requires `XDG_CONFIG_HOME` to be absolute —
/// a relative value is ignored rather than resolved against the working
/// directory). An absolute `XDG_CONFIG_HOME` needs no `HOME` at all, and
/// the `HOME` fallback must itself be absolute — a relative home would
/// make the configuration path depend on the working directory.
pub fn default_config_path() -> Result<PathBuf> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    config_path_from(xdg.as_deref(), home.as_deref())
}

/// Computes the configuration path from explicit environment values.
fn config_path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
    if let Some(directory) = xdg_config_home {
        if directory.starts_with('/') {
            return Ok(PathBuf::from(directory)
                .join("autobahn")
                .join("config.toml"));
        }
    }
    let home = home.context("HOME is not set")?;
    if !home.starts_with('/') {
        anyhow::bail!("HOME ({home:?}) is not an absolute path");
    }
    Ok(PathBuf::from(home)
        .join(".config")
        .join("autobahn")
        .join("config.toml"))
}

/// Returns the default state root, under which session state
/// (`sessions/<id>/`) and supervisor status (`status/`) are kept.
pub fn default_state_root() -> Result<PathBuf> {
    Ok(absolute_home()?.join(".autobahn"))
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
    fn relative_xdg_config_home_is_ignored_per_the_specification() {
        // An absolute XDG_CONFIG_HOME wins and needs no HOME at all.
        assert_eq!(
            config_path_from(Some("/etc/xdg"), None).unwrap(),
            PathBuf::from("/etc/xdg/autobahn/config.toml")
        );
        for invalid in [Some("relative/config"), Some(""), None] {
            assert_eq!(
                config_path_from(invalid, Some("/home/user")).unwrap(),
                PathBuf::from("/home/user/.config/autobahn/config.toml")
            );
        }
        // Without a usable XDG_CONFIG_HOME, HOME must exist and be absolute.
        assert!(config_path_from(None, None).is_err());
        assert!(config_path_from(None, Some("relative-home")).is_err());
    }
}
