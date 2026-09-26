# LOCAL-07: Bench scripts use private work directories

**Findings:** M-20 (KIMI ABN-M20).
**Status:** proposed.

## Problem

`bench/git-sync.sh:15-16` and `bench/ab.sh:39,65,117-125` use fixed `/tmp` paths, created with `rm -rf` then `mkdir -p`, under `set -u` but not `set -e`. A directory another user created first survives and is reused.

The script then writes a config containing `agent_command` into that directory and starts `watch`. Whoever owns the directory can swap the config during setup and have their command run. `alpha-bench.sh` and `smoke.sh` already use `mktemp -d`.

## Proposed resolution

- Both scripts use `WORK="$(mktemp -d)"`, with a `trap` to remove it on exit.
- Both use `set -euo pipefail`.
- The `--corpus` deletion in `ab.sh` is H-15 and is tracked separately.

## Tests

- Run both scripts under `shellcheck`.
- Run a smoke test with `/tmp/autobahn-git-sync` pre-created and owned by someone else. The script ignores it.
