# Formal Specifications in TLA+

Autobahn formally models its synchronization invariants and failover peering protocols using TLA+, with exhaustive model checking provided by TLC.

---

## Specification Models

### 1. Three-Way Reconciliation (`Autobahn.tla`, `Reconcile.tla`)
Models state transitions over hierarchical filesystem paths across one Primary and multiple Replicas:
- Preserves well-formed trees under all concurrent operations.
- Validates non-destructive reconciliation under `two-way-conflict`.
- Proves the primary's precedence under `two-way-primary` and `two-way-primary-strict`.
- Verifies system convergence once external modifications cease.

### 2. Peering & Automated Failover (`Peering.tla`)
Models multi-host leadership lease negotiation, split-brain write fencing, state machine takeovers, reverse connections, and ancestor replication.

---

## Model Checking with TLC

Verification requires Java 11+ and `tla2tools.jar` (pinned via `spec/tla2tools.version`):

```sh
# Fetch pinned TLC runtime jar
spec/check.sh --fetch

# Run model checks across standard reconciliation modes
spec/check.sh

# Run model checks for specific modes
spec/check.sh strict
spec/check.sh strict_n3 primary_n3 conflict_n3

# Verify failover peering safety and liveness
spec/check.sh peering_conflict_safety peering_primary_safety
spec/check.sh peering_conflict_liveness peering_primary_liveness
```

---

## Test Trace Replay Harness

The Rust test suite in `tests/spec_replay.rs` and `tests/spec_peering_replay.rs` binds the production implementation to the formal TLA+ specifications:
- Executes `reconcile()` and lease handling across thousands of randomized schedules.
- Emits execution traces and validates them directly against TLC model constraints:

```sh
AUTOBAHN_TLC=1 cargo test --release --locked --test spec_replay -- --include-ignored
AUTOBAHN_TLC=1 cargo test --release --locked --test spec_peering_replay -- --include-ignored
```
