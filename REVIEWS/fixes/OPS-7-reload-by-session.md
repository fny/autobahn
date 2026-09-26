# OPS-7: Apply a config edit only to the sessions it changes

**Findings:** L-24 (OPUS). Related: H-13, where disabling the last session leaves every worker running. That fault gets its own ticket in the severity walk, but it touches the same code.
**Status:** proposed.

## Problem

When the live reloader loads an edit, it raises one `halt` flag (`src/supervisor/reload.rs:203`). `run_watch` then winds down the whole supervisor (`src/supervisor/mod.rs:586-600`):
- every session stops;
- every pooled SSH connection and agent closes;
- the control socket goes down.

It then starts again from the new config. Renaming one group, or adding a host, interrupts every session, abandons any cycle in flight, and reconnects every host.

## Proposed resolution

- **Diff by identity.** Compare the new plans with the running ones by session identifier:
  - **Unchanged** sessions keep running, untouched.
  - **Removed or disabled** sessions stop through their own stop flag.
  - **Added or re-enabled** sessions start.
  - **Changed** sessions, with the same identifier but a different mode, ignores, interval or options, stop and start again.
- **Keep what is shared.** Connections in the per-host pool stay up. Remove a slot only when no remaining session uses that host. That also fixes the L-25 leak of evicted hosts.
- **Keep the control socket.** It stays up and swaps its registry to the new set.
- **Restart only for global changes.** Changes to supervisor-wide settings, such as the state root or `reload` itself, still fall back to a full restart.

## Tests

- Supervisor integration tests:
  - add a group while another syncs, and the first keeps its cycle count and connection;
  - change one group's ignores, and only that session restarts;
  - disable one group, and the others don't reconnect.
- A reload while a cycle is in flight, in an untouched session, doesn't abandon that cycle.
