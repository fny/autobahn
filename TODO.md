# TODO

Running list. Untracked by git for now — say the word if it should be
committed.

## UX

- [x] **`conflicts --depth` drill-down hint: backticks and wording.** Done
  alongside the positional fix: the hint is now
  `` `autobahn conflicts <group> --depth N` opens the next level ``.

- [x] **`conflicts` took a host where its siblings take a path.** Done.
  `autobahn conflicts voltai autobahn` scopes to that folder, `--host`
  filters the destination, and a folder given as the selector scopes too.
  Depth counts from the scope.

- [ ] ~~superseded~~ **`conflicts --depth` drill-down hint.**
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

- [ ] **`one_file_system = true`, and probably first.** Do not walk into a
  directory that sits on a different filesystem. `rsync -x`,
  `tar --one-file-system`, `find -xdev` and `du -x` all do this. It uses
  the same device number as the item that follows, but needs it only
  during the walk. It never has to remember one, so it needs no new field,
  no ancestor change, no carry-forward, and no epoch bump. It also removes
  the problem in both directions: a mount that appears copies nothing, and
  a mount that goes away deletes nothing. The mount guard is then only
  needed for people who turn this off. Decide the default. `true` matches
  every other tool. The agent needs the option too, so it goes in
  `Initialize`.

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

- [x] **`conflicts` says "no conflicts" for a halted session.** Done, as
  part of `issues`: a failed session is listed by its state, and a filter
  that excludes every path does not exclude it.

- [ ] ~~superseded~~ **`conflicts` says "no conflicts" for a halted session.** Structural:
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

## Ancestor format (started 2026-09-02)

- [x] **Version the ancestor checkpoint.** Done. Format 2 states its own
  version, the digest covers it, and formats 0 and 1 still read and are
  rewritten in the current form on first open. An unknown format is
  refused with the versions, the command, and what the command costs. A
  header-only check runs at startup, before any cycle.

- [ ] **Rebuild an ancestor that cannot be read, when it is safe.** Now
  justified by corruption alone — versioning covers planned changes. Scan
  both sides. If they hold identical content, adopt it and carry on:
  nothing can be resurrected when the two sides already agree, so the fix
  is provably a no-op. If they differ, do not act. Halt and name
  `autobahn reset`, because rebuilding then resurrects deletions. Make it
  loud either way, and refuse a second rebuild on the same session — a
  disk that corrupts one ancestor will corrupt another, and a silent
  retry turns a hardware fault into a mystery.

- [ ] **Version the journal records too.** The checkpoint states its
  format; journal records do not. Today it does not matter, because an
  older checkpoint is converted at open and the journal is retired with
  it, so records never outlive the build that wrote them. A change that
  lands without a checkpoint rewrite would break that. Either state the
  version per record, or write down why the conversion at open is
  sufficient.

## Found while building the shop (2026-09-03)

- [x] **`resolve` cannot settle a directory conflict.** Done, and not by
  teaching it to copy directories. It now retires the *losing* version
  and lets the cycle carry the winner — which is what reconciliation
  already does for one side's deletion, for a file, a symbolic link, or
  a tree alike. The removal goes through `transition`, so an entry that
  moved since the scan is refused rather than destroyed. `--keep both`
  renames the loser aside, a new endpoint primitive (epoch 6). Covered
  for all three winners by
  `resolve_settles_a_conflict_between_a_directory_and_a_file`.

- [ ] **Three integration tests were broken for a day.** `82976d3` (the
  resolve confirmation) and the `issues` rename each broke assertions in
  `tests/supervisor.rs`, and neither was caught, because after each
  change I ran only `--bins`. The lesson is not "run everything every
  time" — it is that a change to a command's *output or prompting* has
  to run that command's integration tests, which are the only place the
  wording is asserted. Worth a note in the contributing guide.

- [ ] **A rebuilt agent never reaches a host that already has that
  version.** `remote.rs:341` installs only when the first connect fails,
  and the binary is named `autobahn-<version>`. A rebuild at the same
  version runs the old agent forever. Found the hard way: the directory
  message added in 82976d3 did not appear on `boite`, whose agent is from
  Sep 2 02:15. It hid because the case I tested failed on alpha, which is
  local. Fix: name the remote binary by a digest of its content
  (`autobahn-<version>-<digest8>`), so a changed binary always deploys and
  an unchanged one never re-uploads. Hashing 4 MB costs a few milliseconds
  and only on connect. Prune old binaries in `clean`. Until then, remove
  `~/.autobahn/bin/autobahn-<version>` on the remote by hand after any
  agent-side change.

- [ ] **A directory conflict is recorded as kind "other".**
  `conflict_detail` maps `Content::Directory` to "directory", but a real
  one (`vibe` → `boite`, the `happy` and `voltagen` trees) comes back as
  "other" — so the change it inspects is not the directory node it
  expects. `issues` and the shop both print it, so the mislabel is
  visible. Worth finding out which change it is actually looking at.

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
