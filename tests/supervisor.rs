//! End-to-end tests for the groups configuration and the supervisor.
//!
//! Every test drives the real production path: a configuration file on disk
//! is loaded and turned into session plans, and a [`Supervisor`] runs those
//! sessions — over local endpoints and over real agent subprocesses (the
//! same code path as SSH, minus the network). State roots live in temporary
//! directories, so persistence claims are proven by fresh supervisor
//! instances over the same root.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use autobahn::config::{Config, SessionPlan};
use autobahn::supervisor::{read_status, SessionOutcome, SessionStatus, Supervisor};
mod common;

/// Returns the path of the autobahn binary under test (used as the agent).
fn agent_binary() -> &'static str {
    common::isolate_home();
    env!("CARGO_BIN_EXE_autobahn")
}

/// A temporary world for one test: a directory holding synchronization
/// roots, the configuration file, and the supervisor state root.
struct World {
    keep: TempDir,
}

impl World {
    fn new() -> World {
        common::isolate_home();
        World {
            keep: TempDir::new().expect("temporary directory should be creatable"),
        }
    }

    /// Returns a path within the world, creating it as a directory.
    fn directory(&self, name: &str) -> PathBuf {
        let path = self.keep.path().join(name);
        fs::create_dir_all(&path).expect("directory should be creatable");
        path
    }

    /// Returns a path within the world without creating anything.
    fn path(&self, name: &str) -> PathBuf {
        self.keep.path().join(name)
    }

    /// Returns the supervisor state root for this world.
    fn state_root(&self) -> PathBuf {
        self.keep.path().join("state")
    }

    /// Writes the configuration file and derives its session plans.
    fn plans(&self, configuration: &str) -> Vec<SessionPlan> {
        let path = self.keep.path().join("config.toml");
        fs::write(&path, configuration).expect("configuration should be writable");
        Config::load(&path)
            .expect("configuration should load")
            .plans()
            .expect("plans should derive")
    }

    /// Runs one supervised pass over the provided plans.
    fn run_once(&self, plans: Vec<SessionPlan>) -> Vec<SessionOutcome> {
        Supervisor::new(plans, self.state_root(), false).run_once()
    }

    /// Reads the recorded status for a plan.
    fn status(&self, plan: &SessionPlan) -> Option<SessionStatus> {
        read_status(&self.state_root(), &plan.identifier()).expect("status should be readable")
    }
}

fn write(root: &Path, path: &str, contents: &str) {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("parent should be creatable");
    }
    fs::write(&full, contents).expect("file should be writable");
}

fn read(root: &Path, path: &str) -> String {
    fs::read_to_string(root.join(path)).expect("file should be readable")
}

/// Polls a condition until it holds or the deadline passes.
fn wait_until(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    condition()
}

/// Stops a watch-mode supervisor when dropped, so that a panicking
/// assertion inside a `thread::scope` unwinds past the (otherwise eternal)
/// watcher thread instead of deadlocking the test against it.
struct StopGuard<'a>(&'a AtomicBool);

impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Serializes the tests that must set a process-wide environment variable.
/// The environment is global to the process and the tests run in parallel,
/// so a test that cannot pass a value any other way holds this for as long
/// as the value is set.
static ENVIRONMENT: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Sets an environment variable for as long as it lives, holding
/// [`ENVIRONMENT`], and restores the previous value when dropped — on a
/// panic as well, so a failing test leaks nothing into the next.
struct EnvironmentGuard {
    name: &'static str,
    previous: Option<std::ffi::OsString>,
    _held: std::sync::MutexGuard<'static, ()>,
}

impl EnvironmentGuard {
    fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> EnvironmentGuard {
        // A test that panicked while holding the lock still restored its
        // variable on the way out, so the poison carries no meaning here.
        let held = ENVIRONMENT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        EnvironmentGuard {
            name,
            previous,
            _held: held,
        }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

/// Asserts that every outcome succeeded.
fn assert_all_synchronized(outcomes: &[SessionOutcome]) {
    for outcome in outcomes {
        assert!(
            outcome.result.is_ok(),
            "{} failed: {:?}",
            outcome.display,
            outcome.result
        );
    }
}

#[test]
fn repeated_one_way_edits_keep_synchronizing_through_a_real_agent() {
    // Exercises the unchanged-scan and fold-tracking paths against a real
    // agent subprocess: after each cycle the agent's snapshot is the tree
    // the controller already models, so it reports itself unchanged — and
    // synchronization must still be exactly correct through many rounds.
    let world = World::new();
    let alpha = world.directory("source");
    let beta = world.directory("mirror");
    for index in 0..20 {
        write(
            &alpha,
            &format!("dir{}/file{index}.txt", index % 3),
            "initial",
        );
    }

    let plans = world.plans(&format!(
        r#"
        [groups.churn]
        alpha = "{alpha}"
        mode = "two-way-safe"
        agent_command = "{agent} agent"
        betas = ["remote-host:{beta}"]
        "#,
        alpha = alpha.display(),
        agent = agent_binary(),
        beta = beta.display(),
    ));

    assert_all_synchronized(&world.run_once(plans.clone()));
    assert_eq!(read(&beta, "dir0/file0.txt"), "initial");

    // Round after round of one-directional edits: only alpha changes, so
    // beta's every scan after the first reports itself unchanged.
    // Each round makes exactly one modification and one creation; the
    // created names lie outside the range the fixture already wrote.
    for round in 1..=5 {
        write(&alpha, "dir0/file0.txt", &format!("round {round}"));
        write(
            &alpha,
            &format!("dir1/added{round}.txt"),
            &format!("new {round}"),
        );
        assert_all_synchronized(&world.run_once(plans.clone()));
        assert_eq!(read(&beta, "dir0/file0.txt"), format!("round {round}"));
        assert_eq!(
            read(&beta, &format!("dir1/added{round}.txt")),
            format!("new {round}")
        );
    }

    // A deletion and a beta-side edit must still cross correctly.
    fs::remove_file(alpha.join("dir2/file2.txt")).expect("file should be removable");
    write(&beta, "beta-only.txt", "from the far side");
    assert_all_synchronized(&world.run_once(plans));
    assert!(!beta.join("dir2/file2.txt").exists());
    assert_eq!(read(&alpha, "beta-only.txt"), "from the far side");
}

#[test]
fn a_remote_alpha_synchronizes_through_a_real_agent() {
    let world = World::new();
    let remote_alpha = world.directory("remote-src");
    let local_beta = world.directory("local-dst");
    write(&remote_alpha, "artifact.bin", "built content");
    write(&remote_alpha, "nested/report.txt", "report");
    write(&local_beta, "local-note.txt", "kept");

    // The alpha is a *remote* specification reached through a real agent
    // subprocess; the beta is a plain local directory.
    let plans = world.plans(&format!(
        r#"
        [groups.pull]
        alpha = "remote-host:{remote_alpha}"
        mode = "two-way-safe"
        agent_command = "{agent} agent"
        betas = ["{local_beta}"]
        "#,
        remote_alpha = remote_alpha.display(),
        agent = agent_binary(),
        local_beta = local_beta.display(),
    ));
    assert_eq!(plans.len(), 1);

    let outcomes = world.run_once(plans);
    assert_all_synchronized(&outcomes);

    // Content flowed in both directions across the remote alpha.
    assert_eq!(read(&local_beta, "artifact.bin"), "built content");
    assert_eq!(read(&local_beta, "nested/report.txt"), "report");
    assert_eq!(read(&remote_alpha, "local-note.txt"), "kept");
}

#[test]
fn a_configuration_file_drives_multiple_groups_and_hosts() {
    let world = World::new();
    let alpha_one = world.directory("project");
    let alpha_two = world.directory("notes");
    let local_beta = world.directory("mirror");
    let second_beta = world.path("second-mirror");
    let agent_beta = world.directory("agent-mirror");
    write(&alpha_one, "src/main.rs", "fn main() {}");
    write(&alpha_one, "README.md", "readme");
    write(&alpha_two, "todo.txt", "everything");

    // One group fans out to two local betas (one of which doesn't exist yet
    // and must be created); the other reaches its beta through a real agent
    // subprocess.
    let plans = world.plans(&format!(
        r#"
        [defaults]
        mode = "two-way-safe"

        [groups.project]
        alpha = "{alpha_one}"
        betas = ["{local_beta}", "{second_beta}"]

        [groups.notes]
        alpha = "{alpha_two}"
        agent_command = "{agent} agent"
        betas = ["remote-host:{agent_beta}"]
        "#,
        alpha_one = alpha_one.display(),
        local_beta = local_beta.display(),
        second_beta = second_beta.display(),
        alpha_two = alpha_two.display(),
        agent = agent_binary(),
        agent_beta = agent_beta.display(),
    ));
    assert_eq!(plans.len(), 3);

    let outcomes = world.run_once(plans.clone());
    assert_all_synchronized(&outcomes);

    // Every destination matches its alpha.
    assert_eq!(read(&local_beta, "src/main.rs"), "fn main() {}");
    assert_eq!(read(&local_beta, "README.md"), "readme");
    assert_eq!(read(&second_beta, "src/main.rs"), "fn main() {}");
    // A root created by the transition takes the configured directory mode
    // (the conservative default), not the umask's.
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        fs::symlink_metadata(&second_beta).expect("root").mode() & 0o777,
        0o700
    );
    assert_eq!(read(&agent_beta, "todo.txt"), "everything");

    // Every session recorded a synchronized status.
    for plan in &plans {
        let status = world.status(plan).expect("status should be recorded");
        assert_eq!(status.state, "synchronized", "{}", plan.display());
        assert_eq!(status.cycles, 1);
        assert_eq!(status.group, plan.group);
        assert!(status.error.is_none());
    }

    // A second pass finds nothing to do, and the cycle counts restart with
    // the new supervisor instance.
    let outcomes = world.run_once(plans.clone());
    assert_all_synchronized(&outcomes);
    for outcome in &outcomes {
        let digest = outcome.result.as_ref().expect("the pass should succeed");
        assert_eq!(digest.alpha_transitions + digest.beta_transitions, 0);
    }
}

#[test]
fn per_group_modes_are_respected() {
    let world = World::new();
    let safe_alpha = world.directory("safe-alpha");
    let safe_beta = world.directory("safe-beta");
    let replica_alpha = world.directory("replica-alpha");
    let replica_beta = world.directory("replica-beta");
    write(&safe_alpha, "shared.txt", "original");
    write(&replica_alpha, "kept.txt", "kept");
    write(&replica_beta, "extra.txt", "beta only");

    let configuration = format!(
        r#"
        [groups.careful]
        alpha = "{safe_alpha}"
        mode = "two-way-safe"
        betas = ["{safe_beta}"]

        [groups.mirror]
        alpha = "{replica_alpha}"
        mode = "one-way-replica"
        betas = ["{replica_beta}"]
        "#,
        safe_alpha = safe_alpha.display(),
        safe_beta = safe_beta.display(),
        replica_alpha = replica_alpha.display(),
        replica_beta = replica_beta.display(),
    );
    let plans = world.plans(&configuration);
    assert_all_synchronized(&world.run_once(plans.clone()));

    // The replica beta mirrors its alpha exactly: the beta-only file is gone.
    assert_eq!(read(&replica_beta, "kept.txt"), "kept");
    assert!(!replica_beta.join("extra.txt").exists());

    // Now diverge the safe group's file on both sides.
    write(&safe_alpha, "shared.txt", "alpha edit");
    write(&safe_beta, "shared.txt", "beta edit");
    let outcomes = world.run_once(plans.clone());
    assert_all_synchronized(&outcomes);

    // The conflict is reported, not resolved: both edits survive.
    assert_eq!(read(&safe_alpha, "shared.txt"), "alpha edit");
    assert_eq!(read(&safe_beta, "shared.txt"), "beta edit");
    let careful = plans
        .iter()
        .find(|plan| plan.group == "careful")
        .expect("the careful plan should exist");
    let status = world.status(careful).expect("status should be recorded");
    assert_eq!(status.state, "conflicts");
    assert_eq!(status.conflicts, vec!["shared.txt".to_owned()]);

    // The same divergence under two-way-resolved resolves in alpha's favor
    // (a fresh state root gives the mode change a clean baseline).
    let resolved_world = World::new();
    let plans = resolved_world.plans(&configuration.replace("two-way-safe", "two-way-resolved"));
    assert_all_synchronized(&resolved_world.run_once(plans.clone()));
    write(&safe_alpha, "shared.txt", "alpha wins");
    write(&safe_beta, "shared.txt", "beta loses");
    assert_all_synchronized(&resolved_world.run_once(plans));
    assert_eq!(read(&safe_beta, "shared.txt"), "alpha wins");
}

#[test]
fn defaults_and_group_ignores_combine() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, ".git/HEAD", "ref: refs/heads/main");
    write(&alpha, "scratch.tmp", "temporary");
    write(&alpha, "real.txt", "real");

    let plans = world.plans(&format!(
        r#"
        [defaults]
        mode = "two-way-safe"
        ignores = [".git"]

        [groups.work]
        alpha = "{alpha}"
        ignores = ["*.tmp"]
        betas = ["{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
    ));
    assert_all_synchronized(&world.run_once(plans));

    assert_eq!(read(&beta, "real.txt"), "real");
    assert!(!beta.join(".git").exists(), "default ignore should apply");
    assert!(
        !beta.join("scratch.tmp").exists(),
        "group ignore should apply"
    );
}

#[test]
fn disabled_hosts_are_excluded_from_supervision() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let enabled_beta = world.directory("enabled-beta");
    let disabled_beta = world.directory("disabled-beta");
    write(&alpha, "file.txt", "content");

    let plans = world.plans(&format!(
        r#"
        disabled_hosts = ["down-host"]

        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        agent_command = "{agent} agent"
        betas = ["up-host:{enabled_beta}", "down-host:{disabled_beta}"]
        "#,
        alpha = alpha.display(),
        agent = agent_binary(),
        enabled_beta = enabled_beta.display(),
        disabled_beta = disabled_beta.display(),
    ));
    // The disabled host never becomes a plan at all.
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].host, "up-host");

    assert_all_synchronized(&world.run_once(plans));
    assert_eq!(read(&enabled_beta, "file.txt"), "content");
    assert!(!disabled_beta.join("file.txt").exists());
}

#[test]
fn an_unreachable_destination_does_not_block_other_sessions() {
    let world = World::new();
    let healthy_alpha = world.directory("healthy-alpha");
    let healthy_beta = world.directory("healthy-beta");
    let doomed_alpha = world.directory("doomed-alpha");
    write(&healthy_alpha, "file.txt", "content");
    write(&doomed_alpha, "file.txt", "content");

    let plans = world.plans(&format!(
        r#"
        [groups.healthy]
        alpha = "{healthy_alpha}"
        mode = "two-way-safe"
        betas = ["{healthy_beta}"]

        [groups.doomed]
        alpha = "{doomed_alpha}"
        mode = "two-way-safe"
        agent_command = "/nonexistent/agent-binary agent"
        betas = ["unreachable-host:/anywhere"]
        "#,
        healthy_alpha = healthy_alpha.display(),
        healthy_beta = healthy_beta.display(),
        doomed_alpha = doomed_alpha.display(),
    ));
    let outcomes = world.run_once(plans.clone());

    // The healthy session completed despite its sibling's failure.
    let healthy = outcomes
        .iter()
        .find(|outcome| outcome.display.starts_with("healthy@"))
        .expect("the healthy outcome should exist");
    assert!(healthy.result.is_ok(), "{:?}", healthy.result);
    assert_eq!(read(&healthy_beta, "file.txt"), "content");

    // The doomed session failed, and both the outcome and its status file
    // say so.
    let doomed = outcomes
        .iter()
        .find(|outcome| outcome.display.starts_with("doomed@"))
        .expect("the doomed outcome should exist");
    assert!(doomed.result.is_err());
    let doomed_plan = plans
        .iter()
        .find(|plan| plan.group == "doomed")
        .expect("the doomed plan should exist");
    let status = world
        .status(doomed_plan)
        .expect("status should be recorded");
    // A destination whose agent command cannot be spawned is a permanent
    // misconfiguration, not a host that happens to be down — so it is
    // `errored`. `unreachable`, which alerting treats with patience
    // because it usually clears itself, is reserved for a host that is
    // genuinely not answering.
    assert_eq!(status.state, "errored");
    assert!(status.error.is_some());
    assert_eq!(status.cycles, 0);
}

