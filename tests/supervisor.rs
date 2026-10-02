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
    let primary = world.directory("source");
    let replica = world.directory("mirror");
    for index in 0..20 {
        write(
            &primary,
            &format!("dir{}/file{index}.txt", index % 3),
            "initial",
        );
    }

    let plans = world.plans(&format!(
        r#"
        [groups.churn]
        primary = "{primary}"
        mode = "two-way-conflict"
        agent_command = "{agent} agent"
        replicas = ["remote-host:{replica}"]
        "#,
        primary = primary.display(),
        agent = agent_binary(),
        replica = replica.display(),
    ));

    assert_all_synchronized(&world.run_once(plans.clone()));
    assert_eq!(read(&replica, "dir0/file0.txt"), "initial");

    // Round after round of one-directional edits: only primary changes, so
    // replica's every scan after the first reports itself unchanged.
    // Each round makes exactly one modification and one creation; the
    // created names lie outside the range the fixture already wrote.
    for round in 1..=5 {
        write(&primary, "dir0/file0.txt", &format!("round {round}"));
        write(
            &primary,
            &format!("dir1/added{round}.txt"),
            &format!("new {round}"),
        );
        assert_all_synchronized(&world.run_once(plans.clone()));
        assert_eq!(read(&replica, "dir0/file0.txt"), format!("round {round}"));
        assert_eq!(
            read(&replica, &format!("dir1/added{round}.txt")),
            format!("new {round}")
        );
    }

    // A deletion and a replica-side edit must still cross correctly.
    fs::remove_file(primary.join("dir2/file2.txt")).expect("file should be removable");
    write(&replica, "replica-only.txt", "from the far side");
    assert_all_synchronized(&world.run_once(plans));
    assert!(!replica.join("dir2/file2.txt").exists());
    assert_eq!(read(&primary, "replica-only.txt"), "from the far side");
}

#[test]
fn a_remote_primary_synchronizes_through_a_real_agent() {
    let world = World::new();
    let remote_primary = world.directory("remote-src");
    let local_replica = world.directory("local-dst");
    write(&remote_primary, "artifact.bin", "built content");
    write(&remote_primary, "nested/report.txt", "report");
    write(&local_replica, "local-note.txt", "kept");

    // The primary is a *remote* specification reached through a real agent
    // subprocess; the replica is a plain local directory.
    let plans = world.plans(&format!(
        r#"
        [groups.pull]
        primary = "remote-host:{remote_primary}"
        mode = "two-way-conflict"
        agent_command = "{agent} agent"
        replicas = ["{local_replica}"]
        "#,
        remote_primary = remote_primary.display(),
        agent = agent_binary(),
        local_replica = local_replica.display(),
    ));
    assert_eq!(plans.len(), 1);

    let outcomes = world.run_once(plans);
    assert_all_synchronized(&outcomes);

    // Content flowed in both directions across the remote primary.
    assert_eq!(read(&local_replica, "artifact.bin"), "built content");
    assert_eq!(read(&local_replica, "nested/report.txt"), "report");
    assert_eq!(read(&remote_primary, "local-note.txt"), "kept");
}

#[test]
fn a_configuration_file_drives_multiple_groups_and_hosts() {
    let world = World::new();
    let primary_one = world.directory("project");
    let primary_two = world.directory("notes");
    let local_replica = world.directory("mirror");
    let second_replica = world.path("second-mirror");
    let agent_replica = world.directory("agent-mirror");
    write(&primary_one, "src/main.rs", "fn main() {}");
    write(&primary_one, "README.md", "readme");
    write(&primary_two, "todo.txt", "everything");

    // One group fans out to two local replicas (one of which doesn't exist yet
    // and must be created); the other reaches its replica through a real agent
    // subprocess.
    let plans = world.plans(&format!(
        r#"
        [defaults]
        mode = "two-way-conflict"

        [groups.project]
        primary = "{primary_one}"
        replicas = ["{local_replica}", "{second_replica}"]

        [groups.notes]
        primary = "{primary_two}"
        agent_command = "{agent} agent"
        replicas = ["remote-host:{agent_replica}"]
        "#,
        primary_one = primary_one.display(),
        local_replica = local_replica.display(),
        second_replica = second_replica.display(),
        primary_two = primary_two.display(),
        agent = agent_binary(),
        agent_replica = agent_replica.display(),
    ));
    assert_eq!(plans.len(), 3);

    let outcomes = world.run_once(plans.clone());
    assert_all_synchronized(&outcomes);

    // Every destination matches its primary.
    assert_eq!(read(&local_replica, "src/main.rs"), "fn main() {}");
    assert_eq!(read(&local_replica, "README.md"), "readme");
    assert_eq!(read(&second_replica, "src/main.rs"), "fn main() {}");
    // A root created by the transition takes the configured directory mode
    // (the conservative default), not the umask's.
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        fs::symlink_metadata(&second_replica).expect("root").mode() & 0o777,
        0o700
    );
    assert_eq!(read(&agent_replica, "todo.txt"), "everything");

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
        assert_eq!(digest.primary_transitions + digest.replica_transitions, 0);
    }
}

#[test]
fn per_group_modes_are_respected() {
    let world = World::new();
    let safe_primary = world.directory("safe-primary");
    let safe_replica = world.directory("safe-replica");
    let mirror_primary = world.directory("mirror-primary");
    let mirror_replica = world.directory("mirror-replica");
    write(&safe_primary, "shared.txt", "original");
    write(&mirror_primary, "kept.txt", "kept");
    write(&mirror_replica, "extra.txt", "replica only");

    let configuration = format!(
        r#"
        [groups.careful]
        primary = "{safe_primary}"
        mode = "two-way-conflict"
        replicas = ["{safe_replica}"]

        [groups.mirror]
        primary = "{mirror_primary}"
        mode = "one-way-primary"
        replicas = ["{mirror_replica}"]
        "#,
        safe_primary = safe_primary.display(),
        safe_replica = safe_replica.display(),
        mirror_primary = mirror_primary.display(),
        mirror_replica = mirror_replica.display(),
    );
    let plans = world.plans(&configuration);
    assert_all_synchronized(&world.run_once(plans.clone()));

    // The mirrored replica matches its primary exactly: the replica-only file is gone.
    assert_eq!(read(&mirror_replica, "kept.txt"), "kept");
    assert!(!mirror_replica.join("extra.txt").exists());

    // Now diverge the safe group's file on both sides.
    write(&safe_primary, "shared.txt", "primary edit");
    write(&safe_replica, "shared.txt", "replica edit");
    let outcomes = world.run_once(plans.clone());
    assert_all_synchronized(&outcomes);

    // The conflict is reported, not resolved: both edits survive.
    assert_eq!(read(&safe_primary, "shared.txt"), "primary edit");
    assert_eq!(read(&safe_replica, "shared.txt"), "replica edit");
    let careful = plans
        .iter()
        .find(|plan| plan.group == "careful")
        .expect("the careful plan should exist");
    let status = world.status(careful).expect("status should be recorded");
    assert_eq!(status.state, "conflicts");
    assert_eq!(status.conflicts, vec!["shared.txt".to_owned()]);

    // The same divergence under two-way-primary resolves in primary's favor
    // (a fresh state root gives the mode change a clean baseline).
    let resolved_world = World::new();
    let plans = resolved_world.plans(&configuration.replace("two-way-conflict", "two-way-primary"));
    assert_all_synchronized(&resolved_world.run_once(plans.clone()));
    write(&safe_primary, "shared.txt", "primary wins");
    write(&safe_replica, "shared.txt", "replica loses");
    assert_all_synchronized(&resolved_world.run_once(plans));
    assert_eq!(read(&safe_replica, "shared.txt"), "primary wins");
}

#[test]
fn defaults_and_group_ignores_combine() {
    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, ".git/HEAD", "ref: refs/heads/main");
    write(&primary, "scratch.tmp", "temporary");
    write(&primary, "real.txt", "real");

    let plans = world.plans(&format!(
        r#"
        [defaults]
        mode = "two-way-conflict"
        ignores = [".git"]

        [groups.work]
        primary = "{primary}"
        ignores = ["*.tmp"]
        replicas = ["{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
    ));
    assert_all_synchronized(&world.run_once(plans));

    assert_eq!(read(&replica, "real.txt"), "real");
    assert!(
        !replica.join(".git").exists(),
        "default ignore should apply"
    );
    assert!(
        !replica.join("scratch.tmp").exists(),
        "group ignore should apply"
    );
}

#[test]
fn disabled_hosts_are_excluded_from_supervision() {
    let world = World::new();
    let primary = world.directory("primary");
    let enabled_replica = world.directory("enabled-replica");
    let disabled_replica = world.directory("disabled-replica");
    write(&primary, "file.txt", "content");

    let plans = world.plans(&format!(
        r#"
        disabled_hosts = ["down-host"]

        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        agent_command = "{agent} agent"
        replicas = ["up-host:{enabled_replica}", "down-host:{disabled_replica}"]
        "#,
        primary = primary.display(),
        agent = agent_binary(),
        enabled_replica = enabled_replica.display(),
        disabled_replica = disabled_replica.display(),
    ));
    // The disabled host never becomes a plan at all.
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].host, "up-host");

    assert_all_synchronized(&world.run_once(plans));
    assert_eq!(read(&enabled_replica, "file.txt"), "content");
    assert!(!disabled_replica.join("file.txt").exists());
}

