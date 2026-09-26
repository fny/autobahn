# CI-08: A release requires green CI on its commit

**Findings:** M-58 (OPUS CI gaps).
**Status:** proposed.

## Problem

Pushing a `v*` tag builds and publishes without running tests. The tagged commit may never have been tested at all:
- `paths-ignore` skips CI for doc-only commits;
- `[skip mac]` skips the macOS job;
- a failing run doesn't stop a tag being pushed.

## Proposed resolution

- **Gate the release.** The first job in `release.yml`, next to the existing tag-vs-`Cargo.toml` check, asks the GitHub API for the check runs on the tagged commit. It requires `linux`, `linux-arm`, `mac` and `spec` to be `success`.
- **Say how to fix it.** If any are missing, skipped or failed, the release stops with an error. The error says to run CI on that commit (`gh workflow run ci.yml --ref <tag>`) and push the tag again.
- **Grant only read.** This job needs only `checks: read`, alongside CI-04's read-only default.

## Tests

- A tag on a commit whose `mac` job was skipped stops before any build.
- A tag on a fully green commit passes the gate.
