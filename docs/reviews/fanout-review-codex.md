# Fan-out design review

## Bottom line

The diagnosis is substantially right, but the proposed tiers draw one important boundary in the wrong place.

- The **ancestor and all reconciliation/quiescence state must remain per session**. That part is non-negotiable.
- A watcher can be shared, but not its current consuming `PendingChanges` queue. For Tier 1 alone, the simplest safe implementation is one physical watcher with one bounded pending accumulator and wake token per live subscriber. A cursor over a bounded journal also works, but is not required.
- Do **not** share a mutable `LocalEndpoint`. Share a root observer that publishes immutable, versioned `Snapshot`s. Each session must retain the exact snapshot lease returned by its own `scan()` until its transition has finished.
- A fixed one-second freshness window is the wrong validity rule. Reuse is valid only while the root's observation generation has not been invalidated by a watcher event, an explicit transition, watcher loss/overflow, or the 120-second full-scan deadline. A time window may coalesce requests, but must not override those conditions.
- Start with watcher sharing, but build it as the front half of an observation broker so Tier 2 does not replace Tier 1's cursor machinery. Implement scan sharing first for one-way modes, where alpha cannot be mutated by a beta. Two-way alpha writes need serialization and explicit invalidation.
- A relay/tree is not semantically equivalent and mostly moves the duplicate-scan problem to the relay. It is not the cheaper safe change.
- Tier 3's rationale is partly wrong: source supply does not digest the file. The receiver verifies the digest while staging. Tier 2 removes duplicate source-side scan digests; Tier 3 can remove repeated source reads/delta work only, and cannot remove destination verification.

## What the code actually owns

The fan-out is real: `Config::plans` emits a `SessionPlan` inside the beta loop (`src/config.rs:468-536`), the watch supervisor starts a worker thread for every plan (`src/supervisor/mod.rs:235-283`), and `connect` constructs both endpoints afresh for each plan (`src/supervisor/mod.rs:614-713`). The same is true for a remote alpha: every multiplexed channel creates its own `LocalEndpoint` on the agent (`src/transport/mod.rs:368-406`, `src/transport/mod.rs:508-545`). Pooling SSH only shares the connection, not the endpoint (`src/supervisor/mod.rs:687-704`).

`LocalEndpoint` is not one shareable unit. It combines at least four kinds of state (`src/endpoint/local.rs:110-155`):

1. observation state: filesystem behavior, watcher, full-scan time, scan cache, and scan baseline;
2. a transition-validation snapshot;
3. per-operation supply and receive streams;
4. per-session staging and transition policy.

Only the first category is a natural sharing target, and even there the exact snapshot used for validation needs a per-session lease.

## 1. Tier 0 and the non-shareable boundary

### Ancestor: yes, always private

Your Tier 0 invariant is correct. A session's ancestor is the provenance record used by three-way reconciliation (`src/session/mod.rs:344-350`), is updated from that session's reconciliation and achieved transitions (`src/session/mod.rs:394-442`), and is synchronously persisted precisely because a stale ancestor can silently overwrite a deliberate revert (`src/session/mod.rs:425-440`). Each beta can be at a different achieved state, so even two sessions that currently have equal ancestors must not turn equality into shared ownership.

I would keep this invariant even for replica mode. It may be possible to prove that parts of the ancestor are redundant for a narrow mode, but the reconciler still consumes it and uses it for agreement, untracked-content handling, and conflict records (`src/tree/reconcile.rs:107-127`, `src/tree/reconcile.rs:420-472`). A mode-specific optimization is not worth weakening the model.

### Other state that must remain per session

