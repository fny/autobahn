# BENCH-6: Verification scripts use the current CLI and fail fast

**Findings:** M-52 (ASTRA F32; OPUS Low).
**Status:** proposed. This is a fault under the bench standard: it produces wrong results, because the scripts cannot run at all.

## Problem

The reviews listed three scripts that call the removed `up` subcommand. There are eleven:

- `scripts/mi:97`
- in `bench/verify/`: `bottleneck.sh`, `coldfan.sh`, `crash.sh`, `differential.sh`, `exhaustion.sh`, `fanout-correctness.sh`, `filesystems.sh`, `gates.sh`, `latency.sh`, `soak.sh`

The current binary rejects `up` as an unknown subcommand. Several of these scripts start the subject in the background and then wait or report without checking that it started. They therefore "finish" with meaningless results.

`scripts/mi` also writes to the real `~/.autobahn`, according to OPUS.

## Proposed resolution

- **Current commands.** Replace `up … --once` with `sync --config … --state-root …`, and plain `up` with `watch`.
- **Fail fast.** After each background start, wait briefly and check that the process is alive, for example with `kill -0 $!`. If it is not, print the log's last lines and exit nonzero. Use `set -euo pipefail` where the script does not already.
- **Isolate `scripts/mi`.** Give it its own state root in a temporary directory, as the verify scripts already do.

## Tests

- `shellcheck` on all eleven scripts.
- `grep -rn ' up ' bench/verify scripts` finds no calls to the removed command.
- Each script run with a deliberately broken binary path fails within seconds.

These scripts are not run in CI, as decided for bench. The checks run locally when the ticket is worked.
