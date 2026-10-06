//! State roots to open the app against, so every view can be looked at
//! without touching this machine.
//!
//! The app reads almost everything it draws out of its state root: the
//! status files under `status/`, the log beside them, and the socket it
//! probes to decide whether a supervisor is up. Point it at a directory
//! built here and the panes fill with a fleet that does not exist.
//!
//! Two things are not in the state root, and each gets an environment
//! variable the library honours: whether `autobahn` is installed
//! (`AUTOBAHN_BIN`) and what the login service is doing
//! (`AUTOBAHN_SERVICE_STATE`). Each fixture writes an `env` file holding
//! its own answers, which `scripts/views.sh` reads before launching.
//!
//!     cargo run --example fixtures -- <directory>
//!     cargo run --example fixtures -- --list

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use autobahn::config::Config;
use autobahn::supervisor::{ConflictDetail, ConflictSide, SessionStatus, Unsynchronizable};

/// One fixture: a configuration, a status per session, and the two
/// answers that do not live in a state root.
struct Fixture {
    name: &'static str,
    /// What it is for, printed by `--list` and written into the fixture.
    about: &'static str,
    config: &'static str,
    /// Whether the machine has an `autobahn` to talk to. False is the
    /// welcome pane, which is otherwise unreachable here.
    installed: bool,
    service: &'static str,
    log: &'static str,
    /// Status per `group@host`, in the shape `plans()` will name them.
    /// A group with no entry here has never run, which is its own state.
    statuses: fn(now: u64) -> BTreeMap<&'static str, SessionStatus>,
}

fn main() -> Result<()> {
    let mut out: Option<String> = None;
    let mut keys = false;
    let mut serve = false;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--list" => {
                for fixture in FIXTURES {
                    println!("{:<10} {}", fixture.name, fixture.about);
                }
                return Ok(());
            }
            // The names a status has to be filed under. An identifier is
            // derived, so adding a session below means asking what it is
            // called rather than working it out. It rides on a real
            // generation because a `file:` entry is only resolvable once
            // the fixture's own ignore directory is on disk.
            "--keys" => keys = true,
            // Stand in for a supervisor against one fixture, until
            // killed. The footer reads the control socket, not a file,
            // so this is the only way to photograph the app with one
            // running.
            "--serve" => serve = true,
            other => out = Some(other.to_owned()),
        }
    }
    // Absolute, because the `env` file names paths and whoever reads it
    // is not standing where this ran.
    let out = PathBuf::from(out.unwrap_or_else(|| "target/views".to_owned()));
    let out = match out.is_absolute() {
        true => out,
        false => std::env::current_dir()
            .context("unable to say where this is running")?
            .join(out),
    };
    if serve {
        return supervise(&out);
    }
    // Built fresh every time: a fixture half from this build and half
    // from the last one is a bug that looks like a drawing bug.
    if out.exists() {
        std::fs::remove_dir_all(&out).context("unable to clear the fixture directory")?;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    for fixture in FIXTURES {
        let at = out.join(fixture.name);
        let filed = write(fixture, &at, now)?;
        match keys {
            true => {
                for key in filed {
                    println!("{:<10} {key}", fixture.name);
                }
            }
            false => println!("{}", at.display()),
        }
    }
    Ok(())
}

