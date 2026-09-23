# The specification

`Autobahn.tla` is the reconciler's rules for files, played by one alpha and any number of betas, and the properties the design promises: nothing a user wrote vanishes without its fate on record, `two-way-conflict` discards nothing, alpha never loses to a beta in the alpha modes, a pair that reported no conflict is levelled, and once the users stop everything converges except at reported conflicts. TLC checks all of it exhaustively, per mode, in about a minute each:

```sh
spec/check.sh            # every mode
spec/check.sh strict     # one
```

It needs Java 11+; `tla2tools.jar` is fetched into `~/.local/lib` on first use, or set `TLA2TOOLS`.

The implementation is held to the spec by `tests/spec_replay.rs`. It plays the same game with the real `reconcile()` making every cycle's move, asserts the same properties in Rust on thousands of random games (always on), and writes games out as traces that TLC validates against the spec — a step the spec does not allow is a deadlock, and a rejection:

```sh
AUTOBAHN_TLC=1 cargo test --test spec_replay
AUTOBAHN_TLC=1 AUTOBAHN_TLC_TRACES=50 AUTOBAHN_TLC_KEEP=1 cargo test --test spec_replay   # more, and keep them
spec/check.sh --traces DIR                                                                  # validate kept traces again
```

What is not in the model: directories (a subtree is one unit to the reconciler, and the rules above apply at the unit), renames (a deletion and a creation, which the model can express as two moves), and the transfer and transition machinery, whose contract — a transition writes only what it validated against its own scan — is pinned by the collision tests in `tests/e2e.rs`. Peering is not modelled.
