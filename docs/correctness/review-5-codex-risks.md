# Ranked remaining correctness risks

## Verification of the new commits

I read commits `e3febf0..f6fc558` and checked the current implementations, not
just their commit messages. The primary reproduced failures are addressed:

- same-session overlapping roots are rejected; actual autobahn temporary names
  are distinguished from user names; and the emptied-root/subtree guard counts
  recursively and now rides reconciliation;
- the journal heals spent/torn material, reset removes the journal first,
  compaction fsyncs its checkpoint, and checkpoints carry an integrity digest;
- scan timestamps have a racy-mtime rule, size-race scans retain the original
  metadata, and publish metadata comes from the staged inode;
- Linux creations use `RENAME_NOREPLACE`, staged survivors are rehashed, a
  newly created root is probed before its children, and transition batches run
  deletions first;
- plans retain endpoint identities, an endpoint-pair lock is independent of
  `--state-root`, exclusion preserves ancestor provenance, and reconciliation
  has generated-property coverage.

The full gate passes locally: `cargo test --all-targets` reports 249/249 tests
(216 library, 1 binary, 12 end-to-end, 20 supervisor), `cargo fmt --check`
passes, and `cargo clippy --all-targets -- -D warnings` passes.

That verification changes the ranking substantially, but it also exposed four
boundaries where the implementation is narrower than the plan's wording:
journal normalization itself is not crash-atomic; agent equality is a package
version, not build equality; pair locking does not exclude sessions that share
one writable endpoint; and plan identity is checked before the original path is
canonicalized a second time.

## Risks, ranked by expected harm

1. **Open-time journal normalization can destroy acknowledged provenance while trying to heal it.**

   **Where:** `AncestorStore::open` reconstructs the applied records and then
   rewrites the *live* journal with `fs::write` at
   `src/session/ancestor.rs:91-122`. The byte-cut tests exercise a crash before
   `open`, then let normalization finish normally
   (`src/session/ancestor.rs:646-749`); they do not cut or fault the
   normalization write itself.

   **Harm:** Suppose a journal contains valid acknowledged records followed by
   a torn tail. `open` correctly reconstructs the newest ancestor, then
   `fs::write` truncates the journal before rewriting those valid records. A
   process kill in that write leaves a shorter valid prefix; an ENOSPC/EDQUOT
   partial write is worse because `open` returns an error after destructively
   truncating the only good journal. The next open accepts the shorter prefix
   and silently rolls provenance back. A later deliberate revert can then be
   overwritten. This reintroduces the exact acknowledged-state loss the
   enumeration was meant to eliminate, but inside recovery rather than append.

   **Likelihood driver:** Normalization runs specifically after a torn append,
   stale prefix, or partial header—conditions correlated with crashes and full
   disks. A second failure during recovery is uncommon, but ENOSPC remaining in
   force on the restart is not exotic.

   **Cheapest meaningful action:** Write normalized bytes to a unique sibling
   temporary and rename it over the journal only after the write succeeds.
   Extend the fault harness to cut/fail every normalization syscall and require
   that either the old journal or the complete normalized journal remains
   readable and appendable. I would fix this before production.

2. **Controller/agent “version locking” does not distinguish these commits from an older unsafe `0.3.0` agent.**

   **Where:** The handshake is only `CARGO_PKG_VERSION`
   (`src/protocol.rs:168-170`), which remains `0.3.0`
   (`Cargo.toml:1-4`) across all of `e3febf0..f6fc558`. The installed filename
   likewise contains only that version (`src/transport/install.rs:28-46`). If
   the old versioned executable starts and its handshake succeeds,
   `establish_ssh` returns immediately and never reinstalls it
   (`src/endpoint/remote.rs:129-153`).

   **Harm:** A host with any earlier `autobahn-0.3.0` can pass the handshake.
   An agent predating `Snapshot::scanned_at_seconds` will probably fail decoding
   closed and wedge remote sessions. More dangerously, a same-schema agent from
   after the snapshot change but before transition hardening can execute remote
   transitions without survivor rehash, root re-probing, `NOREPLACE`, or
   deletion-first ordering. The current controller then believes the safety
   fixes apply remotely when they do not.

   **Likelihood driver:** This is an ordinary upgrade path if any pre-fix 0.3.0
   binary has reached a remote host. It needs no crash or race.

   **Cheapest meaningful action:** Bump the release version before these commits
   ship. Then make the durable mechanism a protocol/build compatibility ID in
   both the handshake and installed-agent path (or a hash of the agent binary),
   because safety-relevant behavior can change without a wire-enum change. Add
   an integration test that seeds a same-semver/different-build agent and proves
   it is rejected or replaced. I would do this alongside item 1.