#[test]
fn a_status_recording_failure_fails_the_run() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "file.txt", "content");

    let plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        betas = ["{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
    ));

    // Make status recording impossible: a regular file squats on the status
    // directory's path.
    fs::create_dir_all(world.state_root()).expect("state root should be creatable");
    fs::write(world.state_root().join("status"), b"not a directory")
        .expect("blocker should be writable");

    let outcomes = world.run_once(plans);
    let error = outcomes[0]
        .result
        .as_ref()
        .expect_err("an unrecordable attempt must not report success");
    assert!(error.contains("unable to record status"), "{error}");
    // The synchronization itself did happen — only its recording failed.
    assert_eq!(read(&beta, "file.txt"), "content");
}

#[test]
fn concurrent_sessions_over_the_same_state_are_refused() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "file.txt", "content");

    // Duplicate plans can't come from one configuration (plans() rejects
    // them), so simulate two supervisor processes: two Supervisors over the
    // same state root, one of whose workers already holds the session lock.
    let plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        betas = ["{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
    ));
    let mut watch_plans = plans.clone();
    watch_plans[0].interval = Duration::from_millis(30);

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let supervisor = Supervisor::new(watch_plans, world.state_root(), false);
        let stop = &stop;
        let watcher = scope.spawn(move || supervisor.run_watch(stop));
        let _guard = StopGuard(stop);
        assert!(
            wait_until(Duration::from_secs(15), || beta.join("file.txt").exists()),
            "the watcher should be running and synchronized"
        );

        // A second "process" attempting the same session is refused while
        // the first holds the lock.
        let outcomes = world.run_once(plans.clone());
        let error = outcomes[0]
            .result
            .as_ref()
            .expect_err("the session lock must refuse a concurrent run");
        assert!(error.contains("another autobahn process"), "{error}");

        // The refusal must not have clobbered the owner's status file: the
        // session's shared state belongs to the lock holder.
        let status = world
            .status(&plans[0])
            .expect("the owner's status should exist");
        assert_ne!(
            status.state, "error",
            "a lock loser must never overwrite the owner's status: {status:?}"
        );

        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });
}

#[test]
fn a_second_supervisor_over_the_same_state_root_is_refused() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "file.txt", "content");

    let mut plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        betas = ["{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
    ));
    plans[0].interval = Duration::from_millis(30);

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let supervisor = Supervisor::new(plans.clone(), world.state_root(), false);
        let stop_ref = &stop;
        let watcher = scope.spawn(move || supervisor.run_watch(stop_ref));
        let _guard = StopGuard(stop_ref);
        assert!(
            wait_until(Duration::from_secs(15), || beta.join("file.txt").exists()),
            "the first supervisor should be running"
        );

        // A second supervisor for the same state root returns immediately
        // instead of capturing the control socket from the first.
        let second = Supervisor::new(plans.clone(), world.state_root(), false);
        let never = AtomicBool::new(false);
        let start = Instant::now();
        let refusal = second.run_watch(&never);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the second supervisor should be refused promptly"
        );
        let error = format!(
            "{:#}",
            refusal.expect_err("the second supervisor must be refused")
        );
        assert!(error.contains("another autobahn process"), "{error}");

        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });
}

#[test]
fn a_missing_alpha_is_a_session_error_not_a_crash() {
    let world = World::new();
    let beta = world.directory("beta");
    let plans = world.plans(&format!(
        r#"
        [groups.ghost]
        alpha = "{missing}"
        mode = "two-way-safe"
        betas = ["{beta}"]
        "#,
        missing = world.path("never-created").display(),
        beta = beta.display(),
    ));
    let outcomes = world.run_once(plans.clone());
    let error = outcomes[0]
        .result
        .as_ref()
        .expect_err("the session should fail");
    assert!(
        error.contains("halted") && error.contains("alpha folder") && error.contains("missing"),
        "{error}"
    );
    // Recorded as the safety stop it is, with the patience of one that
    // clears on its own: a drive back with the wake never alerts.
    let status = world.status(&plans[0]).expect("status should be recorded");
    assert_eq!(status.state, "halted");
    assert_eq!(status.alert_after_seconds, Some(120));
    assert_eq!(
        autobahn::supervisor::alerts_for(&status),
        vec![autobahn::alerts::Alert::Halted]
    );
}

#[test]
fn a_retargeted_root_is_refused_rather_than_bound_to_stale_state() {
    // Session identity resolves through symlinks at plan time. If the link
    // is retargeted before the worker connects, the path now reaches a
    // different tree — and binding that tree to the planned tree's ancestor
    // hands reconciliation the wrong provenance. The worker must refuse.
    let world = World::new();
    let tree_a = world.directory("tree-a");
    let tree_b = world.directory("tree-b");
    let beta = world.directory("beta");
    write(&tree_a, "file.txt", "a's content");
    write(&tree_b, "file.txt", "b's content");
    let link = world.path("entry");
    std::os::unix::fs::symlink(&tree_a, &link).expect("symlink should be creatable");

    let plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{link}"
        mode = "two-way-safe"
        betas = ["{beta}"]
        "#,
        link = link.display(),
        beta = beta.display(),
    ));

    // The retarget lands between planning and connecting.
    std::fs::remove_file(&link).expect("link should be removable");
    std::os::unix::fs::symlink(&tree_b, &link).expect("symlink should be recreatable");

    let outcomes = world.run_once(plans);
    let error = outcomes[0]
        .result
        .as_ref()
        .expect_err("the session must refuse the retargeted root");
    assert!(
        error.contains("no longer resolves"),
        "expected the retarget refusal, got: {error}"
    );
    assert!(
        !beta.join("file.txt").exists(),
        "nothing may be synchronized from the wrong tree"
    );
}

#[test]
fn state_persists_across_supervisor_runs() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "keep.txt", "keep");
    write(&alpha, "remove.txt", "remove");

    let configuration = format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        betas = ["{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
    );
    let plans = world.plans(&configuration);
    assert_all_synchronized(&world.run_once(plans.clone()));
    assert_eq!(read(&beta, "remove.txt"), "remove");

    // Delete on alpha, then run a *fresh* supervisor over the same state
    // root: only a persisted ancestor lets it see a deletion rather than a
    // one-sided file (which two-way-safe would copy back).
    fs::remove_file(alpha.join("remove.txt")).expect("file should be removable");
    assert_all_synchronized(&world.run_once(plans));
    assert!(!beta.join("remove.txt").exists());
    assert!(alpha.join("keep.txt").exists());
    assert_eq!(read(&beta, "keep.txt"), "keep");
}

#[test]
fn watch_mode_synchronizes_continuously_until_stopped() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "first.txt", "first");

    let mut plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        betas = ["{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
    ));
    // The configuration expresses intervals in whole seconds; drive the
    // watch loop faster for the test.
    plans[0].interval = Duration::from_millis(30);
    let plan = plans[0].clone();

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let supervisor = Supervisor::new(plans, world.state_root(), false);
        let stop = &stop;
        let watcher = scope.spawn(move || supervisor.run_watch(stop));
        let _guard = StopGuard(stop);

        // The initial content propagates...
        assert!(
            wait_until(Duration::from_secs(15), || beta.join("first.txt").exists()),
            "initial content should propagate"
        );
        // ...changes made while watching propagate in both directions...
        write(&alpha, "second.txt", "second");
        write(&beta, "from-beta.txt", "reverse");
        assert!(
            wait_until(Duration::from_secs(15), || {
                beta.join("second.txt").exists() && alpha.join("from-beta.txt").exists()
            }),
            "ongoing changes should propagate both ways"
        );

        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });

    // The status reflects a session that cycled repeatedly.
    let status = world.status(&plan).expect("status should be recorded");
    assert!(status.cycles > 1, "expected multiple cycles: {status:?}");
    assert_eq!(status.state, "synchronized");
}

/// Runs a watching supervisor over the configuration at `path`, with the
/// file watched every 20 ms, for as long as `during` runs — and asserts
/// that it was still running at the end: every edit made meanwhile was
/// applied in place, not by winding the supervisor down.
fn supervise_with_reload(world: &World, path: &Path, during: impl FnOnce()) {
    use autobahn::supervisor::reload::{load_for_startup, Reloader};
    use std::sync::Arc;
    let loaded = load_for_startup(path).expect("configuration should load");
    let reloader =
        Arc::new(Reloader::new(path.to_path_buf()).with_interval(Duration::from_millis(20)));
    let stop = AtomicBool::new(false);
    // As `watch` builds it.
    let supervisor = Supervisor::new(loaded.plans, world.state_root(), false)
        .with_alerts(loaded.alerts)
        .with_log_level(loaded.log_level)
        .with_configuration(loaded.text)
        .with_reload(Some(reloader.clone()));
    std::thread::scope(|scope| {
        let _guard = StopGuard(&stop);
        let watcher = scope.spawn(|| supervisor.run_watch(&stop));
        during();
        assert!(
            !watcher.is_finished(),
            "every edit was applied without winding the supervisor down"
        );
        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });
    assert!(reloader.take().is_none(), "nothing was left for a restart");
}

/// An agent script that notes each launch, and each exit, in a counter
/// file, so a test can tell a kept connection from a new one and a closed
/// one from one left open.
fn counting_agent(world: &World, name: &str) -> (PathBuf, PathBuf) {
    let counter = world.path(&format!("{name}.count"));
    let script = world.path(&format!("{name}.sh"));
    fs::write(
        &script,
        format!(
            "#!/bin/sh\necho launch >> {counter}\n{agent} agent\necho exit >> {counter}\n",
            counter = counter.display(),
            agent = agent_binary()
        ),
    )
    .expect("script should be writable");
    let mut permissions = fs::metadata(&script).expect("script").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    fs::set_permissions(&script, permissions).expect("script should be executable");
    (script, counter)
}

/// How many times a counting agent's counter holds `word`.
fn counted(counter: &Path, word: &str) -> usize {
    fs::read_to_string(counter)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == word)
        .count()
}

/// The sessions a running supervisor says it is running, as
/// `group@host`, or None when none answers.
fn supervised(world: &World) -> Option<Vec<String>> {
    autobahn::supervisor::control::query_progress(&world.state_root()).map(|sessions| {
        sessions
            .into_iter()
            .map(|session| format!("{}@{}", session.group, session.host))
            .collect()
    })
}

/// The cycle count a session last recorded.
fn cycles(world: &World, plan: &SessionPlan) -> u64 {
    world.status(plan).map(|status| status.cycles).unwrap_or(0)
}

#[test]
fn an_edited_configuration_is_applied_without_a_restart() {
    use autobahn::supervisor::reload::read_notice;
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    let notes = world.directory("notes");
    let notes_mirror = world.path("notes-mirror");
    write(&alpha, "first.txt", "first");
    write(&notes, "todo.txt", "everything");
    let (script, counter) = counting_agent(&world, "agent");

    let path = world.path("config.toml");
    let one_group = format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        interval = 1
        agent_command = "{script}"
        betas = ["host:{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
        script = script.display(),
    );
    let plans = world.plans(&one_group);
    let work = plans[0].clone();

    supervise_with_reload(&world, &path, || {
        assert!(
            wait_until(Duration::from_secs(15), || beta.join("first.txt").exists()),
            "the first configuration synchronizes"
        );
        // A broken edit is refused and recorded; the session runs on.
        fs::write(&path, format!("{one_group}\n[groups.notes]\nmdoe = 1\n"))
            .expect("configuration should be writable");
        assert!(
            wait_until(Duration::from_secs(15), || {
                read_notice(&world.state_root()).is_some()
            }),
            "the refusal is recorded"
        );
        write(&alpha, "second.txt", "second");
        assert!(
            wait_until(Duration::from_secs(15), || beta.join("second.txt").exists()),
            "the session runs on under the refused edit"
        );
        // A good one adds a group, which starts while the first runs on:
        // the same worker, so its cycle count carries on from where it
        // was, over the same connection.
        let before = cycles(&world, &work);
        assert!(before > 0);
        fs::write(
            &path,
            format!(
                "{one_group}\n[groups.notes]\nmode = \"two-way-safe\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
                notes.display(),
                notes_mirror.display()
            ),
        )
        .expect("configuration should be writable");
        assert!(
            wait_until(Duration::from_secs(15), || {
                notes_mirror.join("todo.txt").exists()
            }),
            "the added group synchronizes"
        );
        write(&alpha, "third.txt", "third");
        assert!(
            wait_until(Duration::from_secs(15), || beta.join("third.txt").exists()),
            "the kept group runs on"
        );
        assert!(
            cycles(&world, &work) > before,
            "the kept session's cycle count carried on rather than starting over"
        );
        assert_eq!(
            counted(&counter, "launch"),
            1,
            "the kept connection was kept"
        );
        assert_eq!(
            read_notice(&world.state_root()),
            None,
            "the refusal is over"
        );
    });
}

/// An edit applied in place is held to the check made at startup: a
/// root holding autobahn's own state is complained about and not applied,
/// and the sessions running carry on.
#[test]
fn an_edit_whose_root_holds_the_state_root_is_not_applied() {
    use autobahn::supervisor::reload::Reloader;
    use std::sync::Arc;
    let world = World::new();
    // The work group's trees are outside the world's directory, which the
    // edit adds a root for.
    let elsewhere = TempDir::new().expect("temporary directory should be creatable");
    let alpha = elsewhere.path().join("alpha");
    let beta = elsewhere.path().join("beta");
    fs::create_dir_all(&alpha).expect("directory should be creatable");
    fs::create_dir_all(&beta).expect("directory should be creatable");
    let mirror = elsewhere.path().join("mirror");
    write(&alpha, "first.txt", "first");
    let path = world.path("config.toml");
    let one_group = format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        interval = 1
        betas = ["{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
    );
    let plans = world.plans(&one_group);
    let reloader = Arc::new(Reloader::new(path.clone()).with_interval(Duration::from_millis(20)));
    let stop = AtomicBool::new(false);
    let supervisor = Supervisor::new(plans, world.state_root(), false)
        .with_reload(Some(reloader.clone()))
        .with_own_state(autobahn::config::OwnState::new(
            &world.state_root(),
            Some(&path),
        ));
    std::thread::scope(|scope| {
        let _guard = StopGuard(&stop);
        let watcher = scope.spawn(|| supervisor.run_watch(&stop));
        assert!(
            wait_until(Duration::from_secs(15), || beta.join("first.txt").exists()),
            "the first configuration synchronizes"
        );
        // The world's directory holds the state root and the configuration.
        fs::write(
            &path,
            format!(
                "{one_group}\n[groups.everything]\nmode = \"two-way-safe\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
                world.path("").display(),
                mirror.display()
            ),
        )
        .expect("configuration should be writable");
        std::thread::sleep(Duration::from_millis(500));
        write(&alpha, "second.txt", "second");
        assert!(
            wait_until(Duration::from_secs(15), || beta.join("second.txt").exists()),
            "the running session carries on"
        );
        assert_eq!(
            supervised(&world),
            Some(vec![format!("work@{}", beta.display())]),
            "the edit was not applied"
        );
        assert!(!mirror.exists(), "nothing ran over the state root");
        assert!(!watcher.is_finished());
        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });
}

