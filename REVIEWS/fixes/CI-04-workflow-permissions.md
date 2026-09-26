# CI-04: Least-privilege workflow tokens

**Findings:** M-19, the permissions part, which includes KIMI ABN-L14's token scope (KIMI; OPUS S7).
**Status:** proposed.

## Problem

- **The reviews assumed too much here.** The repository's default workflow token is already read-only (`default_workflow_permissions: read`). So `ci.yml` and `spec-full.yml`, which set no `permissions:`, can't write today.
- **The release workflow over-grants.** `release.yml:20` sets `contents: write` for the whole workflow. That includes the `mac` job, which holds the Developer ID certificate and the notary key, and which runs third-party build code.

## Proposed resolution

- **Read-only by default.** Add `permissions: contents: read` at the top of `ci.yml`, `spec-full.yml` and `release.yml`, so a change to the repository setting can't widen them.
- **Write only where it's needed.** In `release.yml`, grant `contents: write` only to the final `release` job, the one that runs `gh release create`.

## Tests

- A tag on a fork, or a dry run, completes the release.
- The `mac` job's token can't write. `gh api` from that job gets a 403 on a write.
