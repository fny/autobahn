# Peering — experimental

Failover for the star. The alpha leads; when it is gone for long enough,
the first beta that is up takes the lead; when the alpha returns, it gets
the lead back after one cycle as a follower. Nothing in reconciliation
changes. The design is in `docs/peering.md` (written in phase 6).

One word everywhere: the mode is `peering-*-experimental`, the section is
`[advanced.peering-experimental]`, the state lives in `~/.autobahn/peering/`,
the verbs are `autobahn peering …`. "Peer" is prose for one member.

Rules the build must keep:

- The alpha is never dialed. It dials, as leader or as follower.
- A host accepts writes from one controller: the one named in its lease,
  or any controller when no lease was ever written (the mode is off).
- The ancestor copy on a peer is written with ancestor durability, never
  through the scan-cache writer.
- Every phase compiles, passes its tests, and is one commit on `peering`.

## Phase 1 — the mode and the section

- [x] `SyncMode` gains nothing; `SessionPlan` gains `peering: Option<PeeringPlan>` — `None` for the plain modes
- [x] `parse_mode_spec` accepts `peering-conflict-experimental` → `TwoWaySafe` + peering, `peering-alpha-experimental` → `TwoWayResolved` + peering
- [x] `SessionPlan::mode_name` prints the peering spelling back for a peering plan; status and `mi` use it
- [x] `[advanced.peering-experimental]` with `ttl` (default 30s) and `failover_after` (default 120s), refused when `failover_after < ttl`
- [x] A peering group must have a local alpha and every beta on another host
- [x] CLI `--mode` (one-off `sync`): not applicable — a one-off has no leader; the spelling is refused there
- [x] `init` template names the two modes in the mode comment (the template test now counts `peering-*` too)
- [x] Unit tests: parse and print-back, section defaults and refusal, the local-alpha and remote-beta rules

## Phase 2 — the protocol

- [ ] `Request::Lease(Lease)` / `Response::Lease(LeaseAnswer)` — renew or fence
- [ ] `Request::AncestorRecord { generation, changes }` / `Response::Recorded { generation }`
- [ ] `Request::AncestorCheckpoint { generation, ancestor }` for a peer with no copy or a mismatched generation
- [ ] `Request::PutPeeringFile { name, bytes }` for `config.toml`, `ignores/<name>`, `name`
- [ ] `Request::PeeringState` / `Response::PeeringState` — what the agent holds (lease, generation per session)
- [ ] Agent: `~/.autobahn/peering/` (honours `AUTOBAHN_HOME`), `lease.json` read once per connection
- [ ] Fence: a channel whose lease term is below the stored term gets `Transition`, `StagePush`, `Rename` refused with a named error; `Scan` still answered
- [ ] Agent-side `AncestorStore` copy under `peering/ancestors/<session>/`
- [ ] `COMPATIBILITY_EPOCH` bump; `response_kind` names the new variants
- [ ] Tests: fence refuses a lower term and admits an equal or higher one; a record advances the copy; a checkpoint replaces it

## Phase 3 — the leader

- [ ] `Supervisor` knows its role: `Leader { term }` / `Follower` / `Off`
- [ ] Lease renewed on every peer at every cycle start, on the session's own channel
- [ ] `Lease` refused → the worker records `stepped-down`, the supervisor stops leading
- [ ] After a settled cycle: `AncestorRecord` to that session's beta; `AncestorCheckpoint` when the beta reports a mismatch
- [ ] `config.toml`, ignore files and `name` pushed on first contact and whenever their digest changes
- [ ] `role` and `term` in `SessionStatus`, `status --json`, and the `status` text
- [ ] Tests (supervisor suite, real agents): the record reaches the agent; a fenced agent stops a leader

## Phase 4 — the follower and the takeover

- [ ] `autobahn watch` on a host whose `peering/name` exists runs as a follower from `peering/config.toml`
- [ ] The follower reads `peering/lease.json` each interval; stale for `failover_after` → candidate
- [ ] Candidate order: the alpha, then the betas as listed; a candidate yields to a higher one that answers
- [ ] Takeover: `term + 1`, lease on self, lease on every reachable peer, then sessions with self as alpha
- [ ] Plans re-derived: own spec → local path, alpha → a remote spec of the alpha (only if reachable), other betas unchanged
- [ ] Ancestors seeded from `peering/ancestors/` for the pairs it holds; a pair it does not hold starts without one
- [ ] `peering-alpha-experimental`: the configured alpha stays the alpha of every pair it is in
- [ ] Tests: a stale lease promotes the first beta; a second beta waits for the first; a returning old leader is fenced

## Phase 5 — the alpha dials in, and the handoff

- [ ] `autobahn peering attach`: on the leader host, a bridge from stdio to the supervisor's control socket
- [ ] The follower alpha runs `ssh <leader> autobahn peering attach` with its own agent loop on the ssh stdio
- [ ] The supervisor accepts an attached agent as the endpoint of the pair with that peer
- [ ] `autobahn peering yield --to <spec>`: finish the cycle, push records, write `term + 1` for the named peer everywhere, step down
- [ ] The returning alpha asks for the lead once it has completed one settled cycle as a follower
- [ ] Tests: an attached agent carries a cycle; a yield moves the lead in one cycle; a crash mid-yield falls back to the timeout

## Phase 6 — docs and the rest

- [ ] `docs/peering.md`: the design, the config, what "experimental" means here, what is not covered
- [ ] `docs/modes.md`: the peering row; `docs/configuration.md`: the section row
- [ ] `docs/state.md`: the `peering/` directory
- [ ] `docs/releases.md` / `development.md`: the epoch bump note
- [ ] `run_clean` knows about `peering/`
- [ ] Menu bar and `mi`: role shown on the group line
- [ ] Remove this file once the feature ships, or move what is left into `TODO.md`

## Not in this build

- Reaching a beta from another beta when the configured spec does not work there (`peer_ssh_config`, tailcat)
- The menu bar app driving a remote leader
- More than one group in peering mode on the same machines (allowed, untested)
