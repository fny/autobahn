# Speed

Where the remaining time goes, and what would take it back. Every item names the measurement that would decide it, because the analysis estimate has been wrong every time it was tried; nothing here ships without an A/B (`bench/ab.sh`, now with `--remote HOST` for a destination over ssh, and `tc netem` on a loopback alias for a link with latency).

Numbers are from 2026-09-22 on an 8-core c6i-class box unless a cell name says otherwise. Base is 0.4.0 at `6b67b4a`.

## Done — concurrency, measured and committed

- [x] Both endpoints watched at once (`51f4325`). One editor: p90 125 → 52 ms, p99 140 → 55 ms local; p90 129 → 54, p99 149 → 60 over ssh. p50 unchanged. This was the 125 ms half-slice step in `Session::await_change`, the plateau the `5k-1` cell lost to mutagen on.
- [x] The scan walk spread over threads, one subtree each (`c4e4109`). 160k-file cold scan 7–34 s → 2.9 s. Warm walk unchanged. Capped at 8 threads per scan, cut to the host's cores.
- [x] A transition spread over threads (`f68b0a8`). 40k cold sync 5.6 → 4.5 s; 1,000-file burst cycle 165 → 120 ms (ten reps, no overlap).
- [x] The transition sent right behind the last push (`23e9145`). At 10 ms one-way delay: p50 123 → 104, one round trip of the ~3.5 an edit paid.
- [x] ~~Transfer pipeline depth 1 → 4~~ — measured, nothing: cold sync over ssh 7.7/6.6 → 7.1/7.1 s. Dropped. The pull side is not the bottleneck; apply was.
- [x] `COMPATIBILITY_EPOCH` 12 → 13 (`17715e9`, with the generation-carrying answers the scan skip needed). An agent that never scans measures its wait from the last change it reported; a same-versioned stale agent would have answered `true` forever on that channel.

## Round trips — the largest remaining win off a LAN

An edit cost ~2.5 round trips to a remote beta: the beta scan, `StageBegin`, and push + transition. Measured slope on base was 7 ms of p50 per ms of one-way delay; each round trip is 2 ms/ms.

- [x] A standing watch stands in for the beta scan (`17715e9`). Scan and transition answers carry the agent's generation, the watch waits from it and says whether the root is watched, and a cycle reuses the last snapshot while the watch stands. 1 editor at 10 ms one-way: p50 101.8 → 82.2, one round trip. A watcher in backoff answers "not watching" and is never skipped; a snapshot older than a minute is not reused, so the periodic full walk still runs. Epoch 13.
- [ ] `StageBegin` before the push — the last round trip that could go. Not a small change: the receive side is positional (frames are consumed against the destination's own need list, with the destination's rsync signature), so content cannot be pushed before the needs are known without a frame that names its digest, an agent that accepts named frames for any requested digest and drops the rest, and a session that pushes small files speculatively and the rest after the answer. A protocol change in the transfer path, for one round trip (−20 ms at 10 ms one-way). Worth doing deliberately, with the collision tests extended to the new framing; not worth doing quickly.
- [ ] An edit to a remote beta is now ~1.5 round trips: on a 40 ms link ~80 ms. The item above would make it ~1.

## Cold sync — the second hash

- [x] ~~Skip the re-read in `publish_file` by verifying the staged inode's identity (device, inode, size, ctime) against what receipt recorded.~~ Built and measured 2026-09-23: the 1,000-file burst cycle 0.12–0.13 → 0.12–0.14 s, the 40k cold sync within the run-to-run spread. The re-read is page-cached and hashing is fast; the second hash is not a cost worth a change to the safety path. Dropped.
- [ ] The rsync delta on the source (`supply_pull`) is one thread. Only matters for large files on a cold sync or a patch cell; measure `chromium-1-patch` before touching it.
- [ ] blake3 on a single large file is one thread; `update_rayon` would split a multi-GB file. Nobody has asked; leave it until a cell shows it.

## The warm walk — explained

Profiled 2026-09-23 (`perf`, `sync` on a converged 160k-file pair, 2.27 s wall, 1.9 s user, 3.4 s system). Not a runtime cost: a `watch` session on the same pair, after its 2.8 s cold start, ran 165 s without any cycle reaching the 1 s logging threshold — the periodic full walk of 160k files is under a second in a running session. The 2.2 s is the one-shot process's start:

