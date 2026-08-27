# How autobahn works

An implementation tour of autobahn 0.3.0, for people who change the code.
Each claim gives a `file:line` reference.

## The parts

There is one binary. The command `autobahn agent` makes it the remote half
(`src/main.rs:206`). Every other command makes it the controller. Both ends
are the same build, so the version handshake needs an exact match
(`src/transport/mod.rs:560`).

The controller is a hub. The two endpoints never talk to each other
(`src/endpoint/mod.rs:5`). All data goes through the controller. This costs
one network hop in the rare remote-to-remote case. It gives one place where
reconciliation happens, with one model of both sides.

A missing beta root is legal, because a transition creates it. A missing
alpha root is an error (`src/main.rs:425`). Without this rule, a typo in a
source path plus a mirroring mode empties the destination.

The supervisor runs one thread for each session (`src/supervisor/mod.rs:185`).
Each thread runs a cycle, records status, then waits for activity. After a
failure it backs off. The backoff doubles up to 300 seconds, then adds up to
24% jitter (`src/supervisor/mod.rs:693`). The jitter comes after the cap, so
saturated sessions stay spread out.

## The tree

```rust
pub struct Node { pub name: String, pub content: Content }   // src/tree/mod.rs:82
pub enum Content {                                           // src/tree/mod.rs:53
    Directory(Arc<Vec<Node>>),
    File { digest: Digest, executable: bool, metadata: FileMetadata },
    Symlink { target: String },
    Untracked,
    Problematic { message: String },
}
```

A `Node` is 96 bytes. Three properties do most of the work.

**Children are a sorted `Vec`, not a map.** The code sorts them at
construction (`src/tree/mod.rs:176`). Lookup is a binary search. Sorted
children make reconciliation and diff linear merges of two lists.

**Digests are inline on the file node**, next to the metadata that justifies
their reuse. There is no separate cache to keep in step.

**Children sit behind an `Arc`,** so successive trees share storage. A tree
of one million files costs approximately 96 MB. The next scan of an
unchanged tree adds almost nothing.

Copy-on-write is a property of the type system. Mutation goes through
`Arc::make_mut` (`src/tree/apply.rs:39`), which clones a child list only
when something else holds it.

### nodes_share_storage

```rust
pub fn nodes_share_storage(a: Option<&Node>, b: Option<&Node>) -> bool  // src/tree/mod.rs:378
```

This function compares pointers, not content. Storage sharing tells you how
a tree was made, not what it holds. A tree read from disk shares nothing.
Two trees with equal content can answer `false`.

**Callers can use this function to prove agreement. They must not use it to
prove difference.** Four things depend on it: the cycle short-circuit
(`src/session/mod.rs:284`), diff pruning (`src/tree/diff.rs:32`), scan-cache
write elision (`src/endpoint/local.rs:863`), and the `ScanUnchanged` reply
(`src/transport/mod.rs:420`).

Two content kinds matter later. `Untracked` is content on disk that stays
outside synchronization, such as an ignored or oversized file.
`Problematic` is content that the scan could not read. The ancestor holds
neither (`src/tree/mod.rs:346`).

## Scanning

A scan reads the tree and builds a `Node` (`src/scan/mod.rs:144`). The
previous tree, called the baseline, accelerates two separate things.

The scan reuses a digest only if the mtime seconds, mtime nanoseconds,
size, inode, and file type all match (`src/scan/mod.rs:650`). It adopts a
subtree only if the directories are pointer-equal and the file metadata is
identical (`src/scan/mod.rs:676`). Adoption starts at the bottom, so an
unchanged subtree becomes one `Arc` clone.

The scan reads ignore rules before it descends (`src/scan/mod.rs:491`), so
an ignored subtree costs nothing. A file above `max_file_size` becomes
`Untracked` without an open (`src/scan/mod.rs:528`). It is present, and it
can never look like a deletion.

The scan measures filesystem behavior instead of assuming it
(`src/scan/probes.rs`). It probes executability, Unicode decomposition,
normalization, and case sensitivity once for each endpoint.

### The incremental path

This is what makes a scan cost the size of the change, not the size of the
tree.

`DirtyPaths` is a trie of path components (`src/scan/mod.rs:48`). The
function `mark(path)` sets `relist` on the **parent** and adds a node for
the leaf (`src/scan/mod.rs:69`). It marks the parent because only a
directory listing shows a creation or a removal.

A directory scan then has three cases (`src/scan/mod.rs:293`):

- **Not marked, with a baseline directory.** The scan adopts the baseline
  content. It makes one `Arc` clone and reads nothing.
- **Marked, but `relist` is false.** The scan walks the baseline children.
  It clones the unmarked ones and rescans the marked ones.
- **`relist` is true.** The scan lists the directory. Even here it adopts an
  unmarked entry without a stat, because the listing proves the entry
  exists and the marks prove it did not change (`src/scan/mod.rs:400`).

Tests assert the contract in both directions. An incremental scan must
agree with a full scan from the same baseline (`src/scan/mod.rs:783`). With
nothing marked, a change made behind the scan stays invisible
(`src/scan/mod.rs:888`).

