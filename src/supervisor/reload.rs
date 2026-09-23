//! The configuration, re-read while the supervisor runs.
//!
//! An edit to the configuration used to land on `restart`, and a broken
//! edit landed as a supervisor that exited into the service log a moment
//! after the restart was reported done. Here the running supervisor reads
//! the file itself: an edit that would start is applied in place, and one
//! that would not is refused with the same message `start` gives — kept
//! where `status`, `mi` and the tray can show it, and alerted like any
//! other condition that needs a person — while the sessions carry on under
//! the configuration they had.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{Config, SessionPlan};

/// How often the configuration file is read. A read of a small file; the
/// settle below is what bounds how quickly an edit is acted on.
pub const CONFIG_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// What the supervisor needs from a configuration, validated the way
/// `start` validates it: everything here would have stopped the
/// supervisor at startup, so it is checked before anything is replaced.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub plans: Vec<SessionPlan>,
    pub alerts: crate::alerts::AlertPlan,
    pub log_level: Option<crate::logging::Level>,
    /// Whether the configuration asks to be watched at all.
    pub reload: bool,
}

/// Loads a configuration and derives everything the supervisor runs from,
/// refusing exactly what the supervisor would refuse at startup.
pub fn load(path: &Path) -> Result<Loaded> {
    let configuration = Config::load(path)?;
    let plans = configuration.plans()?;
    if plans.is_empty() {
        anyhow::bail!("the configuration describes no sessions");
    }
    let alerts = configuration.alert_plan()?;
    let log_level = configuration.log_level()?;
    Ok(Loaded {
        plans,
        alerts,
        log_level,
        reload: configuration.reload,
    })
}

/// A refused configuration: what was wrong with it, and when.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Notice {
    /// Seconds since the Unix epoch when the edit was refused.
    pub at: u64,
    /// The message `start` would have printed.
    pub message: String,
}

/// Where a refusal is kept between the supervisor that wrote it and the
/// commands that show it.
fn notice_path(state_root: &Path) -> PathBuf {
    state_root.join("config-notice.json")
}

