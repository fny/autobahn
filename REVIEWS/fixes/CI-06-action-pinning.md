# CI-06: No third-party actions in the release, SHA pins in CI

**Findings:** M-19, the pinning part (DEEPSEEK F22; KIMI; OPUS S7).
**Status:** proposed. Not included: turning on the repository's "require SHA pinning" setting.

## Problem

Third-party actions run from tags that can be moved:
- `dtolnay/rust-toolchain@stable`, in CI and in the release, including the `mac` job that holds the secrets;
- `Swatinem/rust-cache@v2`, in CI.

Whoever controls those repositories can change what runs.

## Proposed resolution

- **Release: no third-party actions.** Replace `dtolnay/rust-toolchain` with direct `rustup` commands, since `rustup` is already on every hosted runner. For example: `rustup toolchain install stable --profile minimal --target <target>`. Don't cache in the release.
- **CI: pin by SHA.** Pin `dtolnay/rust-toolchain` and `Swatinem/rust-cache` to full commit SHAs, with the tag in a trailing comment. Pin GitHub's own `actions/*` too, for consistency.
- **Keep the pins current.** The Dependabot `github-actions` ecosystem in CI-10 raises update PRs.

## Tests

- `grep -n "uses:" .github/workflows/*.yml` shows only SHA-pinned references, and none from outside `actions/` in `release.yml`.
- The next CI run and a release dry run pass.