#[test]
fn an_edit_to_one_groups_ignores_restarts_only_that_session() {
    let world = World::new();
    let alpha_one = world.directory("alpha-one");
    let alpha_two = world.directory("alpha-two");
    let beta_one = world.directory("beta-one");
    let beta_two = world.directory("beta-two");
    let (script, counter) = counting_agent(&world, "agent");
    let path = world.path("config.toml");
    let configuration = |ignores: &str| {
        format!(
            r#"
            [defaults]
            mode = "two-way-safe"
            interval = 1

            [groups.one]
            alpha = "{alpha_one}"
            agent_command = "{script}"
            betas = ["shared-host:{beta_one}"]

            [groups.two]
            alpha = "{alpha_two}"
            agent_command = "{script}"
            ignores = [{ignores}]
            betas = ["shared-host:{beta_two}"]
            "#,
            script = script.display(),
            alpha_one = alpha_one.display(),
            alpha_two = alpha_two.display(),
            beta_one = beta_one.display(),
            beta_two = beta_two.display(),
        )
    };
    let plans = world.plans(&configuration(""));
    let (one, two) = (plans[0].clone(), plans[1].clone());
    write(&alpha_one, "one.txt", "one");
    write(&alpha_two, "two.txt", "two");

    supervise_with_reload(&world, &path, || {
        assert!(wait_until(Duration::from_secs(15), || {
            beta_one.join("one.txt").exists() && beta_two.join("two.txt").exists()
        }));
        // Run the session to be changed through more cycles than one
        // attempt can hold, so its restart shows as a count that went back.
        let limit = autobahn::supervisor::MAXIMUM_FOLLOW_UP_CYCLES as u64 + 1;
        for index in 0..=limit {
            let name = format!("warm-{index}.txt");
            write(&alpha_two, &name, "warm");
            assert!(wait_until(Duration::from_secs(15), || beta_two
                .join(&name)
                .exists()));
        }
        assert!(wait_until(Duration::from_secs(15), || {
            cycles(&world, &one) >= 1 && cycles(&world, &two) > limit
        }));
        let one_before = cycles(&world, &one);
        let two_before = cycles(&world, &two);
        fs::write(&path, configuration("\"*.tmp\"")).expect("configuration should be writable");
        assert!(
            wait_until(Duration::from_secs(15), || cycles(&world, &two)
                < two_before),
            "the changed session starts over ({} after {two_before})",
            cycles(&world, &two)
        );
        write(&alpha_two, "scratch.tmp", "scratch");
        write(&alpha_two, "kept.txt", "kept");
        assert!(
            wait_until(Duration::from_secs(15), || beta_two
                .join("kept.txt")
                .exists()),
            "the changed session runs under its new plan"
        );
        assert!(
            !beta_two.join("scratch.tmp").exists(),
            "the new ignores apply"
        );
        assert!(
            wait_until(Duration::from_secs(15), || cycles(&world, &one)
                > one_before),
            "the untouched session carried on counting rather than starting over"
        );
        assert_eq!(
            counted(&counter, "launch"),
            1,
            "the host's connection stayed up for both"
        );
    });
}

#[test]
fn disabling_one_group_leaves_the_others_connected_and_closes_its_host() {
    let world = World::new();
    let alpha_one = world.directory("alpha-one");
    let alpha_two = world.directory("alpha-two");
    let beta_one = world.directory("beta-one");
    let beta_two = world.directory("beta-two");
    let (script_one, counter_one) = counting_agent(&world, "agent-one");
    let (script_two, counter_two) = counting_agent(&world, "agent-two");
    let path = world.path("config.toml");
    let text = format!(
        r#"
        [defaults]
        mode = "two-way-safe"
        interval = 1

        [groups.one]
        alpha = "{alpha_one}"
        agent_command = "{script_one}"
        betas = ["host-one:{beta_one}"]

        [groups.two]
        alpha = "{alpha_two}"
        agent_command = "{script_two}"
        betas = ["host-two:{beta_two}"]
        "#,
        script_one = script_one.display(),
        script_two = script_two.display(),
        alpha_one = alpha_one.display(),
        alpha_two = alpha_two.display(),
        beta_one = beta_one.display(),
        beta_two = beta_two.display(),
    );
    world.plans(&text);
    write(&alpha_one, "one.txt", "one");
    write(&alpha_two, "two.txt", "two");

    supervise_with_reload(&world, &path, || {
        assert!(wait_until(Duration::from_secs(15), || {
            beta_one.join("one.txt").exists() && beta_two.join("two.txt").exists()
        }));
        let (disabled, _) =
            autobahn::config::set_group_disabled(&text, "two", true).expect("the edit applies");
        fs::write(&path, disabled).expect("configuration should be writable");
        assert!(
            wait_until(Duration::from_secs(15), || {
                supervised(&world).as_deref() == Some(&["one@host-one".to_owned()][..])
            }),
            "only the disabled group stops: {:?}",
            supervised(&world)
        );
        // No session reaches host-two any more, so its connection closes.
        assert!(
            wait_until(Duration::from_secs(15), || counted(&counter_two, "exit")
                == 1),
            "the disabled group's host is not kept connected"
        );
        write(&alpha_one, "more.txt", "more");
        assert!(wait_until(Duration::from_secs(15), || beta_one
            .join("more.txt")
            .exists()));
        assert_eq!(
            counted(&counter_one, "launch"),
            1,
            "the others did not reconnect"
        );
        assert_eq!(counted(&counter_one, "exit"), 0);
    });
}

/// With one group running, `edit` turns it off and `undo` back on: the
/// group stops syncing without the supervisor stopping, and when it is back
/// it resumes from its ancestor — a deletion made meanwhile propagates,
/// where a session that had lost its ancestor would bring the file back.
fn turning_the_only_group_off_and_on(edit: impl Fn(&str) -> String) {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    let path = world.path("config.toml");
    let text = format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        interval = 1
        agent_command = "{agent} agent"
        betas = ["only-host:{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
        agent = agent_binary(),
    );
    world.plans(&text);
    write(&alpha, "kept.txt", "kept");
    write(&alpha, "doomed.txt", "doomed");

    supervise_with_reload(&world, &path, || {
        assert!(wait_until(Duration::from_secs(15), || {
            beta.join("kept.txt").exists() && beta.join("doomed.txt").exists()
        }));
        fs::write(&path, edit(&text)).expect("configuration should be writable");
        assert!(
            wait_until(Duration::from_secs(15), || {
                supervised(&world).is_some_and(|sessions| sessions.is_empty())
            }),
            "the session stops, and the supervisor answers with nothing running"
        );
        let (succeeded, shown) = cli(&world, &path, &["status"]);
        assert!(succeeded, "{shown}");
        assert!(shown.contains("no active sessions"), "{shown}");
        write(&alpha, "while-off.txt", "while off");
        fs::remove_file(alpha.join("doomed.txt")).expect("removable");
        std::thread::sleep(Duration::from_millis(2500));
        assert!(
            !beta.join("while-off.txt").exists(),
            "nothing propagates while off"
        );
        assert!(beta.join("doomed.txt").exists());

        fs::write(&path, &text).expect("configuration should be writable");
        assert!(
            wait_until(Duration::from_secs(15), || beta
                .join("while-off.txt")
                .exists()),
            "syncing resumes"
        );
        assert!(
            wait_until(Duration::from_secs(15), || !beta
                .join("doomed.txt")
                .exists()),
            "the deletion propagates: the ancestor is intact"
        );
        assert!(!alpha.join("doomed.txt").exists());
        assert_eq!(read(&beta, "kept.txt"), "kept");
    });
}

#[test]
fn disabling_the_only_group_stops_it_and_enabling_resumes_it() {
    turning_the_only_group_off_and_on(|text| {
        autobahn::config::set_group_disabled(text, "work", true)
            .expect("the edit applies")
            .0
    });
}

#[test]
fn disabling_the_only_host_stops_it_and_enabling_resumes_it() {
    turning_the_only_group_off_and_on(|text| {
        autobahn::config::set_host_disabled(text, "only-host", true)
            .expect("the edit applies")
            .0
    });
}

#[test]
fn removing_the_last_group_stops_it_and_restoring_resumes_it() {
    turning_the_only_group_off_and_on(|_| "# nothing to do for now\n".to_owned());
}

#[test]
fn watch_mode_heals_after_a_destination_recovers() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "file.txt", "content");

    // The agent command is a script that initially fails, standing in for an
    // unreachable host; rewriting it to exec the real agent stands in for
    // the host coming back.
    let script = world.path("flaky-agent.sh");
    fs::write(&script, "#!/bin/sh\nexit 1\n").expect("script should be writable");
    let mut permissions = fs::metadata(&script)
        .expect("script should exist")
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    fs::set_permissions(&script, permissions).expect("script should be executable");

    let mut plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        agent_command = "{script}"
        betas = ["flaky-host:{beta}"]
        "#,
        alpha = alpha.display(),
        script = script.display(),
        beta = beta.display(),
    ));
    plans[0].interval = Duration::from_millis(30);
    let plan = plans[0].clone();

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let supervisor = Supervisor::new(plans, world.state_root(), false);
        let stop = &stop;
        let watcher = scope.spawn(move || supervisor.run_watch(stop));
        let _guard = StopGuard(stop);

        // The failure is observed and recorded.
        assert!(
            wait_until(Duration::from_secs(15), || {
                world
                    .status(&plan)
                    .is_some_and(|status| status.state == "errored")
            }),
            "the failure should be recorded"
        );
        assert!(!beta.join("file.txt").exists());

        // The destination recovers; the session heals without intervention.
        fs::write(
            &script,
            format!("#!/bin/sh\nexec {} agent\n", agent_binary()),
        )
        .expect("script should be rewritable");
        assert!(
            wait_until(Duration::from_secs(20), || beta.join("file.txt").exists()),
            "the session should heal and synchronize"
        );
        assert!(
            wait_until(Duration::from_secs(15), || {
                world
                    .status(&plan)
                    .is_some_and(|status| status.state == "synchronized")
            }),
            "the recovery should be recorded"
        );

        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });
}

#[test]
fn control_socket_pauses_resumes_and_resets_sessions() {
    use autobahn::supervisor::control::{self, ControlRequest, ControlResponse, Selector};

    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "first.txt", "first");

    let mut plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        interval = 3600
        betas = ["{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
    ));
    // The interval is effectively disabled: everything below must happen
    // through change notifications and control requests.
    plans[0].interval = Duration::from_secs(3600);
    let plan = plans[0].clone();

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let supervisor = Supervisor::new(plans, world.state_root(), false);
        let stop_ref = &stop;
        let watcher = scope.spawn(move || supervisor.run_watch(stop_ref));
        let _guard = StopGuard(stop_ref);

        // Startup synchronizes, and file changes propagate purely through
        // watching (no heartbeat is coming for an hour).
        assert!(
            wait_until(Duration::from_secs(15), || beta.join("first.txt").exists()),
            "initial content should synchronize"
        );
        write(&alpha, "second.txt", "second");
        assert!(
            wait_until(Duration::from_secs(15), || beta.join("second.txt").exists()),
            "a watched change should propagate without a heartbeat"
        );

        // Pause: the state records, and further changes stop propagating.
        let selector = || Selector {
            group: Some("work".into()),
            host: None,
        };
        let response = control::send(&world.state_root(), &ControlRequest::Pause(selector()))
            .expect("pause should send");
        assert!(matches!(response, ControlResponse::Applied { sessions: 1 }));
        assert!(
            wait_until(Duration::from_secs(15), || {
                world
                    .status(&plan)
                    .is_some_and(|status| status.state == "paused")
            }),
            "the pause should be recorded"
        );
        // Delete a synchronized file on alpha while paused; nothing moves.
        fs::remove_file(alpha.join("second.txt")).expect("file should be removable");
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            beta.join("second.txt").exists(),
            "paused sessions must not sync"
        );

        // Reset while paused, then resume: with the ancestor discarded, the
        // deletion is forgotten and beta's copy flows back to alpha.
        let response = control::send(&world.state_root(), &ControlRequest::Reset(selector()))
            .expect("reset should send");
        assert!(matches!(response, ControlResponse::Applied { sessions: 1 }));
        let response = control::send(&world.state_root(), &ControlRequest::Resume(selector()))
            .expect("resume should send");
        assert!(matches!(response, ControlResponse::Applied { sessions: 1 }));
        assert!(
            wait_until(Duration::from_secs(15), || alpha
                .join("second.txt")
                .exists()),
            "after a reset, the deletion is forgotten and content merges back"
        );

        // A selector matching nothing is an error, not a silent no-op.
        let response = control::send(
            &world.state_root(),
            &ControlRequest::Flush(Selector {
                group: Some("absent".into()),
                host: None,
            }),
        )
        .expect("the request should send");
        assert!(matches!(response, ControlResponse::Error(_)));

        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });
}

#[test]
fn watch_mode_observes_remote_changes_through_the_agent() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let remote = world.directory("remote-mirror");
    write(&alpha, "seed.txt", "seed");

    let mut plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        agent_command = "{agent} agent"
        betas = ["fake-host:{remote}"]
        "#,
        alpha = alpha.display(),
        agent = agent_binary(),
        remote = remote.display(),
    ));
    // No heartbeat within the test window: propagation must ride the
    // agent-side watcher through the AwaitChanges protocol.
    plans[0].interval = Duration::from_secs(3600);

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let supervisor = Supervisor::new(plans, world.state_root(), false);
        let stop_ref = &stop;
        let watcher = scope.spawn(move || supervisor.run_watch(stop_ref));
        let _guard = StopGuard(stop_ref);

        assert!(
            wait_until(Duration::from_secs(15), || remote.join("seed.txt").exists()),
            "initial content should synchronize"
        );
        // A change on the *remote* side propagates back without a heartbeat.
        write(&remote, "from-remote.txt", "remote change");
        assert!(
            wait_until(Duration::from_secs(15), || {
                alpha.join("from-remote.txt").exists()
            }),
            "remote changes should be observed through the agent"
        );

        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });
}

