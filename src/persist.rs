//! Background persistence of derived state.
//!
//! Session state — the scan caches and the synchronization ancestor — is
//! large (tens of megabytes on a large tree) and is rewritten whenever
//! content moves. Serializing and writing it on the cycle's own thread put
//! that cost directly into the latency a user sees between saving a file
//! and seeing it appear on the far side.
//!
//! A [`StateWriter`] moves that work to a thread of its own. Two properties
//! make it safe:
//!
//! * **Order is preserved.** One writer thread consumes one slot, so a
//!   later state can never land before an earlier one. What is on disk is
//!   always a state this session actually passed through, never a mixture.
//! * **Lagging is already a supported condition.** Every one of these files
//!   describes work that has *already* been applied to a filesystem, so a
//!   crash between the work and the write leaves the file behind — which is
//!   true today, synchronously, for any crash in that window. Writing in
//!   the background widens the window without changing its character: a
//!   stale ancestor causes reconciliation to re-derive (both sides agreeing
//!   converges, a propagated deletion re-propagates as a no-op), and a
//!   stale scan cache costs one full scan. Neither loses content. The
//!   dangerous direction — a file *ahead* of the filesystem, claiming work
//!   that never happened — cannot arise, because state is only ever queued
//!   after the work it describes has been applied.
//!
//! Only the newest queued state for a target is written: a burst of cycles
//! collapses to one write, since the intermediate states are already
//! superseded.

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
    pub fn new() -> StateWriter {
        let state = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
        let worker = Arc::clone(&state);
        let thread = std::thread::spawn(move || {
            let (lock, signal) = &*worker;
            loop {
                let pending = {
                    let mut shared = lock.lock().expect("the writer lock is never poisoned");
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
                    write_atomically(&pending.path, &data);
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
        let mut shared = lock.lock().expect("the writer lock is never poisoned");
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
        let mut shared = lock.lock().expect("the writer lock is never poisoned");
        while shared.pending.is_some() || !shared.idle {
            shared = signal
                .wait(shared)
                .expect("the writer lock is never poisoned");
        }
    }
}

impl Default for StateWriter {
    fn default() -> StateWriter {
        StateWriter::new()
    }
}

impl Drop for StateWriter {
    fn drop(&mut self) {
        {
            let (lock, signal) = &*self.state;
            let mut shared = lock.lock().expect("the writer lock is never poisoned");
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

/// Writes a file by way of a temporary and a rename, so that a reader never
/// observes a partially written state.
pub fn write_atomically(path: &Path, data: &[u8]) {
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&temporary, data).is_ok() && std::fs::rename(&temporary, path).is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_state_reaches_disk_in_order() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let path = keep.path().join("state");
        let writer = StateWriter::new();
        for generation in 0..50u8 {
            writer.store(path.clone(), move || Some(vec![generation; 16]));
        }
        writer.flush();
        // Coalescing means intermediate generations may be skipped, but the
        // last one queued is always what lands.
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
    fn a_partially_written_state_is_never_published() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let path = keep.path().join("state");
        std::fs::write(&path, b"original").expect("state should be writable");
        write_atomically(&path, b"replacement");
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