- `mode`, `quiesced`, `settled_alpha`, and `settled_beta`. Quiescence is a conclusion about one alpha/beta/ancestor triple, not about the root globally (`src/session/mod.rs:89-113`, `src/session/mod.rs:444-455`).
- The session lock and state directory. It protects one ancestor, staging area, and status namespace (`src/session/mod.rs:601-617`). Do not turn it into an alpha-root lock. A shared observer needs a separate process-local lock/condition variable; the existing per-session lock still has its current job.
- Supply and receive stream cursors. `supply_open` overwrites a single `SupplyState`, and `stage_begin` replaces a single `ReceiveState` (`src/endpoint/local.rs:874-965`). Sharing those fields would interleave different destinations' need lists.
- Transition policy: file/directory modes, ownership, and staging placement. These can differ between plans even when the root is the same (`src/config.rs:187-203`). They do not belong in a scan broker.
- Staging roots under the current implementation. They are deliberately namespaced by session and side (`src/endpoint/local.rs:2004-2033`). More importantly, the final use of a staged digest may rename the staged file into the tree (`src/endpoint/local.rs:1595-1627`); two sessions sharing that file would race over destructive consumption. Shared content storage is possible only after adding immutable objects plus copy/reflink/reference semantics. It is Tier 3 work, not an incidental Tier 1 change.
- A session's **transition-validation lease**. `scan()` puts the returned snapshot in `last_snapshot` (`src/endpoint/local.rs:789-871`), and `transition()` later validates against that field (`src/endpoint/local.rs:1055-1074`). A central “latest snapshot” slot cannot replace it.

### State that may be shared, with qualifications

- Probed filesystem behavior is root/volume observation, not provenance (`src/endpoint/local.rs:132-135`, `src/endpoint/local.rs:789-796`). It may live in the root observer. This merely preserves the current live-endpoint assumption that the volume does not change underneath it.
- `last_full_scan` belongs to an **observation lineage**. With Tier 1 only, subscribers have different baselines and event consumption, so it is effectively per subscriber. With Tier 2, where every subscriber adopts snapshots from one broker lineage, it belongs to the broker. The forced full scan must still happen at least every 120 seconds (`src/endpoint/local.rs:163-168`, `src/endpoint/local.rs:823-841`).
- The scan cache and its writer may be shared only with the observer and only under a cache key that includes scan-affecting policy. Current cache paths are derived from per-session staging roots (`src/endpoint/local.rs:355-393`). One broker-owned cache avoids duplicate tens-of-megabytes writes; concurrent independent `StateWriter`s should not target one common file because each writer only orders its own queue (`src/persist.rs:20-22`, `src/persist.rs:52-58`).

The observer key cannot be merely “same alpha string.” It needs the resolved host/root plus all observation-affecting policy: ignores, symlink mode, and maximum file size (`src/scan/mod.rs:144-152`). `max_entry_count` can be checked per caller against shared snapshot counts because it is a post-scan guard (`src/endpoint/local.rs:843-855`). File modes, ownership, staging mode, and synchronization mode do not affect what the scan sees.

## 2. Tier 1: watcher fan-out

Your first-reader-drains-the-queue failure is correct. `dirty_paths` calls `watcher.take()` (`src/endpoint/local.rs:465-503`), and `take` is literally `mem::take` of the only pending record (`src/endpoint/local.rs:250-258`). An unmarked subtree is adopted without touching the filesystem, and the test demonstrates that an unmarked real change is invisible (`src/scan/mod.rs:888-923`). A shared consuming queue would therefore be incorrect.

I disagree that an event-log cursor is necessarily the simplest implementation. For Tier 1 in isolation, use:

- one `notify::RecommendedWatcher` per observer key;
- one subscriber registration per live endpoint;
- a bounded `PendingChanges` and a capacity-one wake channel per subscriber;
- callback fan-out of each event into every subscriber's accumulator;
- an initial full scan for every new subscription, because a newly established cursor has no history before registration, matching the current ordering requirement (`src/endpoint/local.rs:798-810`).

This duplicates at most the path records, not kernel watches. The existing overflow behavior makes it naturally bounded: at 8192 paths the subscriber flips to `incomplete`, clears the vector, and stops appending until it is consumed (`src/endpoint/local.rs:157-188`, `src/endpoint/local.rs:224-235`). It is O(live sessions × 8192 paths) in the worst case, so a bounded global journal is more memory-efficient at very high fan-out, but it is a more complex first implementation.

If you do use a journal, make it a bounded ring with monotonically increasing sequence numbers. A subscriber cursor older than the retained floor becomes `incomplete` and must full-scan; never retain entries just because a paused/failed subscriber has not acknowledged them.

Lifecycle answers:

- **Paused:** the current worker drops the whole session (`src/supervisor/mod.rs:401-408`). Dropping its subscription removes its accumulator/cursor immediately. On resume it subscribes anew and its first scan is full. No queue remains to grow.
- **Reset:** reset also drops the session before deleting only its ancestor (`src/supervisor/mod.rs:418-439`). Treat reconnection like a new subscription. Reset does not invalidate the shared observation for other sessions.
- **Backoff after failure:** a failed attempt drops the session in `conclude` (`src/supervisor/mod.rs:345-355`). Release that subscriber. Other references may keep the physical watcher alive; the recovering session gets a new subscription.
- **Slow but live:** its accumulator reaches 8192 and becomes incomplete; memory stops growing. Its next scan is full. With a ring, falling behind the floor has the same effect.

