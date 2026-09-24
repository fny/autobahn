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

/// A file inside two nested groups scopes `issues` to that file in each
/// group, named relative to each group's own root, rather than to nothing
/// (the whole of both groups) because the two names differ.
#[test]
fn issues_for_a_path_in_nested_groups_scopes_each_group_to_it() {
    use autobahn::supervisor::{ConflictDetail, SessionStatus};

    let home = tempfile::tempdir().expect("a temporary directory");
    let outer = home.path().join("outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join("file.txt"), "content").unwrap();
    let config = home.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[groups.outer]\nmode = \"one-way-alpha\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n\n\
             [groups.inner]\nmode = \"one-way-alpha\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
            outer.display(),
            home.path().join("b1").display(),
            inner.display(),
            home.path().join("b2").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let state_root = home.path().join("state");
    let plans = autobahn::config::Config::load(&config)
        .unwrap()
        .plans()
        .unwrap();
    std::fs::create_dir_all(state_root.join("status")).unwrap();
    for plan in &plans {
        let conflicts: Vec<String> = match plan.group.as_str() {
            "outer" => vec!["inner/file.txt".into(), "outer-only.txt".into()],
            _ => vec!["file.txt".into(), "inner-only.txt".into()],
        };
        let status = SessionStatus {
            group: plan.group.clone(),
            state: "conflicts".into(),
            cycles: 1,
            conflict_details: conflicts
                .iter()
                .map(|path| ConflictDetail {
                    path: path.clone(),
                    alpha: Default::default(),
                    beta: Default::default(),
                })
                .collect(),
            conflicts,
            ..Default::default()
        };
        std::fs::write(
            state_root
                .join("status")
                .join(format!("{}.json", plan.identifier())),
            serde_json::to_vec(&status).unwrap(),
        )
        .unwrap();
    }

    let issues = |json: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_autobahn"));
        command
            .arg("issues")
            .arg("--config")
            .arg(&config)
            .arg("--state-root")
            .arg(&state_root)
            .arg(inner.join("file.txt"))
            .env("HOME", home.path())
            .env("AUTOBAHN_HOME", home.path().join(".autobahn"));
        if json {
            command.arg("--json");
        }
        let output = command.output().expect("runs autobahn");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    };
    let text = issues(false);
    assert!(text.contains("inner/file.txt"), "{text}");
    assert!(!text.contains("outer-only.txt"), "{text}");
    assert!(!text.contains("inner-only.txt"), "{text}");

    let json = issues(true);
    let report: serde_json::Value = serde_json::from_str(&json).unwrap();
    let mut shown: Vec<(String, String)> = Vec::new();
    for group in report["groups"].as_array().unwrap() {
        for session in group["sessions"].as_array().unwrap() {
            for conflict in session["conflicts"].as_array().unwrap() {
                shown.push((
                    group["name"].as_str().unwrap().to_owned(),
                    conflict["path"].as_str().unwrap().to_owned(),
                ));
            }
        }
    }
    shown.sort();
    assert_eq!(
        shown,
        [
            ("inner".to_owned(), "file.txt".to_owned()),
            ("outer".to_owned(), "inner/file.txt".to_owned()),
        ]
    );
}

/// A configuration's warnings are said once by `status`, however many
/// times it plans the sessions: the credentials warning on standard
/// output, where the rest of the page is, and the ineffective negation
/// with it.
#[test]
fn status_says_each_configuration_warning_once() {
    let home = tempfile::tempdir().expect("a temporary directory");
    let alpha = home.path().join("alpha");
    let beta = home.path().join("beta");
    std::fs::create_dir_all(alpha.join(".ssh")).unwrap();
    std::fs::create_dir_all(&beta).unwrap();
    let config = home.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[groups.g]\nmode = \"two-way-safe\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n\
             ignores = [\"vendor\", \"!vendor/*.patch\"]\n",
            alpha.display(),
            beta.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_autobahn"))
        .arg("status")
        .arg("--config")
        .arg(&config)
        .arg("--state-root")
        .arg(home.path().join("state"))
        .env("HOME", home.path())
        .env("AUTOBAHN_HOME", home.path().join(".autobahn"))
        .output()
        .expect("runs autobahn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert_eq!(stdout.matches(".ssh").count(), 1, "{stdout}\n{stderr}");
    assert!(stdout.contains("acknowledge_secrets"), "{stdout}");
    let both = format!("{stdout}{stderr}");
    assert_eq!(
        both.matches("!vendor/*.patch has no effect").count(),
        1,
        "{both}"
    );
}
