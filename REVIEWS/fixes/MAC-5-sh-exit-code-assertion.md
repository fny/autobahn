# MAC-5: A test asserts a Linux-only `sh` exit code, so the suite fails on macOS

**Findings:** new, from MAC-BENCH 7i. **Status:** proposed, Low (blocks CI-09).

## Problem

`cargo test --release` fails on macOS, with or without `--features tray`:

```
test result: FAILED. 386 passed; 1 failed
  transport::install::tests::the_remote_command_runs_under_sh_and_picks_this_platform
  assertion `left == right` failed: left: Some(126), right: Some(127)
```

`src/transport/install.rs:767` asserts that running the versioned remote command with the agent missing exits **127**. Measured directly, with `sh -c 'exec <path>'`:

| | missing file | unexecutable file |
|---|---|---|
| macOS `sh` | **126** | 126 |
| Linux `sh` | 127 | 126 |

The launcher behaves correctly on both platforms; only the expectation is Linux-only. macOS's `sh` reports 126 for both cases, so the test cannot tell "missing" from "not executable" there.

This blocks CI-09, which proposes running these tests on a macOS runner: the job would go red on its first run, for a reason that has nothing to do with the code under test.

## Proposed resolution

Assert what the test actually cares about — that the command fails, and that nothing was picked — rather than the number. Either accept both codes (`126 | 127`), or assert `!status.success()` plus the absence of the "picked agent" output, which is what the first half of the test already checks.

## Tests

- The existing test, passing on macOS and Linux unchanged.
- A case that distinguishes "missing" from "present but not executable" only by observable behaviour, not by exit code, if that distinction is worth keeping.