/// Builds one fixture on disk.
fn write(fixture: &Fixture, at: &Path, now: u64) -> Result<Vec<String>> {
    // A whole home, not a directory of parts. Every path the app draws
    // goes through `tilde`, which shortens against HOME — so a fixture
    // that keeps its command in `.local/bin` and its state in
    // `.autobahn` is photographed saying `~/.local/bin/autobahn`, the
    // way an installed one would, instead of naming a checkout.
    let home = at.join("home");
    let state = home.join(".autobahn");
    std::fs::create_dir_all(state.join("status")).context("unable to make the state root")?;
    std::fs::create_dir_all(state.join("ignores")).context("unable to make the ignores")?;
    let config = state.join("config.toml");
    std::fs::write(&config, fixture.config).context("unable to write the configuration")?;
    // A `file:` entry is read from the state root's `ignores`, not from
    // beside the configuration, and the configuration is refused without
    // it. Writing it here means the fixture exercises that path instead
    // of stepping around it — and, because the state root is this
    // fixture's, it never reads the real one.
    std::fs::write(
        state.join("ignores").join("Essential.gitignore"),
        autobahn::config::ESSENTIAL_IGNORES,
    )
    .context("unable to write the essential ignores")?;
    // `init` writes this too, and the configuration names it. Without
    // it the loader is right to complain, and the complaint is the
    // first thing the configuration pane draws.
    let hook = state.join("on-alert.sh");
    std::fs::write(&hook, autobahn::config::ON_ALERT_EXAMPLE)
        .context("unable to write the example hook")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
            .context("unable to make the hook runnable")?;
    }
    // Both roots, before a plan is made from the configuration. A
    // session's identifier is derived from its roots, and a root
    // written `~/Workspace` is expanded against HOME — so a status
    // filed under this machine's home is a status the app, running
    // under the fixture's, will never find.
    std::env::set_var("HOME", &home);
    std::env::set_var("AUTOBAHN_HOME", &state);
    std::fs::write(state.join("service.log"), fixture.log).context("unable to write the log")?;

    // The names the app will look for. Asking the library rather than
    // guessing: an identifier is derived, and a fixture whose filenames
    // are a guess is a fixture that silently shows nothing.
    let loaded = Config::load(&config)
        .with_context(|| format!("the {} fixture's configuration does not load", fixture.name))?;
    let plans = loaded.plans().with_context(|| {
        format!(
            "the {} fixture's configuration makes no plans",
            fixture.name
        )
    })?;
    let mut wanted = (fixture.statuses)(now);
    let mut filed = Vec::new();
    for plan in &plans {
        let key = format!("{}@{}", plan.group, plan.host);
        filed.push(key.clone());
        let Some(mut status) = wanted.remove(key.as_str()) else {
            continue;
        };
        status.group = plan.group.clone();
        status.host = plan.host.clone();
        status.replica = plan.replica_spec();
        status.mode = plan.mode_name().to_owned();
        status.updated_at = now.saturating_sub(3);
        let path = state
            .join("status")
            .join(format!("{}.json", plan.identifier()));
        let text = serde_json::to_vec_pretty(&status).context("unable to encode a status")?;
        std::fs::write(&path, text)
            .with_context(|| format!("unable to write {}", path.display()))?;
    }
    anyhow::ensure!(
        wanted.is_empty(),
        "the {} fixture has statuses for sessions its configuration does not describe: {:?}",
        fixture.name,
        wanted.keys().collect::<Vec<_>>(),
    );

    // A shim rather than the real command: a fixture that could run
    // `autobahn clean` against a made-up state root is a fixture that
    // can do damage. This one says what it was asked and stops.
    let shim = home.join(".local").join("bin").join("autobahn");
    if fixture.installed {
        std::fs::create_dir_all(shim.parent().expect("bin has a parent"))
            .context("unable to make the shim directory")?;
        // The shim is this build's version, or the service pane would
        // advise updating a command that is only a stand-in.
        std::fs::write(&shim, SHIM.replace("VERSION", env!("CARGO_PKG_VERSION")))
            .context("unable to write the shim")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
                .context("unable to make the shim runnable")?;
        }
    }

    // What `views.sh` exports. An uninstalled fixture points at a path
    // that is not there, which is the answer "no command" rather than
    // "carry on looking in the usual places".
    std::fs::write(
        at.join("env"),
        format!(
            "HOME={}\nAUTOBAHN_HOME={}\nAUTOBAHN_BIN={}\nAUTOBAHN_SERVICE_STATE={}\n",
            home.display(),
            state.display(),
            shim.display(),
            fixture.service,
        ),
    )
    .context("unable to write the fixture's environment")?;
    Ok(filed)
}

