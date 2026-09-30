# Peering (dangerously experimental)

Failover for the star. The alpha leads, as it always has. When it is gone for long enough, the first beta that is up takes the lead, and the other betas keep syncing through it. When the alpha comes back, it gets the lead back after one cycle as a follower. Nothing in reconciliation changes.

> **Do not enable peering unless you accept the issues below.** A September 2026 review found security issues in peering that are not fixed in this release. Any peer that can lead can run commands on every other peer, and on the alpha's machine through one path. See [Known security issues](#known-security-issues). The collision issues the same review found, where two leaders could write one root or failover could stall, are fixed.

**Dangerously experimental** means three things here:
- The design is new, and the modes carry the words in their names so a configuration says so on its face.
- The on-disk state under `peering/` may change shape between releases without a migration.
- Peering trusts every peer that can lead with every other peer, including the alpha's machine. Use it only among machines that already trust each other with a shell.

The old names, `peering-conflict-experimental`, `peering-alpha-experimental` and `[advanced.peering-experimental]`, are refused with a message pointing here. The rename is deliberate: turning peering on should mean reading this page.

## The configuration

A normal star, with a different mode word:

```toml
[groups.voltai]
mode  = "peering-conflict-dangerously-experimental"  # or peering-alpha-dangerously-experimental
alpha = "~/Workspace/Voltai"                         # this machine; the preferred leader
betas = [                                            # the peers, in failover order
  "ubuntu@fny.voltai.party:~/Workspace",
  "box2:~/Workspace",
]
```

`peering-conflict-dangerously-experimental` is `two-way-conflict` plus failover. `peering-alpha-dangerously-experimental` is `two-way-alpha` plus failover: the configured alpha's version wins wherever the alpha is involved, whoever leads at the time.

The alpha must be the machine the configuration runs on, and every beta must be another host. The mode is refused otherwise.

Timing lives where the alerter's does, correct as shipped:

```toml
[advanced.peering-dangerously-experimental]
ttl            = "30s"     # a lease is stale this long after its last renewal
failover_after = "120s"    # a candidate waits this long past stale before it leads
```

`ttl` must be at least twice the group's `interval`, because a lease is renewed once per cycle. `failover_after` must not be shorter than `ttl`.

Every peer runs `autobahn watch` (or `autobahn install`) with **no configuration of its own**. The leader pushes it everything it needs.

## How it works

### One word: lease

The leader's claim on a host is a small file the leader renews on every cycle, through the agent, at `~/.autobahn/peering/lease.json`:

```json
{ "leader": "alpha", "term": 7, "renewed_at": 1789544514, "ttl_seconds": 30 }
```

The term increases by one at every change of leader. **The agent refuses writes from a controller whose term is below the lease's**, and from a different leader at the same term. That refusal — the fence — is the whole safety argument. Reads still answer, so a fenced controller can see the tree it may no longer change.

The agent checks the fence on every write, not only when a lease is presented. A write is refused once the host's lease names another leader or term, and once the lease has gone a whole `ttl` without renewal. That lifetime is measured from when this host received the lease, by its own clock, so skew between the leader's clock and the host's never matters. The check holds a lock for the length of the write, so a new lease is admitted between two writes, never during one.

Admitting a lease is one step under the same lock: the host reads its lease, checks the one presented, and writes it, flushed to disk, before answering. Of two leases presented at once, exactly one is admitted.

A leader re-presents its lease once a third of `ttl` has passed: before its next write, and while it waits for changes. A leader that pauses for longer comes back to a refusal, presents its lease again, and learns in one round trip whether it still leads.

### What the leader pushes

On every cycle the leader keeps each peer able to lead:

| pushed | so that |
|---|---|
| the lease | the peer knows who leads, and the agent can fence |
| `config.toml`, `ignores/` | the peer knows the star |
| `names/<group>`, `name` | the peer can find itself in each group's star; two groups can reach one host at two roots |
| `sessions/<group>` | the peer knows the session's identifier |
| the ancestor's journal records | the peer holds a copy of the last agreed state |

The last one is what makes a takeover clean. A leader without an ancestor reconciles two trees with no history and calls every difference a conflict. With the copy, the new leader continues the same three-way session the old one ran. The copy is written with the ancestor's own durability, never through the scan cache.

### The takeover

A peer reads its lease every interval. While the lease is fresh, or stale for less than the peer's wait, nothing happens. The wait is `failover_after` plus one `ttl` for every beta ahead of this one in the configuration's order — so the first live beta acts first, and nobody has to be asked. A blip never reaches the wait.

When it is time, the peer writes a lease at the next term, and runs the leader's configuration turned around: itself as the alpha, every other beta as a beta. The configuration is the one last pushed: the peer derives its star again every interval while it follows, so a change the leader pushed meanwhile is the one the takeover runs. Its sessions present the new lease to each host on their first cycle. A host a newer term has already taken refuses it, and the peer steps down and follows again.

