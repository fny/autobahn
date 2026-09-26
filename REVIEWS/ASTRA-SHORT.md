# Review Findings

## P1 Findings

### 1. `resolve --keep` can delete the selected version (Reproduced)
`src/main.rs:2415`

Resolution deletes the losing side and relies on ordinary reconciliation to restore it. When the winner still matches the ancestor, reconciliation instead propagates that deletion.

I synchronized `keep.txt`, ran `resolve ... keep.txt --keep alpha --yes`, then synchronized again. The command reported "one version kept," but the file disappeared from both roots. Keeping beta also conflicts with strict-alpha and one-way semantics.

**Fix:** Implement explicit resolution semantics that preserve the selected version and deliberately update provenance. Test every mode, repeated resolution, and files without recorded conflicts.

### 2. Manual synchronization bypasses root-overlap protection (Reproduced)
`src/main.rs:939`

Explicit-root sync does not run the topology checks used by configured sessions. In a scratch fixture, synchronizing `tree/source` into `tree` with one-way-alpha removed the source directory itself.

**Fix:** Enforce topology validation at a shared construction boundary, before opening endpoints or staging content.

### 3. A transition can replace a newer shared snapshot with stale content (Reproduced)
`src/endpoint/local.rs:1505`, `src/endpoint/local.rs:1542`

A transition folds its old snapshot, but labels that fold with the observer's new generation. This defeats the stale-baseline guard.

I reproduced this with two endpoints: B deleted `right/y` and refreshed the shared scan; A then deleted unrelated `left/x`. B's next scan incorrectly reported `right/y` as present although it was absent on disk. This affects supported shared-root topologies.

**Fix:** Preserve the snapshot's original generation separately from the generation used for subsequent waits.

### 4. Peering does not revoke an already-authorized channel after takeover
`src/transport/mod.rs:552`

A channel's fence changes only when that channel submits a lease and is refused. If A is accepted at term 1, B takes over at term 2, and A resumes a paused transfer, A's writes remain accepted until its next lease request.

**Fix:** Validate the channel's accepted leader/term against authoritative host-wide state at every mutation, coordinated with lease changes.

### 5. Concurrent lease admissions can accept competing leaders
`src/transport/mod.rs:577`

Lease admission is an unlocked read/check/write. Two processes can read the same old lease, both accept different leaders, and both return success. A delayed lower-term write can also overwrite a higher term. Atomic file replacement does not make this operation atomic.

**Fix:** Serialize admission across processes, including agent requests, local takeover, renewal, and handoff.

### 6. Peering can overwrite a newer ancestor with an older replica
`src/peering.rs:734`

`adopt_newer_copy` assumes local generation zero when the checkpoint file is absent, ignoring a valid journal-only ancestor. Local generation 10 can therefore be replaced by replica generation 9. Losing that history can turn a deliberate edit or revert into an apparent unchanged value.

**Fix:** Always use the existing journal-aware `stored_generation` function. Check replica existence using both checkpoint and journal too.

### 7. Configured overlap validation uses nonunique display labels as identities
`src/config.rs:1338`

Comparisons are skipped when `plan.display()` matches. Two destinations such as `host:/tree` and `host:/tree/nested` in one group share `group@host`, so their writable overlap escapes validation.

**Fix:** Compare actual session identities or plan indices. Display-label reuse also causes ambiguous progress, alert, and UI associations.

### 8. Disabling the final active session leaves the previous sessions running under live reload
`src/supervisor/reload.rs:40`

The disable command accepts and saves a configuration with zero active plans. The reloader rejects that configuration and retains the previous workers. Removing the last group has the same result.

**Fix:** Allow an empty live configuration, stop its workers, and retain the control/reload service so enabling sessions can resume operation.

### 9. The benchmark observer permits unauthenticated arbitrary file writes
`bench/harness/src/observer.rs:48`, `bench/harness/src/observer.rs:83`

The listener binds `0.0.0.0`. A client supplies an unrestricted path and payload through `floor_arm`, then writes it with `floor_write`. Any client able to reach the port can overwrite files writable by the benchmark user. The local smoke test starts this listener too.

**Fix:** Authenticate requests, confine writes to a designated scratch root, and default local runs to a private socket or appropriately protected loopback endpoint.

### 10. The A/B benchmark deletes an arbitrary supplied corpus directory
`bench/ab.sh:106`