/// Stands in for the command, for the buttons that shell out.
const SHIM: &str = r#"#!/bin/sh
# A fixture's autobahn. It runs nothing and changes nothing; it exists so
# the app believes the command is installed, and answers plausibly when a
# button shells out to it.
case "$1" in
  --version) echo "autobahn VERSION" ;;
  status)    echo "3 sessions, 1 needs you" ;;
  clean)     echo "nothing to clean" ;;
  resolve)   echo "resolved (fixture: nothing moved)" ;;
  # `diff <group> <path>`, as the window runs it. The real one shells
  # out to `diff -u` with the two sides as labels; this prints what
  # that would, so the pane is photographed with a diff in it.
  diff)
    cat <<DIFF
--- primary/$3
+++ laptop.bmw.de/$3
@@ -14,9 +14,9 @@
     /// How long to wait before giving up on a host.
-    pub timeout: Duration,
+    pub timeout: Option<Duration>,
     /// Where the agent bundle is kept.
     pub bundle: PathBuf,
-    /// Whether to follow symbolic links out of the root.
-    pub follow_links: bool,
+    /// Whether to follow symbolic links out of the root. Off by
+    /// default: a link out of the root is not part of the root.
+    pub follow_links: Option<bool>,
 }
DIFF
    ;;
  *)         echo "fixture autobahn: $*" ;;
esac
"#;

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "fresh",
        about: "nothing installed — the welcome splash",
        installed: false,
        service: "not-installed",
        config: CONFIG_EMPTY,
        log: "",
        statuses: |_| BTreeMap::new(),
    },
    Fixture {
        name: "quiet",
        about: "installed, configured, never run",
        installed: true,
        service: "stopped",
        config: CONFIG_THREE,
        log: LOG_QUIET,
        statuses: |_| BTreeMap::new(),
    },
    Fixture {
        name: "calm",
        about: "everything synchronized",
        installed: true,
        service: "running",
        config: CONFIG_THREE,
        log: LOG_CALM,
        statuses: calm,
    },
    Fixture {
        name: "trouble",
        about: "a conflict, a blocked path, a host away",
        installed: true,
        service: "running",
        config: CONFIG_THREE,
        log: LOG_TROUBLE,
        statuses: trouble,
    },
];

fn calm(_now: u64) -> BTreeMap<&'static str, SessionStatus> {
    let mut all = BTreeMap::new();
    for (key, cycles, entries, files, bytes) in [
        (
            "work@build.audi.de",
            18_204u64,
            61_880u64,
            402_118u64,
            94_221_880_440u64,
        ),
        (
            "work@laptop.bmw.de",
            18_201,
            61_880,
            398_004,
            93_880_112_006,
        ),
        (
            "backup@/Volumes/Backup/Workspace",
            304,
            61_880,
            221_440,
            90_114_002_118,
        ),
        ("notes@laptop.bmw.de", 9_118, 2_044, 14_902, 408_221_118),
    ] {
        all.insert(
            key,
            SessionStatus {
                state: "synchronized".into(),
                cycles,
                primary_entries: entries,
                replica_entries: entries,
                moved_files: files,
                moved_bytes: bytes,
                // The p2p session is the only one with a role and a
                // term, and the pane has to have somewhere to put them.
                role: match key.starts_with("notes@") {
                    true => "leader".into(),
                    false => String::new(),
                },
                term: match key.starts_with("notes@") {
                    true => 7,
                    false => 0,
                },
                ..Default::default()
            },
        );
    }
    all
}