A peer whose own lease names it leads at once at that term: it led and restarted, or the lead was handed to it.

Two candidates acting at once — clocks a lifetime apart, say — present the same term to the same hosts. Each host keeps the first and refuses the second, because it admits a lease in one step under a lock.

### The alpha is never dialed

The alpha is the one member that may be behind NAT, asleep, or on hotel wifi, so peering never assumes it can be reached. It dials. As leader it dials the betas, as it always has. While a beta leads, the alpha dials *the leader* and attaches: `ssh <leader> autobahn peering attach` bridges the alpha's own agent loop to the leading peer's attach socket, and the peer's supervisor takes that connection as the alpha's side of their session. The direction of the connection and the direction of the sync are independent. The alpha's groups that do not peer keep running meanwhile: they are the alpha's alone, whoever leads the star.

The session keeps the alpha on the alpha side, under the identifier the leader pushed, so the ancestor copy is the same session's and is adopted by whichever side leads next.

### The handoff

The lead goes back to the alpha on its own: one settled cycle on the attached session hands it over at the next term. `autobahn peering yield --to alpha` does the same on request, from the leading peer. A handoff writes the local lease first, every running peering session hands the new lease to its peer on its next attempt, and then the supervisor follows. A paused session is not waited for. A restart in the middle comes back as a follower; the member the lease names leads as soon as the lease reaches it, and otherwise the timeout takes it from there.

## What `status` shows

On the alpha, the group line carries the role and the term:

```
~/Workspace/Voltai  voltai  leader (term 7)
```

On a peer, a line above the star says what the peer is doing about the lease:

```
peer ubuntu@fny.voltai.party:~/Workspace: alpha leads at term 7; the lease is fresh
```

While a peer follows, its sessions are in the state `following`. That is not trouble, and the alerter never wakes anyone for it.

## Verbs

```sh
autobahn peering yield --to alpha      # from the leading peer: hand the lead back now
autobahn peering yield --to <spec>     # ...or to a named beta of the star; any other name is refused
autobahn peering attach                # what the alpha runs over ssh; not for typing
```

## Known security issues

These are open and will not be fixed before peering leaves this status.

**Every peer that can lead is trusted like a shell on every other peer.** A leader pushes its configuration to every follower, and a follower runs it when it takes the lead. Two consequences follow:
- **Pushed commands.** A pushed group's `agent_command` is kept as written. A follower runs it when it next leads, even after the leader that pushed it has gone.
- **Pushed roots.** A follower's own root comes from the `name` file the leader pushes. A leader can point a follower at any of that follower's directories.

In the usual setup this grants nothing new, because a leader already holds an SSH login to every peer. It does mean you should peer only machines that would each trust the others with a shell. SSH keys locked to `command="autobahn agent"` do not contain a peering leader, and are not supported with peering.

**While a beta leads, it can change what the alpha syncs with it, and nothing else there.** When the alpha attaches, it serves the leader only its own peering sessions, as its own configuration has them: the root, ignores, modes, owners and staging are the alpha's, whatever the leader asks for, and the alpha takes no pushed files. A leader can change files inside those roots, as any beta can in a two-way mode, and nothing outside them.

**A leader can send false ancestor history.** The replicated ancestor is taken as the record of the last agreed state. A dishonest leader can use it to steer later reconciliation into wrong changes inside the synced tree. This is the same boundary as a dishonest agent in any mode; see [Safety](./safety.md).

## What is not covered

- **A one-off `sync` or `resolve` is not fenced.** A channel that never presents a lease is never refused, so a command run by hand from any machine can still write a peer. Peering trusts the operator here.
- **A beta that cannot reach another beta.** Every peer must be able to reach every other with the specs in the configuration. Two servers that need a tunnel between them are a follow-up (`peer_ssh_config`).
- **A beta's sessions with the other betas start without an ancestor.** The alpha's session with each beta is replicated; a session between two betas exists only during a failover and has no history to inherit. If the two betas were in step when the alpha left, they agree; whatever was in flight shows as a handful of conflicts, never as loss.
- **A peer runs the pushed configuration, or its own — not both.** A machine with a configuration of its own runs that, and is not a peer: a pushed `name` beside it is ignored, with a warning at startup.
- **The menu bar app and `mi`** show the role only through `status`.
- **Clocks.** Staleness is judged on the follower's clock against the leader's `renewed_at`. Seconds of skew do not matter; minutes do.

## Under the hood

`src/peering.rs` holds the lease, the pushed files and the ancestor copy; `src/supervisor/peer.rs` the follow/lead state machine of a peer and of the alpha; the protocol requests are `Lease`, `AncestorRecord`, `AncestorCheckpoint`, `PutPeeringFile` and `PeeringState`. The compatibility epoch moved to 12 with them.
