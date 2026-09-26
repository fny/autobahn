# F-H2: Manual `sync` runs the same overlap checks as configured sessions

**Findings:** H-2 (ASTRA F02, reproduced). M-39 (ASTRA F23) is fixed alongside it, because it is a bug in the same function.
**Status:** proposed. High; fix before v1.

## Problem

- **Manual `sync` skips the check.** A session whose beta is the same tree as its alpha, or is inside it, or contains it, synchronizes a tree with itself. In replica mode, it deletes the alpha root through the beta path. `Config::plans()` refuses that shape (`src/config.rs:1306-1325`), and a comment there says the check was added after a reproduction. But explicit-root `autobahn sync ALPHA BETA` (`run_sync`, `src/main.rs:~861`) builds its own session and never runs the check. `Session::new` and endpoint construction don't check either. ASTRA synchronized `tree/source` into `tree` with `one-way-alpha`, and the source directory itself was deleted.
- **The containment test misses two shapes (M-39).** `overlap` (`src/config.rs:707`) strips the outer path and requires the rest to start with `/`. For an outer path of `/` and an inner path of `/srv/project`, the rest is `srv/project`, so containment is missed. A remote root with a trailing `/` fails the same way.

## Proposed resolution

- **One check, called from both places.** Move the within-session topology check out of `plans()` into a function, for example `config::check_session_topology(alpha_target, beta_target, alpha_identity, beta_identity) -> Result<(), String>`. `plans()` and `run_sync` both call it on the resolved identities, before any endpoint opens. `run_sync` already computes those identities (`resolve_for_identity`, `session_identifier`), so no path is resolved twice.
- **Component-aware containment.** Replace the string-prefix test with a comparison of path components. Split local identities with `Path::components()`. Split remote paths on `/` after normalizing: drop empty and `.` components, keep `..` literally. An outer root of `/` contains everything, and trailing separators never matter.
- **Remote identities stay textual,** as the comment on `overlap` already documents. The comparison runs only within the same destination.
- **The same check for `resolve`.** Any other command that builds a session from explicit roots should call it too. `resolve` works from configured plans, so it is already covered.

## Tests

- CLI tests for `autobahn sync`:
  - equal roots are refused;
  - beta inside alpha is refused;
  - alpha inside beta is refused;
  - a symlink alias to alpha is refused, because identities are physical paths;
  - separate sibling trees are accepted.

  In every refused case, assert that nothing was written.
- **Unit tests for the new containment:**
  - `/` contains `/srv/project`;
  - `/srv/` contains `/srv/project`;
  - `/srv/pro` does not contain `/srv/project`;
  - `host:/tree/` contains `host:/tree/nested`.
- The existing `overlapping_roots_within_a_session_are_rejected` and `nested_writable_endpoints_across_sessions_are_rejected` tests still pass through the extracted function.

## Related

H-25, a sync root containing the state root or `config.toml`, is another topology refusal that belongs in the same function. It has its own ticket in the severity walk.
