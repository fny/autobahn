# Invariants

This reference states the system’s guarantees, enforcement points, tests, and limits. Reviewers can challenge each statement with schedules, crashes, filesystem behavior, or protocol input.

References use paths and symbols rather than line numbers. Mutation-checked tests must fail when their enforcing code is deliberately broken. [RETAINED](./accepted-risks.md) explains unresolved risks and reasons to revisit them.

## I1. A scan describes the tree at a generation

**Statement.** A snapshot cannot be served as current after an announced change that it did not observe. Announcements include delivered watcher events and pre-write `invalidate` calls.

**Enforcement.** In `src/endpoint/observer.rs`, `walk` reads the generation before traversal. A mid-walk event makes publication stale. `scan_inner` serves a publication only while its generation matches the current signal.

`ChangeWatcher::new` in `src/endpoint/local.rs` records paths before advancing the signal. `RootObserver::invalidate` uses `ChangeWatcher::mark_pending` in the same order.

Transitions announce paths before and after writes. The second announcement invalidates a scan that consumed the first marks but read old bytes. This also works without kernel events under polling. Finding I1-A showed that pre-write announcement alone was insufficient.

`offer_baseline` refuses generations older than the standing baseline. A transition offers its fold at its lease generation, not the current observer generation (H-3).

A session advances past its own announcements only if no other change intervened. Foreign writes still wake it.

Kernel overflow, the dirty-path cap, or an inexpressible path makes `ChangeWatcher::take_dirty` return `None` and require a full walk.

**Tests and source references:**

- `a_mid_scan_change_is_never_served_as_current`
- `an_offered_baseline_cannot_hide_an_invalidated_change`
- `every_interleaving_scans_the_truth`
- `src/endpoint/observer.rs`
- `a_scan_racing_a_transition_cannot_outlive_the_writes`
- `src/endpoint/local.rs`
- `a_stale_lease_cannot_roll_back_a_sharing_sessions_deletion_when_watched`
- `_when_polled`
- `a_foreign_change_during_a_transition_still_wakes_the_session`
- `src/endpoint/local.rs`
- `scan`
- `transition`

The transition race test suppresses watchers and pauses a real transition between announcement and writes. The stale-lease tests exercise real endpoints sharing an observer.

Mutation checks cover path recording, stale-offer refusal, the serve gate, and post-write announcement. Randomized sweeps found two defects. Independent review found I1-A. All three are fixed.

