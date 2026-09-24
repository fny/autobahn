//! Private files and directories: the one place that knows how to make
//! something only the current user can read, so each caller does not
//! repeat (or forget) the checks.
//!
//! Everything here is Unix: modes, owners and `O_NOFOLLOW` are the point.

use std::fs::{DirBuilder, File, Metadata, OpenOptions, Permissions};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};

/// How old an entry under `<state root>/tmp/` must be before the startup
/// sweep removes it: long enough that a viewer opened on a scratch copy
/// has finished with it.
pub const TMP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Makes `path` a directory only the current user can use.
///
/// Guarantees, when it returns `Ok`:
///
/// - `path` is a real directory, not a symbolic link to one, owned by the
///   effective uid.
/// - A directory this call created is mode `0700`, whatever the umask.
/// - An existing one has no group, other, setuid, setgid or sticky bits:
///   looser permissions are tightened to the owner's bits, with a warning
///   on standard error naming the path and the old mode.
///
/// It creates with a single `mkdir`, never `create_dir_all`, so a missing
/// parent is an error rather than a chain of directories at the default
/// umask. A symbolic link, a non-directory or a directory owned by another
/// user is refused with an error naming the path and the problem, and
/// nothing is changed through it. The mode is checked and changed through
/// a descriptor opened with `O_NOFOLLOW`, so a swap between the check and
/// the change is refused rather than followed.
pub fn private_dir(path: &Path) -> Result<()> {
    let created = match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => false,
        Err(error) => {
            return Err(error).with_context(|| format!("unable to create {}", path.display()))
        }
    };
    let seen = std::fs::symlink_metadata(path)
        .with_context(|| format!("unable to inspect {}", path.display()))?;
    check_private_dir(path, &seen)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("unable to open {}", path.display()))?;
    let opened = directory
        .metadata()
        .with_context(|| format!("unable to inspect {}", path.display()))?;
    if (opened.dev(), opened.ino()) != (seen.dev(), seen.ino()) {
        bail!("{} was replaced while it was being checked", path.display());
    }
    check_private_dir(path, &opened)?;
    let mode = opened.mode() & 0o7777;
    let wanted = if created { 0o700 } else { mode & 0o700 };
    if mode != wanted {
        if !created {
            crate::complain!(
                "warning: {} was mode {mode:04o}, readable or writable by others; tightening it to {wanted:04o}",
                path.display()
            );
        }
        directory
            .set_permissions(Permissions::from_mode(wanted))
            .with_context(|| format!("unable to tighten the permissions of {}", path.display()))?;
    }
    Ok(())
}

/// Refuses anything at `path`, described by its unfollowed `metadata`,
/// that is not a real directory owned by the effective uid.
fn check_private_dir(path: &Path, metadata: &Metadata) -> Result<()> {
    let kind = metadata.file_type();
    if kind.is_symlink() {
        bail!(
            "{} is a symbolic link; a private directory must be a real one",
            path.display()
        );
    }
    if !kind.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if metadata.uid() != me {
        bail!(
            "{} is owned by uid {}, not by this user (uid {me})",
            path.display(),
            metadata.uid()
        );
    }
    Ok(())
}

/// Creates a new file at `path` that only the current user can read,
/// opened for reading and writing.
///
/// Guarantees, when it returns `Ok`: the file did not exist before this
/// call, this call created it, and its mode is `0600` or tighter (the
/// umask can only remove bits). It is opened with `O_CREAT | O_EXCL` and
/// `O_NOFOLLOW`, so an existing name is refused rather than truncated or
/// reused, and a symbolic link — dangling or not — is refused rather than
/// followed to create or write its target.
pub fn private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("unable to create the private file {}", path.display()))
}

/// Returns `bytes` random bytes from `/dev/urandom` as lowercase hex:
/// `2 * bytes` characters, unpredictable to other local users, for
/// temporary names they must not be able to guess and pre-create.
pub fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0u8; bytes];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut buffer))
        .context("unable to read /dev/urandom")?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Returns `<state_root>/tmp`, making both it and `state_root` private
/// directories first (see [`private_dir`]). Scratch files that must
/// outlive their writer, such as a copy handed to a viewer, go here
/// rather than under the shared `/tmp`; [`sweep_private_tmp`] clears
/// them away later.
pub fn private_tmp_root(state_root: &Path) -> Result<PathBuf> {
    private_dir(state_root)?;
    let tmp = state_root.join("tmp");
    private_dir(&tmp)?;
    Ok(tmp)
}

/// A private temporary directory, removed with everything in it when the
/// guard is dropped. Removal is best effort: a failure is ignored, and
/// the startup sweep catches what is left, as it does after a crash.
#[derive(Debug)]
pub struct PrivateTempDir {
    path: PathBuf,
}

