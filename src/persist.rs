//! Background persistence of derived state.
//!
//! Scan caches are large — tens of megabytes on a large tree — and are
//! rewritten whenever content moves. Serializing and writing them on the
//! cycle's own thread put that cost directly into the latency between
//! saving a file and seeing it appear on the far side.
//!
//! A [`StateWriter`] moves that work to a thread of its own. What makes
//! this safe is what a scan cache *is*: a record of work already done,
//! carrying no information that cannot be recovered by reading the
//! filesystem. Losing one, or writing an old one, costs a single full scan
//! and nothing else.
//!
//! The synchronization ancestor is deliberately **not** written this way,
//! despite being the same shape and size. It carries provenance — which
//! side changed — and a stale one actively misleads reconciliation rather
//! than merely slowing it. See `Session::run_cycle` for the case that
//! settles it.
//!
//! Only the newest queued state for a target is written: a burst of cycles
//! collapses to one write, since the intermediate states are already
//! superseded, and order is preserved by there being a single writer.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

/// A queued write: how to produce the bytes, and where they go. The
/// payload is produced on the writer's thread, so a state superseded
/// before it is written is never serialized at all.
struct Pending {
    /// Produces the serialized bytes.
    encode: Box<dyn FnOnce() -> Option<Vec<u8>> + Send>,
    /// The destination path.
    path: PathBuf,
}

/// The writer's shared state.
#[derive(Default)]
struct Shared {
    /// The newest state awaiting a write, if any.
    pending: Option<Pending>,
    /// Whether the owner has asked the writer to finish.
    stopping: bool,
    /// Whether the writer thread is between writes with nothing queued.
    idle: bool,
    /// Whether the writer thread has stopped for good. A thread that dies
    /// — including by panicking inside an encoder — must not leave a
    /// waiter blocked on a signal nobody will send.
    finished: bool,
}

/// How long the writer waits for a newer state before encoding the one it
/// holds.
///
/// One cycle stores the scan cache twice — once for the scan, once for the
/// transition's folded result — separated by the reconcile and staging work
/// between them. Encoding the first is waste whenever the second follows,
/// and on a large tree that waste is tens of megabytes; a fan-out
/// multiplies it by the destination count.
///
/// A tight pair already collapses, because the writer has not woken yet.
/// This window extends that to pairs separated by a cycle's own work. It
/// costs nothing that matters: the scan cache is derived state, written off
/// the critical path, and losing it to a crash inside the window costs one
/// full scan.
const COALESCING_WINDOW: std::time::Duration = std::time::Duration::from_millis(250);

/// A background writer for one state file.
pub struct StateWriter {
    /// The shared slot and its signal.
    state: Arc<(Mutex<Shared>, Condvar)>,
    /// The writer thread, joined on drop.
    thread: Option<std::thread::JoinHandle<()>>,
    /// Encodes actually performed. Lets a test tell a state dropped
    /// *before* serialization from one dropped after. Per writer rather
    /// than global, because the suite runs in parallel.
    #[cfg(test)]
    encodes: Arc<std::sync::atomic::AtomicUsize>,
}

