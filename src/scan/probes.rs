//! Filesystem behavior probing.
//!
//! Synchronization correctness depends on properties that vary by volume,
//! not by operating system: whether executability bits survive a round trip
//! (they don't on FAT-family filesystems), whether stored names come back
//! Unicode-decomposed (HFS+ always, other volumes never), and whether name
//! lookups are case-insensitive (APFS and HFS+ by default). Rather than
//! hardcoding per-platform assumptions, the behavior is probed empirically:
//! a few scratch files are created in the synchronization root itself — the
//! only location guaranteed to be on the volume in question — named with the
//! temporary prefix that scans skip, and removed immediately.
//!
//! Probing is best-effort: a probe that can't run (an unwritable or missing
//! root) falls back to the conservative Unix-typical defaults, which is
//! exactly the behavior autobahn assumed before probing existed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use unicode_normalization::UnicodeNormalization;

use super::TEMPORARY_PREFIX;

/// The counter that uniquifies probe file names within a process.
static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The behavioral properties of the filesystem underlying a synchronization
/// root.
#[derive(Clone, Copy, Debug)]
pub struct FilesystemBehavior {
    /// Whether or not executability bits survive storage.
    pub preserves_executability: bool,
    /// Whether or not stored names come back Unicode-decomposed (NFD).
    pub decomposes_unicode: bool,
    /// Whether or not name lookups are case-insensitive.
    pub case_insensitive: bool,
}

impl Default for FilesystemBehavior {
    /// The Unix-typical defaults, used when probing isn't possible.
    fn default() -> FilesystemBehavior {
        FilesystemBehavior {
            preserves_executability: true,
            decomposes_unicode: false,
            case_insensitive: false,
        }
    }
}

/// Recomposes a name to Unicode NFC.
///
/// Scans apply this to names read from decomposing volumes, so that the
/// hierarchy model (and everything downstream of it: reconciliation, the
/// ancestor, the wire) always carries composed names regardless of how the
/// volume stores them. The ASCII fast path keeps the common case free.
pub fn recompose(name: &str) -> String {
    if name.is_ascii() {
        name.to_owned()
    } else {
        name.nfc().collect()
    }
}

/// Probes the behavior of the filesystem holding `root`. The root must be
/// an existing directory; individual probe failures fall back to the
/// defaults for the property in question.
pub fn probe(root: &Path) -> FilesystemBehavior {
    FilesystemBehavior {
        preserves_executability: probe_executability(root).unwrap_or(true),
        decomposes_unicode: probe_unicode_decomposition(root).unwrap_or(false),
        case_insensitive: probe_case_insensitivity(root).unwrap_or(false),
    }
}

/// Returns a unique probe file name carrying `token` (which must be ASCII
/// for every use except the Unicode probe's).
fn probe_name(token: &str) -> String {
    let count = PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{TEMPORARY_PREFIX}-probe-{}-{count}-{token}",
        std::process::id()
    )
}

/// Removes a probe file, best-effort.
fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
}

/// Probes whether executability bits survive: create a file, mark it
/// executable, and see whether the bit reads back.
fn probe_executability(root: &Path) -> Option<bool> {
    let path = root.join(probe_name("x"));
    fs::write(&path, b"").ok()?;
    let result = (|| {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).ok()?;
        let metadata = fs::symlink_metadata(&path).ok()?;
        Some(metadata.permissions().mode() & 0o111 != 0)
    })();
    cleanup(&path);
    result
}

/// Probes whether the volume decomposes Unicode names: create a file whose
/// name carries a composed character and check which form the directory
/// listing reports.
fn probe_unicode_decomposition(root: &Path) -> Option<bool> {
    // U+00E9 (é) composed; its decomposed form is "e" + U+0301.
    let name = probe_name("\u{00E9}");
    let path = root.join(&name);
    fs::write(&path, b"").ok()?;
    let result = (|| {
        // The stored name is recovered from a directory listing (a lookup by
        // the composed name would succeed either way on normalizing
        // volumes, revealing nothing).
        let marker = name
            .strip_suffix('\u{00E9}')
            .expect("the probe name ends with the composed character");
        for entry in fs::read_dir(root).ok()? {
            let stored = entry.ok()?.file_name();
            let stored = stored.to_string_lossy();
            if let Some(suffix) = stored.strip_prefix(marker) {
                return Some(suffix != "\u{00E9}");
            }
        }
        None
    })();
    // The stored name may differ from the created one; remove by listing
    // when the direct removal misses.
    if fs::remove_file(&path).is_err() {
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(TEMPORARY_PREFIX)
                {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
    result
}

/// Probes whether name lookups are case-insensitive: create a file with an
/// uppercase marker and look it up lowercased.
fn probe_case_insensitivity(root: &Path) -> Option<bool> {
    let name = probe_name("CASE");
    let path = root.join(&name);
    fs::write(&path, b"").ok()?;
    let result = Some(fs::symlink_metadata(root.join(name.to_lowercase())).is_ok());
    cleanup(&path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probing_reports_unix_semantics_and_leaves_nothing_behind() {
        let root = tempfile::tempdir().expect("temporary directory should be creatable");
        let behavior = probe(root.path());
        // The test suite runs on a byte-preserving, case-sensitive Unix
        // filesystem.
        assert!(behavior.preserves_executability);
        assert!(!behavior.decomposes_unicode);
        assert!(!behavior.case_insensitive);
        // Every probe file was cleaned up.
        let leftovers: Vec<_> = std::fs::read_dir(root.path())
            .expect("root should list")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn recomposition_normalizes_to_nfc() {
        assert_eq!(recompose("plain-ascii.txt"), "plain-ascii.txt");
        // "e" + combining acute accent recomposes to é.
        assert_eq!(recompose("cafe\u{0301}.txt"), "caf\u{00E9}.txt");
        // Already-composed names are unchanged.
        assert_eq!(recompose("caf\u{00E9}.txt"), "caf\u{00E9}.txt");
    }
}
