# ASTRA: Complete Repository Review

Review date: September 23, 2026.

This review found serious defects, including reproduced data loss, stale shared snapshots, and unbounded transfer memory. The highest-priority findings should be addressed before releasing the reviewed working tree.

## Scope and method

The review covered the Rust application and tests, CLI and terminal UI, macOS integration, configuration and service management, local and remote endpoints, shared observers and scanners, reconciliation and ancestor persistence, SSH transport and multiplexing, experimental peering, installation and update paths, release workflows, formal specifications, and Python, Rust, and shell benchmark tooling.

All five requested specialist passes were completed at the primary agent's inherited model and reasoning level: code quality, security, reliability, performance, and test gaps. Separate agents covered the first three areas and test gaps. The session rejected a fifth specialist thread, so the completed code-quality agent performed a separate performance pass. Every specialist received the others' scope. The primary agent cross-checked findings, ran validation, and reproduced several failures independently.

The review examined the working tree as it existed during the review, including pre-existing staged, unstaged, and untracked work. No application, test, configuration, or workflow files were changed. Reproductions and performance probes used `/tmp/autobahn-review.3DcnPF`. This report was written afterward at the user's request. Source line references describe the reviewed files and may drift as those files change.

**Evidence terminology:** “Reproduced” means exercised against the reviewed release build in an isolated scratch fixture. Other findings are supported by code paths and concrete failure scenarios, but were not all triggered dynamically. Performance measurements are local microbenchmarks, not end-to-end improvement claims. P1 denotes high-priority correctness, security, resource, or validation failures; P2 denotes narrower defects or significant improvement opportunities.

The review distinguished new defects from documented retained boundaries. It did not reclassify malicious authenticated agents, forged timestamps, unsupported network-filesystem behavior, ordinary cross-controller ownership overlap, or documented pathname and power-loss publication limits as newly discovered vulnerabilities.

## High-priority findings

### F01 — P1: `resolve --keep` can delete the selected version

