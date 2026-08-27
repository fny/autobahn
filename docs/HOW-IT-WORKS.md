# How autobahn works

This document explains the design of autobahn: the problem it solves, the
decisions that shape it, and what those decisions cost. Code references
support the argument. They are not the structure of it.

## The problem

Keep two directory trees identical across a network. Do it continuously,
while people edit both sides. Never lose an edit.

That statement hides four hard constraints, and every important decision in
autobahn answers one of them.

**You cannot know what changed without looking, and looking is expensive.**
A tree of 500,000 files takes seconds to walk and longer to hash. A design
that walks the tree on every cycle cannot run every second.

**Filesystem events are fast but unreliable.** The kernel drops them when
its queue overflows. A watch can be evicted. A tool that trusts events
alone will silently miss a change and stay wrong forever.

**Two sides that both changed cannot be merged by looking at them.** If
alpha holds A and beta holds B, the current state cannot say who edited and
who is stale. You need a third thing: what both sides agreed on last. That
is the ancestor, and it is what separates a propagation from a conflict.

**Being wrong is worse than being slow.** A sync tool that overwrites a
deliberate edit has destroyed work that may exist nowhere else.

## The central idea: make "nothing changed" free

Most of the time, most of a tree has not changed. A design that spends
effort proportional to the tree size will spend nearly all of it confirming
that nothing happened.

Autobahn makes that case cost almost nothing, and the mechanism is one
decision: **the tree is immutable and shared.**

A scanned tree is a `Node` whose children sit behind an `Arc`
(`src/tree/mod.rs:53`). When a scan finds a subtree unchanged, it does not
rebuild it. It clones one pointer. The new tree and the old tree are then
the *same memory* for that subtree.

That turns an expensive question into a cheap one. "Did anything change
here?" stops being a walk and becomes a pointer comparison
(`nodes_share_storage`, `src/tree/mod.rs:378`).

The payoff is not one optimization. It is the same optimization appearing at
four different layers:

| Question | Answer | Where |
|---|---|---|
| Is this whole session idle? | Compare two pointers | `src/session/mod.rs:284` |
| Which subtrees differ? | Skip any that share storage | `src/tree/diff.rs:32` |
| Must the scan cache be rewritten? | Not if it already describes this tree | `src/endpoint/local.rs:863` |
| Must the agent send a tree over the wire? | No — send one byte | `src/transport/mod.rs:420` |

The last row matters most in practice. An idle remote endpoint answers a
scan request with a single enum tag. No serialization, no transfer, no
decode. A heartbeat over a half-million-file tree costs one round trip.

This idea has a sharp edge, and the code states it as a contract
(`src/tree/mod.rs:366`). Shared storage tells you how a tree was *built*,
not what it *holds*. A tree read from disk shares nothing with an identical
tree in memory. So two equal trees can answer `false`. **Callers can use the
answer to prove agreement. They must never use it to prove difference.**
Every use above is safe in that direction only.

## Decision 2: trust the watcher for speed, never for correctness

Autobahn watches the filesystem, and it uses those events to scan only what
changed. A dirty-path trie records which directories to re-list
(`src/scan/mod.rs:48`). An unmarked subtree is adopted whole, with no
`readdir`, no `stat`, and no `open`.

That makes a scan cost the size of the change instead of the size of the
tree. It also makes the scan exactly as trustworthy as the event stream,
which is not trustworthy enough.

So the design bounds the damage rather than assuming the events are
complete:

- If the kernel queue overflows, or the record grows past 8192 paths, the
  watcher discards its paths and demands a full scan
  (`src/endpoint/local.rs:184`).
- A full scan runs at least every 120 seconds regardless
  (`src/endpoint/local.rs:168`). This is the ceiling on how long a missed
  event can persist.
- A transition problem clears the record (`src/endpoint/local.rs:1102`).
  The filesystem disagreed with the tree, so the tree is proven stale.
- A root that cannot be watched at all still works. It falls back to the
  interval.

