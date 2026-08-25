//! Ignore pattern handling (gitignore-style globs).

use anyhow::Result;

/// A compiled set of ignore patterns.
///
/// Patterns follow gitignore-style semantics over root-relative paths:
/// `name` matches at any depth, `/name` anchors to the root, trailing `/`
/// restricts to directories, `**` crosses directory boundaries, and a `!`
/// prefix negates (re-includes) previously ignored paths, with later
/// patterns taking precedence.
pub struct IgnoreSet {
    _private: (),
}

impl IgnoreSet {
    /// Compiles an ignore set from patterns.
    pub fn new(patterns: &[String]) -> Result<IgnoreSet> {
        todo!("implemented by the scan module")
    }

    /// Indicates whether or not the root-relative path (with `is_directory`
    /// disambiguating directory-only patterns) is ignored.
    pub fn ignored(&self, path: &str, is_directory: bool) -> bool {
        todo!("implemented by the scan module")
    }
}
