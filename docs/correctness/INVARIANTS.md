# Invariants

Every invariant the system claims, stated precisely, with the code that enforces it and the tests that check it. This is the deliverable of NEXT.md phase D: an external reviewer should attack these *statements* — find a schedule, a crash point, a filesystem behavior, or an input that falsifies one — rather than reviewing diffs. Where an invariant has a deliberate boundary, the boundary is stated and cross-referenced to RETAINED.md, which records why it was retained and what would reopen it.

Citations use file paths and function or test names rather than line numbers, so they survive drift. All cited tests exist in the tree; many are mutation-checked — the enforcing code was deliberately broken and the test confirmed red — and those say so.

## I1. A scan describes the tree at a generation

**Statement.** Every snapshot is tagged with the generation it reflects, and no snapshot is ever *served as current* across a change the observer was told about that the snapshot did not observe. "Told about" means: a watcher event was delivered, or a writer announced the change through `invalidate` before writing.

**Enforced by** (`src/endpoint/observer.rs`):
- `walk` reads the generation *before* walking, so an event arriving during the walk leaves the publication immediately stale — the safe direction.
- The serve gate in `scan_inner` returns a published snapshot only while its generation still equals the signal's current value.
- The watcher callback (`src/endpoint/local.rs`, `ChangeWatcher::new`) records paths into the pending set *before* advancing the signal, and `RootObserver::invalidate` plants its announced paths through `ChangeWatcher::mark_pending` in the same record-then-advance order — so any walk old enough to miss a path in its dirty set is also old enough for its publication to be refused as current.
- A transition announces its paths *twice*: before its first write (so a racing scan cannot adopt its baseline) and again after its last (so a racing scan that consumed the first announcement's marks and read the old bytes is deterministically outdated — the kernel's own events do the same job, but on their own schedule, and never at all under the polling fallback). The independent review's finding I1-A showed the single pre-write announcement insufficient.
- `offer_baseline` refuses an offer based on an older generation than the standing baseline, so a stale fold can never roll the baseline back past a change whose dirty marks a scan already consumed.
- Dirty-path tracking fails toward full walks: a kernel queue overflow, a path cap, or an inexpressible path abandons the record (`ChangeWatcher::take_dirty` returning `None`), never narrows it.

**Checked by**: `a_mid_scan_change_is_never_served_as_current`, `an_offered_baseline_cannot_hide_an_invalidated_change`, the randomized interleaving sweep `every_interleaving_scans_the_truth` (all in `src/endpoint/observer.rs`), and `a_scan_racing_a_transition_cannot_outlive_the_writes` (`src/endpoint/local.rs`), which pauses a real transition between its announcement and its writes under the suppressed-watcher seam. The protocol properties are mutation-checked: disabling the path recording, the offer refusal, the serve gate, or the post-write announcement turns a test red. The sweep found two real holes on its first runs, and the independent review found a third (I1-A) — all fixed.

**Boundary.** An *unannounced* external write is visible only when the operating system delivers its event; scans in that delivery window are legitimately stale, as they are for every watcher-based synchronizer. On network filesystems events may never come — RETAINED.md §3.

## I2. The ancestor records only acknowledged agreement

**Statement.** Every path in the ancestor was recorded by a completed cycle whose transitions were acknowledged by both endpoints — or it is absent. A crash can only ever *remove* provenance, never fabricate it, and removed provenance surfaces as a conflict, never as an overwrite.

**Enforced by** (`src/session/ancestor.rs`, `src/session/mod.rs`):
- The achieved record is appended only after both endpoints' transitions return; the journal entry is digest-guarded (`digest(generation, payload)`), so a flipped header bit is corruption, not a skippable or normalizable record.
- Before the first transition of a cycle — but after staging, which mutates neither tree — `intend` appends the union of both sides' transition paths. Whenever a *remote* endpoint participates (or `durability = "power"` is set), that append syncs before any transition runs: the peer's machine persists its transition independently of this machine's page cache, so an unsynced intent there is no ordering at all — no writeback reordering is even needed to lose it (finding I2-B). A local-local session under default durability keeps the residual: losing the intent requires the storage stack to reorder two writes to the same disk, the same class as I10's journal-tail boundary, and `durability = "power"` closes it. An intent left unresolved at open drops those paths from the in-memory ancestor and persists the drop immediately (`Session::with_lock`), so provenance is honestly "unknown" after a crash — process or, where the sync applies, power — inside the transition window.
- Journal normalization is temp-file + fsync + rename, preserves a trailing unresolved intent, and a stray normalization temp is removed at open.
- Corruption anywhere is fail-closed: an undecodable journal is an error, never a silent reset.

