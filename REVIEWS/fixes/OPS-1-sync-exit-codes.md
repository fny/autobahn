# OPS-1: `sync` exit codes say whether it converged

**Findings:** M-36 (OPUS M13).
**Status:** proposed.

## Problem

`docs/commands.md:129` recommends `autobahn sync` for "scripts that need a sync that converges and *exits* with a status code." But `sync` exits `0` even when conflicts or blocked paths remain (`src/main.rs:952-986`, `:1165-1190`). A script can't tell "in sync" from "stopped with conflicts".

## Proposed resolution

- **Three exit codes:**

  | Code | Meaning |
  |---|---|
  | `0` | Every session converged, with no conflicts and no blocked paths. |
  | `1` | An error stopped a session: unreachable, halted, a bad configuration. |
  | `2` | Every session finished its pass, but conflicts or blocked paths remain. |

- **More than one session.** When a run covers several sessions, the exit code is the worst case among them: `1` beats `2`, which beats `0`.
- **`resolve`.** Consider the same scheme where a path couldn't be settled.
- **Docs.** List the codes in `docs/commands.md`, under "One-off syncs and scripting".

## Tests

- A CLI test for each code: a clean pair, a pair with one conflict, and a pair with an unreachable beta.
- A two-session run where one session conflicts and one errors exits `1`.
