# The specification

`Autobahn.tla` is the reconciler's rules over a hierarchy of paths — files, directories, and the unit rule that decides a subtree at the shallowest path where the sides disagree — played by one primary and any number of replicas, and the properties the design promises: trees stay well-formed, nothing a user wrote vanishes without its fate on record, `two-way-conflict` discards nothing, the primary never loses to a replica in the primary-wins modes, a pair that reported no conflict is levelled, and once the users stop everything converges except at reported conflicts. TLC checks all of it exhaustively, per mode — two replicas, a directory of two files beside a file, two values, four user actions: about half a million states and ten minutes per mode (`MC.tla` holds the path hierarchy, which the configuration format cannot spell):

```sh
spec/check.sh            # every mode
spec/check.sh strict     # one
```

The `_n3` configurations check the invariants for three replicas, with the replicas' symmetry folding permuted states into one — about 900,000 distinct states and two minutes per mode. Liveness stays with the two-replica runs, since TLC's symmetry reduction is not sound for it:

```sh
spec/check.sh strict_n3 primary_n3 conflict_n3
```

It needs Java 11+ and the `tla2tools.jar` that `spec/tla2tools.version` pins by version, URL and SHA-256. It is fetched into `~/.local/lib` on first use and checked against that digest, and `spec/check.sh --fetch` fetches it without running anything. `TLA2TOOLS` names another copy, which must match the same digest: a jar that does not — named or cached — is refused, and `TLA2TOOLS_TRUST=1` runs the one `TLA2TOOLS` names regardless. To move to a newer TLC, change the pin, not the check.

The implementation is held to the spec by `tests/spec_replay.rs`. It plays the same game with the real `reconcile()` making every cycle's move, asserts the same properties in Rust on thousands of random games (always on), and writes games out as traces that TLC validates against the spec — a step the spec does not allow is a deadlock, and a rejection:

```sh
AUTOBAHN_TLC=1 cargo test --test spec_replay -- --include-ignored
AUTOBAHN_TLC=1 AUTOBAHN_TLC_TRACES=50 AUTOBAHN_TLC_KEEP=1 cargo test --test spec_replay -- --include-ignored   # more, and keep them
spec/check.sh --traces DIR                                                                                      # validate kept traces again
```

## P2P

`P2P.tla` is the star with failover: the same reconciliation (`Reconcile.tla` holds the rules both specs share), plus leases and terms, the fence, takeover in the configured order, the primary's handoff, replication of each session's ancestor with any lag, and hosts that crash and recover. Two replicas, two files, two values, two edits, one crash, two changes of leadership; takeovers may happen at any moment (`Flaky`), standing for a clock that misjudged staleness, since the fence, not the clock, is the guarantee. About 31 to 35 million distinct states and half an hour per mode for the safety properties: no host is ever written by two controllers at one term, the term a host is written at never falls, a value a user removed never comes back on its own, and Reconcile's properties still hold across a failover and a handoff. The liveness configurations (`Flaky = FALSE`) check that once users and failures stop, a leader stands and every host it reaches is level with it.

A host that comes to lead takes up its copy of a session's ancestor when the copy was written after its own store (`later`), as the implementation compares the two stores' write times on the host: generations cannot tell, once a copy that lagged at a takeover has carried on from where it lagged. It sets aside every path where the copy disagrees with its own tree, which the next cycle then reconciles as new, and the primary takes the lead back only once its copy is level with the leader's. `DisputedKept` checks that a value a host held where a copy it adopted disagreed is never lost. `P2P_conflict_lies.cfg` adds a partner that may write any copy at all, once — a buggy or dishonest leader — at one edit, since a lie widens the space past what two edits can finish; it leaves `Accounted` out on purpose, because a copy that agrees with a host's tree is believed, as a partner changing its own side would be.

```sh
spec/check.sh p2p_conflict_safety p2p_primary_safety
spec/check.sh p2p_conflict_lies
spec/check.sh p2p_conflict_liveness p2p_primary_liveness
```

Leadership changes are budgeted like edits and crashes: every change bumps the term, so without a bound the state space is infinite — the first attempt ran seven hours and filled 73 GB before that was clear. `Autobahn_quick.cfg` is the star spec at three edits, half a minute, for checking a change to the shared rules before the full runs.

`tests/spec_p2p_replay.rs` holds the fence to the spec: every host is a directory with a real lease file, presented leases go through `read_lease` / `Lease::admits` / `write_lease` exactly as the agent's `Request::Lease` handler does, staleness is the real `is_stale_at` on a simulated clock, the failover order is the real `takeover_wait`, and the reconciler makes every cycle's move. Random games of three replicas keep the spec's invariants in Rust; games of two are written out as traces TLC validates against `P2P.tla`, matching trees, liveness, leases and roles and leaving the ancestor stores to the spec.

What is not in the p2p model: the clock (staleness is a nondeterministic judgement), partitions as distinct from crashes, `yield`, the one-off commands the fence does not cover, and who writes a copy (a host takes one up only from its session's partner, which the model's copies always are). The model sets aside every path a copy disagrees on; the implementation keeps the copy's record where the host's file changed after the copy was written, which only keeps an honest record the host has since moved past. The replay harness does not yet drive the p2p state machine; the code-level coupling for p2p is the state-machine tests in `tests/supervisor.rs` and `src/supervisor/peer.rs`.

The reconciliation models cover the three two-way policies (`two-way-conflict`, `two-way-primary`, and `two-way-primary-strict`). They do not model the one-way modes, the optional `guard_dir_deletes_over` rule, untracked or problematic content, or the transfer and transition machinery. A rename can be represented as a removal and a creation. Collision tests in `tests/e2e.rs` check the transition contract; the separate p2p model and its limits are described above.
