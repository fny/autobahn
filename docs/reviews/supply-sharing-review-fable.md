# Supply-path sharing (tier 3-lite): payload cache review

Follow-up to the fan-out design review (`/tmp/fable-fanout.md`, 2026-08-27), responding
to the EC2 fan-out measurements (widths 1–4, 63k files / 520MB, elapsed flat ~51s,
source CPU = 0.70 + 3.27·width with rising marginals) and the source-only attribution
harness (supply 0.62s + bincode encode 0.72s + LZ4 0.27s = 1.61s of the 3.27s per
destination).

## 1. The design is right in concept; two corrections, one of which sinks "no wire format change"

**The safety story is sound, and stronger than you argued.** Keying by (need-list
digest, batch ordinal) is content-addressed end-to-end: the need list embeds every
requested digest and signature, batch formation is deterministic given the needs
(`SUPPLY_BATCH_SIZE` is a constant at src/session/mod.rs:25, the byte budget at
src/endpoint/local.rs:66), and the receiver independently verifies every file against
its requested digest (`finish_receive`, src/endpoint/local.rs:762–785). Even the nasty
race — a file changing on disk between the producer's read and a follower's reuse —
resolves safely: the follower ships the producer's bytes, which either match the
requested digest (fine) or get discarded at the receiver and retried next cycle,
exactly as a torn read does today. One refinement: don't cache a batch containing an
`EndOfFile { error: Some(_) }` frame (src/endpoint/local.rs:582), or one session's
transient read failure gets replayed to nine destinations that would have succeeded.

**Correction 1 — the payload is not verbatim-reusable, because the channel id lives
inside it.** The `[flag][payload]` self-description at src/transport/mod.rs:614–630 is
real, but the encoded message is `MuxRequest::Request { channel, request:
Request::StagePush(frames) }` (src/transport/mux.rs:317–319, src/protocol.rs:144–149)
— the channel id sits in the first bytes of the bincode body, so the LZ4 output
differs per channel. It happens that a fan-out to N distinct hosts gets N fresh
connections whose `next_channel` each starts at 1 (mux.rs:144), so today's happy path
would produce byte-identical envelopes *by coincidence* — broken the first time a
session reconnects (channel ids are monotonic and never reused, mux.rs:196–201) or two
betas share a host. Don't build on that. Cache **beneath the envelope**: add a
`Request::StagePushPrepared(Vec<u8>)` variant carrying the pre-encoded frames payload,
splice it into a per-session envelope. That is a wire change, but the cheapest
possible one — the handshake already enforces exact version equality
(protocol.rs:30–31, and the `Response` doc at :115–119 tells you the team already
treats variant additions as version bumps).

You then face a sub-choice: cache the payload pre-compressed (full 1.0s saved per
follower, but the outer `send_frame` will attempt LZ4 on incompressible bytes —
burning the 0.27s you saved — so the prepared path needs to bypass the compression
attempt), or cache uncompressed bincode and let each session compress (simpler, saves
0.72 of the 1.0). Start with the latter.

**Correction 2 — the cache is a producer/follower protocol, not a map.** Batches must
be sent in order, so a follower that misses ordinal k+1 (leader not there yet, or
evicted) can't just pull independently — its supply stream has never advanced past 0.
The clean escape hatch: on a miss, the follower opens its own supply and
**pulls-and-discards** to fast-forward to its ordinal, using only existing trait
methods — paying the 0.62s read but not the encode, and decoupling itself from the
leader forever after. That makes the worst case (divergent drain rates, leader stall,
leader death) degrade to the status quo with no cross-session blocking, which is the
property that makes this shippable. Bound the cache at a few batches per key (~32MB),
count hits, and let laggards miss.

**Do the cheap thing first, though, because it shrinks the prize.** `bincode 1.3`'s
`serialize()` runs a sizing pass and then an encoding pass — two full traversals — and
`send_frame` then copies the whole thing a third time assembling `payload`
(src/transport/mod.rs:605, :618–630). `serialize_into` a reused buffer kills the
sizing pass; writing the flag/length then the body kills the copy. That's ~10 lines,
no protocol change, and should cut your 0.72s encode roughly in half. Remeasure after:
if the shareable residue drops to ~0.6s/dest, the cache's case weakens accordingly.

## 2. Does the rising marginal change the width-10 verdict?

