# CI-03: Pin the TLA+ tools jar

**Findings:** M-19, the jar part (KIMI ABN-M19; OPUS S7).
**Status:** proposed.

## Problem

Three places download `tla2tools.jar` from `https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar` and run it with no version and no checksum:

- `.github/workflows/ci.yml` (the `spec` job)
- `.github/workflows/spec-full.yml`
- `spec/check.sh:18`, which also runs on developers' machines

A compromised or simply changed tlaplus release would run in CI and on your laptop. It would also make results change between runs with no change in the repo.

## Proposed resolution

- **One pin.** Pick a specific tlaplus release tag. Record its URL and SHA-256 in one place, for example `spec/tla2tools.version`, which all three read.
- **Verify before use.** Download from the tag URL, check the SHA-256, and refuse on mismatch. `check.sh` also verifies a jar that already exists at `$TLA2TOOLS`, unless an explicit override variable says to trust it.
- **Upgrade on purpose.** Bumping the version is a normal change to the recorded file.

## Tests

- `check.sh` with a corrupted cached jar refuses to run it.
- CI's `spec` job logs the pinned version.
