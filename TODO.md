# TODO

Running list. Untracked by git for now — say the word if it should be
committed.

## UX

- [ ] **`conflicts --depth` drill-down hint: backticks and wording.**
  `src/main.rs:1139` currently prints
  `    → autobahn conflicts voltai --depth 2 to look inside`.
  Put backticks around the command, and reword — "to look inside" is
  vague. Check the sibling hint at the end of the unrolled listing
  (`→ autobahn resolve <group> <path> --keep alpha|<host>|both`) for the
  same treatment, so the two agree.

- [ ] **Merge `[alerts]` into `[defaults]`.** Currently its own top-level
  table. Open question to settle when doing it: is this purely
  relocation (tidier config, same global behaviour), or does living under
  `[defaults]` mean groups may override alerts individually — a different
  `on_alert` for `voltai` than for `aws`, say? The second is more work
  (the alerter is currently one state machine over all sessions, and
  per-group commands would need the firing split by group) but is what
  `[defaults]` promises everywhere else in the file, so the name would
  otherwise lie. Config compatibility: keep reading a top-level
  `[alerts]`, since it is in the wild as of 758e240.

## The vibe halt (found 2026-09-02)

- [x] **The emptied-subtree halt lies and withholds the path.** Done: the
  absent-directory form (I7-A) is gone, so deliberate deletions propagate;
  the remaining halt names the path, the side, and the entry count.
  Superseded parts of the original note kept below for context.

- [ ] **Record mount boundaries by device number.** The proper fix for the
  hole reopened by dropping I7-A: a mountpoint removed on eject (macOS
  `/Volumes`, automounts, or any mount whose parent was on the vanished
  filesystem) presents as *absent*, which now propagates. Compare each
  directory's `dev()` with its parent's during the scan — the stat already
  happens, so the device number is free — carry the boundaries in the
  snapshot and the session state, and halt when a recorded boundary is
  empty or absent. That makes the trigger a fact rather than a shape, and
  lets the size threshold go entirely. Costs: `Snapshot` gains a field, so
  the compatibility epoch bumps and every agent reinstalls; the mount list
  must survive incremental scans (which adopt subtrees without visiting
  them) or it silently empties; A/B the scan hot path before commit.

- [ ] ~~The old note:~~ **The emptied-subtree halt lies and withholds the path.**
  `src/session/mod.rs:471` — two distinct conditions raise the same
  `SafetyHalt::RootEmptied`, and the subtree one discards the path it was
  handed (`let _ = path;`) then reports that the *root* was emptied. The
  root is fine. Three parts:
  1. `SafetyHalt::SubtreeEmptied { path }` as its own variant, naming the
     directory.
  2. Name the side too — "gone on beta, 4,150 entries on alpha". Neither
     current message says which side.
  3. Report all of them, not the first only: reconciliation stops at the
     first, so you fix one and halt again on the next. Twelve, for vibe.

- [ ] **`conflicts` says "no conflicts" for a halted session.** Structural:
  the cycle bails before `report.conflicts` is assigned, so a halted
  session records zero conflicts, and `run_conflicts` skips sessions with
  an empty list. A group that has not synchronized in a day reports an
  all-clear. It should say it is halted and therefore not reporting —
  "I don't know" and "nothing is wrong" are different answers. Same for
  `--json`.

- [ ] **`autobahn doctor <group>`** — promote `examples/probe.rs` to a real
  command. Read-only: opens both endpoints, scans, and reports what each
  root looks like plus every directory populated on one side and
  empty/absent on the other. It answered a question the product could not
  answer about itself, which is the argument for shipping it. Delete the
  example when it lands.

## Carried over (not yet asked for, noted so they aren't lost)

- [ ] 21 blocked paths on `fny.voltai.party` — root-owned
  `azure/backend/.ruff_cache/*` (unreadable) and
  `arcturus/frontend/apps/web/public/static/*` (unwritable), plus
  `recruiting/.../Jorge Suárez Jiménez resume.pdf` refusing to create over
  existing content. Needs a `chown` on the remote, or an ignore.
- [ ] `mutagen-bench` (448k files) exists on both sides of the `voltai`
  group. Ignore it or delete it on both.
- [ ] Remote scans report no progress counts: the agent scans inside one
  request and the protocol carries no frame for progress. Needs a new
  response variant and a compatibility-epoch bump.
- [ ] Parallel scanning — hashing pool first (clear win on first scans),
  then a parallel walk (pays off every cycle). Both hot-path: A/B gate
  before commit.
- [ ] Collapse fully-synchronized groups to one line in `status`, so 15
  healthy sessions don't cost 45 lines of scrolling.