3. **The new pair lock excludes only the same unordered pair; sessions sharing one writable endpoint or overlapping subtrees can still race from independent ancestors.**

   **Where:** The lock key hashes exactly two endpoint identities at
   `src/session/mod.rs:630-687`, and its comment deliberately permits fan-out
   and relay pairs. `Config::plans` rejects overlap only *within one session* at
   `src/config.rs:560-588`; cross-plan checking still rejects only an identical
   ordered pair at `src/config.rs:589-599`. This is narrower than the plan's
   “same session and across sessions” language.

   **Harm:** Consider two two-way sessions A↔B and B↔C, with independent
   ancestors `O`. A changes `p` to `X`, C changes it to `Y`, and both sessions
   scan B=`O`. Their pair locks differ. Each can validate B=`O` before either
   publishes; the residual check/use race allows both replacements, last writer
   wins. The session that recorded the losing outcome now sees the winner as a
   sole B-side edit and propagates it back, overwriting X or Y at its source.
   Partially overlapping roots have the same class of problem. A shared
   observer reduces stale work in one process but is not a mutation lock, and
   controllers on different hosts do not even share that observer.

   **Likelihood driver:** Exact duplicate sessions are now closed. The trigger
   is a relay, nested source plan, or other topology that shares a writable
   root—explicitly supported today—plus concurrent edits. One-way fan-out with
   a read-only alpha is not the dangerous case.

   **Cheapest meaningful action:** Add endpoint access locks, not more pair
   locks: shared/read for an alpha in the one-way modes, exclusive/write for
   every beta and for both endpoints in two-way modes. On remote endpoints the
   agent must acquire the host-local endpoint lock. Until then, reject or
   clearly constrain relays and cross-session containment to topologies in
   which the shared endpoint is read-only.

4. **The transition-to-ancestor intent gap remains the largest accepted architectural data-loss window.**

   **Where:** Beta and alpha transitions commit at
   `src/session/mod.rs:378-407`; achieved changes are not recorded until
   `src/session/mod.rs:409-465`.

   **Harm:** A transition publishes `A`, then the process dies or loses the
   remote response before recording the achieved ancestor. While it is down,
   the user deliberately reverts one side to the old ancestor `O`. Restart sees
   that side as unchanged and the peer's `A` as modified, so the deliberate
   revert is overwritten. The repaired journal shortens the post-response part
   of this window but cannot cover mutation-before-response or
   mutation-before-record.

   **Likelihood driver:** It needs a failure in a narrow interval followed by a
   semantically meaningful revert before recovery. That is compound and less
   likely than items 1–3, but every mutating cycle traverses the window and the
   harm is silent.

   **Cheapest meaningful action:** Add a small write-ahead intent record before
   the first endpoint transition. On startup, an intent without a matching
   achieved record marks those paths as unknown provenance and forces a
   conflict/rescan rather than using the old ancestor. The journal format is
   already the right home for it.

5. **NFS/SMB/FUSE can invalidate the assumptions behind scanning, validation, watching, and locking, and the promised support boundary is not actually visible in the README.**

   **Where:** Transition checks are pathname/stat based, for example
   `src/endpoint/local.rs:1693-1710`; digest reuse trusts the current stat tuple
   at `src/scan/mod.rs:729-755`; locks use `flock` at
   `src/session/mod.rs:695-759`. `README.md:238-243` says Unix/Linux/macOS and
   transport scope, but does not state “local filesystems only” or
   “network mounts are best-effort/single-writer,” despite
   `docs/correctness/PLAN.md:112-115` saying that boundary is documented.

   **Harm:** Another NFS client writes a file while this client continues to
   receive cached attributes. A scan may reuse an old digest and destructive
   validation may approve against the same cache. Watchers are absent or
   incomplete, rename semantics vary, and advisory-lock behavior depends on
   mount/server configuration. The result can be silent missed changes or an
   overwrite that the local code believed it had validated.

   **Likelihood driver:** Synchronizing to a NAS is a natural use of a file-sync
   tool; this is more likely than an adversarial local race. Multi-client access
   and attribute-cache duration control the danger.

   **Cheapest meaningful action:** Detect filesystem type with `statfs` on each
   local/agent root and emit a prominent warning—or refuse two-way/destructive
   modes—on NFS, CIFS/SMB, and unknown FUSE filesystems. Put the single-writer,
   best-effort boundary in README and CLI help. If network filesystems are a
   supported target, add close-to-open refreshes and integration tests rather
   than relying on local-FS invariants.