#[test]
fn agents_install_automatically_over_ssh() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let remote_home = world.directory("remote-home");
    let remote_mirror = world.directory("remote-mirror");
    write(&alpha, "file.txt", "content");

    // A fake `ssh` that runs the remote command locally under the fake
    // remote home — auth-free SSH semantics, faithful enough for the
    // install flow (which streams the agent binary through stdin). Every
    // remote command must be wrapped in `sh -c`, since a real login shell
    // may not be POSIX; one that is not here is refused, and the command
    // runs under fish or tcsh when either is installed.
    let script_dir = world.directory("fake-bin");
    let script = script_dir.join("ssh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             while [ $# -gt 0 ]; do case \"$1\" in -o) shift 2;; -T) shift;; --) shift; break;; *) break;; esac; done\n\
             shift\n\
             case \"$*\" in 'sh -c '*) ;; *) echo \"not wrapped in sh -c: $*\" >&2; exit 99;; esac\n\
             login=/bin/sh\n\
             for shell in fish tcsh; do command -v $shell >/dev/null 2>&1 && login=$(command -v $shell) && break; done\n\
             HOME={home} exec \"$login\" -c \"$*\"\n",
            home = remote_home.display()
        ),
    )
    .expect("script should be writable");
    let mut permissions = fs::metadata(&script).expect("script").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    fs::set_permissions(&script, permissions).expect("script should be executable");

    // The agent bundle holds this platform's binary under bundle naming —
    // the fake remote is this very machine, so the installer's uname probe
    // reports whatever platform the test runs on.
    let agents = world.directory("agents");
    let platform = format!(
        "autobahn-{}-{}",
        match std::env::consts::OS {
            "macos" => "darwin",
            other => other,
        },
        std::env::consts::ARCH
    );
    fs::copy(agent_binary(), agents.join(platform)).expect("bundle copy");

    // The fake ssh and the bundle reach the controller through its
    // environment, so the controller runs as a child process with them set
    // on it alone: setting them here would change them for every test in
    // this process, which run in parallel.
    let configuration = world.path("config.toml");
    fs::write(
        &configuration,
        format!(
            r#"
            [groups.work]
            alpha = "{alpha}"
            mode = "two-way-safe"
            betas = ["fake-host:{remote_mirror}"]
            "#,
            alpha = alpha.display(),
            remote_mirror = remote_mirror.display(),
        ),
    )
    .expect("the configuration should be writable");
    let controller_home = world.directory("controller-home");
    let sync_once = || {
        let output = std::process::Command::new(agent_binary())
            .arg("sync")
            .arg("--config")
            .arg(&configuration)
            .arg("--state-root")
            .arg(world.state_root())
            .env("HOME", &controller_home)
            .env_remove("AUTOBAHN_HOME")
            .env("AUTOBAHN_SSH", &script)
            .env("AUTOBAHN_AGENTS_DIR", &agents)
            .output()
            .expect("the controller should run");
        assert!(
            output.status.success(),
            "the sync should succeed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    sync_once();

    // The content synchronized, and the versioned agent was installed into
    // the (fake) remote home along the way.
    assert_eq!(read(&remote_mirror, "file.txt"), "content");
    let installed: Vec<String> = fs::read_dir(remote_home.join(".autobahn/bin"))
        .expect("the agent directory should exist")
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        installed.iter().any(|name| name.starts_with("autobahn-")),
        "{installed:?}"
    );

    // A second pass reuses the installed agent.
    sync_once();
}

#[test]
fn sessions_on_one_host_share_one_agent_connection() {
    let world = World::new();
    let alpha_one = world.directory("alpha-one");
    let alpha_two = world.directory("alpha-two");
    let beta_one = world.directory("beta-one");
    let beta_two = world.directory("beta-two");
    write(&alpha_one, "one.txt", "one");
    write(&alpha_two, "two.txt", "two");

    // The wrapper counts agent launches: two sessions with the same spawn
    // command must share one pooled connection, and therefore one process.
    let counter = world.path("launch-count");
    let script = world.path("counting-agent.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\necho launch >> {counter}\nexec {agent} agent\n",
            counter = counter.display(),
            agent = agent_binary()
        ),
    )
    .expect("script should be writable");
    let mut permissions = fs::metadata(&script).expect("script").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    fs::set_permissions(&script, permissions).expect("script should be executable");

    let plans = world.plans(&format!(
        r#"
        [defaults]
        mode = "two-way-safe"

        [groups.one]
        alpha = "{alpha_one}"
        agent_command = "{script}"
        betas = ["shared-host:{beta_one}"]

        [groups.two]
        alpha = "{alpha_two}"
        agent_command = "{script}"
        betas = ["shared-host:{beta_two}"]
        "#,
        script = script.display(),
        alpha_one = alpha_one.display(),
        alpha_two = alpha_two.display(),
        beta_one = beta_one.display(),
        beta_two = beta_two.display(),
    ));
    let outcomes = world.run_once(plans);
    assert_all_synchronized(&outcomes);

    // Both sessions synchronized...
    assert_eq!(read(&beta_one, "one.txt"), "one");
    assert_eq!(read(&beta_two, "two.txt"), "two");
    // ...through exactly one agent process.
    let launches = fs::read_to_string(&counter).expect("the counter should exist");
    assert_eq!(launches.lines().count(), 1, "{launches:?}");
}

#[test]
fn policy_flows_through_the_agent_protocol() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    fs::write(alpha.join("file.txt"), "content").expect("file should be writable");
    std::os::unix::fs::symlink("file.txt", alpha.join("link")).expect("symlink");

    // The group's policy — ignored symlinks and 0644 files — must govern the
    // *agent-side* endpoint, proving Initialize carries it across the wire.
    let plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        symlink_mode = "ignore"
        file_mode = "0644"
        directory_mode = "0755"
        agent_command = "{agent} agent"
        betas = ["remote-host:{beta}"]
        "#,
        alpha = alpha.display(),
        agent = agent_binary(),
        beta = beta.display(),
    ));
    let outcomes = world.run_once(plans);
    assert_all_synchronized(&outcomes);

    assert_eq!(read(&beta, "file.txt"), "content");
    use std::os::unix::fs::MetadataExt;
    let mode = fs::symlink_metadata(beta.join("file.txt"))
        .expect("file should exist")
        .mode()
        & 0o777;
    assert_eq!(mode, 0o644);
    // The symlink was invisible on both sides.
    assert!(!beta.join("link").exists());
}

#[test]
fn remote_home_relative_roots_resolve_against_the_agent_home() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let remote_home = world.directory("remote-home");
    write(&alpha, "file.txt", "content");

    // The wrapper gives the agent its own home directory, standing in for a
    // remote host whose home differs from the local one.
    let script = world.path("remote-agent.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nHOME={} exec {} agent\n",
            remote_home.display(),
            agent_binary()
        ),
    )
    .expect("script should be writable");
    let mut permissions = fs::metadata(&script)
        .expect("script should exist")
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    fs::set_permissions(&script, permissions).expect("script should be executable");

    let plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        agent_command = "{script}"
        betas = ["remote-host:~/mirror"]
        "#,
        alpha = alpha.display(),
        script = script.display(),
    ));
    assert_all_synchronized(&world.run_once(plans));

    // `~/mirror` resolved against the agent's home, not the controller's.
    assert_eq!(read(&remote_home, "mirror/file.txt"), "content");
}

// ── sync exit codes ──────────────────────────────────────────────────

/// Runs the CLI like [`cli`], answering with its exit code.
fn cli_code(world: &World, config: &Path, args: &[&str]) -> (Option<i32>, String) {
    let output = std::process::Command::new(agent_binary())
        .args(args)
        .arg("--config")
        .arg(config)
        .arg("--state-root")
        .arg(world.state_root())
        .output()
        .expect("the CLI runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code(), text)
}

/// Writes a configuration of one two-way-conflict group per `(name,
/// alpha, beta)`, with an extra line of settings for each.
fn exit_code_config(world: &World, groups: &[(&str, &Path, &str, &str)]) -> PathBuf {
    let config = world.path("config.toml");
    let mut text = String::new();
    for (name, alpha, beta, extra) in groups {
        text.push_str(&format!(
            "[groups.{name}]\nmode = \"two-way-conflict\"\nalpha = \"{}\"\nbetas = [\"{beta}\"]\n{extra}\n",
            alpha.display()
        ));
    }
    fs::write(&config, text).unwrap();
    config
}

#[test]
fn sync_exits_zero_when_every_session_converged() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "file.txt", "content");
    let beta_spec = beta.to_string_lossy().to_string();
    let config = exit_code_config(&world, &[("g", &alpha, &beta_spec, "")]);
    let (code, text) = cli_code(&world, &config, &["sync"]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(read(&beta, "file.txt"), "content");
}

#[test]
fn sync_exits_two_when_a_conflict_remains() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "file.txt", "original");
    let beta_spec = beta.to_string_lossy().to_string();
    let config = exit_code_config(&world, &[("g", &alpha, &beta_spec, "")]);
    assert_eq!(cli_code(&world, &config, &["sync"]).0, Some(0));
    write(&alpha, "file.txt", "v-alpha");
    write(&beta, "file.txt", "v-beta");
    let (code, text) = cli_code(&world, &config, &["sync"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("conflict"), "{text}");
}

#[test]
fn sync_exits_one_when_a_destination_is_unreachable() {
    let world = World::new();
    let alpha = world.directory("alpha");
    write(&alpha, "file.txt", "content");
    let config = exit_code_config(
        &world,
        &[(
            "g",
            &alpha,
            "unreachable-host:/anywhere",
            "agent_command = \"/nonexistent/agent-binary agent\"",
        )],
    );
    let (code, text) = cli_code(&world, &config, &["sync"]);
    assert_eq!(code, Some(1), "{text}");
}

/// Several sessions exit with the worst of them: an error outranks a
/// conflict.
#[test]
fn sync_exits_with_the_worst_session_an_error_beating_a_conflict() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    let doomed = world.directory("doomed");
    write(&alpha, "file.txt", "original");
    write(&doomed, "file.txt", "content");
    let beta_spec = beta.to_string_lossy().to_string();
    let only_conflicted = exit_code_config(&world, &[("g", &alpha, &beta_spec, "")]);
    assert_eq!(cli_code(&world, &only_conflicted, &["sync"]).0, Some(0));
    write(&alpha, "file.txt", "v-alpha");
    write(&beta, "file.txt", "v-beta");
    let config = exit_code_config(
        &world,
        &[
            ("g", &alpha, &beta_spec, ""),
            (
                "doomed",
                &doomed,
                "unreachable-host:/anywhere",
                "agent_command = \"/nonexistent/agent-binary agent\"",
            ),
        ],
    );
    let (code, text) = cli_code(&world, &config, &["sync"]);
    assert_eq!(code, Some(1), "{text}");
    // The conflicted session still ran its pass and said so.
    assert!(text.contains("1 conflict(s)"), "{text}");
}

/// A manual sync of two roots follows the same codes.
#[test]
fn a_manual_sync_exits_two_when_a_conflict_remains() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "file.txt", "original");
    let state = world.path("manual-state");
    let run = || {
        let output = std::process::Command::new(agent_binary())
            .arg("sync")
            .arg(&alpha)
            .arg(&beta)
            .arg("--mode")
            .arg("two-way-safe")
            .arg("--state-dir")
            .arg(&state)
            .output()
            .expect("the CLI runs");
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let (code, text) = run();
    assert_eq!(code, Some(0), "{text}");
    write(&alpha, "file.txt", "v-alpha");
    write(&beta, "file.txt", "v-beta");
    let (code, text) = run();
    assert_eq!(code, Some(2), "{text}");
}

/// A quoted `~` reaches a manual sync unexpanded by the shell; it means
/// home there as it does in the configuration, not a directory named `~`.
#[test]
fn a_manual_sync_expands_a_quoted_tilde() {
    let world = World::new();
    let home = world.directory("home");
    let cwd = world.directory("cwd");
    write(&home, "a/file.txt", "content");
    fs::create_dir_all(home.join("b")).unwrap();
    let output = std::process::Command::new(agent_binary())
        .args(["sync", "~/a", "~/b"])
        .current_dir(&cwd)
        .env("HOME", &home)
        .env_remove("AUTOBAHN_HOME")
        .output()
        .expect("the CLI runs");
    let text = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert_eq!(read(&home, "b/file.txt"), "content");
    assert!(!cwd.join("~").exists(), "a literal ./~ was created");
}

/// A wildcard negation under an ignored directory re-includes nothing,
/// and loading the configuration says so rather than staying silent.
#[test]
fn a_wildcard_negation_under_an_ignored_directory_is_warned_about() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "vendor/fix.patch", "patch");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[groups.g]\nmode = \"two-way-conflict\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n\
             ignores = [\"vendor\", \"!vendor/*.patch\"]\n",
            alpha.display(),
            beta.display()
        ),
    )
    .unwrap();
    let (ok, text) = cli(&world, &config, &["sync"]);
    assert!(ok, "{text}");
    assert!(
        text.contains("warning: group 'g': !vendor/*.patch has no effect"),
        "{text}"
    );
    assert!(!beta.join("vendor").exists(), "{text}");
}

/// `diff` on a file inside two nested groups reads it under each group's
/// own root, so both sessions show their difference.
#[test]
fn diff_of_a_path_in_nested_groups_reads_it_under_each_root() {
    let world = World::new();
    let outer = world.directory("outer");
    let inner = outer.join("inner");
    let b1 = world.directory("b1");
    let b2 = world.directory("b2");
    write(&inner, "file.txt", "old");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[groups.outer]\nmode = \"one-way-alpha\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n\n\
             [groups.inner]\nmode = \"one-way-alpha\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
            outer.display(),
            b1.display(),
            inner.display(),
            b2.display()
        ),
    )
    .unwrap();
    let (ok, text) = cli(&world, &config, &["sync"]);
    assert!(ok, "{text}");
    write(&inner, "file.txt", "new");
    let file = inner.join("file.txt").to_string_lossy().to_string();
    let (_, text) = cli(&world, &config, &["diff", &file]);
    assert_eq!(text.matches("+old").count(), 2, "{text}");
    assert!(!text.contains("identical"), "{text}");
}
// ── clean and disabled sessions ──────────────────────────────────────

/// Turns a group off or on through the CLI.
fn set_group_enabled(config: &Path, group: &str, enabled: bool) {
    let output = std::process::Command::new(agent_binary())
        .arg(if enabled { "enable" } else { "disable" })
        .arg("--group")
        .arg(group)
        .arg("--config")
        .arg(config)
        .output()
        .expect("the CLI runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A group whose session has state, with a second, active group beside it
/// so the configuration still describes a session once the first is off.
fn two_groups(world: &World, mode: &str) -> (PathBuf, PathBuf, PathBuf) {
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    let other_alpha = world.directory("other-alpha");
    let other_beta = world.directory("other-beta");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[groups.g]\nmode = \"{mode}\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n\n\
             [groups.other]\nmode = \"two-way-conflict\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
            alpha.display(),
            beta.display(),
            other_alpha.display(),
            other_beta.display()
        ),
    )
    .unwrap();
    (config, alpha, beta)
}

#[test]
fn clean_keeps_a_disabled_sessions_state_so_enabling_resumes() {
    let world = World::new();
    let (config, alpha, beta) = two_groups(&world, "two-way-conflict");
    write(&alpha, "gone.txt", "content");
    assert!(cli(&world, &config, &["sync"]).0);
    assert_eq!(read(&beta, "gone.txt"), "content");

    // Deleted, then turned off before the deletion was carried.
    fs::remove_file(alpha.join("gone.txt")).unwrap();
    set_group_enabled(&config, "g", false);
    let (ok, text) = cli(&world, &config, &["clean"]);
    assert!(ok, "{text}");
    assert!(!text.contains("removed session"), "{text}");

    set_group_enabled(&config, "g", true);
    let (ok, text) = cli(&world, &config, &["sync"]);
    assert!(ok, "{text}");
    // The ancestor survived, so the deletion is carried, not undone.
    assert!(!alpha.join("gone.txt").exists(), "{text}");
    assert!(!beta.join("gone.txt").exists(), "{text}");
}

#[test]
fn clean_include_disabled_lists_the_disabled_session_and_plain_clean_does_not() {
    let world = World::new();
    let (config, alpha, _) = two_groups(&world, "two-way-conflict");
    write(&alpha, "file.txt", "content");
    assert!(cli(&world, &config, &["sync"]).0);
    set_group_enabled(&config, "g", false);

    let (ok, plain) = cli(&world, &config, &["clean", "--dry-run"]);
    assert!(ok, "{plain}");
    assert!(!plain.contains("would remove session"), "{plain}");

    let (ok, purge) = cli(
        &world,
        &config,
        &["clean", "--include-disabled", "--dry-run"],
    );
    assert!(ok, "{purge}");
    assert!(purge.contains("would remove session"), "{purge}");
    assert!(purge.contains("g@"), "{purge}");

    // Without a terminal to confirm on, the purge asks for --yes.
    let (ok, refused) = cli(&world, &config, &["clean", "--include-disabled"]);
    assert!(!ok, "{refused}");
    assert!(refused.contains("--yes"), "{refused}");
    let (ok, done) = cli(&world, &config, &["clean", "--include-disabled", "--yes"]);
    assert!(ok, "{done}");
    assert!(done.contains("removed session"), "{done}");
}

