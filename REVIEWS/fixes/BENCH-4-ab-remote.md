# BENCH-4: `ab.sh --remote` verifies the destination it synced to

**Findings:** M-50 (ASTRA F30).
**Status:** proposed. This is a fault under the bench standard: it produces wrong numbers.

## Problem

`--remote` changes the sync destination and the agent command. Everything else stays local (`bench/ab.sh:112`, `:129`, `:138`): creating and cleaning up the destination, the manifest checks, the observer process and its address.

On a separate host, the cold-sync loop polls the empty local directory for ten minutes, then carries on without treating the timeout as a failure. The workload checks then observe the same wrong directory. The tested binary is not copied to the remote host either. Every separate-host result from this script is invalid.

## Proposed resolution

- **Remote-aware steps.** Run each remote step on the remote host: create and clean the destination and compute its manifest over `ssh`, copy the tested binary there with `scp`, and start the observer there with the right `--listen` value (BENCH-1).
- **Or limit the flag.** If that is more than the script is worth, refuse `--remote` unless the host resolves to the local machine, and say that the orchestrator covers the separate-host case.
- **Fail on timeout.** Treat a cold-sync timeout, or a subject process that exits early, as a failed leg, with a nonzero exit.

## Tests

- A same-host `--remote localhost` run passes.
- A run whose subject exits immediately fails at once rather than after ten minutes.
