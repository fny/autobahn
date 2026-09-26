# OPS-3: Remote bootstrap works under any login shell

**Findings:** M-37 (OPUS M17).
**Status:** proposed.

## Problem

Setting up a remote host sends a short script over SSH (`src/transport/install.rs:213-221`). The script uses `tmp=…`, `$$` and `{ …; }`, all POSIX `sh` syntax. SSH runs remote commands through the user's *login* shell. So a host whose login shell is fish or tcsh fails, and can never be set up.

The existing fake-SSH test runs the script with `/bin/sh -c`, so it can't catch this.

## Proposed resolution

- **Wrap it.** Send `sh -c '<script>'`, quoting the script as a single argument, so it runs under POSIX `sh` whatever the login shell is.
- **Same for other remote commands.** `prune_agents`'s `rm` and the platform probe get the same treatment.

## Tests

- The fake-SSH test runs the command through `fish -c` or `tcsh -c` when either is installed, and skips otherwise.
- The fake-SSH test asserts that the command it receives starts with `sh -c `.
