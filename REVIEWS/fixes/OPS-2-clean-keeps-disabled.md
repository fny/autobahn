# OPS-2: `clean` keeps the state of disabled sessions

**Findings:** M-22 (ASTRA F13; confirmed with `clean --dry-run`).
**Status:** proposed.

## Problem

`clean` decides what state to keep from the *active* plans (`src/main.rs:2839`). A disabled group or host isn't in those plans, so `clean` deletes its ancestor, session directory, status record and endpoint lock.

The docs say enabling a group resumes where it left off. After `clean`, re-enabling starts with no ancestor. That can bring back files that were deleted, or raise avoidable conflicts.

## Proposed resolution

- **Keep configured sessions.** Build the keep-set from every session the config *describes*, disabled or not.
- **Purge on request.** Add `clean --include-disabled`, which deletes disabled sessions' state after a confirmation, as `clean` already asks for other deletions.
- **Unclear ownership.** A disabled entry whose settings no longer validate is kept, and reported as "kept: could not tell what it belongs to."

## Tests

- Sync, disable, `clean`, enable, sync. Check that the ancestor survives and that a file deleted before the disable is not brought back.
- `clean --include-disabled --dry-run` lists the disabled session. A plain `clean --dry-run` doesn't.
