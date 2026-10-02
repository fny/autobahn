# Safety

Autobahn checks provenance and validates filesystem changes before applying them. Conflict-reporting modes preserve competing edits for review; alpha-winning and mirror modes deliberately discard some edits according to their policy. These safeguards have boundaries, including local pathname races and untrusted peers, described below.

This page explains how, in plain terms. Each guarantee corresponds to a numbered invariant in [`correctness/INVARIANTS.md`](./correctness/INVARIANTS.md), which states it precisely, names the code that enforces it and the tests that check it, and was attacked by an independent review.

## The foundation: three versions, not two

Two copies of a file cannot say which side changed. If alpha holds A and beta holds B, either could be the edit and either could be stale. So autobahn keeps a third version: the **ancestor**, what both sides last agreed on. Against it, these ordinary file-edit cases in `two-way-conflict` have one answer (the one-off `sync` default; configuration files require a mode):

| alpha, against the ancestor | beta, against the ancestor | outcome |
|---|---|---|
| unchanged | unchanged | nothing to do |
| changed | unchanged | alpha's change travels to beta |
| unchanged | changed | beta's change travels to alpha |
| changed | changed, differently | a **conflict** — reported, both kept |

This is how autobahn tells "you deleted this file" from "this file never existed here", and why it never propagates a deletion it cannot justify. The other modes change the outcome for some rows — see [Modes](./modes.md).

Everything below exists to keep that ancestor honest and to make sure that what reconciliation decides is what actually reaches the disk.

## The guarantees

### What it sees is current (I1)

Autobahn watches the filesystem so that a scan re-reads only what changed. Filesystem events are fast but unreliable: the kernel drops them when its queue overflows, and a watch can be lost.

So every scan is tagged with a *generation*, and a scan is never used as current once a change it did not see has been reported. Autobahn's own writes announce their paths before the first write and again after the last, so a scan racing a write cannot outlive it. When the record of changed paths overflows (8,192 paths) or the kernel drops events, the record is thrown away and the whole tree is walked; a full walk is also due every 120 seconds (10 minutes on battery when `power_saver_experimental = true`). With a working watch, that audit runs beside foreground scans and marks differences for the next scan; discovery also takes the walk and next-cycle time. **A watch failure costs latency, never correctness.**

### The ancestor records only real agreement (I2)

The ancestor is updated only after both sides confirm their writes. Before the first write of a cycle, autobahn records its *intent* — the paths it is about to touch — and, whenever a remote machine is involved, forces that record to disk first. If anything crashes mid-cycle, the next start finds the unfinished intent and marks those paths as unknown.

Unknown provenance surfaces as a conflict, never as an overwrite. A crash can remove what the ancestor knows; it can never make it claim an agreement that did not happen. And an ancestor that cannot be read — damaged, or written by another build — is never silently reset, since a reset would bring deleted files back. The first cycle scans both sides: if they already match, reconciling them with no history changes nothing, so the ancestor is rebuilt from them and the old one kept beside it as evidence. If they differ, the session halts and says so, and `autobahn doctor` shows how. Damage is rebuilt once per session; a second time halts until a `reset`, because a disk that damages one ancestor will damage another.

### Content is what its digest says (I3)

Every file that arrives is written to a temporary name while its digest is computed, and moved into staging only if the digest matches. Content left over from an interrupted run is checked again before it is reused. At the moment of publication, the staged bytes are checked once more. Corrupted bytes cannot reach a tree.

### A write happens only if nothing changed since the decision (I4)

Reconciliation decides what to do from one scan, and the filesystem can change before the write. So every write is checked against **that exact scan**: the scan must record the expected digest and the file must still have its recorded metadata before replacement or removal. Validation does not rehash the live file; timestamp-preserving rewrites remain a boundary. A symbolic link anywhere on the path is a refusal, not a redirection. Removal works from the bottom up and must account for every entry on disk, because content the scan never saw is content nobody decided to delete.

New files are created by rename, and on Linux and macOS that rename refuses to replace something that appeared in the meantime. A refused path is reported as a problem and never stops the other paths in the cycle. If the refusal shows that the filesystem disagreed with the scan, the next scan re-reads instead of trusting its baseline.

### A crash never leaves a half-written file (I5)

