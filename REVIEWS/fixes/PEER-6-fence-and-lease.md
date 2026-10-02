# PEER-6: Make the fence hold, and make taking a lease atomic

**Findings:** H-9 (ASTRA F04, OPUS H7, GLM L5), H-10 (ASTRA F05, OPUS H7), M-42 (ASTRA F20).
**Status:** fixed, 2026-09-30. Two departures from the proposal:
- A lease lapses a `ttl` after the agent *received* it, by the agent's clock, rather than a `ttl` after the leader's `renewed_at`. Skew between the two clocks then never matters.
- The controller re-presents its lease once a third of `ttl` has passed, before its next write and while it waits for changes, rather than on a separate timer. A refusal that says "fenced" is followed by one fresh presentation, so a lapse costs one cycle and a takeover ends the cycle at once.

Also here, from PEER-8: peering sessions cap their backoff at half a `ttl`, so a network blip never outlasts the takeover wait.

## Problem

- **The fence is checked only when a lease is presented.** `fence` is set only when a `Request::Lease` is refused (`src/transport/mod.rs:537-590`). Writes on a channel whose lease was already accepted are never checked again. The leader renews only once per cycle, so a paused or slow leader keeps writing after a takeover.
- **Taking a lease is not atomic.** Admission reads the lease, checks it and writes it with no lock. Two candidates can both be accepted, and a delayed lower-term write can overwrite a higher one. There is no fsync.
- **Temporary names collide.** Peering state goes through `.<name>.<pid>.tmp` (`src/peering.rs:267`). Two channels in one agent share that name.

## Proposed resolution

- **Check the lease on every write.** Each channel remembers the leader and term it was accepted at. Every write request (`Transition`, `StagePush`, `Rename`, `AncestorRecord`, `AncestorCheckpoint`, `PutPeeringFile`) re-reads the host's lease under a shared lock and refuses the write if the lease has moved on.
- **Lock admission.** Wrap it in an exclusive `flock` on `peering/lease.lock`, and fsync the file and its directory before answering.
- **Renew on a timer,** not once per cycle.
- **Expire on the agent's own clock.** The agent refuses writes once `renewed_at + ttl` has passed by its clock.
- **Unique temporaries.** Create them with `create_new` and a random suffix.

## Tests

- After a takeover, a write on the old channel is refused without that channel presenting a new lease.
- Two admissions at the same term, released together from a barrier, accept exactly one.
- A delayed lower-term write does not replace a higher term.
- Concurrent pushes of the same file from two channels do not corrupt it.
