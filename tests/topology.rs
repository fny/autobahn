//! Topology refusals made before any endpoint opens: a session over a tree
//! and itself (or a tree nested in it), and a root holding autobahn's own
//! state. Each is driven through the real CLI, with a home of its own, so
//! a refusal can be shown to have written nothing at all.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

mod common;

/// A temporary world: a home for the CLI, and the trees it synchronizes.
struct World {
    keep: TempDir,
}

impl World {
    fn new() -> World {
        common::isolate_home();
        let world = World {
            keep: TempDir::new().expect("temporary directory should be creatable"),
        };
        world.directory("home");
        world
    }

    /// Returns a path within the world, creating it as a directory.
    fn directory(&self, name: &str) -> PathBuf {
        let path = self.keep.path().join(name);
        fs::create_dir_all(&path).expect("directory should be creatable");
        path
    }

    /// The home the CLI runs under.
    fn home(&self) -> PathBuf {
        self.directory("home")
    }

    /// Runs the CLI under the world's home.
    fn cli(&self, args: &[&str]) -> (bool, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_autobahn"))
            .args(args)
            .env("HOME", self.home())
            .env_remove("AUTOBAHN_HOME")
            .output()
            .expect("the CLI runs");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status.success(), text)
    }
}

fn write(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().expect("a parent")).expect("parent should be creatable");
    fs::write(path, contents).expect("file should be writable");
}

/// Every path under a root, relative and sorted: what "nothing was
/// written" is checked against.
fn listing(root: &Path) -> Vec<String> {
    fn walk(root: &Path, directory: &Path, into: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("entry").path();
            into.push(
                path.strip_prefix(root)
                    .expect("under the root")
                    .display()
                    .to_string(),
            );
            if path.is_dir() && !path.is_symlink() {
                walk(root, &path, into);
            }
        }
    }
    let mut into = Vec::new();
    walk(root, root, &mut into);
    into.sort();
    into
}

fn path(path: &Path) -> &str {
    path.to_str().expect("a UTF-8 path")
}

/// `autobahn sync ALPHA BETA` over a tree and itself, or a tree nested
/// either way around, is refused before anything opens — as a configured
/// session over the same shape is. In replica mode the nested shape used
/// to delete the alpha root through the beta path.
#[test]
fn a_manual_sync_refuses_a_tree_and_itself() {
    let world = World::new();
    let tree = world.directory("tree");
    let source = tree.join("source");
    write(&source, "keep.txt", "precious");
    write(&tree, "other.txt", "also precious");
    let alias = world.keep.path().join("alias");
    std::os::unix::fs::symlink(&source, &alias).expect("symlink should be creatable");

    for (alpha, beta, how) in [
        (&source, &source, "the same tree"),
        (&tree, &source, "inside the alpha"),
        (&source, &tree, "containing the alpha"),
        // Identities are physical paths, so an alias is the same tree.
        (&source, &alias, "the same tree"),
    ] {
        let before = listing(world.keep.path());
        let (ok, text) = world.cli(&["sync", path(alpha), path(beta), "--mode", "one-way-replica"]);
        assert!(!ok, "{alpha:?} vs {beta:?} should be refused: {text}");
        assert!(text.contains(how), "{alpha:?} vs {beta:?}: {text}");
        let after = listing(world.keep.path());
        assert_eq!(before, after, "{alpha:?} vs {beta:?} wrote something");
        assert!(
            !world.home().join(".autobahn").exists(),
            "{alpha:?} vs {beta:?} made state"
        );
    }
    assert_eq!(
        fs::read_to_string(source.join("keep.txt")).expect("the alpha survives"),
        "precious"
    );

    // Sibling trees are two trees, and synchronize.
    let sibling = world.keep.path().join("sibling");
    let (ok, text) = world.cli(&[
        "sync",
        path(&source),
        path(&sibling),
        "--mode",
        "one-way-replica",
    ]);
    assert!(ok, "{text}");
    assert_eq!(
        fs::read_to_string(sibling.join("keep.txt")).expect("the beta is written"),
        "precious"
    );
}
