//! `scripts/install.sh` against a fake release served on localhost.
//!
//! The installer is the first code a new machine runs, and it runs it
//! with no autobahn to check anything. So these tests hold it to what
//! the updater already does: every asset verified against `SHA256SUMS`
//! before anything is installed, and a checksum file that cannot be had
//! is a refusal, not a warning.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

/// A release directory served over HTTP: a file is a `200`, a name in
/// `failing` is a `500`, and anything else is a `404`.
struct Release {
    root: tempfile::TempDir,
    base: String,
}

impl Release {
    fn serve(failing: &[&str]) -> Self {
        let root = tempfile::tempdir().expect("a temporary directory");
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        let directory = root.path().to_path_buf();
        let failing: Arc<HashSet<String>> =
            Arc::new(failing.iter().map(|name| (*name).to_owned()).collect());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().expect("clones"));
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                // The rest of the headers, unread, would reset the
                // connection under the response.
                let mut line = String::new();
                while reader.read_line(&mut line).map(|n| n > 2).unwrap_or(false) {
                    line.clear();
                }
                let name = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .trim_start_matches('/')
                    .to_owned();
                let (status, body) = if failing.contains(&name) {
                    ("500 Internal Server Error", b"broken".to_vec())
                } else {
                    match std::fs::read(directory.join(&name)) {
                        Ok(body) if !name.is_empty() => ("200 OK", body),
                        _ => ("404 Not Found", b"Not Found".to_vec()),
                    }
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        Self { root, base }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    /// Publishes a working binary and a bundle, with checksums for both.
    fn publish(&self) {
        std::fs::write(self.path().join(binary_asset()), FAKE_BINARY).unwrap();
        let bundle = tempfile::tempdir().unwrap();
        let agents = bundle.path().join("agents");
        std::fs::create_dir(&agents).unwrap();
        std::fs::write(agents.join("autobahn-linux-x86_64"), b"agent one").unwrap();
        std::fs::write(agents.join("autobahn-darwin-aarch64"), b"agent two").unwrap();
        tar(bundle.path(), &self.path().join(BUNDLE), &["agents"]);
        self.write_sums();
    }

    /// Writes `SHA256SUMS` over what the directory holds now.
    fn write_sums(&self) {
        let mut sums = String::new();
        for name in [binary_asset().as_str(), BUNDLE] {
            let path = self.path().join(name);
            if path.exists() {
                sums.push_str(&format!("{}  {name}\n", sha256(&path)));
            }
        }
        std::fs::write(self.path().join("SHA256SUMS"), sums).unwrap();
    }
}

const BUNDLE: &str = "autobahn-agents.tar.gz";

/// A "binary" that runs anywhere and answers `--version`.
const FAKE_BINARY: &[u8] = b"#!/bin/sh\necho 'autobahn 9.9.9'\n";

fn binary_asset() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("autobahn-{os}-{}", std::env::consts::ARCH)
}

fn tar(directory: &Path, archive: &Path, members: &[&str]) {
    let status = Command::new("tar")
        .arg("czf")
        .arg(archive)
        .arg("-C")
        .arg(directory)
        .args(members)
        .status()
        .expect("runs tar");
    assert!(status.success());
}

fn sha256(path: &Path) -> String {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .or_else(|_| {
            Command::new("shasum")
                .args(["-a", "256"])
                .arg(path)
                .output()
        })
        .expect("a checksum tool");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .expect("a digest")
        .to_owned()
}

