//! What commands print when their output is not a person at a terminal:
//! the same words, with no styling, and names from disk escaped.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

/// `doctor` piped to a file carries no escape byte: its colour goes
/// through the one styling decision, and a file name holding an escape
/// sequence is shown escaped rather than obeyed.
#[test]
fn doctor_piped_prints_no_escape_bytes() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let alpha = home.path().join("alpha");
    let beta = home.path().join("beta");
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(&beta).unwrap();
    std::fs::write(alpha.join("evil\x1b[31mred"), "content").unwrap();
    let config = home.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[groups.g]\nmode = \"two-way-safe\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
            alpha.display(),
            beta.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_autobahn"))
        .arg("doctor")
        .arg("--config")
        .arg(&config)
        .arg("--state-root")
        .arg(home.path().join("state"))
        .arg("g")
        .env("HOME", home.path())
        .env("AUTOBAHN_HOME", home.path().join(".autobahn"))
        .env("TERM", "xterm")
        .env_remove("NO_COLOR")
        .output()
        .expect("runs autobahn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("the next cycle would"), "{stdout}");
    assert!(stdout.contains("red"), "{stdout}");
    assert!(!output.stdout.contains(&0x1b), "{stdout:?}");
}
