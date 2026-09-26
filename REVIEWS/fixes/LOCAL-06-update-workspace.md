# LOCAL-06: Private update workspace, verify what is installed

**Findings:** the workspace part of M-18 (KIMI ABN-M18; OPUS S8).
**Status:** proposed.

## Problem

`autobahn update` stages its download in the shared temp directory with `create_dir_all` and the default permissions (`src/update.rs:721-731`). The staged binary is `0644`, and it is read again after its checksum is checked. That happens once to run `--version` and once to copy it into place, after the slow agent-bundle refresh.

Anyone who can write the staged path in that window replaces what gets installed.

## Proposed resolution

- Stage in `private_tempdir()` from LOCAL-01.
- Open the staged binary once. Compute the checksum from that handle. Copy the verified bytes from the same handle to the install temporary, which is created with `private_file` and then `chmod 0755`. Run `--version` on the install temporary, not the download.
- Do the same for the agent bundle archive.

The downgrade floor and the `GH_HOST` / `GH_TOKEN` trust-root issue, also in M-18, are separate supply-chain items and are not covered here.

## Tests

- Swapping the staged file after verification has no effect on what is installed.
- The workspace is `0700` under `~/.autobahn/tmp/`.