6. **Pathname transition races remain: `NOREPLACE` closes only supported Linux file creation, not replacement, removal, component swaps, or the plain-rename fallback.**

   **Where:** Components are verified and then retained as path strings at
   `src/endpoint/local.rs:1243-1265`. Removals validate and later unlink at
   `src/endpoint/local.rs:1693-1710`; replacements likewise validate before an
   overwrite-capable publish. `publish_rename` uses `RENAME_NOREPLACE` only for
   creations on Linux and falls back to ordinary rename on other targets or
   specifically on `EINVAL` (`src/endpoint/local.rs:2050-2095`); other
   unsupported-error forms fail closed instead.

   **Harm:** An editor can land a replacement between validation and rename or
   unlink, losing the save. A directory component can be replaced by a symlink
   after verification, redirecting an operation outside the root. On macOS and
   on Linux paths that take the `EINVAL` fallback, creation retains the same
   final-name window. This is the accepted symlink/final-component race, not a
   regression in the new commits.

   **Likelihood driver:** The final-name editor race is tiny but plausible on a
   busy tree. The component-symlink form generally needs a cooperating local
   process and matters most if autobahn runs with more privilege than tree
   writers.

   **Cheapest meaningful action:** Document the current single-user threat
   model now. The real fix is descriptor-relative traversal and mutation
   (`openat2`/`openat`, held directory FDs, no-follow constraints, and
   rename/unlink relative to those FDs). Treat `ENOSYS`/`EOPNOTSUPP` explicitly
   so unsupported `renameat2` behavior is known rather than accidental.

7. **The ancestor store's proven model is process truncation, not power-loss reordering or header corruption.**

   **Where:** Per-cycle records use `write_all` without `sync_data` by design at
   `src/session/ancestor.rs:165-213`. Compaction syncs the temporary, but opening
   and syncing the containing directory are best-effort and errors are ignored
   at `src/session/ancestor.rs:248-268`. Checkpoint and journal digests cover
   payloads only: checkpoint generation is outside the digested payload at
   `src/session/ancestor.rs:235-246`, and journal `base_generation`/length are
   outside `digest(payload)` at `src/session/ancestor.rs:276-303` and
   `src/session/ancestor.rs:355-373`.

   **Harm:** Power loss can preserve synchronized content but lose the latest
   unsynced ancestor append, reopening the revert hole. A bit flip in a
   generation header is not detected: valid journal records can be skipped and
   then normalized away, or a wrong-lineage record can coincidentally become
   applicable. An ignored directory-sync error can weaken the intended
   checkpoint-before-truncate ordering.

   **Likelihood driver:** Sudden power loss and storage faults are uncommon but
   ordinary over a long deployment; probability depends heavily on filesystem,
   hardware, and whether this runs on laptops. The blast radius is provenance,
   not necessarily file bytes, but stale provenance can authorize later loss.

   **Cheapest meaningful action:** Digest the complete lineage-bearing record
   (generation, length, and payload), propagate checkpoint directory-sync
   errors where directory fsync is supported, and offer a `durable` mode that
   `sync_data`s each acknowledged journal record (or batches with an explicit
   weaker acknowledgment contract). Add a small power-cut/reordering model;
   byte-prefix enumeration cannot prove this class.

8. **Old, deliberately restored metadata still defeats digest reuse; the racy-mtime rule only closes ordinary same-granule writes.**

   **Where:** `reusable_digest` rejects recent baseline mtimes, then still
   accepts equality of mtime, size, inode, and type bits at
   `src/scan/mod.rs:718-760`.

   **Harm:** A build/deployment tool or user rewrites same-length bytes and
   restores an old timestamp (with the inode retained). Every later scan,
   including a full tree walk, reuses the old digest. The roots can diverge
   indefinitely, and a later peer edit can overwrite the invisible version.

   **Likelihood driver:** This no longer needs an ordinary fast double-write;
   it needs timestamp-preserving tooling, `touch -r`, reproducible-build logic,
   or an adversary. Those are uncommon but credible in development trees.

   **Cheapest meaningful action:** Add an on-demand/periodic checksum scan that
   truly rehashes files, and expose a `--checksum` or “verify now” operation.
   The current term “full scan” should not imply a full content rehash.

