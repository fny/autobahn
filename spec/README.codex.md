# The specification

`Autobahn.tla` models reconciliation over a path hierarchy with one primary and multiple replicas. It includes files, directories, and the rule that resolves a subtree at the shallowest disagreement.

The model checks well-formed trees, recorded outcomes for user data, preservation under `two-way-conflict`, primary authority, and agreement after conflict-free cycles. Once user changes stop, it also checks convergence except at reported conflicts.

The standard bounds use two replicas, a directory containing two files, one adjacent file, two values, and four user actions. TLC explores about half a million states in ten minutes per mode. `MC.tla` defines the hierarchy because the configuration format cannot express it.

```sh
spec/check.sh            # every mode
spec/check.sh strict     # one
```

The `_n3` configurations use three replicas and symmetry reduction. They check safety over about 900,000 distinct states in two minutes per mode. Liveness uses the two-replica runs because TLC symmetry reduction is not sound for that check.

```sh
spec/check.sh strict_n3 primary_n3 conflict_n3
```

## Running TLC

Java 11+ is required. `spec/tla2tools.version` pins the jar version, URL, and SHA-256.

On first use, the script downloads the jar to `~/.local/lib` and checks its digest. `spec/check.sh --fetch` downloads without running models.

`TLA2TOOLS` selects another copy, which must match the pinned digest. `TLA2TOOLS_TRUST=1` bypasses that check for the explicitly selected jar.

For a TLC upgrade, update the pin rather than removing verification.

## Implementation replay

`tests/spec_replay.rs` runs real `reconcile()` cycles and checks the same properties across thousands of random games. These Rust checks always run.

Optional TLC tests export implementation traces. TLC validates each step against the specification. An impossible step produces a deadlock and rejects the trace.

```sh
AUTOBAHN_TLC=1 cargo test --test spec_replay -- --include-ignored
AUTOBAHN_TLC=1 AUTOBAHN_TLC_TRACES=50 AUTOBAHN_TLC_KEEP=1 cargo test --test spec_replay -- --include-ignored   # more, and keep them
spec/check.sh --traces DIR                                                                                      # validate kept traces again
```

## Peering

`Peering.tla` adds failover to the shared rules in `Reconcile.tla`. It models leases, terms, write fencing, takeover order, primary handoff, lagging ancestor replication, crashes, and recovery.

Safety bounds use two replicas, two files, two values, two edits, one crash, and two leadership changes. `Flaky` allows takeover at any time to represent inaccurate staleness judgments.

Each safety mode explores about 31–35 million states in half an hour. Properties include single-controller writes per host and term, nondecreasing write terms, no spontaneous return of removed values, and reconciliation safety across handoffs.

Liveness configurations use `Flaky = FALSE`. After user activity and failures stop, they require a stable leader and agreement with reachable hosts.

A new leader adopts an ancestor copy if it is `later` than its local store. The implementation compares local write times because generation numbers become incomparable after lagging copies advance independently.

The model sets aside copy paths that disagree with local content and reconciles them as new. The primary resumes leadership only after its copy catches up.

`DisputedKept` checks preservation of local values disputed by adopted copies. `Peering_conflict_lies.cfg` allows one arbitrary copy from a buggy or dishonest partner and uses one edit to bound the larger state space.

That configuration omits `Accounted`: a copy agreeing with local content remains trusted, as a partner’s ordinary change can be.

```sh
spec/check.sh peering_conflict_safety peering_primary_safety
spec/check.sh peering_conflict_lies
spec/check.sh peering_conflict_liveness peering_primary_liveness
```

Leadership changes require a bound because each increments the term. An unbounded first attempt ran seven hours and consumed 73 GB.

`Autobahn_quick.cfg` checks the star with three edits in about half a minute. Use it before full runs after changes to shared rules.

## Peering replay

`tests/spec_peering_replay.rs` uses real lease files and the agent’s `read_lease`, `Lease::admits`, and `write_lease` sequence.

It uses real `is_stale_at` with a simulated clock, real `takeover_wait`, and real reconciliation.

Three-replica random games check invariants in Rust. Two-replica games also produce TLC traces. Replay matches trees, liveness, leases, and roles while leaving ancestor stores to the model.

## Model limits

The peering model represents staleness nondeterministically. It does not model clock behavior, network partitions separately from crashes, `yield`, or unfenced one-off commands.

Model copies always come from the session partner, so writer authentication is outside the model.

The model discards every copy path that disagrees locally. The implementation retains a copy record if the local file changed after the copy’s write time. That case preserves an honest older record.

Replay does not yet drive the complete peering state machine. State-machine coverage is in `tests/supervisor.rs` and `src/supervisor/peer.rs`.

Reconciliation models cover `two-way-conflict`, `two-way-primary`, and `two-way-primary-strict`. They exclude one-way modes, `guard_directory_deletes_over`, untracked or problematic content, and transfer/transition machinery.

Renames can appear as removal plus creation. Collision tests in `tests/e2e.rs` cover the transition contract.