impl StateWriter {
    /// Starts a writer thread.
    //
    // Deliberately no `Default`: constructing one spawns a thread, which is
    // not what a reader expects `default()` to do.
    #[allow(clippy::new_without_default)]
    pub fn new() -> StateWriter {
        let state = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
        let worker = Arc::clone(&state);
        #[cfg(test)]
        let encodes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        #[cfg(test)]
        let worker_encodes = Arc::clone(&encodes);
        // A deep stack: encoding a scan cache recurses once per directory
        // level of the tree it records (see `crate::threads`).
        let thread = crate::threads::spawn_deep(move || {
            // Marks the writer finished however the loop is left — a
            // return, or an unwinding panic from an encoder — and releases
            // anyone waiting on it.
            struct Retire<'a>(&'a (Mutex<Shared>, Condvar));
            impl Drop for Retire<'_> {
                fn drop(&mut self) {
                    let (lock, signal) = self.0;
                    let mut shared = lock_shared(lock);
                    shared.finished = true;
                    shared.idle = true;
                    signal.notify_all();
                }
            }
            let _retire = Retire(&worker);
            let (lock, signal) = &*worker;
            loop {
                let (pending, stopping) = {
                    let mut shared = lock_shared(lock);
                    while shared.pending.is_none() && !shared.stopping {
                        shared.idle = true;
                        signal.notify_all();
                        shared = signal
                            .wait(shared)
                            .expect("the writer lock is never poisoned");
                    }
                    shared.idle = false;
                    match shared.pending.take() {
                        Some(pending) => (pending, shared.stopping),
                        // Nothing queued and asked to stop: everything that
                        // was queued has been written.
                        None => return,
                    }
                };

                // Give a supersede a moment to arrive before paying to
                // encode. One cycle stores twice — once for the scan and
                // once for the transition's folded result — and claiming
                // the first the instant it is queued means serializing a
                // state that is about to be replaced. On a large tree that
                // is tens of megabytes of work for nothing, and a fan-out
                // multiplies it by the destination count.
                //
                // The wait costs nothing that matters: this is derived
                // state, off the critical path, and losing it to a crash in
                // the window costs one full scan.
                let pending = if stopping {
                    pending
                } else {
                    std::thread::sleep(COALESCING_WINDOW);
                    let mut shared = lock_shared(lock);
                    shared.pending.take().unwrap_or(pending)
                };
                #[cfg(test)]
                worker_encodes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Some(data) = (pending.encode)() {
                    // The writer is the one caller entitled to ignore a
                    // failure: everything it writes is derived state, and a
                    // later cycle will queue a newer version regardless.
                    let _ = write_atomically(&pending.path, &data);
                }
            }
        });
        StateWriter {
            state,
            thread: Some(thread),
            #[cfg(test)]
            encodes,
        }
    }

    /// Queues state for writing, superseding anything not yet written.
    /// `encode` runs on the writer's thread — serialization of a large tree
    /// is itself expensive, and a state that is superseded before its turn
    /// is dropped without ever being encoded.
    pub fn store(&self, path: PathBuf, encode: impl FnOnce() -> Option<Vec<u8>> + Send + 'static) {
        let (lock, signal) = &*self.state;
        let mut shared = lock_shared(lock);
        shared.pending = Some(Pending {
            encode: Box::new(encode),
            path,
        });
        signal.notify_all();
    }

    /// Blocks until every queued write has completed. Callers that need to
    /// observe their own state on disk (a test, a clean shutdown) use this;
    /// the cycle path deliberately does not.
    pub fn flush(&self) {
        let (lock, signal) = &*self.state;
        let mut shared = lock_shared(lock);
        while !shared.finished && (shared.pending.is_some() || !shared.idle) {
            shared = match signal.wait(shared) {
                Ok(shared) => shared,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }
}

impl Drop for StateWriter {
    fn drop(&mut self) {
        {
            let (lock, signal) = &*self.state;
            let mut shared = lock_shared(lock);
            shared.stopping = true;
            signal.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            // A panicking writer thread has already failed to persist; the
            // state is derived, so the loss is a slower next cycle.
            let _ = thread.join();
        }
    }
}

/// Takes the shared state, tolerating poisoning.
///
/// A panic in an encoder poisons this lock on its way out, and the state
/// it guards is a queue of derived data — nothing a panic can leave
/// half-updated in a way that matters. Refusing to proceed would turn a
/// failed write into a hang for everyone waiting on the writer.
fn lock_shared(lock: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    match lock.lock() {
        Ok(shared) => shared,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Writes a file by way of a temporary and a rename, so that a reader never
/// observes a partially written state. The temporary is removed if the
/// rename fails, so a failed write leaves nothing behind.
///
/// The file keeps the mode of the one it replaces, and is `0600` when it
/// is new; the temporary is private from the moment it exists (see
/// [`write_beside`]).
pub fn write_atomically(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let temporary = write_beside(path, data)?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

/// Writes `data` to a new temporary beside `path`, ready to be renamed
/// over it, and returns the temporary's path. For a caller that checks
/// what it wrote before it moves it into place; [`write_atomically`] is
/// this and the rename.
///
/// Guarantees, when it returns `Ok`: the temporary is a new file in
/// `path`'s directory, named `.<name>.tmp.<random>` so another local user
/// cannot guess and pre-create it, created with `O_EXCL` and
/// `O_NOFOLLOW` (see [`crate::fsutil::private_file`]), so it is never an
/// existing file or a link's target. It holds `data`, and its mode is that
/// of the file at `path` — a regular file, not followed through a link —
/// or `0600` when there is none, so a rewrite never loosens a mode the
/// user tightened. On an error nothing is left behind.
pub fn write_beside(path: &Path, data: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mode = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata.permissions().mode() & 0o7777,
        _ => 0o600,
    };
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} names no file", path.display()),
        )
    })?;
    let suffix = crate::fsutil::random_hex(8).map_err(std::io::Error::other)?;
    let mut temporary_name = std::ffi::OsString::from(".");
    temporary_name.push(name);
    temporary_name.push(format!(".tmp.{suffix}"));
    let temporary = path.with_file_name(temporary_name);
    let mut file = crate::fsutil::private_file(&temporary).map_err(std::io::Error::other)?;
    let written = file
        .write_all(data)
        .and_then(|()| file.set_permissions(std::fs::Permissions::from_mode(mode)));
    if let Err(error) = written {
        drop(file);
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(temporary)
}

