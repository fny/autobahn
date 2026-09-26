# Lane E1r notes (replay of lane-E1 onto integration)

## eaded8e (F-H13/OPS-7/L-23)
- Conflicts: control.rs imports (kept `Duration`, added `RwLock`); mod.rs registry/worker loop (took E1's
  restructure, then re-applied integration's `Entry.session` in `supervise` and `spawn_deep_scoped` in
  `start_session`); main.rs `check_startable` (load_for_startup + F-H25 OwnState check).
- Semantic conflict: F-H25 checked OwnState on every edit in main.rs's loop, but E1 applies edits inside
  `Supervisor::run_watch`, so that check was bypassed. Added `Supervisor::with_own_state` (main passes it);
  a failing edit is complained about and skipped. New test `an_edit_whose_root_holds_the_state_root_is_not_applied`
  (mutation-checked: fails without the check).

## 083213f (M-34)
- Additive conflicts only (Supervisor fields; two new tests side by side). Both spawn sites still use
  `spawn_deep_scoped`.

## db55214 (M-38)
- Entry: unified E1's `identifier` into integration's `session`; added `display` (plan.display()) so the
  inventory uses F-H18 labels. shop::run: owned plans + IsTerminal.
- Follow-ups: status notices via `style::emit` (283d85a); `show_recorded` disambiguates same group+host;
  shop empty-state keyed on `report.groups.is_empty()` (D1's escaping test builds a Shop with no plans).
- Tray still not compiled here (no gtk).
- Finished; full suite green at 727644a. See E1r.done.md.