A full scan happens if there is no watcher, no baseline, or an incomplete
watch record (`src/endpoint/local.rs:465`). It also happens every 120
seconds, which bounds how long a missed event can persist. A transition
problem sets `last_full_scan` to `None` (`src/endpoint/local.rs:1102`),
because the filesystem disagreed with the tree and the record is stale.

## The cycle

One cycle does this (`src/session/mod.rs:260`):

1. Scan both endpoints in parallel.
2. Return early if the session is quiesced.
3. Collect scan problems.
4. Graft executability bits on a filesystem that cannot store them.
5. Halt if one side presents an empty root and the ancestor had children.
6. Reconcile.
7. Stage and apply transitions, beta first.
8. Fold the achieved changes into the ancestor.
9. Validate and write the ancestor synchronously.

### The quiesced short-circuit

The gate opens only if the session is quiesced and both fresh roots share
storage with the recorded roots (`src/session/mod.rs:284`). Two facts make
this safe.

The flag is set only after a cycle that left nothing outstanding
(`src/session/mod.rs:74`). That cycle applied no transitions, found no
conflicts, and reported no problems. Thus "the same as last time" means
"still synchronized".

Only a scan that adopted its baseline whole can produce pointer identity.
Equal content that was freshly built answers `false`. So the gate cannot
open for a tree that the scan actually re-read.

### Folding achieved changes

A transition returns one result for each request, in order
(`src/endpoint/mod.rs:79`). The result describes what is on disk after the
attempt. **Four** parties consume this one rendering
(`src/endpoint/mod.rs:98`): the ancestor, the local endpoint's tree, the
controller's model of the agent, and the agent's `last_sent` anchor. They
cannot disagree about what the cycle did, because they read the same
answer.

The results carry the metadata of the entries as created
(`src/endpoint/local.rs:1106`). Thus the next scan re-digests only what
changed after the transition.

## Watching and the settle

`ChangeWatcher` holds the changed paths and a one-slot wake channel
(`src/endpoint/local.rs:196`). If the record overflows 8192 paths, or the
kernel queue overflows, the watcher gives up its paths and asks for a full
scan (`src/endpoint/local.rs:184`).

A root that cannot be watched waits out the timeout. Watch failures cost
latency, never correctness.

After a change arrives, the session waits before it cycles. This groups a
burst of writes into one cycle. Until 0.3.0 the wait was a fixed 100 ms, so
every isolated edit paid the full window. Against a 0.7 ms measurement
floor, a p50 of 100.7 ms was almost all waiting.

```rust
pub fn settle(&mut self, maximum: Duration, quiet: Duration)  // src/session/mod.rs:227
```

The method samples how much change each endpoint recorded, sleeps a short
slice, then samples again. Growth means writes continue. Two equal samples
mean the burst ended. Both callers pass 100 ms and 20 ms. An isolated edit
now waits 20 ms. A long burst still stops at 100 ms, so the worst case does
not move.

The method samples counts because the watcher signal is standing state, not
a stream of edges. The wake channel holds one token, so it cannot count
arrivals. The token can also be consumed already. The path count is the only
value that grows for each event, and no scan consumes it during a settle.

A remote endpoint reports `None`. Two `None` values compare equal, so the
settle ends after one slice. This is deliberate. An endpoint that cannot
show a burst shortens the wait. Cycles are idempotent, so an early cycle
costs work, never correctness.

## Reconcile and transitions

Reconciliation compares the ancestor, alpha, and beta
(`src/tree/reconcile.rs:479`). The rules follow mutagen's semantics.

If one side is untracked and the ancestor is present, the code keeps both
sides and the ancestor (`src/tree/reconcile.rs:93`). A file that crossed a
size limit never reads as a deletion, in any mode.

In safe two-way mode a conflict is a report. The code applies no transition
to either side. Resolution is manual, and the mechanism is pleasing: one
side with only deletions loses to the other side
(`src/tree/reconcile.rs:288`). To resolve a conflict, delete the side you do
not want.

The transition code refuses and reports instead of guessing
(`src/endpoint/local.rs:1261`). It validates against the last scan, and the
cycle guarantees that scan produced these transitions. It checks every path
component with `symlink_metadata`, so a symlink on the way is a refusal.
Before it writes a file, it needs the expected digest and identical
metadata (`src/endpoint/local.rs:1377`). This check stands between a stale
transition and somebody else's data.

New content becomes visible by rename, so a transition is atomic. If a
digest has no further use in the batch, the staged file is renamed onto the
target (`src/endpoint/local.rs:1598`). This halves the write volume of a
cold transfer. Removal works from the bottom up and must account for every
entry on disk, because content that reconciliation never saw is content
nobody decided to delete.

Default modes are conservative: 0700 for directories and 0600 for files
(`src/endpoint/local.rs:49`). Synchronized trees often hold credentials.

## Transfer and staging

Transfer is classic rsync, streamed (`src/rsync/mod.rs`). Memory stays
bounded whatever the file size. The patch code validates block ranges
before any read, so a hostile delta cannot read out of range
(`src/rsync/mod.rs:362`).