`--corpus DIR` flows directly into `rm -rf "$CORPUS"` on every leg. There is no ownership check, backup, or destructive-operation warning. Passing an existing checkout destroys it and replaces it with generated data.

**Fix:** Copy supplied input into an owned temporary directory, or require and verify an explicitly disposable destination.

### 11. File transfer buffers the complete file before returning its first frame (Reproduced)
`src/endpoint/local.rs:1236`, `src/endpoint/local.rs:848`

The batch limit applies after `buffer_delta` has read the entire file. Requesting just one frame increased live heap by approximately 32 MiB for a 32 MiB file and 128 MiB for a 128 MiB file. Large files and concurrent sessions can exhaust memory, despite the documented streaming model.

**Fix:** Use a resumable supplier or a bounded producer queue with backpressure.

### 12. The formal-specification gate can report success when TLC fails (Reproduced)
`spec/check.sh:41`

The script returns grep's pipeline status, and its filter explicitly matches invariant failures. A controlled Java replacement that printed an invariant violation and exited 17 made `check.sh quick` exit 0.

**Fix:** Preserve TLC's exit status independently of output filtering. Also reject an empty trace directory instead of reporting successful validation.

## P2 Findings

| Finding | Impact and recommended correction |
|---|---|
| Disabled sessions are deleted by `clean`. `src/main.rs:2839` | Cleanup derives retained state from active plans. I confirmed that disabling a group makes `clean --dry-run` select its ancestor/session state for deletion. Preserve configured-but-disabled identities separately. |
| Cached scans bypass entry limits. `src/endpoint/observer.rs:346` | Reproduced: a warm shared snapshot with two entries was accepted by an endpoint limited to one. Apply caller-specific limits on cache hits as well as fresh scans. |
| Polling fallback serves stale snapshots. `src/endpoint/observer.rs:348` | Reproduced without a watcher: creating a file did not change the next scan. Cache reuse can persist until the 120-second full-scan deadline. Require an active, healthy watcher before treating an unchanged generation as proof of freshness. |
| Linux watchers ignore re-included descendants. `src/endpoint/local.rs:233` | `vendor` plus `!vendor/keep.txt` is scanned correctly but its directory is not watched. Share the scanner's re-inclusion traversal rules. |
| Dynamic watch-registration failures are discarded. `src/endpoint/local.rs:401` | Hitting an inotify limit after startup leaves partial coverage marked healthy, without recovery. Surface failure, poll, and retry registration. |
| Temporary staging exposes private content. `src/endpoint/local.rs:954` | With umask 022, receive/copy temporaries are readable as 0644 before final 0600 permissions are applied. Beside-root staging can expose content outside a private root. Create staging directories as 0700 and files as 0600 from their first open. |
| Concurrent publishing can prematurely move shared staged content. `src/endpoint/local.rs:2181` | Usage counts decrement before readers open the file. The last publisher can rename it before an earlier reader opens it, causing unnecessary missing-content errors and retransfers. Establish readers before allowing the final move. |
| Peering temporary filenames collide between channel threads. `src/peering.rs:267` | Temporary names contain only the destination name and PID. Concurrent writes can interfere with each other's rename or published inode. Use unique exclusive temporaries and serialize shared-state updates. |
| Multiple peering groups overwrite one host-wide identity. `src/supervisor/mod.rs:431` | Groups targeting `host:/a` and `host:/b` overwrite the same name file; failover membership then includes only the winning identity. Separate machine identity from group/root identity. |
| Followers take over using outdated configuration. `src/supervisor/peer.rs:45` | Configuration is captured before an indefinitely long following loop. Later pushes are not used for the next takeover. Reload and validate immediately before taking leadership. |
| Containment misses `/` and trailing separators. `src/config.rs:697` | String-prefix checks miss filesystem-root containment and remote roots ending in `/`. Use normalized, component-aware path comparisons. |
| Updater rollback restores only the CLI binary. `src/update.rs:239`, `src/update.rs:383` | The previous agent bundle is already removed. A rolled-back controller can subsequently bootstrap incompatible agents. Retain and roll back the binary and bundle together. |
| Updater success does not establish that the service runs the updated executable. `src/update.rs:791`, `src/service.rs:98` | Installation records `current_exe`, but update defaults to `~/.local/bin`. A service installed elsewhere can restart its old binary and pass the "running" check. Resolve or explicitly retarget the registered executable and verify its version. |
| Service arguments are not safely serialized. `src/service.rs:100`, `src/service.rs:427` | Relative config/state paths retain the caller's working-directory dependency; Linux arguments containing spaces are concatenated without systemd quoting. Resolve paths at installation and encode arguments correctly. |
| Rejected configuration breaks status visibility. `src/main.rs:3118` | Status/UI commands parse the invalid edited file before reading the still-running inventory or rejection notice. An existing UI also retains its original plan list. Read the supervisor's active inventory independently of the candidate configuration. |

