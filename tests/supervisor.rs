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
        assert!(
            wait_until(Duration::from_secs(15), || beta.join("file.txt").exists()),
            "the watcher should be running and synchronized"
        );

        // A second "process" attempting the same session is refused while
        // the first holds the lock.
        let outcomes = world.run_once(plans);
        let error = outcomes[0]
            .result
            .as_ref()
            .expect_err("the session lock must refuse a concurrent run");
        assert!(error.contains("another autobahn process"), "{error}");

        stop.store(true, Ordering::Relaxed);
        watcher.join().expect("the watcher should stop cleanly");
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
    assert!(error.contains("unable to resolve alpha root"), "{error}");
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
        watcher.join().expect("the watcher should stop cleanly");
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
        watcher.join().expect("the watcher should stop cleanly");
    });
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
