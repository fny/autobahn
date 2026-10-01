//! Settings this machine keeps for itself: what it allows, whoever asks.
//!
//! `~/.autobahn/host.toml` (under the state root) is read by this machine
//! alone. No controller sends it and no leader pushes it, so what it says
//! holds against both. It is not a configuration: a machine with one runs
//! its own sessions, and a peer has none, but either can have this.
//!
//! ```toml
//! # The folders an agent on this machine serves, and the only ones a peer
//! # leading from here may sync as its own. Without the key, any.
//! roots = ["~/Workspace"]
//! ```

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// The file's name under the state root.
pub const FILE: &str = "host.toml";

/// The file as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Written {
    roots: Option<Vec<String>>,
}

/// This machine's settings, as `host.toml` gives them.
#[derive(Debug, Default)]
pub struct HostSettings {
    /// Where the settings came from, for messages.
    path: PathBuf,
    /// The folders served, resolved; `None` for no restriction.
    roots: Option<Vec<PathBuf>>,
}

impl HostSettings {
    /// Reads `host.toml` under `state_root`. A machine without one allows
    /// what it always did. One that cannot be read or parsed is an error,
    /// so that a typo never lifts a restriction.
    pub fn load(state_root: &Path) -> Result<HostSettings> {
        let path = state_root.join(FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(HostSettings { path, roots: None })
            }
            Err(error) => {
                return Err(error).with_context(|| format!("unable to read {}", path.display()))
            }
        };
        let written: Written =
            toml::from_str(&text).with_context(|| format!("unable to parse {}", path.display()))?;
        let roots = match written.roots {
            None => None,
            Some(roots) => Some(
                roots
                    .iter()
                    .map(|root| {
                        resolve(&crate::paths::expand_tilde(root)?)
                            .with_context(|| format!("{root:?} in {}", path.display()))
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
        };
        Ok(HostSettings { path, roots })
    }

    /// Refuses a root outside the folders this machine serves. The root is
    /// judged where it really is — through every symbolic link on the way
    /// to it, as far as it exists — so a link inside an allowed folder that
    /// points out of it does not make its target allowed.
    pub fn check_root(&self, root: &Path) -> Result<()> {
        let Some(allowed) = &self.roots else {
            return Ok(());
        };
        let resolved = resolve(root)?;
        if allowed.iter().any(|folder| resolved.starts_with(folder)) {
            return Ok(());
        }
        bail!(
            "{} is outside the folders this machine serves ({}), as {} sets them",
            root.display(),
            allowed
                .iter()
                .map(|folder| folder.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            self.path.display()
        )
    }
}

/// Where a path really is: its longest existing prefix, canonicalized, with
/// the rest appended. A `..` anywhere is refused rather than reasoned about.
fn resolve(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    if path.components().any(|part| part == Component::ParentDir) {
        bail!("{} goes up through `..`", path.display());
    }
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(canonical) => {
                let mut resolved = canonical;
                for part in rest.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = existing.file_name().map(|name| name.to_os_string()) else {
                    return Err(error)
                        .with_context(|| format!("unable to resolve {}", path.display()));
                };
                rest.push(name);
                existing.pop();
            }
            Err(error) => {
                return Err(error).with_context(|| format!("unable to resolve {}", path.display()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(state: &Path, text: &str) -> Result<HostSettings> {
        std::fs::write(state.join(FILE), text).unwrap();
        HostSettings::load(state)
    }

    /// Without the file, every root is served, as before there was one.
    /// With it, a root inside an allowed folder is — one that does not
    /// exist yet included — and one outside is not, nor one reached
    /// through a link inside an allowed folder that points out of it.
    #[test]
    fn a_root_must_lie_inside_the_folders_this_machine_serves() {
        let keep = tempfile::tempdir().unwrap();
        let state = keep.path().join("state");
        let served = keep.path().join("served");
        let elsewhere = keep.path().join("elsewhere");
        for directory in [&state, &served, &elsewhere] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::os::unix::fs::symlink(&elsewhere, served.join("link-out")).unwrap();

        let open = HostSettings::load(&state).unwrap();
        open.check_root(&elsewhere)
            .expect("no file, no restriction");

        let host = settings(&state, &format!("roots = [{:?}]\n", served.display())).unwrap();
        host.check_root(&served).expect("the folder itself");
        host.check_root(&served.join("project/not/yet"))
            .expect("inside, not yet made");
        for refused in [
            elsewhere.clone(),
            served.join("link-out"),
            served.join("link-out/deeper"),
            served.join("../elsewhere"),
            keep.path().to_path_buf(),
        ] {
            let error = host.check_root(&refused).expect_err("outside");
            assert!(
                format!("{error:#}").contains("outside the folders")
                    || format!("{error:#}").contains("`..`"),
                "{}: {error:#}",
                refused.display()
            );
        }
    }

    /// A file that does not parse refuses everything rather than nothing:
    /// a typo must not lift a restriction.
    #[test]
    fn a_file_that_does_not_parse_is_an_error() {
        let keep = tempfile::tempdir().unwrap();
        assert!(settings(keep.path(), "roots = [\"/x\"\nmore").is_err());
        assert!(settings(keep.path(), "rots = [\"/x\"]\n").is_err());
        assert!(settings(keep.path(), "roots = [\"relative\"]\n").is_err());
    }
}
