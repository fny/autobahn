//! Threads with room for deep trees.
//!
//! The scanner recurses once per directory level, and so do the tree
//! model's walks, its serde encoding and decoding, and reconciliation. On
//! the default 2 MiB stack a `d/d/d/…` chain about 1,700 levels deep — a
//! path still well under `PATH_MAX` — overflowed it, and a stack overflow
//! is an abort, not a panic: every session in the supervisor, or the whole
//! agent, died with it, and a login service crash-looped on the tree that
//! was still there after the restart.
//!
//! So any thread that can run such a walk is started here, with
//! [`DEEP_STACK`]. The size costs address space only until it is touched.

use std::thread::{JoinHandle, Scope, ScopedJoinHandle};

/// The stack size of a thread that may walk a deep tree. At roughly 1.2 KB
/// a level for the deepest recursion (the scan), it covers about 50,000
/// levels, far past the roughly 2,000 that `PATH_MAX` allows and past the
/// scanner's own depth cap.
pub const DEEP_STACK: usize = 64 * 1024 * 1024;

/// Spawns a thread with a [`DEEP_STACK`], as [`std::thread::spawn`] does
/// otherwise: panicking if the operating system cannot start it.
pub fn spawn_deep<F, T>(work: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(DEEP_STACK)
        .spawn(work)
        .expect("failed to spawn thread")
}

/// Spawns a scoped thread with a [`DEEP_STACK`], as [`Scope::spawn`] does
/// otherwise.
pub fn spawn_deep_scoped<'scope, 'env, F, T>(
    scope: &'scope Scope<'scope, 'env>,
    work: F,
) -> ScopedJoinHandle<'scope, T>
where
    F: FnOnce() -> T + Send + 'scope,
    T: Send + 'scope,
{
    std::thread::Builder::new()
        .stack_size(DEEP_STACK)
        .spawn_scoped(scope, work)
        .expect("failed to spawn thread")
}

/// Runs `work` to completion on a thread with a [`DEEP_STACK`], and returns
/// what it returns: for work that would otherwise run on a thread whose
/// stack nobody chose, such as a command's main thread. A panic in `work`
/// continues on the calling thread.
pub fn run_deep<F, T>(work: F) -> T
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    std::thread::scope(|scope| match spawn_deep_scoped(scope, work).join() {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Content, Node};
    use std::sync::Arc;

    /// A chain of directories `depth` levels deep, built without recursion.
    pub(crate) fn chain(depth: usize) -> Node {
        let mut node = Node {
            name: "leaf".into(),
            content: Content::Directory(Arc::new(Vec::new())),
        };
        for _ in 0..depth {
            node = Node {
                name: "d".into(),
                content: Content::Directory(Arc::new(vec![node])),
            };
        }
        node
    }

    #[test]
    fn a_deep_node_encodes_decodes_and_drops_on_a_deep_thread() {
        spawn_deep(|| {
            let node = chain(5_000);
            let encoded = bincode::serialize(&node).expect("encodes");
            let decoded: Node = bincode::deserialize(&encoded).expect("decodes");
            assert!(decoded.content_equal(&node, true));
            drop(decoded);
            drop(node);
        })
        .join()
        .expect("the deep thread should finish");
    }

    /// Dropping needs no deep stack at all: freeing is iterative.
    #[test]
    fn a_deep_node_drops_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| drop(chain(100_000)))
            .expect("spawns")
            .join()
            .expect("dropping a deep tree should not overflow");
    }

    #[test]
    fn run_deep_returns_and_propagates_panics() {
        assert_eq!(run_deep(|| 7), 7);
        let caught = std::panic::catch_unwind(|| run_deep(|| panic!("inner")));
        assert!(caught.is_err());
    }
}
