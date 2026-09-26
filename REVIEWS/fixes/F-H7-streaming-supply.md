# F-H7: Supply streams a file instead of buffering all of it

**Findings:** H-7 (ASTRA F11, measured; OPUS H5; KIMI ABN-H8).
**Status:** proposed. High; fix before v1.

## Problem

`supply_pull` (`src/endpoint/local.rs:1242`) calls `buffer_delta` (`:808`) for the next need. That calls `try_supply` and then `supply_from` (`:855`), which reads the *whole* file into the `pending` queue before one frame is returned. With an empty signature, it reads the file in 64 KiB chunks. With a real signature, `rsync::deltify` pushes every operation through its callback. The batch limit applies only afterwards.

For a new file, or one with little reusable content, the delta *is* the file. ASTRA measured the extra live heap when requesting a single frame:

| File | Extra heap before the first frame |
|---|---|
| 32 MiB | 33.6 MB |
| 128 MiB | 134.4 MB |

`max_file_size` defaults to unlimited. Supplying a 20 GB VM image on the controller allocates about 20 GB inside the supervisor, and every session dies when it runs out. Time to first byte also includes reading or deltifying the whole file.

KIMI's hostile variant: a destination sends one fake block hash for the largest file, which forces the non-streaming delta path. After T1-1, only files the source really scanned can be named, but the memory problem stays.

**Design constraint.** Buffering the whole file is what makes the alternate-path fallback work. `try_supply` truncates `pending` back to its mark when an attempt fails (`:846-850`), and `buffer_delta` then tries another path with the same digest. A streaming supplier can't take back frames it has already sent.

## Proposed resolution

- **Resumable supply state.** `SupplyState` keeps the current need's source, not its whole output:
  - **Empty signature:** an open `File`, plus the bytes remaining. Each `supply_pull` reads the next chunks, up to `SUPPLY_TARGET_BYTES`.
  - **Real signature:** run `deltify` on a helper thread, feeding a bounded `sync_channel` sized to a few batches. `supply_pull` drains the channel. Dropping the receiver ends the thread.
- **Fall back only before the first frame.** Open the file and check it, by the T1-1 gate and `fstat`, before sending `Begin`. If that fails, try the alternate paths, as today. Once the first operation frame has gone out, a later failure ends the file with `TransferFrame::EndOfFile { error: Some(..) }`, which the receiver already handles by discarding its partial copy. The next cycle retries.
- **Framing stays as it is.** `Begin`, then operations, then `EndOfFile`. No wire change.
- **Memory bound.** Peak supply memory becomes about one batch per stream, `SUPPLY_TARGET_BYTES` plus the channel's depth. It no longer grows with file size.

## Tests

- **Memory:** supplying a 256 MiB file, with both an empty and a mismatching signature, keeps peak `pending` plus channel memory under a small multiple of `SUPPLY_TARGET_BYTES`. Use ASTRA's allocation-probe approach: a counting global allocator in a test binary.
- **Time to first frame** for a 1 GiB file stays under a fixed bound, independent of size.
- **Fallback before the first frame:** the existing `supply_recovers_from_an_alternate_path_sharing_the_digest` still passes.
- **Failure mid-stream:** truncate the file after the first frame. The receiver gets `EndOfFile` with an error, discards the partial file, and the next cycle converges.
- **Hostile signature:** a single fake block hash doesn't raise memory beyond the bound.
