# F-H13: Disabling the last session stops it under live reload

**Findings:** H-13 (ASTRA F08).
**Status:** proposed. High; fix before v1. Land it with OPS-7, which rewrites the same reload path.

## Problem

`autobahn disable` accepts and saves a configuration in which no plan is active. Removing the last group does the same. But `reload::load` (`src/supervisor/reload.rs:40-44`) refuses any configuration with no sessions:

```rust
if plans.is_empty() {
    anyhow::bail!("the configuration describes no sessions");
}
```

The live reloader treats a refused edit as "keep what's running". So disabling the only group, or the only host, leaves its sessions syncing. The user asked for them to stop, and nothing tells them it didn't happen. A test pins the refusal (`:295`).

`load` serves both startup and live reload, so one rule covers two situations that need different rules.

## Proposed resolution

- **Separate the two policies.** `load` returns the plans, whether or not there are any.
  - **At startup,** `watch` and `start` (through `check_startable`) keep refusing an empty configuration, with the current message, because starting a supervisor that has nothing to do is almost always a mistake.
  - **Under live reload,** an empty configuration is applied. Every worker stops, and the supervisor keeps its control socket and its reloader, so a later `enable` or edit starts sessions again without a restart.
- **Show the state.** `status` says "no active sessions (every group is disabled)" instead of nothing, and the shop and tray show it too.
- **With OPS-7.** Once reload applies changes per session, an empty configuration simply means every session is removed. Build the two together.

## Tests

- **Supervisor integration:** with one group running, `disable` the group. Propagation stops within one interval. Then `enable` it, and syncing resumes with its ancestor intact.
- The same test, disabling the only *host* rather than the group.
- Removing the last group from the file behaves the same way.
- Startup with an empty configuration is still refused.
- **Update the test at `reload.rs:295`** to cover startup only, and add one for live reload.