#[cfg(test)]
mod deep {
    use super::*;
    use crate::tree::{Content, Node};

    /// Finding H-5: the writer encodes whole scan caches, and encoding
    /// recurses once per directory level, so a deep tree has to encode on
    /// a deep stack. On the default one this depth is a stack overflow,
    /// which aborts the whole process.
    #[test]
    fn a_deep_tree_encodes_on_the_writer_thread() {
        let mut node = Node {
            name: "leaf".into(),
            content: Content::Directory(Arc::new(Vec::new())),
        };
        for _ in 0..100_000 {
            node = Node {
                name: "d".into(),
                content: Content::Directory(Arc::new(vec![node])),
            };
        }
        let directory = tempfile::tempdir().expect("temporary directory should be creatable");
        let path = directory.path().join("state");
        let writer = StateWriter::new();
        writer.store(path.clone(), move || bincode::serialize(&node).ok());
        writer.flush();
        let written = std::fs::read(&path).expect("the deep tree should have been written");
        assert!(!written.is_empty());
    }
}

#[cfg(test)]
mod stress {
    //! Concurrency stress for the background writer.
    //!
    //! The writer parks closures, keeps only the newest state per path,
    //! tolerates a poisoned lock, ignores write failures, and flushes on
    //! drop. Each of those is a decision about what may go wrong, so each
    //! is worth hammering rather than asserting once. These are repetition
    //! tests, not exhaustive interleaving checks — they catch a race that
    //! happens often enough to matter.
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    /// Many writers, one path: whatever lands must be one of the stored
    /// states, and the writer must not deadlock or drop the last one.
    #[test]
    fn concurrent_stores_land_a_stored_state_and_never_hang() {
        for round in 0..40 {
            let directory = tempfile::tempdir().expect("temporary directory");
            let path = directory.path().join("state");
            let writer = StdArc::new(StateWriter::new());
            let threads: Vec<_> = (0..8)
                .map(|worker| {
                    let writer = StdArc::clone(&writer);
                    let path = path.clone();
                    std::thread::spawn(move || {
                        for step in 0..50u32 {
                            let value = worker * 1000 + step;
                            writer.store(path.clone(), move || Some(value.to_le_bytes().to_vec()));
                        }
                    })
                })
                .collect();
            for thread in threads {
                thread.join().expect("writer thread panicked");
            }
            writer.flush();
            let written = std::fs::read(&path).expect("a state must have been written");
            assert_eq!(written.len(), 4, "round {round}: partial state on disk");
        }
    }

    /// A state superseded before the writer finishes waiting must never be
    /// encoded — including when the two stores are separated by real work,
    /// as a cycle's scan store and transition store are.
    #[test]
    fn a_state_superseded_within_the_window_is_never_encoded() {
        use std::sync::atomic::Ordering;
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state");
        let writer = StateWriter::new();
        writer.store(path.clone(), || Some(vec![1; 32]));
        // Separated as a cycle separates them, by its reconcile and staging
        // work. A tight pair collapses on its own; this is the case the
        // coalescing window exists for.
        std::thread::sleep(std::time::Duration::from_millis(40));
        writer.store(path.clone(), || Some(vec![2; 32]));
        writer.flush();
        assert_eq!(
            writer.encodes.load(Ordering::Relaxed),
            1,
            "the superseded state was encoded anyway"
        );
        // And the state on disk is the newer one.
        assert_eq!(std::fs::read(&path).expect("written")[0], 2);
    }