**Boundary.** Unannounced writes remain invisible until OS event delivery or an audit. Network filesystems can omit events entirely. See [accepted-risks §3](./accepted-risks.md#3-network-filesystems-beyond-warn-and-document).

## I2. The ancestor records only acknowledged agreement

**Statement.** Ancestor entries represent completed cycles acknowledged by both endpoints. An interrupted cycle can remove provenance but cannot invent agreement. Unknown provenance normally becomes a conflict. Interrupted deletions can instead restore content.

**Enforcement.** `src/session/ancestor.rs` and `src/session/mod.rs` append achieved records only after both transition responses. `digest(generation, payload)` protects journal headers and payloads against undetected corruption.

After staging and before transitions, `intend` appends both endpoints’ affected paths. A remote endpoint or `durability = "power"` requires the append to sync before writes.

Remote transitions can persist independently of local page cache. Finding I2-B required durable intent without assuming local writeback ordering.

Default local-local durability retains a power-loss ordering risk on the same storage stack. Power durability closes that intent-ordering gap.

On open, unresolved intent removes affected ancestor paths and immediately persists the removal through `Session::with_lock`.

Normalization uses temporary-file write, fsync, and rename. It retains trailing unresolved intent and removes stray normalization files on open.

An undecodable journal fails closed. `Session::rebuild_or_halt` rebuilds only if reconciliation without history produces no changes or conflicts. It preserves the unreadable store. Otherwise, `SafetyHalt::AncestorUnreadable` halts.

A second damage event produces `AncestorDamagedAgain` until reset. The `unreadable_ancestor` e2e tests cover this behavior. `decode_record` uses checkpoint encoding, pinned by `the_encodings_this_format_promises_are_unchanged`.

**Tests and source references:**

- `every_journal_cut_reopens_and_stays_appendable`
- `intents_surface_until_a_cycle_completes`
- `an_unresolved_intent_survives_normalization`
- `a_flipped_generation_is_corruption_not_a_skip`
- `a_flipped_checkpoint_generation_is_corruption`
- `an_interrupted_normalization_cannot_lose_acknowledgments`
- `a_torn_final_record_is_discarded`
- `an_interrupted_reset_cannot_fabricate_an_ancestor`
- `corrupt_ancestor_is_an_error_not_a_reset`
- `a_durable_intent_syncs_whatever_the_configured_durability`
- `a_remote_endpoint_makes_the_intent_durable`
- `a_failed_compaction_fails_no_cycle_and_never_clears_the_journal`
- `a_crash_between_transition_and_record_ends_in_conflict_not_overwrite`
- `intend`

The journal-cut test examines every byte boundary. Disabling `intend` makes the end-to-end crash test overwrite a deliberate revert and fail.

**Boundary.** Lost provenance can turn a normal propagation into a conflict. An absent-versus-present state after an interrupted deletion can instead restore the file (I2-C).

Under default durability, power loss can remove an unsynced achieved-record tail. Where intent was durable, recovery treats affected paths as unknown.

A remote acknowledgement means a decoded response from the authenticated agent. It does not independently prove remote disk state (I2-A).

## I3. A digest names exactly its bytes

**Statement.** Content addressed by digest must match that digest at use. Unverified incoming content cannot enter staging or publish to a tree.

**Enforcement.** In `src/endpoint/local.rs`, `DigestingWriter` hashes incoming temporary files. `finish_receive` publishes to the digest-named staging path only after a match.

`staged_content_matches` rereads survivors before reuse. The final move path checks that the staged entry remains a regular file with matching bytes. Otherwise, the copy path hashes what it moves. Finding I3-A closed the unchecked rename path.

Supply also checks alternate paths before serving content under a shared digest.

**Tests and source references:**

- `corrupted_frames_are_discarded_never_published`
- `a_corrupt_staged_survivor_is_retransferred_not_trusted`
- `a_truncated_staging_transfer_recovers_cleanly`
- `supply_recovers_from_an_alternate_path_sharing_the_digest`
- `published_content_moves_out_of_staging_on_its_last_use`
- `tampered_staged_content_is_never_published_by_the_move_path`
- `a_staged_symlink_is_never_published_by_the_move_path`

Disabling the receive digest gate causes the corrupt-frame test to publish corruption and fail.

**Boundary.** Snapshot digests can be reused when metadata matches. Deliberately restored metadata can conceal a rewrite. See [accepted-risks §5](./accepted-risks.md#5-forged-timestamps-beyond-the-verify-verb).

`a_same_granule_rewrite_is_reread_not_trusted` covers accidental timestamp-granule collisions. `a_verified_scan_sees_what_metadata_hides` covers explicit content verification.

## I4. Transitions validate against their lease

**Statement.** A transition validates against the snapshot used for its reconciliation, called its lease. A conflicting live state causes a reported refusal.

Refusals that establish stale observation invalidate the baseline. Predictable refusals, such as existing name collisions, do not force repeated full walks.

**Enforcement.** `Transitioner` in `src/endpoint/local.rs` uses `last_snapshot`. `Problem.disagreement` selects when `distrust_baseline` requires a new scan.

`publish_rename` uses Linux `RENAME_NOREPLACE` or macOS `RENAME_EXCL` to protect creations from concurrent arrivals.

**Tests and source references:**

- `a_creation_rename_refuses_to_replace`
- `a_retargeted_symbolic_link_is_not_removed`
- `transition_folds_achieved_results_into_the_snapshot`

I5’s lifecycle harness also exercises transitions.

**Boundary.** Replacements and removals use pathname checks followed by separate operations on every platform. Creation has the same gap outside atomic no-replace platforms and in unsupported fallbacks.

Linux and macOS provide atomic no-replace rename. FreeBSD and other BSDs do not. Findings I4-A and I4-C clarify this scope. See [accepted-risks §2](./accepted-risks.md#2-pathname-toctou-outside-linux-creations).

Validation compares metadata, not live content. Same-length rewrites with restored metadata can pass (I4-B, accepted-risks §5).

## I5. No crash leaves torn bytes

**Statement.** At staging, transition, and remote frame boundaries, a process crash leaves each file equal to a legitimate version.

After the fault ends, recovery reaches quiescence. Conflicts remain limited to paths involved in the interrupted cycle. Other paths agree. Staging-only crashes require conflict-free recovery because intent begins at transitions.

**Enforcement.** This property combines I2’s intent records, I3’s content checks, and I4’s validation and rename publication.

**Tests and source references:**

- `src/session/mod.rs`
- `a_crash_before_staging_recovers_cleanly`
- `a_truncated_staging_transfer_recovers_cleanly`
- `a_source_that_dies_mid_supply_recovers_cleanly`
- `a_crash_before_any_transition_recovers_safely`
- `a_partially_applied_transition_recovers_safely`
- `a_crash_after_transition_before_record_recovers_safely`
- `tests/e2e.rs`
- `every_cut_connection_recovers_to_a_safe_tree`

The C2 harness covers local lifecycle faults. The C3 sweep cuts real agent connections in both directions. It engaged 29 cut points. Three recovered with conflicts, all inside the remote transition-to-record interval.

**Boundary.** This is a process-crash guarantee. Publication does not sync file data, so power loss can expose unsynced bytes under a renamed path (I5-A). I2 and I10 protect ancestor recovery. Later content checks and transfers repair the tree.

## I6. One writer per tree region

**Statement.** One configuration cannot contain nested writable local roots. Identical shared roots remain legal, with a warning, for fan-out, star, and relay topologies.

Processes sharing a controller user’s default state root exclude identical endpoint pairs, including across `--state-root` or `--state-dir` overrides. Supervisor and session state have exclusive locks.

Endpoint resolution is frozen through validation, locking, and construction for supervisor and manual `sync` paths. I6-C fixed a second resolution in manual sync.

**Enforcement.** `src/config.rs` checks canonical containment. `src/supervisor/mod.rs` freezes endpoint resolution. `EndpointPairLock` and `SessionLock` in `src/session/mod.rs` enforce ownership.

**Tests and source references:**

- `overlapping_roots_within_a_session_are_rejected`
- `nested_writable_endpoints_across_sessions_are_rejected`
- `aliased_paths_are_detected_as_duplicates`
- `the_pair_lock_is_unordered_and_pair_scoped`
- `a_state_directory_admits_only_one_session_at_a_time`
- `a_second_supervisor_over_the_same_state_root_is_refused`
- `a_retargeted_root_is_refused_rather_than_bound_to_stale_state`

**Boundary.** Different overlapping configurations in separate processes remain possible. Pair locks do not cross machines, users, or `AUTOBAHN_HOME` roots (I6-B). Canonical paths also miss physical aliases such as bind mounts (I6-D). See [accepted-risks §4](./accepted-risks.md#4-cross-process-overlapping-configurations).

Identical-root sharing relies on generations and lease validation, rather than exclusive root ownership.

## I7. Reconciliation never destroys silently

**Statement.** Two-way conflict mode reports competing non-deletion changes and restores an edit against a deletion. One-way conflict mode preserves beta-only changes without copying them to alpha.

Alpha-winning policies intentionally discard some competing changes. See [Modes](../modes.md).

Exactly one empty or absent root causes a halt if the ancestor has at least two entries. Emptiness counts synchronizable content, so excluded entries do not prevent this guard.

A missing alpha and a vanished recorded mount have separate safeguards.

`guard_dir_deletes_over = N` protects directories with at least N recursive ancestor entries. Emptying creates a conflict. Two-way modes also restore a missing protected directory from an unchanged peer.

One-way modes retain their direction for missing directories. The optional guard is off by default and replaces `two-way-paranoid`.

**Enforcement.** `src/tree/reconcile.rs` implements `Policy::guard_dir_deletes_over`, `large_in_ancestor`, and the directory rules. `src/session/mod.rs` implements `one_side_emptied_root`, mount tracking, root refusal, and `SafetyHalt`.

**Tests and source references:**

- `conflict_free_reconciliation_converges`
- `agreement_emits_nothing`
- `unsynchronizable_content_never_travels`
- `mutual_exclusion_preserves_the_ancestor`
- `concurrent_divergent_edits_conflict_in_safe_mode_and_resolve_in_resolved_mode`
- `deletion_versus_modification_repropagates_content`
- `one_way_safe_preserves_beta_creations`
- `one_way_modes_never_touch_alpha`
- `content_leaving_tracked_scope_never_reads_as_deletion`
- `emptied_root_detection`
- `a_root_emptied_down_to_an_ignored_entry_halts_in_every_mode`
- `no_change_is_lost_silently`
- `paranoid_treats_an_emptied_large_directory_as_a_conflict`
- `paranoid_treats_a_directory_emptied_down_to_an_ignored_entry_as_emptied`
- `paranoid_restores_a_large_directory_gone_from_one_side`
- `paranoid_lets_the_emptying_side_win_once_the_full_copy_is_retired`

The reconciliation properties and ignored-entry root guard include mutation checks.

**Boundary.** Unobserved mounts below the optional count threshold can escape protection. See [accepted-risks §1](./accepted-risks.md#1-mounts-that-were-never-observed-mounted).

A root containing one empty directory is not empty because that directory itself synchronizes. Distinguishing that shape from intentional clearing requires a different threshold policy.

A forged ancestor can invalidate the policy’s provenance assumptions (I7-B).

## I8. Both ends speak the same safety semantics

**Statement.** Controller and agent package versions and compatibility epochs must match exactly. Mismatch errors name both versions.

`COMPATIBILITY_EPOCH` in `src/protocol.rs` is a maintained convention. It detects semantic incompatibility only when developers increment it for relevant changes (I8-A). This is a release requirement.

**Tests:**

- `a_stale_epoch_fails_the_handshake`
- `a_failed_handshake_reaps_the_spawned_process`

## I9. The wire is hostile until proven otherwise

**Statement.** Received lengths, flags, and compressed sizes require validation before they control allocation.

Frames must fit `MAXIMUM_FRAME_SIZE`: 64 MiB plus the compressed-frame header where applicable. Declared decompressed size is checked before allocation. Invalid flags and decompression bombs fail.

Multi-frame messages use “more follows” markers and cap reassembly at `MAXIMUM_MESSAGE_SIZE`, 4 GiB. The reader rejects a frame that would exceed the cap before appending it. Allocation follows received bytes.

Scan-delta output also caps at 4 GiB, with at most 8 MiB allocated initially. With a baseline, block size must lie between `MINIMUM_BLOCK_SIZE` and `MAXIMUM_BLOCK_SIZE`, 1 KiB to 64 KiB.

Each delta operation must fit the declared output length before application. The result must match its digest and decode to a valid hierarchy.

`Signature::validate` checks block sizes at use. `Signature::validate_for_base` additionally limits hash count by base length, but production receivers lack that base length and do not call it.

**Enforcement.** `read_chunk` and `read_frame` in `src/transport/mod.rs` check frames and reassembly. `check_delta_header`, `apply_delta_ops`, and `check_hierarchy` in `src/endpoint/remote.rs` check scan deltas. Signature validation lives in `src/rsync/mod.rs`.

**Tests and source references:**

- `an_oversized_length_prefix_is_refused_not_allocated`
- `oversized_frames_are_rejected_on_receive`
- `a_decompression_bomb_is_refused`
- `an_unknown_frame_flag_is_refused`
- `a_message_larger_than_a_frame_is_split_and_reassembled`
- `src/transport/mod.rs`
- `a_scan_delta_declaring_an_impossible_length_is_refused`
- `a_scan_delta_with_an_out_of_range_block_size_is_refused`
- `delta_operations_that_expand_past_the_declared_length_are_refused_before_applying`
- `a_scan_with_an_invalid_hierarchy_is_refused`
- `src/endpoint/remote.rs`
- `validation_bounds_the_block_size_and_the_hash_count`
- `src/rsync/mod.rs`

**Boundary.** Outgoing messages are serialized before their size check, so the cap does not protect sender memory (I9-A). Decoded structures have no explicit depth cap beyond input length (I9-B). Hostile-peer availability remains outside the broader guarantee.

## I10. Persisted state is atomic or absent

**Statement.** State publication leaves a complete previous state or detectable partial state after interruption, rather than silently combining versions.

**Enforcement.** `src/session/ancestor.rs` writes checkpoints and normalization through write, fsync, and rename. Compaction preserves the journal unless checkpoint-rename durability is confirmed (I10-B).

A failed compaction continues appending and does not fail an already recorded cycle. Durable intent also syncs a newly created journal’s directory entry. Reset removes journal state before checkpoint state.

`StateWriter` in `src/persist.rs` publishes derived state atomically.

**Tests and source references:**

- `a_partially_written_state_is_never_published`
- `an_interrupted_normalization_cannot_lose_acknowledgments`
- `an_interrupted_reset_cannot_fabricate_an_ancestor`
- `a_reset_leaves_nothing_to_replay`
- `a_state_superseded_within_the_window_is_never_encoded`
- `a_failed_compaction_fails_no_cycle_and_never_clears_the_journal`
- `a_durable_intent_syncs_whatever_the_configured_durability`

Compaction ordering and durable intent have mutation checks.

**Boundary.** Achieved appends remain OS-buffered unless `durability = "power"` is set. Replay drops a torn final record through `a_torn_final_record_is_discarded`. Durable preceding intent converts a lost tail into unknown provenance.

Reset guarantees program order, not all power-loss ordering. Recovery yields the old complete state or a closed failure, with a legacy-checkpoint resurrection corner remaining (I10-A, verified PARTIAL).

Observer caches and status files have no content checksum against torn-sector corruption (I10-C). Cache corruption can resemble forged metadata. Explicit verification bypasses cached digests.

## I11. A peer is confined to the root, genuine or not

**Statement.** Peer requests can access only scanned content inside the synchronization root and the session’s state, including staging and p2p ancestor copies. This applies to hostile protocol peers as well as genuine binaries.

**Enforcement.** Unless stated otherwise, these checks live in `src/endpoint/local.rs`.

`Initialize::validate` in `src/protocol.rs` requires a 32-character lowercase hex session and side `alpha` or `beta`. It runs before `create_endpoint` in `src/transport/mod.rs` touches the filesystem. `ancestor_copy_path` in `src/p2p.rs` repeats the session check.

`validate_path` and `validate_name` reject empty, `.`, `..`, separator, NUL, and reserved temporary-space components.

`open_scanned` serves only scanned regular files with the requested recorded digest. It avoids following a final symlink or blocking on a FIFO and reads no more than the scanned size.

`resolve_confined` and `create_confined_parents` reject linked parents for reads and renames. `stage_begin` rejects unsafe request paths. `open_base` uses the same confinement for rsync signatures.

`prepare_staging_root` rejects an inside-root staging symlink or directory owned by another user. Staging is private. A mode change copies a multiply linked file instead of changing external hard links.

`check_hierarchy` in `src/endpoint/remote.rs` rejects malformed agent trees, including unsorted or repeated names and invalid components. `validate_name` rejects reserved names before local creation. I3 governs received content.

**Tests and source references:**

- `an_absolute_supply_path_is_refused`
- `a_dot_dot_supply_path_is_refused`
- `a_supply_path_through_a_symlinked_parent_is_refused`
- `content_other_than_the_requested_digest_is_refused`
- `an_ignored_file_is_never_supplied`
- `a_fifo_swapped_in_after_the_scan_is_refused_without_hanging`
- `a_file_grown_since_the_scan_never_supplies_more_than_was_scanned`
- `an_agent_supplies_nothing_outside_its_root`
- `staging_requests_for_unsafe_paths_are_refused`
- `a_base_behind_a_symlinked_parent_yields_an_empty_signature`
- `read_file_refuses_a_symlinked_parent`
- `rename_out_through_a_symlinked_parent_is_refused`
- `rename_in_from_a_symlinked_parent_is_refused`
- `a_transition_creating_a_reserved_name_is_refused`
- `a_symlinked_inside_root_staging_directory_is_refused`
- `a_staging_directory_owned_by_another_user_is_refused`
- `a_mode_change_never_reaches_a_hardlink_outside_the_root`
- `path_validation_rejects_escapes`
- `refuses_root_deletion_and_unsafe_paths`
- `src/endpoint/local.rs`
- `a_session_that_is_not_32_hex_characters_is_refused`
- `an_unknown_side_is_refused`
- `src/protocol.rs`
- `a_traversal_session_is_refused_with_no_filesystem_effect`
- `src/transport/mod.rs`
- `a_traversal_session_fails_its_open_and_touches_nothing`
- `src/transport/mux.rs`

**Boundary.** A local writer can race pathname checks by swapping a parent for a symlink. See [accepted-risks §2](./accepted-risks.md#2-pathname-toctou-outside-linux-creations).

Raw symbolic links synchronize as data, including targets outside the root. Autobahn does not follow them during these operations, but other tools can.

P2P followers apply their own `host.toml` restrictions and ignore pushed `agent_command`. Attached alpha serves only its configured sessions. Without restricted keys, peer SSH credentials still provide shell access. See [P2P](../p2p.md).

## The threat-model note

I11 confinement and I9 size checks apply even to hostile protocol peers, subject to the stated local filesystem boundaries.

Other integrity and availability guarantees assume genuine binaries at both endpoints. A hostile agent can fabricate scans or transition results and manipulate ancestor provenance. That can cause changes inside the controller’s root, including under conflict policies (I2-A, I7-B).

Large or deeply nested valid messages can exhaust resources (I9-B). Defending against fabricated results and resource abuse requires independent result validation and semantic limits beyond the current design.

## The review of record

An independent model-lineage review on 2026-08-30 produced 22 findings. Separate verifiers assessed the leading findings.

Six confirmed issues were fixed: I1-A, I2-B, I3-A, I7-A, I10-B, and I6-C. Each received a harness or contract test before its fix, with mutation checks where the failure was constructible.

Other findings clarified the statements and boundaries. Remaining limits include hostile-peer integrity and availability, cross-process writer overlap, power-loss content publication, and unobserved mounts.

## How to attack this document

Prioritize these cases:

1. Shared-root schedules that serve stale snapshots as current (I1).
2. Crashes or power loss that invent ancestor agreement (I2, I10).
3. Hostile bytes that publish unverified content (I3, I9).
4. Protocol requests that escape roots or session state (I11).
5. Concurrent writes that bypass lease validation (I4).
6. Topologies that acquire conflicting writers without I6 enforcement.

Extend existing harness operation sets where possible.

## The specification

`spec/Autobahn.tla` models reconciliation across one alpha and multiple betas. TLC checks bounded states, and `tests/spec_replay.rs` compares implementation behavior. See [Specification guide](../../spec/README.md).