**Checked by**: the crash-point enumeration `every_journal_cut_reopens_and_stays_appendable` (the journal is cut at every byte and must reopen to an acknowledged state), `intents_surface_until_a_cycle_completes`, `an_unresolved_intent_survives_normalization`, `a_flipped_generation_is_corruption_not_a_skip`, `a_flipped_checkpoint_generation_is_corruption`, `an_interrupted_normalization_cannot_lose_acknowledgments`, `a_torn_final_record_is_discarded`, `an_interrupted_reset_cannot_fabricate_an_ancestor`, `corrupt_ancestor_is_an_error_not_a_reset`, `a_durable_intent_syncs_whatever_the_configured_durability`, `a_remote_endpoint_makes_the_intent_durable`, `an_unconfirmed_checkpoint_never_clears_the_journal`, and — end to end — `a_crash_between_transition_and_record_ends_in_conflict_not_overwrite` (mutation-checked: with `intend` disabled, the recovered session silently overwrites the revert and the test goes red).

**Boundary.** The acknowledged cost of intent taint: a crash inside the transition window can turn a clean propagation into a surfaced conflict, and a crashed deletion can recover as *resurrection* — the absent-versus-present shape reconciles as a creation, not a conflict, so the earlier "surfaces as a conflict" reads as the general promise and resurrection as its deletion-shaped exception (finding I2-C). Under default durability a power loss can still drop an unsynced *achieved* record's tail; with the intent durable ahead of it, that recovers as taint noise, never as stale provenance. "Acknowledged" for a remote endpoint means a decoded response from the authenticated agent, not proof of remote disk state — a hostile agent is outside this invariant's model (finding I2-A; see the threat-model note at the end).

## I3. A digest names exactly its bytes

**Statement.** Any content the system addresses by digest — staged files, supply sources, published results — holds exactly the bytes that digest names at the moment of use. Unverified bytes cannot enter the staging store, and corrupted transfer content cannot reach a tree.

**Enforced by** (`src/endpoint/local.rs`):
- Receives write to a temporary path through `DigestingWriter`; `finish_receive` compares the accumulated digest and renames into the digest-named staged path only on a match. A mismatch discards.
- Staged survivors from interrupted cycles are re-verified by content (`staged_content_matches`) before being trusted.
- The last-use publish path re-verifies at the moment of use: the staged entry must still be a regular file whose bytes match the digest, or the publish falls through to the copy path, which digests what it moves (finding I3-A closed the unverified rename).
- Supply re-verifies alternates sharing a digest before serving them.

**Checked by**: `corrupted_frames_are_discarded_never_published` (mutation-checked: disabling the receive digest gate publishes the corruption and turns the torn-bytes assertion red), `a_corrupt_staged_survivor_is_retransferred_not_trusted`, `a_truncated_staging_transfer_recovers_cleanly`, `supply_recovers_from_an_alternate_path_sharing_the_digest`, `published_content_moves_out_of_staging_on_its_last_use`, `tampered_staged_content_is_never_published_by_the_move_path`, `a_staged_symlink_is_never_published_by_the_move_path`.

**Boundary.** A digest *recorded in a snapshot* is reused when metadata matches; content rewritten with deliberately restored metadata evades that reuse (RETAINED.md §5). The racy-mtime rule closes the accidental same-granule case (`a_same_granule_rewrite_is_reread_not_trusted`), and the verify verb re-reads every byte on demand (`a_verified_scan_sees_what_metadata_hides`).

## I4. Transitions validate against their lease