The rule this produces is worth stating plainly: **watch failures cost
latency, never correctness.** The worst outcome is a slower cycle, never a
wrong one.

The tests assert the contract in both directions. An incremental scan must
agree exactly with a full scan (`src/scan/mod.rs:783`), and with nothing
marked, a change made behind the scan must stay invisible
(`src/scan/mod.rs:888`). The second test looks strange until you realize it
is the caller's obligation written down.

## Decision 3: the ancestor is sacred, everything else is disposable

Autobahn writes two kinds of state, and treats them oppositely.

**The scan cache is derived.** It records work already done. Losing it costs
one full scan and nothing else. So it is written by a background thread that
is allowed to fail silently, drop superseded states, and never block a cycle
(`src/persist.rs:8`).

**The ancestor is provenance.** It is the only thing that distinguishes
"this side changed" from "the other side changed". A stale ancestor is not
merely out of date. It is actively misleading.

Here is the failure that justifies the cost. Alpha holds v2, beta holds v2,
and the ancestor says v2. A user reverts alpha to v1. If that ancestor write
is lost, the ancestor still says v1. Alpha now looks unchanged against it.
Beta looks modified. The ordinary three-way rule propagates beta's v2 over
the deliberate revert, and raises no conflict, because only one side appears
to have changed.

That is silent data loss, in every mode. So the ancestor is written
synchronously and a failure fails the cycle (`src/session/mod.rs:419`). It
is validated before it is written, and a corrupt ancestor is an error at
load rather than a reset (`src/session/mod.rs:683`) — because a silent reset
resurrects deletions.

One write sits on the critical path. It is this one, and this is why.

## Decision 4: one rendering of what happened

After a cycle applies changes, four separate parties need to know what
actually landed on disk: the ancestor, the local endpoint's tree, the
controller's model of the remote agent, and the agent's own record of what
it last sent.

If those four derive that answer independently, they will eventually
disagree, and a disagreement here means data loss.

So they do not. A transition returns one result for each request, in order,
describing what is on disk after the attempt — the new content on success,
the surviving old content on refusal, a partial tree where a directory was
only partly created (`src/endpoint/mod.rs:79`). All four parties consume
that single rendering (`src/endpoint/mod.rs:98`).

This also buys speed. The results carry the metadata of files as they were
created, so the next scan re-digests only what changed *after* the
transition (`src/endpoint/local.rs:1106`). On a cold sync that is the
difference between a metadata sweep and rehashing everything you just
wrote.

## Decision 5: refuse rather than guess

Reconciliation decides what should happen. Between that decision and the
write, the filesystem can change underneath. The transition code treats
that gap as hostile.

Every write validates against **the exact scan the transitions were
reconciled from** (`src/endpoint/local.rs:1056`). "Matches the last scan" is
therefore the same statement as "nothing has changed since we decided this
was safe". A file must have the expected digest and byte-identical
metadata before it is replaced (`src/endpoint/local.rs:1377`).

The supporting rules follow the same instinct. Every path component is
checked with `symlink_metadata`, so a symlink anywhere on the way is a
refusal rather than a redirection (`src/endpoint/local.rs:1337`). Removal
works bottom-up and must account for every entry on disk, because content
reconciliation never saw is content nobody decided to delete. New content
becomes visible only by rename, so a reader never sees a half-written file.
A refusal at one path never aborts the others.

Two safety halts sit above all of this. If the ancestor had children and one
side now presents an empty root, the cycle stops
(`src/session/mod.rs:332`). An unmounted volume is far more likely than a
deliberate deletion of everything.

## What the design looks like from outside

The decisions above produce the shape.

**One binary, two roles.** The same executable runs as the controller or,
with `autobahn agent`, as the remote half (`src/main.rs:206`). Both ends are
the same build, so the version handshake demands an exact match. This is why
the agent bundle must be updated together with the CLI.

**The controller is a hub.** Endpoints never talk to each other
(`src/endpoint/mod.rs:5`). Even a session between two remote roots routes
through the controller. That costs a network hop in a rare case, and buys
one place where reconciliation happens, with one model of both sides.

