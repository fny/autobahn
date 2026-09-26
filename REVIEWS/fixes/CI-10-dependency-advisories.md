# CI-10: Dependabot and a weekly advisory check

**Findings:** I-1 (KIMI ABN-I1; DEEPSEEK §5; OPUS). This also keeps CI-06's action pins current.
**Status:** proposed.

## Problem

Nothing reports new security advisories against the dependencies. The shipped CLI and agent compile about 70 crates; the macOS app, about 130. Nothing proposes updates either, for crates or for pinned actions.

## Proposed resolution

- **Dependabot.** Add `.github/dependabot.yml` for two ecosystems, `cargo` and `github-actions`, checking weekly. Group minor and patch updates into one PR per ecosystem to keep the noise down.
- **Advisory check.** Add a workflow, `.github/workflows/audit.yml`, that runs `cargo deny check advisories` (or `cargo audit`). It runs:
  - weekly, on a schedule;
  - on any change to `Cargo.lock`;
  - by hand.

  It is not part of the per-push CI, so a newly published advisory doesn't block unrelated work.
- **Accepted advisories.** Record them in `deny.toml` with a reason and a review date. The GTK and `glib` advisories on the tray-only Linux path are candidates, since that path isn't shipped.

## Tests

- The audit workflow runs by hand and passes, or lists its findings.
- Dependabot opens its first grouped PRs.