- 30% `watch_tree` — registering inotify watches for the whole tree, one `statx` per entry, on both sides. A one-shot `sync` never waits, so it never needs them.
- 14% the scan itself (`probe_entry`, `scan_directory`), already parallel.
- ~10% thread wake-ups of the parallel walk (`futex`, `eventfd`, `schedule`): the helper budget spawns a thread per subtree.
- 2% deserializing the scan cache; reconcile 0.2%.

- [ ] The one-shot verbs (`sync`, `verify`) should not establish watchers. Bounded win for those verbs only: ~30% of their runtime on a large tree, several seconds on Chromium. Nothing changes for `watch`, which registers once at start.
- [ ] The parallel walk could pool its helpers rather than spawn per subtree; ~10% of a cold scan's CPU, no wall-clock evidence yet. Measure the cold scan on the 160k pair before bothering.

## Coalescing and the cycle's own overhead

- [ ] `await_activity`: `SETTLE = 100 ms`, `QUIET = 20 ms`. Examined once (2026-09-22); the settle is only paid by a burst, an isolated edit does not wait it out. Re-measure only if a cell's p50 moves toward 100 ms.
- [ ] `MAXIMUM_FOLLOW_UP_CYCLES = 5`: a busy tree can spend five inner cycles before the outer cycle reports. Not a latency cost for the edit that started it; a throughput cost under a sustained burst. Measure with the 100-agent cells before changing.
- [ ] The extra cycle after every transition: the beta's own writes bump its generation, the watch fires, the next cycle scans and finds nothing. Cheap (an incremental scan of the paths just written), but it is one full round of endpoint requests per edit. The observer could record the generation its own transition produced and let the endpoint measure from there. Decide with idle-CPU and cycles-per-edit counts from the debug log, not latency.

## Fan-out

- [ ] Ten sessions to ten betas share one alpha scan (the root observer) but each stages and transfers the same content ten times from the alpha. `two50k-10-fan` and `chromium-10-fan` are the cells; the supply side could serve one read to N pushes. Big change (the design notes in memory call it tiered alpha-sharing); measure the fan cells' cold sync and p50 first to see how much of the 10× is the alpha's read.
- [ ] Two scans per cycle each take a thread budget of 8; a fan-out group's N sessions on an 8-core alpha can start 8N walk threads on a cold start. The budget should probably be per process, not per scan. Measure `chromium-10-fan` cold sync with a process-wide budget of 8 vs today.

## Idle

- [ ] Idle is now one watch request every 2 s per remote session (`WATCH_REQUEST_MAX`), against four a second before. The cap is there so a watch outliving its endpoint releases its channel; a close on the watch channel from the endpoint's drop would let the request run the whole heartbeat. Measure idle CPU on both ends with 10 sessions before and after; the README's "0.2% of a core" is the number to hold.
- [ ] The 120 s full walk on the beta host is now an 8-thread burst every two minutes. Fine on a build box, worth a look on a laptop on battery: measure with `powermetrics` on the Mac.

## The harness — so the above can be measured honestly

- [ ] A burst cell in the AWS matrix (an unpacked archive, a `git checkout`), timed from the tool's own debug log. The manifest-polling measurement has ~100 ms of resolution and misread a 45 ms improvement as a regression on 2026-09-22.
- [ ] `aggregate.py` refuses a job whose recorded `destinations` do not match its cell's `betas`. The field is recorded since `6b67b4a`; nothing checks it. This is the guard that would have caught the six 10× jobs in `bench-1789947877` in the results alone.
- [ ] A scan test that walks one tree serially and in parallel and asserts identical hierarchies and identical storage sharing. The parallel path is exercised by the existing tests on an 8-core box and by nothing at all on a 1-core CI runner.
- [ ] Re-run the five cells the width leak contaminated (`50k-10`, `chromium-1-bidir`, `chromium-1-patch`, `chromium-10`, `chromium-10-bidir`) with the fixed harness, and correct `docs/benchmarks.md` and `docs/benchmark-matrix.md`. Corrected pooled p90s from the clean repeats, for reference: `chromium-10` 4,264 → 425 ms; `chromium-1-bidir` 9,273 → 1,616; `chromium-10-bidir` 8,226 → 1,944; `chromium-1-patch` 3,038 → 292; `50k-10` 636 → 96.
