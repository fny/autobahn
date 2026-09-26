# Lane 3br notes

- One commit to replay: 2688d08 (F-H1 stage 2), which became 16f3e9d.
- Conflicts:
  - `src/session/mod.rs`: kept the new Side/Retirement/Settlement types, and put the doc line added on integration back on `Session`.
  - `src/supervisor/mod.rs` `attempt_once`: integration (37c1892) removed the closure. The cycle hook and `apply_resolutions` go into the flat body, before verify and run_cycles.
  - `src/main.rs` `run_resolve`: kept 3a's `RESOLVE_FLUSHES` and per-session flush (72bd691). `touched` is now computed from `losers` with actions before the parts are built. A supervised resolve sends one flush per touched session (when settled > 0), not the group flush. The flushes are recorded for tests on both paths, so 3a's unit test (unsupervised) still sees exactly one flush.
- Full suite green, fmt and clippy clean.