9. **The generalized emptied-tree guard is still a count heuristic: a vanished low-entry, high-byte mount remains destructive, while an intentional large emptying halts.**

   **Where:** Root protection requires two recursive entries; subtrees require
   eight at `src/session/mod.rs:594-629` and
   `src/tree/reconcile.rs:75-87`, `src/tree/reconcile.rs:146-162`.

   **Harm:** A mount containing one multi-terabyte database image, or seven
   very large media files, can vanish and stay below the guard despite enormous
   byte loss; reconciliation propagates the apparent deletions. Conversely,
   intentionally emptying a tracked directory with eight small entries halts
   until operator intervention. The fused implementation is new and has not
   seen real mount/remount patterns.

   **Likelihood driver:** Most vanished mounts contain many entries, so the new
   guard catches the common catastrophic shape. Low-entry bulk-data volumes are
   less common but precisely where count is a poor proxy for harm.

   **Cheapest meaningful action:** Trigger on either recursive entry count or
   synchronized byte magnitude/fraction, and log the path plus counts/bytes in
   the halt. Make the threshold configurable only after collecting telemetry;
   first add tests for huge single-file and low-entry submounts.

10. **Plan identity is checked, then the original local path is resolved again; a smaller version of the wrong-tree race remains.**

   **Where:** `connect` compares a fresh resolution to the planned identity at
   `src/supervisor/mod.rs:628-650`, but the endpoint closure later calls
   `path.canonicalize()` again at `src/supervisor/mod.rs:652-673` and uses that
   second result. The pair lock is keyed by the first/planned identity.

   **Harm:** Retarget a symlink after the equality check but before the second
   canonicalization. The endpoint attaches tree B to tree A's state and holds a
   lock for A, not B. Reconciliation can then use A's ancestor to authorize a
   write into unrelated B. This is the original defect with a much smaller
   startup window, not complete elimination.

   **Likelihood driver:** It requires an atomic retarget in a very narrow
   interval during connect; ordinary reconfiguration is unlikely to hit it.
   A hostile local process can target it.

   **Cheapest meaningful action:** Resolve once at connect and pass that exact
   `PathBuf` into `LocalEndpoint`; better, carry the resolved local path in
   `SessionPlan` and never return to the textual/symlinked path. Add a seam test
   that mutates the original path after resolution and proves the endpoint still
   opens only the frozen target.

11. **The shared observer's generation/baseline protocol has no enumerating concurrency test.**

   **Where:** A scan publishes only against the generation read before its walk
   (`src/endpoint/observer.rs:214-347`); transitions invalidate before writing
   and later overwrite the shared baseline with their fold
   (`src/endpoint/observer.rs:350-380`,
   `src/endpoint/local.rs:940-1026`). Multiple sessions can interleave these
   operations on one observer.

   **Harm:** The intended direction is conservative, and I did not find a new
   direct loss sequence independent of the accepted transition races. The risk
   is an unmodeled interleaving: two sessions invalidate, write, and offer
   baselines in opposite orders while watcher delivery is delayed; a stale fold
   can become the next incremental baseline. Current validation and the
   120-second full scan usually turn this into refusal or bounded staleness, but
   this protocol is subtle enough that example tests are weak evidence.

   **Likelihood driver:** Shared observers are exercised by every fan-out;
   unlucky interleavings increase with session count and filesystem-event
   delay. Harm is mitigated by validation, `NOREPLACE`, distrust-on-problem, and
   periodic full walks.

   **Cheapest meaningful action:** Build a deterministic state-machine test with
   two subscribers and a fake watcher. Enumerate scan-start/end, invalidate,
   filesystem write, event delivery, fold offer, and dirty-queue overflow;
   assert that no snapshot is served as current across a generation it did not
   observe and that the next destructive operation refuses stale state.

