# CI-02: `spec/check.sh` fails when TLC fails

**Findings:** H-16 (ASTRA F12, reproduced).
**Status:** proposed. Land it with CI-01, so the first real `spec` run can be trusted.

## Problem

`spec/check.sh:41` runs `tlc … | grep -E "Error|violated|…" || status=1`. The script sets `-u` but not `pipefail`. So the exit status is `grep`'s, and `grep` succeeds precisely when TLC prints `Error` or `violated`.

ASTRA replaced `java` with a stub that printed an invariant violation and exited 17. `check.sh quick` then exited 0.

The `--traces` branch (`:21-31`) does check TLC's exit status, but it reports success when the trace directory is empty.

## Proposed resolution

- **Keep TLC's exit status.** Capture TLC's output to a file, keep its exit code, then filter the file for display. A nonzero exit fails the model.
- **Don't trust exit 0 alone.** Also fail when the output contains `violated`, `Error:` or `Deadlock`, or when it lacks `Finished`.
- **Reject empty traces.** `--traces` fails when the directory holds no traces.
- **Pipes elsewhere.** Add `set -o pipefail` for the rest of the script.

## Tests

A small test script runs `check.sh` against a stub `java` in four cases:

| Stub behaviour | Expected result |
|---|---|
| Prints a violation and exits nonzero | fails |
| Prints a violation and exits 0 | fails |
| Prints a normal finish | passes |
| `--traces` on an empty directory | fails |

The stub goes on `PATH`, or is passed via a `JAVA` variable if the script gains one.
