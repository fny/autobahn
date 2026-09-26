# CI-05: Build the app before the signing keychain exists

**Findings:** M-19, the keychain part (KIMI ABN-L14).
**Status:** proposed.

## Problem

The `mac` release job imports the Developer ID certificate into an unlocked keychain. Only after that does it run `apps/macos/release.sh`, which calls `apps/macos/build.sh` (`release.sh:34`) to compile the app with `--features tray`.

So every crate's build script and proc macro runs while the signing identity is usable. A compromised dependency could sign arbitrary code as you. The command-line binaries are already built before the import; only the app build is affected.

## Proposed resolution

- **Split build from sign.** `build.sh` gains a mode that builds the unsigned bundle. `release.sh` gains a mode that signs, notarizes and staples an existing bundle.
- **Reorder the job:** build both binaries and the app, then import the certificate, then sign and notarize everything, then delete the keychain.
- **Keep laptop use unchanged.** Running `release.sh` with no arguments still builds and signs in one go.

## Tests

- A release dry run on a fork, with a throwaway certificate, produces a signed and notarized app. The job log shows no `cargo` step after the certificate import.
