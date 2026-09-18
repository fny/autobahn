# Peering (experimental)

Failover for the star. The alpha leads, as it always has. When it is gone for long enough, the first beta that is up takes the lead, and the other betas keep syncing through it. When the alpha comes back, it gets the lead back after one cycle as a follower. Nothing in reconciliation changes.

**Experimental** means three things here. The design is new and the modes carry the word in their names so a configuration says so on its face. The on-disk state under `peering/` may change shape between releases without a migration. And a few edges are known and left open — they are listed at the end.

## The configuration

A normal star, with a different mode word:

```toml
[groups.voltai]
mode  = "peering-conflict-experimental"      # or peering-alpha-experimental
alpha = "~/Workspace/Voltai"                 # this machine; the preferred leader
betas = [                                    # the peers, in failover order
  "ubuntu@fny.voltai.party:~/Workspace",
  "box2:~/Workspace",
]
```

`peering-conflict-experimental` is `two-way-conflict` plus failover. `peering-alpha-experimental` is `two-way-alpha` plus failover: the configured alpha's version wins wherever the alpha is involved, whoever leads at the time.

The alpha must be the machine the configuration runs on, and every beta must be another host. The mode is refused otherwise.

Timing lives where the alerter's does, correct as shipped:

```toml
[advanced.peering-experimental]
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

The term increases by one at every change of leader. **The agent refuses writes from a controller whose term is below the lease's**, and from a different leader at the same term. That refusal — the fence — is the whole safety argument. No root is ever written by two controllers, whatever the network does. Reads still answer, so a fenced controller can see the tree it may no longer change.

### What the leader pushes

On every cycle the leader keeps each peer able to lead:

| pushed | so that |
|---|---|
| the lease | the peer knows who leads, and the agent can fence |
| `config.toml`, `ignores/` | the peer knows the star |
| `name` | the peer can find itself in the star |
| `sessions/<group>` | the peer knows the session's identifier |
| the ancestor's journal records | the peer holds a copy of the last agreed state |

The last one is what makes a takeover clean. A leader without an ancestor reconciles two trees with no history and calls every difference a conflict. With the copy, the new leader continues the same three-way session the old one ran. The copy is written with the ancestor's own durability, never through the scan cache.

### The takeover

A peer reads its lease every interval. While the lease is fresh, or stale for less than the peer's wait, nothing happens. The wait is `failover_after` plus one `ttl` for every beta ahead of this one in the configuration's order — so the first live beta acts first, and nobody has to be asked. A blip never reaches the wait.

When it is time, the peer writes a lease at the next term, and runs the leader's configuration turned around: itself as the alpha, every other beta as a beta. Its sessions present the new lease to each host on their first cycle. A host a newer term has already taken refuses it, and the peer steps down and follows again.

Two candidates acting at once — clocks a lifetime apart, say — present the same term to the same hosts. Each host keeps the first and refuses the second. The fence, not the stagger, is the guarantee.

### The alpha is never dialed

The alpha is the one member that may be behind NAT, asleep, or on hotel wifi, so peering never assumes it can be reached. It dials. As leader it dials the betas, as it always has. While a beta leads, the alpha dials *the leader* and attaches: `ssh <leader> autobahn peering attach` bridges the alpha's own agent loop to the leading peer's attach socket, and the peer's supervisor takes that connection as the alpha's side of their session. The direction of the connection and the direction of the sync are independent.

The session keeps the alpha on the alpha side, under the identifier the leader pushed, so the ancestor copy is the same session's and is adopted by whichever side leads next.

### The handoff

The lead goes back to the alpha on its own: one settled cycle on the attached session hands it over at the next term. `autobahn peering yield --to alpha` does the same on request, from the leading peer. A handoff writes the local lease first, every session hands the new lease to its peer on its next attempt, and then the supervisor follows. A restart in the middle comes back as a follower, and the timeout takes it from there.

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
autobahn peering yield --to <spec>     # ...or to a named beta
autobahn peering attach                # what the alpha runs over ssh; not for typing
```

## What is not covered

- **A one-off `sync` or `resolve` is not fenced.** A channel that never presents a lease is never refused, so a command run by hand from any machine can still write a peer. Peering trusts the operator here.
- **A beta that cannot reach another beta.** Every peer must be able to reach every other with the specs in the configuration. Two servers that need a tunnel between them are a follow-up (`peer_ssh_config`).
- **A beta's sessions with the other betas start without an ancestor.** The alpha's session with each beta is replicated; a session between two betas exists only during a failover and has no history to inherit. If the two betas were in step when the alpha left, they agree; whatever was in flight shows as a handful of conflicts, never as loss.
- **A peer runs the pushed configuration, or its own — not both.** A configuration of its own alongside a pushed one is refused at startup.
- **The menu bar app and `mi`** show the role only through `status`.
- **Clocks.** Staleness is judged on the follower's clock against the leader's `renewed_at`. Seconds of skew do not matter; minutes do.

## Under the hood

`src/peering.rs` holds the lease, the pushed files and the ancestor copy; `src/supervisor/peer.rs` the follow/lead state machine of a peer and of the alpha; the protocol requests are `Lease`, `AncestorRecord`, `AncestorCheckpoint`, `PutPeeringFile` and `PeeringState`. The compatibility epoch moved to 12 with them.
