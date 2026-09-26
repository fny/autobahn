# F-M-OBS: The shared observer and its watcher report what's on disk

**Findings:**
- M-23: cached scans bypass the entry limit (ASTRA F14, reproduced).
- M-24: the polling fallback serves stale snapshots (ASTRA F15, reproduced; OPUS M7).
- M-25: Linux watches skip re-included directories (ASTRA F16; OPUS M8, reproduced).
- M-26: errors extending the watch at runtime are discarded (ASTRA F17; OPUS M10).
- M-27: a replaced root keeps a dead watcher (OPUS M9, reproduced).
- M-28: a failed scan loses the dirty marks it used up (OPUS M18).
- L-27: `scanning` stays set after a panic in the walk (OPUS Low).

**Status:** proposed. Medium, except M-24, which is the fallback users land on when inotify runs out of watches. Treat M-24 as the first fix in this ticket.

## Problems

All references are to `src/endpoint/observer.rs` unless noted.

- **M-23.** `scan` returns the published snapshot on a cache hit (`:349-353`) before it checks the caller's `max_entry_count` (`:398-408`). The observer is shared across sessions, and its key doesn't include that per-session limit. So a session with a generous limit can warm the cache for a stricter one. ASTRA saw an endpoint limited to one entry accept a two-entry snapshot.
- **M-24.** The cache is reused whenever the generation hasn't moved and no full scan is due (`:351`). Nothing requires a working watcher. Without one, meaning watch setup failed, or the watch limit was reached, or a one-shot run, external writes never advance the generation. Repeated scans then return old data until the 120-second full walk, not the documented polling interval.
- **M-25.** The scanner walks through an ignored directory when a `!` pattern re-includes something under it (`holds_a_re_inclusion`, `src/scan/mod.rs:~875`). Watch registration stops at any ignored directory (`src/endpoint/local.rs:233-238`). So with `vendor` plus `!vendor/keep.txt`, edits to `keep.txt` wait for the full walk.
- **M-26.** When new directories appear, the code adds watches for them with `let _ = watch_tree(…)` (`src/endpoint/local.rs:~401-408`). If the inotify limit is hit after startup, the new subtree goes unwatched, while the observer still reports itself healthy. Nothing logs it, falls back to polling, or retries.
- **M-27.** After `mv A A.old && cp -a A.old A`, the watcher keeps watching the old inode. Every later edit waits for the full walk, and events from `A.old` mark unrelated paths.
- **M-28.** If the walk fails, `result?` returns (`:394`) after the walk has already taken the dirty marks, and without resetting `last_full_scan`. The entry-limit refusal also returns before the baseline is updated. Either way, the next incremental scan has lost the marks it needed.
- **L-27.** `state.scanning = false` runs only after `walk` returns (`:392`). A panic in the walk, or an `EAGAIN` from spawning a helper, leaves it `true`, and every caller then loops on the 60-second wait (`:357-363`).

## Proposed resolution

- **M-24: no cache without a healthy watcher.** Reuse the published snapshot only while a watcher is established and healthy. Otherwise every scan walks, which is the polling promised in the docs. One-shot runs never reuse the cache.
- **M-23: check the limit on every return.** Move the entry-limit check into a helper, and call it on the cached path too.
- **M-28: put the marks back on failure.** If the walk fails, merge the marks it took back into the pending set, and require a full scan next time. The entry-limit refusal still updates nothing, but it also doesn't consume marks.
- **L-27: reset `scanning` in a guard.** A drop guard sets it to `false` and notifies waiters, so a panic or an early return can't leave it set.
- **M-25: one traversal policy.** Watch registration asks the same question as the scanner: does this ignored directory hold a re-inclusion? Share one function between the two.
- **M-26: surface watch failures.** A failure to extend the watch marks the observer "partly watched". That turns off cache reuse, as in M-24, appears in `status`, is logged once, and is retried with backoff to rebuild coverage.
- **M-27: detect a replaced root.** Record the root's `(dev, ino)` when the watch is set up. On `MoveSelf` or `DeleteSelf`, or whenever a scan finds a different inode, rebuild the watcher.

## Tests

- **M-24:** with watching turned off, create a file with an ordinary external write, without any announcement from autobahn. The next scan includes it.
- **M-23:** warm the cache with a permissive caller, then call from a strict one. The strict caller is refused. Try both orders.
- **M-25:** with `vendor` and `!vendor/keep.txt`, an edit to `vendor/keep.txt` reaches the next incremental scan. Linux only.
- **M-26:** make the watch limit fail, through a test hook, when a new directory is created. The observer reports partial coverage, and scans walk.
- **M-27:** replace the root, then edit a file. The edit arrives without waiting for the full walk.
- **M-28:** make the walk fail once, through the existing `after_walk` hook or a new one. The next scan still sees the marked change.
- **L-27:** a walk that panics, through a test hook, leaves `scanning` false, and a second caller doesn't wait 60 seconds.
