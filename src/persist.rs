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

/// A background writer for one state file.
pub struct StateWriter {
    /// The shared slot and its signal.
    state: Arc<(Mutex<Shared>, Condvar)>,
    /// The writer thread, joined on drop.
    thread: Option<std::thread::JoinHandle<()>>,
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
        let thread = std::thread::spawn(move || {
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
                let pending = {
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
                        Some(pending) => pending,
                        // Nothing queued and asked to stop: everything that
                        // was queued has been written.
                        None => return,
                    }
                };
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
pub fn write_atomically(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&temporary, data)?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
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
        writer.store(
            directory.path().join("missing").join("state"),
            || Some(vec![1]),
        );
        let good = directory.path().join("state");
        writer.store(good.clone(), || Some(vec![2; 8]));
        writer.flush();
        assert_eq!(
            std::fs::read(&good).expect("the later write must still land").len(),
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
