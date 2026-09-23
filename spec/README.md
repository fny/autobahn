# The specification

`Autobahn.tla` is the reconciler's rules over a hierarchy of paths — files, directories, and the unit rule that decides a subtree at the shallowest path where the sides disagree — played by one alpha and any number of betas, and the properties the design promises: trees stay well-formed, nothing a user wrote vanishes without its fate on record, `two-way-conflict` discards nothing, alpha never loses to a beta in the alpha modes, a pair that reported no conflict is levelled, and once the users stop everything converges except at reported conflicts. TLC checks all of it exhaustively, per mode — two betas, a directory of two files beside a file, two values, four user actions: about half a million states and ten minutes per mode (`MC.tla` holds the path hierarchy, which the configuration format cannot spell):

```sh
spec/check.sh            # every mode
spec/check.sh strict     # one
```

The `_n3` configurations check the invariants for three betas, with the betas' symmetry folding permuted states into one — about 900,000 distinct states and two minutes per mode. Liveness stays with the two-beta runs, since TLC's symmetry reduction is not sound for it:

```sh
spec/check.sh strict_n3 alpha_n3 conflict_n3
```

It needs Java 11+; `tla2tools.jar` is fetched into `~/.local/lib` on first use, or set `TLA2TOOLS`.

The implementation is held to the spec by `tests/spec_replay.rs`. It plays the same game with the real `reconcile()` making every cycle's move, asserts the same properties in Rust on thousands of random games (always on), and writes games out as traces that TLC validates against the spec — a step the spec does not allow is a deadlock, and a rejection:

```sh
AUTOBAHN_TLC=1 cargo test --test spec_replay
AUTOBAHN_TLC=1 AUTOBAHN_TLC_TRACES=50 AUTOBAHN_TLC_KEEP=1 cargo test --test spec_replay   # more, and keep them
spec/check.sh --traces DIR                                                                  # validate kept traces again
```

## Peering

`Peering.tla` is the star with failover: the same reconciliation (`Reconcile.tla` holds the rules both specs share), plus leases and terms, the fence, takeover in the configured order, the alpha's handoff, replication of each session's ancestor with any lag, and hosts that crash and recover. Two betas, two files, two values, two edits, one crash, two changes of leadership; takeovers may happen at any moment (`Flaky`), standing for a clock that misjudged staleness, since the fence, not the clock, is the guarantee. About 36 million distinct states and half an hour per mode for the safety properties: no host is ever written by two controllers at one term, the term a host is written at never falls, a value a user removed never comes back on its own, and Reconcile's properties still hold across a failover and a handoff. The liveness configurations (`Flaky = FALSE`) check that once users and failures stop, a leader stands and every host it reaches is level with it.

```sh
spec/check.sh peering_conflict_safety peering_alpha_safety
spec/check.sh peering_conflict_liveness peering_alpha_liveness
```

Leadership changes are budgeted like edits and crashes: every change bumps the term, so without a bound the state space is infinite — the first attempt ran seven hours and filled 73 GB before that was clear. `Autobahn_quick.cfg` is the star spec at three edits, half a minute, for checking a change to the shared rules before the full runs.

`tests/spec_peering_replay.rs` holds the fence to the spec: every host is a directory with a real lease file, presented leases go through `read_lease` / `Lease::admits` / `write_lease` exactly as the agent's `Request::Lease` handler does, staleness is the real `is_stale_at` on a simulated clock, the failover order is the real `takeover_wait`, and the reconciler makes every cycle's move. Random games of three betas keep the spec's invariants in Rust; games of two are written out as traces TLC validates against `Peering.tla`, matching trees, liveness, leases and roles and leaving the ancestor stores to the spec.

What is not in the peering model: the clock (staleness is a nondeterministic judgement), partitions as distinct from crashes, `yield`, and the one-off commands the fence does not cover. The replay harness does not yet drive the peering state machine; the code-level coupling for peering is the state-machine tests in `tests/supervisor.rs` and `src/supervisor/peer.rs`.

What is not in the model: renames (a removal and a creation, which the model can express as two moves), `two-way-paranoid`'s large-directory rule, untracked and problematic content, and the transfer and transition machinery, whose contract — a transition writes only what it validated against its own scan — is pinned by the collision tests in `tests/e2e.rs`. Peering is not modelled.
