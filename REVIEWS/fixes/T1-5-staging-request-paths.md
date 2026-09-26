# T1-5: Validate staging request paths at `stage_begin`

**Tier 1 fix for:** L-36 (GLM verified-safe note), the receive-side sites. The supply-side site is T1-1.

**Status:** proposed, not implemented. Line numbers are from the working tree on 2026-09-24.

## The problem

`stage_begin` (`src/endpoint/local.rs:1128`) receives `Vec<FileRequest>` from the controller. Each request's `path` names where the content will eventually be published. That path is used in two places before any validation:

- `:1216`: `base_signature(&self.root.join(&request.path))` builds the rsync signature of whatever is at that path. The signature goes back to the supplier.
- `:986`: `open_receive_file` opens `self.root.join(&need.request.path)` as the patch base, when the need's signature is non-empty.

Both are safe today, but only by accident. Each runs only when `snapshot_records_file(&request.path)` is true, and a path the snapshot records cannot be absolute or contain `..`. Nothing states that dependency, and a refactor that computes signatures more eagerly, for example to skip the snapshot probe, would reopen it.

This is the same site as rsync's CVE-2024-12086. There, a malicious server named the client's delta basis file. The client opened it without sanitizing the name, and the server read the file back one byte at a time through checksum responses. In autobahn the signature returned from `base_signature` is the equivalent channel (KIMI ABN-L4).

## The fix

Refuse invalid request paths at the top of `stage_begin`, before any of them is used:

```rust
fn stage_begin(&mut self, files: Vec<FileRequest>) -> Result<Vec<StagingNeed>> {
    for request in &files {
        validate_path(&request.path)
            .map_err(|e| anyhow!("refusing staging request for {:?}: {e}", request.path))?;
        if request.path.is_empty() {
            bail!("refusing a staging request for the synchronization root itself");
        }
    }
    self.prepare_staging_root()?; // T1-4
    ...
```

Refusing the whole batch is intentional. A genuine controller builds these requests from reconciliation over real scans, so an invalid path means the controller is broken or hostile. There is nothing useful to salvage from the rest of the batch.

`open_receive_file` takes its needs from `ReceiveState`, which only `stage_begin` builds. One check at the top of `stage_begin` therefore covers both sites.

`validate_path` includes the reserved-prefix refusal from T1-4, so a request cannot target autobahn's own staging names either.

## Also: make the base-signature path explicitly confined

The snapshot gate stays, because it is the fast path. It also prevents a signature being computed over an ignored file. Make the dependency visible by resolving the base through `resolve_confined` from T1-3:

```rust
let signature = if self.snapshot_records_file(&request.path) {
    match resolve_confined(&self.root, &request.path) {
        Ok(full) => base_signature(&full),
        Err(_) => Signature::default(),
    }
} else {
    Signature::default()
};
```

Inside `base_signature` (`:3021`), the `symlink_metadata` then `File::open` sequence is still a small race (L-4). Switch it to `O_NOFOLLOW | O_NONBLOCK` and an `fstat` on the opened file, matching T1-1. Apply the same change to the base open in `open_receive_file`.

## Compatibility

Genuine controllers never send invalid or empty staging paths, so nothing changes for real traffic.

## Tests

1. **Absolute path refused.** `stage_begin` with `FileRequest { path: "/etc/hosts", .. }` returns an error, and no signature is computed. Assert the error, and that the staging directory holds no new receive state.
2. **`..` refused.** `"../outside.txt"`.
3. **Empty path refused.**
4. **Reserved name refused.** `".autobahn-tmp-staging-x/y"`.
5. **Symlinked parent yields an empty signature.** Create `root/link -> <outside>` and `<outside>/big.bin`. Force the snapshot to record `link/big.bin`. That shape cannot come from a real scan, so build the snapshot by hand. The returned need carries `Signature::default()`.
6. **Regression.** Existing staging tests pass unchanged.