12. **Staging and deletion-first transition lifecycle has good integrity checks but no crash/fault enumeration.**

   **Where:** `stage_begin` discards prior receive state and inventories/reuses
   survivors at `src/endpoint/local.rs:754-835`; transition reorders all
   deletions ahead of every creation/replacement at
   `src/endpoint/local.rs:925-994`; partial outcomes are then folded at
   `src/endpoint/local.rs:996-1034`.

   **Harm:** Hashing makes wrong-byte publication much less likely. The
   remaining concern is lifecycle composition: crash or ENOSPC between receive
   finalization, survivor reuse, delete-first application, partial creation,
   outcome return, and ancestor recording. For a rename, deletion-first can
   temporarily remove the destination's old copy before the new-name creation
   fails; the source retains it and the next cycle should repair it, but
   redundancy is reduced exactly during a fault. Unenumerated cleanup/reuse
   states may also cause persistent stalls.

   **Likelihood driver:** Interrupted transfers and ENOSPC are ordinary; hashes
   and achieved-result folding constrain most failures to retry rather than
   corruption. The ordering code is new and its test coverage is example-based.

   **Cheapest meaningful action:** Add a model/fault harness that cuts or fails
   every staging and transition boundary, reopens, runs to quiescence, and
   asserts: every target path is old/new/explicitly partial, no wrong digest is
   published, the ancestor equals an acknowledged outcome, and a retry
   converges. Include case-only renames and failures after deletion but before
   creation.

13. **Remote multiplexing is fail-closed in reviewed paths, but reconnect and response-loss behavior is not enumerated end to end.**

   **Where:** Routing uses channel plus FIFO outstanding counts rather than
   request IDs (`src/transport/mux.rs:150-185`); the pool rebuilds dead
   connections at `src/transport/mux.rs:530-563`; staging permits four
   unacknowledged pushes and drains them before transition
   (`src/endpoint/remote.rs:216-260`). A lost transition response feeds directly
   into the intent risk in item 4.

   **Harm:** TCP ordering, never-reused channel IDs, typed responses, and a new
   namespace per connection make the obvious wrong-request attribution paths
   fail closed. The residual risk is behavior at cut points: an agent may have
   completed a push or transition whose response is lost while the controller
   tears down a pooled channel and other channels continue. A defect here could
   produce an incorrect achieved model; the more likely result is duplicate
   work or backoff.

   **Likelihood driver:** SSH disconnects and agent kills are routine over long
   runs. Current tests cover abrupt disconnect and pooled channels separately,
   not every frame boundary followed by reconnect and recovery.

   **Cheapest meaningful action:** Put a deterministic byte/frame-cutting proxy
   between controller and real agent. Cut before/after every request and
   response (especially each StagePush ack and Transition response), reconnect,
   and assert eventual convergence with no response from the old connection
   satisfying a new request. This will also measure the practical size of item
   4's remote-response window.

14. **The assurance process still has correlated blind spots: one implementation lineage, two model reviewers, and hand-written generators derived from the same semantics.**

   **Where:** The new property tests are valuable, but their generated trees
   are shallow/fixed-width and the convergence oracle is implemented with the
   same `apply`/tree model as production (`src/tree/reconcile.rs:830-1015`).
   Mutation checking was focused on reconciliation, while persistence,
   observer, staging, and reconnect state machines have no equivalent
   independent oracle.

   **Harm:** A shared mistaken invariant can survive code review, tests, and
   property checks simultaneously. This is not evidence of a particular bug;
   it is the multiplier on every unknown risk above, especially the new pair
   lock, fused guard, and normalization code.

   **Likelihood driver:** The review depth is unusually good, but both reviews
   followed the same written briefs and code history. The one-day concentration
   of large safety changes increases correlated-change risk despite the green
   suite.

   **Cheapest meaningful action:** Get one independent human review organized
   around explicit invariants rather than the existing defect list, and spend
   the next testing effort on independent state-machine/fault oracles for items
   1, 11, 12, and 13. Run a destructive soak on disposable real filesystems
   (ext4, APFS case-sensitive/insensitive, removable-mount loss, ENOSPC) before
   calling the safety plan production-proven.

## My proposed next order

1. Fix atomic journal normalization and add normalization fault cuts.
2. Bump `0.3.0` and make agent compatibility build/protocol-specific.
3. Decide the shared-endpoint contract; either add endpoint read/write locks or
   prohibit writable relays/overlapping plans.
4. Add the intent record design next; it is the largest accepted architectural
   gap after those release blockers.
5. In parallel, document/detect network filesystems and build the observer,
   staging, and reconnect fault harnesses before extending functionality.