One subtlety is wake delivery. The physical watcher's current capacity-one token is for one waiter (`src/endpoint/local.rs:203-206`, `src/endpoint/local.rs:213-247`). Every subscriber needs its own coalescing wake token, or a generation check before blocking plus a condition variable. Otherwise one session can consume the only wake just as it consumes the only dirty record.

## 3. Tier 2: shared scans, validation, and quiescence

### A fixed freshness window is not sound as the validity rule

“A scan is already recent rather than instantaneous” does not justify deliberately returning it after a known change. The current implementation consumes dirty paths before walking so events that arrive during the scan remain pending for the next scan (`src/endpoint/local.rs:823-837`). A cache that returns the same snapshot for one second despite such a pending event defeats that invariant. It can also make the quiesced pointer shortcut claim “nothing can have changed” when the observer already knows otherwise (`src/session/mod.rs:281-298`).

Use an observation generation instead:

1. Watcher event, watcher error/overflow, or explicit transition increments/invalidates the generation.
2. Concurrent scan requests for one generation are single-flighted; one walk publishes an immutable `Snapshot` and generation.
3. Requests may reuse that snapshot only if no invalidation has occurred and the periodic full-scan deadline is not due.
4. A transition invalidates **before** it starts mutating the root, not merely after it finishes. Its own watcher event may arrive late.
5. The broker serializes scan publication with transitions to that root. It may allow sessions to reconcile and supply concurrently from immutable old leases, but it must not publish a scan taken through a transition.

Under a reliable, unchanged watcher generation, a one-second TTL is actually unnecessary: returning the same snapshot is equivalent to today's incremental scan with no useful dirty marks, which adopts the baseline storage wholesale (`src/scan/mod.rs:293-307`). The 120-second full scan remains the bound for missed watcher events. A short window can still be useful to wait briefly for peer scan requests and improve coalescing, but it is scheduling policy, not evidence of validity.

### Exact transition validation can be preserved, but not by sharing `last_snapshot`

Sharing the immutable snapshot storage does not break the guarantee. Sharing a mutable endpoint slot does.

The transitioner's `scanned` tree is taken from `self.last_snapshot` (`src/endpoint/local.rs:1071-1074`), and file removal/replacement validates both expected digest and exact scanned metadata against it (`src/endpoint/local.rs:1375-1407`). Therefore each session-side proxy should retain something like:

```text
ScanLease { observer_id, generation, snapshot: Snapshot }
```

The proxy's `scan()` stores that lease privately and returns a clone. Its later `transition()` submits the transitions **with that lease**, and validation uses the lease's snapshot even if the broker has since published generation N+1. That is exactly the present scan→reconcile→transition relationship. The broker's latest snapshot must never silently replace a session's lease between those calls.

After a transition, `fold_transition` creates a new snapshot from the achieved results (`src/endpoint/mod.rs:120-143`), and the endpoint currently installs that fold as its next private baseline (`src/endpoint/local.rs:1108-1126`). With a broker:

- fold the result into the session's private lease/model so controller and endpoint still agree;
- invalidate the shared observer;
- publish the fold as the observer's new baseline only if the transition was serialized from the broker's current generation and no watcher/external invalidation intervened;
- otherwise force the next broker scan to rebuild from a safe baseline (full if completeness is uncertain).

“Invalidate rather than mutate” is therefore necessary but incomplete. It prevents future reuse, but the in-flight transition still needs its exact old lease.

There is already a widened-race cost to stale scans: refusal is safe at the filesystem boundary, but it creates transition problems and forces a full next scan (`src/endpoint/local.rs:1098-1106`). Reusing across known events would produce more refusals and follow-up cycles even if eventual safety survives. Do not spend the race budget casually.

### Quiesced short-circuit remains sound with generation-correct sharing