**Evidence: Reproduced.** References: [main.rs:2415](src/main.rs#L2415), [main.rs:2518](src/main.rs#L2518), [reconcile.rs:369](src/tree/reconcile.rs#L369).

Resolution deletes the losing side and relies on ordinary reconciliation to restore it. When the selected winner still matches the ancestor, reconciliation instead propagates the newly introduced deletion. The CLI explicitly permits named paths without a recorded conflict, so this is a supported command shape.

The isolated reproduction synchronized `keep.txt` and a sentinel, ran `resolve ... keep.txt --keep alpha --yes`, and synchronized again. Resolution reported “settled 1 of 1 (one version kept),” but `keep.txt` disappeared from both roots. The sentinel remained, excluding the root-emptying guard from the scenario.

Keeping beta also conflicts with strict-alpha and one-way semantics. In `two-way-alpha-strict`, deleting alpha can cause its deletion to win over the selected beta version. One-way modes do not generally import beta's content into alpha. Keeping both versions has a related unchanged-ancestor problem at the original pathname.

**Recommended correction:** Implement explicit resolution semantics that preserve the selected version, coordinate with running workers, and deliberately update provenance and directionality. Merely retiring the loser cannot implement the advertised contract.

**Required coverage:** Every mode and winner, already-agreed files, repeated resolution, stale or missing conflict records, directories, and fan-out, followed by additional synchronization cycles.

### F02 — P1: Manual synchronization bypasses root-overlap protection

**Evidence: Reproduced.** Reference: [main.rs:939](src/main.rs#L939).

Explicit-root `sync` does not run the topology checks used by configured sessions. Neither endpoint construction nor `Session::new` supplies equivalent protection.

In an isolated fixture, synchronizing `tree/source` into `tree` with `one-way-alpha` removed the source directory itself. Its file was copied into the parent destination, but the command was allowed to destroy its own source topology.

**Recommended correction:** Enforce topology validation at a shared construction boundary before opening endpoints or staging content. Apply the same rules to manual synchronization and configured sessions.

**Required coverage:** CLI-level tests for equal roots, nested roots, aliases, and a destination containing its source.

### F03 — P1: A transition can replace a newer shared snapshot with stale content

**Evidence: Reproduced.** References: [local.rs:1505](src/endpoint/local.rs#L1505), [local.rs:1542](src/endpoint/local.rs#L1542), [observer.rs:509](src/endpoint/observer.rs#L509).

`offer_baseline` expects the generation of the snapshot from which a transition's fold was built. The caller instead passes `self.seen_generation` after replacing it with the observer's current, post-transition generation. This defeats the guard that should refuse a stale fold.

The reproduction used two real endpoints sharing one observer. Both initially saw `left/x` and `right/y`. B deleted `right/y` and refreshed the shared scan. A then deleted the unrelated `left/x` using its older snapshot. B's next scan incorrectly reported `right/y` as present although it was absent on disk.

The failure affects supported shared-root topologies. Dirty marks for the unrelated change can already have been consumed, so the next incremental scan has no reason to correct the stale sibling. At minimum, propagation can be suppressed until a full walk; reconciliation can also act on an obsolete value.

**Recommended correction:** Preserve the snapshot's original generation separately from the generation used for subsequent waits, and use the original generation when offering a folded baseline.

**Required coverage:** Two actual endpoints, two paths, an intervening scan, and a transition based on an older lease. Exercise both watcher and polling behavior.

### F04 — P1: Peering does not revoke an already-authorized channel after takeover

**Evidence: Source-backed interleaving.** Reference: [transport/mod.rs:552](src/transport/mod.rs#L552).

A channel's fence changes only when that channel submits a lease and is refused. Accepting a higher term on another channel or process does not invalidate existing authorized channels.

A concrete schedule is: A is accepted at term 1, pauses during staging, B takes over at term 2, and A resumes. A's `StagePush`, `Transition`, or ancestor mutations still see no fence and remain accepted until A submits its next lease request. Large transfers and suspended controllers can make this a substantial window.

This contradicts the experimental peering feature's stated fencing guarantee. It is distinct from the documented exception for manual commands that never participate in leasing.

**Recommended correction:** Retain each channel's accepted leader and term, and validate them against authoritative host-wide state at every mutation. Coordinate these checks with lease changes and mutations across processes.

### F05 — P1: Concurrent lease admissions can accept competing leaders

**Evidence: Source-backed interleaving.** References: [transport/mod.rs:577](src/transport/mod.rs#L577), [peer.rs:54](src/supervisor/peer.rs#L54).

Lease admission is an unlocked read, check, and write. Two processes can read the same old lease, independently accept different leaders at the next term, write their leases, and both return success. A delayed lower-term write can also overwrite a higher term that was accepted after the first writer's read.

Atomic file replacement prevents readers from seeing some partial-file states; it does not make the admission transaction atomic. This defect is separate from F04: checking authorization again does not solve an admission process that can regress or admit conflicting leaders.

**Recommended correction:** Serialize admission across processes, including agent requests, local takeover, renewal, and handoff. Test simultaneous equal-term candidates and delayed lower-term writers.

### F06 — P1: Peering can overwrite a newer ancestor with an older replica

**Evidence: Source-backed control flow.** Reference: [peering.rs:734](src/peering.rs#L734).

`adopt_newer_copy` assumes local generation zero when the checkpoint file is absent, ignoring a valid journal-only ancestor. Small histories can remain entirely in `ancestor.journal` until compaction.

A local journal at generation 10 can therefore be replaced by a replica at generation 9 because the code compares 9 with zero. A stale replica is a realistic state because replication follows local recording and is best effort. Losing acknowledged provenance can turn a deliberate edit or revert into an apparent unchanged value and allow another side's content to overwrite it.

**Recommended correction:** Always use the existing journal-aware `stored_generation` function. Check replica existence using both checkpoint and journal too.

**Required coverage:** A journal-only local ancestor newer than a replica, as well as a journal-only replica that should be adopted.

### F07 — P1: Configured overlap validation uses nonunique display labels as identities

**Evidence: Source-backed configuration path.** Reference: [config.rs:1338](src/config.rs#L1338).

Cross-session comparisons are skipped when `plan.display()` matches. Destinations such as `host:/tree` and `host:/tree/nested` in one group both display as `group@host`. Their writable overlap therefore escapes validation even though they are different sessions with independent ancestors.

The individual sessions' local-to-remote endpoint pairs do not expose the overlap to the earlier within-session check. Reusing the same display label elsewhere also causes ambiguous progress, alert, and UI associations.

**Recommended correction:** Compare actual session identities or plan indices. Keep display labels solely for presentation, and carry a stable typed identity through control, progress, alerts, and UI state.

### F08 — P1: Disabling the final active session leaves previous sessions running under live reload

**Evidence: Source-backed configuration and reload paths.** Reference: [reload.rs:40](src/supervisor/reload.rs#L40).

The disable command accepts and saves a configuration with zero active plans. The live reloader rejects that same configuration as describing no sessions. Its failure behavior is to retain the previous workers, so disabling the final group or host leaves those sessions running. Removing the last group has the same result.

**Recommended correction:** Distinguish startup policy from live-reconfiguration policy. Allow an empty live configuration, stop its workers, and retain the control and reload service so enabling sessions can resume operation.

**Required coverage:** Disable the only group and the only active host, verify that propagation stops, then enable them and verify that operation resumes.

### F09 — P1: The benchmark observer permits unauthenticated arbitrary file writes

**Evidence: Source-backed request path; no network exploit was executed.** References: [observer.rs:48](bench/harness/src/observer.rs#L48), [observer.rs:83](bench/harness/src/observer.rs#L83), [observer.rs:149](bench/harness/src/observer.rs#L149), [smoke.sh:73](bench/smoke.sh#L73).

The benchmark listener binds `0.0.0.0`. A client supplies an unrestricted filesystem path and arbitrary payload through `floor_arm`, then writes it with `floor_write`. There is no authentication or benchmark-root restriction.

Any client able to reach the port can overwrite files writable by the benchmark user, including shell startup or SSH authorization files. The advertised local smoke test starts this listener too. The EC2 benchmark security group restricts non-SSH traffic to fleet members, so this is not a claim that the deployed EC2 listener is publicly reachable from the internet.

**Recommended correction:** Authenticate requests, confine writes to a designated scratch root without symlink escapes, and default local runs to a private socket or appropriately protected loopback endpoint. Bound request sizes and worker counts as additional protection.

### F10 — P1: The A/B benchmark deletes an arbitrary supplied corpus directory

**Evidence: Source-backed command path; destructive execution was not attempted.** References: [ab.sh:49](bench/ab.sh#L49), [ab.sh:106](bench/ab.sh#L106).

`--corpus DIR` assigns the supplied directory directly to `CORPUS`, and every leg executes `rm -rf "$dest" "$state" "$CORPUS"`. There is no ownership check, backup, or destructive-operation warning. Supplying an existing checkout as the corpus destroys it and replaces it with generated data.

**Recommended correction:** Treat supplied input as read-only and copy it into an owned temporary directory, or require and verify an explicitly disposable destination. Only remove directories the harness created.

### F11 — P1: File transfer buffers the complete file before returning its first frame

**Evidence: Reproduced with allocation measurements.** References: [local.rs:1236](src/endpoint/local.rs#L1236), [local.rs:848](src/endpoint/local.rs#L848).

`supply_pull` calls `buffer_delta` synchronously, which reads the complete file or delta into a pending queue. The batch limit applies only afterward. During a cold sync, the empty signature means the delta contains every byte of the file. A changed file with little reusable content has the same behavior.

Requesting just one frame returned only `Begin`, but live heap allocations increased as follows:

| Source file | Additional live heap before the first frame returned |
|---|---:|
| 32 MiB | 33,595,728 bytes |
| 128 MiB | 134,381,904 bytes |

Large files therefore require memory proportional to their size; simultaneous sessions and fan-out multiply that requirement. First-byte latency also includes reading or deltifying the entire file. This contradicts the documented end-to-end streaming model.

**Recommended correction:** Retain a resumable supplier or delta-generator state between pulls, or use a bounded producer queue with backpressure. Preserve begin/end/error framing and alternate-source behavior.

**Required coverage:** Memory and time-to-first-batch measurements across increasing file sizes, with both empty and mismatching base signatures.

### F12 — P1: The formal-specification gate can report success when TLC fails

**Evidence: Reproduced using a controlled Java replacement.** Reference: [spec/check.sh:41](spec/check.sh#L41).

The script pipes TLC output into `grep` without preserving TLC's exit status. It sets `-u`, but not `pipefail`, and the filter explicitly matches `Error`, `violated`, `Deadlock`, and `Temporal`. A failing model check can therefore make the pipeline succeed.

A controlled Java replacement printed `Error: Invariant Safety is violated.` and exited 17. Running `check.sh quick` with that replacement returned exit status zero. Both ordinary and full-spec CI depend on this wrapper. The separate trace-validation branch checks TLC's exit status correctly, but accepts an empty trace directory and reports success.

**Recommended correction:** Preserve TLC's exit status independently of output filtering. Reject an empty trace directory. Add wrapper tests for invariant failures, failures after normal progress output, successful checks, and missing traces.

## Additional correctness, security, and operational findings

### F13 — P2: Disabled sessions are deleted by `clean`

Reference: [main.rs:2839](src/main.rs#L2839).

Cleanup derives retained state from active plans. Disabled groups and hosts are absent from those plans, even though their configuration remains present and enabling is documented as resuming the previous state. After disabling a scratch group, `clean --dry-run` selected its ancestor/session directory, status record, and endpoint lock for removal.

Deleting the ancestor makes re-enabling start without provenance, which can resurrect deletions or create avoidable conflicts. Preserve configured-but-disabled identities separately from active runnable plans. Handle invalid-but-disabled settings conservatively when ownership cannot be established safely.

### F14 — P2: Cached scans bypass entry limits

Reference: [observer.rs:346](src/endpoint/observer.rs#L346).

The shared observer returns a published snapshot before checking the requesting endpoint's `max_entry_count`. The observer key intentionally excludes that per-session limit. A permissive caller can therefore warm the cache for a stricter caller.

This was reproduced: a snapshot containing two entries was accepted by an endpoint limited to one. Apply caller-specific limits on every successful return, including cache hits.

### F15 — P2: Polling fallback serves stale snapshots

Reference: [observer.rs:348](src/endpoint/observer.rs#L348).

The cache-reuse condition does not require an active watcher. When watch establishment fails, or one-shot mode disables watching, ordinary external writes do not advance the generation. Repeated scans can return old data until the 120-second full-scan deadline, despite the documented interval-polling fallback.

This was reproduced through an unwatched endpoint: creating a second file did not change the next scan's file count. Require an active, healthy watcher before treating an unchanged generation as proof of freshness. Polling calls must perform a real walk.

### F16 — P2: Linux watchers ignore re-included descendants

References: [local.rs:233](src/endpoint/local.rs#L233), [scan/mod.rs:875](src/scan/mod.rs#L875).

The scanner descends through an ignored directory when `holds_a_re_inclusion` requires it; watcher registration stops at any ignored directory. Patterns such as `vendor` plus `!vendor/keep.txt` therefore synchronize initial content correctly, but edits beneath the ignored ancestor do not receive Linux watch coverage and wait for a periodic full walk.

Share the scanner's directory traversal policy with watcher registration and add a Linux regression for editing a re-included descendant.

### F17 — P2: Dynamic watch-registration failures are discarded

Reference: [local.rs:401](src/endpoint/local.rs#L401).

Errors while adding watches for newly created or renamed directories are ignored. Reaching the inotify limit after startup leaves partial coverage marked healthy, without the documented polling fallback or registration retries. Freeing capacity later does not automatically repair the unwatched subtree.

Surface extension failures through observer health, report them, use actual polling, and retry rebuilding coverage.

### F18 — P2: Temporary staging exposes private content

References: [local.rs:954](src/endpoint/local.rs#L954), [local.rs:1107](src/endpoint/local.rs#L1107), [local.rs:2296](src/endpoint/local.rs#L2296), [local.rs:2969](src/endpoint/local.rs#L2969).

Staging directories use default creation permissions, and receive/copy temporaries use `File::create`. Under umask 022, these become traversable 0755 directories and readable 0644 files. Final configured permissions, including 0600, are applied only later.

Another Unix user can read transferred content when the staging ancestors are traversable. Beside-root staging can expose content outside a private 0700 synchronization root. Interrupted transfers can leave readable temporaries behind. This requires neither a hostile agent nor a symlink race.

Create staging directories as 0700 and temporary files as 0600 from their first open, using exclusive creation and no-follow behavior. Apply final configured permissions only when publishing. Test incomplete transfers and interruption under a permissive umask.

### F19 — P2: Concurrent publishing can prematurely move shared staged content

Reference: [local.rs:2181](src/endpoint/local.rs#L2181).

The staged-use counter is decremented before a publisher opens the source for copying. With two concurrent users of the same digest, A can decrement 2 to 1 and pause; B can decrement 1 to 0 and rename the staged blob; A then fails to open it and reports missing content.

This produces avoidable retransfers and follow-up cycles. Permanent nonconvergence was not established. Open each reader before allowing a final move, or wait for earlier users to finish before moving the blob.

### F20 — P2: Peering temporary filenames collide between channel threads

Reference: [peering.rs:267](src/peering.rs#L267).

Temporary filenames contain only the destination name and process ID. Multiple workers on one pooled agent can concurrently write the same lease, configuration, or name file. Their writes and renames can collide, and an already-open descriptor can modify an inode after another thread publishes it as the live file.

Use uniquely and exclusively created temporary files, and serialize logically shared state updates. Unique temporary names alone do not fix the lease-admission transaction in F05.

### F21 — P2: Multiple peering groups overwrite one host-wide identity

References: [supervisor/mod.rs:431](src/supervisor/mod.rs#L431), [peering.rs:510](src/peering.rs#L510).

Groups targeting `host:/a` and `host:/b` push different full endpoint specifications into the same host-wide `peering/name` file. The last push wins. Failover derivation includes only groups containing that exact full specification, so the peer can fail over only the matching subset.

Separate machine identity from group/root identity, or persist identities per group and derive all memberships.

### F22 — P2: Followers take over using outdated configuration

Reference: [peer.rs:45](src/supervisor/peer.rs#L45).

The peer derives its configuration before entering a following loop that can remain active indefinitely. That loop reads leases but does not refresh the pushed configuration. Changes to roots, ignores, modes, or membership can therefore be ignored during the next takeover.

Reload and validate pushed state while following, or at least immediately before committing takeover and constructing its supervisor.

### F23 — P2: Containment checks miss filesystem roots and trailing separators

References: [config.rs:697](src/config.rs#L697), [config.rs:1371](src/config.rs#L1371).

The string-prefix containment test strips the outer path and requires the remainder to begin with `/`. For outer `/` and inner `/srv/project`, the remainder is `srv/project`, so containment is missed. Remote roots ending in `/` have the same problem against nested paths.

Separate remote authority from path, normalize safe lexical components and separators, and use component-aware ancestry checks. Cover both within-session and cross-session validation.

### F24 — P2: Updater rollback restores only the CLI binary

References: [update.rs:239](src/update.rs#L239), [update.rs:383](src/update.rs#L383).

The updater refreshes the agent bundle before installing and restarting the CLI. It deletes the previous bundle after that refresh. If restart fails, rollback restores only the previous CLI binary. The recovered controller can subsequently bootstrap agents from the incompatible new bundle and fail version handshakes.

Retain and roll back the binary and agent bundle together. Treat their installation and service restart as one recoverable update operation.

### F25 — P2: Updater success does not establish that the service runs the updated executable

References: [update.rs:791](src/update.rs#L791), [service.rs:98](src/service.rs#L98).

Service installation records `current_exe`, but update defaults to installing under `~/.local/bin`. A service installed from another path can restart its old executable while the updater's running-state check reports success.

Resolve or explicitly retarget the registered service executable, and verify the running version rather than only process presence.

### F26 — P2: Service arguments are not safely serialized

References: [service.rs:100](src/service.rs#L100), [service.rs:427](src/service.rs#L427).

Explicit configuration and state paths are stored without making relative paths absolute. A login service therefore interprets them relative to a different working directory. On Linux, executable paths and arguments are concatenated into `ExecStart` without systemd argument encoding; ordinary spaces can break startup. Environment values also need appropriate encoding.

Resolve paths at installation time and serialize arguments and environment values according to the service manager's syntax.

### F27 — P2: Rejected configuration breaks status visibility

References: [main.rs:3118](src/main.rs#L3118), [main.rs:1971](src/main.rs#L1971), [shop.rs:190](src/shop.rs#L190).

Status and UI commands parse the edited on-disk configuration before reading the still-running session inventory or rejection notice. A syntax error or invalid mode therefore prevents them from showing sessions the supervisor deliberately retained. An already-open terminal UI also retains its original plan list after a successful topology reload.

Expose the supervisor's active inventory independently of the candidate configuration, and refresh UI state from that inventory. Preserve enough information for useful reporting after the supervisor stops.

## Performance opportunities and benchmark validity

### F28 — P2 opportunity: Tiny remote-tree changes still process complete snapshots

References: [transport/mod.rs:795](src/transport/mod.rs#L795), [remote.rs:198](src/endpoint/remote.rs#L198).

For each changed snapshot, the agent serializes the complete new and previous snapshots, hashes both, builds a baseline rsync signature, and scans the entire target encoding. The controller reserializes and signs its baseline, materializes a complete patched encoding, and deserializes and validates a new tree. Network traffic shrinks, but CPU work and transient allocations remain proportional to tree size.

Local microbenchmarks used synthetic trees with 100 files per directory and one changed leaf. The measurements below are medians from the reviewed release library, not end-to-end latency or projected gains:

| Files | Reconciliation | Agent snapshot-delta preparation | Controller decode and validation only |
|---:|---:|---:|---:|
| 10,000 | 0.82 ms | 5.16 ms | 1.22 ms |
| 100,000 | 3.59 ms | 28.02 ms | 8.08 ms |
| 500,000 | 16.20 ms | 132.47 ms | 43.05 ms |

At 500,000 files, a 40.6 MB snapshot produced only a 38.7 KB delta. Controller baseline encoding, signing, patching, and network time are additional costs.

Evaluate structural snapshot deltas that preserve unchanged subtrees and carry metadata changes explicitly. A smaller experiment could cache serialized baselines and signatures, trading idle memory for CPU. The existing implementation deliberately avoids retaining that memory, so compare both approaches using large remote-edited trees, transient memory, and fan-out. A wire change requires a compatibility-epoch update.

### F29 — P2 opportunity: One-shot watcher avoidance does not reach all synchronization paths

References: [supervisor/mod.rs:1565](src/supervisor/mod.rs#L1565), [transport/mod.rs:903](src/transport/mod.rs#L903).

The manual local endpoint path sets `one_shot` according to execution mode. Configured synchronization always sets it false, and remote initialization does not carry one-shot intent. These paths pay for recursive watch registration even when they never wait for a change.

Pass execution intent through configured construction and remote initialization. Measure manual-local, configured-local, and remote startup independently; the existing optimization's benefit cannot be assumed to apply to all three.

### F30 — P2: Remote A/B runs verify the wrong filesystem

References: [ab.sh:112](bench/ab.sh#L112), [ab.sh:129](bench/ab.sh#L129), [ab.sh:138](bench/ab.sh#L138).

`--remote` changes the synchronization destination and agent command, but destination creation and cleanup, manifest checks, the observer process, and the observer address remain local. On a separate host, the cold-sync loop checks the empty local destination for ten minutes, then proceeds without treating timeout as failure. Workload verification observes the same wrong directory. Tested binaries are not provisioned remotely either.

Provision the tested binary and scratch destination remotely and run verification there, or explicitly restrict this script to same-host SSH experiments. Fail when the subject exits or convergence times out.

### F31 — P2: Cold-sync aggregation includes destination-width-contaminated jobs

Reference: [aggregate.py:255](bench/aggregate.py#L255).

Taint detection identifies jobs whose destination count does not match the benchmark cell. Latency and resource aggregation exclude them, but cold-sync aggregation does not. A contaminated job that eventually verifies its digest can still contribute a headline time for the wrong cell.

Apply a consistent exclusion policy using the same run identity. Add a fixture that is digest-verified but has an invalid destination count.

### F32 — P2: Verification scripts invoke removed CLI commands

References: [scripts/mi:97](scripts/mi#L97), [crash.sh:57](bench/verify/crash.sh#L57), [differential.sh:40](bench/verify/differential.sh#L40).

Several scripts under `bench/verify`, as well as the guided CLI tour, still invoke `up` or `up --once`. The reviewed binary rejects `up` as an unrecognized subcommand; this was checked directly. These scripts cannot validate the current CLI, and some ignore launch failure before waiting or reporting later results.

Update them to the current `watch` and `sync` interface and fail immediately when the subject process does not start.

### Additional performance and measurement cautions

- Changed cycles still traverse the full reconciliation tree. Incremental scans and folded transitions also recount the tree, and problem collection walks both snapshots. At 500,000 files, recount and problem collection cost approximately 0.84 ms and 0.97 ms respectively in the probe, substantially less than remote snapshot processing. Prioritize according to these measured relative costs. Simple pointer equality between ancestors and endpoints is not generally sufficient because persisted and remote trees have independent storage. Any shortcut must preserve unresolved conflicts and problems.
- CPU aggregation in `bench/job.py` sums cumulative samples in rounded timestamp buckets without carrying each host's latest value forward. Missing a host in a bucket can make the merged total decrease and distort a window's CPU result. Compute per-host window deltas before summing, or align each host independently.
- `examples/cycle_cost.rs` includes full synchronous ancestor serialization and writing in its reported total, while production sessions use the journal. It remains useful for primitive costs but does not fully represent the current production cycle.

## Test gaps and CI findings

The repository already has meaningful coverage: randomized reconciliation properties, ancestor journal cut and recovery tests, real-agent collision tests, fan-out interleavings, transport failures, and supervisor integration. The major gaps lie between components whose isolated tests encode the intended contract without driving the actual caller that violates it.

| Area | Existing blind spot | Required regression |
|---|---|---|
| Conflict resolution | Tests start with genuine three-way conflicts. | Already-agreed files, repeated resolution, stale conflict records, every winner and mode, and additional cycles after resolution. |
| Shared observation | Property tests mostly use one path, explicitly invalidate writes, and pass the correct saved generation directly to `offer_baseline`. | Two real endpoints, two paths, an intervening scan, and a transition based on an old lease. |
| Peering fencing | The wire test refreshes the old channel's lease after takeover before checking that writes are refused. | Accept a newer leader, then write through the old channel without another lease request. Include takeover during staging and before transition. |
| Lease admission | Replay models read/check/write as a sequential operation. | Concurrent real agent processes with barrier-controlled admission, requiring monotonic terms and at most one admitted leader per term. |
| Ancestor adoption | Checkpoint presence is treated as store presence. | Newer local journal-only history, older replica, and journal-only replica adoption. |
| Entry limits | A single endpoint's cold scan is checked. | Warm a shared observer with a permissive caller, then use a stricter caller; vary order and concurrency. |
| Polling and watchers | Existing no-watcher tests explicitly invalidate tool-mediated writes. | Ordinary external edits without manual invalidation; failed establishment, watch exhaustion, and later recovery. |
| Live configuration | Reload tests cover additions and malformed edits, but not removing all active plans. | Disable the last group or host, verify propagation stops, then enable and resume. |
| Cleanup | No equivalent CLI integration covers disabled state retention. | Sync, disable, clean, enable, and prove ancestor history survives; distinguish disabled from removed groups. |
| Permissions | Tests inspect final published files. | Inspect received and copied staging while incomplete and after interruption under a permissive umask, for all placements. |
| Update recovery | Primitive placement and rollback decisions do not establish an atomic CLI/bundle/service update. | Restart failure after bundle replacement, nondefault registered executable paths, and version-aware recovery checks. |

### F33 — P2: The connection-cut oracle can accept loss of the latest user versions

Reference: [e2e.rs:879](tests/e2e.rs#L879).

The oracle permits either old or new bytes for a modified file, and either absence or new bytes for a created file. It then requires equal trees when there are no conflicts. Both sides reverting to old content, or both sides losing the new file, can satisfy these conditions.

This is a test false-negative, not evidence that the implementation currently performs that rollback. Require each unsuperseded user value to survive somewhere, and require conflict-free recovery to converge to the expected new modification and creation. Extend the fault matrix to deletions, type changes, and restart boundaries around committed ancestor changes.

### F34 — P2: The standing-watch test uses an insufficient synchronization condition

Reference: [e2e.rs:1723](tests/e2e.rs#L1723).

The test stops on the first cycle where beta's scan is not skipped, then immediately requires the latest beta edit on alpha. A requested scan does not establish that this specific kernel event has arrived. A delayed event from an earlier transition can satisfy the stopping condition while the new edit remains unobserved.

The test failed at line 1729 in both the full run and a targeted rerun on macOS. This evidence does not establish permanent production data loss. Use bounded user-visible convergence or the relevant observed generation, and separately test late events and polling behavior with deterministic seams.

### F35 — P2: Benchmark source changes bypass relevant CI

Reference: [ci.yml:24](.github/workflows/ci.yml#L24).

CI ignores all `bench/**`, including executable Rust, Python, and shell sources. The benchmark harness is a separate package and is not covered by the root package's test command. No workflow invokes the existing benchmark smoke suite.

Add a gate for benchmark source changes that builds the harness and runs relevant tests. Do not apply report-only exclusions to executable benchmark code. Address the observer security and scratch-directory safety findings before running that tooling on a general-purpose CI or developer machine.

### F36 — P2: Tray code is built but its feature-gated tests are not executed in CI

References: [ci.yml:109](.github/workflows/ci.yml#L109), [build.sh:23](apps/macos/build.sh#L23), [tray.rs:1295](src/tray.rs#L1295).

The macOS workflow runs default-feature tests and then builds the app with the tray feature. This does not execute the tray-specific test, and menu actions and health transitions lack comparable behavioral coverage.

Run feature-enabled macOS tests and add coverage for important menu actions and state transitions.

## Additional security hardening

These issues have narrower prerequisites or weaker independent security impact than the benchmark listener and temporary-file permission findings.

- **Terminal control injection and unsafe suggested commands:** Conflict filenames are printed without escaping control characters, and the pager preserves escape sequences. A filename containing ESC or carriage return can spoof or alter terminal output without a hostile agent. Suggested shell commands also interpolate raw filenames. Escape data fields before applying application-owned ANSI formatting and shell-quote command arguments. References: [main.rs:1812](src/main.rs#L1812), [main.rs:1858](src/main.rs#L1858), [pager.rs:143](src/pager.rs#L143).
- **Installer verification gaps:** The shell installer extracts the agent archive without checking its `SHA256SUMS` entry. A checksum-download failure also permits an unverified CLI install. Verify every artifact and require explicit opt-in for legacy checksum-free releases. This improves integrity against corruption or an independently replaced asset; it does not independently defend against complete compromise of the trusted publisher. Reference: [install.sh:135](scripts/install.sh#L135).
- **Remote cleanup constructs shell syntax from filenames:** Agent cleanup accepts names with the `autobahn-` prefix and joins them into a remote `rm` command without adequate quoting. Shell metacharacters can become executable syntax. The usual prerequisite is permission to create files in the remote principal's agent directory, so this is primarily hardening unless that directory admits less-trusted writers. Validate the version-name grammar and quote operands. Reference: [install.rs:299](src/transport/install.rs#L299).

A targeted credential-pattern search did not identify matching embedded credential material. This was not a historical secret audit or a dependency CVE assessment.

## Validation results

Validation used Rust 1.91.1 on macOS with a separate build directory at `/tmp/autobahn-review.3DcnPF/target`, avoiding replacement of the repository's normal release binary.

| Check | Result |
|---|---|
| Library unit tests | 370 passed |
| Binary unit tests | 33 passed |
| End-to-end tests | 28 passed, 1 failed |
| Supervisor integration tests | 38 passed |
| Spec harness test cases | 5 reported success; optional TLC comparisons did not execute TLC |
| Total reported test results | 474 passed, 1 failed |
| Targeted rerun of standing-watch failure | Failed again at `tests/e2e.rs:1729` |
| `cargo fmt --check` | Passed |
| CI-equivalent Clippy with warnings denied | Failed on three diagnostics |
| Tray-feature compilation | Passed |
| Separate benchmark-harness compilation | Passed |
| Shell and Python syntax checks | Passed |
| Documentation tests | Passed; zero cases |
| Actual TLC model checking | Not run: Java runtime unavailable |

The Clippy diagnostics were `large_enum_variant` for `Receiving` at [local.rs:1588](src/endpoint/local.rs#L1588), and `type_complexity` at [remote.rs:773](src/endpoint/remote.rs#L773) and line 786. The largest enum variant was reported as at least 2,064 bytes.

The initial full test command stopped after the end-to-end failure. Supervisor and spec suites were then run separately. The standing-watch failure appears consistent with F34's synchronization assumption; it must not be described as independently established permanent production data loss.

The following findings received direct isolated confirmation: selected-version deletion during resolution; manual nested-root source removal; stale shared-baseline publication; warm-cache entry-limit bypass; stale scans without watching; disabled-state selection by cleanup dry run; whole-file transfer buffering; and false-success behavior in the spec wrapper. The obsolete `up` command was also checked against the built CLI.

No real multi-host failover, Linux/FreeBSD-specific runtime behavior, graphical UI interaction, dependency CVE audit, or historical secret audit was performed. The review does not establish that every other path is defect-free. Experimental peering findings are distinguished from ordinary synchronization, but its experimental status does not make the documented guarantees hold.

## Review artifacts

Diagnostic artifacts were left under `/tmp/autobahn-review.3DcnPF` and may disappear when temporary storage is cleaned:

- `probe.rs`: shared-observer, polling, and entry-limit reproduction.
- `resolve/`: isolated CLI resolution fixture and synchronization state.
- `nested/`: isolated manual nested-root fixture.
- `fake-tools/java`: controlled failing Java replacement for the spec-wrapper check.
- `performance/memory.rs`: live-allocation probe for the first supply frame.
- `performance/probe.rs`: in-memory tree and snapshot-processing measurements.

## Code-quality assessment and remediation order

The recurring design problem is duplicated policy across entry points. Topology validation, session identity, explicit conflict resolution, and active configuration have multiple inconsistent implementations. Shared observation and peering also depend on contracts that are tested below the boundary where the caller violates them.

First address the reproduced data-loss and stale-observation failures, then peering authorization, admission, and ancestor adoption. Remove the benchmark listener's unrestricted write capability and the corpus-deletion hazard before using those tools broadly. Bound transfer memory and repair the false-green specification gate. Add the cross-component regressions alongside those fixes, then address the narrower operational defects and measured performance opportunities.

Centralizing validation and stable identity, defining explicit resolution behavior, separating snapshot provenance from wait generations, and exposing the supervisor's actual active inventory would prevent several of these defects from recurring.
