# HYG-1: Remove the committed agent binary; derive the app version

**Findings:** L-37, two of its parts (OPUS §4).
**Status:** proposed.

## Problem

- **A stale binary is committed.** `dist/agents/autobahn-linux-x86_64` is an unstripped 4 MB build from an old perf commit (`25488f2`). `dist/` isn't ignored, so the next local agent build can be committed by accident too.
- **The app version is hard-coded.** `apps/macos/Info.plist` sets `CFBundleShortVersionString` and `CFBundleVersion` to `0.4.0`. The release workflow checks that the tag matches `Cargo.toml`, but never checks the plist. The next release will ship an app that reports the old version.

## Proposed resolution

- **Remove the binary.** `git rm dist/agents/autobahn-linux-x86_64`, then add `/dist/` to `.gitignore`.
- **Derive the app version.** `apps/macos/build.sh` writes both plist keys from `Cargo.toml`'s `version` when it assembles the bundle, for example with `plutil -replace`. Replace the value committed in `Info.plist` with a placeholder, so a stale number can't ship.
- **Check it in the release.** The existing version job also fails if a built app's plist version differs from the tag.

## Not in this ticket

- **Committed bench results.** Kept deliberately, as `.gitignore` already says. Revisit if the repository grows uncomfortably.
- **The old half of the README.** Your merge in progress: lines 136 to 348, below the "kept for merging" marker.
- **The three root TODO files.** Your call.

## Tests

- `apps/macos/build.sh`, run with `Cargo.toml` at a test version, produces an app whose `Info.plist` reports that version.