/// The standing refusal, if the running supervisor recorded one.
pub fn read_notice(state_root: &Path) -> Option<Notice> {
    let text = std::fs::read_to_string(notice_path(state_root)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_notice(state_root: &Path, notice: &Notice) -> Result<()> {
    let path = notice_path(state_root);
    let text = serde_json::to_string(notice).context("unable to encode the notice")?;
    let staged = path.with_extension("json.tmp");
    std::fs::write(&staged, text)
        .with_context(|| format!("unable to write {}", staged.display()))?;
    std::fs::rename(&staged, &path).with_context(|| format!("unable to publish {}", path.display()))
}

/// Forgets a refusal: the configuration is good again, or a supervisor
/// just started from one that passed.
pub fn clear_notice(state_root: &Path) {
    let _ = std::fs::remove_file(notice_path(state_root));
}

/// The watch over one configuration file, shared between the supervisor
/// that reads it and the caller that rebuilds from what it read.
pub struct Reloader {
    path: PathBuf,
    interval: Duration,
    /// What the last edit loaded to, waiting for the caller to run it.
    pending: Mutex<Option<Loaded>>,
    /// The bytes the running configuration came from — or the last edit
    /// refused, which has been heard about. Kept across watches, so an
    /// edit made while no supervisor was watching (the alpha attached to
    /// a beta that led) is found by the next one.
    applied: Mutex<Option<Vec<u8>>>,
}

impl Reloader {
    pub fn new(path: PathBuf) -> Reloader {
        let applied = std::fs::read(&path).ok();
        Reloader {
            path,
            interval: CONFIG_CHECK_INTERVAL,
            pending: Mutex::default(),
            applied: Mutex::new(applied),
        }
    }

    /// Reads the file this often instead of every two seconds; for tests
    /// that cannot wait that long.
    pub fn with_interval(mut self, interval: Duration) -> Reloader {
        self.interval = interval;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether an edit has loaded and waits to be run.
    pub fn is_pending(&self) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }

    /// The configuration a valid edit loaded to, once — the caller runs it
    /// and the slot is empty again.
    pub fn take(&self) -> Option<Loaded> {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    fn applied(&self) -> Option<Vec<u8>> {
        self.applied
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn set_applied(&self, bytes: Vec<u8>) {
        *self
            .applied
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(bytes);
    }

    /// Watches the file until `stop`, or until an edit loads: then the
    /// new configuration is left in `take` and `halt` is raised so the
    /// supervisor winds its workers down. An edit that does not load is
    /// recorded and reported, and the watch goes on.
    ///
    /// The bytes are what is compared, not the mtime: an editor that saves
    /// twice in a second and a `touch` both leave the mtime a poor
    /// witness. An edit counts once it has read the same twice in a row,
    /// so a file caught half-written is read again rather than refused.
    pub(super) fn watch(
        &self,
        state_root: &Path,
        alerts: &crate::alerts::AlertPlan,
        stop: &AtomicBool,
        halt: &AtomicBool,
    ) {
        let mut seen: Option<Vec<u8>> = None;
        while !stop.load(Ordering::Relaxed) {
            super::sleep_interruptible(self.interval, stop);
            if stop.load(Ordering::Relaxed) {
                return;
            }
            // A file that cannot be read — mid-rename, or gone — is left
            // alone: the sessions run on, and the next read decides.
            let Ok(current) = std::fs::read(&self.path) else {
                continue;
            };
            if self.applied().as_ref() == Some(&current) {
                seen = None;
                continue;
            }
            if seen.as_ref() != Some(&current) {
                seen = Some(current);
                continue;
            }
            match load(&self.path) {
                Ok(loaded) => {
                    crate::note!("configuration reloaded from {}", self.path.display());
                    self.set_applied(current);
                    clear_notice(state_root);
                    *self
                        .pending
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = Some(loaded);
                    halt.store(true, Ordering::Relaxed);
                    return;
                }
                Err(error) => {
                    // Heard once: the same bytes are read again every few
                    // seconds until the next edit.
                    self.set_applied(current);
                    let message = format!("{error:#}");
                    crate::complain!(
                        "configuration refused; the sessions keep running as before: {message}"
                    );
                    let notice = Notice {
                        at: super::epoch_seconds(),
                        message,
                    };
                    if let Err(error) = write_notice(state_root, &notice) {
                        crate::complain!("unable to record the refusal: {error:#}");
                    }
                    alert(alerts, state_root, &notice);
                }
            }
        }
    }
}

/// Runs the alert hook for a refused edit. The alerter proper watches
/// sessions and confirms over time; a refusal is one event from one file,
/// so it goes straight to the hook, with the environment the hook already
/// reads.
fn alert(plan: &crate::alerts::AlertPlan, state_root: &Path, notice: &Notice) {
    if !plan.is_configured() {
        return;
    }
    let summary = "the configuration was refused".to_owned();
    let environment = vec![
        ("AUTOBAHN_SUMMARY".to_owned(), summary),
        (
            "AUTOBAHN_DETAIL".to_owned(),
            format!("  {}", notice.message),
        ),
        ("AUTOBAHN_ALERT_COUNT".to_owned(), "1".to_owned()),
        ("AUTOBAHN_STATES".to_owned(), "config".to_owned()),
        (
            "AUTOBAHN_ICON".to_owned(),
            crate::icon::ensure(state_root)
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        ),
        ("AUTOBAHN_EVENT".to_owned(), "config".to_owned()),
    ];
    let document = serde_json::to_string(notice).unwrap_or_default();
    crate::alerts::Dispatcher::default().dispatch(
        plan.on_alert.iter().cloned().collect(),
        environment,
        document,
        plan.timeout,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration(alpha: &Path, beta: &Path) -> String {
        format!(
            "[groups.work]\nmode = \"two-way-safe\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
            alpha.display(),
            beta.display()
        )
    }

    #[test]
    fn a_notice_round_trips_and_clears() {
        let root = tempfile::tempdir().expect("a temporary directory");
        assert_eq!(read_notice(root.path()), None);
        let notice = Notice {
            at: 7,
            message: "unable to parse configuration: unknown field `mdoe`".into(),
        };
        write_notice(root.path(), &notice).expect("the notice is written");
        assert_eq!(read_notice(root.path()), Some(notice));
        clear_notice(root.path());
        assert_eq!(read_notice(root.path()), None);
        // Clearing what is not there is not an error.
        clear_notice(root.path());
    }

    #[test]
    fn load_refuses_what_start_refuses() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let path = root.path().join("config.toml");
        std::fs::write(&path, "").expect("written");
        let error = load(&path).expect_err("no sessions is refused");
        assert!(format!("{error:#}").contains("no sessions"), "{error:#}");
        std::fs::write(&path, "reload = true\nmdoe = \"two-way-safe\"\n").expect("written");
        let error = load(&path).expect_err("an unknown key is refused");
        assert!(format!("{error:#}").contains("mdoe"), "{error:#}");
        let alpha = root.path().join("alpha");
        let beta = root.path().join("beta");
        std::fs::create_dir_all(&alpha).expect("created");
        std::fs::write(&path, configuration(&alpha, &beta)).expect("written");
        let loaded = load(&path).expect("a plain configuration loads");
        assert_eq!(loaded.plans.len(), 1);
        assert!(loaded.reload, "the watch is on unless said otherwise");
        std::fs::write(
            &path,
            format!("reload = false\n{}", configuration(&alpha, &beta)),
        )
        .expect("written");
        assert!(!load(&path).expect("loads").reload);
    }

    /// Runs one watch on its own thread until it returns or the deadline
    /// passes, returning whether it raised `halt`.
    fn watched(
        reloader: &Reloader,
        state_root: &Path,
        deadline: Duration,
        during: impl FnOnce(),
    ) -> bool {
        let stop = AtomicBool::new(false);
        let halt = AtomicBool::new(false);
        let alerts = crate::alerts::AlertPlan::default();
        std::thread::scope(|scope| {
            let watcher = scope.spawn(|| reloader.watch(state_root, &alerts, &stop, &halt));
            during();
            let started = std::time::Instant::now();
            while !watcher.is_finished() && started.elapsed() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            stop.store(true, Ordering::Relaxed);
            watcher.join().expect("the watch returns");
        });
        halt.load(Ordering::Relaxed)
    }

    #[test]
    fn a_broken_edit_is_refused_and_a_good_one_loads() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let state_root = root.path().join("state");
        std::fs::create_dir_all(&state_root).expect("created");
        let alpha = root.path().join("alpha");
        let beta = root.path().join("beta");
        std::fs::create_dir_all(&alpha).expect("created");
        let path = root.path().join("config.toml");
        std::fs::write(&path, configuration(&alpha, &beta)).expect("written");
        let reloader = Reloader::new(path.clone()).with_interval(Duration::from_millis(10));

        // Untouched, the watch stands: nothing loads, nothing is refused.
        let halted = watched(&reloader, &state_root, Duration::from_millis(100), || {});
        assert!(!halted);
        assert!(!reloader.is_pending());
        assert_eq!(read_notice(&state_root), None);

        // Broken: refused, recorded, and the watch stands.
        let halted = watched(&reloader, &state_root, Duration::from_millis(300), || {
            std::fs::write(&path, "[groups.work]\nmdoe = 1\n").expect("written");
        });
        assert!(!halted, "a refused edit does not halt the workers");
        assert!(!reloader.is_pending());
        let notice = read_notice(&state_root).expect("the refusal is recorded");
        assert!(notice.message.contains("mdoe"), "{}", notice.message);
        assert!(notice.at > 0);

        // Fixed, with a second group: loads, and the workers are halted
        // for the caller to run it.
        let other = root.path().join("other");
        std::fs::create_dir_all(&other).expect("created");
        let halted = watched(&reloader, &state_root, Duration::from_secs(5), || {
            std::fs::write(
                &path,
                format!(
                    "{}[groups.notes]\nmode = \"two-way-safe\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
                    configuration(&alpha, &beta),
                    other.display(),
                    root.path().join("other-mirror").display()
                ),
            )
            .expect("written");
        });
        assert!(halted, "a loaded edit halts the workers");
        assert_eq!(read_notice(&state_root), None, "the refusal is over");
        let loaded = reloader.take().expect("the edit is pending");
        assert_eq!(loaded.plans.len(), 2);
        assert!(reloader.take().is_none(), "taken once");

        // The same bytes again are the running configuration, not an edit.
        let halted = watched(&reloader, &state_root, Duration::from_millis(100), || {});
        assert!(!halted);
    }

    #[test]
    fn an_edit_made_between_watches_is_found_by_the_next() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let state_root = root.path().join("state");
        std::fs::create_dir_all(&state_root).expect("created");
        let alpha = root.path().join("alpha");
        std::fs::create_dir_all(&alpha).expect("created");
        let path = root.path().join("config.toml");
        std::fs::write(&path, configuration(&alpha, &root.path().join("beta"))).expect("written");
        let reloader = Reloader::new(path.clone()).with_interval(Duration::from_millis(10));
        // No watch stands while the alpha follows a beta that leads; the
        // edit is still an edit when it leads again.
        std::fs::write(&path, configuration(&alpha, &root.path().join("elsewhere")))
            .expect("written");
        let halted = watched(&reloader, &state_root, Duration::from_secs(5), || {});
        assert!(halted);
        assert!(reloader.take().is_some());
    }
}
