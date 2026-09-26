# F-H3: A transition offers its fold at the lease's generation

**Findings:** H-3 (ASTRA F03, reproduced; OPUS H3). Regression from `17715e9` (2026-09-23).
**Status:** proposed. High; fix before v1.

## Problem

Several sessions over one root share an observer. The observer holds a baseline snapshot, and after a transition, an endpoint offers the observer a *fold*: the snapshot its lease scan returned, with its own writes applied. `offer_baseline` (`src/endpoint/observer.rs:522-530`) is supposed to refuse a fold built from an older snapshot than the one it holds:

```rust
if based_on < state.baseline_generation { return; }
```

`LocalEndpoint::transition` passes the wrong generation. After the post-write announcement it sets `self.seen_generation = self.observer.generation()` (`src/endpoint/local.rs:1528`), and then offers the fold with that value (`:1564`). The fold was built from the lease snapshot, but it is labelled with the observer's *current* generation. So the refusal can never fire.

Scenario, reproduced by ASTRA with two real endpoints sharing one observer:

1. A and B both see `left/x` and `right/y`.
2. B deletes `right/y` and refreshes the shared scan. The baseline moves on.
3. A deletes the unrelated `left/x`, working from its older lease, and offers `fold(old snapshot − left/x)`. The offer is accepted, so the baseline again contains `right/y`.
4. B's next scan reports `right/y` as present, although it is gone from disk.

Dirty marks for `right/y` were already used up at step 2, so the next incremental scan has no reason to look again. The wrong state stands until the periodic full walk, up to 120 s. Propagation can be held back until then, and reconciliation can act on the stale value.

The same line also **swallows wake-ups**. An external change that lands between A's lease scan and its post-write announcement gets folded into `seen_generation`, so `watch_poll` doesn't wake for it.

The existing tests call `offer_baseline` directly with generations chosen by hand, so they can't see this.

## Proposed resolution

- **Offer at the lease's generation.** At the start of `transition`, before the first `invalidate`, capture the generation of the snapshot the lease came from. `self.seen_generation` holds it after `scan` (`:1114`, `:1123`), so copy it into a local, say `lease_generation`. Offer the fold with that value. Then a baseline that moved past the lease refuses the fold, and the next scan walks what changed.
- **Advance `seen_generation` only past our own writes.** Have `invalidate` return the generation it produced. After the post-write announcement, set `seen_generation` to that value only if the observer's generation still equals it, meaning nobody else advanced it in between. Otherwise leave `seen_generation` at `lease_generation`, so the next `watch_poll` wakes and the next scan reads the foreign change.
- **Keep one extra cycle.** The existing extra cycle after a session's own writes stays. `TODO-SPEED.md` records it as measured and accepted.
- **Keep the comment honest.** Update the comment at `:1515-1527` to match.

## Tests

- **Two endpoints, one observer, driven only through `LocalEndpoint::scan` and `transition`:**
  - two paths, as in the scenario above;
  - B's intervening scan;
  - A's transition from its older lease.

  Then B's next scan must report the path it deleted as absent. Run it once with a watcher and once with watching turned off, the polling fallback.
- **Wake-up:** make an external write between A's lease scan and its transition. Then `watch_poll` from A's `seen_generation` returns "changed".
- **Mutation check:** restore line 1528's current behaviour, and confirm the two-endpoint test fails.
- **Randomized interleavings:** extend the existing interleaving sweep so the offering endpoint's lease can be older than the observer's baseline.

## Docs

`docs/correctness/INVARIANTS.md` I1 promises that `offer_baseline` refuses an offer based on an older generation. Name the new two-endpoint test under "Checked by".
