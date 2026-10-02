# Modes

A **mode** defines two things:

1. **Direction:** Do changes flow bidirectionally (`two-way`), or only from source to destination (`one-way` from alpha to beta)?
2. **Conflict Policy:** When both sides disagree on a file, does it pause as a **conflict**, or does **alpha** automatically win?

---

## Core Modes at a Glance

| Mode | Direction | Policy | Best For... |
| --- | --- | --- | --- |
| `two-way-conflict` | Alpha ↔ Beta | Report conflict | Editing on both sides without risking data loss. |
| `two-way-alpha` | Alpha ↔ Beta | Alpha wins | Active editing on both sides, but Alpha is the primary authority. |
| `two-way-alpha-strict` | Alpha ↔ Beta | Alpha wins (strict) | Alpha is authoritative, and Alpha's deletions must override Beta's edits. |
| `one-way-conflict` | Alpha → Beta | Beta changes pause | Deployments where Beta generates local files (logs, caches) that Alpha must not touch. |
| `one-way-alpha` *(alias: `mirror`)* | Alpha → Beta | Alpha mirrors strictly | Backups and releases where Beta must be an exact, identical replica of Alpha. |
| `p2p-*-dangerously-experimental` | Failover mesh | Conflict or Alpha | Multi-node failover when Alpha goes offline. *(See [P2P](./p2p.md))* |


## Behavior Matrix

The modes behave identically for standard actions: unchanged files remain untouched, new files on Alpha propagate to Beta, and renames apply across sides. The table covers regular files within the synchronized scope. Root and mount safeguards, excluded content, and an optional directory-deletion guard can prevent a transition:

| Event | `two-way-conflict` | `two-way-alpha` | `two-way-alpha-strict` | `one-way-conflict` | `one-way-alpha` (`mirror`) |
| --- | --- | --- | --- | --- | --- |
| **Alpha edits a file** | Copied to Beta | Copied to Beta | Copied to Beta | Copied to Beta | Copied to Beta |
| **Beta edits a file** | Copied to Alpha | Copied to Alpha | Copied to Alpha | Stays on Beta; **reported as conflict** | **Overwritten** from Alpha |
| **Both edit the same file** | **Conflict reported**; both versions kept | Alpha wins silently | Alpha wins silently | **Conflict reported**; both versions kept | Alpha wins silently |
| **Alpha deletes a file** | Deleted on Beta | Deleted on Beta | Deleted on Beta | Deleted on Beta | Deleted on Beta |
| **Alpha deletes, but Beta edited** | Beta's edit wins and copies back to Alpha | Beta's edit wins and copies back to Alpha | **Deleted on Beta** (Alpha's deletion wins) | Stays on Beta untracked (treated as a new Beta file) | **Deleted on Beta** |
| **Beta creates a new file** | Kept & copied to Alpha | Kept & copied to Alpha | Kept & copied to Alpha | Kept locally on Beta | **Deleted** on Beta |
| **Beta deletes a file** | Deleted on Alpha | Deleted on Alpha | Deleted on Alpha | Restored from Alpha | Restored from Alpha |

---

## Key Behaviors Explained

### 1. `one-way-conflict` vs. `one-way-alpha`

* **`one-way-conflict` protects Beta from data loss:** It pushes updates from Alpha to Beta, but if Beta modifies a file locally, the engine refuses to overwrite it and flags a conflict. New local files on Beta are ignored and preserved.
* **`one-way-alpha` enforces parity:** Beta is made to match Alpha within the synchronized scope. Beta-only files—including runtime logs, caches, and build artifacts—are **deleted** unless they are excluded by policy. Ignored entries are not mirrored individually, but deleting their parent can remove them; see [Ignores](./ignores.md#ignores-and-deletion).

### 2. Deletions vs. Edits: Standard vs. Strict

Because a deletion contains no file content, reconciling a deletion against an active edit risks permanently destroying someone's work.

* **`two-way-conflict` & `two-way-alpha` (Edit Wins):** If Alpha deletes a file that Beta modified, the system prioritizes preserving data: Beta's edit survives and is copied back to Alpha. (This commonly happens during renames: Alpha renames `foo` to `bar` while Beta edits `foo`; `foo` reappears on Alpha with Beta's changes).
* **`two-way-alpha-strict` (Deletion Wins):** Removes this safety fallback. Alpha's deletion is absolute. If Alpha deletes a file, Beta's local edits to that file are discarded. Beta can still push newly created files back to Alpha.

### 3. Large Directory Protection (`guard_dir_deletes_over`)

When a mounted drive disconnects, an operating system often presents the mount point as a valid, empty folder. Standard three-way sync engines interpret this as: *"Every file inside was intentionally deleted,"* propagating mass deletions to the other side.

This was a mode of its own, `two-way-paranoid`, which did nothing else — so it could not be had alongside a one-way or p2p direction. It is a setting now, and works with any mode:

```toml
[groups.work]
guard_dir_deletes_over = 8
```

Set it, and a directory the ancestor recorded with that many entries or more — **counted recursively**, so one subfolder of seven files reaches eight — is no longer trusted when it disappears from exactly one side:

* **Directory emptied on one side:** Flags a **directory conflict** at that path and moves nothing beneath it. Resolve with `resolve --keep <full-side>` to restore the files, or `resolve --keep <empty-side>` to approve deleting the entire tree.
* **Directory missing entirely on one side, in a two-way mode:** If the remaining side matches the last-known synchronized state, the directory is **restored**, not deleted. To intentionally delete it, remove it on both sides or empty it first and resolve the conflict. In a one-way mode, the ordinary directional rules still apply to a missing directory: this setting does not restore alpha from beta.

Unset—the default—this extra directory guard is off. The independent safeguards for a missing alpha, an emptied root with at least two ancestor entries, and a previously recorded mount still apply; see [Safety](./safety.md).

---

## Fan-Out Topology (One Alpha, Multiple Betas)

When an Alpha syncs concurrently to multiple Betas, each pair runs an independent session. The chosen mode determines how concurrent edits from different Betas are handled:

* **Under `two-way-conflict`:** If Beta A's edit reaches Alpha before the session with a differently edited Beta B reconciles, that session reports a conflict against its old ancestor. Both edits are preserved until manually reviewed.
* **Under `two-way-alpha`:** Once Beta A's edit reaches Alpha, a competing Beta B edit against the old ancestor loses to the version now on Alpha. A beta edit still travels to Alpha when Alpha is unchanged against that pair's ancestor. This is per-session three-way reconciliation, not a last-writer-wins policy; transition validation refuses writes based on stale scans. Use `two-way-conflict` when competing edits need review.

---

`two-way-paranoid` has been removed; migrate it to `two-way-conflict` with `guard_dir_deletes_over = 8`.

## See Also

* [Configuration](./configuration.md) — How to set the `mode` parameter.
* [Conflicts](./conflicts.md) — Resolving and settling flagged file collisions.
* [Safety](./safety.md) — Root-level safeguards against accidental wipeouts.
* [P2P](./p2p.md) — Automatic leader failover setup and edge cases. Dangerously experimental, with known security issues.
