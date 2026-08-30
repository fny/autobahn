# Invariants

Every invariant the system claims, stated precisely, with the code that
enforces it and the tests that check it. This is the deliverable of
NEXT.md phase D: an external reviewer should attack these *statements* —
find a schedule, a crash point, a filesystem behavior, or an input that
falsifies one — rather than reviewing diffs. Where an invariant has a
deliberate boundary, the boundary is stated and cross-referenced to
RETAINED.md, which records why it was retained and what would reopen it.

Citations use file paths and function or test names rather than line
numbers, so they survive drift. All cited tests exist in the tree; many
are mutation-checked — the enforcing code was deliberately broken and the
test confirmed red — and those say so.

## I1. A scan describes the tree at a generation

**Statement.** Every snapshot is tagged with the generation it reflects,
and no snapshot is ever *served as current* across a change the observer
was told about that the snapshot did not observe. "Told about" means: a
watcher event was delivered, or a writer announced the change through
`invalidate` before writing.

**Enforced by** (`src/endpoint/observer.rs`):
- `walk` reads the generation *before* walking, so an event arriving
  during the walk leaves the publication immediately stale — the safe
  direction.
- The serve gate in `scan_inner` returns a published snapshot only while
  its generation still equals the signal's current value.
- The watcher callback (`src/endpoint/local.rs`, `ChangeWatcher::new`)
  records paths into the pending set *before* advancing the signal, and
  `RootObserver::invalidate` plants its announced paths through
  `ChangeWatcher::mark_pending` in the same record-then-advance order —
  so any walk old enough to miss a path in its dirty set is also old
  enough for its publication to be refused as current.
- `offer_baseline` refuses an offer based on an older generation than
  the standing baseline, so a stale fold can never roll the baseline
  back past a change whose dirty marks a scan already consumed.
- Dirty-path tracking fails toward full walks: a kernel queue overflow,
  a path cap, or an inexpressible path abandons the record
  (`ChangeWatcher::take_dirty` returning `None`), never narrows it.

**Checked by**: `a_mid_scan_change_is_never_served_as_current`,
`an_offered_baseline_cannot_hide_an_invalidated_change`, and the
randomized interleaving sweep `every_interleaving_scans_the_truth`
(all in `src/endpoint/observer.rs`). All three protocol properties are
mutation-checked: disabling the path recording, the offer refusal, or
the serve gate turns the sweep red. The sweep found two real holes on
its first runs — both fixed before it first passed.

**Boundary.** An *unannounced* external write is visible only when the
operating system delivers its event; scans in that delivery window are
legitimately stale, as they are for every watcher-based synchronizer.
On network filesystems events may never come — RETAINED.md §3.

## I2. The ancestor records only acknowledged agreement

**Statement.** Every path in the ancestor was recorded by a completed
cycle whose transitions were acknowledged by both endpoints — or it is
absent. A crash can only ever *remove* provenance, never fabricate it,
and removed provenance surfaces as a conflict, never as an overwrite.

**Enforced by** (`src/session/ancestor.rs`, `src/session/mod.rs`):
- The achieved record is appended only after both endpoints' transitions
  return; the journal entry is digest-guarded (`digest(generation,
  payload)`), so a flipped header bit is corruption, not a skippable or
  normalizable record.
- Before the first transition of a cycle — but after staging, which
  mutates neither tree — `intend` appends the union of both sides'
  transition paths. An intent left unresolved at open drops those
  paths from the in-memory ancestor and persists the drop immediately
  (`Session::with_lock`), so provenance is honestly "unknown" after a
  crash inside the transition window.
- Journal normalization is temp-file + fsync + rename, preserves a
  trailing unresolved intent, and a stray normalization temp is removed
  at open.
- Corruption anywhere is fail-closed: an undecodable journal is an
  error, never a silent reset.

**Checked by**: the crash-point enumeration
`every_journal_cut_reopens_and_stays_appendable` (the journal is cut at
every byte and must reopen to an acknowledged state),
`intents_surface_until_a_cycle_completes`,
`an_unresolved_intent_survives_normalization`,
`a_flipped_generation_is_corruption_not_a_skip`,
`a_flipped_checkpoint_generation_is_corruption`,
`an_interrupted_normalization_cannot_lose_acknowledgments`,
`a_torn_final_record_is_discarded`,
`an_interrupted_reset_cannot_fabricate_an_ancestor`,
`corrupt_ancestor_is_an_error_not_a_reset`, and — end to end —
`a_crash_between_transition_and_record_ends_in_conflict_not_overwrite`
(mutation-checked: with `intend` disabled, the recovered session
silently overwrites the revert and the test goes red).