fn trouble(now: u64) -> BTreeMap<&'static str, SessionStatus> {
    let mut all = BTreeMap::new();
    all.insert(
        "work@build.audi.de",
        SessionStatus {
            state: "conflicts".into(),
            cycles: 18_207,
            conflicts: vec!["api/src/config.rs".into(), "web/package.json".into()],
            conflict_details: vec![
                ConflictDetail {
                    path: "api/src/config.rs".into(),
                    primary: ConflictSide {
                        present: true,
                        kind: "file".into(),
                        size: 11_204,
                        mtime_seconds: now.saturating_sub(640) as i64,
                        unsynchronizable: None,
                    },
                    replica: ConflictSide {
                        present: true,
                        kind: "file".into(),
                        size: 10_880,
                        mtime_seconds: now.saturating_sub(98) as i64,
                        unsynchronizable: None,
                    },
                },
                ConflictDetail {
                    path: "web/package.json".into(),
                    primary: ConflictSide {
                        present: true,
                        kind: "file".into(),
                        size: 2_914,
                        mtime_seconds: now.saturating_sub(120) as i64,
                        unsynchronizable: None,
                    },
                    replica: ConflictSide::default(),
                },
            ],
            primary_entries: 61_902,
            replica_entries: 61_898,
            moved_files: 402_440,
            moved_bytes: 94_228_118_004,
            ..Default::default()
        },
    );
    all.insert(
        "work@laptop.bmw.de",
        SessionStatus {
            state: "blocked".into(),
            cycles: 18_206,
            blocked: vec![
                "replica api/.venv/bin/python: broken symlink".into(),
                "primary web/.next/cache: permission denied".into(),
            ],
            conflict_details: vec![ConflictDetail {
                path: "api/data".into(),
                primary: ConflictSide {
                    present: true,
                    kind: "directory".into(),
                    mtime_seconds: now.saturating_sub(40) as i64,
                    ..Default::default()
                },
                replica: ConflictSide {
                    present: true,
                    kind: "directory".into(),
                    mtime_seconds: now.saturating_sub(44) as i64,
                    unsynchronizable: Some(Unsynchronizable {
                        entries: 3,
                        example: "api/data/dev.sqlite-wal".into(),
                        reason: "a database being written".into(),
                    }),
                    ..Default::default()
                },
            }],
            primary_entries: 61_902,
            replica_entries: 61_899,
            moved_files: 398_220,
            moved_bytes: 93_886_004_552,
            ..Default::default()
        },
    );
    all.insert(
        "backup@/Volumes/Backup/Workspace",
        SessionStatus {
            // The disk that was unplugged, which is the ordinary way a
            // one-way group stops: the replica is simply not there.
            state: "halted".into(),
            cycles: 304,
            error: Some("replica /Volumes/Backup/Workspace is not a directory any more".into()),
            alert_after_seconds: Some(900),
            primary_entries: 61_880,
            moved_files: 221_440,
            moved_bytes: 90_114_002_118,
            ..Default::default()
        },
    );
    all.insert(
        "notes@laptop.bmw.de",
        SessionStatus {
            // The other machine took the lead while this one slept, so
            // this side is a follower and the term has moved on.
            state: "synchronized".into(),
            cycles: 9_121,
            role: "follower".into(),
            term: 9,
            primary_entries: 2_046,
            replica_entries: 2_046,
            moved_files: 14_910,
            moved_bytes: 408_440_002,
            ..Default::default()
        },
    );
    all
}

/// A fresh install's configuration: what `autobahn init` writes, with
/// every group still commented out.
const CONFIG_EMPTY: &str = r#"# A machine that has only just run `autobahn init`.
log_level = "normal"

[defaults]
mode = "two-way-conflict"
ignores = ["file:Essential.gitignore"]
interval = 5
"#;

/// A configuration somebody has been living in.
///
/// Not a demonstration of five features in a row: a tidy example teaches
/// nothing, because nobody's file is tidy. This one has a group that is
/// switched off, ignores that name the actual offenders rather than
/// standing in for them, intervals that differ because the folders do,
/// and an alert hook — the accretions a file picks up over a year.
///
/// Every mode is here, and each because its folder wants it. The
/// destinations are car makers because this is a motorway.
const CONFIG_THREE: &str = r#"# Run when a session needs a person.
on_alert = "~/.autobahn/on-alert.sh"
log_level = "normal"

