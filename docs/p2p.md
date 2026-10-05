# Multi-Host Failover P2P (Dangerously Experimental)

Autobahn supports automatic failover in star topologies. The primary coordinates synchronization during normal operation. If the primary becomes unreachable for an extended period, an eligible replica assumes temporary leadership, coordinating synchronization across remaining peers. When the primary reconnects, leadership goes back to it automatically.

> [!WARNING]
> P2P is classified as **dangerously experimental**. Because leadership transition requires peer-to-peer communication across the replica hosts, enabling it requires explicit trust across all member machines unless you use SSH in restricted mode. Review [Security Boundaries & Access Control](#security-boundaries--access-control) prior to deployment.

## Configuration

Enable it by specifying a p2p mode on the group:

```toml
# ~/.autobahn/config.toml (on Primary controller)

[groups.notes]
mode = "p2p-conflict-dangerously-experimental"  # or p2p-primary-dangerously-experimental
primary = "~/Documents/Notes"                   # the coordinator
replicas = [                                    # peers in designated failover order
  "laptop.bmw.de",
  "studio.audi.de",
]
interval = 10                                   # at most half the ttl below

[experimental.p2p-dangerously-experimental]
ttl            = "30s"     # Lease expiration duration
failover_after = "120s"    # Unreachability duration before failover initiates
manage_keys    = true      # Automatically provisions restricted SSH keys
```

- `p2p-conflict-dangerously-experimental`: Standard `two-way-conflict` reconciliation with automated failover.
- `p2p-primary-dangerously-experimental`: `two-way-primary` reconciliation where the primary's version (the current leader's version) retains precedence whenever the primary is involved.
- `ttl` must equal or exceed twice the group `interval`.
- Peer nodes run `autobahn watch` or `autobahn install` **without local configuration**; the active leader dynamically replicates required configuration and state.

## Architectural Mechanics

### Lease-Based Write Fencing

Leadership authority is established via a cryptographic lease file (`~/.autobahn/p2p/lease.json`) renewed each cycle:

```json
{ "leader": "primary", "term": 7, "renewed_at": 1789544514, "ttl_seconds": 30 }
```

- **Monotonic Terms:** The term number increments with each leadership transition.
- **Write Fence:** The local Autobahn agent rejects write requests from any controller whose term is lower than the active lease. This fence prevents split-brain concurrent writes.
- **Local Expiration:** Lease expiration is evaluated using the local host's monotonic clock, avoiding clock-skew vulnerabilities across machines.

### State Replication

To ensure clean failover without treating the existing tree as un-synchronized, the active leader replicates:

- Group configurations and ignore rules.
- Ancestor journal checkpoints and incremental delta records.

When a Replica assumes leadership, it adopts the replicated ancestor, allowing three-way reconciliation to proceed without false conflicts or data loss.

### Failover Sequence

1. Each replica monitors its local lease.
2. If the primary's lease expires and remains unrenewed for `failover_after + (index * ttl)`, the candidate node increments the term and publishes a new lease.
3. The new leader initiates a reversed supervisor topology, treating itself as the primary and the remaining nodes as its replicas.

### Reverse Connection Attachment

Because the primary may reside behind NAT, a firewall, or dynamic networking, replicas never dial it directly.

- Upon reconnecting, the primary initiates an outbound SSH tunnel to the current leader:
  ```sh
  ssh <leader> autobahn p2p attach
  ```
- The leader routes synchronization frames through this reverse channel.
- Once synchronized, the leader voluntarily yields leadership back to the primary via an orderly handoff (`autobahn p2p yield --to primary`).

---

## Security Boundaries & Access Control

### Restricted SSH Keys (`manage_keys = true`)

By default, inter-peer communication over SSH grants full shell execution rights across the replica hosts. Enabling `manage_keys = true` enforces strict command containment:

1. Each replica generates a dedicated key pair (`~/.autobahn/p2p/id_ed25519`).
2. Public keys are registered in `~/.ssh/authorized_keys` restricted to the Autobahn security gate:
   ```
   restrict,command="$HOME/.autobahn/bin/autobahn-gate gate" ssh-ed25519 ...
   ```
3. The gate permits only verified protocol invocations (`agent`, `p2p attach`, and signed `gate install`).

### Directory Whitelisting (`host.toml`)

Even with restricted SSH keys, agents execute with user privileges. To restrict which directory trees an agent can access on each Replica, define explicit boundaries in `~/.autobahn/host.toml`:

```toml
# ~/.autobahn/host.toml
roots = ["~/Workspace"]
```

The agent strictly rejects any connection requesting access to paths outside the configured whitelist.

## See Also

- [Accepted Risks](./correctness/accepted-risks.md#6-p2p-trusts-every-machine-in-the-group): Peer trust and unresolved leadership collisions
- [Modes](./modes.md): Synchronization modes without leadership failover
- [Configuration](./configuration.md): Mode settings and host access policy
- [Safety](./safety.md): Synchronization safeguards and the host trust model
- [Limitations](./limitations.md): Supported environments and operational restrictions
