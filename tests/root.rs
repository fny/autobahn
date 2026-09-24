//! Running as root, where the tests themselves run as root (a CI
//! container). Elsewhere these return at once: the checks are also unit
//! tested with the identity passed in, in `autobahn::root`.

use std::process::Command;

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[test]
fn as_root_watch_is_refused_without_the_override() {
    if euid() != 0 {
        return;
    }
    let home = tempfile::tempdir().expect("a temporary directory");
    let run = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_autobahn"))
            .args(arguments)
            .env("HOME", home.path())
            .env("AUTOBAHN_HOME", home.path().join(".autobahn"))
            .output()
            .expect("runs autobahn")
    };
    let output = run(&["watch"]);
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(complaint.contains("refusing to run as root"), "{complaint}");

    // With the override it gets past the check, to the missing
    // configuration.
    let output = run(&["watch", "--allow-root"]);
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(
        !complaint.contains("refusing to run as root"),
        "{complaint}"
    );
}