/// A disabled group whose settings no longer validate cannot be matched to
/// its state, so the state it may own is kept, and said to be.
#[test]
fn clean_keeps_state_it_cannot_attribute_to_a_broken_disabled_group() {
    let world = World::new();
    let (config, alpha, _) = two_groups(&world, "two-way-conflict");
    write(&alpha, "file.txt", "content");
    assert!(cli(&world, &config, &["sync"]).0);
    let text = fs::read_to_string(&config).unwrap().replacen(
        "mode = \"two-way-conflict\"",
        "disabled = true\nmode = \"no-such-mode\"",
        1,
    );
    fs::write(&config, text).unwrap();

    let (ok, output) = cli(&world, &config, &["clean", "--dry-run"]);
    assert!(ok, "{output}");
    assert!(!output.contains("would remove session"), "{output}");
    assert!(
        output.contains("could not tell what it belongs to"),
        "{output}"
    );
}
// ── conflicts, diff, resolve ─────────────────────────────────────────

/// Runs the CLI against a world's configuration file and state root.
fn cli(world: &World, config: &Path, args: &[&str]) -> (bool, String) {
    let output = std::process::Command::new(agent_binary())
        .args(args)
        .arg("--config")
        .arg(config)
        .arg("--state-root")
        .arg(world.state_root())
        .output()
        .expect("the CLI runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

/// A fan-out in conflict three ways: one file edited differently on alpha
/// and on each of two destinations.
fn three_way_conflict(world: &World) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let alpha = world.directory("alpha");
    let b1 = world.directory("b1");
    let b2 = world.directory("b2");
    write(&alpha, "notes.txt", "original");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[groups.r]\nmode = \"two-way-conflict\"\nalpha = \"{}\"\nbetas = [\"{}\", \"{}\"]\n",
            alpha.display(),
            b1.display(),
            b2.display()
        ),
    )
    .unwrap();
    assert!(cli(world, &config, &["sync"]).0, "the first sync converges");
    write(&alpha, "notes.txt", "v-alpha");
    write(&b1, "notes.txt", "v-b1");
    write(&b2, "notes.txt", "v-b2");
    cli(world, &config, &["sync"]);
    (config, alpha, b1, b2)
}

#[test]
fn conflicts_lists_every_side_with_what_it_holds() {
    let world = World::new();
    let (config, _, b1, b2) = three_way_conflict(&world);
    let (ok, text) = cli(&world, &config, &["conflicts"]);
    assert!(ok, "{text}");
    assert!(text.contains("notes.txt"), "{text}");
    assert!(text.contains(&b1.to_string_lossy().to_string()), "{text}");
    assert!(text.contains(&b2.to_string_lossy().to_string()), "{text}");
    // Both sides are described — the size is the proof the details
    // recorded at conflict time reached the listing.
    assert!(text.contains("alpha  7 B"), "{text}");
    assert!(text.contains("--keep alpha|"), "{text}");
}

#[test]
fn diff_shows_the_two_sides_by_group_or_by_file_path() {
    let world = World::new();
    let (config, alpha, _, b2) = three_way_conflict(&world);
    let b2_spec = b2.to_string_lossy().to_string();

    let (_, by_group) = cli(
        &world,
        &config,
        &["diff", "r", "notes.txt", "--host", &b2_spec],
    );
    assert!(
        by_group.contains("-v-alpha") && by_group.contains("+v-b2"),
        "{by_group}"
    );

    // Addressed by the file itself, from anywhere.
    let file = alpha.join("notes.txt").to_string_lossy().to_string();
    let (_, by_path) = cli(&world, &config, &["diff", &file, "--host", &b2_spec]);
    assert!(
        by_path.contains("-v-alpha") && by_path.contains("+v-b2"),
        "{by_path}"
    );
}

#[test]
fn resolve_keeping_one_destination_settles_the_whole_fan_out() {
    let world = World::new();
    let (config, alpha, b1, b2) = three_way_conflict(&world);
    let b1_spec = b1.to_string_lossy().to_string();
    // Addressed by a path inside the root, and keeping b1's version: it
    // must reach alpha *and* b2, whose own conflict is settled by it.
    let file = alpha.join("notes.txt").to_string_lossy().to_string();
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", &file, "--keep", &b1_spec, "--yes"],
    );
    assert!(ok, "{text}");
    // Resolution retires the losing versions; the cycle carries the winner.
    // Two cycles, because b2's copy reaches it through alpha.
    cli(&world, &config, &["sync"]);
    cli(&world, &config, &["sync"]);
    for root in [&alpha, &b1, &b2] {
        assert_eq!(read(root, "notes.txt"), "v-b1");
    }
    let (_, after) = cli(&world, &config, &["conflicts"]);
    assert!(after.contains("nothing needs you"), "{after}");
}

#[test]
fn resolve_keeping_both_renames_the_loser_aside() {
    let world = World::new();
    let (config, alpha, b1, b2) = three_way_conflict(&world);
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "notes.txt", "--keep", "both", "--yes"],
    );
    assert!(ok, "{text}");
    // The rename happens at once, because the losing content is only on
    // the losing side and nothing has to be moved to preserve it.
    assert_eq!(read(&b1, "notes.txt.b1"), "v-b1");
    assert_eq!(read(&b2, "notes.txt.b2"), "v-b2");
    // Everything else is ordinary propagation: alpha's version fills the
    // names the renames vacated, and each aside reaches the other roots.
    cli(&world, &config, &["sync"]);
    cli(&world, &config, &["sync"]);
    for root in [&alpha, &b1, &b2] {
        assert_eq!(read(root, "notes.txt"), "v-alpha");
        assert_eq!(read(root, "notes.txt.b1"), "v-b1");
        assert_eq!(read(root, "notes.txt.b2"), "v-b2");
    }
}

/// A conflict whose sides are not both files is the case that byte-copying
/// resolution could never settle: a directory has no content to read, and
/// "write no content" means removing it, which `remove_file` refuses. Every
/// winner is exercised, because each retires a different side.
#[test]
fn resolve_settles_a_conflict_between_a_directory_and_a_file() {
    for keep in ["alpha", "b1", "both"] {
        let world = World::new();
        let (config, alpha, b1, _) = three_way_conflict(&world);
        // `tree` is a populated directory on one side and a file on the
        // other, both created since the ancestor: neither change is a
        // deletion, so it is a genuine conflict rather than a propagation.
        // Large enough that a mistake would trip the emptied-subtree halt.
        let (directory, file) = match keep {
            "b1" => (&alpha, &b1),
            _ => (&b1, &alpha),
        };
        fs::create_dir_all(directory.join("tree/inner")).unwrap();
        for n in 0..9 {
            write(directory, &format!("tree/inner/f{n}"), "held");
        }
        write(file, "tree", "the file version");
        cli(&world, &config, &["sync"]);
        let (_, listed) = cli(&world, &config, &["conflicts"]);
        assert!(listed.contains("tree"), "a conflict at `tree`: {listed}");

        let winner = match keep {
            "b1" => b1.to_string_lossy().to_string(),
            other => other.to_owned(),
        };
        let (ok, text) = cli(
            &world,
            &config,
            &["resolve", "r", "tree", "--keep", &winner, "--yes"],
        );
        assert!(ok, "keeping {keep}: {text}");
        for _ in 0..3 {
            cli(&world, &config, &["sync"]);
        }

        // `notes.txt` is still conflicted — the fixture leaves it that way
        // — so the claim is about `tree` alone.
        let (_, after) = cli(&world, &config, &["conflicts"]);
        assert!(!after.contains("tree"), "keeping {keep}: {after}");
        match keep {
            // The file wins: the tree is gone from every root.
            "alpha" | "b1" => {
                assert_eq!(read(file, "tree"), "the file version");
                assert_eq!(read(directory, "tree"), "the file version");
                assert!(!directory.join("tree/inner").exists(), "the tree is gone");
            }
            // Both are kept: the loser's whole tree survives under a free
            // name, which is the thing a rename can do and a copy cannot.
            _ => {
                assert_eq!(read(&alpha, "tree"), "the file version");
                assert_eq!(read(&alpha, "tree.b1/inner/f0"), "held");
                assert_eq!(read(&b1, "tree.b1/inner/f8"), "held");
            }
        }
    }
}

/// Ignored content is invisible, so it must not stand in the way of an
/// ordinary deletion. Nearly every project directory holds a `.git` or a
/// `node_modules`, and while excluded content blocked deletions none of
/// them could be deleted through synchronization at all: the deletion
/// became a conflict that no resolution could settle.
#[test]
fn deleting_a_project_propagates_even_though_it_holds_ignored_content() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("b1");
    write(&alpha, "seed", "seed");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[defaults]\nmode = \"two-way-conflict\"\nignores = [\".git\", \"node_modules\"]\n\
             [groups.r]\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
            alpha.display(),
            beta.display()
        ),
    )
    .unwrap();
    assert!(cli(&world, &config, &["sync"]).0);

    write(&beta, "project/.git/HEAD", "ref");
    write(&beta, "project/node_modules/dep.js", "dep");
    write(&beta, "project/src/main.rs", "fn main() {}");
    cli(&world, &config, &["sync"]);
    assert_eq!(read(&alpha, "project/src/main.rs"), "fn main() {}");
    assert!(!alpha.join("project/.git").exists(), "ignored, so not sent");

    fs::remove_dir_all(alpha.join("project")).unwrap();
    let (_, first) = cli(&world, &config, &["sync"]);

    // The whole tree goes, ignored content included. An ignore says which
    // files synchronization *carries*, not which files exist; deleting a
    // directory is an instruction about the directory, and taking the
    // source while leaving the `.git` and the `node_modules` obeys
    // neither reading — the tree is not deleted, and what stays is litter
    // synchronization can never clear.
    assert!(
        !beta.join("project").exists(),
        "the tree evaporated: {first}"
    );

    // And nothing is reported as an obstacle on the way.
    assert!(!first.contains("blocked"), "{first}");

    for _ in 0..3 {
        let (_, text) = cli(&world, &config, &["sync"]);
        assert!(text.contains("0 change(s) to alpha"), "quiet: {text}");
        assert!(text.contains("0 change(s) to beta"), "quiet: {text}");
    }
    let (_, issues) = cli(&world, &config, &["conflicts"]);
    assert!(issues.contains("nothing needs you"), "{issues}");
}

/// The one case where deleting an ignored path costs something nobody
/// agreed to: the ignored path is *another session's root*. Group A never
/// looked inside it, so its deletion takes the tree whole — and the second
/// session then finds its root gone.
///
/// It must stop there. The copy inside group A's tree is the deletion that
/// was asked for; the copy on the far side of the nested session is not,
/// and no session may carry a loss it did not originate.
#[test]
fn a_nested_session_halts_when_an_ignored_path_holding_its_root_is_deleted() {
    let world = World::new();
    let outer_alpha = world.directory("outer-alpha");
    let outer_beta = world.directory("outer-beta");
    let inner_beta = world.directory("inner-beta");

    let outer = world.path("outer.toml");
    fs::write(
        &outer,
        format!(
            "[groups.outer]\nmode = \"two-way-conflict\"\nignores = [\"nested\"]\n\
             alpha = \"{}\"\nbetas = [\"{}\"]\n",
            outer_alpha.display(),
            outer_beta.display()
        ),
    )
    .unwrap();
    // The inner session's root lives inside the outer session's ignored
    // path, which is the only reason the outer session may delete it.
    let inner = world.path("inner.toml");
    fs::write(
        &inner,
        format!(
            "[groups.inner]\nmode = \"two-way-conflict\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
            outer_beta.join("proj/nested").display(),
            inner_beta.display()
        ),
    )
    .unwrap();

    // A sibling, so deleting `proj` is not also emptying the root — that
    // trips a different guard and would prove nothing about this one.
    write(&outer_alpha, "other.txt", "keep");
    write(&outer_alpha, "proj/src/main.rs", "code");
    assert!(cli(&world, &outer, &["sync"]).0);
    write(&outer_beta, "proj/nested/data.txt", "precious");
    assert!(cli(&world, &inner, &["sync"]).0);
    assert_eq!(read(&inner_beta, "data.txt"), "precious");

    fs::remove_dir_all(outer_alpha.join("proj")).unwrap();
    let (ok, text) = cli(&world, &outer, &["sync"]);
    assert!(ok, "{text}");
    // The tree went whole, the nested root with it.
    assert!(!outer_beta.join("proj").exists(), "{text}");

    // The nested session refuses to carry that loss any further.
    let (ok, text) = cli(&world, &inner, &["sync"]);
    assert!(!ok, "the nested session must not succeed: {text}");
    assert!(
        text.contains("halted") && text.contains("is missing"),
        "{text}"
    );
    assert_eq!(read(&inner_beta, "data.txt"), "precious");
}

/// A conflict whose losing side holds ignored content settles like any
/// other. The removal takes what synchronization knows about and leaves
/// the rest, and what remains is invisible — the same rule the cycle
/// follows for an ordinary deletion. `resolve` must not be stricter than
/// the cycle it stands in for.
#[test]
fn resolve_settles_a_conflict_whose_loser_holds_ignored_content() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("b1");
    write(&alpha, "seed", "seed");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[defaults]\nmode = \"two-way-conflict\"\nignores = [\".git\"]\n\
             [groups.r]\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
            alpha.display(),
            beta.display()
        ),
    )
    .unwrap();
    assert!(cli(&world, &config, &["sync"]).0);

    // A genuine disagreement, not a deletion: a file on one side and a
    // project directory on the other, both new since the ancestor.
    write(&alpha, "project", "alpha's file");
    write(&beta, "project/.git/HEAD", "ref");
    write(&beta, "project/src/main.rs", "fn main() {}");
    cli(&world, &config, &["sync"]);
    let (_, listed) = cli(&world, &config, &["conflicts"]);
    assert!(listed.contains("project"), "a conflict: {listed}");

    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "project", "--keep", "alpha", "--yes"],
    );
    assert!(ok, "{text}");
    assert!(text.contains("settled 1 of 1"), "{text}");
    for _ in 0..3 {
        cli(&world, &config, &["sync"]);
    }

    // Alpha's version won everywhere, and the loser's tree went whole —
    // resolution follows the same rule the cycle does, so a `.git` inside
    // the losing version is no more of an obstacle here than there.
    assert_eq!(read(&alpha, "project"), "alpha's file");
    assert_eq!(read(&beta, "project"), "alpha's file");
    let (_, after) = cli(&world, &config, &["conflicts"]);
    assert!(after.contains("nothing needs you"), "{after}");
}

/// A group of one alpha and one beta in `mode`, holding `keep.txt` and a
/// sibling (so no guard about emptied roots is in play), synchronized once.
fn one_pair(world: &World, mode: &str) -> (PathBuf, PathBuf, PathBuf) {
    let alpha = world.directory("alpha");
    let beta = world.directory("b1");
    write(&alpha, "keep.txt", "original");
    write(&alpha, "other.txt", "other");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[groups.r]\nmode = \"{mode}\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
            alpha.display(),
            beta.display()
        ),
    )
    .unwrap();
    assert!(cli(world, &config, &["sync"]).0, "the first sync converges");
    assert_eq!(read(&beta, "keep.txt"), "original");
    (config, alpha, beta)
}

