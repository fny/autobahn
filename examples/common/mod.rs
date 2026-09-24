//! Helpers shared by the diagnostic examples.
//!
//! Not an example itself: cargo discovers `examples/*.rs` and
//! `examples/*/main.rs`, so a `mod.rs` in a subdirectory is a module the
//! examples can share rather than a target it tries to build.

use std::path::PathBuf;

/// Puts back the file this benchmark edits, however the benchmark exits.
///
/// The measurement needs one real edit to provoke one real rescan, and the
/// roots worth measuring are real trees — which may be live
/// synchronization roots. An edit left behind in one of those is not a
/// stray byte in a scratch corpus: the supervisor sees a modified file and
/// propagates it to every other machine, indistinguishable from something
/// a person typed.
///
/// So the original bytes go back, and so does the original modification
/// time. Restoring the time is not tidiness — it is what stops a scan
/// noticing at all, since digest reuse skips a file whose metadata has not
/// moved. The write is in place, so the inode, size and mode are unchanged
/// too, and what is left behind is the file that was there.
///
/// A `Drop` guard rather than a line at the end of the loop — which is
/// what this replaces — because the run that most needs the restore is
/// the one that does not reach the end: an `.expect()` on an unreadable
/// file, an assertion about the tree, a tree that changed underneath.
/// That is exactly when a stray edit would be left behind to propagate
/// with nobody watching — and not hypothetical: `cycle_cost`'s transition
/// assertion used to fail whenever the watcher had not delivered the edit
/// before the rescan read it, which is how this guard came about.
pub struct Restore {
    path: PathBuf,
    content: Vec<u8>,
    /// Access and modification times, each as (seconds, nanoseconds).
    times: Option<((i64, i64), (i64, i64))>,
}

impl Restore {
    pub fn of(path: &std::path::Path, content: Vec<u8>) -> Restore {
        use std::os::unix::fs::MetadataExt;
        let times = std::fs::metadata(path).ok().map(|metadata| {
            (
                (metadata.atime(), metadata.atime_nsec()),
                (metadata.mtime(), metadata.mtime_nsec()),
            )
        });
        Restore {
            path: path.to_path_buf(),
            content,
            times,
        }
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        // Reported rather than swallowed: a benchmark that quietly fails to
        // put a file back is the exact failure this type exists to prevent,
        // and the only thing worse than the edit is not knowing about it.
        if let Err(error) = std::fs::write(&self.path, &self.content) {
            eprintln!(
                "WARNING: could not restore {}: {error}\n  \
                 it still holds this benchmark's edit",
                self.path.display()
            );
            return;
        }
        let Some((atime, mtime)) = self.times else {
            eprintln!(
                "WARNING: restored the content of {} but never read its \
                 original timestamp",
                self.path.display()
            );
            return;
        };
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(self.path.as_os_str().as_bytes()) else {
            eprintln!(
                "WARNING: restored the content of {} but its name cannot be \
                 passed to the timestamp call",
                self.path.display()
            );
            return;
        };
        let times = [
            libc::timespec {
                tv_sec: atime.0 as _,
                tv_nsec: atime.1 as _,
            },
            libc::timespec {
                tv_sec: mtime.0 as _,
                tv_nsec: mtime.1 as _,
            },
        ];
        // Safety: `path` is a valid NUL-terminated string that outlives the
        // call, and `times` is the two-element array the interface requires.
        let restored = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
        // Said out loud rather than swallowed. The consequence is mild —
        // the file reads as modified, costing one re-read, and its content
        // is already back — but a restore that half worked and said nothing
        // is how the next person concludes it worked.
        if restored != 0 {
            eprintln!(
                "WARNING: restored the content of {} but not its timestamp: {}",
                self.path.display(),
                std::io::Error::last_os_error()
            );
        }
    }
}
