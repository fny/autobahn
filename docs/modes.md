# Modes

A **mode** defines two things:

1. **Direction:** Do changes flow bidirectionally (`two-way`), or only from source to destination (`one-way` from the primary to the replica)?
2. **Conflict Policy:** When both sides disagree on a file, does it pause as a **conflict**, or does the **primary** automatically win?

---

## Core Modes at a Glance

| Mode | Direction | Policy | Best For... |
| --- | --- | --- | --- |
| `two-way-conflict` | Primary ↔ Replica | Report conflict | Editing on both sides without risking data loss. |
| `two-way-primary` | Primary ↔ Replica | Primary wins | Active editing on both sides, with the primary having the final say. |
| `two-way-primary-strict` | Primary ↔ Replica | Primary wins (strict) | The primary is authoritative, and its deletions override the replica's edits. |
| `one-way-conflict` | Primary → Replica | Replica changes pause | Deployments where the replica generates local files (logs, caches) that the primary must not touch. |
| `one-way-primary` *(alias: `mirror`)* | Primary → Replica | Primary mirrors strictly | Backups and releases where the replica must be an exact, identical copy of the primary. |
| `p2p-*-dangerously-experimental` | Failover mesh | Conflict or primary wins | Multi-node failover when the primary goes offline. *(See [P2P](./p2p.md))* |


## Behavior Matrix

The modes behave identically for standard actions: unchanged files remain untouched, new files on the primary propagate to the replica, and renames apply across sides. The table covers regular files within the synchronized scope. Root and mount safeguards, excluded content, and an optional directory-deletion guard can prevent a transition:

| Event | `two-way-conflict` | `two-way-primary` | `two-way-primary-strict` | `one-way-conflict` | `one-way-primary` (`mirror`) |
| --- | --- | --- | --- | --- | --- |
| **The primary edits a file** | Copied to the replica | Copied to the replica | Copied to the replica | Copied to the replica | Copied to the replica |
| **The replica edits a file** | Copied to the primary | Copied to the primary | Copied to the primary | Stays on the replica; **reported as conflict** | **Overwritten** from the primary |
| **Both edit the same file** | **Conflict reported**; both versions kept | The primary wins silently | The primary wins silently | **Conflict reported**; both versions kept | The primary wins silently |
| **The primary deletes a file** | Deleted on the replica | Deleted on the replica | Deleted on the replica | Deleted on the replica | Deleted on the replica |
| **The primary deletes, but the replica edited** | The replica's edit wins and copies back to the primary | The replica's edit wins and copies back to the primary | **Deleted on the replica** (the primary's deletion wins) | Stays on the replica untracked (treated as a new file on the replica) | **Deleted on the replica** |
| **The replica creates a new file** | Kept & copied to the primary | Kept & copied to the primary | Kept & copied to the primary | Kept locally on the replica | **Deleted** on the replica |
| **The replica deletes a file** | Deleted on the primary | Deleted on the primary | Deleted on the primary | Restored from the primary | Restored from the primary |

---

## Key Behaviors Explained

### 1. `one-way-conflict` vs. `one-way-primary`

* **`one-way-conflict` protects the replica from data loss:** It pushes updates from the primary to the replica, but if the replica modifies a file locally, the engine refuses to overwrite it and flags a conflict. New local files on the replica are ignored and preserved.
* **`one-way-primary` enforces parity:** The replica is made to match the primary within the synchronized scope. Files only on the replica—including runtime logs, caches, and build artifacts—are **deleted** unless they are excluded by policy. Ignored entries are not mirrored individually, but deleting their parent can remove them; see [Ignores](./ignores.md#ignores-and-deletion).

### 2. Deletions vs. Edits: Standard vs. Strict

Because a deletion contains no file content, reconciling a deletion against an active edit risks permanently destroying someone's work.

* **`two-way-conflict` & `two-way-primary` (Edit Wins):** If the primary deletes a file that the replica modified, the system prioritizes preserving data: the replica's edit survives and is copied back to the primary. (This commonly happens during renames: the primary renames `foo` to `bar` while the replica edits `foo`; `foo` reappears on the primary with the replica's changes).
* **`two-way-primary-strict` (Deletion Wins):** Removes this safety fallback. The primary's deletion is absolute. If the primary deletes a file, the replica's local edits to that file are discarded. The replica can still push newly created files back to the primary.

### 3. Large Directory Protection (`guard_dir_deletes_over`)

When a mounted drive disconnects, an operating system often presents the mount point as a valid, empty folder. Standard three-way sync engines interpret this as: *"Every file inside was intentionally deleted,"* propagating mass deletions to the other side.

This was a mode of its own, `two-way-paranoid`, which did nothing else — so it could not be had alongside a one-way or p2p direction. It is a setting now, and works with any mode:

```toml
[groups.work]
guard_dir_deletes_over = 8
```

Set it, and a directory the ancestor recorded with that many entries or more — **counted recursively**, so one subfolder of seven files reaches eight — is no longer trusted when it disappears from exactly one side:

* **Directory emptied on one side:** Flags a **directory conflict** at that path and moves nothing beneath it. Resolve with `resolve --keep <full-side>` to restore the files, or `resolve --keep <empty-side>` to approve deleting the entire tree.
* **Directory missing entirely on one side, in a two-way mode:** If the remaining side matches the last-known synchronized state, the directory is **restored**, not deleted. To intentionally delete it, remove it on both sides or empty it first and resolve the conflict. In a one-way mode, the ordinary directional rules still apply to a missing directory: this setting does not restore the primary from the replica.

Unset—the default—this extra directory guard is off. The independent safeguards for a missing primary, an emptied root with at least two ancestor entries, and a previously recorded mount still apply; see [Safety](./safety.md).

---

## Fan-Out Topology (One Primary, Multiple Replicas)

When a Primary syncs concurrently to multiple Replicas, each pair runs an independent session. The chosen mode determines how concurrent edits from different Replicas are handled:

* **Under `two-way-conflict`:** If replica A's edit reaches the primary before the session with a differently edited replica B reconciles, that session reports a conflict against its old ancestor. Both edits are preserved until manually reviewed.
* **Under `two-way-primary`:** Once replica A's edit reaches the primary, a competing edit on replica B against the old ancestor loses to the version now on the primary. A replica edit still travels to the primary when the primary is unchanged against that pair's ancestor. This is per-session three-way reconciliation, not a last-writer-wins policy; transition validation refuses writes based on stale scans. Use `two-way-conflict` when competing edits need review.

---

`two-way-paranoid` has been removed; migrate it to `two-way-conflict` with `guard_dir_deletes_over = 8`.

## See Also

* [Configuration](./configuration.md) — How to set the `mode` parameter.
* [Conflicts](./conflicts.md) — Resolving and settling flagged file collisions.
* [Safety](./safety.md) — Root-level safeguards against accidental wipeouts.
* [P2P](./p2p.md) — Automatic leader failover setup and edge cases. Dangerously experimental, with known security issues.
