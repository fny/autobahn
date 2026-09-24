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
    Command::new("/bin/sh")
        .arg("-c")
        .arg("umask 022 && exec \"$0\" \"$@\"")
        .arg(env!("CARGO_BIN_EXE_autobahn"))
        .args(arguments)
        .env("AUTOBAHN_HOME", home)
        .output()
        .expect("the binary should run")
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
