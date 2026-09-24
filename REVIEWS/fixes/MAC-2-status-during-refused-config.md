# MAC-2: `autobahn status` is unusable while a configuration edit is refused

**Findings:** new, from MAC-BENCH 1. **Status:** proposed, Low.

## Problem

Live reload does the right thing with a bad edit: the supervisor keeps the last good configuration, logs *configuration refused; the sessions keep running as before: …*, and every session goes on syncing. The app is expected to show a line saying so.

The CLI does not. `autobahn status` parses the configuration itself before it reads anything else, so while the file is broken it exits with the TOML error and prints nothing about the sessions:

```
autobahn: unable to parse configuration /Users/faraz/.autobahn/config.toml: TOML parse error at line 1, column 1
  |
1 | mdoe = "two-way-conflict"
  | ^^^^
unknown field `mdoe`, expected one of `reload`, `on_alert`, `disabled_hosts`, `disabled`, `log`, `defaults`, `groups`, `advanced`, `alerts`
```

So at the one moment a person most wants to see their fleet — they have just edited the file and want to know what it did — the command that answers that question is the one command that stops working. `issues`, `mi` and anything else that resolves a selector are in the same position.

Measured on macOS 26.5.1, Apple M4, commit `d7c2e21`, 2026-09-24, under `watch --log` in a throwaway state root. The supervisor kept syncing throughout: a file written after the refusal reached the beta six seconds later.

## Proposed resolution

`status` should fall back the way the supervisor does. When the configuration will not parse, read the recorded status files under the state root, print them, and lead with a line naming the refusal — the same text the supervisor logged. The exit code can stay non-zero so scripts still notice.

The plan list a session belongs to is only needed for filtering and for the alpha labels; both are already in the status records.

## Tests

- With a configuration that does not parse, `status` prints the sessions and a refusal line, and exits non-zero.
- With a configuration that parses but plans nothing, behaviour is unchanged.
- `status --json` carries the refusal as a field rather than failing.