New content becomes visible only by rename, so a reader sees the old file or the new one, never a mixture. Together with the intent record and the digest checks, a crash at any point recovers to a tree in which every file is one of its legitimate versions. This is tested at every step of the staging and transition lifecycle, and by cutting a real remote connection at every frame boundary, in both directions.

### One writer per region (I6)

Two sessions writing overlapping trees from separate ancestors would each read the other's writes as edits and send them back, forever. So a nested writable root is refused when the configuration loads — unless the outer session ignores the inner root, in which case they do not overlap at all (see [Overlapping and nested roots](./nesting.md)). The same pair of trees is locked machine-wide, even across different state roots, and a state root admits one supervisor at a time.

### Nothing is destroyed silently (I7)

In `two-way-conflict`, content is overwritten or deleted only when the ancestor proves the other side already had it. A deletion on one side against a modification on the other brings the content back.

Disappearance of a whole root is **halted** rather than propagated. If the ancestor recorded at least two entries and a synchronization root is empty or gone on exactly one side, the session stops: an unmounted disk is far more likely than a deliberate wipe. A missing *alpha* root is a halt too, never an empty source, so a mistyped path in a mirroring mode cannot empty the destination. A halt needs a person, with one exception: a missing alpha clears on its own when the folder comes back — a drive reconnected, a share remounted — and it alerts only once it has been gone two minutes.

