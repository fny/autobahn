# PEER-2: The attached alpha refuses pushed files

**Findings:** new, found during the 2026-09-24 walkthrough. It is not in any of the six reviews.
**Status:** deferred, not in v1. Documented in `docs/peering.md` with a workaround.

## Problem

A genuine leader never pushes files to the attached alpha. The code skips it deliberately, with the comment "the attached alpha gets no files: it has its own" (`src/supervisor/mod.rs:1024`). But the alpha's agent loop still accepts `PutPeeringFile`, subject only to the `is_pushable` filename allowlist.

A hostile leader can push `name` and `config.toml` into the alpha's `~/.autobahn/peering/`:
- On the next start, `is_peer` is true and the alpha's own config also exists. Startup refuses (`src/main.rs:1501`).
- The refusal says "move one of them aside."
- If the user moves their own config aside, the alpha comes up as a follower running the attacker's config. At failover it runs the attacker's `agent_command`.

## Proposed resolution

- Under the PEER-1 attach policy, refuse `PutPeeringFile`. Keep accepting the lease and ancestor records, which the handback needs.
- At startup, a machine with its own configuration ignores a stray `peering/name` with a warning, rather than refusing to start.
- Reword the refusal so it never suggests moving the user's own configuration aside.

## Tests

- `PutPeeringFile` over an attached connection is refused, and nothing is written.
- A stray `peering/name` next to a real config starts normally and logs a warning.
