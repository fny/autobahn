# Your job

You are one lane of a parallel effort to implement code-review tickets in autobahn, a Rust file-synchronization tool. Other lanes are working at the same time, in their own git worktrees, on other parts of the code. An integrator lands every lane's commits afterwards. Nobody will answer questions, so work autonomously, and finish.

## Where things are

- **Your worktree:** the current directory. It is a git worktree on your own branch, `lane-A1r`. Commit there only. Never switch branches, never create branches or worktrees, never rebase, and never push. There is no remote.
- **Tickets:** `/home/ubuntu/autobahn/REVIEWS/fixes/<TICKET>.md`. Read-only. Each one holds the confirmed problem with file and line references, the agreed fix, and the tests it requires. Line numbers may have drifted slightly, so find the code by name.
- **Review summary:** `/home/ubuntu/autobahn/REVIEWS/FINAL.md`. Read-only. Its top section records the product decisions behind the tickets.
- **Parallel plan:** `/home/ubuntu/autobahn/REVIEWS/RUNBOOK-parallel-fixes.md` §6 has the lane table and says which files and functions each lane owns.

## Rules

1. **Stay in your region.** Edit only what your tickets need, inside the files and functions your lane owns (listed below). A tiny, unavoidable edit elsewhere, such as a `mod` line or a call site, is allowed if you name it in the commit message. If a ticket truly needs a larger change in another lane's region, skip that part and write it up in your notes file.
2. **For each ticket, in order:**
   1. Read the ticket and the code it names. Check the premise against the current code. Use sub-agents (the Agent tool) freely for reading, searching and checking; they are cheap and parallel.
   2. Write the ticket's tests first, and watch them fail for the reason the ticket describes. For a mutation check, confirm the test fails without the fix.
   3. Implement the fix, following the ticket's proposed resolution. You may improve on the details if the code shows a better way, but keep its intent and explain any departure in the commit message.
   4. Run `cargo fmt`, then `cargo clippy --release --all-targets -- -D warnings`, then the tests you touched plus the module's existing tests. Always run tests through the isolation wrapper: `isolated lane-A1r cargo test --release <filter>`. It gives tests a private HOME, and it must be used for every test run. Before your final commit, run the whole suite once: `isolated lane-A1r cargo test --release --no-fail-fast`.
   5. Commit once per ticket. Match the repository's message style: a lowercase `area: what is now true` subject line, like `ancestor: the journal is the checkpoint's format, and its bytes are held still` or `fix(mux): a scan's progress reports do not answer it`, then a short body explaining why, and a final line `Ticket: <ID>`.
3. **Keep the baseline clean.** At your starting point (`wave-0`), `cargo fmt --check` and `cargo clippy --release --all-targets -- -D warnings` are clean, and the full suite passes. Keep it that way. The only known flake is `a_standing_watch_lets_the_next_cycle_skip_the_beta_scan`, which fails intermittently; a later lane fixes it, so don't count it against yourself. Shared helpers from wave 0 are available to use:
   - `crate::fsutil`: `private_dir`, `private_file`, `private_tempdir`, `sweep_private_tmp`, `TMP_MAX_AGE`;
   - `crate::text`: `shell_quote`, `shell_join`, `display_safe`, `cap_line`;
   - `crate::protocol::is_session_identifier` and `Initialize::validate`;
   - `serve_agent_in` for tests.
4. **When a premise is wrong.** If a ticket's premise is wrong in the current code, or a fix would be unsafe, skip it. Explain why in your notes file, and move on.
5. **Never edit anything under `REVIEWS/`**, nor anything under `/home/ubuntu/autobahn` outside your worktree.
6. **Keep your notes current.** Append short progress notes to `/home/ubuntu/lanes/A1r.notes.md` as you go: decisions, departures from the ticket, anything the integrator must know.
7. **When you are finished,** write `/home/ubuntu/lanes/A1r.done.md` with:
   - one line per ticket, marked `done`, `partial` or `skipped`, with the commit hash and one sentence;
   - the final line of each `test result:` from your last full-suite run;
   - anything the integrator should do or check.

   Then stop.

Build output goes to `$CARGO_TARGET_DIR`, which is already set. Builds use `sccache`. The machine is shared with about 15 other lanes, so keep `cargo test` to one invocation at a time.

# Your lane: A1r

## Your job: replay lane A1's finished work onto the current code

Lane `A1` finished its tickets on branch `lane-A1`, which started from tag `wave-0`. Since then, other lanes' work has landed on `integration`, and replaying `lane-A1` onto it conflicts. Your worktree is a new branch `lane-A1r`, starting from the current `integration`.

1. Read `/home/ubuntu/lanes/A1.done.md` and `/home/ubuntu/lanes/A1.notes.md`, and the tickets they name.
2. Replay the commits of `git rev-list --reverse wave-0..lane-A1` one at a time, with `git cherry-pick -x <commit>`.
3. **On a conflict, resolve it so both sides' intent survives.**
   - Read the conflicting commit on `integration`, which is another lane's ticket work (`git log integration -- <file>`), and its ticket in `REVIEWS/fixes/`. The other lane's behaviour must be kept. Your commit's behaviour must be added on top of it.
   - When both lanes changed the same function differently, combine them by hand, then re-run both lanes' tests for that code.
   - Never drop another lane's change to make yours apply.
4. **After each commit,** run `cargo fmt`, clippy, and the tests for the touched modules, using `isolated lane-A1r cargo test --release <filter>`. If a resolution needs a follow-up fix, amend it into that cherry-pick's commit.
5. **Finish** with the full suite: `isolated lane-A1r cargo test --release --no-fail-fast`. Write `/home/ubuntu/lanes/A1r.done.md`, listing each replayed commit, how each conflict was resolved, and the final test results.

Everything else in the common rules applies. Your edits may touch any file a conflict involves, but only to resolve that conflict.
