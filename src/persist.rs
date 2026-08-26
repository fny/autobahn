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