**Boundary.** The acknowledged cost of intent taint: a crash inside the
transition window can turn a clean propagation into a surfaced
conflict, and a crashed deletion can recover as resurrection. Noise,
never loss.

## I3. A digest names exactly its bytes

**Statement.** Any content the system addresses by digest — staged
files, supply sources, published results — holds exactly the bytes that
digest names at the moment of use. Unverified bytes cannot enter the
staging store, and corrupted transfer content cannot reach a tree.

**Enforced by** (`src/endpoint/local.rs`):
- Receives write to a temporary path through `DigestingWriter`;
  `finish_receive` compares the accumulated digest and renames into the
  digest-named staged path only on a match. A mismatch discards.
- Staged survivors from interrupted cycles are re-verified by content
  (`staged_content_matches`) before being trusted.
- Supply re-verifies alternates sharing a digest before serving them.

**Checked by**: `corrupted_frames_are_discarded_never_published`
(mutation-checked: disabling the receive digest gate publishes the
corruption and turns the torn-bytes assertion red),
`a_corrupt_staged_survivor_is_retransferred_not_trusted`,
`a_truncated_staging_transfer_recovers_cleanly`,
`supply_recovers_from_an_alternate_path_sharing_the_digest`,
`published_content_moves_out_of_staging_on_its_last_use`.

**Boundary.** A digest *recorded in a snapshot* is reused when metadata
matches; content rewritten with deliberately restored metadata evades
that reuse (RETAINED.md §5). The racy-mtime rule closes the accidental
same-granule case (`a_same_granule_rewrite_is_reread_not_trusted`), and
the verify verb re-reads every byte on demand
(`a_verified_scan_sees_what_metadata_hides`).

## I4. Transitions validate against their lease

**Statement.** A transition mutates a path only if the tree there still
matches the exact snapshot its reconciliation was computed from — the
endpoint's *lease*, not whatever the shared observer has published
since. Anything else is refused as a problem, and a refusal marks the
whole observation untrustworthy.

**Enforced by** (`src/endpoint/local.rs`): the `Transitioner` validates
against `last_snapshot` (the lease); refusals surface as problems and
`distrust_baseline` forces the next scan to read rather than adopt;
creations on Linux publish via `RENAME_NOREPLACE`
(`publish_rename`), so a created path cannot silently replace a
concurrent arrival.

**Checked by**: `a_creation_rename_refuses_to_replace`,
`a_retargeted_symbolic_link_is_not_removed`,
`transition_folds_achieved_results_into_the_snapshot`, and the
lifecycle harness of I5.

**Boundary.** Outside Linux creations, check and use are separated by a
pathname re-resolution window — RETAINED.md §2.

## I5. No crash leaves torn bytes

**Statement.** At every boundary of the staging and transition
lifecycle — and at every byte of the remote wire exchange — a crash
leaves every file on both trees bytewise equal to one of its legitimate
versions. Recovery from any such crash reaches quiescence, its
conflicts confined to the crashed cycle's own paths, with full
agreement on everything else.

**Enforced by**: the composition of I2 (intent records), I3 (staged
content verification), and I4 (lease validation, rename publication) —
there is deliberately no additional mechanism to cite; this invariant
is what the others buy.

**Checked by**: the phase-C2 fault harness in `src/session/mod.rs`
(`a_crash_before_staging_recovers_cleanly`,
`a_truncated_staging_transfer_recovers_cleanly`,
`a_source_that_dies_mid_supply_recovers_cleanly`,
`a_crash_before_any_transition_recovers_safely`,
`a_partially_applied_transition_recovers_safely`,
`a_crash_after_transition_before_record_recovers_safely`) and the
phase-C3 reconnect sweep in `tests/e2e.rs`
(`every_cut_connection_recovers_to_a_safe_tree`), which cuts a real
agent connection at every frame boundary in both directions. Staging-
phase crashes additionally owe *conflict-free* recovery, because the
intent window opens only at the first transition. Measured by the C3
sweep: 29 engaged cut points, 3 recovering with conflicts — exactly
the cuts inside the remote transition-to-record window.

## I6. One writer per tree region

**Statement.** Within one configuration load, no two sessions may write
overlapping local roots; across processes, an identical endpoint pair
is excluded by lock, and a state root admits one supervisor and one
session at a time. Endpoint identity is resolved once and the resolved
identity is carried through validation, locking, and construction.

**Enforced by**: `src/config.rs` (canonical containment refusal for
nested writable endpoints), `src/supervisor/mod.rs` (frozen endpoint
resolution), `src/session/mod.rs` (`EndpointPairLock`,
`SessionLock`).

