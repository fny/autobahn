# PEER-8: Host identity, stale follower config, handoff and backoff

**Findings:** M-43 (ASTRA F21), M-44 (ASTRA F22), M-45 (OPUS, related peering bugs).
**Status:** deferred, not in v1. Documented in `docs/peering.md`.

## Problem

- **One host-wide identity.** Groups aimed at `host:/a` and `host:/b` push different specs into one `peering/name` file (`src/supervisor/mod.rs:431`, `src/peering.rs:510`). The last push wins, and failover then covers only the matching groups.
- **Stale config at takeover.** A follower derives its config before entering its follow loop (`src/supervisor/peer.rs:45`). The loop re-reads leases but never the config, so pushes made while it follows are ignored at takeover.
- **Handoff and timing bugs:**
  - Handoff counts every plan, but plain and paused plans never report handed (`src/supervisor/mod.rs:304-318`, `:493-497`). So handoff never completes when those exist.
  - Plain groups stop while the alpha follows, because `Role::Follower` only attaches (`peer.rs:291-320`).
  - `peering yield --to` never checks its target.
  - Session backoff, up to 300 s plus jitter, can outlast the takeover wait. A healthy alpha then loses the lead after a blip.
  - `for_alpha` and the yield paths use `DEFAULT_PEERING_TTL` instead of the configured TTL.

## Proposed resolution

- Push identity per group, as `name/<group>`, instead of one host-wide `name`.
- Re-derive the star from pushed state immediately before committing a takeover.
- Count only peering plans that are running when judging handoff, and keep plain groups running while the alpha follows.
- Validate `--to` against the star's members.
- Cap backoff for peering sessions below `ttl`. Renew the lease on a timer (see PEER-6), so backoff no longer affects it.
- Pass the configured TTL through every lease constructor.

## Tests

- Two groups on one host both fail over.
- A config pushed while following is the one used at takeover.
- Handoff completes with a paused session present.
- `yield --to` with an unknown target is refused.
- A 90 s outage does not cause a takeover.
