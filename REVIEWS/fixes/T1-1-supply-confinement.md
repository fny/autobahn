# T1-1: Supply only files this side's own snapshot records

**Tier 1 fix for:** C-2 (KIMI ABN-C1, OPUS S2). Also covers the supply half of L-36.

**Status:** proposed, not implemented. Line numbers are from the working tree on 2026-09-24.

## The problem

A destination's `StageBegin` answer is a list of `StagingNeed { request: FileRequest { path, digest, .. }, signature }`. The source stores it verbatim in `supply_open` (`src/endpoint/local.rs:1233`). `supply_pull` (`:1242`) walks the needs and calls `buffer_delta` (`:808`), which calls `try_supply` then `supply_from` (`:855`).

`supply_from` does this with the wire path:

```rust
let disk_path = self.root.join(path);
match File::open(&disk_path) { ... }
```

There are three escapes, all with default settings:

- An absolute path replaces the root entirely, because `Path::join` with an absolute argument discards the base.
- `..` components walk out of the root.
- `File::open` follows symlinks. A symlink inside the root, which a peer can create because `SymlinkMode::Raw` is the default, redirects a lexically clean path.

Two more problems come from the same site:

- A FIFO or `/dev/zero` named by the peer hangs `open()` or streams forever into `pending`.
- A path inside the root that the local scan marked as ignored, such as `.env`, is still supplied. Ignores are not enforced on the supply side.

The same code serves both directions. The controller supplies its local files to a remote destination, so a hostile agent can read the controller's files. An agent answers `Request::SupplyOpen` (`src/transport/mod.rs:729`), so a hostile controller in peering attach mode can read the follower's files.

## The fix

Gate every supply attempt on this endpoint's own last scan. The scanner records a path as `Content::File` only after `lstat` shows a regular file, and it never descends through a symlink. A path the snapshot records as a file is therefore root-relative, free of `..`, free of symlinked parents, and not ignored. Requiring the digest to match means only the content the destination asked for can leave.

### 1. A lookup that returns the recorded node

`snapshot_records_file` (`:758`) already walks `last_snapshot` but returns a `bool`. Generalize it:

```rust
/// The digest and scan metadata the last scan recorded for a regular file
/// at a root-relative path, or `None` when it recorded anything else.
fn snapshot_file(&self, path: &str) -> Option<(&Digest, &FileMetadata)> {
    let mut node = self.last_snapshot.as_ref()?.root.as_ref()?;
    if path.is_empty() {
        return None;
    }
    for component in path.split('/') {
        node = node.child(component)?;
    }
    match &node.content {
        Content::File { digest, metadata, .. } => Some((digest, metadata)),
        _ => None,
    }
}

fn snapshot_records_file(&self, path: &str) -> bool {
    self.snapshot_file(path).is_some()
}
```

### 2. Pass the requested digest down to `supply_from`

`buffer_delta` already holds `need.request.digest`. Thread it through `try_supply` into `supply_from`:

```rust
fn try_supply(&self, path: &str, digest: &Digest, signature: &Signature,
              pending: &mut VecDeque<TransferFrame>) -> Result<(), String>
```

Both call sites in `buffer_delta` pass `&need.request.digest`: the primary path and the `digest_paths` alternates. The alternates already come from the local snapshot, so they pass the gate by construction.

### 3. Gate and open safely in `supply_from`

```rust
fn supply_from(&self, path: &str, digest: &Digest, signature: &Signature,
               pending: &mut VecDeque<TransferFrame>) -> Result<(), String> {
    validate_path(path).map_err(|e| format!("refused {path:?}: {e}"))?;
    let (recorded, scanned) = self
        .snapshot_file(path)
        .ok_or_else(|| format!("refused {path:?}: not a file the last scan recorded"))?;
    if recorded != digest {
        return Err(format!("refused {path:?}: its scanned content is not the requested content"));
    }

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(self.root.join(path))
        .map_err(|e| format!("unable to open {path}: {e}"))?;
    let now = file.metadata().map_err(|e| format!("unable to stat {path}: {e}"))?;
    if !now.file_type().is_file()
        || (scanned.inode != 0 && now.ino() != scanned.inode)
        || now.len() != scanned.size
    {
        return Err(format!("{path} changed since the scan"));
    }
    let file = file.take(scanned.size);
    // ... existing streaming / deltify logic, reading from `file` ...
}
```

What each check does:

| Check | Stops |
|---|---|
| `validate_path` | Absolute and `..` paths. The snapshot walk would also stop them, but an explicit refusal gives a clear error. |
| Snapshot membership | Paths outside the root, paths through symlinked parents, ignored or untracked paths, special files. |
| Digest equality | Supplying content that was not requested. |
| `O_NOFOLLOW` | A final component swapped for a symlink after the scan. |
| `O_NONBLOCK` | A FIFO swapped in after the scan hanging `open()`. Reads from a regular file ignore the flag. |
| Inode and size against the snapshot | A parent directory swapped for a symlink after the scan. The file reached through the swap has a different inode. `inode == 0` means the platform did not report one, so the check is skipped. |
| `take(scanned.size)` | Streaming forever from a file that grows after the scan. |

A refusal returns `Err(String)`, the same way a file that vanished since the scan does today. `buffer_delta` then tries the alternates and, failing those, ends the stream with `TransferFrame::EndOfFile { error: Some(..) }`. The receiver already discards a file ended that way. No protocol change and no compatibility-epoch bump are needed.

## Compatibility

- A genuine destination only requests paths the source's snapshot records with that digest. Those requests come from the controller's reconciliation over the same snapshot. The gate therefore never refuses legitimate traffic.
- There is one timing case. If a file is edited between the scan and the supply, the digest no longer matches the disk. Today the supply goes ahead and the receiver's digest check fails the file. With the gate, the size or inode check refuses first. The outcome is the same: the file is retried next cycle.
- `supply_from` gains a `digest` parameter. Nothing outside the file calls it.

## Tests

Add to the `local.rs` test module:

1. **Absolute path is refused.** Open a supply with a need for `/etc/hosts`. Assert that the stream holds `Begin`, then `EndOfFile { error: Some(_) }`, and no `Op` frames.
2. **`..` is refused.** Same shape with `../outside.txt`, where the file exists next to the root.
3. **Symlinked parent is refused.** Create `root/link -> <outside dir>` and `<outside dir>/secret.txt`. Scan. Request `link/secret.txt`. Nothing is streamed.
4. **Unrequested digest is refused.** Request an in-root scanned file but with a different digest.
5. **Ignored file is refused.** Ignore `.env`, scan, and request `.env` with its real digest.
6. **FIFO swapped in after the scan is refused without hanging.** Scan a regular file, replace it with a FIFO, and supply. Wrap the test in a timeout.
7. **Growing file stops at the scanned size.** Scan, append to the file, and supply. The streamed bytes equal the scanned size and the receiver's digest check passes or fails cleanly.
8. **The agent direction.** Through `serve_agent`, send `SupplyOpen` with an absolute path and assert no content frames come back.
9. **Regression.** The existing `supply_recovers_from_an_alternate_path_sharing_the_digest` still passes.

## Docs

`docs/correctness/INVARIANTS.md` gets a confinement statement that does not depend on genuine binaries. Proposed wording: "A peer can name only content this side's own scan recorded inside its root. No request from a peer, genuine or not, reads, writes or deletes outside the synchronization root or the session's own state." Point it at tests 1 to 3 and 8.