**Checked by**: `overlapping_roots_within_a_session_are_rejected`,
`nested_writable_endpoints_across_sessions_are_rejected`,
`aliased_paths_are_detected_as_duplicates`,
`the_pair_lock_is_unordered_and_pair_scoped`,
`a_state_directory_admits_only_one_session_at_a_time`,
`a_second_supervisor_over_the_same_state_root_is_refused`,
`a_retargeted_root_is_refused_rather_than_bound_to_stale_state`.

**Boundary.** Different-but-overlapping configurations in separate
processes — RETAINED.md §4.

## I7. Reconciliation never destroys silently

**Statement.** In safe modes, content is overwritten or deleted only
when the ancestor proves the other side already had it; two-sided
divergence is a conflict, left unresolved; a proposed deletion and a
proposed modification of the same path re-propagates the content. Mass
disappearance is halted, not propagated: a root or large subtree
(eight or more ancestor entries) empty on exactly one side halts the
session rather than deleting the other side.

**Enforced by**: `src/tree/reconcile.rs` (the safe-mode rules;
`Reconciliation::emptied_subtree` riding the descend path;
`EMPTIED_SUBTREE_MINIMUM`), `src/session/mod.rs`
(`one_side_emptied_root`, the root-deletion refusal, `SafetyHalt`).

**Checked by**: the reconcile property suite
(`conflict_free_reconciliation_converges`, `agreement_emits_nothing`,
`unsynchronizable_content_never_travels`,
`mutual_exclusion_preserves_the_ancestor` — mutation-checked),
`concurrent_divergent_edits_conflict_in_safe_mode_and_resolve_in_resolved_mode`,
`deletion_versus_modification_repropagates_content`,
`one_way_safe_preserves_beta_creations`,
`one_way_modes_never_touch_alpha`,
`content_leaving_tracked_scope_never_reads_as_deletion`,
`emptied_root_detection`,
`emptied_subtree_detection_rides_reconciliation`.

**Boundary.** A vanished mount holding fewer than eight entries — one
huge file — evades the count guard: RETAINED.md §1.

## I8. Both ends speak the same safety semantics

**Statement.** A controller and an agent synchronize only if they share
the compatibility epoch; the epoch rides the version string
(`src/protocol.rs`, `COMPATIBILITY_EPOCH`), so any safety-semantics
change fails the handshake with both versions named rather than
producing subtly mixed behavior.

**Checked by**: `a_stale_epoch_fails_the_handshake`,
`a_failed_handshake_reaps_the_spawned_process`.

## I9. The wire is hostile until proven otherwise

**Statement.** No length, flag, or compressed size received from a
connection is trusted before validation: oversized frames are refused
on both send and receive before allocation, decompression bombs are
refused, and unknown flags are errors.

**Enforced by**: `src/transport/mod.rs` (`read_frame` validates length
and decompressed size against `MAXIMUM_FRAME_SIZE` before allocating).

**Checked by**: `an_oversized_length_prefix_is_refused_not_allocated`,
`oversized_frames_are_rejected_on_receive`,
`oversized_frames_are_rejected_on_send`,
`a_decompression_bomb_is_refused`, `an_unknown_frame_flag_is_refused`.

## I10. Persisted state is atomic or absent

**Statement.** Every state file the system persists — the ancestor
journal and checkpoint, the observer's snapshot cache, session status —
is replaced atomically; a crash during any write leaves the previous
complete state or a detectable partial, never a silently mixed one.

**Enforced by**: `src/session/ancestor.rs` (checkpoint and
normalization write-fsync-rename; the journal-first reset order),
`src/persist.rs` (`StateWriter`).

**Checked by**: `a_partially_written_state_is_never_published`,
`an_interrupted_normalization_cannot_lose_acknowledgments`,
`an_interrupted_reset_cannot_fabricate_an_ancestor`,
`a_reset_leaves_nothing_to_replay`,
`a_state_superseded_within_the_window_is_never_encoded`.

**Boundary.** Journal *appends* are buffered by the OS unless
`durability = "power"` syncs each one; the default trades the tail of
the journal under power loss for latency, never its integrity — replay
discards a torn tail record (`a_torn_final_record_is_discarded`).

## How to attack this document

The highest-value falsifications, in order: a schedule of two sessions
over one shared root that serves a stale snapshot as current (I1); a
crash point or power-loss state that leaves the ancestor claiming an
agreement that never completed (I2, I10); a byte sequence from a hostile
agent that lands unverified content in a tree (I3, I9); an interleaving
of a slow transition with a concurrent writer that bypasses lease
validation (I4); a configuration or process topology that acquires two
writers over one region without tripping I6. The harnesses named above
are the existing search machinery; extending their op alphabets is
usually cheaper than writing new ones.
