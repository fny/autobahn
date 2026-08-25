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
/// configuration mean the right thing on every host they fan out to.
pub fn expand_tilde(path: &str) -> Result<PathBuf> {
    if path != "~" && !path.starts_with("~/") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var("HOME").context("unable to expand ~ (HOME is not set)")?;
    if path == "~" {
        return Ok(PathBuf::from(home));
    }
    Ok(PathBuf::from(home).join(&path["~/".len()..]))
}

/// Returns the default configuration file path:
/// `$XDG_CONFIG_HOME/autobahn/config.toml`, falling back to
/// `~/.config/autobahn/config.toml` per the XDG base directory specification.
pub fn default_config_path() -> Result<PathBuf> {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(directory) if !directory.is_empty() => PathBuf::from(directory),
        _ => {
            let home = std::env::var("HOME").context("HOME is not set")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("autobahn").join("config.toml"))
}

/// Returns the default state root, under which session state
/// (`sessions/<id>/`) and supervisor status (`status/`) are kept.
pub fn default_state_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".autobahn"))
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
}
