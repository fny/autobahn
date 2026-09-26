# OPS-5: Document three deliberate choices

**Findings:** L-33 (DEEPSEEK I1), L-34 (DEEPSEEK I3), L-15 (OPUS).
**Status:** proposed. Docs only.

## Problem

Three behaviours are deliberate, but users aren't told about them:

- **Symlinks are copied as-is (L-33).** The default symlink mode syncs link targets verbatim, including targets outside the root. After the T1 fixes autobahn never follows them, but other tools that walk the tree might.
- **`on_alert` inherits the environment (L-34).** A hook started by `watch` in a terminal sees every variable of that terminal. Under the login service it sees only a small `PATH` and `AUTOBAHN_HOME`.
- **Unchanged-file detection ignores ctime (L-15).** A rewrite that keeps the same size and restores the modification time isn't noticed. `RETAINED.md` §5 accepts this, and `autobahn verify` re-reads every byte.

## Proposed resolution

- **Symlinks.** In the symlink section of `docs/configuration.md`, say that links pointing outside the root are copied verbatim. Say that autobahn never follows them. Recommend `symlink_mode = "portable"` for trees other tools will walk.
- **Hooks.** In `docs/alerts.md`, one paragraph on what environment a hook sees under `watch` and under the login service.
- **ctime.** In `docs/safety.md`, where the same-length-rewrite limit is listed, add that ctime is not consulted, and point to `autobahn verify`.

## Tests

None; docs only.