**A cycle is the unit of work** (`src/session/mod.rs:260`). Scan both sides
in parallel, return early if nothing moved, reconcile, stage, apply, fold
the results, write the ancestor. Everything above is a property of one of
those steps.

**Sessions are independent.** The supervisor runs a thread for each
(`src/supervisor/mod.rs:185`). A failure backs off with jitter applied after
the cap, so sessions that all fail do not synchronize their retries
(`src/supervisor/mod.rs:693`). One SSH process carries many sessions as
channels, so a channel waiting for changes never blocks another channel's
scan.

### Why the cycle can return early, safely

The early return is the most valuable path in the system, so its soundness
deserves stating. It fires only if the session is quiesced *and* both fresh
roots share storage with the recorded ones.

Two independent facts make it safe. The quiesced flag is set only after a
cycle that left nothing outstanding — no transitions, no conflicts, no
problems (`src/session/mod.rs:74`). So "the same as last time" means "still
synchronized", not "unchanged since a cycle that still had work to do". And
only a scan that adopted its baseline whole can produce pointer identity, so
the gate cannot open for a tree that was actually re-read.

### The settle, as an illustration

The design has one more habit worth showing, because it was recently wrong.

When a change arrives, cycling immediately would fragment a burst of writes
across many cycles. So autobahn waits. Until 0.3.0 that wait was a fixed
100 ms, which meant every isolated edit paid the full window whether or not
a burst followed. Measured against a 0.7 ms floor, a median latency of
100.7 ms was almost entirely waiting.

The fix keeps the ceiling and removes the floor. The session samples how
much change each endpoint has recorded, sleeps a short slice, then samples
again (`src/session/mod.rs:227`). Growth means writes continue. Two equal
samples mean the burst ended. An isolated edit now waits 20 ms. A sustained
burst still stops at 100 ms, so the worst case does not move.

It samples counts rather than waiting for another event, because the
watcher's signal is standing state rather than a stream of edges. The wake
channel holds one token, so it cannot count arrivals, and the token may
already be consumed. The path count is the only value that grows for each
event.

## What it costs

Every decision above has a price, and these are the visible ones.

**A hard ceiling near 750,000 files for a remote root.** The wire format
caps a frame at 64 MiB (`src/protocol.rs:23`), the check is on the
uncompressed length, and nothing chunks a scan reply or a transition
request. A file entry encodes to approximately 89 bytes, so the first cold
scan of a larger tree fails and takes the cycle with it. Local sessions are
unaffected, because they serialize nothing.

This ceiling is **measured, not specified.** The constant describes itself
as a defense against corrupt length prefixes, which is true for file
content — that streams in bounded batches. It does not acknowledge that
snapshot frames grow with the entry count. There is no pre-flight check and
no chunking, so the failure at scale is abrupt rather than graceful.

**Up to 120 seconds of latency in the worst case.** If the watcher misses an
event and the path is never touched again, the periodic full scan is what
finds it. That is the price of not trusting events.

**Unix only.** The code uses `std::os::unix` and `libc` without gating. The
single platform conditional is peer-credential retrieval
(`src/supervisor/control.rs:211`), which already has a `getpeereid` branch,
so macOS works and the BSDs likely need little beyond a build target.
Windows would be a port, not a build target.

**One sharp edge in ignore rules.** A negation cannot recover content below
an ignored *directory*, because a scan never descends into one
(`src/scan/ignore.rs:9`). Write `node_modules/*` with `!node_modules/keep`
rather than `node_modules` with a negation.

## Where to start reading

| To understand | Read |
|---|---|
| The tree and its sharing | `src/tree/mod.rs` |
| How a scan reuses work | `src/scan/mod.rs:293` |
| The cycle | `src/session/mod.rs:260` |
| Why writes are safe | `src/endpoint/local.rs:1261` |
| What crosses the wire | `src/protocol.rs`, `src/transport/mod.rs:400` |
