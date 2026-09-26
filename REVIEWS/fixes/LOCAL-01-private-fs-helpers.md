# LOCAL-01: Private file and directory helpers

**Findings:** building block for LOCAL-02 to LOCAL-06. It has no finding of its own.
**Status:** proposed.

## Problem

Files and directories are created in many places with the default umask and predictable names. Each place would need the same checks, and repeating them invites the next one to be missed.

## Proposed resolution

Add a small `src/fsutil.rs` with three helpers:

- **`private_dir(path)`** creates the directory with a plain `mkdir`, not `create_dir_all`, at mode `0700`. If it already exists, it checks with `lstat` that it is a real directory owned by the current user. It tightens looser permissions with a warning, and refuses anything else with an error that names the path and the problem.
- **`private_file(path)`** opens with `create_new(true)`, mode `0600` and `O_NOFOLLOW`.
- **`private_tempdir()`** creates a random-named `0700` directory under `~/.autobahn/tmp/`, never under the shared `/tmp`. It returns a guard that removes the directory when dropped. Promote `tempfile`, already a dev dependency, to a regular one, or write the random name directly.

At startup, sweep `~/.autobahn/tmp/` of entries older than a day.

## Tests

- `private_dir` on a directory owned by another uid is refused. This test runs only as root.
- `private_dir` on a symlink to a directory is refused.
- `private_dir` on an existing `0755` directory tightens it to `0700`.
- `private_file` on an existing name, and on a symlink, is refused.
- New files are `0600` and new directories `0700` under umask `022`.