/// Where one run of the installer puts things.
struct Machine {
    _root: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("a temporary directory");
        let home = root.path().join("home");
        let bin = root.path().join("bin");
        std::fs::create_dir(&home).unwrap();
        Self {
            _root: root,
            home,
            bin,
        }
    }

    fn state(&self) -> PathBuf {
        self.home.join(".autobahn")
    }

    fn install(&self, release: &Release, arguments: &[&str]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh");
        Command::new("sh")
            .arg(script)
            .args(arguments)
            .env("HOME", &self.home)
            .env("AUTOBAHN_HOME", self.state())
            .env("AUTOBAHN_BIN_DIR", &self.bin)
            .env("AUTOBAHN_RELEASE_BASE", &release.base)
            .env_remove("AUTOBAHN_INSECURE")
            .env_remove("AUTOBAHN_PREFIX")
            .output()
            .expect("runs the installer")
    }

    fn installed(&self) -> bool {
        self.bin.join("autobahn").exists()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The environment these tests need. A machine without one of these tools
/// cannot run the installer either, so there is nothing to test there.
fn tools_present() -> bool {
    ["sh", "tar", "curl"].iter().all(|tool| {
        Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    })
}

#[test]
fn a_release_that_matches_its_checksums_installs() {
    if !tools_present() {
        return;
    }
    let release = Release::serve(&[]);
    release.publish();
    let machine = Machine::new();
    let output = machine.install(&release, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        std::fs::read(machine.bin.join("autobahn")).unwrap(),
        FAKE_BINARY
    );
    assert_eq!(
        std::fs::read(machine.state().join("agents/autobahn-linux-x86_64")).unwrap(),
        b"agent one"
    );
    assert!(stdout(&output).contains("verified"), "{}", stdout(&output));
}

/// The bundle is what the controller uploads to and runs on every remote
/// host. It is verified like the binary, and a mismatch installs nothing
/// at all, not the binary alone.
#[test]
fn a_tampered_bundle_is_refused_and_nothing_is_installed() {
    if !tools_present() {
        return;
    }
    let release = Release::serve(&[]);
    release.publish();
    let bundle = release.path().join(BUNDLE);
    let mut bytes = std::fs::read(&bundle).unwrap();
    bytes.extend_from_slice(b"tampered");
    std::fs::write(&bundle, bytes).unwrap();

    let machine = Machine::new();
    let output = machine.install(&release, &[]);
    assert!(!output.status.success(), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("checksum mismatch for autobahn-agents.tar.gz"),
        "{}",
        stderr(&output)
    );
    assert!(!machine.installed(), "the binary was installed anyway");
    assert!(!machine.state().join("agents").exists());
}

/// A checksum file that fails to download is not a release without one.
/// Blocking one file must not be enough to install unverified bytes.
#[test]
fn a_checksum_file_that_fails_to_download_refuses() {
    if !tools_present() {
        return;
    }
    let release = Release::serve(&["SHA256SUMS"]);
    release.publish();
    let machine = Machine::new();
    let output = machine.install(&release, &[]);
    assert!(!output.status.success(), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("unable to download SHA256SUMS"),
        "{}",
        stderr(&output)
    );
    assert!(!machine.installed());
}

/// An old release that publishes no checksums installs only when asked
/// to, by flag or by environment, and says loudly what it skipped.
#[test]
fn a_release_without_checksums_installs_only_when_insecure() {
    if !tools_present() {
        return;
    }
    let release = Release::serve(&[]);
    release.publish();
    std::fs::remove_file(release.path().join("SHA256SUMS")).unwrap();

    let machine = Machine::new();
    let output = machine.install(&release, &[]);
    assert!(!output.status.success(), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("publishes no SHA256SUMS"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("--insecure"),
        "{}",
        stderr(&output)
    );
    assert!(!machine.installed());

    let output = machine.install(&release, &["--insecure"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(machine.installed());
    assert!(
        stderr(&output).contains("UNVERIFIED"),
        "{}",
        stderr(&output)
    );

    let again = Machine::new();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh");
    let output = Command::new("sh")
        .arg(script)
        .env("HOME", &again.home)
        .env("AUTOBAHN_HOME", again.state())
        .env("AUTOBAHN_BIN_DIR", &again.bin)
        .env("AUTOBAHN_RELEASE_BASE", &release.base)
        .env("AUTOBAHN_INSECURE", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(again.installed());
}

/// `--insecure` excuses a missing checksum file, never a wrong checksum.
#[test]
fn insecure_does_not_excuse_a_mismatch() {
    if !tools_present() {
        return;
    }
    let release = Release::serve(&[]);
    release.publish();
    std::fs::write(
        release.path().join(binary_asset()),
        b"#!/bin/sh\necho evil\n",
    )
    .unwrap();
    let machine = Machine::new();
    let output = machine.install(&release, &["--insecure"]);
    assert!(!output.status.success(), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("checksum mismatch"),
        "{}",
        stderr(&output)
    );
    assert!(!machine.installed());
}

/// A bundle member that would land outside the extraction directory is
/// refused before `tar` runs, whatever this machine's `tar` would do.
#[test]
fn a_bundle_member_that_escapes_is_refused() {
    if !tools_present() {
        return;
    }
    let release = Release::serve(&[]);
    release.publish();
    let bundle = tempfile::tempdir().unwrap();
    let agents = bundle.path().join("agents");
    std::fs::create_dir(&agents).unwrap();
    std::fs::write(agents.join("autobahn-linux-x86_64"), b"agent").unwrap();
    // `agents/../escaped`, written by name so no tar strips it.
    std::fs::write(bundle.path().join("escaped"), b"outside").unwrap();
    let status = Command::new("tar")
        .arg("czf")
        .arg(release.path().join(BUNDLE))
        .arg("-C")
        .arg(bundle.path())
        .args(["--transform", "s,^escaped,agents/../escaped,"])
        .args(["agents", "escaped"])
        .status()
        .expect("runs tar");
    if !status.success() {
        // Not GNU tar: no way to write such a member from here.
        return;
    }
    release.write_sums();

    let machine = Machine::new();
    let output = machine.install(&release, &[]);
    assert!(!output.status.success(), "{}", stdout(&output));
    assert!(stderr(&output).contains("outside"), "{}", stderr(&output));
    assert!(!machine.installed());
}

/// The script is published and run by `sh`; shellcheck is the linter it
/// has, where the machine has it.
#[test]
fn the_installer_passes_shellcheck() {
    let Ok(output) = Command::new("shellcheck")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh"))
        .output()
    else {
        return;
    };
    assert!(output.status.success(), "{}", stdout(&output));
}
