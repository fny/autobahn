//! Ignore patterns kept in files rather than in the configuration.
//!
//! A useful ignore list for a language ecosystem runs to dozens of lines,
//! which is more than belongs in a configuration file next to the hosts
//! and the intervals. These live in `~/.autobahn/ignores` instead, one
//! file per concern, and groups name the ones they want:
//!
//! ```toml
//! [defaults]
//! ignore_files = ["common"]
//!
//! [groups.work]
//! ignore_files = ["rust", "node"]
//! ```
//!
//! Naming them, rather than loading whatever the directory happens to
//! hold, is deliberate. Ignore patterns are decided last-match-wins, so
//! order is meaning: `!gradle-wrapper.jar` followed by `*.jar` is not the
//! same list as the reverse. A directory scan would order them by whatever
//! the filesystem returned, and dropping a new file in would silently
//! change what the existing ones mean. A written list is an order someone
//! chose and can see.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The directory holding ignore files, under the state root.
pub const DIRECTORY: &str = "ignores";

/// The extension tried when a name does not match a file directly, so that
/// `"rust"` finds `Rust.gitignore` — which is what these files are called
/// when they come from a template collection.
const EXTENSION: &str = "gitignore";

/// Reads the patterns from one named ignore file.
///
/// Names resolve within the ignore directory only: a name containing a
/// path separator, or `..`, is refused rather than followed, so a
/// configuration cannot read a file elsewhere on the machine by naming it.
pub fn read(directory: &Path, name: &str) -> Result<Vec<String>> {
    if name.is_empty() {
        bail!("an ignore file name cannot be empty");
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        bail!(
            "invalid ignore file name {name:?}: names resolve inside {} and cannot contain a \
             path",
            directory.display()
        );
    }

    let path = resolve(directory, name).ok_or_else(|| {
        let available = list(directory);
        match available.is_empty() {
            true => anyhow::anyhow!(
                "no ignore file named {name:?} in {} (the directory is empty or absent)",
                directory.display()
            ),
            false => anyhow::anyhow!(
                "no ignore file named {name:?} in {} (available: {})",
                directory.display(),
                available.join(", ")
            ),
        }
    })?;

    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("unable to read the ignore file {}", path.display()))?;
    Ok(parse(&text))
}

/// Finds the file a name refers to: the name itself, or the name with the
/// usual extension, matched without regard to case because a template
/// collection capitalises its files and a configuration usually does not.
fn resolve(directory: &Path, name: &str) -> Option<PathBuf> {
    let direct = directory.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    let suffixed = directory.join(format!("{name}.{EXTENSION}"));
    if suffixed.is_file() {
        return Some(suffixed);
    }
    let wanted = name.to_ascii_lowercase();
    std::fs::read_dir(directory)
        .ok()?
        .flatten()
        .find_map(|entry| {
            let file = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let matches = file == wanted || file == format!("{wanted}.{EXTENSION}");
            (matches && entry.path().is_file()).then(|| entry.path())
        })
}

/// The names available in the directory, for the message shown when one is
/// not found. Sorted, so the list does not reorder between runs.
pub fn list(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// Turns a gitignore-style file into patterns.
///
/// Comments and blank lines are dropped — without this every blank line in
/// a template is a pattern with no content, which the compiler refuses,
/// and every comment is a glob that matches nothing but is still tested
/// against every path scanned.
fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim_end)
        .map(|line| match line.starts_with(char::is_whitespace) {
            // Leading whitespace is not significant in these files, but a
            // pattern is matched literally, so it has to go or the pattern
            // silently never matches.
            true => line.trim_start(),
            false => line,
        })
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_blank_lines_are_dropped() {
        let text = "# build output\n\ntarget\n\n  node_modules  \n\n# nothing below\n";
        assert_eq!(parse(text), vec!["target", "node_modules"]);
    }

    /// A `#` only comments when it begins the line: `foo#bar` is a pattern.
    #[test]
    fn a_hash_inside_a_pattern_is_not_a_comment() {
        assert_eq!(parse("a#b\n#c\n"), vec!["a#b"]);
    }

    /// Negations and directory markers survive intact — they are what the
    /// compiler reads to decide meaning.
    #[test]
    fn pattern_syntax_is_left_alone() {
        let text = "*.jar\n!gradle-wrapper.jar\nbuild/\n/anchored\n";
        assert_eq!(
            parse(text),
            vec!["*.jar", "!gradle-wrapper.jar", "build/", "/anchored"]
        );
    }

    #[test]
    fn a_name_cannot_escape_the_directory() {
        let directory = std::env::temp_dir();
        for name in ["../secrets", "a/b", "..", ""] {
            assert!(read(&directory, name).is_err(), "{name:?} must be refused");
        }
    }

    #[test]
    fn a_name_finds_a_capitalised_template() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(directory.path().join("Rust.gitignore"), "target\n").expect("the template");
        assert_eq!(
            read(directory.path(), "rust").expect("the patterns"),
            vec!["target"]
        );
    }

    #[test]
    fn a_missing_name_says_what_is_available() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(directory.path().join("Rust.gitignore"), "target\n").expect("write");
        let error = read(directory.path(), "python").expect_err("no such file");
        let message = format!("{error:#}");
        assert!(message.contains("python"), "{message}");
        assert!(message.contains("Rust.gitignore"), "{message}");
    }
}
