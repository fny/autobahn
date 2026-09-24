# Future features

Features that have been thought through but not built. Each says what it is for, how it would work, what it costs, and where it falls short, so picking one up starts from the design rather than from scratch.

## Halt before a sync fills a disk

**The problem.** Nothing checks free space today. A cycle that doesn't fit stages until a write fails, then errors and retries with backoff. Nothing is corrupted, because content is staged and renamed into place only when it is whole. But every retry fills the disk to the brim again, which starves everything else on that machine, and the person hears about it only as a generic `errored` after two minutes.

**The check.** Before staging, the session already knows how many bytes the cycle will write: the sizes of the files it is about to transfer (`staged_bytes`, from `file_sizes` in `src/session/mod.rs`). It compares that, plus a reserve, with the free space of the filesystem holding the destination's staging directory, and of its root when that is a different filesystem. If the transfer doesn't fit, it stops before writing anything:

> halted: beta needs 12.4 GB for this cycle and has 3.1 GB free (keeping 1 GB spare), so nothing was copied; free up space and it resumes on its own

- **A new `SafetyHalt` variant.** Like the missing-alpha halt, it clears on its own when space frees up, and its `alert_after` is a couple of minutes rather than immediate. A disk being freed while you watch is common.
- **The reserve** is the larger of 5% of the filesystem and 1 GB, so the machine is never left with nothing. It could become a setting later if anyone asks.
- **Both sides are checked**, since two-way cycles write to both.

**No latency on the edit path.** This is the part that has to be right:

- **Local free space** is one `statvfs` call, a few microseconds.
- **Remote free space must not cost a round trip.** Small files (under 64 KB) are sent before the destination answers the staging request, and a transition that sent everything goes out without waiting for that answer. That is what made an edit to a remote beta one round trip instead of three and a half. So the answer to the staging request is the wrong place for free space. Instead the agent *volunteers* it in answers it already sends — every scan answer and every "unchanged" answer — and the controller keeps the latest reading, a few seconds old at most.
- **The check runs only on cycles that could plausibly fill a disk:** more than 64 MB to stage, or more than the last reading's spare. An ordinary edit of a few kilobytes never waits on anything and compares against nothing but a number in memory.

**Cost.** About a day:

- the free-space reading on each side
- one field on two existing responses, which is a wire change and so rides on the next compatibility epoch bump rather than forcing one of its own
- the check and the halt
- tests, taking the reading through a seam a test can override, since a test cannot really fill a disk; a small `tmpfs` checks it live on Linux

**Where it falls short.**

- **The reading is a moment's estimate.** Another process writing to the same disk mid-cycle can still fill it. The check stops the predictable case — a large first sync, one huge file — and staging keeps the rest safe, as it does today.
- **All or nothing.** It halts the whole cycle rather than syncing what fits. That is simpler and matches the other halts; syncing what fits is possible later, but a partly synchronized tree is harder to reason about.
- **Deletions in the same cycle free space too**, but they are applied in the same pass as the writes, so the check doesn't count on them.

**Related, and separate: a sync that saturates the disk.** A large sync can keep a disk busy enough that the machine crawls. That calls for a limit, not a halt: a cap on transfer bandwidth, or on the scan and apply threads, set in the configuration. Also about a day.
