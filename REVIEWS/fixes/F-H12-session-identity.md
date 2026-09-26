# F-H12: Sessions are told apart by identity, not by display label

**Findings:** H-12 (ASTRA F07). L-30 (OPUS Low) has the same root cause and is fixed here too.
**Status:** proposed. High; fix before v1.

## Problem

`Config::plans()` checks writable endpoints for nesting *across* sessions (`src/config.rs:1366-1400`). It skips pairs from the same session, because the within-session check covers those. But it decides "same session" by comparing `plan.display()`, which is `format!("{}@{}", group, host)` (`:630-632`):

```rust
if owner == other_owner {
    continue; // within-session overlap is checked above
}
```

Two betas in one group on the same host, such as `host:/tree` and `host:/tree/nested`, both display as `group@host`. They are different sessions with separate ancestors, and one writes inside the other's root, yet the check skips them.

The same label is also used as an identity in about 17 other places: progress, alerts, `select`, the shop and the tray. So two betas on one host get mixed up there too. For example, progress is keyed by `(group, host)` (`src/main.rs:3249`, L-30), so the two sessions show each other's progress.

## Proposed resolution

- **The overlap check uses real identities.** Replace `plan.display()` with the plan's index, or `plan.identifier()`, which is unique per endpoint pair.
- **A typed session key for internal bookkeeping.** Carry a `SessionKey` (the session identifier) through control requests, progress, alert state and the status inventory. Use `display()` only for text shown to people.
- **Labels that tell sessions apart.** When two plans in one group share a host, show the path too, as in `group@host:/tree` and `group@host:/tree/nested`, in status, the shop and the tray.
- **An exhaustive match for writability.** The `writable` closure uses `matches!` over the two-way modes, so a newly added mode would count as read-only and escape the check. Switch to a `match` with no wildcard arm, so the compiler flags a new mode.

## Tests

- A config whose one group has two betas `host:/tree` and `host:/tree/nested`, both writable, is refused by `plans()`.
- The same config in a one-way mode, where the betas are read-only in the relevant direction, follows the existing rule, whatever that allows.
- Two betas on one host in one group show separate progress and separate lines in `status`.
- A new unit test checks that every `SyncMode` variant is classified.
