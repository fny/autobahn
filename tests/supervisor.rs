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

/// Returns the path of the autobahn binary under test (used as the agent).
fn agent_binary() -> &'static str {
    env!("CARGO_BIN_EXE_autobahn")
}

/// A temporary world for one test: a directory holding synchronization
/// roots, the configuration file, and the supervisor state root.
struct World {
    keep: TempDir,
}

impl World {
    fn new() -> World {
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
        disabled = ["down-host"]

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
    assert_eq!(status.state, "error");
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
    let outcomes = world.run_once(plans);
    let error = outcomes[0]
        .result
        .as_ref()
        .expect_err("the session should fail");
    assert!(
        error.contains("alpha root") && error.contains("does not exist"),
        "{error}"
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
                    .is_some_and(|status| status.state == "error")
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
    // install flow (which streams the agent binary through stdin).
    let script_dir = world.directory("fake-bin");
    let script = script_dir.join("ssh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             while [ $# -gt 0 ]; do case \"$1\" in -o) shift 2;; --) shift; break;; *) break;; esac; done\n\
             shift\n\
             HOME={home} exec /bin/sh -c \"$*\"\n",
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

    // These variables are consulted only by the SSH connection path, which
    // only this test exercises (every other test connects via
    // agent_command); tests in this binary can therefore run in parallel.
    std::env::set_var("AUTOBAHN_SSH", &script);
    std::env::set_var("AUTOBAHN_AGENTS_DIR", &agents);

    let plans = world.plans(&format!(
        r#"
        [groups.work]
        alpha = "{alpha}"
        mode = "two-way-safe"
        betas = ["fake-host:{remote_mirror}"]
        "#,
        alpha = alpha.display(),
        remote_mirror = remote_mirror.display(),
    ));
    let outcomes = world.run_once(plans.clone());
    assert_all_synchronized(&outcomes);

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
    assert_all_synchronized(&world.run_once(plans));
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
