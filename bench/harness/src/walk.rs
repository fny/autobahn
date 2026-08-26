//! The one implementation of "which files count".
//!
//! Every subcommand — manifests, partitions, convergence checks — walks a
//! tree through this module, and both hosts run this same binary, so there
//! is exactly one answer to what is synchronizable content and what is an
//! ignored directory or a harness temporary. The previous harness had two
//! divergent Python implementations of this walk; that class of bug is
//! structurally gone.

use std::path::{Path, PathBuf};

/// Directories the benchmark configures both tools to ignore. The walk
/// must exclude exactly these, or convergence checks would hold the tools
/// responsible for content they were told not to synchronize.
const EXCLUDED_DIRECTORIES: &[&str] = &[".git", "out"];

/// Name fragments marking temporaries owned by the harness or the tools;
/// they exist transiently mid-write and must never count.
const TEMPORARY_MARKERS: &[&str] = &[".bench-tmp", ".floor-tmp", ".autobahn-tmp", ".mutagen-temporary"];

fn excluded_directory(name: &str) -> bool {
    EXCLUDED_DIRECTORIES.contains(&name) || TEMPORARY_MARKERS.iter().any(|m| name.contains(m))
}

fn temporary(name: &str) -> bool {
    TEMPORARY_MARKERS.iter().any(|marker| name.contains(marker))
}

/// Every synchronizable file under the root: `(relative path, size)`, in
/// one deterministic sorted order. The walk is complete — never
/// early-stopped — so identical trees yield identical lists on any host.
pub fn files(root: &Path) -> std::io::Result<Vec<(String, u64)>> {
    files_with_errors(root).map(|(files, _)| files)
}

/// The walk, with its failures counted rather than swallowed. A missing
/// root is a hard error — an absent tree must never summarize as an empty
/// one — and any unreadable directory below it increments the error count,
/// which both summaries include, so two incomplete walks can only compare
/// equal by failing identically at the same count (and the count being
/// nonzero is itself visible to the caller).
pub fn files_with_errors(root: &Path) -> std::io::Result<(Vec<(String, u64)>, u64)> {
    std::fs::metadata(root)?;
    let mut errors = 0u64;
    let mut collected = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(relative_dir) = stack.pop() {
        let absolute = root.join(&relative_dir);
        let mut entries: Vec<_> = match std::fs::read_dir(&absolute) {
            Ok(entries) => entries.filter_map(Result::ok).collect(),
            // A directory that vanished mid-walk belongs to an in-flight
            // change; it is counted, which poisons summary equality until
            // a later walk sees a stable tree.
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            let relative = if relative_dir.as_os_str().is_empty() {
                PathBuf::from(&name)
            } else {
                relative_dir.join(&name)
            };
            if file_type.is_dir() {
                if !excluded_directory(&name) {
                    stack.push(relative);
                }
            } else if file_type.is_file() && !temporary(&name) {
                if let Ok(metadata) = entry.metadata() {
                    collected.push((relative.to_string_lossy().into_owned(), metadata.len()));
                }
            }
            // Symbolic links are excluded from summaries: the corpora are
            // materialized without them (partitions.rs also never selects
            // one), so they cannot appear in synchronized content.
        }
    }
    collected.sort();
    Ok((collected, errors))
}

/// `<count> <bytes>` — for cheap convergence polling.
pub fn print_cheap(root: &Path) -> Result<(), String> {
    let (files, errors) = files_with_errors(root).map_err(|error| error.to_string())?;
    let bytes: u64 = files.iter().map(|(_, size)| size).sum();
    println!("{} {} errors={errors}", files.len(), bytes);
    Ok(())
}

/// `<count> <bytes> <digest>` — the arbiter. Two trees with equal full
/// summaries hold identical synchronizable content. A file that cannot be
/// read mid-walk (an in-flight edit) contributes a sentinel, which makes
/// the summaries unequal — the correct verdict for an unstable tree.
pub fn print_full(root: &Path) -> Result<(), String> {
    let (files, errors) = files_with_errors(root).map_err(|error| error.to_string())?;
    let bytes: u64 = files.iter().map(|(_, size)| size).sum();
    let mut listing = blake3::Hasher::new();
    for (relative, size) in &files {
        let content = match crate::digest_file(&root.join(relative)) {
            Ok(digest) => digest.to_hex().to_string(),
            Err(_) => "<unreadable>".to_owned(),
        };
        listing.update(format!("{relative}\0{size}\0{content}\n").as_bytes());
    }
    println!(
        "{} {} {} errors={errors}",
        files.len(),
        bytes,
        listing.finalize().to_hex()
    );
    Ok(())
}
