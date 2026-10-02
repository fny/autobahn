# Safety Architecture & Invariants

Autobahn is designed around formal correctness invariants to prevent silent data loss, corrupt transfers, or unintended overwrites during continuous synchronization.

## Core Principles: Three-Way Reconciliation

Two filesystem endpoints cannot determine provenance in isolation. If Primary holds version $A$ and Replica holds version $B$, neither endpoint can tell which side was updated and which side is stale.

Autobahn maintains a third artifact: the **Ancestor Baseline**, which records the last mutually agreed state. Changes are evaluated against this ancestor:

| Primary vs. Ancestor | Replica vs. Ancestor | Reconciliation Action (`two-way-conflict`) |
| :--- | :--- | :--- |
| Unchanged | Unchanged | No operation. |
| Modified | Unchanged | Propagate Primary modification to Replica. |
| Unchanged | Modified | Propagate Replica modification to Primary. |
| Modified | Modified | **Conflict detected:** Propagation pauses on path; both versions preserved. |

This three-way model allows Autobahn to differentiate intentional deletions from files that never existed, ensuring deletions are never propagated without historical proof.

## Invariant Guarantees

Every operational guarantee corresponds to a formal invariant defined in [`correctness/invariants.md`](./correctness/invariants.md):

### I1. Observation Currency
Scans are assigned monotonic **generations**. An observation snapshot is never served as current if subsequent filesystem events or write invalidations have been reported. Watcher failures drop into scheduled full directory audits (every 120s by default, or 10m on battery). Watcher failures impact propagation latency, never correctness.

### I2. Unambiguous Provenance
The ancestor record is updated only after both endpoints acknowledge transition completion. Prior to writing, the controller records its write **intent** to disk. If an unrecoverable failure occurs mid-cycle, pending intent paths are marked as unknown on recovery, surfacing as conflicts rather than silent overwrites.

### I3. Digest Integrity
All in-flight data is staged under content-addressed temporary paths. Content bytes are verified against their cryptographic digest upon receipt, prior to staging, and once more immediately before atomic publication.

### I4. Lease Validation
Transitions validate that the target filesystem entry matches the exact snapshot generation (the **lease**) from which reconciliation was planned. If a concurrent external modification altered the path during transfer, the write is refused, flagging a problem and triggering an immediate scan.

### I5. Crash Resistance via Atomic Publication
Files are never modified in-place. Updates are staged to temporary files on the same filesystem and published using atomic renames (`RENAME_NOREPLACE` on Linux, `renamex_np` with `RENAME_EXCL` on macOS). An abrupt crash or power failure leaves either the original file or the new file intact, never a truncated state.

### I6. Confinement to Single Writers
Two independent sessions cannot synchronize overlapping directories unless the parent session explicitly ignores the child root via `ignores`. Mutex locks prevent concurrent sessions from driving identical root pairs on the same host.

### I7. Root Disappearance Safeguards
If an endpoint's root directory is emptied or disappears while the ancestor records two or more entries, synchronization immediately triggers a **Safety Halt** rather than propagating mass deletions. This guards against unmounted external drives or broken NFS shares being interpreted as intentional deletions.

### I8. Strict Compatibility Handshake
Controllers and remote agents verify matching protocol versions and compatibility epochs (`version+eNN`). Incompatible wire protocols or differing serialization schemas fail during initial connection negotiation.

### I9. Bounded Wire Framing
All network frames enforce strict bounds: 64 MiB frame limits, 4 GiB maximum aggregate message size, and bounded decompression buffers. Wire decoders validate lengths prior to allocating buffers to defend against memory exhaustion attacks.

### I10. State Journal Atomicity
Ancestor journals utilize checksummed binary records. Journal compaction and state updates execute via write-to-temporary and atomic rename sequences.

### I11. Path Traversal Confinement
The agent validates all requested operations component-by-component. Paths containing `..`, absolute references, reserved system names, or symbolic links traversing outside the synchronization root are strictly rejected.

## Verification & Validation Methodology

Autobahn's safety invariants are verified through automated testing pipelines:
- **Crash Point Enumeration:** The ancestor journal is truncated at every byte offset to ensure crash-recovery mechanisms consistently yield valid states.
- **Fault-Injection & Mutation Testing:** Key validation checks in the source code are systematically disabled in mutation tests to confirm test suites detect the breakage.
- **Schedule Interleaving Sweeps:** Multi-threaded randomized test runners simulate race conditions between scanner invalidations and file transitions.
- **Formal Independent Security Audits:** What they found is fixed, or written down in [accepted risks](./correctness/accepted-risks.md).

## Operational Boundaries

Guarantees operate within explicit physical and software constraints:
- **Local Filesystem Requirement:** Network filesystems (NFS/SMB) with aggressive client-side attribute caching may mask external modifications.
- **Timestamp Preservation:** Tools that rewrite files while deliberately preserving size and modification timestamps (`touch -r`) evade metadata-based change detection. Use `autobahn verify` to detect these changes.
- **Host Trust Model:** Protocols assume both endpoints execute genuine, authenticated Autobahn binaries. While a compromised agent cannot access paths outside its root (I11), it could emit falsified scan data within its designated root.
