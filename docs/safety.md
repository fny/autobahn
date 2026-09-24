# Safety

Autobahn's first promise is that it never loses an edit. Speed comes second. Being wrong is worse than being slow: a synchronizer that overwrites a deliberate change has destroyed work that may exist nowhere else. So every part of the design fails toward *refusing*, *halting*, or *reporting a conflict* — never toward guessing.

This page explains how, in plain terms. Each guarantee corresponds to a numbered invariant in [`correctness/INVARIANTS.md`](./correctness/INVARIANTS.md), which states it precisely, names the code that enforces it and the tests that check it, and was attacked by an independent review.

## The foundation: three versions, not two

Two copies of a file cannot say which side changed. If alpha holds A and beta holds B, either could be the edit and either could be stale. So autobahn keeps a third version: the **ancestor**, what both sides last agreed on. Against it, every case in the default mode has one answer:

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

So every scan is tagged with a *generation*, and a scan is never used as current once a change it did not see has been reported. Autobahn's own writes announce their paths before the first write and again after the last, so a scan racing a write cannot outlive it. When the record of changed paths overflows (8,192 paths) or the kernel drops events, the record is thrown away and the whole tree is walked; a full walk also runs at least every 120 seconds regardless. **A watch failure costs latency, never correctness.**

### The ancestor records only real agreement (I2)

The ancestor is updated only after both sides confirm their writes. Before the first write of a cycle, autobahn records its *intent* — the paths it is about to touch — and, whenever a remote machine is involved, forces that record to disk first. If anything crashes mid-cycle, the next start finds the unfinished intent and marks those paths as unknown.

Unknown provenance surfaces as a conflict, never as an overwrite. A crash can remove what the ancestor knows; it can never make it claim an agreement that did not happen. And an ancestor that cannot be read — damaged, or written by another build — is never silently reset, since a reset would bring deleted files back. The first cycle scans both sides: if they already match, reconciling them with no history changes nothing, so the ancestor is rebuilt from them and the old one kept beside it as evidence. If they differ, the session halts and says so, and `autobahn doctor` shows how. Damage is rebuilt once per session; a second time halts until a `reset`, because a disk that damages one ancestor will damage another.

### Content is what its digest says (I3)

Every file that arrives is written to a temporary name while its digest is computed, and moved into staging only if the digest matches. Content left over from an interrupted run is checked again before it is reused. At the moment of publication, the staged bytes are checked once more. Corrupted bytes cannot reach a tree.

### A write happens only if nothing changed since the decision (I4)

Reconciliation decides what to do from one scan, and the filesystem can change before the write. So every write is checked against **that exact scan**: a file must still have the expected digest and metadata before it is replaced or removed. A symbolic link anywhere on the path is a refusal, not a redirection. Removal works from the bottom up and must account for every entry on disk, because content the scan never saw is content nobody decided to delete.

New files are created by rename, and on Linux and macOS that rename refuses to replace something that appeared in the meantime. A refused path is reported as a problem, makes the next scan re-read instead of trusting what it had, and never stops the other paths in the cycle.

### A crash never leaves a half-written file (I5)

New content becomes visible only by rename, so a reader sees the old file or the new one, never a mixture. Together with the intent record and the digest checks, a crash at any point recovers to a tree in which every file is one of its legitimate versions. This is tested at every step of the staging and transition lifecycle, and by cutting a real remote connection at every frame boundary, in both directions.

### One writer per region (I6)

Two sessions writing overlapping trees from separate ancestors would each read the other's writes as edits and send them back, forever. So a nested writable root is refused when the configuration loads — unless the outer session ignores the inner root, in which case they do not overlap at all (see [Overlapping and nested roots](./nesting.md)). The same pair of trees is locked machine-wide, even across different state roots, and a state root admits one supervisor at a time.

### Nothing is destroyed silently (I7)

In the default mode, content is overwritten or deleted only when the ancestor proves the other side already had it. A deletion on one side against a modification on the other brings the content back.