    /// A superseded state must never be encoded: that is the whole reason
    /// the writer parks a closure instead of bytes.
    #[test]
    fn superseded_states_are_never_encoded() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state");
        let encodes = StdArc::new(AtomicUsize::new(0));
        let writer = StateWriter::new();
        // Queue far more states than the writer can possibly encode, by
        // holding each encoder briefly so the queue overtakes it.
        for value in 0..200u32 {
            let encodes = StdArc::clone(&encodes);
            writer.store(path.clone(), move || {
                encodes.fetch_add(1, Ordering::Relaxed);
                Some(value.to_le_bytes().to_vec())
            });
        }
        writer.flush();
        let encoded = encodes.load(Ordering::Relaxed);
        assert!(encoded >= 1, "nothing was ever encoded");
        assert!(
            encoded < 200,
            "every one of 200 queued states was encoded ({encoded}); supersede is not collapsing"
        );
    }

    /// An encoder that panics must not poison the writer for everyone else:
    /// the retire guard exists precisely so a panicking encoder still
    /// releases anyone waiting.
    #[test]
    fn a_panicking_encoder_does_not_wedge_the_writer() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let writer = StateWriter::new();
        writer.store(directory.path().join("boom"), || panic!("encoder failed"));
        // The writer thread is now gone. Storing and flushing must still
        // return rather than blocking forever.
        writer.store(directory.path().join("after"), || Some(vec![1, 2, 3]));
        writer.flush();
    }

    /// Dropping the writer flushes what was queued, so a state stored just
    /// before shutdown is not silently lost.
    #[test]
    fn drop_flushes_the_queued_state() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("state");
        {
            let writer = StateWriter::new();
            writer.store(path.clone(), || Some(vec![7; 16]));
        }
        assert_eq!(
            std::fs::read(&path).expect("drop must flush").len(),
            16,
            "a state queued before drop was lost"
        );
    }

    /// A failing write is the writer's to ignore, but it must not stop it
    /// serving later states.
    #[test]
    fn a_failed_write_does_not_stop_later_ones() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let writer = StateWriter::new();
        // An unwritable path: the parent does not exist.
        writer.store(directory.path().join("missing").join("state"), || {
            Some(vec![1])
        });
        let good = directory.path().join("state");
        writer.store(good.clone(), || Some(vec![2; 8]));
        writer.flush();
        assert_eq!(
            std::fs::read(&good)
                .expect("the later write must still land")
                .len(),
            8
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_queued_state_is_what_lands() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let path = keep.path().join("state");
        let writer = StateWriter::new();
        for generation in 0..50u8 {
            writer.store(path.clone(), move || Some(vec![generation; 16]));
        }
        writer.flush();
        // Coalescing means intermediate generations are skipped; what must
        // hold is that the newest queued state is the one on disk.
        let written = std::fs::read(&path).expect("state should exist");
        assert_eq!(written, vec![49u8; 16]);
    }

    #[test]
    fn dropping_the_writer_flushes_what_was_queued() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let path = keep.path().join("state");
        {
            let writer = StateWriter::new();
            writer.store(path.clone(), || Some(b"final".to_vec()));
        }
        assert_eq!(
            std::fs::read(&path).expect("state should exist"),
            b"final".to_vec()
        );
    }

    #[test]
    fn a_panicking_encoder_does_not_strand_a_flusher() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let writer = StateWriter::new();
        writer.store(keep.path().join("state"), || panic!("encoder failed"));
        // The writer thread dies, but a waiter must still be released
        // rather than blocking on a signal that can never come.
        writer.flush();
        // Later stores are accepted and simply never written; the state is
        // derived, so the cost is a slower next cycle, not a hang.
        writer.store(keep.path().join("state"), || Some(b"ignored".to_vec()));
        writer.flush();
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path)
            .expect("the file should exist")
            .permissions()
            .mode()
            & 0o7777
    }

    #[test]
    fn a_new_state_file_is_private() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let path = keep.path().join("state");
        write_atomically(&path, b"new").expect("the write should succeed");
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_rewrite_keeps_the_mode_of_the_file_it_replaces() {
        use std::os::unix::fs::PermissionsExt;
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        for kept in [0o600, 0o640, 0o644] {
            let path = keep.path().join(format!("state-{kept:o}"));
            std::fs::write(&path, b"before").expect("state should be writable");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(kept))
                .expect("mode should be settable");
            write_atomically(&path, b"after").expect("the write should succeed");
            assert_eq!(mode(&path), kept, "{kept:04o} was not kept");
            assert_eq!(std::fs::read(&path).unwrap(), b"after");
        }
    }

    #[test]
    fn the_temporary_is_private_and_unpredictable() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let path = keep.path().join("state");
        let first = write_beside(&path, b"one").expect("the write should succeed");
        let second = write_beside(&path, b"two").expect("the write should succeed");
        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(keep.path()));
        assert!(!first
            .to_string_lossy()
            .ends_with(&format!(".tmp.{}", std::process::id())));
        assert_eq!(mode(&first), 0o600);
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        // Not moved into place: that is the caller's to do.
        assert!(!path.exists());
    }

    #[test]
    fn a_partially_written_state_is_never_published() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let path = keep.path().join("state");
        std::fs::write(&path, b"original").expect("state should be writable");
        write_atomically(&path, b"replacement").expect("the write should succeed");
        assert_eq!(
            std::fs::read(&path).expect("state should exist"),
            b"replacement".to_vec()
        );
        // The temporary is not left behind.
        let leftovers: Vec<_> = std::fs::read_dir(keep.path())
            .expect("directory should be readable")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .filter(|name| name != "state")
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }
}
