# LOCAL-04: `diff` and tray diff scratch files out of `/tmp`

**Findings:** H-26 (KIMI ABN-H12; DEEPSEEK F16; GLM L3; OPUS S8), M-4 (KIMI ABN-M16; DEEPSEEK F20; OPUS S8).
**Status:** proposed.

## Problem

- **`autobahn diff`.** It writes both sides of a conflict into `temp_dir()/autobahn-diff-<pid>` (`src/main.rs:2064-2090`). It uses `create_dir_all`, which accepts a directory another user created first, and `fs::write`, which follows symlinks. On a shared Linux host, another user can pre-create the directory. They can then read both sides, redirect the writes onto the victim's files, or swap the contents the user compares before choosing a `resolve --keep` winner.
- **The tray's "Show diff".** It writes `temp_dir()/autobahn-diff-<path with / replaced>.diff` the same way (`src/tray.rs:729-736`). Different paths can also collide on one name, such as `a/b` and `a_b`.

## Proposed resolution

- **`diff`** uses `private_tempdir()` from LOCAL-01, writes both sides with `private_file`, and removes the directory when the diff tool exits.
- **The tray** writes into `~/.autobahn/tmp/`, naming the file by a hash of the conflict path. The viewer opens the file after the tray has moved on, so it is not deleted at once. The startup sweep in LOCAL-01 removes old files.

## Tests

- `diff` succeeds when `/tmp/autobahn-diff-<pid>` exists and is owned by someone else, because it no longer uses that path.
- The diff scratch files are `0600` in a `0700` directory, and are gone after the tool exits.
- The tray diff names for `a/b` and `a_b` differ.