/// Resolving a path both sides already agree on retires nothing. Before
/// the guard it retired beta's copy, and the next cycle read that as a
/// deletion against an unchanged alpha and took the file from both sides.
#[test]
fn resolving_an_in_sync_path_twice_keeps_it_everywhere() {
    let world = World::new();
    let (config, alpha, beta) = one_pair(&world, "two-way-conflict");
    for _ in 0..2 {
        let (ok, text) = cli(
            &world,
            &config,
            &["resolve", "r", "keep.txt", "--keep", "alpha", "--yes"],
        );
        assert!(ok, "{text}");
        assert!(text.contains("already the same on every side"), "{text}");
        cli(&world, &config, &["sync"]);
    }
    cli(&world, &config, &["sync"]);
    assert_eq!(read(&alpha, "keep.txt"), "original");
    assert_eq!(read(&beta, "keep.txt"), "original");
}

/// The root is never a path to resolve: retiring it would retire the
/// whole synchronizable tree.
#[test]
fn resolving_the_root_is_refused() {
    let world = World::new();
    let (config, alpha, beta) = one_pair(&world, "two-way-conflict");
    write(&beta, "keep.txt", "beta's edit");
    for root in ["./", ".", ""] {
        let (ok, text) = cli(
            &world,
            &config,
            &["resolve", "r", root, "--keep", "alpha", "--yes"],
        );
        assert!(!ok, "resolving {root:?} must fail: {text}");
        assert!(text.contains("not the root"), "{text}");
    }
    assert_eq!(read(&alpha, "keep.txt"), "original");
    assert_eq!(read(&beta, "keep.txt"), "beta's edit");
    assert_eq!(read(&beta, "other.txt"), "other");
}

/// Keeping a side that has not changed since the last sync would delete
/// it: removing the other copy reads as a deletion against an untouched
/// file, and a deletion against an untouched file propagates.
#[test]
fn keeping_an_unchanged_side_is_refused_as_a_deletion() {
    let world = World::new();
    let (config, alpha, beta) = one_pair(&world, "two-way-conflict");
    write(&beta, "keep.txt", "beta's edit");
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "keep.txt", "--keep", "alpha", "--yes"],
    );
    assert!(ok, "{text}");
    assert!(
        text.contains("keeping alpha here would delete it"),
        "{text}"
    );
    assert!(text.contains("settled 0 of 1"), "{text}");
    assert_eq!(read(&alpha, "keep.txt"), "original");
    assert_eq!(read(&beta, "keep.txt"), "beta's edit");
    // The same holds for `--keep both`: alpha's copy would be deleted by
    // the rename aside of beta's.
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "keep.txt", "--keep", "both", "--yes"],
    );
    assert!(ok, "{text}");
    assert!(
        text.contains("keeping alpha here would delete it"),
        "{text}"
    );
    assert_eq!(read(&beta, "keep.txt"), "beta's edit");
    assert!(!beta.join("keep.txt.b1").exists());
}

/// A one-way mode never carries beta's content to alpha, so retiring
/// alpha's copy cannot make beta's version win.
#[test]
fn keeping_beta_in_a_one_way_mode_is_refused() {
    let world = World::new();
    let (config, alpha, beta) = one_pair(&world, "one-way-alpha");
    write(&alpha, "keep.txt", "alpha's edit");
    let beta_spec = beta.to_string_lossy().to_string();
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "keep.txt", "--keep", &beta_spec, "--yes"],
    );
    assert!(!ok, "{text}");
    assert!(text.contains("one-way-alpha"), "{text}");
    assert_eq!(read(&alpha, "keep.txt"), "alpha's edit");
    assert_eq!(read(&beta, "keep.txt"), "original");
}

/// In two-way-alpha-strict alpha's deletion beats beta's edit, so
/// retiring alpha's copy to keep beta's would delete beta's too.
#[test]
fn keeping_beta_in_the_strict_mode_is_refused() {
    let world = World::new();
    let (config, alpha, beta) = one_pair(&world, "two-way-alpha-strict");
    write(&beta, "keep.txt", "beta's edit");
    let beta_spec = beta.to_string_lossy().to_string();
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "keep.txt", "--keep", &beta_spec, "--yes"],
    );
    assert!(ok, "{text}");
    assert!(text.contains("two-way-alpha-strict"), "{text}");
    assert!(text.contains("settled 0 of 1"), "{text}");
    assert_eq!(read(&alpha, "keep.txt"), "original");
    assert_eq!(read(&beta, "keep.txt"), "beta's edit");
}

/// A file named `--all`, settled the way the shop and the tray settle a
/// row, settles that file and nothing else. Passed without a separator,
/// the name was parsed as the flag and every conflict in the group was
/// settled.
#[test]
fn settling_a_file_named_like_a_flag_settles_only_that_file() {
    let world = World::new();
    let (config, alpha, b1, _) = three_way_conflict(&world);
    write(&alpha, "--all", "a");
    write(&b1, "--all", "b");
    cli(&world, &config, &["sync"]);
    let (_, listed) = cli(&world, &config, &["conflicts"]);
    assert!(listed.contains("--all"), "{listed}");

    let command = autobahn::invocation::resolve_command("r", "alpha", &["--all".to_owned()]);
    let command = autobahn::invocation::with_options(
        &command,
        &[
            "--config".to_owned(),
            config.to_string_lossy().into_owned(),
            "--state-root".to_owned(),
            world.state_root().to_string_lossy().into_owned(),
        ],
    );
    let output = std::process::Command::new(agent_binary())
        .args(&command)
        .output()
        .expect("the CLI runs");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    cli(&world, &config, &["sync"]);
    assert_eq!(read(&b1, "--all"), "a");
    // The other conflict is untouched.
    assert_eq!(read(&b1, "notes.txt"), "v-b1");
    let (_, after) = cli(&world, &config, &["conflicts"]);
    assert!(after.contains("notes.txt"), "{after}");
}

/// Removes the colour sequences autobahn writes itself (`ESC [ … m`), so
/// what is left can be checked for control characters that came from a
/// name.
fn without_colour(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("\x1b[") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let end = tail.find(|c: char| !(c.is_ascii_digit() || c == ';'));
        match end {
            Some(end) if tail[end..].starts_with('m') => rest = &tail[end + 1..],
            _ => {
                out.push_str("\x1b[");
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A name chosen by the other side reaches the terminal escaped. Printed
/// raw, OSC 52 writes the reader's clipboard, and CR or CSI sequences
/// repaint what `status` and `conflicts` appear to say. `--json` keeps the
/// name exactly.
#[test]
fn a_name_with_control_characters_is_printed_escaped() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("b1");
    write(&alpha, "seed", "seed");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[groups.r]\nmode = \"two-way-conflict\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
            alpha.display(),
            beta.display()
        ),
    )
    .unwrap();
    assert!(cli(&world, &config, &["sync"]).0);
    let name = "evil\x1b]52;c;cHduZWQ=\x07\r\x1b[2Jsettled.txt";
    write(&alpha, name, "a");
    write(&beta, name, "b");
    cli(&world, &config, &["sync"]);

    for args in [&["status", "--all", "--conflicts"][..], &["conflicts"][..]] {
        let (_, text) = cli(&world, &config, args);
        assert!(text.contains("evil\\x1b]52;c;"), "{args:?}: {text:?}");
        let bare = without_colour(&text);
        assert!(
            !bare.contains(|c: char| c.is_control() && c != '\n'),
            "{args:?}: {bare:?}"
        );
    }

    let (ok, json) = cli(&world, &config, &["conflicts", "--json"]);
    assert!(ok, "{json}");
    let report: serde_json::Value = serde_json::from_str(&json).expect("json");
    let path = &report["groups"][0]["sessions"][0]["conflicts"][0]["path"];
    assert_eq!(path.as_str(), Some(name), "{json}");
}

/// A report read by a script is text: `status` and `conflicts` written to
/// a pipe carry no escape codes, and the same words.
#[test]
fn a_report_to_a_pipe_carries_no_escape_codes() {
    let world = World::new();
    let (config, _, _, _) = three_way_conflict(&world);
    for args in [&["status", "--all"][..], &["conflicts"][..]] {
        let (_, text) = cli(&world, &config, args);
        assert!(
            text.contains("notes.txt") || text.contains("conflicts"),
            "{text}"
        );
        assert!(!text.contains('\x1b'), "{args:?}: {text:?}");
    }
}

/// On a terminal, `status` is coloured — unless `NO_COLOR` asks otherwise,
/// which takes the colour and leaves the words. Run through a
/// pseudo-terminal, by `script`, so the terminal check is the real one.
#[cfg(target_os = "linux")]
#[test]
fn no_color_on_a_terminal_takes_the_colour_away() {
    if std::process::Command::new("script")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: no `script` to make a pseudo-terminal with");
        return;
    }
    let world = World::new();
    let (config, _, _, _) = three_way_conflict(&world);
    let on_a_terminal = |no_color: Option<&str>| {
        let line = format!(
            "{} status --all --config '{}' --state-root '{}'",
            agent_binary(),
            config.display(),
            world.state_root().display()
        );
        let mut command = std::process::Command::new("script");
        command
            .args(["-qec", &line, "/dev/null"])
            .env("TERM", "xterm")
            .env_remove("NO_COLOR");
        if let Some(value) = no_color {
            command.env("NO_COLOR", value);
        }
        let output = command.output().expect("script runs");
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let coloured = on_a_terminal(None);
    assert!(coloured.contains("\x1b[33m"), "{coloured:?}");
    let plain = on_a_terminal(Some("1"));
    assert!(plain.contains("conflicts"), "{plain:?}");
    for colour in ["\x1b[31m", "\x1b[32m", "\x1b[33m"] {
        assert!(!plain.contains(colour), "{plain:?}");
    }
    // Emphasis is not colour, and stays.
    assert!(plain.contains("\x1b[1m"), "{plain:?}");
}

/// Keeping one destination's version where alpha has no copy at all:
/// alpha's side has nothing to retire, and the other destination must
/// still be checked as though alpha already holds the winner's version,
/// which is what it will hold once the winning session has run.
#[test]
fn keeping_a_destination_where_alpha_has_no_copy_settles_the_fan_out() {
    let world = World::new();
    let (config, alpha, b1, b2) = three_way_conflict(&world);
    write(&alpha, "fresh.txt", "seed");
    cli(&world, &config, &["sync"]);
    fs::remove_file(alpha.join("fresh.txt")).unwrap();
    write(&b1, "fresh.txt", "v-b1");
    write(&b2, "fresh.txt", "v-b2");
    let b1_spec = b1.to_string_lossy().to_string();
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "fresh.txt", "--keep", &b1_spec, "--yes"],
    );
    assert!(ok, "{text}");
    assert!(!text.contains("not settled"), "{text}");
    for _ in 0..3 {
        cli(&world, &config, &["sync"]);
    }
    for root in [&alpha, &b1, &b2] {
        assert_eq!(read(root, "fresh.txt"), "v-b1");
    }
}

/// A folder and a path inside it, named together, are one resolution.
#[test]
fn naming_a_folder_and_a_path_inside_it_settles_the_folder_once() {
    let world = World::new();
    let (config, alpha, beta) = one_pair(&world, "two-way-conflict");
    write(&alpha, "d/f", "alpha");
    write(&beta, "d/f", "beta");
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "d", "d/f", "--keep", "alpha", "--yes"],
    );
    assert!(ok, "{text}");
    assert!(text.contains("settled 1 of 1"), "{text}");
    for _ in 0..2 {
        cli(&world, &config, &["sync"]);
    }
    assert_eq!(read(&beta, "d/f"), "alpha");
}

#[test]
fn resolve_all_requires_a_winner_and_asks_first() {
    let world = World::new();
    let (config, alpha, b1, _) = three_way_conflict(&world);
    write(&alpha, "more.txt", "a");
    write(&b1, "more.txt", "b");
    cli(&world, &config, &["sync"]);
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "--all", "--keep", "both"],
    );
    assert!(!ok && text.contains("choose whose version wins"), "{text}");
    // Without --yes and with no terminal to answer, it refuses rather than
    // guessing: a resolution overwrites work someone did deliberately, so
    // an unattended run must say what to pass, not quietly do nothing.
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "--all", "--keep", "alpha"],
    );
    assert!(!ok && text.contains("pass --yes"), "{text}");
    assert_eq!(read(&b1, "more.txt"), "b");
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "--all", "--keep", "alpha", "--yes"],
    );
    assert!(ok, "{text}");
    cli(&world, &config, &["sync"]);
    assert_eq!(read(&b1, "more.txt"), "a");
    assert_eq!(read(&b1, "notes.txt"), "v-alpha");
}

#[test]
fn a_group_name_wins_over_a_directory_of_the_same_name() {
    // The selector is a group name unless it carries a path marker; a bare
    // word that happens to also name a directory in the working directory
    // must still select the group.
    let world = World::new();
    let (config, _, _, _) = three_way_conflict(&world);
    let decoy = world.directory("r");
    let output = std::process::Command::new(agent_binary())
        .current_dir(decoy.parent().unwrap())
        .args(["conflicts", "r", "--config"])
        .arg(&config)
        .arg("--state-root")
        .arg(world.state_root())
        .output()
        .expect("runs");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("notes.txt"), "{text}");
}

/// A supervised session says what it is doing, and leaves behind the totals
/// that let the next scan be estimated rather than merely timed.
///
/// This is the live half of status. Without it, a session in the middle of a
/// long first scan is described entirely by what preceded that scan —
/// commonly an error, under an age that only grows — and reads as stuck.
#[test]
fn a_supervised_session_reports_what_it_is_doing() {
    use autobahn::progress::Phase;
    use autobahn::supervisor::control;

    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    for index in 0..64 {
        write(&alpha, &format!("file{index:02}.txt"), "content");
    }

    let plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        interval = 1
        betas = ["{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
    ));
    let plan = plans[0].clone();

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let supervisor = Supervisor::new(plans, world.state_root(), false);
        let stop_ref = &stop;
        let watcher = scope.spawn(move || supervisor.run_watch(stop_ref));
        let _guard = StopGuard(stop_ref);

        assert!(
            wait_until(Duration::from_secs(20), || beta.join("file00.txt").exists()),
            "the initial content should synchronize"
        );

        // Every supervised session is reported, whatever it is doing.
        assert!(
            wait_until(Duration::from_secs(10), || control::query_progress(
                &world.state_root()
            )
            .is_some()),
            "a running supervisor reports progress"
        );
        let live = control::query_progress(&world.state_root()).expect("progress is reported");
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].group, "work");

        // Once the pair settles, the session is waiting — and a waiting
        // session is described by its recorded status, not by a phase.
        assert!(
            wait_until(Duration::from_secs(20), || {
                control::query_progress(&world.state_root())
                    .is_some_and(|live| live[0].progress.phase == Phase::Waiting)
            }),
            "a settled session waits"
        );
        assert!(!Phase::Waiting.is_working());

        // Both sides have completed a scan, so both left a total behind.
        // These are what a later scan is measured against; without them
        // there is no honest estimate, only elapsed time.
        let live = control::query_progress(&world.state_root()).expect("progress is reported");
        let alpha_entries = live[0].progress.alpha.expected.expect("alpha has a total");
        let beta_entries = live[0].progress.beta.expected.expect("beta has a total");
        assert!(
            alpha_entries >= 65,
            "the total counts the tree: {alpha_entries}"
        );
        assert_eq!(
            alpha_entries, beta_entries,
            "synchronized trees hold the same number of entries"
        );

        // The totals are recorded alongside the status, so the next run's
        // first scan starts with a yardstick rather than without one.
        let status = world.status(&plan).expect("a status is recorded");
        assert_eq!(status.alpha_entries, alpha_entries);
        assert_eq!(status.beta_entries, beta_entries);

        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });
}

