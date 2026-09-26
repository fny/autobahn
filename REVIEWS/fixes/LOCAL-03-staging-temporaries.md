# LOCAL-03: Private staging temporaries

**Findings:** the rest of M-3 (ASTRA F18, KIMI ABN-M2 and ABN-M4, OPUS S3). T1-4 covers the staging directory itself.
**Status:** proposed. Land it with T1-4.

## Problem

- **Readable received content.** Received and copied content is created with `File::create`, which gives `0644` under a normal umask (`src/endpoint/local.rs:976`, `:779`, and the copy-publish path). It stays that way until publication applies the configured mode. A transfer interrupted midway leaves a readable leftover.
- **Predictable, followable names.** Temporary names are `.autobahn-tmp-<purpose>-<pid>-<counter>`, and the process id is visible to other users. No `O_EXCL` or `O_NOFOLLOW` is used, so a planted symlink at a predicted name redirects the write.
- **Paths leak to the peer.** Temp paths are included in error text sent to the peer (`:957`).

## Proposed resolution

- `temporary_name` adds a random component.
- Every staging temporary opens through `private_file` from LOCAL-01.
- Errors that cross the wire say "a staging file" rather than the path.

## Tests

- Interrupt a transfer after its first frame, and check that the leftover is `0600`.
- Plant a symlink at the next temporary name, and check that nothing is written through it.
- Check that a supply failure's error text contains no staging path.