**Statement.** A transition mutates a path only if the tree there still matches the exact snapshot its reconciliation was computed from — the endpoint's *lease*, not whatever the shared observer has published since. Anything else is refused as a problem, and a refusal marks the whole observation untrustworthy.

**Enforced by** (`src/endpoint/local.rs`): the `Transitioner` validates against `last_snapshot` (the lease); refusals surface as problems and `distrust_baseline` forces the next scan to read rather than adopt; creations on Linux publish via `RENAME_NOREPLACE` (`publish_rename`), so a created path cannot silently replace a concurrent arrival.

**Checked by**: `a_creation_rename_refuses_to_replace`, `a_retargeted_symbolic_link_is_not_removed`, `transition_folds_achieved_results_into_the_snapshot`, and the lifecycle harness of I5.

**Boundary.** Check and use are separated by a pathname re-resolution window for *replacements and removals on every platform*, and for creations on platforms with no atomic no-replace rename — Linux (`RENAME_NOREPLACE`) and macOS (`renamex_np`) have one, FreeBSD and the other BSDs do not — RETAINED.md §2 (findings I4-A and I4-C sharpened its scope; `a_creation_rename_refuses_to_replace` pins which behavior each platform gets). Lease validation compares metadata, not content: a same-length rewrite with restored metadata passes it — RETAINED.md §5 (finding I4-B).

## I5. No crash leaves torn bytes

**Statement.** At every boundary of the staging and transition lifecycle — and at every frame boundary of the remote wire exchange — a *process* crash leaves every file on both trees bytewise equal to one of its legitimate versions. Recovery from any such crash, with the triggering fault gone, reaches quiescence, its conflicts confined to the crashed cycle's own paths, with full agreement on everything else. This is a process-crash invariant: publication paths do not sync file data, so a *power loss* can expose unsynced bytes under a renamed name (finding I5-A) — the durable-ancestor guarantees of I2 and I10 are the power-loss story, and content re-verification plus re-transfer restore the trees on the cycles that follow.

**Enforced by**: the composition of I2 (intent records), I3 (staged content verification), and I4 (lease validation, rename publication) — there is deliberately no additional mechanism to cite; this invariant is what the others buy.

**Checked by**: the phase-C2 fault harness in `src/session/mod.rs` (`a_crash_before_staging_recovers_cleanly`, `a_truncated_staging_transfer_recovers_cleanly`, `a_source_that_dies_mid_supply_recovers_cleanly`, `a_crash_before_any_transition_recovers_safely`, `a_partially_applied_transition_recovers_safely`, `a_crash_after_transition_before_record_recovers_safely`) and the phase-C3 reconnect sweep in `tests/e2e.rs` (`every_cut_connection_recovers_to_a_safe_tree`), which cuts a real agent connection at every frame boundary in both directions. Staging- phase crashes additionally owe *conflict-free* recovery, because the intent window opens only at the first transition. Measured by the C3 sweep: 29 engaged cut points, 3 recovering with conflicts — exactly the cuts inside the remote transition-to-record window.

## I6. One writer per tree region

