# F-L-STATE: Small journal, ancestor and scan-state correctness fixes

**Findings:** L-16, L-17, L-18, L-19, L-26 (all OPUS, Low).
**Status:** proposed. Low. Each is small. Do them alongside F-M-STATE, which changes the same journal code.

Verification: all of these come from OPUS, which traced each one by hand. This walkthrough checked only that the code they point at still exists. Before fixing each, write its test and watch it fail first.

## Items

- **L-16. The journal's directory entry isn't synced.** A journal first created by `checkpoint()` never has its directory entry fsynced. A later durable `intend()` then skips the directory sync because the file already exists (`src/session/ancestor.rs:~398-401`, `~507-514`). After a power loss, the file itself can vanish.
  - **Fix:** fsync the parent directory whenever the journal file is created, wherever that happens.
  - **Test:** a unit test with a directory-sync counter hook shows exactly one sync on creation.
- **L-17. Unresolved intents can be lost at open.** A format upgrade at open rewrites the journal while intents are still unresolved (`:~238-268`), and `stored_generation()` (`:317`) drops the unresolved list. Paths that crashed mid-transition then lose their taint, so they can be overwritten instead of raising a conflict.
  - **Fix:** carry unresolved intents through the rewrite, as intent records in the new journal. Have `stored_generation` ignore them only for the generation it computes, never by rewriting.
  - **Test:** a journal in the old format with one unresolved intent, opened and upgraded, still reports that intent.
- **L-18. A compaction failure fails a successful cycle.** If compaction fails after a durable append succeeded, the whole cycle is reported as failed (`:~376-381`). On FUSE or network home directories, where directory fsync always fails, it fails every cycle.
  - **Fix:** a compaction failure after a successful append is logged once, and the journal is left uncompacted; the next cycle retries. The cycle's own result stands. After repeated failures, back off compaction attempts.
  - **Test:** a compaction hook that always fails leaves cycles succeeding and the journal growing, with one log line.
- **L-19. Result counts are never checked.** `achieved_changes` pairs transitions with results using `zip` (`src/endpoint/mod.rs:~119-129`). If an endpoint, remote or buggy, returns fewer results than transitions, the extra transitions are dropped silently, and the ancestor records less than was attempted.
  - **Fix:** assert equal lengths, and treat a mismatch as a failed transition. It is a protocol error from a remote endpoint.
  - **Test:** a fake endpoint returning one result for two transitions makes the cycle fail, and the ancestor is unchanged.
- **L-26. Recent-write protection is lost on adopted subtrees.** An incremental scan stamps `scanned_at` as now (`src/scan/mod.rs:266`) on a snapshot that includes adopted subtrees, whose digests were recorded earlier. A later full scan then applies the recent-write rule, which re-reads a file whose mtime is too close to its scan time, against this newer `scanned_at`. So it trusts a digest recorded while the file's mtime was still recent, and a same-second rewrite can be missed.
  - **Fix:** keep the rule's reference time per subtree. The simplest is for adopted subtrees to keep their baseline's `scanned_at`, which the snapshot already carries (`baseline_scanned_at`, `:413`, `:486`). An alternative is for an incremental snapshot to carry the *oldest* `scanned_at` of anything it adopted.
  - **Test:** write a file, scan fully, rewrite it within the same second with the same size, scan incrementally without that path marked, then scan fully. The full scan must re-read it.
