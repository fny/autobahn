//! Ignore patterns kept in files rather than in the configuration.
//!
//! A useful ignore list for a language ecosystem runs to dozens of lines,
//! which is more than belongs in a configuration file next to the hosts
//! and the intervals. These live in `~/.autobahn/ignores` instead, one
//! file per concern, and groups name the ones they want:
//!
//! ```toml
//! [defaults]
//! ignore_files = ["common.gitignore"]
//!
//! [groups.work]
//! ignore_files = ["Rust.gitignore", "Node.gitignore"]
//! ```
//!
//! A name is the file's name exactly, extension and capitals included.
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

/// Reads the patterns from one ignore file.
///
/// An entry is one of two things, decided by whether it looks like a path:
///
/// - A bare file name (`"Rust.gitignore"`) is the name of a file in the
///   ignore directory, matched exactly. Nothing is appended and case is
///   not ignored: naming one file and silently reading another is worse
///   than typing an extension.
/// - Anything containing a separator, or beginning with `~`, is a path
///   taken as written. `~/` expands against the home directory, because a
///   configuration is written by a person and that is how a person writes
///   a path in one.
///
/// A relative path is refused rather than guessed at. The supervisor runs
/// under a login service, whose working directory is not the one the
/// reader was in when they wrote the line, so "relative to here" has no
/// answer that would still be right tomorrow.
pub fn read(directory: &Path, entry: &str) -> Result<Vec<String>> {
    let path = locate(directory, entry)?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("unable to read the ignore file {}", path.display()))?;
    Ok(parse(&text))
}

/// Turns a configured entry into the file it names.
fn locate(directory: &Path, entry: &str) -> Result<PathBuf> {
    if entry.trim().is_empty() {
        bail!("an ignore file entry cannot be empty");
    }

    let looks_like_a_path = entry.starts_with('~') || entry.contains('/');
    if !looks_like_a_path {
        let path = directory.join(entry);
        if path.is_file() {
            return Ok(path);
        }
        let available = list(directory);
        bail!(match available.is_empty() {
            true => format!(
                "no ignore file named {entry:?} in {} (the directory is empty or absent)",
                directory.display()
            ),
            false => format!(
                "no ignore file named {entry:?} in {} (available: {})",
                directory.display(),
                available.join(", ")
            ),
        });
    }

    let path = crate::paths::expand_tilde(entry)
        .with_context(|| format!("invalid ignore file path {entry:?}"))?;
    if !path.is_absolute() {
        bail!(
            "ignore file path {entry:?} is relative; give an absolute path (or one starting \
             with ~/), because the supervisor's working directory is not the one this was \
             written in"
        );
    }
    if !path.is_file() {
        bail!("no ignore file at {}", path.display());
    }
    Ok(path)
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

    /// A bare name is the filename as written: nothing is appended to it
    /// and the directory is not searched for something close. Whether the
    /// name is compared with regard to case is the filesystem's business,
    /// not autobahn's — the same name resolves on APFS and does not on
    /// ext4, and being stricter than the filesystem underneath would be
    /// its own kind of surprise.
    #[test]
    fn a_bare_name_is_the_filename_as_written() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(directory.path().join("Rust.gitignore"), "target\n").expect("the template");
        assert_eq!(
            read(directory.path(), "Rust.gitignore").expect("the patterns"),
            vec!["target"]
        );
        for near_miss in ["rust", "Rust", "Rust.gitignore.txt"] {
            assert!(
                read(directory.path(), near_miss).is_err(),
                "{near_miss:?} must not resolve: no extension is appended"
            );
        }
    }

    /// A path is taken as written, wherever it points.
    #[test]
    fn a_path_is_read_from_where_it_says() {
        let elsewhere = tempfile::tempdir().expect("a temporary directory");
        let file = elsewhere.path().join("mine.gitignore");
        std::fs::write(&file, "# mine\nbuild\n").expect("the file");
        let ignores = tempfile::tempdir().expect("the ignore directory");
        assert_eq!(
            read(ignores.path(), &file.display().to_string()).expect("the patterns"),
            vec!["build"]
        );
    }

    /// `~/` is how a person writes a path in a configuration, so it has to
    /// mean what they meant.
    #[test]
    fn a_tilde_expands_against_the_home_directory() {
        let ignores = tempfile::tempdir().expect("the ignore directory");
        let error =
            read(ignores.path(), "~/definitely-not-here.gitignore").expect_err("no such file");
        let message = format!("{error:#}");
        assert!(
            !message.contains('~'),
            "the path must be expanded: {message}"
        );
        assert!(message.contains("definitely-not-here"), "{message}");
    }

    /// A service's working directory is not the one the line was written
    /// in, so a relative path is refused rather than guessed at.
    #[test]
    fn a_relative_path_is_refused() {
        let ignores = tempfile::tempdir().expect("the ignore directory");
        let error = read(ignores.path(), "some/where.gitignore").expect_err("relative");
        assert!(format!("{error:#}").contains("relative"), "{error:#}");
    }

    #[test]
    fn an_empty_entry_is_refused() {
        let ignores = tempfile::tempdir().expect("the ignore directory");
        assert!(read(ignores.path(), "").is_err());
        assert!(read(ignores.path(), "   ").is_err());
    }

    #[test]
    fn a_missing_name_says_what_is_available() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(directory.path().join("Rust.gitignore"), "target\n").expect("write");
        let error = read(directory.path(), "Python.gitignore").expect_err("no such file");
        let message = format!("{error:#}");
        assert!(message.contains("Python.gitignore"), "{message}");
        assert!(message.contains("Rust.gitignore"), "{message}");
    }
}