#[test]
fn an_unreachable_destination_does_not_block_other_sessions() {
    let world = World::new();
    let healthy_primary = world.directory("healthy-primary");
    let healthy_replica = world.directory("healthy-replica");
    let doomed_primary = world.directory("doomed-primary");
    write(&healthy_primary, "file.txt", "content");
    write(&doomed_primary, "file.txt", "content");

    let plans = world.plans(&format!(
        r#"
        [groups.healthy]
        primary = "{healthy_primary}"
        mode = "two-way-conflict"
        replicas = ["{healthy_replica}"]

        [groups.doomed]
        primary = "{doomed_primary}"
        mode = "two-way-conflict"
        agent_command = "/nonexistent/agent-binary agent"
        replicas = ["unreachable-host:/anywhere"]
        "#,
        healthy_primary = healthy_primary.display(),
        healthy_replica = healthy_replica.display(),
        doomed_primary = doomed_primary.display(),
    ));
    let outcomes = world.run_once(plans.clone());

    // The healthy session completed despite its sibling's failure.
    let healthy = outcomes
        .iter()
        .find(|outcome| outcome.display.starts_with("healthy@"))
        .expect("the healthy outcome should exist");
    assert!(healthy.result.is_ok(), "{:?}", healthy.result);
    assert_eq!(read(&healthy_replica, "file.txt"), "content");

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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "file.txt", "content");

    let plans = world.plans(&format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        replicas = ["{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
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
    assert_eq!(read(&replica, "file.txt"), "content");
}

#[test]
fn concurrent_sessions_over_the_same_state_are_refused() {
    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "file.txt", "content");

    // Duplicate plans can't come from one configuration (plans() rejects
    // them), so simulate two supervisor processes: two Supervisors over the
    // same state root, one of whose workers already holds the session lock.
    let plans = world.plans(&format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        replicas = ["{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
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
            wait_until(Duration::from_secs(15), || replica
                .join("file.txt")
                .exists()),
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "file.txt", "content");

    let mut plans = world.plans(&format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        replicas = ["{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
    ));
    plans[0].interval = Duration::from_millis(30);

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let supervisor = Supervisor::new(plans.clone(), world.state_root(), false);
        let stop_ref = &stop;
        let watcher = scope.spawn(move || supervisor.run_watch(stop_ref));
        let _guard = StopGuard(stop_ref);
        assert!(
            wait_until(Duration::from_secs(15), || replica
                .join("file.txt")
                .exists()),
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
fn a_missing_primary_is_a_session_error_not_a_crash() {
    let world = World::new();
    let replica = world.directory("replica");
    let plans = world.plans(&format!(
        r#"
        [groups.ghost]
        primary = "{missing}"
        mode = "two-way-conflict"
        replicas = ["{replica}"]
        "#,
        missing = world.path("never-created").display(),
        replica = replica.display(),
    ));
    let outcomes = world.run_once(plans.clone());
    let error = outcomes[0]
        .result
        .as_ref()
        .expect_err("the session should fail");
    assert!(
        error.contains("halted") && error.contains("primary folder") && error.contains("missing"),
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
    let replica = world.directory("replica");
    write(&tree_a, "file.txt", "a's content");
    write(&tree_b, "file.txt", "b's content");
    let link = world.path("entry");
    std::os::unix::fs::symlink(&tree_a, &link).expect("symlink should be creatable");

    let plans = world.plans(&format!(
        r#"
        [groups.work]
        primary = "{link}"
        mode = "two-way-conflict"
        replicas = ["{replica}"]
        "#,
        link = link.display(),
        replica = replica.display(),
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
        !replica.join("file.txt").exists(),
        "nothing may be synchronized from the wrong tree"
    );
}

#[test]
fn state_persists_across_supervisor_runs() {
    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "keep.txt", "keep");
    write(&primary, "remove.txt", "remove");

    let configuration = format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        replicas = ["{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
    );
    let plans = world.plans(&configuration);
    assert_all_synchronized(&world.run_once(plans.clone()));
    assert_eq!(read(&replica, "remove.txt"), "remove");

    // Delete on primary, then run a *fresh* supervisor over the same state
    // root: only a persisted ancestor lets it see a deletion rather than a
    // one-sided file (which two-way-conflict would copy back).
    fs::remove_file(primary.join("remove.txt")).expect("file should be removable");
    assert_all_synchronized(&world.run_once(plans));
    assert!(!replica.join("remove.txt").exists());
    assert!(primary.join("keep.txt").exists());
    assert_eq!(read(&replica, "keep.txt"), "keep");
}

#[test]
fn watch_mode_synchronizes_continuously_until_stopped() {
    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "first.txt", "first");

    let mut plans = world.plans(&format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        replicas = ["{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
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
            wait_until(Duration::from_secs(15), || replica
                .join("first.txt")
                .exists()),
            "initial content should propagate"
        );
        // ...changes made while watching propagate in both directions...
        write(&primary, "second.txt", "second");
        write(&replica, "from-replica.txt", "reverse");
        assert!(
            wait_until(Duration::from_secs(15), || {
                replica.join("second.txt").exists() && primary.join("from-replica.txt").exists()
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    let notes = world.directory("notes");
    let notes_mirror = world.path("notes-mirror");
    write(&primary, "first.txt", "first");
    write(&notes, "todo.txt", "everything");
    let (script, counter) = counting_agent(&world, "agent");

    let path = world.path("config.toml");
    let one_group = format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        interval = 1
        agent_command = "{script}"
        replicas = ["host:{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
        script = script.display(),
    );
    let plans = world.plans(&one_group);
    let work = plans[0].clone();

    supervise_with_reload(&world, &path, || {
        assert!(
            wait_until(Duration::from_secs(15), || replica
                .join("first.txt")
                .exists()),
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
        write(&primary, "second.txt", "second");
        assert!(
            wait_until(Duration::from_secs(15), || replica
                .join("second.txt")
                .exists()),
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
                "{one_group}\n[groups.notes]\nmode = \"two-way-conflict\"\nprimary = \"{}\"\nreplicas = [\"{}\"]\n",
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
        write(&primary, "third.txt", "third");
        assert!(
            wait_until(Duration::from_secs(15), || replica
                .join("third.txt")
                .exists()),
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
    let primary = elsewhere.path().join("primary");
    let replica = elsewhere.path().join("replica");
    fs::create_dir_all(&primary).expect("directory should be creatable");
    fs::create_dir_all(&replica).expect("directory should be creatable");
    let mirror = elsewhere.path().join("mirror");
    write(&primary, "first.txt", "first");
    let path = world.path("config.toml");
    let one_group = format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        interval = 1
        replicas = ["{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
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
            wait_until(Duration::from_secs(15), || replica
                .join("first.txt")
                .exists()),
            "the first configuration synchronizes"
        );
        // The world's directory holds the state root and the configuration.
        fs::write(
            &path,
            format!(
                "{one_group}\n[groups.everything]\nmode = \"two-way-conflict\"\nprimary = \"{}\"\nreplicas = [\"{}\"]\n",
                world.path("").display(),
                mirror.display()
            ),
        )
        .expect("configuration should be writable");
        std::thread::sleep(Duration::from_millis(500));
        write(&primary, "second.txt", "second");
        assert!(
            wait_until(Duration::from_secs(15), || replica
                .join("second.txt")
                .exists()),
            "the running session carries on"
        );
        assert_eq!(
            supervised(&world),
            Some(vec![format!("work@{}", replica.display())]),
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
    let primary_one = world.directory("primary-one");
    let primary_two = world.directory("primary-two");
    let replica_one = world.directory("replica-one");
    let replica_two = world.directory("replica-two");
    let (script, counter) = counting_agent(&world, "agent");
    let path = world.path("config.toml");
    let configuration = |ignores: &str| {
        format!(
            r#"
            [defaults]
            mode = "two-way-conflict"
            interval = 1

            [groups.one]
            primary = "{primary_one}"
            agent_command = "{script}"
            replicas = ["shared-host:{replica_one}"]

            [groups.two]
            primary = "{primary_two}"
            agent_command = "{script}"
            ignores = [{ignores}]
            replicas = ["shared-host:{replica_two}"]
            "#,
            script = script.display(),
            primary_one = primary_one.display(),
            primary_two = primary_two.display(),
            replica_one = replica_one.display(),
            replica_two = replica_two.display(),
        )
    };
    let plans = world.plans(&configuration(""));
    let (one, two) = (plans[0].clone(), plans[1].clone());
    write(&primary_one, "one.txt", "one");
    write(&primary_two, "two.txt", "two");

    supervise_with_reload(&world, &path, || {
        assert!(wait_until(Duration::from_secs(15), || {
            replica_one.join("one.txt").exists() && replica_two.join("two.txt").exists()
        }));
        // Run the session to be changed through more cycles than one
        // attempt can hold, so its restart shows as a count that went back.
        let limit = autobahn::supervisor::MAXIMUM_FOLLOW_UP_CYCLES as u64 + 1;
        for index in 0..=limit {
            let name = format!("warm-{index}.txt");
            write(&primary_two, &name, "warm");
            assert!(wait_until(Duration::from_secs(15), || replica_two
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
        write(&primary_two, "scratch.tmp", "scratch");
        write(&primary_two, "kept.txt", "kept");
        assert!(
            wait_until(Duration::from_secs(15), || replica_two
                .join("kept.txt")
                .exists()),
            "the changed session runs under its new plan"
        );
        assert!(
            !replica_two.join("scratch.tmp").exists(),
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
    let primary_one = world.directory("primary-one");
    let primary_two = world.directory("primary-two");
    let replica_one = world.directory("replica-one");
    let replica_two = world.directory("replica-two");
    let (script_one, counter_one) = counting_agent(&world, "agent-one");
    let (script_two, counter_two) = counting_agent(&world, "agent-two");
    let path = world.path("config.toml");
    let text = format!(
        r#"
        [defaults]
        mode = "two-way-conflict"
        interval = 1

        [groups.one]
        primary = "{primary_one}"
        agent_command = "{script_one}"
        replicas = ["host-one:{replica_one}"]

        [groups.two]
        primary = "{primary_two}"
        agent_command = "{script_two}"
        replicas = ["host-two:{replica_two}"]
        "#,
        script_one = script_one.display(),
        script_two = script_two.display(),
        primary_one = primary_one.display(),
        primary_two = primary_two.display(),
        replica_one = replica_one.display(),
        replica_two = replica_two.display(),
    );
    world.plans(&text);
    write(&primary_one, "one.txt", "one");
    write(&primary_two, "two.txt", "two");

    supervise_with_reload(&world, &path, || {
        assert!(wait_until(Duration::from_secs(15), || {
            replica_one.join("one.txt").exists() && replica_two.join("two.txt").exists()
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
        write(&primary_one, "more.txt", "more");
        assert!(wait_until(Duration::from_secs(15), || replica_one
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    let path = world.path("config.toml");
    let text = format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        interval = 1
        agent_command = "{agent} agent"
        replicas = ["only-host:{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
        agent = agent_binary(),
    );
    world.plans(&text);
    write(&primary, "kept.txt", "kept");
    write(&primary, "doomed.txt", "doomed");

    supervise_with_reload(&world, &path, || {
        assert!(wait_until(Duration::from_secs(15), || {
            replica.join("kept.txt").exists() && replica.join("doomed.txt").exists()
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
        write(&primary, "while-off.txt", "while off");
        fs::remove_file(primary.join("doomed.txt")).expect("removable");
        std::thread::sleep(Duration::from_millis(2500));
        assert!(
            !replica.join("while-off.txt").exists(),
            "nothing propagates while off"
        );
        assert!(replica.join("doomed.txt").exists());

        fs::write(&path, &text).expect("configuration should be writable");
        assert!(
            wait_until(Duration::from_secs(15), || replica
                .join("while-off.txt")
                .exists()),
            "syncing resumes"
        );
        assert!(
            wait_until(Duration::from_secs(15), || !replica
                .join("doomed.txt")
                .exists()),
            "the deletion propagates: the ancestor is intact"
        );
        assert!(!primary.join("doomed.txt").exists());
        assert_eq!(read(&replica, "kept.txt"), "kept");
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "file.txt", "content");

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
        primary = "{primary}"
        mode = "two-way-conflict"
        agent_command = "{script}"
        replicas = ["flaky-host:{replica}"]
        "#,
        primary = primary.display(),
        script = script.display(),
        replica = replica.display(),
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
        assert!(!replica.join("file.txt").exists());

        // The destination recovers; the session heals without intervention.
        fs::write(
            &script,
            format!("#!/bin/sh\nexec {} agent\n", agent_binary()),
        )
        .expect("script should be rewritable");
        assert!(
            wait_until(Duration::from_secs(20), || replica
                .join("file.txt")
                .exists()),
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "first.txt", "first");

    let mut plans = world.plans(&format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        interval = 3600
        replicas = ["{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
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
            wait_until(Duration::from_secs(15), || replica
                .join("first.txt")
                .exists()),
            "initial content should synchronize"
        );
        write(&primary, "second.txt", "second");
        assert!(
            wait_until(Duration::from_secs(15), || replica
                .join("second.txt")
                .exists()),
            "a watched change should propagate without a heartbeat"
        );

        // Pause: the state records, and further changes stop propagating.
        let selector = || Selector {
            group: Some("work".into()),
            ..Selector::default()
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
        // Delete a synchronized file on primary while paused; nothing moves.
        fs::remove_file(primary.join("second.txt")).expect("file should be removable");
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            replica.join("second.txt").exists(),
            "paused sessions must not sync"
        );

        // Reset while paused, then resume: with the ancestor discarded, the
        // deletion is forgotten and replica's copy flows back to primary.
        let response = control::send(&world.state_root(), &ControlRequest::Reset(selector()))
            .expect("reset should send");
        assert!(matches!(response, ControlResponse::Applied { sessions: 1 }));
        let response = control::send(&world.state_root(), &ControlRequest::Resume(selector()))
            .expect("resume should send");
        assert!(matches!(response, ControlResponse::Applied { sessions: 1 }));
        assert!(
            wait_until(Duration::from_secs(15), || primary
                .join("second.txt")
                .exists()),
            "after a reset, the deletion is forgotten and content merges back"
        );

        // A selector matching nothing is an error, not a silent no-op.
        let response = control::send(
            &world.state_root(),
            &ControlRequest::Flush(Selector {
                group: Some("absent".into()),
                ..Selector::default()
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
    let primary = world.directory("primary");
    let remote = world.directory("remote-mirror");
    write(&primary, "seed.txt", "seed");

    let mut plans = world.plans(&format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        agent_command = "{agent} agent"
        replicas = ["fake-host:{remote}"]
        "#,
        primary = primary.display(),
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
                primary.join("from-remote.txt").exists()
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
    let primary = world.directory("primary");
    let remote_home = world.directory("remote-home");
    let remote_mirror = world.directory("remote-mirror");
    write(&primary, "file.txt", "content");

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
            primary = "{primary}"
            mode = "two-way-conflict"
            replicas = ["fake-host:{remote_mirror}"]
            "#,
            primary = primary.display(),
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
    let primary_one = world.directory("primary-one");
    let primary_two = world.directory("primary-two");
    let replica_one = world.directory("replica-one");
    let replica_two = world.directory("replica-two");
    write(&primary_one, "one.txt", "one");
    write(&primary_two, "two.txt", "two");

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
        mode = "two-way-conflict"

        [groups.one]
        primary = "{primary_one}"
        agent_command = "{script}"
        replicas = ["shared-host:{replica_one}"]

        [groups.two]
        primary = "{primary_two}"
        agent_command = "{script}"
        replicas = ["shared-host:{replica_two}"]
        "#,
        script = script.display(),
        primary_one = primary_one.display(),
        primary_two = primary_two.display(),
        replica_one = replica_one.display(),
        replica_two = replica_two.display(),
    ));
    let outcomes = world.run_once(plans);
    assert_all_synchronized(&outcomes);

    // Both sessions synchronized...
    assert_eq!(read(&replica_one, "one.txt"), "one");
    assert_eq!(read(&replica_two, "two.txt"), "two");
    // ...through exactly one agent process.
    let launches = fs::read_to_string(&counter).expect("the counter should exist");
    assert_eq!(launches.lines().count(), 1, "{launches:?}");
}

#[test]
fn policy_flows_through_the_agent_protocol() {
    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    fs::write(primary.join("file.txt"), "content").expect("file should be writable");
    std::os::unix::fs::symlink("file.txt", primary.join("link")).expect("symlink");

    // The group's policy — ignored symlinks and 0644 files — must govern the
    // *agent-side* endpoint, proving Initialize carries it across the wire.
    let plans = world.plans(&format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        symlink_mode = "ignore"
        file_mode = "0644"
        directory_mode = "0755"
        agent_command = "{agent} agent"
        replicas = ["remote-host:{replica}"]
        "#,
        primary = primary.display(),
        agent = agent_binary(),
        replica = replica.display(),
    ));
    let outcomes = world.run_once(plans);
    assert_all_synchronized(&outcomes);

    assert_eq!(read(&replica, "file.txt"), "content");
    use std::os::unix::fs::MetadataExt;
    let mode = fs::symlink_metadata(replica.join("file.txt"))
        .expect("file should exist")
        .mode()
        & 0o777;
    assert_eq!(mode, 0o644);
    // The symlink was invisible on both sides.
    assert!(!replica.join("link").exists());
}

#[test]
fn remote_home_relative_roots_resolve_against_the_agent_home() {
    let world = World::new();
    let primary = world.directory("primary");
    let remote_home = world.directory("remote-home");
    write(&primary, "file.txt", "content");

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
        primary = "{primary}"
        mode = "two-way-conflict"
        agent_command = "{script}"
        replicas = ["remote-host:~/mirror"]
        "#,
        primary = primary.display(),
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
/// primary, replica)`, with an extra line of settings for each.
fn exit_code_config(world: &World, groups: &[(&str, &Path, &str, &str)]) -> PathBuf {
    let config = world.path("config.toml");
    let mut text = String::new();
    for (name, primary, replica, extra) in groups {
        text.push_str(&format!(
            "[groups.{name}]\nmode = \"two-way-conflict\"\nprimary = \"{}\"\nreplicas = [\"{replica}\"]\n{extra}\n",
            primary.display()
        ));
    }
    fs::write(&config, text).unwrap();
    config
}

#[test]
fn sync_exits_zero_when_every_session_converged() {
    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "file.txt", "content");
    let replica_spec = replica.to_string_lossy().to_string();
    let config = exit_code_config(&world, &[("g", &primary, &replica_spec, "")]);
    let (code, text) = cli_code(&world, &config, &["sync"]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(read(&replica, "file.txt"), "content");
}

#[test]
fn sync_exits_two_when_a_conflict_remains() {
    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "file.txt", "original");
    let replica_spec = replica.to_string_lossy().to_string();
    let config = exit_code_config(&world, &[("g", &primary, &replica_spec, "")]);
    assert_eq!(cli_code(&world, &config, &["sync"]).0, Some(0));
    write(&primary, "file.txt", "v-primary");
    write(&replica, "file.txt", "v-replica");
    let (code, text) = cli_code(&world, &config, &["sync"]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("conflict"), "{text}");
}

#[test]
fn sync_exits_one_when_a_destination_is_unreachable() {
    let world = World::new();
    let primary = world.directory("primary");
    write(&primary, "file.txt", "content");
    let config = exit_code_config(
        &world,
        &[(
            "g",
            &primary,
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    let doomed = world.directory("doomed");
    write(&primary, "file.txt", "original");
    write(&doomed, "file.txt", "content");
    let replica_spec = replica.to_string_lossy().to_string();
    let only_conflicted = exit_code_config(&world, &[("g", &primary, &replica_spec, "")]);
    assert_eq!(cli_code(&world, &only_conflicted, &["sync"]).0, Some(0));
    write(&primary, "file.txt", "v-primary");
    write(&replica, "file.txt", "v-replica");
    let config = exit_code_config(
        &world,
        &[
            ("g", &primary, &replica_spec, ""),
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "file.txt", "original");
    let state = world.path("manual-state");
    let run = || {
        let output = std::process::Command::new(agent_binary())
            .arg("sync")
            .arg(&primary)
            .arg(&replica)
            .arg("--mode")
            .arg("two-way-conflict")
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
    write(&primary, "file.txt", "v-primary");
    write(&replica, "file.txt", "v-replica");
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "vendor/fix.patch", "patch");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[groups.g]\nmode = \"two-way-conflict\"\nprimary = \"{}\"\nreplicas = [\"{}\"]\n\
             ignores = [\"vendor\", \"!vendor/*.patch\"]\n",
            primary.display(),
            replica.display()
        ),
    )
    .unwrap();
    let (ok, text) = cli(&world, &config, &["sync"]);
    assert!(ok, "{text}");
    assert!(
        text.contains("warning: group 'g': !vendor/*.patch has no effect"),
        "{text}"
    );
    assert!(!replica.join("vendor").exists(), "{text}");
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
            "[groups.outer]\nmode = \"one-way-primary\"\nprimary = \"{}\"\nreplicas = [\"{}\"]\n\n\
             [groups.inner]\nmode = \"one-way-primary\"\nprimary = \"{}\"\nreplicas = [\"{}\"]\n",
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    let other_primary = world.directory("other-primary");
    let other_replica = world.directory("other-replica");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[groups.g]\nmode = \"{mode}\"\nprimary = \"{}\"\nreplicas = [\"{}\"]\n\n\
             [groups.other]\nmode = \"two-way-conflict\"\nprimary = \"{}\"\nreplicas = [\"{}\"]\n",
            primary.display(),
            replica.display(),
            other_primary.display(),
            other_replica.display()
        ),
    )
    .unwrap();
    (config, primary, replica)
}

#[test]
fn clean_keeps_a_disabled_sessions_state_so_enabling_resumes() {
    let world = World::new();
    let (config, primary, replica) = two_groups(&world, "two-way-conflict");
    write(&primary, "gone.txt", "content");
    assert!(cli(&world, &config, &["sync"]).0);
    assert_eq!(read(&replica, "gone.txt"), "content");

    // Deleted, then turned off before the deletion was carried.
    fs::remove_file(primary.join("gone.txt")).unwrap();
    set_group_enabled(&config, "g", false);
    let (ok, text) = cli(&world, &config, &["clean"]);
    assert!(ok, "{text}");
    assert!(!text.contains("removed session"), "{text}");

    set_group_enabled(&config, "g", true);
    let (ok, text) = cli(&world, &config, &["sync"]);
    assert!(ok, "{text}");
    // The ancestor survived, so the deletion is carried, not undone.
    assert!(!primary.join("gone.txt").exists(), "{text}");
    assert!(!replica.join("gone.txt").exists(), "{text}");
}

#[test]
fn clean_include_disabled_lists_the_disabled_session_and_plain_clean_does_not() {
    let world = World::new();
    let (config, primary, _) = two_groups(&world, "two-way-conflict");
    write(&primary, "file.txt", "content");
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
    let (config, primary, _) = two_groups(&world, "two-way-conflict");
    write(&primary, "file.txt", "content");
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
/// Two `clean`s at once over the same stale endpoint locks both succeed.
/// Each takes a lock before removing it, so the second can wait out the
/// first and then find the directory gone; that is success, not an error.
/// (Seen on CI, where parallel tests share a home directory.)
#[test]
fn concurrent_cleans_both_succeed_over_the_same_stale_locks() {
    let world = World::new();
    let (config, _, _) = two_groups(&world, "two-way-conflict");
    let home = world.keep.path().join("home");
    let locks = home.join(".autobahn").join("endpoint-locks");
    for round in 0..5 {
        for index in 0..40 {
            let lock = locks.join(format!("{round:02x}{index:030x}"));
            fs::create_dir_all(&lock).unwrap();
            fs::write(lock.join("lock"), "").unwrap();
        }
        let spawn = || {
            std::process::Command::new(agent_binary())
                .args(["clean", "--config"])
                .arg(&config)
                .arg("--state-root")
                .arg(world.state_root())
                .env("HOME", &home)
                .env_remove("AUTOBAHN_HOME")
                .output()
        };
        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(spawn);
            let second = scope.spawn(spawn);
            (first.join().unwrap(), second.join().unwrap())
        });
        for output in [first.expect("clean runs"), second.expect("clean runs")] {
            assert!(
                output.status.success(),
                "round {round}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(fs::read_dir(&locks).unwrap().count(), 0, "round {round}");
    }
}

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

/// A fan-out in conflict three ways: one file edited differently on primary
/// and on each of two destinations.
fn three_way_conflict(world: &World) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let primary = world.directory("primary");
    let b1 = world.directory("b1");
    let b2 = world.directory("b2");
    write(&primary, "notes.txt", "original");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[groups.r]\nmode = \"two-way-conflict\"\nprimary = \"{}\"\nreplicas = [\"{}\", \"{}\"]\n",
            primary.display(),
            b1.display(),
            b2.display()
        ),
    )
    .unwrap();
    assert!(cli(world, &config, &["sync"]).0, "the first sync converges");
    write(&primary, "notes.txt", "v-primary");
    write(&b1, "notes.txt", "v-b1");
    write(&b2, "notes.txt", "v-b2");
    cli(world, &config, &["sync"]);
    (config, primary, b1, b2)
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
    assert!(text.contains("primary  9 B"), "{text}");
    assert!(text.contains("--keep primary|"), "{text}");
}

#[test]
fn diff_shows_the_two_sides_by_group_or_by_file_path() {
    let world = World::new();
    let (config, primary, _, b2) = three_way_conflict(&world);
    let b2_spec = b2.to_string_lossy().to_string();

    let (_, by_group) = cli(
        &world,
        &config,
        &["diff", "r", "notes.txt", "--host", &b2_spec],
    );
    assert!(
        by_group.contains("-v-primary") && by_group.contains("+v-b2"),
        "{by_group}"
    );

    // Addressed by the file itself, from anywhere.
    let file = primary.join("notes.txt").to_string_lossy().to_string();
    let (_, by_path) = cli(&world, &config, &["diff", &file, "--host", &b2_spec]);
    assert!(
        by_path.contains("-v-primary") && by_path.contains("+v-b2"),
        "{by_path}"
    );
}

#[test]
fn resolve_keeping_one_destination_settles_the_whole_fan_out() {
    let world = World::new();
    let (config, primary, b1, b2) = three_way_conflict(&world);
    let b1_spec = b1.to_string_lossy().to_string();
    // Addressed by a path inside the root, and keeping b1's version: it
    // must reach primary *and* b2, whose own conflict is settled by it.
    let file = primary.join("notes.txt").to_string_lossy().to_string();
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", &file, "--keep", &b1_spec, "--yes"],
    );
    assert!(ok, "{text}");
    // Resolution retires the losing versions; the cycle carries the winner.
    // Two cycles, because b2's copy reaches it through primary.
    cli(&world, &config, &["sync"]);
    cli(&world, &config, &["sync"]);
    for root in [&primary, &b1, &b2] {
        assert_eq!(read(root, "notes.txt"), "v-b1");
    }
    let (_, after) = cli(&world, &config, &["conflicts"]);
    assert!(after.contains("nothing needs you"), "{after}");
}

#[test]
fn resolve_keeping_both_renames_the_loser_aside() {
    let world = World::new();
    let (config, primary, b1, b2) = three_way_conflict(&world);
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
    // Everything else is ordinary propagation: primary's version fills the
    // names the renames vacated, and each aside reaches the other roots.
    cli(&world, &config, &["sync"]);
    cli(&world, &config, &["sync"]);
    for root in [&primary, &b1, &b2] {
        assert_eq!(read(root, "notes.txt"), "v-primary");
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
    for keep in ["primary", "b1", "both"] {
        let world = World::new();
        let (config, primary, b1, _) = three_way_conflict(&world);
        // `tree` is a populated directory on one side and a file on the
        // other, both created since the ancestor: neither change is a
        // deletion, so it is a genuine conflict rather than a propagation.
        // Large enough that a mistake would trip the emptied-subtree halt.
        let (directory, file) = match keep {
            "b1" => (&primary, &b1),
            _ => (&b1, &primary),
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
            "primary" | "b1" => {
                assert_eq!(read(file, "tree"), "the file version");
                assert_eq!(read(directory, "tree"), "the file version");
                assert!(!directory.join("tree/inner").exists(), "the tree is gone");
            }
            // Both are kept: the loser's whole tree survives under a free
            // name, which is the thing a rename can do and a copy cannot.
            _ => {
                assert_eq!(read(&primary, "tree"), "the file version");
                assert_eq!(read(&primary, "tree.b1/inner/f0"), "held");
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
    let primary = world.directory("primary");
    let replica = world.directory("b1");
    write(&primary, "seed", "seed");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[defaults]\nmode = \"two-way-conflict\"\nignores = [\".git\", \"node_modules\"]\n\
             [groups.r]\nprimary = \"{}\"\nreplicas = [\"{}\"]\n",
            primary.display(),
            replica.display()
        ),
    )
    .unwrap();
    assert!(cli(&world, &config, &["sync"]).0);

    write(&replica, "project/.git/HEAD", "ref");
    write(&replica, "project/node_modules/dep.js", "dep");
    write(&replica, "project/src/main.rs", "fn main() {}");
    cli(&world, &config, &["sync"]);
    assert_eq!(read(&primary, "project/src/main.rs"), "fn main() {}");
    assert!(
        !primary.join("project/.git").exists(),
        "ignored, so not sent"
    );

    fs::remove_dir_all(primary.join("project")).unwrap();
    let (_, first) = cli(&world, &config, &["sync"]);

    // The whole tree goes, ignored content included. An ignore says which
    // files synchronization *carries*, not which files exist; deleting a
    // directory is an instruction about the directory, and taking the
    // source while leaving the `.git` and the `node_modules` obeys
    // neither reading — the tree is not deleted, and what stays is litter
    // synchronization can never clear.
    assert!(
        !replica.join("project").exists(),
        "the tree evaporated: {first}"
    );

    // And nothing is reported as an obstacle on the way.
    assert!(!first.contains("blocked"), "{first}");

    for _ in 0..3 {
        let (_, text) = cli(&world, &config, &["sync"]);
        assert!(text.contains("0 change(s) to primary"), "quiet: {text}");
        assert!(text.contains("0 change(s) to replica"), "quiet: {text}");
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
    let outer_primary = world.directory("outer-primary");
    let outer_replica = world.directory("outer-replica");
    let inner_replica = world.directory("inner-replica");

    let outer = world.path("outer.toml");
    fs::write(
        &outer,
        format!(
            "[groups.outer]\nmode = \"two-way-conflict\"\nignores = [\"nested\"]\n\
             primary = \"{}\"\nreplicas = [\"{}\"]\n",
            outer_primary.display(),
            outer_replica.display()
        ),
    )
    .unwrap();
    // The inner session's root lives inside the outer session's ignored
    // path, which is the only reason the outer session may delete it.
    let inner = world.path("inner.toml");
    fs::write(
        &inner,
        format!(
            "[groups.inner]\nmode = \"two-way-conflict\"\nprimary = \"{}\"\nreplicas = [\"{}\"]\n",
            outer_replica.join("proj/nested").display(),
            inner_replica.display()
        ),
    )
    .unwrap();

    // A sibling, so deleting `proj` is not also emptying the root — that
    // trips a different guard and would prove nothing about this one.
    write(&outer_primary, "other.txt", "keep");
    write(&outer_primary, "proj/src/main.rs", "code");
    assert!(cli(&world, &outer, &["sync"]).0);
    write(&outer_replica, "proj/nested/data.txt", "precious");
    assert!(cli(&world, &inner, &["sync"]).0);
    assert_eq!(read(&inner_replica, "data.txt"), "precious");

    fs::remove_dir_all(outer_primary.join("proj")).unwrap();
    let (ok, text) = cli(&world, &outer, &["sync"]);
    assert!(ok, "{text}");
    // The tree went whole, the nested root with it.
    assert!(!outer_replica.join("proj").exists(), "{text}");

    // The nested session refuses to carry that loss any further.
    let (ok, text) = cli(&world, &inner, &["sync"]);
    assert!(!ok, "the nested session must not succeed: {text}");
    assert!(
        text.contains("halted") && text.contains("is missing"),
        "{text}"
    );
    assert_eq!(read(&inner_replica, "data.txt"), "precious");
}

/// A conflict whose losing side holds ignored content settles like any
/// other. The removal takes what synchronization knows about and leaves
/// the rest, and what remains is invisible — the same rule the cycle
/// follows for an ordinary deletion. `resolve` must not be stricter than
/// the cycle it stands in for.
#[test]
fn resolve_settles_a_conflict_whose_loser_holds_ignored_content() {
    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("b1");
    write(&primary, "seed", "seed");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[defaults]\nmode = \"two-way-conflict\"\nignores = [\".git\"]\n\
             [groups.r]\nprimary = \"{}\"\nreplicas = [\"{}\"]\n",
            primary.display(),
            replica.display()
        ),
    )
    .unwrap();
    assert!(cli(&world, &config, &["sync"]).0);

    // A genuine disagreement, not a deletion: a file on one side and a
    // project directory on the other, both new since the ancestor.
    write(&primary, "project", "primary's file");
    write(&replica, "project/.git/HEAD", "ref");
    write(&replica, "project/src/main.rs", "fn main() {}");
    cli(&world, &config, &["sync"]);
    let (_, listed) = cli(&world, &config, &["conflicts"]);
    assert!(listed.contains("project"), "a conflict: {listed}");

    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "project", "--keep", "primary", "--yes"],
    );
    assert!(ok, "{text}");
    assert!(text.contains("settled 1 of 1"), "{text}");
    for _ in 0..3 {
        cli(&world, &config, &["sync"]);
    }

    // Primary's version won everywhere, and the loser's tree went whole —
    // resolution follows the same rule the cycle does, so a `.git` inside
    // the losing version is no more of an obstacle here than there.
    assert_eq!(read(&primary, "project"), "primary's file");
    assert_eq!(read(&replica, "project"), "primary's file");
    let (_, after) = cli(&world, &config, &["conflicts"]);
    assert!(after.contains("nothing needs you"), "{after}");
}

/// A group of one primary and one replica in `mode`, holding `keep.txt` and a
/// sibling (so no guard about emptied roots is in play), synchronized once.
fn one_pair(world: &World, mode: &str) -> (PathBuf, PathBuf, PathBuf) {
    let primary = world.directory("primary");
    let replica = world.directory("b1");
    write(&primary, "keep.txt", "original");
    write(&primary, "other.txt", "other");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[groups.r]\nmode = \"{mode}\"\nprimary = \"{}\"\nreplicas = [\"{}\"]\n",
            primary.display(),
            replica.display()
        ),
    )
    .unwrap();
    assert!(cli(world, &config, &["sync"]).0, "the first sync converges");
    assert_eq!(read(&replica, "keep.txt"), "original");
    (config, primary, replica)
}

/// Resolving a path both sides already agree on retires nothing. Before
/// the guard it retired replica's copy, and the next cycle read that as a
/// deletion against an unchanged primary and took the file from both sides.
#[test]
fn resolving_an_in_sync_path_twice_keeps_it_everywhere() {
    let world = World::new();
    let (config, primary, replica) = one_pair(&world, "two-way-conflict");
    for _ in 0..2 {
        let (ok, text) = cli(
            &world,
            &config,
            &["resolve", "r", "keep.txt", "--keep", "primary", "--yes"],
        );
        assert!(ok, "{text}");
        assert!(text.contains("already the same on every side"), "{text}");
        cli(&world, &config, &["sync"]);
    }
    cli(&world, &config, &["sync"]);
    assert_eq!(read(&primary, "keep.txt"), "original");
    assert_eq!(read(&replica, "keep.txt"), "original");
}

/// The root is never a path to resolve: retiring it would retire the
/// whole synchronizable tree.
#[test]
fn resolving_the_root_is_refused() {
    let world = World::new();
    let (config, primary, replica) = one_pair(&world, "two-way-conflict");
    write(&replica, "keep.txt", "replica's edit");
    for root in ["./", ".", ""] {
        let (ok, text) = cli(
            &world,
            &config,
            &["resolve", "r", root, "--keep", "primary", "--yes"],
        );
        assert!(!ok, "resolving {root:?} must fail: {text}");
        assert!(text.contains("not the root"), "{text}");
    }
    assert_eq!(read(&primary, "keep.txt"), "original");
    assert_eq!(read(&replica, "keep.txt"), "replica's edit");
    assert_eq!(read(&replica, "other.txt"), "other");
}

/// Keeping a side that has not changed since the last sync makes every
/// side match it. Stage 1 refused this: removing the other copy read as a
/// deletion against an untouched file, and the deletion propagated.
/// Forgetting the path in the ancestor makes the kept version a creation.
#[test]
fn keeping_an_unchanged_side_makes_every_side_match_it() {
    let world = World::new();
    let (config, primary, replica) = one_pair(&world, "two-way-conflict");
    write(&replica, "keep.txt", "replica's edit");
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "keep.txt", "--keep", "primary", "--yes"],
    );
    assert!(ok, "{text}");
    assert!(text.contains("settled 1 of 1"), "{text}");
    for _ in 0..3 {
        assert!(cli(&world, &config, &["sync"]).0);
        assert_eq!(read(&primary, "keep.txt"), "original");
        assert_eq!(read(&replica, "keep.txt"), "original");
    }

    // `--keep both` keeps the unchanged name, and the copy moved aside.
    let world = World::new();
    let (config, primary, replica) = one_pair(&world, "two-way-conflict");
    write(&replica, "keep.txt", "replica's edit");
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "keep.txt", "--keep", "both", "--yes"],
    );
    assert!(ok, "{text}");
    for _ in 0..3 {
        assert!(cli(&world, &config, &["sync"]).0);
        for root in [&primary, &replica] {
            assert_eq!(read(root, "keep.txt"), "original");
            assert_eq!(read(root, "keep.txt.b1"), "replica's edit");
        }
    }
}

/// A one-way mode never carries replica's content to primary, so retiring
/// primary's copy cannot make replica's version win.
#[test]
fn keeping_replica_in_a_one_way_mode_is_refused() {
    let world = World::new();
    let (config, primary, replica) = one_pair(&world, "one-way-primary");
    write(&primary, "keep.txt", "primary's edit");
    let replica_spec = replica.to_string_lossy().to_string();
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "keep.txt", "--keep", &replica_spec, "--yes"],
    );
    assert!(!ok, "{text}");
    assert!(text.contains("one-way-primary"), "{text}");
    assert_eq!(read(&primary, "keep.txt"), "primary's edit");
    assert_eq!(read(&replica, "keep.txt"), "original");
}

/// In two-way-primary-strict primary's deletion beats replica's edit, so stage 1
/// refused to keep replica there. With the path forgotten, replica's version is
/// a creation, which flows to primary in that mode like any other.
#[test]
fn keeping_replica_in_the_strict_mode_wins() {
    let world = World::new();
    let (config, primary, replica) = one_pair(&world, "two-way-primary-strict");
    write(&primary, "keep.txt", "primary's edit");
    let replica_spec = replica.to_string_lossy().to_string();
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "keep.txt", "--keep", &replica_spec, "--yes"],
    );
    assert!(ok, "{text}");
    assert!(text.contains("settled 1 of 1"), "{text}");
    for _ in 0..3 {
        assert!(cli(&world, &config, &["sync"]).0);
        assert_eq!(read(&primary, "keep.txt"), "original");
        assert_eq!(read(&replica, "keep.txt"), "original");
    }
}

/// What a path holds on the two sides when it is resolved.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// The same file on both sides, as last synchronized.
    Agreed,
    /// The losing side edited the file; the winner's is as last synced.
    LoserEdited,
    /// Both sides edited the file, differently.
    Conflict,
    /// A directory both sides edited: a file inside it differently, and
    /// the loser added one.
    Directory,
}

/// Resolves `shape` in `mode`, keeping `keep` (`primary`, `replica` or `both`),
/// and checks the outcome over three more cycles.
fn resolve_in(mode: &str, keep: &str, shape: Shape) {
    let label = format!("{mode}, keeping {keep}, {shape:?}");
    let world = World::new();
    let (config, primary, replica) = one_pair(&world, mode);
    let path = match shape {
        Shape::Directory => {
            for n in 0..9 {
                write(&primary, &format!("tree/f{n}"), "original");
            }
            assert!(cli(&world, &config, &["sync"]).0, "{label}");
            assert_eq!(read(&replica, "tree/f8"), "original", "{label}");
            "tree"
        }
        _ => "keep.txt",
    };
    let (winner, loser) = match keep {
        "replica" => (&replica, &primary),
        _ => (&primary, &replica),
    };
    match shape {
        Shape::Agreed => {}
        Shape::LoserEdited => write(loser, "keep.txt", "loser's edit"),
        Shape::Conflict => {
            write(winner, "keep.txt", "winner's edit");
            write(loser, "keep.txt", "loser's edit");
        }
        Shape::Directory => {
            write(winner, "tree/f0", "winner's edit");
            write(loser, "tree/f0", "loser's edit");
            write(loser, "tree/extra", "loser's own");
        }
    }

    let replica_spec = replica.to_string_lossy().to_string();
    let argument = match keep {
        "replica" => replica_spec.as_str(),
        other => other,
    };
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", path, "--keep", argument, "--yes"],
    );
    let one_way = mode.starts_with("one-way");
    if (one_way && keep == "replica") || (mode == "one-way-primary" && keep == "both") {
        assert!(!ok, "{label}: {text}");
        assert!(text.contains(mode), "{label}: {text}");
        assert!(text.contains("Nothing was changed"), "{label}: {text}");
        return;
    }
    assert!(ok, "{label}: {text}");
    match shape {
        Shape::Agreed => assert!(text.contains("already the same"), "{label}: {text}"),
        _ => assert!(text.contains("settled 1 of 1"), "{label}: {text}"),
    }

    // The loser's version, kept aside, reaches primary too — except in the
    // one mode that never carries replica's additions.
    let aside_roots: Vec<&PathBuf> = match mode {
        "one-way-conflict" => vec![&replica],
        _ => vec![&primary, &replica],
    };
    for cycle in 1..=3 {
        let (ok, synced) = cli(&world, &config, &["sync"]);
        assert!(ok, "{label}, cycle {cycle}: {synced}");
        let context = format!("{label}, cycle {cycle}");
        for root in [&primary, &replica] {
            assert_eq!(read(root, "other.txt"), "other", "{context}");
            match shape {
                Shape::Agreed | Shape::LoserEdited => {
                    assert_eq!(read(root, "keep.txt"), "original", "{context}")
                }
                Shape::Conflict => {
                    assert_eq!(read(root, "keep.txt"), "winner's edit", "{context}")
                }
                Shape::Directory => {
                    assert_eq!(read(root, "tree/f0"), "winner's edit", "{context}");
                    assert_eq!(read(root, "tree/f8"), "original", "{context}");
                    assert!(!root.join("tree/extra").exists(), "{context}");
                }
            }
        }
        if keep != "both" {
            continue;
        }
        for root in [&primary, &replica] {
            let expected = aside_roots.contains(&root);
            let (aside, content) = match shape {
                Shape::Agreed => {
                    assert!(!root.join("keep.txt.b1").exists(), "{context}");
                    continue;
                }
                Shape::Directory => ("tree.b1/extra", "loser's own"),
                _ => ("keep.txt.b1", "loser's edit"),
            };
            match expected {
                true => assert_eq!(read(root, aside), content, "{context}"),
                false => assert!(!root.join(aside).exists(), "{context}"),
            }
        }
    }
}

/// Every mode, every winner, one shape: the stage 2 matrix.
fn resolve_in_every_mode(shape: Shape) {
    for mode in [
        "two-way-conflict",
        "two-way-primary",
        "two-way-primary-strict",
        "one-way-conflict",
        "one-way-primary",
    ] {
        for keep in ["primary", "replica", "both"] {
            resolve_in(mode, keep, shape);
        }
    }
}

#[test]
fn resolving_an_agreed_file_keeps_it_in_every_mode() {
    resolve_in_every_mode(Shape::Agreed);
}

#[test]
fn resolving_toward_an_unchanged_winner_works_in_every_mode() {
    resolve_in_every_mode(Shape::LoserEdited);
}

#[test]
fn resolving_a_conflict_works_in_every_mode() {
    resolve_in_every_mode(Shape::Conflict);
}

#[test]
fn resolving_a_directory_works_in_every_mode() {
    resolve_in_every_mode(Shape::Directory);
}

/// Keeping a destination that has not changed since the last sync, named
/// alone, while primary and the other destination edited: every side ends
/// with the winner's version, the unnamed destination included.
#[test]
fn keeping_an_unchanged_destination_reaches_every_other_one() {
    let world = World::new();
    let primary = world.directory("primary");
    let b1 = world.directory("b1");
    let b2 = world.directory("b2");
    write(&primary, "notes.txt", "original");
    write(&primary, "other.txt", "other");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[groups.r]\nmode = \"two-way-conflict\"\nprimary = \"{}\"\nreplicas = [\"{}\", \"{}\"]\n",
            primary.display(),
            b1.display(),
            b2.display()
        ),
    )
    .unwrap();
    assert!(cli(&world, &config, &["sync"]).0);
    write(&primary, "notes.txt", "v-primary");
    write(&b2, "notes.txt", "v-b2");
    let b1_spec = b1.to_string_lossy().to_string();
    let (ok, text) = cli(
        &world,
        &config,
        &[
            "resolve",
            "r",
            "notes.txt",
            "--keep",
            &b1_spec,
            "--host",
            &b1_spec,
            "--yes",
        ],
    );
    assert!(ok, "{text}");
    assert!(text.contains("settled 1 of 1"), "{text}");
    // Two cycles to reach b2 through primary, then one more to be sure.
    for _ in 0..3 {
        assert!(cli(&world, &config, &["sync"]).0);
    }
    for root in [&primary, &b1, &b2] {
        assert_eq!(read(root, "notes.txt"), "original");
        assert_eq!(read(root, "other.txt"), "other");
    }
    let (_, after) = cli(&world, &config, &["conflicts"]);
    assert!(after.contains("nothing needs you"), "{after}");
}

/// With a supervisor running, `resolve` hands the resolution to it: each
/// session applies its part between its cycles, under its own lock, and
/// cycles. A fan-out whose winner is one destination ends with the
/// winner's version on every side, without a `sync`.
#[test]
fn a_running_supervisor_applies_a_resolution_across_the_fan_out() {
    let world = World::new();
    let (config, primary, b1, b2) = three_way_conflict(&world);
    let text = fs::read_to_string(&config).unwrap();
    let plans = world.plans(&text);
    fs::write(&config, &text).unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let supervisor = Supervisor::new(plans, world.state_root(), false);
        let stop_ref = &stop;
        let watcher = scope.spawn(move || supervisor.run_watch(stop_ref));
        let _guard = StopGuard(stop_ref);
        assert!(
            wait_until(Duration::from_secs(15), || {
                autobahn::supervisor::control::supervisor_is_running(&world.state_root())
            }),
            "the supervisor listens"
        );

        let b1_spec = b1.to_string_lossy().to_string();
        let (ok, text) = cli(
            &world,
            &config,
            &["resolve", "r", "notes.txt", "--keep", &b1_spec, "--yes"],
        );
        assert!(ok, "{text}");
        assert!(text.contains("settled 1 of 1"), "{text}");
        assert!(text.contains("copying the kept version across"), "{text}");
        assert!(
            wait_until(Duration::from_secs(20), || {
                [&primary, &b1, &b2].iter().all(|root| {
                    fs::read_to_string(root.join("notes.txt")).ok().as_deref() == Some("v-b1")
                })
            }),
            "every side ends with b1's version"
        );

        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });
    let (_, after) = cli(&world, &config, &["conflicts"]);
    assert!(after.contains("nothing needs you"), "{after}");
}

/// A file named `--all`, settled the way the shop and the tray settle a
/// row, settles that file and nothing else. Passed without a separator,
/// the name was parsed as the flag and every conflict in the group was
/// settled.
#[test]
fn settling_a_file_named_like_a_flag_settles_only_that_file() {
    let world = World::new();
    let (config, primary, b1, _) = three_way_conflict(&world);
    write(&primary, "--all", "a");
    write(&b1, "--all", "b");
    cli(&world, &config, &["sync"]);
    let (_, listed) = cli(&world, &config, &["conflicts"]);
    assert!(listed.contains("--all"), "{listed}");

    let command = autobahn::invocation::resolve_command("r", "primary", &["--all".to_owned()]);
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
    let primary = world.directory("primary");
    let replica = world.directory("b1");
    write(&primary, "seed", "seed");
    let config = world.path("config.toml");
    fs::write(
        &config,
        format!(
            "[groups.r]\nmode = \"two-way-conflict\"\nprimary = \"{}\"\nreplicas = [\"{}\"]\n",
            primary.display(),
            replica.display()
        ),
    )
    .unwrap();
    assert!(cli(&world, &config, &["sync"]).0);
    let name = "evil\x1b]52;c;cHduZWQ=\x07\r\x1b[2Jsettled.txt";
    write(&primary, name, "a");
    write(&replica, name, "b");
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

    // Standard output alone: a warning on standard error (a configuration
    // group-writable under umask 002, say) is not part of the report.
    let output = std::process::Command::new(agent_binary())
        .args(["conflicts", "--json", "--config"])
        .arg(&config)
        .arg("--state-root")
        .arg(world.state_root())
        .output()
        .expect("the CLI runs");
    let json = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{json}");
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

/// Keeping one destination's version where primary has no copy at all:
/// primary's side has nothing to retire, and the other destination must
/// still be checked as though primary already holds the winner's version,
/// which is what it will hold once the winning session has run.
#[test]
fn keeping_a_destination_where_primary_has_no_copy_settles_the_fan_out() {
    let world = World::new();
    let (config, primary, b1, b2) = three_way_conflict(&world);
    write(&primary, "fresh.txt", "seed");
    cli(&world, &config, &["sync"]);
    fs::remove_file(primary.join("fresh.txt")).unwrap();
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
    for root in [&primary, &b1, &b2] {
        assert_eq!(read(root, "fresh.txt"), "v-b1");
    }
}

/// A folder and a path inside it, named together, are one resolution.
#[test]
fn naming_a_folder_and_a_path_inside_it_settles_the_folder_once() {
    let world = World::new();
    let (config, primary, replica) = one_pair(&world, "two-way-conflict");
    write(&primary, "d/f", "primary");
    write(&replica, "d/f", "replica");
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "d", "d/f", "--keep", "primary", "--yes"],
    );
    assert!(ok, "{text}");
    assert!(text.contains("settled 1 of 1"), "{text}");
    for _ in 0..2 {
        cli(&world, &config, &["sync"]);
    }
    assert_eq!(read(&replica, "d/f"), "primary");
}

#[test]
fn resolve_all_requires_a_winner_and_asks_first() {
    let world = World::new();
    let (config, primary, b1, _) = three_way_conflict(&world);
    write(&primary, "more.txt", "a");
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
        &["resolve", "r", "--all", "--keep", "primary"],
    );
    assert!(!ok && text.contains("pass --yes"), "{text}");
    assert_eq!(read(&b1, "more.txt"), "b");
    let (ok, text) = cli(
        &world,
        &config,
        &["resolve", "r", "--all", "--keep", "primary", "--yes"],
    );
    assert!(ok, "{text}");
    cli(&world, &config, &["sync"]);
    assert_eq!(read(&b1, "more.txt"), "a");
    assert_eq!(read(&b1, "notes.txt"), "v-primary");
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    for index in 0..64 {
        write(&primary, &format!("file{index:02}.txt"), "content");
    }

    let plans = world.plans(&format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        interval = 1
        replicas = ["{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
    ));
    let plan = plans[0].clone();

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let supervisor = Supervisor::new(plans, world.state_root(), false);
        let stop_ref = &stop;
        let watcher = scope.spawn(move || supervisor.run_watch(stop_ref));
        let _guard = StopGuard(stop_ref);

        assert!(
            wait_until(Duration::from_secs(20), || replica
                .join("file00.txt")
                .exists()),
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
        // there is no honest estimate, only elapsed time. Waited for, not
        // read at once: the session can be caught waiting right after the
        // cycle that filled replica, before any scan of replica since — its total
        // is then the empty root it first saw, until the next cycle (a
        // heartbeat, at most a second here) scans it again.
        let totals = || {
            control::query_progress(&world.state_root()).and_then(|live| {
                Some((
                    live[0].progress.primary.expected?,
                    live[0].progress.replica.expected?,
                ))
            })
        };
        assert!(
            wait_until(Duration::from_secs(20), || {
                totals().is_some_and(|(primary, replica)| primary == replica)
            }),
            "both sides leave a total: {:?}",
            totals()
        );
        let (primary_entries, replica_entries) = totals().expect("both sides have totals");
        assert!(
            primary_entries >= 65,
            "the total counts the tree: {primary_entries}"
        );
        assert_eq!(
            primary_entries, replica_entries,
            "synchronized trees hold the same number of entries"
        );

        // The totals are recorded alongside the status, so the next run's
        // first scan starts with a yardstick rather than without one.
        let status = world.status(&plan).expect("a status is recorded");
        assert_eq!(status.primary_entries, primary_entries);
        assert_eq!(status.replica_entries, replica_entries);

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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    let evidence = world.directory("evidence");
    let fired = evidence.join("fired");

    let configuration = format!(
        r#"
        on_alert = "cat > {fired}.stdin; printf '%s' \"$AUTOBAHN_SUMMARY|$AUTOBAHN_STATES|$AUTOBAHN_EVENT\" > {fired}"

        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        interval = 1
        replicas = ["{replica}"]

        # Deliberately the old spelling of the section: the supervisor
        # still reads a file written before `[experimental]` was named,
        # and this is where that is proved end to end.
        [experimental.alerts]
        alert_after = "1s"
        # This case is about the hook running at all. Coalescing has its
        # own tests; without this the window would hold the hook for a
        # minute and the case would be timing out on the wrong rule.
        coalesce_after = "0s"
        "#,
        primary = primary.display(),
        replica = replica.display(),
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
        write(&primary, "shared.txt", "from primary");
        assert!(
            wait_until(Duration::from_secs(20), || replica
                .join("shared.txt")
                .exists()),
            "the initial content should synchronize"
        );
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            !fired.exists(),
            "a healthy supervisor must run nothing at all"
        );

        // Now make the two sides disagree about the same file.
        write(&primary, "shared.txt", "primary's version");
        write(&replica, "shared.txt", "replica's version");

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
/// agent's `~/.autobahn/p2p` is not the leader's: in production they
/// are on different machines, and the leader's own p2p directory holds
/// its term while the agent's holds the lease it was given.
fn p2p_agent_script(world: &World, agent_home: &Path) -> PathBuf {
    let script = world.path("p2p-agent.sh");
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

/// P2P, phase 3: a leading supervisor presents its lease, pushes the
/// follower's files, and keeps the replica's ancestor copy level — all of it
/// visible on the replica's host afterwards.
#[test]
fn a_p2p_leader_pushes_its_lease_files_and_ancestor_to_the_replica() {
    use autobahn::supervisor::P2pContext;

    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    let agent_home = world.directory("agent-home");
    write(&primary, "hello.txt", "hello");
    let script = p2p_agent_script(&world, &agent_home);
    let configuration = format!(
        r#"
        [groups.g]
        mode = "p2p-conflict-dangerously-experimental"
        primary = "{primary}"
        agent_command = "{script}"
        replicas = ["peer:{replica}"]
        "#,
        primary = primary.display(),
        script = script.display(),
        replica = replica.display(),
    );
    let plans = world.plans(&configuration);
    let plan = plans[0].clone();
    let leader_directory = world.path("leader-p2p");
    let context = || {
        P2pContext::for_primary(
            world.path("config.toml"),
            leader_directory.clone(),
            autobahn::config::DEFAULT_P2P_TTL,
        )
        .expect("a p2p context")
    };

    let outcomes = Supervisor::new(plans.clone(), world.state_root(), false)
        .with_p2p(context())
        .run_once();
    assert_all_synchronized(&outcomes);
    assert_eq!(read(&replica, "hello.txt"), "hello");

    // The replica's host now holds everything a follower needs.
    let p2p = agent_home.join(".autobahn").join("p2p");
    let lease = autobahn::p2p::read_lease(&p2p)
        .expect("lease readable")
        .expect("a lease was written");
    assert_eq!((lease.leader.as_str(), lease.term), ("primary", 1));
    assert_eq!(
        fs::read_to_string(p2p.join("name")).expect("name"),
        plan.replica_spec()
    );
    assert_eq!(
        fs::read_to_string(p2p.join("config.toml")).expect("config"),
        configuration
    );
    let copy = autobahn::p2p::ancestor_copy_path(&p2p, &plan.identifier())
        .expect("a plan's identifier is a session identifier");
    assert!(
        copy.exists(),
        "the ancestor copy exists at {}",
        copy.display()
    );

    // The leader remembers its own term, and the status says what it is.
    let own = autobahn::p2p::read_lease(&leader_directory)
        .expect("lease readable")
        .expect("the leader's own lease");
    assert_eq!((own.leader.as_str(), own.term), ("primary", 1));
    let status = world.status(&plan).expect("a status");
    assert_eq!((status.role.as_str(), status.term), ("leader", 1));
    assert_eq!(status.state, "synchronized");

    // A change on the primary reaches the replica, and the copy follows the
    // ancestor: it stands at the same generation the leader does.
    write(&primary, "more.txt", "more");
    let outcomes = Supervisor::new(plans, world.state_root(), false)
        .with_p2p(context())
        .run_once();
    assert_all_synchronized(&outcomes);
    assert_eq!(read(&replica, "more.txt"), "more");
    let lease = autobahn::p2p::read_lease(&p2p)
        .expect("lease readable")
        .expect("renewed");
    assert_eq!(lease.term, 1, "the same leader keeps its term");
}

/// P2P, phase 3: a host whose lease names a newer leader refuses the
/// old one, which steps down before a byte moves and stays down across a
/// restart.
#[test]
fn a_fenced_p2p_leader_steps_down_and_stays_down() {
    use autobahn::p2p::{write_lease, Lease};
    use autobahn::supervisor::P2pContext;
    use std::time::Duration;

    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    let agent_home = world.directory("agent-home");
    write(&primary, "hello.txt", "hello");
    let script = p2p_agent_script(&world, &agent_home);
    let plans = world.plans(&format!(
        r#"
        [groups.g]
        mode = "p2p-conflict-dangerously-experimental"
        primary = "{primary}"
        agent_command = "{script}"
        replicas = ["peer:{replica}"]
        "#,
        primary = primary.display(),
        script = script.display(),
        replica = replica.display(),
    ));
    let plan = plans[0].clone();
    // The replica led at term 9 while the primary was away.
    let p2p = agent_home.join(".autobahn").join("p2p");
    write_lease(
        &p2p,
        &Lease::new(&plan.replica_spec(), 9, Duration::from_secs(30)),
    )
    .expect("the replica's lease");

    let leader_directory = world.path("leader-p2p");
    let outcomes = Supervisor::new(plans.clone(), world.state_root(), false)
        .with_p2p(
            P2pContext::for_primary(
                world.path("config.toml"),
                leader_directory.clone(),
                autobahn::config::DEFAULT_P2P_TTL,
            )
            .expect("a p2p context"),
        )
        .run_once();
    assert!(outcomes[0].result.is_err(), "{:?}", outcomes[0].result);
    assert!(
        !replica.join("hello.txt").exists(),
        "a fenced leader writes nothing"
    );
    let status = world.status(&plan).expect("a status");
    assert_eq!(status.state, "following", "{status:?}");
    assert_eq!((status.role.as_str(), status.term), ("follower", 9));
    // The replica's lease is untouched, and the primary recorded it as its own.
    let held = autobahn::p2p::read_lease(&p2p)
        .expect("readable")
        .expect("held");
    assert_eq!(
        (held.leader.as_str(), held.term),
        (plan.replica_spec().as_str(), 9)
    );
    let own = autobahn::p2p::read_lease(&leader_directory)
        .expect("readable")
        .expect("recorded");
    assert_eq!(own.term, 9);

    // A restart reads its own lease and comes back as a follower: it does
    // not connect, and the replica is still untouched.
    let context = P2pContext::for_primary(
        world.path("config.toml"),
        leader_directory,
        autobahn::config::DEFAULT_P2P_TTL,
    )
    .expect("a p2p context");
    assert!(matches!(
        context.role(),
        autobahn::p2p::Role::Follower { term: 9, .. }
    ));
    let outcomes = Supervisor::new(plans, world.state_root(), false)
        .with_p2p(context)
        .run_once();
    assert!(outcomes[0].result.is_err());
    assert!(!replica.join("hello.txt").exists());
    assert_eq!(world.status(&plan).expect("a status").state, "following");
}

/// With `manage_keys`, the primary sets up the replicas' keys to one another
/// over the logins it already has: each replica makes a key of its own, and is
/// given every other replica's, forced through the gate, inside autobahn's
/// block of its `authorized_keys` — with the gate itself, and the others'
/// host keys. Never its own.
#[test]
fn the_primary_gives_each_replica_the_others_keys_through_the_gate() {
    use autobahn::supervisor::P2pContext;

    let world = World::new();
    let primary = world.directory("primary");
    let homes = [world.directory("one-home"), world.directory("two-home")];
    let roots = [world.directory("one"), world.directory("two")];
    let scripts = [p2p_agent_script(&world, &homes[0]), {
        let script = world.path("p2p-agent-two.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nHOME={home} exec {agent} agent\n",
                home = homes[1].display(),
                agent = agent_binary()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&script, permissions).unwrap();
        script
    }];
    let plans = world.plans(&format!(
        r#"
        [experimental.p2p-dangerously-experimental]
        manage_keys = true

        [groups.g1]
        mode = "p2p-conflict-dangerously-experimental"
        primary = "{primary}/g1"
        agent_command = "{one_script}"
        replicas = ["one:{one}"]

        [groups.g2]
        mode = "p2p-conflict-dangerously-experimental"
        primary = "{primary}/g2"
        agent_command = "{two_script}"
        replicas = ["two:{two}"]
        "#,
        primary = primary.display(),
        one_script = scripts[0].display(),
        two_script = scripts[1].display(),
        one = roots[0].display(),
        two = roots[1].display(),
    ));
    fs::create_dir_all(primary.join("g1")).unwrap();
    fs::create_dir_all(primary.join("g2")).unwrap();
    let supervisor = Supervisor::new(plans, world.state_root(), false).with_p2p(
        P2pContext::for_primary(
            world.path("config.toml"),
            world.path("primary-p2p"),
            autobahn::config::DEFAULT_P2P_TTL,
        )
        .expect("a p2p context"),
    );
    // Twice: the first pass learns each replica's key, and whichever replica was
    // given its block first is given it again with the other's in it.
    assert_all_synchronized(&supervisor.run_once());
    assert_all_synchronized(&supervisor.run_once());

    let public = |home: &Path| {
        let text = fs::read_to_string(home.join(".autobahn/p2p/id_ed25519.pub"))
            .expect("a p2p key was made");
        let mut words = text.split_whitespace();
        format!("{} {}", words.next().unwrap(), words.next().unwrap())
    };
    for (index, home) in homes.iter().enumerate() {
        let other = &homes[1 - index];
        let authorized = fs::read_to_string(home.join(".ssh/authorized_keys"))
            .expect("autobahn's block was written");
        assert!(
            authorized.contains(&format!("{} {}", autobahn::peerkeys::FORCED, public(other))),
            "{authorized}"
        );
        assert!(
            !authorized.contains(&public(home)),
            "never its own: {authorized}"
        );
        assert!(home
            .join(".autobahn/bin")
            .join(autobahn::peerkeys::GATE_BINARY)
            .is_file());
        assert!(home.join(".autobahn/p2p/known_hosts").is_file());
    }
}

/// While the primary follows, its plain groups keep running: they are the
/// primary's alone, whoever leads the star. Only its p2p sessions wait
/// for the lead to come back.
#[test]
fn plain_groups_keep_running_while_the_primary_follows() {
    use autobahn::p2p::{self, write_lease, Lease};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    let plain = world.directory("plain");
    let mirror = world.directory("plain-mirror");
    let agent_home = world.directory("agent-home");
    write(&primary, "hello.txt", "hello");
    let script = p2p_agent_script(&world, &agent_home);
    let plans = world.plans(&format!(
        r#"
        [groups.g]
        mode = "p2p-conflict-dangerously-experimental"
        interval = 1
        primary = "{primary}"
        agent_command = "{script}"
        replicas = ["peer:{replica}"]

        [groups.plain]
        mode = "two-way-conflict"
        interval = 1
        primary = "{plain}"
        replicas = ["{mirror}"]
        "#,
        primary = primary.display(),
        script = script.display(),
        replica = replica.display(),
        plain = plain.display(),
        mirror = mirror.display(),
    ));
    let p2p_plan = plans
        .iter()
        .find(|plan| plan.group == "g")
        .expect("the p2p group")
        .clone();
    // The replica led at term 9 while the primary was away.
    write_lease(
        &agent_home.join(".autobahn").join("p2p"),
        &Lease::new(&p2p_plan.replica_spec(), 9, Duration::from_secs(30)),
    )
    .expect("the replica's lease");
    // There is no leader to attach to here: attaching fails at once.
    let _attach = EnvironmentGuard::set(p2p::ATTACH_COMMAND_VARIABLE, "false");

    let stop = AtomicBool::new(false);
    let alerts = autobahn::alerts::AlertPlan::default();
    let directory = world.path("primary-p2p");
    let state_root = world.state_root();
    let config_path = world.path("config.toml");
    std::thread::scope(|scope| {
        let _guard = StopGuard(&stop);
        let run = scope.spawn(|| {
            autobahn::supervisor::peer::run_primary(
                &config_path,
                &directory,
                &plans,
                &alerts,
                &state_root,
                false,
                &stop,
                None,
            )
        });
        assert!(
            wait_until(Duration::from_secs(20), || world
                .status(&p2p_plan)
                .is_some_and(|status| status.state == "following")),
            "the primary should be fenced and follow"
        );
        write(&plain, "while-following.txt", "plain");
        assert!(
            wait_until(Duration::from_secs(20), || mirror
                .join("while-following.txt")
                .exists()),
            "the plain group should keep running while the primary follows"
        );
        assert!(
            !replica.join("hello.txt").exists(),
            "the p2p group writes nothing while it follows"
        );
        stop.store(true, Ordering::Relaxed);
        run.join()
            .expect("the primary thread")
            .expect("the primary ran");
    });
}

/// P2P, phase 4: a peer whose lease has been stale for its wait takes
/// the lead at the next term, runs the leader's star turned around, and
/// reaches the other replica; the old leader, back at its old term, is fenced.
#[test]
fn a_peer_takes_the_lead_when_the_lease_goes_stale() {
    use autobahn::p2p::{self, Lease};
    use autobahn::supervisor::P2pContext;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    let world = World::new();
    // Three machines: the primary (never dialed here), this peer, and one
    // other replica. Each replica has its own home, so its agent's p2p
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

    // What the primary pushed to this peer: its own star, with a lease
    // lifetime and a wait short enough for a test.
    let pushed = format!(
        r#"
        [experimental.p2p-dangerously-experimental]
        ttl = "2s"
        failover_after = "2s"

        [groups.g]
        mode = "p2p-conflict-dangerously-experimental"
        interval = 1
        primary = "/nonexistent/primary"
        agent_command = "{script}"
        replicas = ["peer:{peer_root}", "other:{other_root}"]
        "#,
        script = other_script.display(),
        peer_root = peer_root.display(),
        other_root = other_root.display(),
    );
    let name = format!("peer:{}", peer_root.display());
    let p2p_directory = peer_home.join(".autobahn").join("p2p");
    p2p::write_pushed_file(&p2p_directory, "config.toml", pushed.as_bytes()).unwrap();
    // The pushed agent_command is never run: how this peer reaches the
    // other replica is its own to say.
    fs::write(
        peer_home.join(".autobahn").join("host.toml"),
        format!("agent_command = {:?}\n", other_script.display().to_string()),
    )
    .unwrap();
    p2p::write_pushed_file(&p2p_directory, "name", name.as_bytes()).unwrap();
    // The primary's lease, last renewed a while ago.
    let stale = Lease {
        leader: p2p::PRIMARY.to_owned(),
        term: 3,
        renewed_at: p2p::now_seconds().saturating_sub(120),
        ttl_seconds: 1,
    };
    p2p::write_lease(&p2p_directory, &stale).unwrap();

    let stop = AtomicBool::new(false);
    let state_root = world.state_root();
    std::thread::scope(|scope| {
        let _guard = StopGuard(&stop);
        let peer = scope
            .spawn(|| autobahn::supervisor::peer::run(&p2p_directory, &state_root, false, &stop));
        // The peer takes the lead and its file reaches the other replica.
        assert!(
            wait_until(Duration::from_secs(20), || other_root
                .join("from-peer.txt")
                .exists()),
            "the peer's file should reach the other replica"
        );
        let own = p2p::read_lease(&p2p_directory)
            .expect("readable")
            .expect("written");
        assert_eq!((own.leader.as_str(), own.term), (name.as_str(), 4));
        let theirs = other_home.join(".autobahn").join("p2p");
        assert!(
            wait_until(Duration::from_secs(10), || {
                p2p::read_lease(&theirs)
                    .ok()
                    .flatten()
                    .is_some_and(|lease| lease.term == 4 && lease.leader == name)
            }),
            "the other replica holds the peer's lease"
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
        // other replica: it steps down, writes nothing.
        let old_primary = world.directory("primary-root");
        write(&old_primary, "from-primary.txt", "late");
        let plans = world.plans(&format!(
            r#"
            [groups.g]
            mode = "p2p-conflict-dangerously-experimental"
            primary = "{primary}"
            agent_command = "{script}"
            replicas = ["other:{other_root}"]
            "#,
            primary = old_primary.display(),
            script = other_script.display(),
            other_root = other_root.display(),
        ));
        let primary_directory = world.path("primary-p2p");
        p2p::write_lease(
            &primary_directory,
            &Lease::new(p2p::PRIMARY, 3, Duration::from_secs(30)),
        )
        .unwrap();
        let outcomes = Supervisor::new(plans.clone(), world.path("primary-state"), false)
            .with_p2p(
                P2pContext::for_primary(
                    world.path("config.toml"),
                    primary_directory.clone(),
                    autobahn::config::DEFAULT_P2P_TTL,
                )
                .expect("context"),
            )
            .run_once();
        assert!(outcomes[0].result.is_err(), "{:?}", outcomes[0].result);
        assert!(!other_root.join("from-primary.txt").exists());
        let recorded = p2p::read_lease(&primary_directory)
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

/// P2P, phase 5, the whole loop from the primary's side. A replica leads;
/// the primary comes back and dials the replica as it always did, is fenced,
/// and steps down; it then dials in and attaches as an agent; the replica
/// runs their session over the attachment and, once it settles, hands
/// the lead back; the primary leads again and dials the replica as before.
#[test]
fn the_primary_attaches_to_a_leading_peer_and_gets_the_lead_back() {
    use autobahn::p2p::{self, Lease};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    let world = World::new();
    let primary_root = world.directory("primary-root");
    let peer_root = world.directory("peer-root");
    let peer_home = world.directory("peer-home");
    write(&primary_root, "from-primary.txt", "from the primary");
    write(&peer_root, "from-peer.txt", "from the peer");
    let peer_script = p2p_agent_script(&world, &peer_home);

    // What the primary pushed to the peer before it went away: a star of
    // one replica, its own root as the primary, and the session's identifier.
    let configuration = format!(
        r#"
        [experimental.p2p-dangerously-experimental]
        ttl = "2s"
        failover_after = "2s"

        [groups.g]
        mode = "p2p-conflict-dangerously-experimental"
        interval = 1
        primary = "{primary_root}"
        agent_command = "{script}"
        replicas = ["peer:{peer_root}"]
        "#,
        primary_root = primary_root.display(),
        script = peer_script.display(),
        peer_root = peer_root.display(),
    );
    let plans = world.plans(&configuration);
    let name = format!("peer:{}", peer_root.display());
    let p2p_directory = peer_home.join(".autobahn").join("p2p");
    p2p::write_pushed_file(&p2p_directory, "config.toml", configuration.as_bytes()).unwrap();
    p2p::write_pushed_file(&p2p_directory, "name", name.as_bytes()).unwrap();
    p2p::write_pushed_file(
        &p2p_directory,
        "sessions/g",
        plans[0].identifier().as_bytes(),
    )
    .unwrap();
    p2p::write_lease(
        &p2p_directory,
        &Lease {
            leader: p2p::PRIMARY.to_owned(),
            term: 3,
            renewed_at: p2p::now_seconds().saturating_sub(120),
            ttl_seconds: 2,
        },
    )
    .unwrap();
    // The primary remembers leading at term 3, and reaches the peer's attach
    // socket directly rather than over ssh. The primary runs in this process,
    // so the variable is set process-wide, under the guard that restores it.
    let primary_directory = p2p::directory().expect("the primary's p2p directory");
    p2p::write_lease(
        &primary_directory,
        &Lease::new(p2p::PRIMARY, 3, Duration::from_secs(30)),
    )
    .unwrap();
    let socket = p2p_directory.join(p2p::ATTACH_SOCKET);
    let _attach = EnvironmentGuard::set(
        p2p::ATTACH_COMMAND_VARIABLE,
        format!(
            "{} p2p attach --socket {}",
            agent_binary(),
            socket.display()
        ),
    );

    let stop = AtomicBool::new(false);
    let peer_state = world.state_root();
    let primary_state = world.path("primary-state");
    let alerts = autobahn::alerts::AlertPlan::default();
    std::thread::scope(|scope| {
        let _guard = StopGuard(&stop);
        let peer = scope
            .spawn(|| autobahn::supervisor::peer::run(&p2p_directory, &peer_state, true, &stop));
        assert!(
            wait_until(Duration::from_secs(20), || socket.exists()),
            "the peer should lead and listen for the primary"
        );

        // The primary comes back.
        let primary = scope.spawn(|| {
            autobahn::supervisor::peer::run_primary(
                &world.path("config.toml"),
                &primary_directory,
                &plans,
                &alerts,
                &primary_state,
                true,
                &stop,
                None,
            )
        });

        // Fenced, attached, synchronized both ways over the attachment.
        assert!(
            wait_until(Duration::from_secs(30), || peer_root
                .join("from-primary.txt")
                .exists()
                && primary_root.join("from-peer.txt").exists()),
            "the attached session should carry both roots' files"
        );
        // The lead comes back to the primary at the next term, on both hosts.
        assert!(
            wait_until(Duration::from_secs(30), || {
                let theirs = p2p::read_lease(&p2p_directory).ok().flatten();
                let mine = p2p::read_lease(&primary_directory).ok().flatten();
                theirs.is_some_and(|l| l.leader == p2p::PRIMARY && l.term == 5)
                    && mine.is_some_and(|l| l.leader == p2p::PRIMARY && l.term == 5)
            }),
            "the lead should come back to the primary at term 5"
        );
        // The primary leads again the ordinary way: it dials the peer's
        // agent, and a new file crosses; the peer's lease stays fresh
        // because the primary renews it every cycle.
        write(&primary_root, "after.txt", "after the handback");
        assert!(
            wait_until(Duration::from_secs(30), || peer_root
                .join("after.txt")
                .exists()),
            "the primary should lead again and reach the peer"
        );
        std::thread::sleep(Duration::from_secs(3));
        let theirs = p2p::read_lease(&p2p_directory)
            .expect("readable")
            .expect("held");
        assert_eq!((theirs.leader.as_str(), theirs.term), (p2p::PRIMARY, 5));
        assert!(
            !theirs.is_stale_at(p2p::now_seconds()),
            "the primary keeps the peer's lease fresh: {theirs:?}"
        );
        let status = autobahn::supervisor::peer::read_status(&p2p_directory)
            .expect("readable")
            .expect("written");
        assert_eq!(status.standing, "fresh", "{status:?}");

        stop.store(true, Ordering::Relaxed);
        peer.join().expect("the peer thread").expect("the peer ran");
        primary
            .join()
            .expect("the primary thread")
            .expect("the primary ran");
    });
}

/// The primary dials in to a leading replica once, and every p2p group it
/// shares with that replica syncs over the one connection, each session on
/// channels of its own. Taken whole by the first session, the connection
/// left the primary's other groups unsynced until the lead came back.
#[test]
fn every_group_syncs_over_the_primaries_one_attachment() {
    use autobahn::p2p::{self, Lease};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    let world = World::new();
    let primaries = [world.directory("primary1"), world.directory("primary2")];
    let peers = [world.directory("peer1"), world.directory("peer2")];
    let peer_home = world.directory("peer-home");
    write(&peers[0], "from-peer1.txt", "p1");
    write(&peers[1], "from-peer2.txt", "p2");
    let peer_script = p2p_agent_script(&world, &peer_home);
    let configuration = format!(
        r#"
        [experimental.p2p-dangerously-experimental]
        ttl = "2s"
        failover_after = "2s"

        [groups.g1]
        mode = "p2p-conflict-dangerously-experimental"
        interval = 1
        primary = "{a1}"
        agent_command = "{script}"
        replicas = ["peer:{p1}"]

        [groups.g2]
        mode = "p2p-conflict-dangerously-experimental"
        interval = 1
        primary = "{a2}"
        agent_command = "{script}"
        replicas = ["peer:{p2}"]
        "#,
        a1 = primaries[0].display(),
        a2 = primaries[1].display(),
        script = peer_script.display(),
        p1 = peers[0].display(),
        p2 = peers[1].display(),
    );
    let plans = world.plans(&configuration);
    let plan = |group: &str| {
        plans
            .iter()
            .find(|plan| plan.group == group)
            .expect("the group's plan")
            .clone()
    };
    let directory = peer_home.join(".autobahn").join("p2p");
    p2p::write_pushed_file(&directory, "config.toml", configuration.as_bytes()).unwrap();
    for (group, root) in [("g1", &peers[0]), ("g2", &peers[1])] {
        let name = format!("peer:{}", root.display());
        p2p::write_pushed_file(&directory, &format!("names/{group}"), name.as_bytes()).unwrap();
        p2p::write_pushed_file(&directory, "name", name.as_bytes()).unwrap();
        p2p::write_pushed_file(
            &directory,
            &format!("sessions/{group}"),
            plan(group).identifier().as_bytes(),
        )
        .unwrap();
    }
    p2p::write_lease(
        &directory,
        &Lease {
            leader: p2p::PRIMARY.to_owned(),
            term: 3,
            renewed_at: p2p::now_seconds().saturating_sub(120),
            ttl_seconds: 2,
        },
    )
    .unwrap();
    let primary_directory = p2p::directory().expect("the primary's p2p directory");
    p2p::write_lease(
        &primary_directory,
        &Lease::new(p2p::PRIMARY, 3, Duration::from_secs(30)),
    )
    .unwrap();
    let socket = directory.join(p2p::ATTACH_SOCKET);
    let _attach = EnvironmentGuard::set(
        p2p::ATTACH_COMMAND_VARIABLE,
        format!(
            "{} p2p attach --socket {}",
            agent_binary(),
            socket.display()
        ),
    );

    let stop = AtomicBool::new(false);
    let peer_state = world.state_root();
    let primary_state = world.path("primary-state");
    let alerts = autobahn::alerts::AlertPlan::default();
    std::thread::scope(|scope| {
        let _guard = StopGuard(&stop);
        let peer =
            scope.spawn(|| autobahn::supervisor::peer::run(&directory, &peer_state, false, &stop));
        assert!(wait_until(Duration::from_secs(20), || socket.exists()));
        let primary = scope.spawn(|| {
            autobahn::supervisor::peer::run_primary(
                &world.path("config.toml"),
                &primary_directory,
                &plans,
                &alerts,
                &primary_state,
                false,
                &stop,
                None,
            )
        });
        // Each group's copy of its ancestor on the primary is written by the
        // leading replica's session with the attached primary, after a cycle
        // over the attachment: both, for both groups.
        let written_by_the_peer = |group: &str| {
            p2p::copy_writer(&primary_directory, &plan(group).identifier())
                .ok()
                .flatten()
                .is_some_and(|writer| writer.starts_with("peer:"))
        };
        assert!(
            wait_until(Duration::from_secs(30), || written_by_the_peer("g1")
                && written_by_the_peer("g2")),
            "both groups should sync with the primary while the replica leads: g1 {}, g2 {}",
            written_by_the_peer("g1"),
            written_by_the_peer("g2")
        );
        assert!(wait_until(Duration::from_secs(30), || primaries[0]
            .join("from-peer1.txt")
            .exists()
            && primaries[1].join("from-peer2.txt").exists()));
        stop.store(true, Ordering::Relaxed);
        peer.join().expect("the peer thread").expect("the peer ran");
        primary
            .join()
            .expect("the primary thread")
            .expect("the primary ran");
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
        primary = "{quiet}"
        replicas = ["{quiet_mirror}"]

        [groups.noisy]
        primary = "{noisy}"
        replicas = ["{noisy_mirror}"]
        "#,
        quiet = quiet.display(),
        quiet_mirror = quiet_mirror.display(),
        noisy = noisy.display(),
        noisy_mirror = noisy_mirror.display(),
    ));
    assert_all_synchronized(&world.run_once(plans.clone()));
    // A conflict in one group only.
    write(&noisy, "b.txt", "primary's");
    write(&noisy_mirror, "b.txt", "replica's");
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "keep.txt", "keep");
    write(&primary, "gone.txt", "gone");
    let config = world.path("config.toml");
    let plans = world.plans(&format!(
        r#"
        [groups.work]
        mode = "two-way-conflict"
        primary = "{primary}"
        replicas = ["{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
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
    fs::remove_file(primary.join("gone.txt")).unwrap();
    let sessions = world.state_root().join("sessions");
    let before: Vec<(PathBuf, std::time::SystemTime)> = walk_files(&sessions);
    let (ok, text) = cli(&world, &config, &["doctor", "work"]);
    assert!(ok, "{text}");
    assert!(
        text.contains("delete gone.txt to replica"),
        "the next cycle: {text}"
    );
    assert!(
        text.contains("copy gone.txt to primary"),
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

/// An atomic deploy swap on the primary, seen by the watcher as three
/// renamed names and nothing inside them. Reproduced before the fix: the
/// replica kept the old `live/` contents, duplicated them into `old/`, and
/// deleted `staging/`, until a full walk minutes later.
#[test]
fn a_swapped_directory_reaches_the_replica_with_its_new_contents() {
    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "live/f1", "old content");
    write(&primary, "live/sub/f2", "old deeper");
    write(&primary, "staging/f1", "new content");
    write(&primary, "staging/sub/f2", "new deeper");

    let mut plans = world.plans(&format!(
        r#"
        [groups.work]
        primary = "{primary}"
        mode = "two-way-conflict"
        replicas = ["{replica}"]
        "#,
        primary = primary.display(),
        replica = replica.display(),
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
                replica.join("staging/sub/f2").exists() && replica.join("live/sub/f2").exists()
            }),
            "initial content should propagate"
        );
        // Let the first cycles settle, so the swap is seen by an
        // incremental scan against a baseline that holds both trees.
        std::thread::sleep(Duration::from_millis(500));

        fs::rename(primary.join("live"), primary.join("old")).expect("rename");
        fs::rename(primary.join("staging"), primary.join("live")).expect("rename");

        let settled = || {
            !replica.join("staging").exists()
                && fs::read_to_string(replica.join("live/f1")).ok().as_deref()
                    == Some("new content")
                && fs::read_to_string(replica.join("live/sub/f2"))
                    .ok()
                    .as_deref()
                    == Some("new deeper")
                && fs::read_to_string(replica.join("old/f1")).ok().as_deref() == Some("old content")
                && fs::read_to_string(replica.join("old/sub/f2"))
                    .ok()
                    .as_deref()
                    == Some("old deeper")
        };
        assert!(
            wait_until(Duration::from_secs(15), settled),
            "the replica should hold the swapped tree: live/f1 = {:?}, old/f1 = {:?}, staging = {}",
            fs::read_to_string(replica.join("live/f1")).ok(),
            fs::read_to_string(replica.join("old/f1")).ok(),
            replica.join("staging").exists(),
        );

        stop.store(true, Ordering::Relaxed);
        watcher
            .join()
            .expect("the watcher should stop cleanly")
            .expect("supervision should succeed");
    });
}

/// A machine with a configuration of its own is never a peer: a `name` a
/// leader pushed into its p2p directory is ignored with a warning, and
/// its own configuration runs. It used to refuse to start and tell the
/// user to move one of the two aside — and a name a hostile leader pushed
/// would have them move their own.
#[test]
fn a_stray_peer_name_beside_a_configuration_is_ignored_with_a_warning() {
    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "first.txt", "first");
    let home = world.directory("home");
    let state = home.join(".autobahn");
    fs::create_dir_all(state.join("p2p")).expect("the p2p directory");
    fs::write(state.join("p2p").join("name"), "hostile:/x").expect("a stray name");
    fs::write(
        state.join("config.toml"),
        format!(
            "[groups.work]\nprimary = \"{}\"\nmode = \"two-way-conflict\"\ninterval = 1\nreplicas = [\"{}\"]\n",
            primary.display(),
            replica.display()
        ),
    )
    .expect("configuration should be writable");
    let log = world.path("watch.log");
    let child = std::process::Command::new(agent_binary())
        .args(["watch", "--log"])
        .env("HOME", &home)
        .env("AUTOBAHN_HOME", &state)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(fs::File::create(&log).expect("a log"))
        .spawn()
        .expect("watch starts");
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Kill(child);
    assert!(
        wait_until(Duration::from_secs(15), || replica
            .join("first.txt")
            .exists()),
        "its own configuration runs: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );
    assert!(child.0.try_wait().expect("waitable").is_none());
    let said = fs::read_to_string(&log).expect("the log");
    assert!(
        said.contains("names this machine as a peer") && said.contains("is never a peer"),
        "{said}"
    );
    assert!(!said.contains("aside"), "{said}");
}

#[test]
fn watch_keeps_synchronizing_after_its_standard_output_closes() {
    // `autobahn watch | head -1`: the reader goes away, and every later
    // log line meets a closed pipe. That costs the lines, not the sessions.
    let world = World::new();
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "first.txt", "first");
    let path = world.path("config.toml");
    fs::write(
        &path,
        format!(
            "[groups.work]\nprimary = \"{}\"\nmode = \"two-way-conflict\"\ninterval = 1\nreplicas = [\"{}\"]\n",
            primary.display(),
            replica.display()
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
        wait_until(Duration::from_secs(15), || replica
            .join("first.txt")
            .exists()),
        "the first file synchronizes"
    );
    for index in 0..3 {
        let name = format!("later-{index}.txt");
        write(&primary, &name, "later");
        assert!(
            wait_until(Duration::from_secs(15), || replica.join(&name).exists()),
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    let notes = world.directory("notes");
    let notes_mirror = world.path("notes-mirror");
    write(&primary, "file.txt", "file");
    write(&notes, "todo.txt", "todo");
    let path = world.path("config.toml");
    let one = format!(
        "[groups.work]\nprimary = \"{}\"\nmode = \"two-way-conflict\"\ninterval = 1\nreplicas = [\"{}\"]\n",
        primary.display(),
        replica.display()
    );
    world.plans(&one);
    let shown_groups = || -> Vec<String> {
        autobahn::supervisor::shown_plans(&path, &world.state_root())
            .map(|shown| shown.plans.into_iter().map(|plan| plan.group).collect())
            .unwrap_or_default()
    };

    supervise_with_reload(&world, &path, || {
        assert!(wait_until(Duration::from_secs(15), || replica
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
                "{one}[groups.notes]\nmode = \"two-way-conflict\"\nprimary = \"{}\"\nreplicas = [\"{}\"]\n",
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
    let primary = world.directory("primary");
    let replica = world.directory("replica");
    write(&primary, "file.txt", "file");
    let path = world.path("config.toml");
    let plans = world.plans(&format!(
        "[groups.work]\nprimary = \"{}\"\nmode = \"two-way-conflict\"\nreplicas = [\"{}\"]\n",
        primary.display(),
        replica.display()
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
