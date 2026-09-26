# T1-4: Reserve autobahn's own names and verify the staging root

**Tier 1 fix for:** the inside-root staging symlink arm of M-3 (DEEPSEEK F10). It also closes the staging-directory permission half of M-3 (KIMI ABN-M4, OPUS S3, ASTRA F18).

**Status:** proposed, not implemented. Line numbers are from the working tree on 2026-09-24.

## The problem

### Peers can create names autobahn treats as its own

The scanner hides every entry whose name starts with `.autobahn-tmp` (`src/scan/mod.rs:59`, `:78`). Those names are autobahn's temporaries and staging directories. But `validate_name` (`src/endpoint/local.rs:3060`) only rejects empty names, `.`, `..`, separators and NUL. So a transition from a peer can create an entry with the reserved prefix. Once created, the entry is invisible to every later scan and is never cleaned up or reported.

With `staging = "inside-root"`, the staging directory is `root/.autobahn-tmp-staging-<session>-<side>` (`staging_root_for`, `:2745`). If a peer creates a symlink with that name, pointing outside the root, then `stage_begin` (`:1128`) runs `fs::create_dir_all(&self.staging_root)`. That succeeds through a symlink to a directory. After that:

- `open_receive_file` (`:975`) creates every received file at `staging_root.join(temporary_name("recv"))`, outside the root.
- Staged blobs are renamed to `staging_root.join(<digest hex>)`, outside the root.
- `sweep_staging` deletes 64-hex-named files at the symlink target.

With T1-2 in place, the session and side parts of the name are genuine. The peer only needs to learn them, and a session id is visible in logs and errors.

### Staging directories are created with default permissions

`create_dir_all` gives `0755` under a normal umask. Received content sits there as `0644` until publication applies the configured mode (`0600` by default). This is outside tier 1 strictly, but it is the same line of code, so it is fixed here.

## The fix

### 1. Refuse the reserved prefix in incoming names

Unify the two copies of the constant. `src/endpoint/local.rs:47` and `src/scan/mod.rs:59` both define `TEMPORARY_PREFIX = ".autobahn-tmp"`. Make the scan one `pub(crate)` and import it in `local.rs`, so the refusal and the hiding cannot drift apart.

```rust
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("empty path component".into());
    }
    if name == "." || name == ".." {
        return Err("dot path component".into());
    }
    if name.contains('/') || name.contains('\0') {
        return Err("path component contains a separator or NUL".into());
    }
    if crate::scan::autobahn_temporary(name) {
        return Err("path component uses a name reserved for autobahn".into());
    }
    Ok(())
}
```

Using `autobahn_temporary` means the rule is literally "what the scanner hides, a peer cannot create."

This is safe for genuine traffic. A genuine controller only proposes names that appear in some scan, and the scanner never records reserved names. `validate_path` runs on every transition path (`:1883`), and `validate_name` runs on every child name of a created directory in `create_children` (`:2138`). The refusal therefore covers nested names too. `autobahn_temporary` is currently private to `scan`, so it needs `pub(crate)`.

### 2. Verify the staging root before use

Replace the `create_dir_all` at the top of `stage_begin`:

```rust
fn prepare_staging_root(&self) -> Result<()> {
    let root = &self.staging_root;
    if let Some(parent) = root.parent() {
        // The state area's parents are ours; beside-root and inside-root
        // parents are verified to be real directories.
        verify_directory(parent).map_err(|e| anyhow!("unable to stage: {e}"))?;
    }
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                bail!("{} is not a directory; refusing to stage through it", root.display());
            }
            if metadata.uid() != unsafe { libc::geteuid() } {
                bail!("{} is owned by another user; refusing to stage in it", root.display());
            }
            if metadata.mode() & 0o077 != 0 {
                fs::set_permissions(root, Permissions::from_mode(0o700))?;
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            DirBuilder::new().mode(0o700).create(root)
                .with_context(|| format!("unable to create staging directory {}", root.display()))?;
        }
        Err(e) => return Err(e).context("unable to inspect the staging directory"),
    }
    Ok(())
}
```

For `StagingMode::State`, the parent `~/.autobahn/staging` may not exist yet. Create it with `DirBuilder::new().recursive(true).mode(0o700)` before the checks above.

Notes:

- Tightening an existing directory that has loose permissions, rather than refusing, keeps upgrades working for staging directories created by older versions.
- The owner check matters for `BesideRoot`. There the staging directory sits in the root's parent, which may be writable by other users.
- Temporaries created inside the verified directory still use `File::create`. Switching them to `create_new(true)` with mode `0o600` finishes the M-3 permission fix. That is a one-line change in each of `open_receive_file` (`:976`) and `stage_locally` (`:779`), and belongs in the same commit.

## Compatibility

- Existing inside-root or beside-root staging directories that are real directories owned by the user keep working. Loose ones are tightened to `0700`.
- A tree that genuinely contains a user-created file named `.autobahn-tmp...` is already invisible to autobahn today. Nothing changes for it.

## Tests

1. **A transition creating a reserved name is refused.** Propose creating `.autobahn-tmp-x` as a file, and as a directory child of a new directory. Both are reported as problems and nothing is created.
2. **A symlinked inside-root staging directory is refused.** Create `root/.autobahn-tmp-staging-<session>-beta -> <outside>`. `stage_begin` fails and `<outside>` stays empty.
3. **A staging directory owned by another user is refused.** Skip unless running as root.
4. **New staging directories are `0700`.** Check all three placements under umask `022`.
5. **A pre-existing `0755` staging directory is tightened to `0700`.**
6. **Received temporaries are `0600`.** Interrupt a transfer after the first frame and check the leftover file's mode.
