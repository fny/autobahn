# Peering (dangerously experimental)

Failover for the star. The alpha leads, as it always has. When it is gone for long enough, the first beta that is up takes the lead, and the other betas keep syncing through it. When the alpha comes back, it gets the lead back after one cycle as a follower. Nothing in reconciliation changes.

> **Do not enable peering unless you accept the issues below.** A September 2026 review found security issues in peering that are not fixed in this release. Unless [restricted keys](#restricted-keys) are on, any peer that can lead can run commands on every other beta; the alpha's machine it can reach only through what the alpha syncs with it. See [Known security issues](#known-security-issues). The collision issues the same review found, where two leaders could write one root or failover could stall, are fixed.

**Dangerously experimental** means three things here:
- The design is new, and the modes carry the words in their names so a configuration says so on its face.
- The on-disk state under `peering/` may change shape between releases without a migration.
- Peering trusts every peer that can lead with the folders it syncs, on every member, and — unless [restricted keys](#restricted-keys) are on — with a shell on every other beta. Use it only among machines that already trust each other that far.

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
[experimental.peering-dangerously-experimental]
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

A host that comes to lead a session takes up the copy when the copy was written after its own history, since that is the later agreement. It compares when each was written rather than generation numbers: a copy that lagged when a beta took over carries on from where it lagged, so after a handback it can be the later record at the lower number. Both are on the one host and stamped by its clock, so the times compare. Before taking a copy up, the host checks who wrote it and what it says about the host's own files; see [Known security issues](#known-security-issues). Records are sent best effort, one per cycle; a copy that misses one is found out by the next, and brought level before any handoff.

### The takeover

A peer reads its lease every interval. While the lease is fresh, or stale for less than the peer's wait, nothing happens. The wait is `failover_after` plus one `ttl` for every beta ahead of this one in the configuration's order — so the first live beta acts first, and nobody has to be asked. A blip never reaches the wait.

When it is time, the peer writes a lease at the next term, and runs the leader's configuration turned around: itself as the alpha, every other beta as a beta. The configuration is the one last pushed: the peer derives its star again every interval while it follows, so a change the leader pushed meanwhile is the one the takeover runs. Its sessions present the new lease to each host on their first cycle. A host a newer term has already taken refuses it, and the peer steps down and follows again.

A peer whose own lease names it leads at once at that term: it led and restarted, or the lead was handed to it.

Two candidates acting at once — clocks a lifetime apart, say — present the same term to the same hosts. Each host keeps the first and refuses the second, because it admits a lease in one step under a lock.

### The alpha is never dialed

The alpha is the one member that may be behind NAT, asleep, or on hotel wifi, so peering never assumes it can be reached. It dials. As leader it dials the betas, as it always has. While a beta leads, the alpha dials *the leader* and attaches: `ssh <leader> autobahn peering attach` bridges the alpha's own agent loop to the leading peer's attach socket, and the peer's supervisor takes that connection as the alpha's side of their session. The direction of the connection and the direction of the sync are independent. The alpha dials in once, and every group it shares with the leader syncs over that one connection, each on channels of its own. The alpha's groups that do not peer keep running meanwhile: they are the alpha's alone, whoever leads the star.

The session keeps the alpha on the alpha side, under the identifier the leader pushed, so the ancestor copy is the same session's and is adopted by whichever side leads next.

### The handoff

The lead goes back to the alpha on its own, at the next term, once every one of the alpha's groups with the leader has had a cycle with it and one of them has settled. `autobahn peering yield --to alpha` does the same on request, from the leading peer. A handoff writes the local lease first, every running peering session brings its peer's copy of the ancestor level and hands the new lease to it on its next attempt, and then the supervisor follows. The lead goes back to the alpha on its own only once the alpha's copy is confirmed level. A paused session is not waited for. A restart in the middle comes back as a follower; the member the lease names leads as soon as the lease reaches it, and otherwise the timeout takes it from there.

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

## Restricted keys

Any beta must be able to take the lead, so each reaches every other over SSH. With ordinary keys that is a shell on every beta from every beta: one compromised beta is all of them. With `manage_keys`, the alpha sets up keys between the betas that can run autobahn and nothing else:

```toml
[advanced.peering-dangerously-experimental]
manage_keys = true
```

On its next cycles as leader, over the logins it already has:

1. Each beta makes a key pair of its own, `~/.autobahn/peering/id_ed25519`, with `ssh-keygen`, and hands back the public half and its SSH host keys. Private keys never leave the machine that made them.
2. Each beta is given every *other* beta's public key, in a block of its `~/.ssh/authorized_keys` that autobahn owns and rewrites whole on every change — nothing outside the block is touched — each line forced through the gate: `restrict,command="$HOME/.autobahn/bin/autobahn-gate gate"`. It is also given the other betas' host keys, in `~/.autobahn/peering/known_hosts`, and the gate itself.
3. A beta that leads dials the others with its peering key, tried first, and with those host keys beside your own `known_hosts`.

The gate runs only the agent, exactly as autobahn asks for it, `autobahn peering attach`, and `autobahn gate install <release> <version>`. That last one installs an agent from autobahn's signed release, downloaded and checked on the beta itself: a controller behind the gate names a release and never sends a binary. A build that is not a release cannot be installed that way, so install its agent by hand. An agent run by the gate refuses to manage keys, so a peering key cannot widen its own access: only the alpha does that, over your own login.

What else to know:
- **Folders, too.** A key restricted to the agent still reaches every folder its user can. Bound that on each beta with `roots` in [`host.toml`](./configuration.md#this-machines-own-settings).
- **Managed elsewhere.** Where `authorized_keys` is a symbolic link, the lines are not written, and the alpha's log gives them for you to add.
- **A new key.** Delete `~/.autobahn/peering/id_ed25519` on a beta, and the alpha makes and hands out a new one on its next connection there.
- **A beta removed** from the configuration leaves the others' blocks when the alpha runs the edited configuration.
- **What it does not restrict.** A compromised beta can still change the files it syncs, as any two-way peer can. The alpha's own keys are as powerful as you make them.
- **Addresses.** `from=` restrictions are not added: the address one beta reaches another from is not always the one the alpha sees.

## Known security issues

These are open.

**Without restricted keys, every peer that can lead holds a shell on every other beta.** Any beta must be able to take over, so each reaches the others, and with ordinary keys a login is a shell. Turn on [restricted keys](#restricted-keys), and bound the folders each beta serves with `roots` in [`host.toml`](./configuration.md#this-machines-own-settings); otherwise peer only machines that would each trust the others with a shell. Either way, a follower never runs a command its leader pushed — it ignores the pushed configuration's `agent_command` and takes its own from `host.toml` — and refuses a pushed `name` outside the folders its `host.toml` allows.

**While a beta leads, it can change what the alpha syncs with it, and nothing else there.** When the alpha attaches, it serves the leader only its own peering sessions, as its own configuration has them: the root, ignores, modes, owners and staging are the alpha's, whatever the leader asks for, and the alpha takes no pushed files. A leader can change files inside those roots, as any beta can in a two-way mode, and nothing outside them.

**A leader's account of the history is checked, not proven.** The replicated ancestor is a leader's record of the last agreed state, and a leader can be wrong or lie. A host takes up a copy only from the member its session is with — the agent notes who wrote every copy — and sets aside every path where the copy records something other than what the host holds and the host's file has not changed since the copy was written: those paths are reconciled as new, so a disagreement is a conflict, never one side overwriting the other on the copy's word. What is left is a leader stating things that are true of the host's own tree, which is no more than it could do by changing its own files and letting the session carry the change, as any two-way peer can. See [Safety](./safety.md).

## What is not covered

- **A one-off `sync` or `resolve` is not fenced.** A channel that never presents a lease is never refused, so a command run by hand from any machine can still write a peer. Peering trusts the operator here.
- **A beta that cannot reach another beta.** Every peer must be able to reach every other with the specs in the configuration. Two servers that need a tunnel between them are a follow-up (`peer_ssh_config`).
- **A beta's sessions with the other betas start without an ancestor.** The alpha's session with each beta is replicated; a session between two betas exists only during a failover and has no history to inherit. If the two betas were in step when the alpha left, they agree; whatever was in flight shows as a handful of conflicts, never as loss.
- **A peer runs the pushed configuration, or its own — not both.** A machine with a configuration of its own runs that, and is not a peer: a pushed `name` beside it is ignored, with a warning at startup.
- **The menu bar app and `mi`** show the role only through `status`.
- **Clocks.** Staleness is judged on the follower's clock against the leader's `renewed_at`. Seconds of skew do not matter; minutes do.

## Under the hood

`src/peering.rs` holds the lease, the pushed files and the ancestor copy; `src/supervisor/peer.rs` the follow/lead state machine of a peer and of the alpha; the protocol requests are `Lease`, `AncestorRecord`, `AncestorCheckpoint`, `PutPeeringFile` and `PeeringState`. The compatibility epoch moved to 12 with them.