Yes, several sessions may share the same alpha `Arc<Vec<Node>>`. `nodes_share_storage` is explicitly only a proof of agreement, never a proof of difference (`src/tree/mod.rs:366-392`). Each session compares the new alpha root only to **its own** `settled_alpha`, and independently compares its beta to its own `settled_beta` (`src/session/mod.rs:288-296`). Cross-session pointer identity cannot satisfy the beta half or cross session boundaries.

It is sound if the broker returns the same storage only for the same still-valid observation generation. It is unsound as the comment's claimed proof if the broker intentionally returns that pointer after a known event for the sake of a freshness window. The likely symptom is delayed work rather than immediate corruption, but the shortcut would no longer establish what its comment and `CycleReport::settled` require (`src/session/mod.rs:74-86`).

### Remote alpha is a larger Tier 2 than local alpha

In-process `Snapshot::clone` is cheap because directory children are `Arc<Vec<Node>>` (`src/tree/mod.rs:52-64`, `src/tree/mod.rs:395-412`). Across an agent channel it is not free: every channel owns an endpoint and serializes a `Snapshot` response (`src/transport/mod.rs:400-430`, `src/protocol.rs:95-100`). A complete remote-alpha Tier 2 needs either an agent-side observer plus generation-aware protocol, or a group-level controller channel that receives one tree and distributes the decoded snapshot locally. Merely pooling SSH leaves N scans, N watchers, and N serialized trees.

## 4. Ordering and cheaper alternatives

My recommended order is:

0. **Instrument and expose watcher failure now.** `scan()` discards watcher construction errors with `.ok()` (`src/endpoint/local.rs:805-810`), and `await_change()` also suppresses the reason and sleeps (`src/endpoint/local.rs:1021-1032`). At minimum count/log “watch unavailable → full-scan polling,” the errno, root, and number of affected sessions. Also report the actual host's inotify limits; do not bake “8192” into the design argument as though it were universal.
1. **Introduce a shared observer registry and one physical watcher**, initially with per-subscriber bounded accumulators. This removes the resource ceiling without touching transition semantics.
2. **Evolve that observer into a generation/single-flight scan broker.** Do this first for `OneWaySafe` and `OneWayReplica`: reconciliation never emits alpha transitions in those handlers (`src/tree/reconcile.rs:351-418`, `src/tree/reconcile.rs:420-472`), so shared alpha observation cannot be invalidated by a beta writing alpha. Keep session validation leases even there so the abstraction remains correct.
3. **Add two-way support** only with a root mutation gate: serialize alpha transitions with shared scans, invalidate before mutation, and rescan/rebase after inbound beta changes as needed. Today independent session threads can already scan and transition the common alpha concurrently; the new gate would improve that behavior rather than preserve its nondeterminism.
4. **Evaluate shared supply only after Tier 2 measurements.**

If Tier 2 is expected soon, avoid investing heavily in a per-session path journal. Once scans are brokered, dirty paths are consumed once by the broker; subscribers only need a last-seen **snapshot generation**, which is constant-size and cannot grow. A generation check before sleep also solves the “broker consumed the event before this session waited” race.

A useful narrower implementation may be a **group-level one-way fan-out controller**: scan alpha once, scan betas independently, reconcile each with its private ancestor, then feed all destinations. This fits the configuration's actual group semantics better than forcing a shared mutable object through the current `Endpoint` trait, whose `&mut self` methods deliberately assume one serialized session (`src/endpoint/mod.rs:146-159`). It does change supervisor/control plumbing, but it makes ownership explicit and avoids pretending one endpoint's single supply slot serves N operations.

The relay/tree proposal is not simpler or equivalent:

- In two-way mode it changes conflict and causality semantics: a beta edit reaches alpha through another beta's ancestor and schedule, rather than its own alpha/beta/ancestor triple.
- It adds propagation hops and makes an intermediate beta's availability/capacity part of every downstream session.
- Ordinary sessions from beta 1 to N other betas would scan beta 1 N times, so the duplicate observer merely moves from alpha to the relay unless the relay is itself a new fan-out service.
- It can reduce source NIC fan-out if peers redistribute bytes, but it cannot reduce total bytes and is a topology/failure-domain feature, not an observation-sharing optimization.

One cheaper optimization with partial benefit is a broker-owned persistent alpha scan cache. It removes duplicate cache files/writes and lets later processes reuse digests after metadata checks, but it does not remove N full directory walks, N node trees, or N watchers. Treat it as an incremental win, not a substitute for Tiers 1–2.

