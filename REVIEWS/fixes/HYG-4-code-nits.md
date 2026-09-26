# HYG-4: Small code cleanups

**Findings:** L-35 (GLM L9), L-40 (OPUS §4).
**Status:** proposed. Low priority. Do it opportunistically, or alongside HYG-2 and HYG-3.

## Problem and proposed resolution

- **Dead code.** Remove `let _ = shown;`, `let _ = inner;`, `let _ = intent_recorded;` and `let _ = root;` together with the values they discard, if those are unused. Remove the immediately-invoked closure in `attempt_once`. `Response::Scan` is also unused, since every scan answers as a delta or "unchanged". Either remove it, which is a wire change and needs a compatibility-epoch bump, or give it the same validation the delta path has (L-9). Removing it can wait for the next epoch bump.
- **Poisoned locks.** `.expect` on a mutex that can be poisoned (`src/transport/mux.rs:160-162`) panics once any other holder panics. Use `unwrap_or_else(PoisonError::into_inner)`, which the rest of that file already does.
- **Remote command path.** `src/transport/install.rs:31-32` builds a home-relative remote command from `protocol::version()` without quoting it. That is safe only because the version is a compile-time constant. Add a test asserting the version matches `[0-9A-Za-z._+-]+`, so a future change can't break the assumption silently.
- **Terminal checks.** Replace the five `unsafe { libc::isatty }` calls with `std::io::IsTerminal`, unless HYG-2 already does.

**Not tickets:** the duplicated helpers and long functions in OPUS §4 stay there as refactoring notes: `thousands`, `terminal_size`, the width and truncate helpers, `format_age`, `format_size` against `format_bytes`, and the two TOML stacks.

## Tests

- The existing suite passes.
- The version-charset test is added.
