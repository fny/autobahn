# F-L-MISC: Small correctness and consistency fixes

**Findings:** L-7 (KIMI), L-9 (DEEPSEEK F23, KIMI ABN-L5, OPUS), L-20, L-22, L-23, L-25, L-28, L-29, L-31 (OPUS).
**Status:** proposed. Low. Each item is independent, so they can be picked off one at a time.

Verification is noted per item: **confirmed** means it was checked in the current code during this walkthrough, and **from OPUS** means not re-checked.

## Items

- **L-7. Manual `sync` doesn't expand `~`.** Confirmed. `run_sync` builds local paths with `PathBuf::from(spec)` (`src/main.rs`, in `run_sync`), while configured endpoints go through `expand_tilde` (`src/config.rs:1644`). A quoted `'~/backup'` becomes a literal `./~/backup` directory, and the session's identity and lock are computed on that wrong path.
  - **Fix:** expand local specs in `run_sync` exactly as `parse_endpoint` does, before identity is computed.
  - **Test:** `autobahn sync '~/a' '~/b'` syncs the home-relative directories and creates no `./~`.
- **L-9. `Response::Scan` isn't validated.** Confirmed. The delta path runs `root.validate(false)` on a snapshot it rebuilds, but the direct `Response::Scan` arm (`src/endpoint/remote.rs:145`) stores the snapshot as received. Unsorted or duplicate children then break the ordering that every merge relies on. The variant also appears unused, since every scan answers as a delta or "unchanged".
  - **Fix:** validate on that arm now. Remove the variant at the next epoch bump, as HYG-4 notes.
  - **Test:** an agent answering with an unsorted `Response::Scan` is refused.
- **L-20. Keep-both rename can overwrite.** Confirmed. `rename()` checks that the target doesn't exist, then calls plain `fs::rename`, which replaces anything created in between. `publish_rename(…, false)` (`src/endpoint/local.rs:2922`) already does an atomic no-replace rename.
  - **Fix:** use it in `rename()`. T1-3 rewrites the same function, so do this there.
  - **Test:** a target created between the check and the rename, through a test hook, isn't overwritten.
- **L-22. Upload errors hide their cause.** From OPUS. `upload_agent` reports "Broken pipe" when the remote install script fails, instead of the script's error output, and its stdout is piped but never read (`src/transport/install.rs`, around `:231-234` in OPUS's numbering).
  - **Fix:** capture the remote's stderr, include its last lines in the error, and drain stdout.
  - **Test:** with a fake SSH whose install script writes "disk full" to stderr and exits 1, the error contains "disk full".
- **L-23. Reload checks one read and applies another.** Confirmed. `watch` reads the file to compare its bytes, then calls `load(&self.path)`, which reads it again (`src/supervisor/reload.rs:~183-195`). A write landing between the two reads can mark bytes as applied that were never validated.
  - **Fix:** parse the bytes already read, with a `load_bytes(&[u8])`, and record exactly those as applied.
  - **Test:** a hook that swaps the file between the read and the load doesn't apply the swapped content.
- **L-25. SSH connection pool slots are never removed.** From OPUS. A host removed from the config keeps its SSH process and agent until the supervisor exits (`src/transport/mux.rs:~534-632`).
  - **Fix:** OPS-7, per-session reload, covers this: remove a slot when no running session uses its host.
  - **Test:** in OPS-7.
- **L-28. Wildcard re-inclusions under an ignored directory do nothing.** Confirmed (`src/scan/ignore.rs:~103`). With `vendor` and `!vendor/*.patch`, nothing is re-included, because a wildcard makes the scanner give up on finding which directories to descend into. There is no warning, although one was reported in the past. Separately, inside a re-included region, *any* negation, such as `!*.md`, re-includes files there.
  - **Fix:**
    - Warn at config load when a wildcard negation sits under an ignored directory, with a message saying it has no effect, and suggest listing the directory explicitly.
    - Document the rule in `docs/ignores.md`.
    - Check the second behaviour against gitignore semantics, and fix or document it.
  - **Test:** that config produces the warning. A fixture matching gitignore's result for `!*.md` passes.
- **L-29. `select` keeps only one relative path.** From OPUS. With nested groups, the last match overwrites the relative path of earlier ones (`src/main.rs`, around `:1433-1435` in OPUS's numbering; since moved).
  - **Fix:** keep a relative path per matched plan.
  - **Test:** a path inside two nested groups resolves correctly for both.
- **L-31. The modes table may not match the code.** From OPUS. `docs/modes.md:~37` says that under `one-way-conflict`, "alpha deletes a file beta edited" gives "beta's edit comes back to alpha". OPUS says the code does something else. In a one-way mode, carrying beta's edit to alpha would be surprising.
  - **Fix:** write a reconcile test for exactly that case in each of the five modes. Correct whichever of the code and the table is wrong.
  - **Test:** that reconcile test, with one assertion per cell of that table row.