[defaults]
mode = "two-way-conflict"
ignores = ["file:Essential.gitignore"]
interval = 5

# Work, on the box with the cores. Both ends edit it — an agent over
# there, me over here — so a clash is reported and nothing is touched.
[groups.work]
primary = "~/Workspace"
replicas = [
  "dev@build.audi.de:/home/dev/workspace",
  "laptop.bmw.de",
]
ignores = ["target", "node_modules", ".venv", ".next", "*.sqlite"]

# The same folder onto the disk that keeps a copy. One way, and the disk
# is made identical: a backup that can push a deletion back is not one.
# Slower, because nothing is waiting on it.
[groups.backup]
mode = "one-way-primary"
primary = "~/Workspace"
replicas = ["/Volumes/Backup/Workspace"]
interval = 300

# Notes, where either machine may be the one that is awake. The lease a
# p2p leader holds is renewed once a cycle and lasts 30s, so the interval
# cannot go past half of that.
[groups.notes]
mode = "p2p-conflict-dangerously-experimental"
primary = "~/Documents/Notes"
replicas = ["laptop.bmw.de"]
interval = 10

# The photo library onto the NAS. Off since the NAS started refusing
# connections; turn it back on when that is sorted.
[groups.photos]
primary = "~/Pictures/Lightroom"
replicas = ["nas.porsche.de:/volume1/photos"]
disabled = true
"#;

const LOG_QUIET: &str = "\
2026-10-02 09:14:02 info  autobahn 1.0.0 starting
2026-10-02 09:14:02 info  read 4 groups (1 disabled), 4 sessions
2026-10-02 09:14:02 info  no session has run yet
";

const LOG_CALM: &str = "\
2026-10-02 09:14:02 info  autobahn 1.0.0 starting
2026-10-02 09:14:02 info  read 4 groups (1 disabled), 4 sessions
2026-10-02 09:14:03 info  work@build.audi.de scanning primary
2026-10-02 09:14:03 info  work@build.audi.de 61880 entries, 0 changed
2026-10-02 09:14:04 info  work@laptop.bmw.de 61880 entries, 0 changed
2026-10-02 09:14:04 info  notes@laptop.bmw.de leader for term 7
2026-10-02 09:14:04 info  notes@laptop.bmw.de 2044 entries, 0 changed
2026-10-02 09:18:02 info  backup@/Volumes/Backup/Workspace 61880 entries, 0 changed
2026-10-02 09:18:02 info  all sessions synchronized
2026-10-02 09:23:02 info  heartbeat: 4 synchronized
2026-10-02 09:28:02 info  heartbeat: 4 synchronized
";

const LOG_TROUBLE: &str = "\
2026-10-02 09:14:02 info  autobahn 1.0.0 starting
2026-10-02 09:14:02 info  read 4 groups (1 disabled), 4 sessions
2026-10-02 09:14:03 info  work@build.audi.de scanning primary
2026-10-02 09:14:04 warn  work@build.audi.de conflict at api/src/config.rs
2026-10-02 09:14:04 warn  work@build.audi.de conflict at web/package.json
2026-10-02 09:14:05 error work@laptop.bmw.de replica api/.venv/bin/python: broken symlink
2026-10-02 09:14:05 error work@laptop.bmw.de primary web/.next/cache: permission denied
2026-10-02 09:14:05 warn  work@laptop.bmw.de 2 paths could not be carried
2026-10-02 09:14:06 info  notes@laptop.bmw.de lease lost, follower for term 9
2026-10-02 09:18:02 error backup@/Volumes/Backup/Workspace replica is not a directory any more
2026-10-02 09:18:02 error backup@/Volumes/Backup/Workspace halted, will not retry
2026-10-02 09:23:02 warn  heartbeat: 1 conflicts, 1 blocked, 1 halted
";

