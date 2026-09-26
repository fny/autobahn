# F-H5: Deep directory trees don't overflow a thread's stack

**Findings:** H-5 (OPUS H4, reproduced; KIMI ABN-L9).
**Status:** proposed. High; fix before v1.

## Problem

The scanner recurses once per directory level: `scan_directory` → `walk` → `scan_entry` (`src/scan/mod.rs:588`, `:902`, `:987`). So do many functions on the tree model: `recount`, `tree::apply`, `diff`, `Node::validate` (`src/tree/mod.rs:341`), reconcile, serde encode and decode, and `Drop` for `Node`.

Nothing sets a thread stack size anywhere in the repository. Scan helpers (`src/scan/mod.rs:800`), session workers (`src/supervisor/mod.rs:533`) and the connection router (`src/transport/mux.rs:172`) therefore all run on default 2 MiB stacks.

OPUS reproduced it with `autobahn sync` on a `d/d/d/…` chain. Depth 1,400 worked. Depths 1,700 and 1,850 aborted with `fatal runtime error: stack overflow`, exit code 134. The path at that depth is about 3.5 KB, under `PATH_MAX`, so the name-too-long guard never trips.

A stack overflow aborts the process; it is not a panic, so every session in the supervisor, or the whole agent, dies with it. The tree is still there after a restart, so a login service crash-loops.

## Proposed resolution

- **Give the deep workers big stacks.** Set an explicit stack size wherever recursion over a tree can run:
  - scan helpers and the scan's own thread;
  - session workers;
  - the connection router and agent channel threads, which decode `Node`;
  - the thread running a one-shot `sync`.

  Use `std::thread::Builder::stack_size`. For `scope.spawn`, use `std::thread::Builder::spawn_scoped`. The work the one-shot CLI does on the main thread moves onto one such thread. 64 MiB is enough, and costs only address space until it is touched. At roughly 1.2 KB per level, it covers about 50,000 levels, well past the roughly 2,000 that `PATH_MAX` allows.
- **One constant, one helper.** Put the size in a single `DEEP_STACK` constant, with a `spawn_deep` helper next to it, so new threads use it by default.
- **A depth cap in the scanner, as a backstop.** Stop descending past a fixed depth, say 4,096, and record the deeper directory as `Problematic`, "too deep". A FUSE filesystem can fake an arbitrarily deep tree without going over `PATH_MAX` per component, and this bounds it.
- **Iterative `Drop` for `Node`,** so freeing a deep tree needs no deep stack.
- **Peer-supplied depth stays out of scope.** A hostile peer sending a deeply nested `Node` is H-6, a tier 2 boundary, but the bigger stacks cover honest deep trees coming over the wire.

## Tests

- A one-shot `sync` of a 2,500-level chain succeeds. Skip on filesystems that refuse paths that long.
- A scan that goes past the cap records a `Problematic` entry and doesn't abort.
- Encoding, decoding and dropping a 5,000-level `Node` on a `spawn_deep` thread all succeed.
- **Mutation check:** with the stack size removed, the 2,500-level test aborts. Run it in a child process so the abort shows up as a test failure.
