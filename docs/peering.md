# Peering (dangerously experimental)

Failover for the star. The alpha leads, as it always has. When it is gone for long enough, the first beta that is up takes the lead, and the other betas keep syncing through it. When the alpha comes back, it gets the lead back after one cycle as a follower. Nothing in reconciliation changes.

> **Do not enable peering unless you accept the issues below.** A September 2026 review found security and collision issues in peering that are not fixed in this release. Any peer that can lead can run commands on every other peer, and on the alpha's machine through one path. Two leaders can write the same root at once in some timings. See [Known security issues](#known-security-issues) and [Known collision issues](#known-collision-issues).

**Dangerously experimental** means four things here:
- The design is new, and the modes carry the words in their names so a configuration says so on its face.
- The on-disk state under `peering/` may change shape between releases without a migration.
- The fence described below does not yet hold in every timing. Peering can let two controllers write one root.
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

The design intent is that no root is ever written by two controllers, whatever the network does. The current code does not meet it. The fence is checked only when a controller presents a lease, not on every write, and taking a lease is not atomic. [Known collision issues](#known-collision-issues) lists the timings where two controllers can write.

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

Two candidates acting at once — clocks a lifetime apart, say — present the same term to the same hosts. The intent is that each host keeps the first and refuses the second. Today a host can accept both, because it reads, checks and writes the lease without a lock. See [Known collision issues](#known-collision-issues).

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

## Known security issues

These are open and will not be fixed before peering leaves this status.

**Every peer that can lead is trusted like a shell on every other peer.** A leader pushes its configuration to every follower, and a follower runs it when it takes the lead. Two consequences follow:
- **Pushed commands.** A pushed group's `agent_command` is kept as written. A follower runs it when it next leads, even after the leader that pushed it has gone.
- **Pushed roots.** A follower's own root comes from the `name` file the leader pushes. A leader can point a follower at any of that follower's directories.

In the usual setup this grants nothing new, because a leader already holds an SSH login to every peer. It does mean you should peer only machines that would each trust the others with a shell. SSH keys locked to `command="autobahn agent"` do not contain a peering leader, and are not supported with peering.

**While a beta leads, it controls the alpha's machine more than it should.** When the alpha attaches, it runs the full agent for the leader over a connection the alpha opened. The leader never needed any access to the alpha, yet today it can:
- choose which directory on the alpha to sync, with any ignores, symlink handling and permissions;
- push files into the alpha's `~/.autobahn/peering/`. A genuine leader never does this. A pushed `name` file makes the alpha refuse to start alongside its own configuration. If you then move your configuration aside as the error suggests, the alpha runs the pushed configuration instead, including its commands. If this happens, delete `~/.autobahn/peering/name` and keep your own configuration.

A laptop that attaches to a leading beta therefore trusts that beta with the laptop user's files.

**A pushed session identifier can escape `~/.autobahn`.** A follower uses the pushed `sessions/<group>` content as a directory name after trimming whitespace, without other checks. A malicious leader can place session state, locks and status files elsewhere in the follower's home directory.

**A leader can send false ancestor history.** The replicated ancestor is taken as the record of the last agreed state. A dishonest leader can use it to steer later reconciliation into wrong changes inside the synced tree. This is the same boundary as a dishonest agent in any mode; see [Safety](./safety.md).

**The attach socket does not check who connects.** On the leading beta, `~/.autobahn/peering/attach.sock` has no peer-credential check, no explicit permissions and no timeout. Unlike the control socket, it accepts any local process that can connect and send the alpha's greeting. On a multi-user host, another local user could pose as the alpha. Do not run a peering beta on a machine shared with users you do not trust.

## Known collision issues

These can let two controllers write one root, or leave a peer stuck, in some timings.

- **An accepted channel is not fenced after a takeover.** A channel whose lease was accepted keeps writing until it next presents a lease. A leader that pauses mid-transfer, or runs a cycle longer than `ttl` plus `failover_after`, can keep writing after another peer has taken the lead.
- **Taking a lease is not atomic.** Two candidates can both be accepted at the same term. A delayed write at a lower term can overwrite a higher one. Lease files are not fsynced, so a power loss can roll the term back.
- **A newer ancestor can be replaced by an older replica.** If a peer's ancestor still lives only in its journal, adoption treats it as generation zero. An older replica then overwrites it, losing the history that tells a deliberate edit from an unchanged file.
- **Temporary file names collide.** Peering state files are written through temporary names built from the process id only. Two channels in one agent can overwrite each other's lease, configuration or name file.
- **Several peering groups share one identity.** Groups aimed at different roots on one host push different `name` files to the same place. The last push wins, so failover covers only part of the groups.
- **A follower takes over with an old configuration.** A follower reads the pushed configuration once, before it starts following. Changes pushed while it follows are ignored at takeover.
- **Handoff can stall or go to the wrong peer.**
  - Handoff never completes while the leader has plain groups or a paused session.
  - Plain groups stop while the alpha follows.
  - `peering yield --to <beta>` does not check its target, so a typo leaves nobody leading until the timeout.
  - Some leases ignore the configured `ttl`.
- **A healthy alpha can lose the lead.** Session backoff can reach several minutes, longer than the takeover wait. After a network blip of about a minute, a beta can take over from an alpha that is fine.
- **An oversized ancestor record can wedge a follower.** A record over the 1 GiB read limit is still written, and every later open of that ancestor then fails until `reset`.

## What is not covered

- **A one-off `sync` or `resolve` is not fenced.** A channel that never presents a lease is never refused, so a command run by hand from any machine can still write a peer. Peering trusts the operator here.
- **A beta that cannot reach another beta.** Every peer must be able to reach every other with the specs in the configuration. Two servers that need a tunnel between them are a follow-up (`peer_ssh_config`).
- **A beta's sessions with the other betas start without an ancestor.** The alpha's session with each beta is replicated; a session between two betas exists only during a failover and has no history to inherit. If the two betas were in step when the alpha left, they agree; whatever was in flight shows as a handful of conflicts, never as loss.
- **A peer runs the pushed configuration, or its own — not both.** A configuration of its own alongside a pushed one is refused at startup.
- **The menu bar app and `mi`** show the role only through `status`.
- **Clocks.** Staleness is judged on the follower's clock against the leader's `renewed_at`. Seconds of skew do not matter; minutes do.

## Under the hood

`src/peering.rs` holds the lease, the pushed files and the ancestor copy; `src/supervisor/peer.rs` the follow/lead state machine of a peer and of the alpha; the protocol requests are `Lease`, `AncestorRecord`, `AncestorCheckpoint`, `PutPeeringFile` and `PeeringState`. The compatibility epoch moved to 12 with them.