## Performance

The largest measured performance opportunity, after fixing transfer memory, is remote snapshot processing. A one-file edit still serializes and processes complete snapshots on both ends.

| Synthetic tree | Reconciliation | Agent snapshot-delta preparation | Controller decode and validation |
|---:|---:|---:|---:|
| 10,000 files | 0.82 ms | 5.16 ms | 1.22 ms |
| 100,000 files | 3.59 ms | 28.02 ms | 8.08 ms |
| 500,000 files | 16.20 ms | 132.47 ms | 43.05 ms |

These are local microbenchmark medians, not end-to-end latency estimates. At 500,000 files, a 40.6 MB snapshot produced only a 38.7 KB delta, yet preparation still processed the whole tree. Snapshot preparation (`src/transport/mod.rs:795`) and reassembly (`src/endpoint/remote.rs:198`) are the first places I would investigate structural deltas or carefully measured baseline caching.

### Other performance and measurement issues

- Configured and remote one-shot syncs still register recursive watchers because execution intent is not passed through. `src/supervisor/mod.rs:1565`
- `bench/ab.sh --remote` changes the synchronization destination but continues running verification against the local destination. Separate-host results are consequently invalid. `bench/ab.sh:129`
- Cold-sync aggregation includes destination-width-contaminated runs that latency/resource aggregation rejects. Apply the same exclusion policy consistently. `bench/aggregate.py:255`
- Several verification scripts and the guided CLI tour invoke removed `up` commands. I confirmed the current binary rejects them. `scripts/mi:97`

## Test Coverage Gaps

The test suite is substantial, but its most consequential gaps are at component boundaries:

| Gap | Regression needed |
|---|---|
| Resolution tests begin with genuine conflicts. | Already-agreed files, repeated resolution, stale conflict records, every winner and mode, followed by additional sync cycles. |
| Observer property tests pass the correct generation directly and largely use one path. | Two real endpoints, two paths, an intervening scan, and a transition from an older lease. |
| Peering tests refresh the old channel's lease before checking its fence. | Takeover followed by an old-channel write without another lease request; concurrent admission through real production handlers. |
| Entry-limit and permission tests inspect cold scans or final files. | Shared warm-cache callers with different limits; temporary permissions during interrupted transfers. |
| Reload tests do not cover disabling the final session or cleanup afterward. | Disable → verify propagation stops → clean → enable → verify history survives. |
| The crash-cut oracle permits both sides to retain old content or lose a new file. | Require an unsuperseded user value to survive somewhere, and exact desired convergence when recovery is conflict-free. `tests/e2e.rs:879` |

CI also ignores all `bench/**` changes, including executable sources, and does not run the separate benchmark package or smoke suite. Tray code is built on macOS but its feature-gated tests are not executed. `.github/workflows/ci.yml:24`

## Lower-Priority Security Hardening

Escape terminal controls in displayed filenames and quote suggested shell commands; verify the installer's agent archive checksum; and validate/quote remote agent filenames before inserting them into cleanup shell commands. These have narrower prerequisites than the benchmark listener and staging-permission findings.

## Validation Results

- 474 tests reported passing; one end-to-end test failed twice. The failure is at `tests/e2e.rs:1729`. Its stopping condition equates "scan requested" with "this edit observed," so the evidence currently points to a test synchronization flaw, not proven permanent production data loss.
- Formatting, tray-feature compilation, benchmark-harness compilation, and shell/Python syntax checks passed.
- CI-equivalent Clippy failed on `large_enum_variant` at `src/endpoint/local.rs:1588` and two `type_complexity` diagnostics at `src/endpoint/remote.rs:773`.
- Actual TLC checking was unavailable because Java is absent. The optional TLC comparison tests returned without executing TLC.
- Linux/FreeBSD-specific behavior, real multi-host failover, GUI interactions, dependency CVEs, and historical secrets were not dynamically audited.

## Summary

The recurring code-quality problem is duplicated policy across entry points: topology validation, session identity, resolution semantics, and active configuration have multiple inconsistent implementations. Centralizing those contracts would prevent several findings above from recurring.
