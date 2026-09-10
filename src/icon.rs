//! The icon autobahn hands to whatever shows a notification.
//!
//! macOS will not let a notification carry an image it cannot reach on
//! disk, and the two things that raise one here — a hook in the config,
//! and the menu bar app — run in different processes with different
//! working directories. So the binary carries the image and writes it
//! into the state directory, where both can name it by path.
//!
//! Carried rather than installed because an alert is exactly when the
//! machine is least likely to be in a tidy state: a bundle unpacked, a
//! checkout moved, a binary copied to a server. An icon that lives in the
//! executable cannot be left behind by any of those.

use std::path::{Path, PathBuf};

/// The image, at the size a notification shows.
const IMAGE: &[u8] = include_bytes!("../assets/notification.png");

/// Writes the icon into the state directory if it is not already there,
/// and returns its path.
///
/// A failure is not worth reporting: the icon is decoration, and a
/// notification without one still says what is wrong. The caller gets
/// `None` and leaves the variable unset.
pub fn ensure(state_root: &Path) -> Option<PathBuf> {
    let path = state_root.join("icon.png");
    // Rewritten when the size differs, which is what changes when the art
    // does — cheap enough to check on every alert, and it means an
    // upgraded binary refreshes the file without anyone thinking about it.
    let current = std::fs::metadata(&path).map(|meta| meta.len()).ok();
    if current != Some(IMAGE.len() as u64) {
        std::fs::create_dir_all(state_root).ok()?;
        // Written beside and renamed, so a reader never sees half an image.
        let temporary = path.with_extension("png.tmp");
        std::fs::write(&temporary, IMAGE).ok()?;
        std::fs::rename(&temporary, &path).ok()?;
    }
    Some(path)
}