/// A supervisor running unattended tells someone when a session needs them.
///
/// The end-to-end path: a session goes into conflict, the condition holds
/// for its confirmation period, and the configured command runs with the
/// summary in its environment and the status document on its input. And —
/// the property that makes the feature usable rather than infuriating — a
/// healthy supervisor runs nothing at all.
#[test]
fn a_session_needing_attention_runs_the_configured_hook() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    let evidence = world.directory("evidence");
    let fired = evidence.join("fired");

    let configuration = format!(
        r#"
        on_alert = "cat > {fired}.stdin; printf '%s' \"$AUTOBAHN_SUMMARY|$AUTOBAHN_STATES|$AUTOBAHN_EVENT\" > {fired}"

        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-conflict"
        interval = 1
        betas = ["{beta}"]

        [advanced.alerts]
        alert_after = "1s"
        # This case is about the hook running at all. Coalescing has its
        # own tests; without this the window would hold the hook for a
        # minute and the case would be timing out on the wrong rule.
        coalesce_after = "0s"
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
        fired = fired.display(),
    );
    let plans = world.plans(&configuration);
    let alerts = toml::from_str::<Config>(&configuration)
        .expect("the configuration parses")
        .alert_plan()
        .expect("the alert plan resolves");

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let supervisor = Supervisor::new(plans, world.state_root(), false).with_alerts(alerts);
        let stop_ref = &stop;
        let watcher = scope.spawn(move || supervisor.run_watch(stop_ref));
        let _guard = StopGuard(stop_ref);

        // Healthy: the hook must not run. Silence is the normal state, and
        // a tool that announces its own good health is one people mute.
        write(&alpha, "shared.txt", "from alpha");
        assert!(
            wait_until(Duration::from_secs(20), || beta.join("shared.txt").exists()),
            "the initial content should synchronize"
        );
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            !fired.exists(),
            "a healthy supervisor must run nothing at all"
        );

        // Now make the two sides disagree about the same file.
        write(&alpha, "shared.txt", "alpha's version");
        write(&beta, "shared.txt", "beta's version");

        assert!(
            wait_until(Duration::from_secs(30), || fired.exists()),
            "a session in conflict should run the hook"
        );
        // Give the hook's writes a moment to land in full.
        std::thread::sleep(Duration::from_millis(500));

        let reported = fs::read_to_string(&fired).expect("the hook wrote its environment");
        let fields: Vec<&str> = reported.split('|').collect();
        assert!(
            fields[0].contains("work → ") && fields[0].contains("1 conflict"),
            "the summary names the session, source to destination, and what is \
             wrong — in the singular, for one file: {reported}"
        );
        assert_eq!(fields[1], "conflicts", "the states are listed: {reported}");
        assert_eq!(fields[2], "alert", "the event is an alert: {reported}");

        // The full report arrives on standard input — the same document
        // `status --json` prints, rather than a second one invented for
        // hooks.
        let document =
            fs::read_to_string(fired.with_extension("stdin")).expect("the hook read its input");
        let report: serde_json::Value =
            serde_json::from_str(&document).expect("the document is the JSON report");
        assert!(report["version"].is_number());
        assert_eq!(report["groups"][0]["name"], "work");

        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });
}

/// A wrapper that runs the agent under its own home directory, so the
/// agent's `~/.autobahn/peering` is not the leader's: in production they
/// are on different machines, and the leader's own peering directory holds
/// its term while the agent's holds the lease it was given.
fn peering_agent_script(world: &World, agent_home: &Path) -> PathBuf {
    let script = world.path("peering-agent.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nHOME={home} exec {agent} agent\n",
            home = agent_home.display(),
            agent = agent_binary()
        ),
    )
    .expect("script should be writable");
    let mut permissions = fs::metadata(&script).expect("script").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    fs::set_permissions(&script, permissions).expect("script should be executable");
    script
}

/// Peering, phase 3: a leading supervisor presents its lease, pushes the
/// follower's files, and keeps the beta's ancestor copy level — all of it
/// visible on the beta's host afterwards.
#[test]
fn a_peering_leader_pushes_its_lease_files_and_ancestor_to_the_beta() {
    use autobahn::supervisor::PeeringContext;

    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    let agent_home = world.directory("agent-home");
    write(&alpha, "hello.txt", "hello");
    let script = peering_agent_script(&world, &agent_home);
    let configuration = format!(
        r#"
        [groups.g]
        mode = "peering-conflict-dangerously-experimental"
        alpha = "{alpha}"
        agent_command = "{script}"
        betas = ["peer:{beta}"]
        "#,
        alpha = alpha.display(),
        script = script.display(),
        beta = beta.display(),
    );
    let plans = world.plans(&configuration);
    let plan = plans[0].clone();
    let leader_directory = world.path("leader-peering");
    let context = || {
        PeeringContext::for_alpha(world.path("config.toml"), leader_directory.clone())
            .expect("a peering context")
    };

    let outcomes = Supervisor::new(plans.clone(), world.state_root(), false)
        .with_peering(context())
        .run_once();
    assert_all_synchronized(&outcomes);
    assert_eq!(read(&beta, "hello.txt"), "hello");

    // The beta's host now holds everything a follower needs.
    let peering = agent_home.join(".autobahn").join("peering");
    let lease = autobahn::peering::read_lease(&peering)
        .expect("lease readable")
        .expect("a lease was written");
    assert_eq!((lease.leader.as_str(), lease.term), ("alpha", 1));
    assert_eq!(
        fs::read_to_string(peering.join("name")).expect("name"),
        plan.beta_spec()
    );
    assert_eq!(
        fs::read_to_string(peering.join("config.toml")).expect("config"),
        configuration
    );
    let copy = autobahn::peering::ancestor_copy_path(&peering, &plan.identifier())
        .expect("a plan's identifier is a session identifier");
    assert!(
        copy.exists(),
        "the ancestor copy exists at {}",
        copy.display()
    );

    // The leader remembers its own term, and the status says what it is.
    let own = autobahn::peering::read_lease(&leader_directory)
        .expect("lease readable")
        .expect("the leader's own lease");
    assert_eq!((own.leader.as_str(), own.term), ("alpha", 1));
    let status = world.status(&plan).expect("a status");
    assert_eq!((status.role.as_str(), status.term), ("leader", 1));
    assert_eq!(status.state, "synchronized");

    // A change on the alpha reaches the beta, and the copy follows the
    // ancestor: it stands at the same generation the leader does.
    write(&alpha, "more.txt", "more");
    let outcomes = Supervisor::new(plans, world.state_root(), false)
        .with_peering(context())
        .run_once();
    assert_all_synchronized(&outcomes);
    assert_eq!(read(&beta, "more.txt"), "more");
    let lease = autobahn::peering::read_lease(&peering)
        .expect("lease readable")
        .expect("renewed");
    assert_eq!(lease.term, 1, "the same leader keeps its term");
}

/// Peering, phase 3: a host whose lease names a newer leader refuses the
/// old one, which steps down before a byte moves and stays down across a
/// restart.
#[test]
fn a_fenced_peering_leader_steps_down_and_stays_down() {
    use autobahn::peering::{write_lease, Lease};
    use autobahn::supervisor::PeeringContext;
    use std::time::Duration;

    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    let agent_home = world.directory("agent-home");
    write(&alpha, "hello.txt", "hello");
    let script = peering_agent_script(&world, &agent_home);
    let plans = world.plans(&format!(
        r#"
        [groups.g]
        mode = "peering-conflict-dangerously-experimental"
        alpha = "{alpha}"
        agent_command = "{script}"
        betas = ["peer:{beta}"]
        "#,
        alpha = alpha.display(),
        script = script.display(),
        beta = beta.display(),
    ));
    let plan = plans[0].clone();
    // The beta led at term 9 while the alpha was away.
    let peering = agent_home.join(".autobahn").join("peering");
    write_lease(
        &peering,
        &Lease::new(&plan.beta_spec(), 9, Duration::from_secs(30)),
    )
    .expect("the beta's lease");

    let leader_directory = world.path("leader-peering");
    let outcomes = Supervisor::new(plans.clone(), world.state_root(), false)
        .with_peering(
            PeeringContext::for_alpha(world.path("config.toml"), leader_directory.clone())
                .expect("a peering context"),
        )
        .run_once();
    assert!(outcomes[0].result.is_err(), "{:?}", outcomes[0].result);
    assert!(
        !beta.join("hello.txt").exists(),
        "a fenced leader writes nothing"
    );
    let status = world.status(&plan).expect("a status");
    assert_eq!(status.state, "following", "{status:?}");
    assert_eq!((status.role.as_str(), status.term), ("follower", 9));
    // The beta's lease is untouched, and the alpha recorded it as its own.
    let held = autobahn::peering::read_lease(&peering)
        .expect("readable")
        .expect("held");
    assert_eq!(
        (held.leader.as_str(), held.term),
        (plan.beta_spec().as_str(), 9)
    );
    let own = autobahn::peering::read_lease(&leader_directory)
        .expect("readable")
        .expect("recorded");
    assert_eq!(own.term, 9);

    // A restart reads its own lease and comes back as a follower: it does
    // not connect, and the beta is still untouched.
    let context = PeeringContext::for_alpha(world.path("config.toml"), leader_directory)
        .expect("a peering context");
    assert!(matches!(
        context.role(),
        autobahn::peering::Role::Follower { term: 9, .. }
    ));
    let outcomes = Supervisor::new(plans, world.state_root(), false)
        .with_peering(context)
        .run_once();
    assert!(outcomes[0].result.is_err());
    assert!(!beta.join("hello.txt").exists());
    assert_eq!(world.status(&plan).expect("a status").state, "following");
}

/// Peering, phase 4: a peer whose lease has been stale for its wait takes
/// the lead at the next term, runs the leader's star turned around, and
/// reaches the other beta; the old leader, back at its old term, is fenced.
#[test]
fn a_peer_takes_the_lead_when_the_lease_goes_stale() {
    use autobahn::peering::{self, Lease};
    use autobahn::supervisor::PeeringContext;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    let world = World::new();
    // Three machines: the alpha (never dialed here), this peer, and one
    // other beta. Each beta has its own home, so its agent's peering
    // directory is its own.
    let peer_root = world.directory("peer-root");
    let peer_home = world.directory("peer-home");
    let other_root = world.directory("other-root");
    let other_home = world.directory("other-home");
    write(&peer_root, "from-peer.txt", "from the peer");
    let other_script = world.path("other-agent.sh");
    fs::write(
        &other_script,
        format!(
            "#!/bin/sh\nHOME={home} exec {agent} agent\n",
            home = other_home.display(),
            agent = agent_binary()
        ),
    )
    .expect("script");
    let mut permissions = fs::metadata(&other_script).expect("script").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    fs::set_permissions(&other_script, permissions).expect("executable");

    // What the alpha pushed to this peer: its own star, with a lease
    // lifetime and a wait short enough for a test.
    let pushed = format!(
        r#"
        [advanced.peering-dangerously-experimental]
        ttl = "2s"
        failover_after = "2s"

        [groups.g]
        mode = "peering-conflict-dangerously-experimental"
        interval = 1
        alpha = "/nonexistent/alpha"
        agent_command = "{script}"
        betas = ["peer:{peer_root}", "other:{other_root}"]
        "#,
        script = other_script.display(),
        peer_root = peer_root.display(),
        other_root = other_root.display(),
    );
    let name = format!("peer:{}", peer_root.display());
    let peering_directory = peer_home.join(".autobahn").join("peering");
    peering::write_pushed_file(&peering_directory, "config.toml", pushed.as_bytes()).unwrap();
    peering::write_pushed_file(&peering_directory, "name", name.as_bytes()).unwrap();
    // The alpha's lease, last renewed a while ago.
    let stale = Lease {
        leader: peering::ALPHA.to_owned(),
        term: 3,
        renewed_at: peering::now_seconds().saturating_sub(120),
        ttl_seconds: 1,
    };
    peering::write_lease(&peering_directory, &stale).unwrap();

    let stop = AtomicBool::new(false);
    let state_root = world.state_root();
    std::thread::scope(|scope| {
        let _guard = StopGuard(&stop);
        let peer = scope.spawn(|| {
            autobahn::supervisor::peer::run(&peering_directory, &state_root, false, &stop)
        });
        // The peer takes the lead and its file reaches the other beta.
        assert!(
            wait_until(Duration::from_secs(20), || other_root
                .join("from-peer.txt")
                .exists()),
            "the peer's file should reach the other beta"
        );
        let own = peering::read_lease(&peering_directory)
            .expect("readable")
            .expect("written");
        assert_eq!((own.leader.as_str(), own.term), (name.as_str(), 4));
        let theirs = other_home.join(".autobahn").join("peering");
        assert!(
            wait_until(Duration::from_secs(10), || {
                peering::read_lease(&theirs)
                    .ok()
                    .flatten()
                    .is_some_and(|lease| lease.term == 4 && lease.leader == name)
            }),
            "the other beta holds the peer's lease"
        );
        assert_eq!(
            fs::read_to_string(theirs.join("name")).expect("name pushed on"),
            format!("other:{}", other_root.display())
        );
        assert_eq!(
            fs::read_to_string(theirs.join("config.toml")).expect("config pushed on"),
            pushed
        );

        // The old leader comes back at its old term and is fenced on the
        // other beta: it steps down, writes nothing.
        let old_alpha = world.directory("alpha-root");
        write(&old_alpha, "from-alpha.txt", "late");
        let plans = world.plans(&format!(
            r#"
            [groups.g]
            mode = "peering-conflict-dangerously-experimental"
            alpha = "{alpha}"
            agent_command = "{script}"
            betas = ["other:{other_root}"]
            "#,
            alpha = old_alpha.display(),
            script = other_script.display(),
            other_root = other_root.display(),
        ));
        let alpha_directory = world.path("alpha-peering");
        peering::write_lease(
            &alpha_directory,
            &Lease::new(peering::ALPHA, 3, Duration::from_secs(30)),
        )
        .unwrap();
        let outcomes = Supervisor::new(plans.clone(), world.path("alpha-state"), false)
            .with_peering(
                PeeringContext::for_alpha(world.path("config.toml"), alpha_directory.clone())
                    .expect("context"),
            )
            .run_once();
        assert!(outcomes[0].result.is_err(), "{:?}", outcomes[0].result);
        assert!(!other_root.join("from-alpha.txt").exists());
        let recorded = peering::read_lease(&alpha_directory)
            .expect("readable")
            .expect("recorded");
        assert_eq!(
            (recorded.leader.as_str(), recorded.term),
            (name.as_str(), 4)
        );

        stop.store(true, Ordering::Relaxed);
        peer.join().expect("the peer thread").expect("the peer ran");
    });
}

