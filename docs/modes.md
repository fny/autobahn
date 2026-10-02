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
| `one-way-alpha`<br>

<br>*(alias: `mirror`)* | Alpha → Beta | Alpha mirrors strictly | Backups and releases where Beta must be an exact, identical replica of Alpha. |
| `peering-*-dangerously-experimental` | Failover mesh | Conflict or Alpha | Multi-node failover when Alpha goes offline. *(See [Peering](./peering.md))* |


## Behavior Matrix

The modes behave identically for standard actions: unchanged files remain untouched, new files on Alpha propagate to Beta, and renames apply across sides. They differ in only seven specific collision and deletion scenarios:

| Event | `two-way-conflict` | `two-way-alpha` | `two-way-alpha-strict` | `one-way-conflict` | `one-way-alpha` (`mirror`) |
| --- | --- | --- | --- | --- | --- |
| **Alpha edits a file** | Copied to Beta | Copied to Beta | Copied to Beta | Copied to Beta | Copied to Beta |
| **Beta edits a file** | Copied to Alpha | Copied to Alpha | Copied to Alpha | Stays on Beta; **reported as conflict** | **Overwritten** from Alpha |
| **Both edit the same file** | **Conflict reported**;<br>
<br>both versions kept | Alpha wins silently | Alpha wins silently | **Conflict reported**;<br>
<br>both versions kept | Alpha wins silently |
| **Alpha deletes a file** | Deleted on Beta | Deleted on Beta | Deleted on Beta | Deleted on Beta | Deleted on Beta |
| **Alpha deletes, but Beta edited** | Beta's edit wins and copies back to Alpha | Beta's edit wins and copies back to Alpha | **Deleted on Beta** (Alpha's deletion wins) | Stays on Beta untracked (treated as a new Beta file) | **Deleted on Beta** |
| **Beta creates a new file** | Kept & copied to Alpha | Kept & copied to Alpha | Kept & copied to Alpha | Kept locally on Beta | **Deleted** on Beta |
| **Beta deletes a file** | Deleted on Alpha | Deleted on Alpha | Deleted on Alpha | Restored from Alpha | Restored from Alpha |

---

## Key Behaviors Explained

### 1. `one-way-conflict` vs. `one-way-alpha`

* **`one-way-conflict` protects Beta from data loss:** It pushes updates from Alpha to Beta, but if Beta modifies a file locally, the engine refuses to overwrite it and flags a conflict. New local files on Beta are ignored and preserved.
* **`one-way-alpha` enforces parity:** Beta is forced to match Alpha byte-for-byte. Any untracked file on Beta—including runtime logs, caches, and build artifacts—is immediately **deleted**.

### 2. Deletions vs. Edits: Standard vs. Strict

Because a deletion contains no file content, reconciling a deletion against an active edit risks permanently destroying someone's work.

* **`two-way-conflict` & `two-way-alpha` (Edit Wins):** If Alpha deletes a file that Beta modified, the system prioritizes preserving data: Beta's edit survives and is copied back to Alpha. (This commonly happens during renames: Alpha renames `foo` to `bar` while Beta edits `foo`; `foo` reappears on Alpha with Beta's changes).
* **`two-way-alpha-strict` (Deletion Wins):** Removes this safety fallback. Alpha's deletion is absolute. If Alpha deletes a file, Beta's local edits to that file are discarded. Beta can still push newly created files back to Alpha.

### 3. Large Directory Protection (`guard_directory_deletes_over`)

When a mounted drive disconnects, an operating system often presents the mount point as a valid, empty folder. Standard three-way sync engines interpret this as: *"Every file inside was intentionally deleted,"* propagating mass deletions to the other side.

This was a mode of its own, `two-way-paranoid`, which did nothing else — so it could not be had alongside a one-way or peering direction. It is a setting now, and works with any mode:

```toml
[groups.work]
guard_directory_deletes_over = 8
```

Set it, and a directory the ancestor recorded with that many entries or more — **counted recursively**, so one subfolder of seven files reaches eight — is no longer trusted when it disappears from exactly one side:

* **Directory emptied on one side:** Flags a **directory conflict** at that path and moves nothing beneath it. Resolve with `resolve --keep <full-side>` to restore the files, or `resolve --keep <empty-side>` to approve deleting the entire tree.
* **Directory missing entirely on one side:** If the remaining side matches the last-known synchronized state, the directory is **restored**, not deleted. To intentionally delete it, remove it on both sides simultaneously or empty it first and resolve the conflict.

Unset — the default — every disappearance propagates, whatever its size.

---

## Fan-Out Topology (One Alpha, Multiple Betas)

When an Alpha syncs concurrently to multiple Betas, each pair runs an independent session. The chosen mode determines how concurrent edits from different Betas are handled:

* **Under `two-way-conflict`:** Whichever Beta syncs first updates Alpha. When the second Beta syncs, Alpha detects a collision and flags a conflict. Both edits are preserved until manually reviewed.
* **Under `two-way-alpha`:** Whichever Beta syncs last wins. Because "Alpha always wins," Beta B's update to Alpha will silently overwrite Beta A's previous sync across the entire cluster. Use this mode only for central broadcast setups, not multi-node collaborative editing.

---

## See Also

* [Configuration](./configuration.md) — How to set the `mode` parameter.
* [Conflicts](./conflicts.md) — Resolving and settling flagged file collisions.
* [Safety](./safety.md) — Root-level safeguards against accidental wipeouts.
* [Peering](./peering.md) — Automatic leader failover setup and edge cases. Dangerously experimental, with known security issues.
