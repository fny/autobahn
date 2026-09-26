# Lane E1r: done (replay of lane-E1 onto integration)

## Replayed commits
- eaded8e → **514d35a** (F-H13, OPS-7, L-23, L-25): **done**. Conflicts in control.rs imports, the
  supervisor's registry and worker loop, and main.rs `check_startable`. I took E1's restructure, then
  re-applied integration's changes on top: `Entry.session`, and `threads::spawn_deep_scoped` in
  `start_session` (so B2's deep stack is already in place). `check_startable` does
  `load_for_startup` and then F-H25's OwnState check.
  There was one conflict git did not flag: F-H25 checked OwnState on every edit inside main.rs's
  reload loop, but E1 now applies edits inside `Supervisor::run_watch`, so that check was skipped.
  I added `Supervisor::with_own_state`, which main passes. An edit that fails the check is
  complained about and not applied. New test `an_edit_whose_root_holds_the_state_root_is_not_applied`;
  I confirmed it fails when the check is removed.
- 083213f → **cb2948c** (F-M-SUP M-34): **done**. Additive conflicts only: a new Supervisor field, and
  two new tests side by side.
- db55214 → **695e8e4** (F-M-SUP M-38): **done**. Conflicts in `Entry`, `shop::run` and Supervisor
  fields.
  - `Entry`: E1's `identifier` is merged into integration's `session`. It gains `display` from
    `plan.display()`, so the inventory uses F-H18's path labels.
  - `shop::run`: owned plans, and the `IsTerminal` check kept.
  - Follow-ups for other lanes' behaviour:
    - The new `status` notices go through `style::emit`, per 283d85a (NO_COLOR, non-terminal).
    - `show_recorded` tells apart two recorded betas of the same group on the same host.
    - The shop's empty-state line now checks `report.groups` rather than `plans`. This keeps D1's
      test `a_name_with_control_characters_is_drawn_escaped` passing.
- a7ffd8c → **727644a** (F-M-OUT M-7, log part): **done**, applied cleanly.

## Last full-suite run (`isolated lane-E1r cargo test --release --no-fail-fast`, at 727644a)
- lib: test result: ok. 522 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 57.33s
- main: test result: ok. 44 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
- e2e: test result: ok. 35 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 25.10s
- spec_peering_replay: test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 6.76s
- spec_replay: test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.99s
- supervisor: test result: ok. 72 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.72s
- topology: test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.79s
- doctests: test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

`cargo fmt --check` and clippy (`-D warnings`) are clean.

## For the integrator
- The `tray` feature is still not compiled, because this machine has no gtk. Run `cargo check --features tray`.
- Peering supervisors (`peer::run_alpha` and the beta side) do not get `with_own_state`. F-H25 already
  said it did not cover them. They restart on every edit through main.rs's loop, which still runs
  its own OwnState check.
- E1's original commits had no Co-Authored-By trailer, so the replays carry none either. Each commit
  has a "Replayed onto integration" paragraph and a `(cherry picked from …)` line.
- The rest of E1's notes still apply: the wire indices of `Sessions`, the files touched outside the
  lane, and the behaviour changes. See `/home/ubuntu/lanes/E1.done.md`.
