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

- [x] `Request::Lease(Lease)` / `Response::Lease(LeaseAnswer)` — renew or fence
- [x] `Request::AncestorRecord { generation, changes }` / `Response::Recorded { generation }`
- [x] `Request::AncestorCheckpoint { generation, ancestor }` for a peer with no copy or a mismatched generation
- [x] `Request::PutPeeringFile { name, bytes }` for `config.toml`, `ignores/<name>`, `name`
- [x] `Request::PeeringState` / `Response::PeeringState` — what the agent holds (lease, generation per session)
- [x] Agent: `~/.autobahn/peering/` (honours `AUTOBAHN_HOME`); `lease.json` read at every `Lease` request, so channels see each other's terms through the file
- [x] Fence: a channel whose lease term is below the stored term gets every write refused with a named error (`Transition`, `StagePush`, `Rename`, and the peering writes); `Scan` still answered. A same-term claim by a different leader is refused too
- [x] Agent-side `AncestorStore` copy under `peering/ancestors/<session>/` (`peering::AncestorCopy`)
- [x] `COMPATIBILITY_EPOCH` 11 → 12; `response_kind` names the new variants
- [x] Tests: unit tests in `peering.rs`; `peering_fence_and_ancestor_copy_over_the_wire` in the e2e suite against a real agent
- Residual: a channel that never presents a lease is not fenced, so a one-off `sync` or `resolve` from any machine can still write a peer. Peering trusts the operator here; noted in the docs

## Phase 3 — the leader

- [x] `Supervisor` knows its role (`PeeringContext`, `peering::Role`): `Leader { leader, term }` / `Follower` / `Off`; the alpha resumes its term from its own `lease.json`, or follows if that file names a beta
- [x] Lease presented on every cycle start, on the session's own channel (`Session::present_lease`); once per session the beta's copy generation is compared and a checkpoint sent if it differs
- [x] `Lease` refused → `peering::Fenced`; the supervisor steps down for every session, records the beta's lease locally, and the status says `following`
- [x] After the ancestor advances: `AncestorRecord` to the beta; `AncestorCheckpoint` when the copy reports another generation. Best effort — a miss costs a checkpoint later, never the cycle
- [x] `config.toml`, every file in `ignores/`, and `name` pushed after the lease is accepted, whenever their digest changes
- [x] `role` and `term` in `SessionStatus`, on the group in `status --json`, and on the group line of the `status` text
- [x] Tests (supervisor suite, real agents under their own `HOME`): the lease, files and copy reach the agent; a fenced leader steps down and stays down across a restart

## Phase 4 — the follower and the takeover

- [x] `autobahn watch` on a host whose `peering/name` exists runs `supervisor::peer::run` from `peering/config.toml`; a configuration of its own alongside is refused, not ignored
- [x] The follower reads `peering/lease.json` each interval and writes `peering/follower.json`; stale for its wait → candidate. `autobahn status` on a peer shows the pushed star and the lease's standing
- [x] Candidate order by stagger, not by asking: position *n* waits `failover_after + (n − 1) × ttl`. Two candidates at once are settled by the fence (same term, second leader refused)
- [x] Takeover: `term + 1`, lease on self (renewed every `ttl / 2` while leading), then a supervisor whose sessions present the lease to every other beta on their first cycle
- [x] Plans re-derived (`peering::derive_star`): own spec → local path, other betas unchanged, plain groups dropped. The configured alpha is *not* in the star — it dials in (phase 5)
- [x] Ancestors seeded from `peering/ancestors/` (`peering::adopt_newer_copy`, under the session lock, whenever the copy is newer): a beta that leads seeds the (alpha, me) session; an alpha that gets the lead back adopts what the beta recorded. A (me, other) pair starts without one
- [x] `peering-alpha-experimental`: the configured alpha stays the alpha of every pair it is in — the attached session keeps the alpha on the alpha side and the leader pushes `sessions/<group>` so the identifier is the same
- [x] Tests: `a_follower_turns_the_star_around` (unit); `a_peer_takes_the_lead_when_the_lease_goes_stale` (supervisor suite, real agents) — the takeover reaches the other beta with lease, name and config, and the returning old leader is fenced and steps down

## Phase 5 — the alpha dials in, and the handoff

- [x] `autobahn peering attach`: on the leader host, a bridge from stdio to the leading peer's `peering/attach.sock` (its own socket, not the control socket; the greeting is the peer's name)
- [x] The alpha runs `ssh <leader> autobahn peering attach` (or `AUTOBAHN_PEERING_ATTACH`) with its own agent loop on the ssh stdio (`transport::attach_as_agent`); `supervisor::peer::run_alpha` is the alpha's lead ↔ attach state machine
- [x] The supervisor accepts an attached agent as the endpoint of the pair with that peer: `<name>@attached:<path>` specs, an attachment slot in the agent pool, `Unreachable` until the alpha dials in
- [x] `autobahn peering yield --to <spec>` over the control socket; every session hands the new lease to its peer on its next attempt, then the supervisor follows. The local lease names the new leader at once, and a leading peer renews only a lease that still names it
- [x] The lead goes back to the alpha automatically: one settled cycle on the attached session yields to `alpha`; no ask is needed
- [x] `ttl` must be at least twice the group's `interval` — the lease is renewed once per cycle
- [x] Tests: `the_alpha_attaches_to_a_leading_peer_and_gets_the_lead_back` runs the whole loop with real agents: fenced, attached, synced both ways, handed back, leading and renewing again, the ancestor copy adopted. A crash mid-yield is covered by design (the local lease names the next leader; the timeout takes it from there), not by a test

## Phase 6 — docs and the rest

- [x] `docs/peering.md`: the design, the config, what "experimental" means here, what is not covered
- [x] `docs/modes.md`: the peering row and a section; `docs/configuration.md`: the section row; `docs/README.md`: the index row
- [x] `docs/state.md`: the `peering/` directory
- [x] The epoch bump is noted in `docs/peering.md` (`releases.md` and `development.md` are in flight elsewhere)
- [x] `run_clean` leaves `peering/` alone — it never removes directories it does not own, so nothing to add; said in `state.md`
- [ ] Menu bar and `mi`: role shown on the group line (`tray.rs` and `shop.rs` are in flight elsewhere; `status` shows it)
- [ ] Remove this file once the feature ships, or move what is left into `TODO.md`

## Not in this build

- Reaching a beta from another beta when the configured spec does not work there (`peer_ssh_config`, tailcat)
- The menu bar app driving a remote leader
- More than one group in peering mode on the same machines (allowed, untested)