/// Peering, phase 5, the whole loop from the alpha's side. A beta leads;
/// the alpha comes back and dials the beta as it always did, is fenced,
/// and steps down; it then dials in and attaches as an agent; the beta
/// runs their session over the attachment and, once it settles, hands
/// the lead back; the alpha leads again and dials the beta as before.
#[test]
fn the_alpha_attaches_to_a_leading_peer_and_gets_the_lead_back() {
    use autobahn::peering::{self, Lease};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    let world = World::new();
    let alpha_root = world.directory("alpha-root");
    let peer_root = world.directory("peer-root");
    let peer_home = world.directory("peer-home");
    write(&alpha_root, "from-alpha.txt", "from the alpha");
    write(&peer_root, "from-peer.txt", "from the peer");
    let peer_script = peering_agent_script(&world, &peer_home);

    // What the alpha pushed to the peer before it went away: a star of
    // one beta, its own root as the alpha, and the session's identifier.
    let configuration = format!(
        r#"
        [advanced.peering-dangerously-experimental]
        ttl = "2s"
        failover_after = "2s"

        [groups.g]
        mode = "peering-conflict-dangerously-experimental"
        interval = 1
        alpha = "{alpha_root}"
        agent_command = "{script}"
        betas = ["peer:{peer_root}"]
        "#,
        alpha_root = alpha_root.display(),
        script = peer_script.display(),
        peer_root = peer_root.display(),
    );
    let plans = world.plans(&configuration);
    let name = format!("peer:{}", peer_root.display());
    let peering_directory = peer_home.join(".autobahn").join("peering");
    peering::write_pushed_file(&peering_directory, "config.toml", configuration.as_bytes())
        .unwrap();
    peering::write_pushed_file(&peering_directory, "name", name.as_bytes()).unwrap();
    peering::write_pushed_file(
        &peering_directory,
        "sessions/g",
        plans[0].identifier().as_bytes(),
    )
    .unwrap();
    peering::write_lease(
        &peering_directory,
        &Lease {
            leader: peering::ALPHA.to_owned(),
            term: 3,
            renewed_at: peering::now_seconds().saturating_sub(120),
            ttl_seconds: 2,
        },
    )
    .unwrap();
    // The alpha remembers leading at term 3, and reaches the peer's attach
    // socket directly rather than over ssh. The alpha runs in this process,
    // so the variable is set process-wide, under the guard that restores it.
    let alpha_directory = peering::directory().expect("the alpha's peering directory");
    peering::write_lease(
        &alpha_directory,
        &Lease::new(peering::ALPHA, 3, Duration::from_secs(30)),
    )
    .unwrap();
    let socket = peering_directory.join(peering::ATTACH_SOCKET);
    let _attach = EnvironmentGuard::set(
        peering::ATTACH_COMMAND_VARIABLE,
        format!(
            "{} peering attach --socket {}",
            agent_binary(),
            socket.display()
        ),
    );

    let stop = AtomicBool::new(false);
    let peer_state = world.state_root();
    let alpha_state = world.path("alpha-state");
    let alerts = autobahn::alerts::AlertPlan::default();
    std::thread::scope(|scope| {
        let _guard = StopGuard(&stop);
        let peer = scope.spawn(|| {
            autobahn::supervisor::peer::run(&peering_directory, &peer_state, true, &stop)
        });
        assert!(
            wait_until(Duration::from_secs(20), || socket.exists()),
            "the peer should lead and listen for the alpha"
        );

        // The alpha comes back.
        let alpha = scope.spawn(|| {
            autobahn::supervisor::peer::run_alpha(
                &world.path("config.toml"),
                &alpha_directory,
                &plans,
                &alerts,
                &alpha_state,
                true,
                &stop,
                None,
            )
        });

        // Fenced, attached, synchronized both ways over the attachment.
        assert!(
            wait_until(Duration::from_secs(30), || peer_root
                .join("from-alpha.txt")
                .exists()
                && alpha_root.join("from-peer.txt").exists()),
            "the attached session should carry both roots' files"
        );
        // The lead comes back to the alpha at the next term, on both hosts.
        assert!(
            wait_until(Duration::from_secs(30), || {
                let theirs = peering::read_lease(&peering_directory).ok().flatten();
                let mine = peering::read_lease(&alpha_directory).ok().flatten();
                theirs.is_some_and(|l| l.leader == peering::ALPHA && l.term == 5)
                    && mine.is_some_and(|l| l.leader == peering::ALPHA && l.term == 5)
            }),
            "the lead should come back to the alpha at term 5"
        );
        // The alpha leads again the ordinary way: it dials the peer's
        // agent, and a new file crosses; the peer's lease stays fresh
        // because the alpha renews it every cycle.
        write(&alpha_root, "after.txt", "after the handback");
        assert!(
            wait_until(Duration::from_secs(30), || peer_root
                .join("after.txt")
                .exists()),
            "the alpha should lead again and reach the peer"
        );
        std::thread::sleep(Duration::from_secs(3));
        let theirs = peering::read_lease(&peering_directory)
            .expect("readable")
            .expect("held");
        assert_eq!((theirs.leader.as_str(), theirs.term), (peering::ALPHA, 5));
        assert!(
            !theirs.is_stale_at(peering::now_seconds()),
            "the alpha keeps the peer's lease fresh: {theirs:?}"
        );
        let status = autobahn::supervisor::peer::read_status(&peering_directory)
            .expect("readable")
            .expect("written");
        assert_eq!(status.standing, "fresh", "{status:?}");

        stop.store(true, Ordering::Relaxed);
        peer.join().expect("the peer thread").expect("the peer ran");
        alpha
            .join()
            .expect("the alpha thread")
            .expect("the alpha ran");
    });
}

#[test]
fn a_healthy_group_is_one_line_in_status_and_trouble_is_shown_in_full() {
    let world = World::new();
    let quiet = world.directory("quiet");
    let quiet_mirror = world.directory("quiet-mirror");
    let noisy = world.directory("noisy");
    let noisy_mirror = world.directory("noisy-mirror");
    write(&quiet, "a.txt", "a");
    write(&noisy, "b.txt", "b");
    let config = world.path("config.toml");
    let plans = world.plans(&format!(
        r#"
        [defaults]
        mode = "two-way-conflict"

        [groups.quiet]
        alpha = "{quiet}"
        betas = ["{quiet_mirror}"]

        [groups.noisy]
        alpha = "{noisy}"
        betas = ["{noisy_mirror}"]
        "#,
        quiet = quiet.display(),
        quiet_mirror = quiet_mirror.display(),
        noisy = noisy.display(),
        noisy_mirror = noisy_mirror.display(),
    ));
    assert_all_synchronized(&world.run_once(plans.clone()));
    // A conflict in one group only.
    write(&noisy, "b.txt", "alpha's");
    write(&noisy_mirror, "b.txt", "beta's");
    world.run_once(plans);

    let (ok, text) = cli(&world, &config, &["status"]);
    assert!(ok, "{text}");
    let quiet_line = text
        .lines()
        .find(|line| line.contains("quiet") && !line.contains("mirror"))
        .expect("the quiet group is listed");
    assert!(quiet_line.contains("✓ 1 synchronized"), "{text}");
    assert!(
        !text.contains(&quiet_mirror.display().to_string()),
        "the healthy group's destinations are folded into its line: {text}"
    );
    assert!(
        text.contains(&noisy_mirror.display().to_string()) && text.contains("conflicts"),
        "the group in trouble is in full: {text}"
    );

    let (_, text) = cli(&world, &config, &["status", "--all"]);
    assert!(text.contains(&quiet_mirror.display().to_string()), "{text}");
    let (_, text) = cli(&world, &config, &["status", "quiet"]);
    assert!(text.contains(&quiet_mirror.display().to_string()), "{text}");
}

#[test]
fn doctor_says_whether_a_reset_is_free_and_writes_nothing() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "keep.txt", "keep");
    write(&alpha, "gone.txt", "gone");
    let config = world.path("config.toml");
    let plans = world.plans(&format!(
        r#"
        [groups.work]
        mode = "two-way-conflict"
        alpha = "{alpha}"
        betas = ["{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
    ));
    assert_all_synchronized(&world.run_once(plans));

    let (ok, text) = cli(&world, &config, &["doctor", "work"]);
    assert!(ok, "{text}");
    assert!(text.contains("baseline: readable"), "{text}");
    assert!(text.contains("nothing to do"), "{text}");
    assert!(
        text.contains("a reset: ") && text.contains("free"),
        "{text}"
    );

    // A deletion the baseline knows about, not yet carried across.
    fs::remove_file(alpha.join("gone.txt")).unwrap();
    let sessions = world.state_root().join("sessions");
    let before: Vec<(PathBuf, std::time::SystemTime)> = walk_files(&sessions);
    let (ok, text) = cli(&world, &config, &["doctor", "work"]);
    assert!(ok, "{text}");
    assert!(
        text.contains("delete gone.txt to beta"),
        "the next cycle: {text}"
    );
    assert!(
        text.contains("copy gone.txt to alpha"),
        "what a reset would bring back: {text}"
    );
    // The baseline is untouched. (A scan refreshes its scan cache, as any
    // scan does; that is a cache, written by rename, and nothing else.)
    let baseline = |files: Vec<(PathBuf, std::time::SystemTime)>| -> Vec<_> {
        files
            .into_iter()
            .filter(|(path, _)| !path.to_string_lossy().ends_with(".scancache"))
            .collect()
    };
    assert_eq!(
        baseline(walk_files(&sessions)),
        baseline(before),
        "doctor changed session state"
    );
}

fn walk_files(root: &Path) -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                found.push((path, modified));
            }
        }
    }
    found.sort();
    found
}

/// An atomic deploy swap on the alpha, seen by the watcher as three
/// renamed names and nothing inside them. Reproduced before the fix: the
/// beta kept the old `live/` contents, duplicated them into `old/`, and
/// deleted `staging/`, until a full walk minutes later.
#[test]
fn a_swapped_directory_reaches_the_beta_with_its_new_contents() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "live/f1", "old content");
    write(&alpha, "live/sub/f2", "old deeper");
    write(&alpha, "staging/f1", "new content");
    write(&alpha, "staging/sub/f2", "new deeper");

    let mut plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        betas = ["{beta}"]
        "#,
        alpha = alpha.display(),
        beta = beta.display(),
    ));
    plans[0].interval = Duration::from_millis(30);

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let supervisor = Supervisor::new(plans, world.state_root(), false);
        let stop = &stop;
        let watcher = scope.spawn(move || supervisor.run_watch(stop));
        let _guard = StopGuard(stop);

        assert!(
            wait_until(Duration::from_secs(15), || {
                beta.join("staging/sub/f2").exists() && beta.join("live/sub/f2").exists()
            }),
            "initial content should propagate"
        );
        // Let the first cycles settle, so the swap is seen by an
        // incremental scan against a baseline that holds both trees.
        std::thread::sleep(Duration::from_millis(500));

        fs::rename(alpha.join("live"), alpha.join("old")).expect("rename");
        fs::rename(alpha.join("staging"), alpha.join("live")).expect("rename");

        let settled = || {
            !beta.join("staging").exists()
                && fs::read_to_string(beta.join("live/f1")).ok().as_deref() == Some("new content")
                && fs::read_to_string(beta.join("live/sub/f2")).ok().as_deref()
                    == Some("new deeper")
                && fs::read_to_string(beta.join("old/f1")).ok().as_deref() == Some("old content")
                && fs::read_to_string(beta.join("old/sub/f2")).ok().as_deref() == Some("old deeper")
        };
        assert!(
            wait_until(Duration::from_secs(15), settled),
            "the beta should hold the swapped tree: live/f1 = {:?}, old/f1 = {:?}, staging = {}",
            fs::read_to_string(beta.join("live/f1")).ok(),
            fs::read_to_string(beta.join("old/f1")).ok(),
            beta.join("staging").exists(),
        );

        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });
}

#[test]
fn watch_keeps_synchronizing_after_its_standard_output_closes() {
    // `autobahn watch | head -1`: the reader goes away, and every later
    // log line meets a closed pipe. That costs the lines, not the sessions.
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "first.txt", "first");
    let path = world.path("config.toml");
    fs::write(
        &path,
        format!(
            "[groups.work]\nalpha = \"{}\"\nmode = \"two-way-safe\"\ninterval = 1\nbetas = [\"{}\"]\n",
            alpha.display(),
            beta.display()
        ),
    )
    .expect("configuration should be writable");
    let mut child = std::process::Command::new(agent_binary())
        .args(["watch", "--log", "--config"])
        .arg(&path)
        .arg("--state-root")
        .arg(world.state_root())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("watch starts");
    drop(child.stdout.take());
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Kill(child);
    assert!(
        wait_until(Duration::from_secs(15), || beta.join("first.txt").exists()),
        "the first file synchronizes"
    );
    for index in 0..3 {
        let name = format!("later-{index}.txt");
        write(&alpha, &name, "later");
        assert!(
            wait_until(Duration::from_secs(15), || beta.join(&name).exists()),
            "{name} synchronizes with nobody reading the log"
        );
    }
    assert!(
        child.0.try_wait().expect("waitable").is_none(),
        "the supervisor is still running"
    );
}

#[test]
fn status_shows_the_running_sessions_and_the_refusal_when_the_file_breaks() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    let notes = world.directory("notes");
    let notes_mirror = world.path("notes-mirror");
    write(&alpha, "file.txt", "file");
    write(&notes, "todo.txt", "todo");
    let path = world.path("config.toml");
    let one = format!(
        "[groups.work]\nalpha = \"{}\"\nmode = \"two-way-safe\"\ninterval = 1\nbetas = [\"{}\"]\n",
        alpha.display(),
        beta.display()
    );
    world.plans(&one);
    let shown_groups = || -> Vec<String> {
        autobahn::supervisor::shown_plans(&path, &world.state_root())
            .map(|shown| shown.plans.into_iter().map(|plan| plan.group).collect())
            .unwrap_or_default()
    };

    supervise_with_reload(&world, &path, || {
        assert!(wait_until(Duration::from_secs(15), || beta
            .join("file.txt")
            .exists()));
        fs::write(&path, format!("{one}[groups.notes]\nmdoe = 1\n"))
            .expect("configuration should be writable");
        assert!(wait_until(Duration::from_secs(15), || {
            autobahn::supervisor::reload::read_notice(&world.state_root()).is_some()
        }));
        let (succeeded, shown) = cli(&world, &path, &["status"]);
        assert!(succeeded, "status does not fail on the file: {shown}");
        assert!(
            shown.contains("work"),
            "the running session is listed: {shown}"
        );
        assert!(shown.contains("refused"), "the refusal is shown: {shown}");
        assert!(shown.contains("mdoe"), "with what was wrong: {shown}");

        // What the shop and the tray load on every poll: an edit the
        // supervisor applies is there at the next one.
        assert_eq!(shown_groups(), ["work"]);
        fs::write(
            &path,
            format!(
                "{one}[groups.notes]\nmode = \"two-way-safe\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
                notes.display(),
                notes_mirror.display()
            ),
        )
        .expect("configuration should be writable");
        assert!(
            wait_until(Duration::from_secs(15), || shown_groups().len() == 2),
            "the added group is shown: {:?}",
            shown_groups()
        );
    });
}

#[test]
fn status_with_nothing_running_and_a_broken_file_shows_what_was_recorded() {
    let world = World::new();
    let alpha = world.directory("alpha");
    let beta = world.directory("beta");
    write(&alpha, "file.txt", "file");
    let path = world.path("config.toml");
    let plans = world.plans(&format!(
        "[groups.work]\nalpha = \"{}\"\nmode = \"two-way-safe\"\nbetas = [\"{}\"]\n",
        alpha.display(),
        beta.display()
    ));
    assert_all_synchronized(&world.run_once(plans));
    fs::write(&path, "[groups.work]\nmdoe = 1\n").expect("configuration should be writable");
    let (succeeded, shown) = cli(&world, &path, &["status"]);
    assert!(succeeded, "status does not fail on the file: {shown}");
    assert!(shown.contains("does not load"), "{shown}");
    assert!(shown.contains("mdoe"), "the parse error is shown: {shown}");
    assert!(
        shown.contains("work@"),
        "the recorded session is listed: {shown}"
    );
    assert!(
        shown.contains("synchronized"),
        "with its recorded state: {shown}"
    );
}