Partially — I'll concede the forecast was too absolute, but your own numbers say the
binding constraint at width 10 on this hardware still isn't CPU. Width 1 moves 520MB
in ~51s ≈ 10MB/s per pair: this cell is **network-bound**, which is exactly why
elapsed is flat. Extrapolating your rising marginals (+0.65/step) to width 10 gives
~56 CPU-s over the window — barely over one core average on 8 vCPUs. The knee you'd
actually hit first at width 10 is the instance NIC (100MB/s aggregate egress), not the
CPU. So: **sharing buys headroom, not makespan, until either the links get fast or the
source gets small** — a fat-NIC/same-rack fan-out inverts this completely (at 2.5GB/s
aggregate the transfer window collapses and those 56 CPU-s become the wall).

Before concluding anything from the rising marginal, diagnose it — it's one `perf
stat` away: run width 1 and width 4 and compare **instructions per destination** vs
**cycles per destination**. Flat instructions + rising cycles = SMT/memory-bandwidth
contention (your 8 vCPUs are 4 physical cores; at width 4 you have ~8 busy
hyperthreads), which sharing relieves superlinearly — every CPU-second removed also
de-contends the siblings. Rising instructions = something genuinely superlinear per
session, in which case a payload cache is treating a symptom and you should find the
O(width) term first.

One sequencing point that strengthens your design: **tier 2 raises this cache's hit
rate.** Need lists are identical only when the sessions reconciled from identical
alpha snapshots; the shared scan guarantees that structurally, where today it holds
only because cold-sync scans happen to agree. Payload sharing layered on the shared
observer is a better system than payload sharing bolted onto ten independent scans.

## 3. Where the other 1.66s lives — ranked, with the discriminating measurement for each

Your harness starts at `supply_open` with a warm snapshot and stops before the
transport. What it excludes, in likely-size order at 63k/520MB:

1. **Per-session alpha scan + digest (~0.3–0.5s/dest).** Every session cold-starts its
   own `LocalEndpoint`, which stats 63k entries and blake3-digests 520MB
   (src/endpoint/local.rs:789–872). Measure by adding a `scan()` step to your harness.
   Note this line item is already scheduled to die with tier 2 — attribute it before
   crediting it to tier 3.
2. **The ssh child processes (~0.2–0.4s/dest) — if your "source CPU" metric is
   machine-wide.** Cipher over the ~compressed wire volume plus ssh's own buffering.
   `pidstat -p` splitting the autobahn process from its ssh children answers this in
   one run; also try forcing `aes128-gcm@openssh.com` vs chacha20 and watch the delta.
   Unshareable by any tier — this plus egress is the true per-destination floor.
3. **Frame assembly and pipe writes (~0.1–0.2s/dest).** The third copy in `send_frame`
   (:618–630), length-prefixed writes into a 64KB pipe to ssh (thousands of write
   syscalls and context switches per GB), all under the connection's writer mutex
   (mux.rs:135).
4. **Session bookkeeping (~0.15–0.3s/dest).** `transition_dependencies` allocating 63k
   path strings (src/session/mod.rs:465), reconcile over the tree, `apply` +
   `validate` + synchronous ancestor serialize/write (src/session/mod.rs:419–441),
   two `store_scan_cache` serializations (~6–7MB each at this scale, on the background
   writer but still CPU).
5. **Decode of beta traffic (~0.05–0.1s/dest).** The mux router thread deserializes
   the StageBegin needs list (63k `StagingNeed`s), the `TransitionOutcome` carrying
   63k achieved `Node`s, and the beta's post-sync snapshot — deserialize is
   allocation-heavy (one String per node).

Those ranges sum to 0.8–1.5s, which brackets your missing 1.66 — but don't take my
ranking on faith when one `perf record -g` on the supervisor during a width-4 run will
hand you the whole attribution by symbol: `blake3::`, `bincode::`, `lz4_flex::`,
`memcpy`, `__libc_write`, with the ssh children showing up as separate processes. That
single profile plus the instructions-vs-cycles comparison from Q2 answers everything
here without guessing twice.

**Bottom line:** design concept confirmed (content addressing makes reuse safe), but
cache below the mux envelope with a small protocol addition rather than relying on
channel-id coincidence, give followers the pull-and-discard escape hatch, and do the
bincode buffer-reuse fix first since it halves the thing you're proposing to share.
Updated forecast: tier-3-lite pays iff cold fan-out runs on links fast enough that the
source's CPU window binds — which your next two measurements (instructions/cycles
split, and the perf profile) will tell you directly.
