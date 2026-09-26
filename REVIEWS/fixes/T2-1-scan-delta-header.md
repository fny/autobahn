# T2-1: Validate the scan-delta header before using it

**Findings:** H-19 (DEEPSEEK F1, KIMI ABN-H6, GLM M2, OPUS S1). It also carries the cheap part of H-23 (KIMI ABN-H9).
**Status:** proposed. This is the one tier 2 item we fix: it contradicts invariant I9 as written, which says no length received from a connection is trusted before validation.

## Problem

`reassemble` in `src/endpoint/remote.rs` uses three peer-chosen values before checking any of them:

- **`header.length`.** `Vec::with_capacity(header.length as usize)` runs before a single byte arrives. `u64::MAX` aborts the controller with a capacity overflow. A merely huge value runs it out of memory. Either way, every session dies.
- **`header.block_size`.** It is passed straight to `rsync::signature` with no range check. `u32::MAX` allocates about 4 GiB per scan. A value of `1` means one hash per byte of baseline.
- **Ops expansion.** The `output.len() > header.length` check runs once per batch, after all of the batch's ops are applied. A single frame of `Blocks` ops can expand millions of times before the check sees it.

`Signature::validate` in `src/rsync/mod.rs` also doesn't bound `block_size` or `hashes.len()`. That is the cheap part of H-23.

## Proposed resolution

- **Cap the length.** Refuse `header.length` above the maximum message size (`MAXIMUM_MESSAGE_SIZE`, 4 GiB). Pre-allocate at most `min(length, 8 MiB)` and let the vector grow as data arrives.
- **Range-check the block size.** When there is a baseline, require `MINIMUM_BLOCK_SIZE <= block_size <= MAXIMUM_BLOCK_SIZE`, the rsync module's own 1 KiB to 64 KiB.
- **Check every op.** Before applying each op, work out how many bytes it adds, a data length or `count × block_size`. Refuse it if the output would exceed `header.length`.
- **Bound signatures.** `Signature::validate` checks the same block-size range. It also refuses a `hashes.len()` greater than the base length divided by the block size, plus one.

Each refusal is an error on the connection, as a failed digest check is today.

## Tests

- A `ScanDelta` with `length = u64::MAX` is refused without allocating.
- A `ScanDelta` with `block_size` of `0`, `1` or `u32::MAX` and a baseline is refused.
- One batch holding a `Blocks` op that would expand past `length` is refused before it is applied.
- A `Signature` with too many hashes for its size fails `validate`.
- Existing scan-delta round-trip tests pass unchanged.
