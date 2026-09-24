//! What autobahn keeps is private to the user who runs it, whatever the
//! umask: the state root, its subdirectories and the configuration. Run
//! through the binary, under umask `022`, in a child process of its own,
//! so no test changes the umask or the environment the suite shares.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o7777
}

/// Runs the binary with `arguments` under umask `022`, with its state root
/// at `home`.
fn autobahn(home: &Path, arguments: &[&str]) -> Output {
    autobahn_with(home, arguments, &[])
}

/// As [`autobahn`], with more of the child's environment set.
fn autobahn_with(home: &Path, arguments: &[&str], environment: &[(&str, &Path)]) -> Output {
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("umask 022 && exec \"$0\" \"$@\"")
        .arg(env!("CARGO_BIN_EXE_autobahn"))
        .args(arguments)
        .env("AUTOBAHN_HOME", home);
    for (name, value) in environment {
        command.env(name, value);
    }
    command.output().expect("the binary should run")
}

fn succeeded(output: &Output) -> &Output {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// A configuration naming one remote host, `h`, in a private directory.
fn configuration(scratch: &Path) -> std::path::PathBuf {
    let directory = scratch.join("configured");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let alpha = scratch.join("alpha");
    std::fs::create_dir(&alpha).unwrap();
    let path = directory.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[groups.g]\nalpha = \"{}\"\nbetas = [\"h:/srv/b\"]\nmode = \"two-way-conflict\"\n",
            alpha.display()
        ),
    )
    .unwrap();
    path
}

#[test]
fn init_under_umask_022_leaves_the_state_root_0700_and_the_configuration_0600() {
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join(".autobahn");
    succeeded(&autobahn(&home, &["init"]));
    assert_eq!(mode(&home), 0o700);
    assert_eq!(mode(&home.join("config.toml")), 0o600);
    for name in ["sessions", "status", "staging", "peering"] {
        assert_eq!(mode(&home.join(name)), 0o700, "{name}");
    }
}

#[test]
fn disable_keeps_a_0600_configuration_0600() {
    let scratch = tempfile::tempdir().unwrap();
    let config = configuration(scratch.path());
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config_argument = config.to_str().unwrap();
    succeeded(&autobahn(
        &scratch.path().join(".autobahn"),
        &["disable", "--host", "h", "--config", config_argument],
    ));
    assert!(std::fs::read_to_string(&config).unwrap().contains("\"h\""));
    assert_eq!(mode(&config), 0o600);
    // No temporary is left beside it.
    let beside: Vec<_> = std::fs::read_dir(config.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(beside, ["config.toml"]);
}

#[test]
fn a_world_writable_configuration_loads_with_a_warning() {
    let scratch = tempfile::tempdir().unwrap();
    let config = configuration(scratch.path());
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o666)).unwrap();
    let output = autobahn(
        &scratch.path().join(".autobahn"),
        &[
            "enable",
            "--host",
            "h",
            "--config",
            config.to_str().unwrap(),
        ],
    );
    let stderr = String::from_utf8_lossy(&succeeded(&output).stderr).into_owned();
    assert!(stderr.contains("writable by others"), "{stderr}");
    assert!(stderr.contains(&config.display().to_string()), "{stderr}");
}

#[test]
fn a_private_configuration_loads_without_a_warning() {
    let scratch = tempfile::tempdir().unwrap();
    let config = configuration(scratch.path());
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
    let output = autobahn(
        &scratch.path().join(".autobahn"),
        &[
            "enable",
            "--host",
            "h",
            "--config",
            config.to_str().unwrap(),
        ],
    );
    let stderr = String::from_utf8_lossy(&succeeded(&output).stderr).into_owned();
    assert!(!stderr.contains("writable by others"), "{stderr}");
}

/// The session's own directory is private, so its ancestor — which paths
/// exist, and their digests — is out of other users' reach whatever mode
/// the ancestor file itself was written with.
#[test]
fn a_manual_sync_keeps_its_session_state_private() {
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join(".autobahn");
    let alpha = scratch.path().join("alpha");
    let beta = scratch.path().join("beta");
    std::fs::create_dir(&alpha).unwrap();
    std::fs::write(alpha.join("file"), b"content").unwrap();
    succeeded(&autobahn(
        &home,
        &["sync", alpha.to_str().unwrap(), beta.to_str().unwrap()],
    ));
    assert_eq!(std::fs::read(beta.join("file")).unwrap(), b"content");
    assert_eq!(mode(&home), 0o700);
    assert_eq!(mode(&home.join("sessions")), 0o700);
    let sessions: Vec<_> = std::fs::read_dir(home.join("sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert_eq!(mode(&sessions[0]), 0o700);
}

/// `diff` never touches the shared temporary directory — here one it
/// cannot write, standing in for an `autobahn-diff-<pid>` another user
/// made first — and hands the diff tool both sides as `0600` files in a
/// `0700` directory of their own, gone once the tool has exited.
#[test]
fn diff_compares_private_copies_outside_the_shared_temporary_directory() {
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join(".autobahn");
    let alpha = scratch.path().join("alpha");
    let beta = scratch.path().join("beta");
    std::fs::create_dir(&alpha).unwrap();
    std::fs::create_dir(&beta).unwrap();
    std::fs::write(alpha.join("f"), b"one\n").unwrap();
    std::fs::write(beta.join("f"), b"two\n").unwrap();
    let config = scratch.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[groups.g]\nalpha = \"{}\"\nbetas = [\"{}\"]\nmode = \"two-way-conflict\"\n",
            alpha.display(),
            beta.display()
        ),
    )
    .unwrap();

    let shared = scratch.path().join("shared-tmp");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o500)).unwrap();
    // A stand-in diff tool that records what it was handed.
    let bin = scratch.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let record = scratch.path().join("record");
    let tool = bin.join("diff");
    std::fs::write(
        &tool,
        "#!/bin/sh\n\
         shift 5\n\
         for file in \"$1\" \"$2\" \"$(dirname \"$1\")\"; do\n\
         printf '%s %s\\n' \"$(stat -c %a \"$file\")\" \"$file\" >> \"$RECORD\"\n\
         done\n\
         exit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());

    let output = autobahn_with(
        &home,
        &["diff", "g", "f", "--config", config.to_str().unwrap()],
        &[
            ("TMPDIR", &shared),
            ("RECORD", &record),
            ("PATH", Path::new(&path)),
        ],
    );
    succeeded(&output);
    let recorded = std::fs::read_to_string(&record).expect("the diff tool ran");
    let lines: Vec<(&str, &str)> = recorded
        .lines()
        .map(|line| line.split_once(' ').unwrap())
        .collect();
    assert_eq!(lines.len(), 3, "{recorded}");
    assert_eq!(lines[0].0, "600", "{recorded}");
    assert_eq!(lines[1].0, "600", "{recorded}");
    assert_eq!(lines[2].0, "700", "{recorded}");
    let directory = Path::new(lines[2].1);
    assert!(directory.starts_with(home.join("tmp")), "{recorded}");
    assert!(
        !directory.exists(),
        "the scratch directory outlived the diff"
    );
    assert_eq!(std::fs::read_dir(&shared).unwrap().count(), 0);
}