/// Answers like a supervisor, against one fixture, until killed.
///
/// The app decides whether a supervisor is running by connecting to a
/// socket in the state root and asking — not by reading a file — so a
/// fixture cannot say yes on its own. This says it: enough of the
/// control protocol to answer the two questions the window asks, and
/// nothing else. It runs no sessions and touches no files.
///
/// Both answers matter. `Progress` is what turns the footer green, and
/// `Sessions` is what `shown_plans` filters the configured plans
/// against once a supervisor answers — so a server that answered only
/// the first would light the footer and empty every pane.
fn supervise(at: &Path) -> Result<()> {
    use autobahn::supervisor::control::{
        socket_path, ControlRequest, ControlResponse, Inventory, SessionKey, SessionSummary,
    };

    let home = at.join("home");
    let state = home.join(".autobahn");
    // The same roots the app will use. A `file:` entry resolves against
    // AUTOBAHN_HOME, so without this the configuration is read against
    // the real ~/.autobahn and refused for an ignore file not there.
    std::env::set_var("HOME", &home);
    std::env::set_var("AUTOBAHN_HOME", &state);
    let config = state.join("config.toml");
    let text = std::fs::read_to_string(&config).context("the fixture has no configuration")?;
    let loaded = Config::load(&config).context("the fixture's configuration does not load")?;
    let plans = loaded.plans().context("the configuration makes no plans")?;
    let sessions: Vec<SessionSummary> = plans
        .iter()
        .map(|plan| SessionSummary {
            identifier: SessionKey::of(plan),
            display: format!("{}@{}", plan.group, plan.host),
            mode: plan.mode_name().to_owned(),
            state: "synchronized".to_owned(),
        })
        .collect();

    let path = socket_path(&state);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("unable to make the socket's directory")?;
    }
    // A socket file left by a killed run is not a running server.
    let _ = std::fs::remove_file(&path);
    let listener =
        std::os::unix::net::UnixListener::bind(&path).context("unable to bind the socket")?;
    println!("{}", path.display());

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let Ok(mut reader) = stream.try_clone() else {
            continue;
        };
        let mut writer = stream;
        let Ok(request) =
            autobahn::transport::receive_control_frame::<_, ControlRequest>(&mut reader)
        else {
            continue;
        };
        // Everything arrives wrapped in the sender's build. Answering a
        // different one with `Mismatch` is what the real supervisor
        // does, and the window draws it as another build running.
        let ControlRequest::Versioned { version, request } = request else {
            continue;
        };
        if version != autobahn::protocol::version() {
            let _ = autobahn::transport::send_control_frame(
                &mut writer,
                &ControlResponse::Mismatch {
                    supervisor: autobahn::protocol::version(),
                },
            );
            continue;
        }
        let answer = match autobahn::wire::decode::<ControlRequest>(&request) {
            // Nothing is cycling, so every session is simply waiting:
            // the footer goes green and no pane grows a progress bar.
            Ok(ControlRequest::Progress) => ControlResponse::Progress(Vec::new()),
            Ok(ControlRequest::Sessions) => ControlResponse::Sessions(Inventory {
                sessions: sessions.clone(),
                configuration: Some(text.clone()),
                notice: None,
                logging_failed: false,
            }),
            // The build the app compares against the command's: this
            // one's, so the service pane has nothing to advise.
            Ok(ControlRequest::Build) => ControlResponse::Build(autobahn::protocol::build()),
            // A button was pressed. Say it applied to nothing rather
            // than pretending to have done work.
            Ok(_) => ControlResponse::Applied { sessions: 0 },
            Err(_) => ControlResponse::Error("the fixture did not understand that".to_owned()),
        };
        let _ = autobahn::transport::send_control_frame(&mut writer, &answer);
    }
    Ok(())
}