Disappearance of a whole root is **halted** rather than propagated. If a synchronization root is empty or gone on exactly one side, the session stops: an unmounted disk is far more likely than a deliberate wipe. A missing *alpha* root is a halt too, never an empty source, so a mistyped path in a mirroring mode cannot empty the destination. A halt needs a person, with one exception: a missing alpha clears on its own when the folder comes back — a drive reconnected, a share remounted — and it alerts only once it has been gone two minutes.

Below the root, a directory emptied on one side is deletions and they propagate — except under `two-way-paranoid`, where a directory of eight or more synchronized entries emptied on one side is a conflict, and one gone on one side is restored. See [Large directories](./modes.md#large-directories).

### Both ends play by the same rules (I8)

The controller and the remote agent must report exactly the same version, including a *compatibility epoch* that is bumped whenever a change alters safety behavior or how a tree is read. A mismatch fails the handshake, with both versions named. The one mistake the installer cannot see — an old agent binary uploaded under a new version's name — is caught at that handshake, and the message names the bundle and its age. See [State](./state.md#compatibility-epochs).

### The network is not trusted (I9)

Lengths, flags and compressed sizes arriving over the connection are checked before anything is allocated. Oversized frames and decompression bombs are refused, and unknown flags are errors. Large messages travel as a sequence of 16 MiB frames and may reassemble to at most 4 GiB.

### Saved state is whole or absent (I10)

The ancestor journal and its checkpoints, the scan cache, and the status files are all written to a temporary and renamed into place, with syncs where a power loss would matter. A torn final journal record is discarded on replay. `durability = "power"` syncs every journal append, trading a little latency for power-loss durability of the journal's tail.

## Before anything runs

A configuration mistake is refused at startup with the problem named, not half-applied or ignored:

- an unknown key, anywhere — a typo is an error, not a silent no-op
- two sessions that would write one tree region
- an ignore negation that can never take effect, because a later pattern ignores it again
- an `ignore_files` entry that is missing, or a relative path
- an unknown mode, log level, or alert state

## When you act

The commands that change things are built on the same guarantees:

- **`resolve`** retires the losing version through the same checked write path a cycle uses, so an entry that changed since you started is refused and reported, not destroyed. It asks before acting.
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

- **Both endpoints must run genuine autobahn binaries.** A hostile agent that speaks the protocol correctly could fabricate results. Defending against the machine you synchronize with is a different product.
- **Exclusion is per machine, per user, per state root.** The same pair of trees driven from two machines is not detected (§4).
- **Network filesystems** may never deliver change events and may cache attributes (§3). If a root must live on one, treat this client as its only writer.
- **A same-length rewrite that restores the modification time** evades change detection. `autobahn verify` re-reads every byte (§5).
- **A vanished mount holding fewer than eight entries** — one huge file — evades the mass-disappearance guard (§1), but only with `ignore_mounts = false`. By default a mount inside a root is not synchronized at all, and one that goes away is remembered and left out while its mount point is empty; followed, a mount point that was one and is now empty where the ancestor held content halts.
- **A power loss** can expose unsynced bytes under a renamed name. The durable ancestor and re-verification restore the tree on the cycles that follow.

## Filesystems

Autobahn adapts to each filesystem it touches, probing per root: executable bits are carried across volumes that cannot store them, names recompose to NFC on decomposing (HFS+-style) volumes, and case-insensitive volumes refuse case-colliding siblings instead of corrupting them.

## See also

- [How autobahn works](./how-it-works.md) — the design these guarantees come from
- [`correctness/INVARIANTS.md`](./correctness/INVARIANTS.md) — each guarantee stated precisely, with its code and tests
- [Modes](./modes.md) — deletions travel in every mode; what varies is the reverse
- [Overlapping and nested roots](./nesting.md) — why two ancestors over one region is refused
- [Scope and support boundaries](./support-boundaries.md) — platforms and filesystems