## 5. Measurements and decision gates

The existing matrix already separates fan-out width and cold sync (`bench/orchestrate.py:46-97`), verifies that every destination agrees (`bench/job.py:351-376`), and samples process-tree RSS/CPU (`bench/harness/src/sampler.rs:1-25`). Extend it rather than relying on wall time alone.

### Baseline/Tier 2 measurements

Run widths 1, 2, 5, and 10 (100 only after the shape is understood), with the same alpha, corpus, machine size, and destination state. Report separately:

- cold empty-state sync;
- warm process restart with persisted caches;
- idle heartbeat for at least two full-scan intervals;
- one-file edit, burst edit, directory creation/removal, and sustained churn;
- one-way-safe/replica separately from two-way-safe with beta→alpha edits.

On the source, collect:

- wall time to **last** destination, per-destination spread, p50/p95/p99 propagation latency;
- user CPU and system CPU, peak RSS and post-quiescence RSS/PSS;
- `read_bytes` (physical I/O) **and** `rchar`/logical bytes read. Page cache can make physical reads look free while N scans still hash/copy the same bytes;
- minor/major faults, context switches, thread count, open FDs;
- inotify instances and watch marks from `/proc/*/fdinfo`, watcher creation failures, actual `max_user_instances`, `max_user_watches`, and `max_queued_events`;
- scan-cache bytes written and serialization CPU.

Add internal counters/timers around `LocalEndpoint::scan`:

- scan requests, actual scans, and coalesced/reused requests;
- full vs incremental scans and forced-full reasons (new subscription, overflow/error, periodic deadline, transition problem);
- directories listed, entries stated, file bytes hashed, snapshot generation and age;
- watcher event count, pending high-water mark, subscriber lag/overflow;
- transition invalidations, validations refused, and immediate follow-up cycles.

**Tier 2 gate:** implement it if width-10 source CPU, peak/steady RSS, logical scan bytes, or cache-write volume materially scales with N, or if scan contention worsens tail propagation latency. Do not reject it merely because `read_bytes` is flat: that only proves the page cache is working. Conversely, if Tier 1 plus warm caches leaves width-10 source CPU/RSS within roughly 20–30% of width 1 and latency is destination/network-bound, Tier 2 is probably not worth a risky two-way implementation. A one-way-only broker may still be worthwhile.

### Tier 3 measurements

First correct the cost model. `supply_from` reads/deltifies source content but does not hash it (`src/endpoint/local.rs:602-640`). The receiver's `DigestingWriter` verifies the requested digest before publishing staging (`src/endpoint/local.rs:757-784`). That receiver verification must remain per destination. Tier 2 has already removed repeated alpha scan hashing.

Instrument, per digest and fan-out cycle:

- number of destinations requesting it;
- source logical bytes read and time in `supply_from`/`rsync::deltify`;
- distribution/hash of destination signatures—deltas are shareable only for identical base signatures; cold destinations with empty signatures are the best case;
- generated frame bytes, encoded/compressed wire bytes, source NIC utilization, and destination backpressure;
- receiver verification CPU and staging write bytes (not removable by source sharing);
- cache/broadcast memory high-water mark and how long a slow destination retains shared content.

**Tier 3 gate:** pursue it only if, after Tier 2, repeated source reads/delta generation are a significant fraction of source CPU or limit last-destination latency, and a large fraction of fan-out needs have identical (especially empty) signatures. If signatures differ, a single encoded delta cannot be broadcast. If the source NIC or destinations dominate, sharing reads will save some CPU/memory bandwidth but not N copies of network and staging work; a relay/tree is then a separate network-distribution decision.

### Correctness/soak tests required before shipping

Resource wins are irrelevant without adversarial tests for:

- a subscriber paused longer than the pending cap, then resumed;
- reset and reconnect while other subscribers keep running;
- watcher overflow/error and watcher establishment failure;
- alpha edits during a shared scan and during supply;
- two betas concurrently changing the same alpha path and disjoint paths;
- a beta-triggered alpha transition while other sessions hold old scan leases;
- a transition problem forcing the next scan full;
- process restart with broker cache plus private ancestors;
- eventual convergence and no quiesced shortcut across a known generation invalidation.

The invariant to assert is: **observation storage may be shared; every reconciliation result remains tied to one session's ancestor and to the exact immutable alpha/beta scan leases from which that result was computed.**
