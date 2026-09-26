# F-M-STAGE: Staging and publishing don't strand, race or fail whole cycles

**Findings:**
- M-29: concurrent publishers race on shared staged content (ASTRA F19; OPUS Low).
- M-30: leftover `.autobahn-tmp-apply-*` files are never removed, and they block directory deletion (OPUS M3).
- M-31: a staging base that changes mid-stream fails the whole cycle (OPUS M11).

**Status:** proposed. Medium.

## Problems

- **M-29.** Content with the same digest is staged once and used by several publishers. Each publisher decrements the use count *before* opening the staged file (`src/endpoint/local.rs:~2181-2240`). With two users of one digest, A can take the count from 2 to 1 and pause, then B takes it from 1 to 0 and renames the staged file into place. A then fails to open it and reports "staged content unavailable". The cost is an extra transfer and an extra cycle. Nothing is lost, and the sync still converges.
- **M-30.** A crash in the middle of a copy-publish leaves an `.autobahn-tmp-apply-*` file inside the synced tree (`:2233`). The scanner hides such names (`src/scan/mod.rs:59`, `:78`), and `sweep_staging` cleans only the staging directory, so nothing ever removes the file. Worse, when its directory is later deleted, `remove_directory` finds the leftover, treats it as unexpected content, and reports a *disagreement* (`:2519-2528`). A disagreement distrusts the baseline, which forces a full walk every cycle, indefinitely. Leftovers from large files can also leak gigabytes of disk.
- **M-31.** If the destination's base file shrinks between `stage_begin` and the push, `rsync::patch` hits the end of the file. The *whole* staging stream then errors, and every file in the cycle fails (`src/endpoint/local.rs:922-973`, `src/rsync/mod.rs:343` onward). A change on the source side is already handled per file.

## Proposed resolution

- **M-29: open before releasing.** A publisher opens the staged file first, and only then decrements the use count. The publisher whose decrement reaches zero may move the file; everyone else copies from their open handle. Alternatively, keep a per-digest lock across open-and-decrement.
- **M-30: clean up and don't stall.**
  - When publishing into a directory, remove `.autobahn-tmp-apply-*` files older than a few minutes, whose process id isn't running.
  - When removing a directory, treat an autobahn temporary as removable rather than unexpected. It is autobahn's own litter, and T1-4 prevents peers creating that prefix.
  - At startup, sweep the root for such leftovers, as a background task after the first scan.
- **M-31: fail one file, not the stream.** When patching one file fails because its base changed or went short, discard that file's partial output. End it with an error, as a failed supply does, keep the stream going, and let the next cycle transfer it again. Only a framing error ends the whole stream.

## Tests

- **M-29:** two publishers of one digest, with a barrier between the decrement and the open, both publish without an extra transfer.
- **M-30:** leave an old `.autobahn-tmp-apply-x`, then delete its directory on the other side. The directory is removed, no disagreement is reported, and the next cycle is incremental.
- **M-31:** truncate the destination base between staging and push, for one of three files. Two files publish, the third retries and converges on the next cycle.