**Statement.** Within one configuration load, no two sessions may hold *nested* writable local roots (an exactly-equal shared root warns and is a pinned-legal topology — fan-out, star, relay); across processes on one controller under one state root, an identical endpoint pair is excluded by lock, and a state root admits one supervisor and one session at a time. Endpoint identity is resolved once — on the supervisor path *and* the manual `sync` path (finding I6-C closed the latter's second resolution) — and the frozen resolution is carried through validation, locking, and construction.

**Enforced by**: `src/config.rs` (canonical containment refusal for nested writable endpoints), `src/supervisor/mod.rs` (frozen endpoint resolution), `src/session/mod.rs` (`EndpointPairLock`, `SessionLock`).

**Checked by**: `overlapping_roots_within_a_session_are_rejected`, `nested_writable_endpoints_across_sessions_are_rejected`, `aliased_paths_are_detected_as_duplicates`, `the_pair_lock_is_unordered_and_pair_scoped`, `a_state_directory_admits_only_one_session_at_a_time`, `a_second_supervisor_over_the_same_state_root_is_refused`, `a_retargeted_root_is_refused_rather_than_bound_to_stale_state`.

**Boundary.** Different-but-overlapping configurations in separate processes — RETAINED.md §4. The pair lock is controller-local (one machine, one user, one state root); two controllers, two users, or two lock roots are outside it (finding I6-B), as are physical aliases that pathname canonicalization cannot see, such as bind mounts (finding I6-D). Equal-root sharing relies on the observer's generation protocol and lease validation, not on mutual exclusion.

## I7. Reconciliation never destroys silently

**Statement.** In safe modes, content is overwritten or deleted only when the ancestor proves the other side already had it; two-sided divergence is a conflict, left unresolved; a proposed deletion and a proposed modification of the same path re-propagates the content. Mass disappearance of a *root* is halted, not propagated: a root that is empty or absent on exactly one side halts the session rather than deleting the other side. Below the root, the same shape — a directory of eight or more ancestor entries emptied on one side — is deletions and propagates in every mode but `two-way-paranoid`, where it is a conflict at the directory, and where a large directory gone on one side against an untouched other side is restored. (It was a halt in every mode until the guard fired on `git gc` packing loose refs; the tree alone cannot tell a vanished mount from a tool's cleanup, so the choice is now the mode's.)

**Enforced by**: `src/tree/reconcile.rs` (the safe-mode rules; the paranoid guard and restore rule, `PARANOID_MINIMUM`), `src/session/mod.rs` (`one_side_emptied_root`, the root-deletion refusal, `SafetyHalt`).

**Checked by**: the reconcile property suite (`conflict_free_reconciliation_converges`, `agreement_emits_nothing`, `unsynchronizable_content_never_travels`, `mutual_exclusion_preserves_the_ancestor` — mutation-checked), `concurrent_divergent_edits_conflict_in_safe_mode_and_resolve_in_resolved_mode`, `deletion_versus_modification_repropagates_content`, `one_way_safe_preserves_beta_creations`, `one_way_modes_never_touch_alpha`, `content_leaving_tracked_scope_never_reads_as_deletion`, `emptied_root_detection`, `paranoid_treats_an_emptied_large_directory_as_a_conflict`, `paranoid_restores_a_large_directory_gone_from_one_side`, `paranoid_lets_the_emptying_side_win_once_the_full_copy_is_retired`.

**Boundary.** A vanished mount holding fewer than eight entries — one huge file — evades the count guard: RETAINED.md §1. The guard also presumes trustworthy provenance; a fabricated ancestor (I2's hostile- agent boundary) makes safe-mode arithmetic destructive (finding I7-B).

## I8. Both ends speak the same safety semantics

**Statement.** A controller and an agent synchronize only if their version strings — package version plus compatibility epoch (`src/protocol.rs`, `COMPATIBILITY_EPOCH`) — match exactly, with both versions named on mismatch. The epoch is a *maintained convention*, not a mechanical property: it catches safety-semantics changes exactly when a change bumps it (finding I8-A), which is why bumping it is a standing release-gate rule rather than an optimization.

**Checked by**: `a_stale_epoch_fails_the_handshake`, `a_failed_handshake_reaps_the_spawned_process`.

## I9. The wire is hostile until proven otherwise

**Statement.** No length, flag, or compressed size received from a connection is trusted before validation: oversized incoming frames are refused *before allocation*, decompression bombs are refused, and unknown flags are errors. Outgoing oversized frames are refused before the write but *after* serialization — the sender allocates its own frame first (finding I9-A); the cap protects the peer and the wire, not the sender's memory. Structural depth of decoded messages is bounded only by bincode's input length, not by an explicit depth gate (finding I9-B) — a hostile peer is the threat-model note's territory.

**Enforced by**: `src/transport/mod.rs` (`read_frame` validates length and decompressed size against `MAXIMUM_FRAME_SIZE` before allocating).

**Checked by**: `an_oversized_length_prefix_is_refused_not_allocated`, `oversized_frames_are_rejected_on_receive`, `oversized_frames_are_rejected_on_send`, `a_decompression_bomb_is_refused`, `an_unknown_frame_flag_is_refused`.

## I10. Persisted state is atomic or absent

**Statement.** Every state file the system persists — the ancestor journal and checkpoint, the observer's snapshot cache, session status — is replaced atomically; a crash during any write leaves the previous complete state or a detectable partial, never a silently mixed one.

**Enforced by**: `src/session/ancestor.rs` (checkpoint and normalization write-fsync-rename; a compaction whose checkpoint-rename durability cannot be *confirmed* — the parent-directory sync fails — is an error that leaves the journal untouched, never a truncation over an unconfirmed rename (finding I10-B closed that ordering); intent appends sync unconditionally, and one that creates the journal file also syncs its directory entry; the journal-first reset order), `src/persist.rs` (`StateWriter`).

**Checked by**: `a_partially_written_state_is_never_published`, `an_interrupted_normalization_cannot_lose_acknowledgments`, `an_interrupted_reset_cannot_fabricate_an_ancestor`, `a_reset_leaves_nothing_to_replay`, `a_state_superseded_within_the_window_is_never_encoded`, `an_unconfirmed_checkpoint_never_clears_the_journal` (mutation-checked: truncating before the confirmed sync rolls acknowledged generations back and turns the test red), `a_durable_intent_syncs_whatever_the_configured_durability` (mutation-checked).

**Boundary.** Journal *achieved* appends are buffered by the OS unless `durability = "power"` syncs each one; the default trades the tail of the journal under power loss for latency, never its integrity — replay discards a torn tail record (`a_torn_final_record_is_discarded`), and the durable intent ahead of the lost tail downgrades the loss to taint noise. `reset` orders its removals by program order only; under power-loss reordering the surviving outcomes are the previous complete state or a fail-closed replay error, with silent resurrection confined to a legacy-checkpoint corner (finding I10-A, verified PARTIAL). The observer cache and status files are atomically *replaced* but their contents carry no integrity check against torn-sector corruption (finding I10-C) — the cache is a performance hint whose worst corruption case is equivalent to the forged-metadata boundary of RETAINED.md §5, and the verify verb re-reads past it.

## The threat-model note

Several invariants say "acknowledged", "authenticated", or "verified" about the remote agent. The agent is trusted at the level SSH authenticates it: a *hostile* agent binary — one that fabricates scan results or transition outcomes while speaking the protocol correctly — can manufacture ancestor provenance and thereby steer safe-mode reconciliation into overwriting the controller's own tree (findings I2-A, I7-B), and can send structurally deep messages that exhaust the decoder (finding I9-B). Defending the controller against the machine it synchronizes with is a different product than defending it against crashes, races, and power loss; it would need result validation against independent rescans, semantic caps on decoded structures, and an explicit trust boundary in the docs. Until that is built, the honest statement is: every guarantee in this document assumes both endpoints run genuine binaries.

## The review of record

This document was independently attacked (2026-08-30, an external model lineage, 22 findings), and each top finding was then verified or refuted by separate fresh verifiers. Six confirmed findings were fixed — I1-A, I2-B, I3-A, I7-A, I10-B, I6-C, each with a harness or contract test added first and the fix mutation-checked where the failure is constructible — and the statement-precision findings were folded into the invariant texts above, so the document now says what the code does. The remaining open items are deliberate boundaries: the hostile-agent model (above), cross-process and cross-machine writer exclusion (RETAINED.md §4), power-loss publication of tree *content* (I5), and the sub-threshold emptied-mount shape (RETAINED.md §1).

## How to attack this document

The highest-value falsifications, in order: a schedule of two sessions over one shared root that serves a stale snapshot as current (I1); a crash point or power-loss state that leaves the ancestor claiming an agreement that never completed (I2, I10); a byte sequence from a hostile agent that lands unverified content in a tree (I3, I9); an interleaving of a slow transition with a concurrent writer that bypasses lease validation (I4); a configuration or process topology that acquires two writers over one region without tripping I6. The harnesses named above are the existing search machinery; extending their op alphabets is usually cheaper than writing new ones.
