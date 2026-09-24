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

/// Writes the configuration at the world home's default location.
fn default_config(world: &World, text: &str) -> PathBuf {
    let path = world.home().join(".autobahn/config.toml");
    write(&world.home(), ".autobahn/config.toml", text);
    path
}

/// A root of `~` holds `~/.autobahn`: the configuration, the alert hook,
/// the ancestors and the installed agents. Synchronized, a peer that
/// edits its copy of `config.toml` chooses the next `agent_command` run
/// here. The root is refused unless the group ignores the state root.
#[test]
fn a_home_root_holding_the_state_root_is_refused_unless_ignored() {
    let world = World::new();
    let home = world.home();
    write(&home, "notes.txt", "mine");
    let beta = world.keep.path().join("beta");
    let config = |ignores: &str| {
        format!(
            r#"
            [groups.home]
            alpha = "~"
            mode = "two-way-safe"
            betas = ["{beta}"]
            {ignores}
            "#,
            beta = beta.display()
        )
    };

    default_config(&world, &config(""));
    let (ok, text) = world.cli(&["sync"]);
    assert!(!ok, "{text}");
    let state = home.join(".autobahn");
    assert!(
        text.contains(&format!(
            "the root {} contains autobahn's own state at {}; add it to ignores, or choose a \
             narrower root",
            fs::canonicalize(&home).unwrap().display(),
            fs::canonicalize(&state).unwrap().display()
        )),
        "{text}"
    );
    assert!(!beta.exists(), "a refused sync wrote the beta");

    // The same refusal from `watch`, at startup rather than never.
    let mut child = Command::new(env!("CARGO_BIN_EXE_autobahn"))
        .args(["watch", "--log"])
        .env("HOME", &home)
        .env_remove("AUTOBAHN_HOME")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("watch runs");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("watch can be waited for") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("watch ran over a root holding its own state");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let output = child.wait_with_output().expect("watch output");
    assert!(!status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("contains autobahn's own state"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!beta.exists(), "a refused watch wrote the beta");

    // Ignored, the home directory synchronizes without its state.
    default_config(&world, &config(r#"ignores = [".autobahn"]"#));
    let (ok, text) = world.cli(&["sync"]);
    assert!(ok, "{text}");
    assert_eq!(fs::read_to_string(beta.join("notes.txt")).unwrap(), "mine");
    assert!(!beta.join(".autobahn").exists());
}

/// A state root moved with `--state-root` is refused inside a root just
/// the same, and so is a root holding the configuration file.
#[test]
fn a_root_holding_a_custom_state_root_or_the_configuration_is_refused() {
    let world = World::new();
    let tree = world.directory("tree");
    write(&tree, "file.txt", "content");
    let beta = world.keep.path().join("beta");
    let text = format!(
        r#"
        [groups.tree]
        alpha = "{tree}"
        mode = "two-way-safe"
        betas = ["{beta}"]
        "#,
        tree = tree.display(),
        beta = beta.display()
    );
    let outside = world.keep.path().join("config.toml");
    fs::write(&outside, &text).unwrap();
    let state = tree.join("state");
    let (ok, output) = world.cli(&[
        "sync",
        "--config",
        path(&outside),
        "--state-root",
        path(&state),
    ]);
    assert!(!ok, "{output}");
    assert!(output.contains("contains autobahn's own state"), "{output}");
    assert!(
        !beta.exists() && !state.exists(),
        "a refused sync wrote something"
    );

    let inside = tree.join("settings/config.toml");
    write(&tree, "settings/config.toml", &text);
    let elsewhere = world.keep.path().join("state");
    let (ok, output) = world.cli(&[
        "sync",
        "--config",
        path(&inside),
        "--state-root",
        path(&elsewhere),
    ]);
    assert!(!ok, "{output}");
    assert!(
        output.contains("contains autobahn's configuration"),
        "{output}"
    );
    assert!(
        !beta.exists() && !elsewhere.exists(),
        "a refused sync wrote something"
    );
}

/// `autobahn sync ALPHA BETA` is held to the same rule, with `--ignore`
/// as the way through.
#[test]
fn a_manual_sync_of_the_home_directory_needs_the_state_root_ignored() {
    let world = World::new();
    let home = world.home();
    write(&home, "notes.txt", "mine");
    write(&home, ".autobahn/config.toml", "");
    let beta = world.keep.path().join("beta");
    let (ok, text) = world.cli(&["sync", path(&home), path(&beta)]);
    assert!(!ok, "{text}");
    assert!(text.contains("contains autobahn's own state"), "{text}");
    assert!(!beta.exists());

    let (ok, text) = world.cli(&["sync", path(&home), path(&beta), "--ignore", ".autobahn"]);
    assert!(ok, "{text}");
    assert_eq!(fs::read_to_string(beta.join("notes.txt")).unwrap(), "mine");
    assert!(!beta.join(".autobahn").exists());
}

/// A root holding credentials is synchronized as asked, and said so,
/// once, at the start of the run.
#[test]
fn a_root_holding_credentials_is_warned_about_at_startup() {
    let world = World::new();
    let tree = world.directory("tree");
    write(&tree, ".aws/credentials", "secret");
    let beta = world.keep.path().join("beta");
    let config = world.keep.path().join("config.toml");
    let text = |extra: &str| {
        format!(
            r#"
            [groups.tree]
            alpha = "{tree}"
            mode = "two-way-safe"
            betas = ["{beta}"]
            {extra}
            "#,
            tree = tree.display(),
            beta = beta.display()
        )
    };
    let state = world.keep.path().join("state");
    let run = || {
        world.cli(&[
            "sync",
            "--config",
            path(&config),
            "--state-root",
            path(&state),
        ])
    };

    fs::write(&config, text("")).unwrap();
    let (ok, output) = run();
    assert!(ok, "{output}");
    assert_eq!(
        output.matches("holds credentials (.aws)").count(),
        1,
        "{output}"
    );

    fs::write(&config, text("acknowledge_secrets = true")).unwrap();
    let (ok, output) = run();
    assert!(ok && !output.contains("credentials"), "{output}");

    // The manual form says so too, and names its own way out.
    let other = world.keep.path().join("other");
    let (ok, output) = world.cli(&["sync", path(&tree), path(&other)]);
    assert!(ok, "{output}");
    assert!(output.contains("holds credentials (.aws)"), "{output}");
    assert!(output.contains("--ignore"), "{output}");
}
