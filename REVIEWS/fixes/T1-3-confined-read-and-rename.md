# T1-3: Resolve `read_file` and `rename` paths without following symlinks

**Tier 1 fix for:** the non-race part of M-5 (DEEPSEEK F15). The reviews described this as a local race. It is reachable without one.

**Not covered here:** the check-then-use race in the transition path (KIMI ABN-M3). That needs fd-anchored operations and belongs to the local-attacker decision.

**Status:** proposed, not implemented. Line numbers are from the working tree on 2026-09-24.

## The problem

Both requests go through `resolve_relative` (`src/endpoint/local.rs:2882`):

```rust
fn resolve_relative(root: &Path, path: &str) -> Result<PathBuf> {
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative.components().any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("{path:?} is not a plain root-relative path");
    }
    Ok(root.join(relative))
}
```

That check is lexical only. After it:

- `read_file` (`:1355`) calls `symlink_metadata` on the full path, which checks only the final component, then `fs::read`, which follows every symlinked parent.
- `rename` (`:1367`) calls `fs::create_dir_all(parent)` and `fs::rename`. Both follow symlinked parents.

No race is needed to exploit this. A peer can create a symlink inside the root, because `SymlinkMode::Raw` is the default and the transition code creates symlink targets as sent. Once `root/link -> /home/you` exists:

- `ReadFile("link/.ssh/id_ed25519")` returns the key. The controller sends `ReadFile` for `autobahn diff`. A hostile controller in attach mode can send it directly.
- A keep-both rename from or to `link/...` moves a file out of, or into, a directory outside the root.

The transition path does not have this bug. It uses `resolve_parent` (`:1903`), which checks every parent component with `verify_directory` (`:3036`).

## The fix

### 1. A `Result`-returning confined resolver

`resolve_parent` reports failures into `self.problem` and returns `Option`, which suits transitions but not these two requests. Add a free function with the same walk:

```rust
/// Resolves a root-relative path to its on-disk location, refusing any
/// path whose root or parent components are not real directories.
/// The final component is returned unresolved: callers decide whether
/// to follow it.
fn resolve_confined(root: &Path, path: &str) -> Result<PathBuf> {
    validate_path(path).map_err(|e| anyhow!("{path:?} is not a valid path: {e}"))?;
    if path.is_empty() {
        bail!("the synchronization root itself cannot be named here");
    }
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    let mut current = root.to_path_buf();
    verify_directory(&current).map_err(|e| anyhow!("unable to resolve {path:?}: {e}"))?;
    for component in parent.split('/').filter(|c| !c.is_empty()) {
        current.push(component);
        verify_directory(&current).map_err(|e| anyhow!("unable to resolve {path:?}: {e}"))?;
    }
    Ok(current.join(name))
}
```

`validate_path` replaces the lexical check in `resolve_relative`, and it also rejects NUL. It gains the reserved-prefix refusal from T1-4.

### 2. `read_file`

```rust
fn read_file(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
    let full = resolve_confined(&self.root, path)?;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&full)
    {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("unable to read {}", full.display())),
    };
    if !file.metadata()?.file_type().is_file() {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(MAXIMUM_READ_FILE_SIZE).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}
```

`ELOOP` from `O_NOFOLLOW` means the final component is a symlink. Today that case returns `Ok(None)`, and this keeps that behaviour. Opening before checking the type removes the gap between `symlink_metadata` and `fs::read`. The read cap is new. `ReadFile` serves `diff`, so a cap well below `MAXIMUM_MESSAGE_SIZE` is appropriate.

### 3. `rename`

```rust
fn rename(&mut self, from: &str, to: &str) -> Result<()> {
    let source = resolve_confined(&self.root, from)?;
    let target = self.create_confined_parents(to)?;
    if fs::symlink_metadata(&target).is_ok() {
        bail!("{to} already exists; move it out of the way first");
    }
    self.observer.invalidate([from, to]);
    let result = fs::rename(&source, &target) /* unchanged */;
    self.observer.invalidate([from, to]);
    result
}

/// Like `resolve_confined`, but creates missing parent directories one
/// component at a time, verifying each, instead of `create_dir_all`.
fn create_confined_parents(&self, path: &str) -> Result<PathBuf> {
    validate_path(path)...;
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    let mut current = self.root.to_path_buf();
    verify_directory(&current)?;
    for component in parent.split('/').filter(|c| !c.is_empty()) {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => bail!("{} is not a directory", current.display()),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // Use the endpoint's configured directory mode, as the
                // transition path does when it creates directories.
                DirBuilder::new().mode(DIRECTORY_MODE).create(&current)?;
                verify_directory(&current)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(current.join(name))
}
```

`fs::rename` does not follow a symlink in the final component of either argument; it renames the link itself. So the final component needs no extra handling.

The existing "refuse, don't overwrite" check followed by a plain `fs::rename` is still a small overwrite window (OPUS Low, L-20). A follow-up can switch it to the existing `publish_rename(.., false)`, which uses `RENAME_NOREPLACE`.

### 4. Retire `resolve_relative`

After this change it has no callers. Delete it so no future request can use the lexical-only check.

## Compatibility

- Genuine requests name paths the controller saw in a scan. Those parents are real directories, so they resolve as before.
- A conflict whose path runs through a symlinked directory can no longer be diffed or keep-both renamed. That is the intended refusal, and the error names the component.

## Tests

1. **`ReadFile` through a symlinked parent is refused.** Create `root/link -> <outside>` and `<outside>/secret`. `read_file("link/secret")` returns an error and no bytes.
2. **`ReadFile` of a final-component symlink returns `None`.** Same as today.
3. **`ReadFile` of a FIFO returns `None` without hanging.**
4. **`rename` out through a symlinked parent is refused.** `rename("a.txt", "link/a.txt")` fails and `<outside>/a.txt` does not exist.
5. **`rename` in from a symlinked parent is refused.** `rename("link/secret", "stolen")` fails and `<outside>/secret` is still there.
6. **`rename` creates missing real parents.** `rename("a.txt", "new/dir/a.txt")` succeeds and both parents are real directories.
7. **The agent direction.** Through `serve_agent`, send `Request::ReadFile("link/secret")` and assert an error response.
