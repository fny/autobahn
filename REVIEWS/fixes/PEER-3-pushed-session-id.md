# PEER-3: Validate pushed session identifiers

**Findings:** H-22 (KIMI ABN-H5).
**Status:** deferred, not in v1. This is a tier 1 confinement break, but it is reachable only with peering enabled. Documented in `docs/peering.md`.

## Problem

`derive_star` reads `sessions/<group>` and uses its trimmed contents as the session identifier (`src/peering.rs:570-575`). `is_pushable` checks pushed file *names*, not their *content*. A value like `../../x` places the follower's session directory, lock, ancestor store and status files outside `~/.autobahn`.

## Proposed resolution

Reuse `is_session_identifier` from [T1-2](T1-2-initialize-identifiers.md). Apply it in three places:
- where `derive_star` reads the pushed identifier;
- in `SessionPlan::attached_alpha`;
- inside `ancestor_copy_path`, which T1-2 already changes to return a `Result`.

Refuse the whole derived star when an identifier is invalid, with an error naming the group.

## Tests

- A pushed `../../x` identifier makes `derive_star` fail, and nothing is created outside the state root.
- A genuine 32-hex identifier derives as before.