impl PrivateTempDir {
    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateTempDir {
    fn drop(&mut self) {
        // `remove_dir_all` does not follow symbolic links inside it.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Creates a new temporary directory under `~/.autobahn/tmp/` (the state
/// root's, so `$AUTOBAHN_HOME` moves it too); see [`private_tempdir_in`].
pub fn private_tempdir() -> Result<PrivateTempDir> {
    private_tempdir_in(&crate::paths::default_state_root()?)
}

/// Creates a new temporary directory under `<state_root>/tmp/`, never
/// under the shared `/tmp`.
///
/// Guarantees, when it returns `Ok`: `state_root` and its `tmp` are
/// private directories (see [`private_dir`]); the new directory has a
/// random 128-bit name from `/dev/urandom`, was created by this call (a
/// name collision is retried, never reused) and is mode `0700`. The
/// returned guard removes it, and everything in it, when dropped.
pub fn private_tempdir_in(state_root: &Path) -> Result<PrivateTempDir> {
    let tmp = private_tmp_root(state_root)?;
    for _ in 0..8 {
        let path = tmp.join(random_hex(16)?);
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(PrivateTempDir { path }),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("unable to create {}", path.display()))
            }
        }
    }
    bail!("unable to find an unused name under {}", tmp.display())
}

/// Removes every entry directly under `<state_root>/tmp/` last modified
/// at least `max_age` ago, returning how many it removed. Meant for
/// startup, with [`TMP_MAX_AGE`].
///
/// Guarantees: a missing `tmp` is nothing to do, and is not created. A
/// `tmp` that is a symbolic link, not a directory or owned by another
/// user is refused with an error, and nothing is removed. Entries are
/// judged by their own modification time, not their target's, and
/// symbolic links among them are removed, never followed. An entry that
/// cannot be inspected or removed is reported on standard error and
/// skipped, so one stuck entry does not stop the rest.
pub fn sweep_private_tmp(state_root: &Path, max_age: Duration) -> Result<usize> {
    let tmp = state_root.join("tmp");
    let metadata = match std::fs::symlink_metadata(&tmp) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("unable to inspect {}", tmp.display()))
        }
    };
    check_private_dir(&tmp, &metadata)?;
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in
        std::fs::read_dir(&tmp).with_context(|| format!("unable to list {}", tmp.display()))?
    {
        let entry = entry.with_context(|| format!("unable to list {}", tmp.display()))?;
        let path = entry.path();
        let swept = entry.metadata().and_then(|metadata| {
            let modified = metadata.modified()?;
            // A time in the future is young, not old.
            if now.duration_since(modified).unwrap_or(Duration::ZERO) < max_age {
                return Ok(false);
            }
            if metadata.file_type().is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
            Ok(true)
        });
        match swept {
            Ok(true) => removed += 1,
            Ok(false) => {}
            Err(error) => {
                crate::complain!("unable to sweep {}: {error}", path.display());
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use std::time::SystemTime;

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    /// Runs `body` under umask `022`, the usual default, and puts the
    /// process's umask back afterwards.
    fn under_umask_022<T>(body: impl FnOnce() -> T) -> T {
        // SAFETY: `umask` only swaps the process's mask.
        let previous = unsafe { libc::umask(0o022) };
        let result = body();
        unsafe { libc::umask(previous) };
        result
    }

    fn age(path: &Path, by: Duration) {
        let file = File::open(path).unwrap();
        file.set_modified(SystemTime::now() - by).unwrap();
    }

    #[test]
    fn a_new_directory_is_0700_and_a_new_file_0600_under_umask_022() {
        let scratch = tempfile::tempdir().unwrap();
        let directory = scratch.path().join("state");
        let file = directory.join("file");
        under_umask_022(|| {
            private_dir(&directory).unwrap();
            private_file(&file).unwrap();
        });
        assert_eq!(mode(&directory), 0o700);
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn an_existing_private_directory_is_accepted_as_it_is() {
        let scratch = tempfile::tempdir().unwrap();
        let directory = scratch.path().join("state");
        private_dir(&directory).unwrap();
        std::fs::write(directory.join("kept"), b"x").unwrap();
        private_dir(&directory).unwrap();
        assert_eq!(mode(&directory), 0o700);
        assert!(directory.join("kept").exists());
    }

    #[test]
    fn an_existing_0755_directory_is_tightened_to_0700() {
        let scratch = tempfile::tempdir().unwrap();
        let directory = scratch.path().join("state");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&directory).unwrap();
        assert_eq!(mode(&directory), 0o700);
    }

    #[test]
    fn a_symlink_to_a_directory_is_refused_as_a_private_directory() {
        let scratch = tempfile::tempdir().unwrap();
        let target = scratch.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = scratch.path().join("state");
        symlink(&target, &link).unwrap();
        let error = format!("{:#}", private_dir(&link).unwrap_err());
        assert!(error.contains("symbolic link"), "{error}");
        assert!(error.contains(&link.display().to_string()), "{error}");
        // Nothing was done through the link.
        assert_eq!(mode(&target), 0o755);
    }

    #[test]
    fn a_file_is_refused_as_a_private_directory() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("state");
        std::fs::write(&path, b"x").unwrap();
        let error = format!("{:#}", private_dir(&path).unwrap_err());
        assert!(error.contains("not a directory"), "{error}");
    }

    #[test]
    fn a_private_directory_is_made_only_under_an_existing_parent() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("missing").join("state");
        assert!(private_dir(&path).is_err());
        assert!(!scratch.path().join("missing").exists());
    }

    #[test]
    fn a_directory_owned_by_another_user_is_refused() {
        // SAFETY: `geteuid` has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: giving a directory to another uid needs root");
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let directory = scratch.path().join("state");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::chown(&directory, Some(65534), Some(65534)).unwrap();
        let error = format!("{:#}", private_dir(&directory).unwrap_err());
        assert!(error.contains("owned by uid 65534"), "{error}");
    }

    #[test]
    fn an_existing_name_is_refused_as_a_private_file() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("file");
        std::fs::write(&path, b"before").unwrap();
        assert!(private_file(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"before");
    }

    #[test]
    fn a_symlink_is_refused_as_a_private_file() {
        let scratch = tempfile::tempdir().unwrap();
        let target = scratch.path().join("target");
        let link = scratch.path().join("file");
        // Dangling: opening through it would create the target.
        symlink(&target, &link).unwrap();
        assert!(private_file(&link).is_err());
        assert!(!target.exists());
        // And pointing at a file: opening through it would write there.
        std::fs::write(&target, b"before").unwrap();
        assert!(private_file(&link).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"before");
    }

    #[test]
    fn a_private_tempdir_is_private_randomly_named_and_removed_on_drop() {
        let scratch = tempfile::tempdir().unwrap();
        let state_root = scratch.path().join(".autobahn");
        let (first, second) = under_umask_022(|| {
            (
                private_tempdir_in(&state_root).unwrap(),
                private_tempdir_in(&state_root).unwrap(),
            )
        });
        assert_eq!(first.path().parent().unwrap(), state_root.join("tmp"));
        assert_ne!(first.path(), second.path());
        assert_eq!(mode(first.path()), 0o700);
        assert_eq!(mode(&state_root.join("tmp")), 0o700);
        assert_eq!(mode(&state_root), 0o700);
        std::fs::create_dir(first.path().join("inner")).unwrap();
        std::fs::write(first.path().join("inner").join("file"), b"x").unwrap();
        let path = first.path().to_path_buf();
        drop(first);
        assert!(!path.exists());
        assert!(second.path().exists());
    }

    #[test]
    fn the_sweep_removes_only_old_entries() {
        let scratch = tempfile::tempdir().unwrap();
        let state_root = scratch.path().join(".autobahn");
        let fresh = private_tempdir_in(&state_root).unwrap();
        let stale = private_tempdir_in(&state_root).unwrap();
        let stale_path = stale.path().to_path_buf();
        std::fs::write(stale_path.join("file"), b"x").unwrap();
        std::mem::forget(stale);
        let stale_file = state_root.join("tmp").join("viewer-copy");
        std::fs::write(&stale_file, b"x").unwrap();
        let day = Duration::from_secs(24 * 60 * 60);
        age(&stale_path, 2 * day);
        age(&stale_file, 2 * day);

        assert_eq!(sweep_private_tmp(&state_root, day).unwrap(), 2);
        assert!(!stale_path.exists());
        assert!(!stale_file.exists());
        assert!(fresh.path().exists());
    }

    #[test]
    fn the_sweep_does_not_follow_links() {
        let scratch = tempfile::tempdir().unwrap();
        let state_root = scratch.path().join(".autobahn");
        let outside = scratch.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("precious"), b"x").unwrap();
        age(&outside, Duration::from_secs(1_000_000));
        drop(private_tempdir_in(&state_root).unwrap());
        let link = state_root.join("tmp").join("link");
        symlink(&outside, &link).unwrap();
        // Linking is instant, so wait out a zero age.
        sweep_private_tmp(&state_root, Duration::ZERO).unwrap();
        assert!(outside.join("precious").exists());

        // A tmp that is itself a link is refused, not swept through.
        std::fs::remove_dir_all(state_root.join("tmp")).unwrap();
        symlink(&outside, state_root.join("tmp")).unwrap();
        assert!(sweep_private_tmp(&state_root, Duration::ZERO).is_err());
        assert!(outside.join("precious").exists());
    }

    #[test]
    fn the_sweep_of_a_missing_tmp_is_nothing_to_do() {
        let scratch = tempfile::tempdir().unwrap();
        assert_eq!(
            sweep_private_tmp(scratch.path(), Duration::ZERO).unwrap(),
            0
        );
    }
}
