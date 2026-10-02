# System Architecture & Internals

This document details the internal design, architectural trade-offs, and operational mechanisms implemented in Autobahn.

## 1. Core Architectural Tenet: Zero Cost for Unchanged Subtrees

In typical software repositories, the overwhelming majority of files remain unchanged between sync cycles. Conventional sync tools incur CPU and memory overhead proportional to the total file count by repeatedly traversing directory structures or re-hashing trees.

Autobahn eliminates this overhead by enforcing an **immutable, shared in-memory tree representation**:
- **Pointer-Equality Shortcuts:** Scanned directory trees are structured as `Node` elements whose child collections reside behind atomic reference counters (`Arc<[Node]>`, defined in `src/tree/mod.rs`). When an incremental scan determines that a subdirectory has not been modified, it clones the pointer rather than allocating a new tree.
- **Constant-Time Equivalence Checks:** Testing whether two subtrees are identical reduces to comparing pointer addresses (`nodes_share_storage` in `src/tree/mod.rs`).
- **Wire Optimizations:** If a remote endpoint determines that its tree is unchanged, it replies to the controller with a single-byte enum tag, eliminating serialization, network transfer, and deserialization overhead.
- **Incremental Merkle Digests:** Subtrees compute hierarchical Merkle digests. When a file is modified, only directory digests on the path from the modified file to the root are recomputed; unmodified sibling trees reuse cached digests.

> [!IMPORTANT]
> Pointer identity proves subtree equality, but the inverse does not hold: two independently scanned trees containing identical file contents do not share storage pointers. The system uses pointer identity strictly to prove equivalence, never to prove divergence.


## 2. Change Detection: High-Speed Watchers with Bounded Auditing

Autobahn couples asynchronous kernel filesystem event notifications (`inotify` on Linux, `FSEvents` on macOS) with periodic background audits:
1. **Dirty-Path Tries:** Filesystem events populate an in-memory trie of modified directory paths (`src/scan/mod.rs`). Incremental scans inspect only dirtied branches, adopting unmodified subtrees via pointer copies without calling `stat` or `readdir`.
2. **Kernel Event Overflow Protection:** If the kernel event queue overflows or the dirty-path set exceeds 8,192 entries, the watcher discards its cache and schedules an immediate full scan.
3. **Periodic Audit Thread:** A background full-walk audit executes every 120 seconds (10 minutes on battery when `power_saver_experimental = true`). Audits run concurrently alongside active scans, identifying changes missed by the kernel without delaying active sync cycles.


## 3. Persistent State & Provenance

Autobahn differentiates between derived runtime data and authoritative synchronization history:
- **Scan Caches (Ephemeral):** Caches record previous filesystem states to accelerate subsequent scans. Losing a scan cache incurs only the cost of a fresh full scan.
- **Ancestor Baselines (Authoritative):** The ancestor record represents the last acknowledged state agreed upon by both endpoints. Losing or corrupting the ancestor eliminates provenance, converting user rollbacks into apparent modifications on the opposing endpoint.
- **Durable Logging:** Ancestor mutations are committed to an append-only, checksummed journal (`src/session/ancestor.rs`). If a crash occurs mid-cycle, intent markers identify affected paths and expose them as conflicts rather than risking data corruption.

## 4. Single-Source Transition Rendering

When a sync cycle applies changes, multiple components require state updates: the ancestor journal, the local endpoint model, the controller's remote model, and the remote agent's local state.

To eliminate divergence across these subsystems, file transition engines emit a unified execution record for each requested operation (`src/endpoint/mod.rs`):
- Describes the exact post-transition state of disk paths (successful creation, surviving content upon refusal, or partial tree construction).
- All four subsystems consume this identical rendering, guaranteeing state convergence across the cluster.

## 5. Write Validation & Lease Verification

Reconciliation decisions are planned from a specific point-in-time filesystem snapshot. In the window between planning and disk application, external processes may modify the local filesystem.

To prevent race conditions, write transitions validate every path against the exact scan generation from which the plan was derived (the **lease**):
- Before modifying or unlinking a file, the transition engine verifies that the live filesystem entry matches the expected metadata recorded in the snapshot (`src/endpoint/local.rs`).
- Path components are traversed using `symlink_metadata` to prevent symlink redirect attacks.
- File creations utilize atomic no-replace operations (`RENAME_NOREPLACE` on Linux, `RENAME_EXCL` on macOS).

## 6. Execution Lifecycle & Debounce Tuning

Synchronization sessions execute within dedicated worker threads coordinated by the supervisor:

1. **Parallel Endpoint Scans:** Controller queries local and remote endpoints concurrently.
2. **Early Quiescence Return:** If both endpoints share storage pointers and no pending issues exist, the cycle terminates immediately.
3. **Debounce Settle Windows:** Rather than syncing on the first detected filesystem event, Autobahn monitors change counters across short sleep slices (5ms to 25ms). If change counts stabilize, the burst has concluded and the cycle begins.
4. **Active Write Descriptor Detection:** On Linux, `inotify` tracks whether temporary save files remain open for writing. If an editor (e.g., Vim) is mid-flush, synchronization pauses until the file descriptor closes, preventing incomplete temporary files from being transmitted across the wire.
