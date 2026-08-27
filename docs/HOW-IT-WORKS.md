# How autobahn works

An implementation tour of autobahn 0.3.0, for people who intend to change
it. Every structural claim cites `file:line`. Where a design choice has a
reason the code makes discoverable, the reason is given — those are
usually the parts that look arbitrary until you know what they prevent.

**Contents**

1. [Shape of the system](#1-shape-of-the-system)
2. [The tree](#2-the-tree)
3. [Scanning](#3-scanning)
4. [The cycle](#4-the-cycle)
5. [Watching and the settle](#5-watching-and-the-settle)
6. [Reconciliation and transitions](#6-reconciliation-and-transitions)
7. [Transfer](#7-transfer)
8. [Staging](#8-staging)
9. [Persistence](#9-persistence)
10. [The protocol](#10-the-protocol)
11. [Limits](#11-limits)

---

## 1. Shape of the system

There is one binary. `autobahn agent` (`src/main.rs:206`) turns it into
the remote half; everything else runs as the controller. The agent is
normally spawned as `ssh host '~/.autobahn/bin/autobahn-<version> agent'`
(`src/transport/install.rs:29`, `src/transport/mod.rs:161`), and because
both ends are the same build, the version handshake demands an exact match
(`src/transport/mod.rs:560`).

The controller is a hub. Endpoints never talk to each other
(`src/endpoint/mod.rs:5`, `src/session/mod.rs:6`) — even a session with
both roots remote routes every byte through the controller. That costs a
network hop in the rare remote-to-remote case and buys one place where
reconciliation happens, with one model of both sides.

| Command | Handler |
|---|---|
| `sync ALPHA BETA` | `run_sync` `src/main.rs:342` — one-shot, or a watch loop |
| `up` | `run_up` `src/main.rs:502` — config-driven supervisor |
| `status` | `run_status` `src/main.rs:552` — reads status JSON, no daemon required |
| `flush` / `pause` / `resume` / `reset` | `run_control` `src/main.rs:473` — over a Unix socket |
| `agent` | `serve_agent` `src/transport/mod.rs:266` |

A root counts as remote when it has a colon before any slash
(`src/main.rs:300`), so `/local/path` stays local and so does
`relative/path:with-colon`.

One asymmetry is deliberate: a missing **beta** root is legal, because a
transition will create it, but a missing **alpha** is an error
(`src/main.rs:425`, `src/supervisor/mod.rs:585`). A typo in a source path
combined with a mirroring mode would otherwise empty the destination.

### From configuration to running work

`Config::plans()` (`src/config.rs:276`) fans each group — one alpha, N
betas — into one `SessionPlan` per beta. Each plan carries fully resolved
policy: mode, ignores, interval, symlink handling, permissions, limits,
staging placement, ownership, and a stable identifier.

Session identity is BLAKE3 over the two *resolved* endpoint identities
(`src/session/mod.rs:128`). Resolution canonicalizes an existing path,
and for a root that does not exist yet, canonicalizes the deepest existing
ancestor (`src/paths.rs:63`). So `/real/new` and `/alias/new` produce one
identity, and a supervisor session collides with a manual `sync` over the
same roots on the same lock instead of racing it. Duplicate identity pairs
are a configuration error rather than a runtime surprise
(`src/config.rs:527`), and all configuration errors are reported together
(`src/config.rs:539`).

`Supervisor::run_watch` (`src/supervisor/mod.rs:185`) takes a
supervisor-wide lock, binds the control socket (a failure here is not
fatal), and spawns one thread per session with a startup stagger. Each
thread loops: honor the pause/reset flags, attempt a cycle, record status,
then either back off or wait for activity.

Backoff doubles per consecutive failure to a 300 s cap, then adds 0–24%
jitter derived from the session identifier — applied *after* the cap, so
sessions that have all saturated stay spread out
(`src/supervisor/mod.rs:693`). A failed attempt drops the `Session` after
status is recorded (`src/supervisor/mod.rs:315`), which releases the state
lock and reaps the agent; the next attempt reconnects. That is the whole
healing story.

`status` reads one atomically written JSON file per session
(`src/supervisor/mod.rs:767`). It is the only channel between `up` and
`status`, which is why `status` needs no daemon and cannot hang.

---

## 2. The tree

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

`Digest` is a BLAKE3 `[u8; 32]` (`src/tree/mod.rs:29`). `FileMetadata`
holds mtime seconds and nanos, size, inode, and mode
(`src/tree/mod.rs:38`).

Three properties of this layout do most of the work.

**Children are a sorted `Vec`, not a map.** Name-sorted and name-unique,
enforced on construction (`src/tree/mod.rs:176`) and by `validate`
(`src/tree/mod.rs:314`); lookup is binary search
(`src/tree/mod.rs:194`). Sorting is what makes reconciliation, diffing,
and executability propagation *linear merges* over two sorted lists rather
than hash lookups per entry.

**Digests are inline on the file node**, alongside the scan metadata that
justifies reusing them (`src/lib.rs:11`). There is no separate path-keyed
digest cache to keep coherent, so the decision "can I reuse this digest"
rides the same traversal as "has this subtree changed".

**Children hang off an `Arc`,** so snapshot generations share storage.
A `Node` measures 96 bytes inline (24 for the name header, 72 for
`Content`), plus the name's bytes on the heap, plus one allocation per
directory. A million files is roughly 96 MB of nodes — and successive
snapshots of a mostly-unchanged tree share nearly all of it rather than
duplicating it.

Copy-on-write is enforced by the type system, not by convention:
mutation goes through `Arc::make_mut` (`src/tree/apply.rs:39`), which
clones a child vector only when it is actually shared. The stale-aliasing
bugs that hand-rolled CoW bookkeeping invites are structurally impossible
here (`src/tree/mod.rs:5`, `src/tree/apply.rs:8`).

Content equality (`src/tree/mod.rs:205`) deliberately ignores **names**
(they are positional) and **scan metadata** (an observation time, not
content). Deep comparison short-circuits on `Arc::ptr_eq`.

### `nodes_share_storage`

```rust
pub fn nodes_share_storage(a: Option<&Node>, b: Option<&Node>) -> bool  // src/tree/mod.rs:378
```

Pointer equality of two directories' child `Arc`s. Its contract
(`src/tree/mod.rs:366`) is worth internalizing before using it: sharing is
an artifact of how a hierarchy was *produced*, not of what it contains. A
tree decoded from disk or off the wire shares nothing with an identical
tree built by scanning, so equal content can and does answer `false`.

**Callers may use it to prove agreement, never to prove difference.** It
is a one-directional proof, and four things depend on it:

| Use | Where | Effect |
|---|---|---|
| Cycle short-circuit | `src/session/mod.rs:284` | Skip reconciling an untouched pair in O(1) |
| Diff pruning | `src/tree/diff.rs:32` | Unchanged subtrees compare in constant time |
| Scan-cache write elision | `src/endpoint/local.rs:863` | Don't rewrite a cache that already describes this tree |
| `ScanUnchanged` on the wire | `src/transport/mod.rs:420` | Answer a rescan with one byte |

Two content classes matter later. `Untracked` is content present on disk
but deliberately outside synchronization — ignored, oversized, a socket or
device. `Problematic` is content that could not be scanned. Neither is
`synchronizable()` (`src/tree/mod.rs:165`), and neither may appear in the
ancestor (`src/tree/mod.rs:346`).

---

## 3. Scanning

`scan(root, baseline, ignores, behavior, symlink_mode, max_file_size, dirty)`
(`src/scan/mod.rs:144`). A missing root is a legitimate state, not an
error; a root that exists but is not a directory is an error.

The baseline — the previous snapshot — accelerates two independent things:

- **Digest reuse.** `reusable_digest` (`src/scan/mod.rs:650`) returns a
  recorded digest only if mtime seconds *and* nanos, size, inode, and the
  file-type bits all match.
- **Structural sharing.** `adoptable` (`src/scan/mod.rs:676`) is stricter
  than content equality: directories must be pointer-equal and files must
  carry *identical* metadata, so adoption never discards a fresher
  observation. Adoption is bottom-up, so an unchanged subtree collapses to
  a single `Arc` clone at its top.

Per-entry work is in `scan_entry` (`src/scan/mod.rs:464`): stat without
following links, consult ignores **before descending** (which is why an
ignored subtree costs nothing), then dispatch. A file over `max_file_size`
becomes `Untracked` *without being opened* (`src/scan/mod.rs:528`) — it is
present, and can never be mistaken for a deletion. Digesting streams
through one 64 KB buffer per scan. A file whose size changes between stat
and read is re-stat'd so the recorded metadata matches the digest it
actually produced (`src/scan/mod.rs:544`).

Filesystem behavior is **probed, not assumed** (`src/scan/probes.rs`):
executability preservation (tested in both directions, so a `mode=0777`
FAT mount does not pass), Unicode decomposition, normalization
insensitivity, case insensitivity. Probed once per endpoint, then cached.

### The scan cache

A bincode `Snapshot` at `<staging_root>.scancache`
(`src/endpoint/local.rs:357`) — the whole hierarchy, digests and metadata
included. It is read only on a cold start, and any error means "no cache".
Because its digests flow (metadata-gated) into the first scan and
everything downstream, it is **trusted state, exactly like the ancestor it
lives beside** — protected by the state directory, not by re-verification
(`src/endpoint/local.rs:361`).

### The incremental path

This is what makes a scan cost the size of the *change* rather than the
size of the tree.

`DirtyPaths` (`src/scan/mod.rs:48`) is a trie over path components:

```rust
struct DirtyNode { relist: bool, children: HashMap<String, DirtyNode> }
```

`mark(path)` (`src/scan/mod.rs:69`) sets `relist` on the **parent** and
inserts a node for the leaf. Marking the parent is essential: creation and
removal are only observable by listing a directory.

`scan_directory` (`src/scan/mod.rs:293`) then has three regimes:

1. **Unmarked, with a baseline directory** — adopt the baseline's content
   wholesale. One `Arc` clone. No `readdir`, no `stat`, no `open`.
2. **Marked but `relist == false`** — walk the *baseline's* children;
   clone the unmarked ones, rescan the marked ones. Disabled on a
   decomposing volume, where on-disk names are NFD while the hierarchy
   carries NFC, so a disk path cannot be reconstructed from a recorded
   name (`src/scan/mod.rs:313`).
3. **`relist == true`** — full `readdir`. Even here, an entry that is
   unmarked, UTF-8, and non-problematic in the baseline is adopted
   **without a stat**: the listing proved it exists and the marks prove it
   did not change (`src/scan/mod.rs:400`). Problematic content is always
   retried, since a problem can resolve with no event to announce it.

Statistics cannot be counted during a partial walk, so they are recomputed
by `recount` (`src/scan/mod.rs:208`) — a pointer walk over a mostly-shared
tree, touching no filesystem.

The contract is asserted both ways by test: an incremental scan must agree
exactly with a full scan from the same baseline across modification,
creation, removal, new subtree, recursive delete, and file-to-directory
replacement (`src/scan/mod.rs:783`); and with nothing marked, a change
made behind the scan's back is invisible (`src/scan/mod.rs:888`). The
second test is the caller's obligation made explicit.

### What forces a full scan

`LocalEndpoint::dirty_paths` (`src/endpoint/local.rs:465`) refuses the
incremental path when there is no watcher, no baseline, the last full scan
is missing or older than `FULL_SCAN_INTERVAL` (120 s), the watcher's
record is `incomplete`, or any reported path is not
strippable-to-root-relative UTF-8.

`incomplete` is set by a kernel queue overflow, by exceeding
`MAXIMUM_PENDING_PATHS` (8192), or by a watcher error
(`src/endpoint/local.rs:226`). Two further rules matter:

- Creating the watcher sets `last_full_scan = None`
  (`src/endpoint/local.rs:805`) — a fresh watch knows nothing about what
  happened before it existed.
- **Any transition problem** sets `last_full_scan = None`
  (`src/endpoint/local.rs:1102`): a problem means the filesystem disagreed
  with the snapshot, so the record is proven stale.

The dirty record is consumed *before* the walk
(`src/endpoint/local.rs:471`), so a change arriving mid-scan stays pending
for the next one — at worst repeating work, never dropping a notification.

### Digest-index gating

Before building a digest→path index to satisfy staging requests locally,
`local_reuse_is_worthwhile` (`src/endpoint/local.rs:410`) asks whether it
is worth it:

```rust
const VISITS_PER_SAVED_TRANSFER: u64 = 1_000;
candidates.saturating_mul(1_000) >= indexed_files
```

Two ideas. Only requests for paths that hold no file *today* can
plausibly be served from elsewhere — a modification asks for content that
is new by definition. And the index costs a visit and a path allocation
per file in the root, so one renamed file must not index half a million
entries, while a bulk copy or a cold start still should.

---

## 4. The cycle

`Session::run_cycle` (`src/session/mod.rs:260`):

1. **Scan both endpoints in parallel** — alpha on a scoped thread, beta on
   the caller's.
2. **Quiesced short-circuit** (below).
3. **Collect scan problems.**
4. **Executability graft** (`src/session/mod.rs:309`). On a filesystem
   that cannot preserve the executable bit, the scanned bits are noise;
   `propagate_executability` (`src/tree/executability.rs:38`) replaces
   them from the first reference that can vouch — the **peer**, if it
   preserves bits and holds the same digest at the same path, then the
   **ancestor**, else stripped. Consulting the peer goes beyond Mutagen,
   which checks only the ancestor and so reports a false conflict for a
   brand-new executable (`src/tree/executability.rs:15`).
5. **Emptied-root safety halt** (`src/session/mod.rs:332`). The ancestor
   was a directory with two or more children and one side now presents an
   empty or absent root: far more likely an unmounted volume than intent.
6. **Reconcile.**
7. **Root-deletion safety halt.**
8. **Stage and transition**, beta first, then alpha. `stage()`
   (`src/session/mod.rs:506`) runs pull and push on separate threads with
   a `sync_channel(1)` between them, so source reads overlap destination
   writes; a remote destination additionally keeps a window of pushes in
   flight.
9. **Fold achieved changes into the ancestor.**
10. **Validate and persist the ancestor synchronously** (§9).
11. **Record quiescence.**

### The quiesced short-circuit, and why it is sound

The session keeps `quiesced: bool` plus the settled roots
(`src/session/mod.rs:97`). The gate (`src/session/mod.rs:284`) fires only
when `quiesced` holds **and** both fresh scan roots share storage with the
recorded ones — the same storage, not merely equal content.

Two independent facts make it safe:

- **`quiesced` is armed only after a cycle that left nothing outstanding.**
  `CycleReport::settled()` (`src/session/mod.rs:74`) requires no
  transitions applied, no conflicts, no missing staged files, and no scan
  or transition problems on either side. So "the same as last time" means
  "still synchronized", not merely "unchanged since a cycle that still had
  work to do". A cycle that reported a scan problem does not arm it, so
  the problem is reported again rather than skipped into silence.
- **Pointer identity can only be produced by a scan that adopted its
  baseline wholesale**, which is to say a scan that observed no change.
  Equal-but-freshly-allocated content answers `false`, so the gate can
  never fire on a tree that was actually re-read.

An empty report therefore means "found nothing to do", whether or not the
looking was performed (`src/session/mod.rs:256`).

### Folding achieved changes

`TransitionOutcome.results` (`src/endpoint/mod.rs:79`) is one
`Option<Node>` per transition, in request order, describing what is
actually on disk after the attempt: the target content on success, the
surviving old content on refusal, a partial hierarchy where a directory
was only partly created.

`achieved_changes` (`src/endpoint/mod.rs:100`) renders those as changes,
and `fold_transition` (`src/endpoint/mod.rs:122`) grafts them onto a
snapshot. **Four** parties consume that one rendering, which is precisely
why they cannot disagree about what a cycle accomplished
(`src/endpoint/mod.rs:98`): the session's ancestor
(`src/session/mod.rs:392`), the local endpoint's retained snapshot and
scan cache (`src/endpoint/local.rs:1114`), the controller's model of a
remote agent (`src/endpoint/remote.rs:253`), and the agent's `last_sent`
anchor (`src/transport/mod.rs:441`).

The payoff on the local side (`src/endpoint/local.rs:1106`): results carry
the metadata of entries *as created*, so the next scan re-digests only
what changed **after** the transition. On a cold sync that is the
difference between a metadata sweep and rehashing the entire tree.

---

## 5. Watching and the settle

`ChangeWatcher` (`src/endpoint/local.rs:196`) wraps
`notify::RecommendedWatcher` in recursive mode and holds two things: the
accumulated paths, and a **`sync_channel(1)`** wake token written with
`try_send`.

```rust
struct PendingChanges { paths: Vec<PathBuf>, incomplete: bool }  // src/endpoint/local.rs:170
```

`give_up()` (`src/endpoint/local.rs:184`) sets `incomplete` and *releases*
the paths, since a full scan supersedes them.

`await_change` (`src/endpoint/local.rs:1021`) creates the watcher lazily,
returns immediately if changes are already pending (a wake token may
already have been consumed), and otherwise blocks on the channel. A root
that cannot be watched degrades to waiting out the timeout — watching
failures cost latency, never correctness. `Session::await_change`
(`src/session/mod.rs:190`) slices the wait between the two endpoints in
250 ms slices so either side is noticed promptly without cross-thread
cancellation machinery.

### The settle

When a change is signalled, cycling *immediately* would fragment a burst
of writes across many cycles. The obvious fix — sleep a fixed interval —
was what autobahn did until 0.3.0, and it made every isolated edit pay the
full window. Measured against a 0.7 ms harness floor, a p50 propagation
latency of 100.7 ms was essentially all waiting.

```rust
pub fn settle(&mut self, maximum: Duration, quiet: Duration)  // src/session/mod.rs:227
```

Sample how much change each endpoint has recorded, sleep a quiet slice,
sample again. Growth means writing is still in progress and is worth
coalescing; two agreeing samples mean the burst is over. Both callers pass
`(100 ms, 20 ms)` (`src/main.rs:465`, `src/supervisor/mod.rs:352`), so an
isolated edit waits **20 ms** and a sustained burst still caps at
**100 ms** — the worst case does not move.

**Why it samples counts instead of using edge-triggered events.** The
watcher's signal is standing state, not a stream of edges:

- The wake channel is `sync_channel(1)` fed by `try_send`. Once a token
  sits in it, every later event's send fails silently, so arrivals cannot
  be counted from it — one token covers any number of events.
- The token may already have been consumed by the `await_change` that
  triggered this settle, so its presence says nothing about what has
  arrived since.
- `pending.paths.len()` is the only quantity that grows per delivered
  event, and no scan runs during a settle, so nothing consumes it.

`ChangeActivity` (`src/endpoint/mod.rs:222`) is therefore compared by
equality alone — the values carry no meaning beyond inequality. An
endpoint that cannot report activity answers `None`, which every remote
endpoint does; two `None`s compare equal and the settle ends after one
slice. That is deliberate: an endpoint that cannot evidence a burst
shortens the wait rather than lengthening it, and since cycles are
idempotent, cycling eagerly costs work, never correctness.

> A caveat for readers of the source: `ChangeActivity` is documented as
> "monotone", but `give_up()` resets the path count to zero while setting
> `incomplete`. The equality comparison still registers that transition as
> activity, so the behavior is right — the word is loose.

---

## 6. Reconciliation and transitions

`reconcile(ancestor, alpha, beta, mode)` (`src/tree/reconcile.rs:479`)
produces ancestor changes, per-side transitions, and conflicts. It is a
faithful port of Mutagen's semantics (`src/tree/reconcile.rs:1`).

Per path, in order (`src/tree/reconcile.rs:66`):

1. Either side purely **problematic** → do nothing; the scan problem
   already surfaces it.
2. Both **nil or untracked** → nil the ancestor.
3. Exactly one side **untracked with an ancestor present** → return,
   preserving both sides *and* the ancestor. This is the
   deliberately-left-alone case, such as a file that crossed a size limit:
   it never reads as a deletion in any mode, and content re-entering
   tracked scope resumes as an ordinary three-way update.
4. Alpha and beta **shallowly equal** → record a slim ancestor update if
   the ancestor disagrees, then recurse over the union of child names as a
   three-way linear merge.
5. Otherwise **disagreement**, dispatched by mode.

In **safe two-way** mode a conflict is pure reporting: no transition is
emitted for either side, and the conflict carries its root plus both
sides' change lists (`src/tree/mod.rs:129`) up to the CLI
(`src/main.rs:646`) or the status file (`src/supervisor/mod.rs:501`).
Resolution is manual and has a pleasing mechanism — because "exactly one
side purely deletions" means the other side's content wins
(`src/tree/reconcile.rs:288`), you resolve a conflict by **deleting the
side you don't want**.

### Applying transitions

`Transitioner` (`src/endpoint/local.rs:1261`) refuses and reports rather
than guessing, never follows a symlink while validating or removing, and
publishes only by renaming a fully written temporary into place.

Validation is **against the last scan** (`src/endpoint/local.rs:1056`),
and the cycle guarantees that scan is the one the transitions were
reconciled from — so "matches the last scan" is exactly "unchanged since
reconciliation decided this was safe". Each change applies independently;
a refusal at one path never aborts the rest.

- `validate_path` / `validate_name` (`src/endpoint/local.rs:2126`) reject
  empty, dot, separator, and NUL components before anything touches the
  filesystem.
- `resolve_parent` (`src/endpoint/local.rs:1337`) verifies every component
  with `symlink_metadata`, so a symlink anywhere along the way is a
  refusal, not a redirection.
- `validate_file` (`src/endpoint/local.rs:1377`) requires a real regular
  file, a scanned file node at that path, the expected digest, and
  byte-identical metadata. This is the check standing between a stale
  transition and somebody else's data.
- **Publishing** (`src/endpoint/local.rs:1598`): if this is a digest's
  last use in the batch, permissions are set on the staged file and it is
  `rename`d straight onto the target — two syscalls, no data movement,
  halving the write volume of a cold transfer. Otherwise it is copied to a
  temporary beside the target and renamed. Either way the target's
  transition is atomic.
- **Removal** (`src/endpoint/local.rs:1688`) is bottom-up and total: every
  on-disk entry must be accounted for by the expectation, because content
  reconciliation never saw is content nobody decided to delete.
- **Replacement** (`src/endpoint/local.rs:1861`): file-to-file happens in
  place, so the path never transiently ceases to exist; a digest-equal
  replacement is an executability-only change and the content is left
  entirely alone.

Defaults are conservative — `0700` directories, `0600` files
(`src/endpoint/local.rs:49`) — because synchronized trees frequently hold
credentials.

---

## 7. Transfer

Classic rsync, streamed end to end, never buffering whole files
(`src/rsync/mod.rs:1`). Block size is clamped to 1 KiB–64 KiB and chosen
as `sqrt(24 × len)` per the rsync thesis (`src/rsync/mod.rs:111`).
`deltify` (`src/rsync/mod.rs:180`) makes one forward pass with a rolling
weak checksum, confirming candidates with BLAKE3, in memory bounded by
`MAXIMUM_DATA_OPERATION_SIZE + block_size` regardless of file size.
`patch` (`src/rsync/mod.rs:343`) validates every block range against the
signature **before any I/O**, so a hostile delta cannot induce an
out-of-range read.

### Negotiate, or just send?

Three places decide, all keyed on whether a usable base exists, and all
answered from the retained snapshot where possible:

1. `stage_begin` computes a base signature **only** when the last scan
   recorded a regular file at the target path
   (`src/endpoint/local.rs:943`). On a cold destination it does not probe
   the filesystem at all.
2. On the source, `supply_from` (`src/endpoint/local.rs:603`) branches on
   whether the signature is empty. If it is, the file is read directly
   into owned operation-sized chunks — no shared scratch buffer, no size
   probe — and anything under 64 KiB, which is most files, arrives as a
   single chunk.
3. On the destination, `open_receive_file` (`src/endpoint/local.rs:730`)
   does not even open the target when the signature is empty, since no
   block operation can reference a base.

If the requested path cannot supply — vanished, unreadable, changed — any
other scanned path recording the same digest is tried
(`src/endpoint/local.rs:559`), and that index is consulted only on
failure, keeping the walk off the hot path.

**Hashing is BLAKE3 everywhere**: file digests, rsync strong hashes,
receive-side verification, session identity. `DigestingWriter`
(`src/endpoint/local.rs:1223`) verifies during the write it was already
performing, and digests only the bytes the underlying writer accepted.

**Compression is LZ4 per frame** with a flag byte, a 256-byte floor, and a
fall back to verbatim when the payload does not shrink
(`src/transport/mod.rs:590`). SSH-level compression is deliberately
**disabled** (`src/transport/mod.rs:71`): the stream is already
compressed, and recompressing with zlib costs seconds of CPU on both ends
of a large transfer for almost nothing.

---

## 8. Staging

| Mode | Location | Why |
|---|---|---|
| `State` (default) | `<state dir>/staging-<side>` | Keyed by session *and side*, so the two sides never share staging space |
| `BesideRoot` | `<root parent>/.autobahn-tmp-staging-…` | Same filesystem as the root, so publishing is a rename; storage charged to the root's volume |
| `InsideRoot` | `<root>/.autobahn-tmp-staging-…` | The only placement guaranteed to share the root's filesystem when the root is a mount point |

Both root-relative placements use the `.autobahn-tmp` prefix, which scans
ignore (`src/scan/mod.rs:32`).

**Staging is content-addressed**: verified content lands at
`<staging_root>/<digest-hex>` (`src/endpoint/local.rs:2029`). Idempotence,
resumability across interrupted cycles, and deduplication between paths
sharing content all fall out for free (`src/endpoint/local.rs:19`).

The staging directory is created on first use, never at construction — an
inside-root placement would otherwise conjure a missing root into
existence as an empty directory, which the safety checks would correctly
read as an emptied root (`src/endpoint/local.rs:296`).

`stage_begin` (`src/endpoint/local.rs:874`) inventories previously staged
content with **one `read_dir`** rather than a stat per request; on a cold
destination that replaces tens of thousands of stats against an empty
directory. Batches are sized by bytes (`SUPPLY_TARGET_BYTES`, 8 MiB) as
well as frames, because small files produce tiny frames and a
purely frame-counted batch would carry under a megabyte — hundreds of
round trips for a large tree.

---

## 9. Persistence

| Artifact | Path | Written |
|---|---|---|
| **Ancestor** | `<state dir>/ancestor` | **Synchronously**; failure fails the cycle |
| Scan cache | `<staging_root>.scancache` | Asynchronously, best-effort |
| Session status | `<state root>/status/<id>.json` | Synchronously |
| Session lock | `<state dir>/lock` | `flock`, held for the session's life |

All state writes are temp-file-plus-rename (`src/persist.rs:178`).

### The `StateWriter`

One background thread per local endpoint (`src/persist.rs:53`). `store`
parks a **closure**, not bytes, so a state superseded before its turn is
never serialized at all (`src/persist.rs:26`). Only the newest queued
state survives; a burst of cycles collapses to one write. On a large tree
that removes tens of megabytes of serialization from the latency between
saving a file and seeing it arrive (`src/endpoint/local.rs:385`).

It is robust in the ways derived state permits: a drop guard releases
waiters however the loop exits, including through a panicking encoder; the
lock tolerates poisoning, because refusing to proceed would turn a failed
write into a hang for everyone waiting; and write failures are ignored —
the writer is the one caller entitled to ignore a failure
(`src/persist.rs:104`).

What makes that safe is what a scan cache *is*: a record of work already
done, carrying no information that cannot be recovered by reading the
filesystem. Losing one costs a single full scan and nothing else
(`src/persist.rs:8`).

### Why the ancestor is not written that way

The ancestor is the same shape and size, and is deliberately **not**
deferred. It carries *provenance*: it is what distinguishes "this side
changed" from "the other side did".

The concrete failure (`src/session/mod.rs:419`): alpha holds v2, beta
holds v2, the ancestor says v2. A user reverts alpha to v1. If the
ancestor write were deferred and lost, the ancestor still says **v1** — so
alpha diffs empty against it, beta diffs non-empty, and the ordinary
three-way rule propagates beta's v2 over alpha's deliberate revert. No
conflict is raised, because from reconciliation's point of view only one
side changed. That is silent data loss, **in every mode**, and it is why
this one write sits on the critical path.

Two supporting rules: the new ancestor is `validate(true)`-checked before
being written (`src/session/mod.rs:409`), and a corrupt ancestor on load
is an error rather than a reset (`src/session/mod.rs:683`), since
silently discarding it would resurrect deletions.

### The session lock

Advisory `flock` held for the session's lifetime
(`src/session/mod.rs:606`). Two sessions over one state directory would
race each other's ancestor, staging, and status writes — destructively, if
their modes differ. Acquisition retries for up to 2 s, because a
concurrently forked child can inherit the descriptor in the microseconds
between fork and exec. The supervisor takes the lock **before**
constructing endpoints (`src/supervisor/mod.rs:573`), so a conflict is
found before spawning SSH.

---

## 10. The protocol

Bincode frames, length-prefixed, behind a version handshake that demands
an exact match — both ends are the same binary
(`src/transport/mod.rs:560`). `Initialize` (`src/protocol.rs:37`) carries
the entire policy, so both endpoints always operate under identical rules.
Ownership travels as **names, not IDs**, each host resolving against its
own passwd and group databases (`src/ownership.rs:1`).

The request set mirrors the `Endpoint` trait one-for-one: `Scan`,
`StageBegin`, `SupplyOpen`, `SupplyPull`, `StagePush`, `Transition`,
`AwaitChanges`. `MuxRequest`/`MuxResponse` (`src/protocol.rs:134`) wrap
them so one SSH process carries any number of sessions as channels, each
served on its own thread (`src/transport/mod.rs:284`) — a channel blocked
in `AwaitChanges` never stalls a sibling's scan. `AgentPool`
(`src/transport/mux.rs:517`) keys connections by spawn argv, so every
session to one host shares one authentication.

### `ScanUnchanged`

The optimization that keeps a heartbeat from costing a full snapshot
serialization, transfer, and decode on every cycle.

The agent's channel keeps `last_sent` — the snapshot **this channel
actually transmitted**, not the endpoint's latest
(`src/transport/mod.rs:400`). That distinction is load-bearing:
transitions fold their achieved results into the endpoint's snapshot,
leaving the endpoint holding a tree the controller has never seen.

On `Scan` (`src/transport/mod.rs:416`), if the sent root shares storage
with the current one and executability preservation matches, the answer is
a single enum tag. The anchor is updated **only after a successful send**
(`src/transport/mod.rs:469`) — a response that fails to encode or transmit
leaves the controller with its previous model, and recording the new one
would make the next rescan report "unchanged" against a tree that never
arrived. Any send failure clears `last_sent` entirely, because forgetting
everything costs one full resend and avoids reasoning about which failures
leave the controller's model intact.

After a successful transition, `last_sent` is re-anchored to the folded
snapshot (`src/transport/mod.rs:441`), because the controller folds its
own model identically — so what it now believes *is* this tree. That is
what lets the next scan of an otherwise untouched destination answer
"unchanged", which is the common case under one-directional editing.

On the controller, receiving `ScanUnchanged` with no cached snapshot is a
hard error rather than a silent rescan (`src/endpoint/remote.rs:183`) —
that would be a protocol defect, and papering over it would hide it.

---

## 11. Limits

| Limit | Value | Where |
|---|---|---|
| `MAXIMUM_FRAME_SIZE` | 64 MiB | `src/protocol.rs:23` |
| `MAXIMUM_DATA_OPERATION_SIZE` | 64 KiB | `src/rsync/mod.rs:19` |
| `SUPPLY_TARGET_BYTES` | 8 MiB | `src/endpoint/local.rs:66` |
| `SUPPLY_BATCH_SIZE` | 16 384 frames | `src/session/mod.rs:25` |
| `PUSH_WINDOW` | 4 batches | `src/endpoint/remote.rs:30` |
| `MAXIMUM_PENDING_PATHS` | 8192 | `src/endpoint/local.rs:161` |
| `FULL_SCAN_INTERVAL` | 120 s | `src/endpoint/local.rs:168` |
| `MAXIMUM_FOLLOW_UP_CYCLES` | 5 | `src/supervisor/mod.rs:42` |
| `MAXIMUM_BACKOFF` | 300 s | `src/supervisor/mod.rs:37` |

`max_file_size` is configurable; larger files scan as `Untracked` and are
never opened. `max_entry_count` **fails the scan and the cycle** when
exceeded (`src/endpoint/local.rs:847`) — it guards against synchronizing
the wrong tree entirely, such as a home directory or a build output
volume, so partial progress on a probable mistake is refused.

### The frame cap, and where it bites

The 64 MiB check is on the **uncompressed** length, before LZ4
(`src/transport/mod.rs:604`), so compression buys no headroom. Nothing
chunks a `Scan` response, a `Transition` request, or a `StageBegin`
request; only supply *pulls* are batched.

A file entry encodes to about **89 bytes** (8 for the name length, the
name, 4 for the variant tag, 32 for the digest, 1 for the executable flag,
32 for metadata), which puts the ceiling around **750,000 file entries**,
and lower with long names. Past that, the first cold scan response from a
remote agent fails and takes the cycle with it. Purely local sessions are
unaffected, because nothing is serialized.

> This ceiling is **measured, not specified**. The constant's own comment
> frames it as a defense against corrupt or adversarial length prefixes
> rather than a limit on transfer size, which is true of file content —
> that streams in bounded batches — but does not acknowledge that snapshot
> and transition frames are unbounded in entry count. There is no
> pre-flight check and no chunking, so the failure at scale is a hard
> cycle error rather than degraded operation.

### Platform

Unix only. `src/endpoint/local.rs`, `src/scan/`, `src/ownership.rs`, the
session lock, and the control socket use `std::os::unix` and `libc`
unconditionally, with no Windows path and no `cfg` gating. The single
`target_os` conditional in the whole source is peer-credential retrieval
(`src/supervisor/control.rs:211`), which already has its `SO_PEERCRED` and
`getpeereid` branches — so macOS works, and the BSDs would likely need
little beyond a build target.

### A documented sharp edge

Ignore-pattern negation cannot resurrect content beneath an ignored
*directory*, because scans never descend into one
(`src/scan/ignore.rs:9`). Write `node_modules/*` plus
`!node_modules/keep` rather than `node_modules` plus a negation.