Below the root, a directory emptied on one side is deletions and they propagate — unless `guard_directory_deletes_over` is set, where a directory of at least that many synchronized entries emptied on one side is a conflict, and in two-way modes one missing against an unchanged peer is restored. Recorded mounts have a separate guard independent of that threshold. See [Large directories](./modes.md#3-large-directory-protection-guard_directory_deletes_over).

### Both ends play by the same rules (I8)

The controller and the remote agent must report exactly the same version, including a *compatibility epoch* that is bumped whenever a change alters safety behavior or how a tree is read. A mismatch fails the handshake, with both versions named. The one mistake the installer cannot see — an old agent binary uploaded under a new version's name — is caught at that handshake, and the message names the bundle and its age. See [State](./state.md#compatibility-epochs).

### The network is not trusted (I9)

Lengths, flags and compressed sizes arriving over the connection are checked before anything is allocated. Each frame's length is checked against the 64 MiB frame cap before its buffer exists; decompression bombs are refused, and unknown flags are errors. Large messages travel as a sequence of 16 MiB frames and may reassemble to at most 4 GiB, a cap checked as each frame arrives, so memory follows the bytes actually received rather than a size the peer declared. A remote scan's delta is held to the same rule: its declared length is capped at 4 GiB and sizes at most 8 MiB of buffer up front, its block size must be one autobahn itself would choose, and each operation is refused before it is applied if it would outgrow the declared length.

### Saved state is whole or absent (I10)

Checkpoints, scan caches, and status files are published by temporary-file rename. The ancestor journal is appended with checksummed records; normalization replaces it atomically, and compaction confirms checkpoint durability before retiring journal history. A torn final journal record is discarded on replay. `durability = "power"` syncs every journal append, trading a little latency for power-loss durability of the journal's tail.

### A peer stays inside the root, genuine or not (I11)

Nothing the other end sends — whether it runs a genuine autobahn or anything else that speaks the protocol — can make this side read, write or delete outside the synchronization root or the session's own state. A peer can ask for only content this side's own scan recorded, by the digest it recorded; every path it names is checked component by component, with no `..`, no absolute path, no name autobahn reserves for itself, and no symbolic link on the way; and the session identifier that names the staging directory must be one autobahn generates. This is the one guarantee that does not assume genuine binaries on both ends. It is a pathname check, so a local process racing writes into the tree is RETAINED §2's boundary, and peering has additional trust requirements: ordinary peer SSH keys grant shell access unless restricted keys are used (see [Peering](./peering.md)).

## Before anything runs

A configuration mistake is refused at startup with the problem named, not half-applied or ignored:

- an unknown key, anywhere — a typo is an error, not a silent no-op
- two sessions that would write one tree region
- a session whose two sides are one tree, or one inside the other — from a configuration or from `autobahn sync ALPHA BETA`
- a local root that is, or holds, autobahn's own state or the directory the configuration lives in, unless its ignores keep that out
- an ignore negation that can never take effect, because a later pattern ignores it again
- an `ignores` `file:` entry that is missing, or a relative path
- an unknown mode, log level, or alert state

A root that holds credentials — `.ssh`, `.aws` and the like — is not refused, since someone may mean to synchronize them, but `sync` and `watch` warn about it when they start, until the group sets `acknowledge_secrets` or ignores them. See [What is refused](./configuration.md#what-is-refused).

And the commands that act refuse to run as root, unless `--allow-root` or `experimental.allow_root` says root is meant — and never as root under another user's home.

## When you act

The commands that change things are built on the same guarantees:

- **`resolve`** retires the losing version and forgets the settled ancestor paths through the session, using the same checked write path a cycle uses, so an entry that changed since you started is refused and reported, not destroyed. It asks before acting.
- **`reset`** forgets the ancestor, which brings deletions back — so it requires the group name and is never a default.
- **`restart`** kills the supervisor and starts it again. That is safe because of the guarantees above: a cycle in flight is abandoned and redone, never half-applied.
- **`clean`** removes state only for sessions the configuration no longer describes, never a running session's state, and never anything in the synchronized trees. `clean --agents` never removes the agent version in use.

Staging is also swept at the end of every cycle, of anything no request named — so a file that changed mid-transfer does not leave its previous version behind in `~/.autobahn`.

## How the guarantees are checked

- **Crash points are enumerated, not sampled.** The ancestor journal is cut at every byte and must reopen to an acknowledged state. A real agent connection is cut at every frame boundary in both directions.
- **Many tests are mutation-checked.** The enforcing code was deliberately broken, and the test was confirmed to fail — so the test proves the code matters, not merely that it runs.
- **Randomized interleaving sweeps** search for a schedule in which a stale scan is served as current. The sweep found two real holes on its first runs; both are fixed.
- **An independent review** from a different model lineage attacked the invariants themselves and produced 22 findings. The six confirmed ones were fixed, each with a test written first.

## Where the guarantees stop

These are deliberate boundaries, each with its reasoning in [`correctness/RETAINED.md`](./correctness/RETAINED.md):

- **Integrity and availability assume both endpoints run genuine autobahn binaries.** A hostile agent that speaks the protocol correctly could fabricate results — and so steer reconciliation into overwriting or deleting content inside the root — or exhaust the controller's memory. Defending the tree's contents against the machine you synchronize with is a different product. What such a peer cannot do is reach outside the root (I11).
- **Pair exclusion is per machine, per user, per default state root (`AUTOBAHN_HOME`).** Explicit `--state-root` and `--state-dir` overrides share that lock root. The same pair of trees driven from two machines is not detected (§4).
- **Network filesystems** may never deliver change events and may cache attributes (§3). If a root must live on one, treat this client as its only writer.
- **A same-length rewrite that restores the modification time** evades change detection. Unchanged-file detection compares modification time, size, inode and file type; the change time (ctime) is not consulted, so restoring the modification time is enough. `autobahn verify` re-reads every byte (§5).
- **A mount that was never seen mounted.** A mount inside a root is walked like any other directory unless `ignore_mounts = true`. One that goes away where the ancestor held content halts the session, at any size — but only if an earlier scan recorded it as a mount. A drive mounted and unmounted between two cycles, or a session created while it was already unplugged, is just a directory that emptied, and below the root that propagates as deletions unless `guard_directory_deletes_over` is set.
- **A power loss** can expose unsynced bytes under a renamed name. The durable ancestor and re-verification restore the tree on the cycles that follow.

## Filesystems

Autobahn adapts to each filesystem it touches, probing per root: executable bits are carried across volumes that cannot store them, names recompose to NFC on decomposing (HFS+-style) volumes, and case-insensitive volumes refuse case-colliding siblings instead of corrupting them.

## See also

- [How autobahn works](./how-it-works.md) — the design these guarantees come from
- [`correctness/INVARIANTS.md`](./correctness/INVARIANTS.md) — each guarantee stated precisely, with its code and tests
- [Modes](./modes.md) — deletions travel in every mode; what varies is the reverse
- [Overlapping and nested roots](./nesting.md) — why two ancestors over one region is refused
- [Scope and support boundaries](./support-boundaries.md) — platforms and filesystems
