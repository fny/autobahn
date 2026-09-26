# BENCH-2: `ab.sh --corpus` never deletes a directory it did not create

**Findings:** H-15 (ASTRA F10).
**Status:** proposed. This is a fault under the bench standard: it can harm the machine it runs on.

## Problem

`bench/ab.sh --corpus DIR` sets `CORPUS=DIR` (`:49`). Every leg then runs `rm -rf "$dest" "$state" "$CORPUS"` and refills the corpus from the pristine copy (`:106-108`). The flag reads like "use this corpus", but the script treats the directory as scratch space. If you point it at a checkout, the checkout is deleted and replaced with generated data. The default (`:66`) is under the script's own work directory, so only an explicit `--corpus` is dangerous.

## Proposed resolution

- **Make `--corpus` mean what it says.** The given directory becomes the pristine source and is never written. Each leg copies it into a working corpus under `$WORK`, which is the only directory `rm -rf` ever touches.
- **Guard the deletion.** Every `rm -rf` in the script refuses a path outside `$WORK`, as a backstop against future edits.
- **Document the flag.** State in `--help` and `bench/README.md` that the given directory is only read.

## Tests

- Run `ab.sh --corpus <a directory holding a marker file>` for one short leg. The directory and the marker are unchanged afterwards.
- Run `shellcheck bench/ab.sh`.