Three places decide whether to negotiate a delta or send the bytes. All
three ask whether a usable base exists, and all three answer from the last
scan (`src/endpoint/local.rs:943`, `:603`, `:730`). On a cold destination,
autobahn does not probe the filesystem and does not open the target.

Hashing is BLAKE3 everywhere. Compression is LZ4 for each frame, with a
256-byte floor (`src/transport/mod.rs:590`). SSH compression is off
(`src/transport/mod.rs:71`), because the stream is compressed already.

Staging is content-addressed. Verified content lands at
`<staging_root>/<digest-hex>` (`src/endpoint/local.rs:2029`). Idempotence,
resumability, and deduplication follow from that one choice.

The staging directory appears at first use, never at construction. An
inside-root placement would otherwise create a missing root as an empty
directory, and the safety checks would read that as an emptied root
(`src/endpoint/local.rs:296`).

## Persistence

The scan cache is written by a background thread (`src/persist.rs:53`). The
writer parks a closure, not bytes, so a superseded state is never encoded
(`src/persist.rs:26`). Only the newest queued state survives. Write failures
are ignored.

That is safe because of what a scan cache is: a record of work already done.
It holds nothing that a filesystem read cannot recover. A lost cache costs
one full scan (`src/persist.rs:8`).

**The ancestor is different, and the code writes it synchronously**
(`src/session/mod.rs:419`). The ancestor carries provenance. It is what
separates "this side changed" from "the other side changed".

Here is the failure it prevents. Alpha holds v2, beta holds v2, and the
ancestor says v2. A user reverts alpha to v1. If the ancestor write is lost,
the ancestor still says v1. Alpha then differs from it in nothing, and beta
differs from it in one file. The ordinary rule propagates beta's v2 over the
deliberate revert. No conflict appears, because only one side looks
modified. That is silent data loss, in every mode.

A corrupt ancestor is an error at load, not a reset
(`src/session/mod.rs:683`). A silent reset resurrects deletions.

## The protocol

The wire format is bincode frames behind a version handshake. `Initialize`
carries the whole policy, so both endpoints run identical rules
(`src/protocol.rs:37`). Ownership travels as names, and each host resolves
against its own database (`src/ownership.rs:1`).

One SSH process carries many sessions as channels
(`src/transport/mux.rs:517`). Each channel has its own thread, so a channel
that waits for changes never blocks a scan on another channel.

### ScanUnchanged

The agent channel keeps `last_sent`, the tree **this channel sent**, not the
endpoint's newest tree (`src/transport/mod.rs:400`). The difference matters,
because a transition folds results into the endpoint's tree and leaves the
controller behind.

If the sent root shares storage with the current root, the reply is one enum
tag. The anchor moves only after a successful send
(`src/transport/mod.rs:469`). A failed send leaves the controller with its
old model, and a recorded anchor would then report "unchanged" for a tree
that never arrived.

After a successful transition, the anchor moves to the folded tree
(`src/transport/mod.rs:441`). The controller folds its own model the same
way, so the two agree. This is what lets the next scan of an untouched
destination answer "unchanged".

## Limits

| Limit | Value | Where |
|---|---|---|
| Frame size | 64 MiB | `src/protocol.rs:23` |
| Pending watch paths | 8192 | `src/endpoint/local.rs:161` |
| Full scan interval | 120 s | `src/endpoint/local.rs:168` |
| Supply batch | 8 MiB / 16384 frames | `src/endpoint/local.rs:66` |
| Backoff cap | 300 s | `src/supervisor/mod.rs:37` |

`max_file_size` and `max_entry_count` are configurable. An entry count above
the limit fails the scan and the cycle (`src/endpoint/local.rs:847`). The
limit guards against synchronization of the wrong tree, such as a home
directory.

### The frame cap

The 64 MiB check applies to the uncompressed length
(`src/transport/mod.rs:604`), so compression gives no headroom. No code
splits a `Scan` reply, a `Transition` request, or a `StageBegin` request.

A file entry encodes to approximately 89 bytes. The ceiling is thus near
**750,000 entries**, and lower with long names. Above that, the first cold
scan of a remote root fails, and the cycle fails with it. A local session is
not affected, because it serializes nothing.

**This ceiling is measured, not specified.** The constant describes itself
as a defense against corrupt length prefixes. That is true for file content,
which streams in bounded batches. It does not cover snapshot and transition
frames, which grow with the entry count. There is no pre-flight check and no
chunking.

### Platform

Unix only. The code uses `std::os::unix` and `libc` without cfg gating. The
one `target_os` conditional is peer-credential retrieval
(`src/supervisor/control.rs:211`), and it already has a `getpeereid` branch.
So macOS works, and the BSDs probably need little more than a build target.

### One sharp edge

An ignore negation cannot recover content below an ignored **directory**,
because a scan never descends into one (`src/scan/ignore.rs:9`). Write
`node_modules/*` with `!node_modules/keep` instead of `node_modules` with a
negation.
