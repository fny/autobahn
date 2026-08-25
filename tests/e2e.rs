//! End-to-end synchronization tests.
//!
//! Every scenario here synchronizes real on-disk trees and verifies real
//! on-disk results. Sessions are constructed fresh for every cycle, so
//! ancestor persistence (the heart of three-way reconciliation) is exercised
//! continuously rather than assumed. Scenarios run over the agent transport —
//! a spawned `autobahn agent` subprocess speaking the wire protocol over
//! stdio, which is byte-for-byte the SSH code path minus the ssh wrapper —
//! and, where marked, additionally over in-process local endpoints.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use autobahn::endpoint::local::LocalEndpoint;
use autobahn::endpoint::remote::RemoteEndpoint;
use autobahn::endpoint::Endpoint;
use autobahn::scan::IgnoreSet;
use autobahn::session::{CycleReport, SafetyHalt, Session};
use autobahn::transport::Connection;
use autobahn::tree::SyncMode;

/// The transports a scenario can run over.
#[derive(Clone, Copy, PartialEq)]
enum Transport {
    /// Both endpoints in-process.
    Local,
    /// Beta behind a spawned agent subprocess (the SSH code path).
    Agent,
}

/// A test harness holding the roots and session state for one scenario.
struct Harness {
    _keep: tempfile::TempDir,
    alpha: PathBuf,
    beta: PathBuf,
    state: PathBuf,
    mode: SyncMode,
    transport: Transport,
    ignores: Vec<String>,
    session_counter: u32,
}

impl Harness {
    fn new(mode: SyncMode, transport: Transport) -> Harness {
        let keep = tempfile::tempdir().expect("tempdir");
        let alpha = keep.path().join("alpha");
        let beta = keep.path().join("beta");
        let state = keep.path().join("state");
        fs::create_dir_all(&alpha).unwrap();
        fs::create_dir_all(&beta).unwrap();
        Harness {
            _keep: keep,
            alpha,
            beta,
            state,
            mode,
            transport,
            ignores: Vec::new(),
            session_counter: 0,
        }
    }

    fn with_ignores(mut self, ignores: &[&str]) -> Harness {
        self.ignores = ignores.iter().map(|s| s.to_string()).collect();
        self
    }

    /// Runs one synchronization cycle through a freshly constructed session
    /// (proving that all cross-cycle state lives in persisted form).
    fn cycle(&mut self) -> anyhow::Result<CycleReport> {
        self.session_counter += 1;
        let alpha_endpoint: Box<dyn Endpoint + Send> = Box::new(
            LocalEndpoint::new(
                self.alpha.clone(),
                self.state.join("staging-alpha"),
                IgnoreSet::new(&self.ignores)?,
            )
            .expect("alpha endpoint"),
        );
        let beta_endpoint: Box<dyn Endpoint + Send> = match self.transport {
            Transport::Local => Box::new(
                LocalEndpoint::new(
                    self.beta.clone(),
                    self.state.join("staging-beta"),
                    IgnoreSet::new(&self.ignores)?,
                )
                .expect("beta endpoint"),
            ),
            Transport::Agent => {
                let binary = env!("CARGO_BIN_EXE_autobahn").to_owned();
                let connection =
                    Connection::spawn(&[binary, "agent".to_owned()]).expect("spawn agent");
                Box::new(
                    RemoteEndpoint::connect(
                        connection,
                        self.beta.to_string_lossy().into_owned(),
                        format!(
                            "e2e-{}-{}",
                            self.state.to_string_lossy().len(),
                            blake3::hash(self.state.to_string_lossy().as_bytes()).to_hex()
                        ),
                        self.ignores.clone(),
                    )
                    .expect("connect agent"),
                )
            }
        };
        let mut session = Session::new(alpha_endpoint, beta_endpoint, self.mode, self.state.clone())?;
        session.run_cycle()
    }

    fn cycle_ok(&mut self) -> CycleReport {
        self.cycle().expect("cycle")
    }

    fn assert_trees_equal(&self, context: &str) {
        let alpha = hash_tree(&self.alpha);
        let beta = hash_tree(&self.beta);
        assert_eq!(alpha, beta, "{context}: alpha and beta trees differ");
    }
}

/// Builds a deterministic content digest of a directory hierarchy: names,
/// types, executability, file contents, and symlink targets.
fn hash_tree(root: &Path) -> String {
    fn walk(path: &Path, relative: &str, hasher: &mut blake3::Hasher) {
        let metadata = fs::symlink_metadata(path).expect("stat");
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(path).expect("readlink");
            hasher.update(format!("L|{relative}|{}\n", target.display()).as_bytes());
        } else if metadata.is_file() {
            let executable = metadata.permissions().mode() & 0o111 != 0;
            hasher.update(format!("F|{relative}|{executable}|").as_bytes());
            hasher.update(&fs::read(path).expect("read"));
            hasher.update(b"\n");
        } else if metadata.is_dir() {
            hasher.update(format!("D|{relative}\n").as_bytes());
            let mut entries: Vec<_> = fs::read_dir(path)
                .expect("readdir")
                .map(|e| e.expect("entry").file_name())
                .collect();
            entries.sort();
            for name in entries {
                let child = path.join(&name);
                let child_relative = format!("{relative}/{}", name.to_string_lossy());
                walk(&child, &child_relative, hasher);
            }
        }
    }
    let mut hasher = blake3::Hasher::new();
    walk(root, "", &mut hasher);
    hasher.finalize().to_hex().to_string()
}

/// Populates a standard mixed tree: nested directories, distinct file
/// contents, an executable, and a symlink.
fn build_tree(root: &Path) {
    for d in 0..4 {
        let dir = root.join(format!("dir{d}")).join("nested");
        fs::create_dir_all(&dir).unwrap();
        for f in 0..3 {
            fs::write(dir.join(format!("file{f}.txt")), format!("content {d}/{f}")).unwrap();
        }
        fs::write(dir.join("tool.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("tool.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("file0.txt", dir.join("link")).unwrap();
    }
    fs::create_dir_all(root.join("empty")).unwrap();
    fs::create_dir_all(root.join("ünïcode")).unwrap();
    fs::write(root.join("ünïcode").join("ファイル.txt"), "unicode").unwrap();
}

/// All four synchronization modes.
const ALL_MODES: [SyncMode; 4] = [
    SyncMode::TwoWaySafe,
    SyncMode::TwoWayResolved,
    SyncMode::OneWaySafe,
    SyncMode::OneWayReplica,
];

#[test]
fn initial_sync_and_alpha_propagation_all_modes_over_agent() {
    for mode in ALL_MODES {
        let mut harness = Harness::new(mode, Transport::Agent);
        build_tree(&harness.alpha);
        harness.cycle_ok();
        harness.assert_trees_equal("initial");

        // Modify alpha: change, add, remove, chmod, retarget a symlink.
        fs::write(harness.alpha.join("dir0/nested/file0.txt"), "changed").unwrap();
        fs::write(harness.alpha.join("dir1/nested/new.txt"), "new").unwrap();
        fs::remove_file(harness.alpha.join("dir2/nested/file1.txt")).unwrap();
        fs::set_permissions(
            harness.alpha.join("dir3/nested/file2.txt"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::remove_file(harness.alpha.join("dir0/nested/link")).unwrap();
        std::os::unix::fs::symlink("file2.txt", harness.alpha.join("dir0/nested/link")).unwrap();
        fs::create_dir_all(harness.alpha.join("dir1/nested/added")).unwrap();
        fs::write(harness.alpha.join("dir1/nested/added/inner.txt"), "inner").unwrap();

        harness.cycle_ok();
        harness.assert_trees_equal("alpha propagation");

        // A further cycle must be a no-op.
        let report = harness.cycle_ok();
        assert!(!report.changed(), "steady state should be a no-op ({mode:?})");
    }
}

#[test]
fn initial_sync_and_alpha_propagation_local_endpoints() {
    for mode in [SyncMode::TwoWaySafe, SyncMode::OneWayReplica] {
        let mut harness = Harness::new(mode, Transport::Local);
        build_tree(&harness.alpha);
        harness.cycle_ok();
        harness.assert_trees_equal("initial");
        fs::write(harness.alpha.join("dir0/nested/file0.txt"), "changed").unwrap();
        harness.cycle_ok();
        harness.assert_trees_equal("alpha propagation");
    }
}

#[test]
fn beta_addition_semantics_by_mode() {
    for mode in ALL_MODES {
        let mut harness = Harness::new(mode, Transport::Agent);
        build_tree(&harness.alpha);
        harness.cycle_ok();

        let beta_added = harness.beta.join("dir0/nested/beta-added.txt");
        let alpha_added = harness.alpha.join("dir0/nested/beta-added.txt");
        fs::write(&beta_added, "from beta").unwrap();
        harness.cycle_ok();

        match mode {
            SyncMode::TwoWaySafe | SyncMode::TwoWayResolved => {
                assert!(alpha_added.exists(), "{mode:?}: addition should propagate");
                harness.assert_trees_equal("beta addition");
            }
            SyncMode::OneWaySafe => {
                assert!(!alpha_added.exists(), "one-way-safe must not reverse-propagate");
                assert!(beta_added.exists(), "one-way-safe must preserve beta additions");
            }
            SyncMode::OneWayReplica => {
                assert!(!beta_added.exists(), "replica must remove beta additions");
                harness.assert_trees_equal("replica mirroring");
            }
        }
    }
}

#[test]
fn beta_modification_semantics_by_mode() {
    for mode in ALL_MODES {
        let mut harness = Harness::new(mode, Transport::Agent);
        build_tree(&harness.alpha);
        harness.cycle_ok();

        let path = "dir1/nested/file0.txt";
        fs::write(harness.beta.join(path), "modified on beta").unwrap();
        let report = harness.cycle_ok();

        match mode {
            SyncMode::TwoWaySafe | SyncMode::TwoWayResolved => {
                let alpha_content = fs::read_to_string(harness.alpha.join(path)).unwrap();
                assert_eq!(alpha_content, "modified on beta", "{mode:?}");
                harness.assert_trees_equal("beta modification");
            }
            SyncMode::OneWaySafe => {
                // The modification is preserved on beta (reported as a
                // conflict) and never reaches alpha.
                let beta_content = fs::read_to_string(harness.beta.join(path)).unwrap();
                assert_eq!(beta_content, "modified on beta");
                let alpha_content = fs::read_to_string(harness.alpha.join(path)).unwrap();
                assert_eq!(alpha_content, "content 1/0");
                assert!(!report.conflicts.is_empty(), "expected a conflict report");
            }
            SyncMode::OneWayReplica => {
                let beta_content = fs::read_to_string(harness.beta.join(path)).unwrap();
                assert_eq!(beta_content, "content 1/0", "replica must overwrite");
                harness.assert_trees_equal("replica overwrite");
            }
        }
    }
}

#[test]
fn beta_deletion_semantics_by_mode() {
    for mode in [SyncMode::TwoWaySafe, SyncMode::OneWayReplica] {
        let mut harness = Harness::new(mode, Transport::Agent);
        build_tree(&harness.alpha);
        harness.cycle_ok();

        let path = "dir2/nested/file2.txt";
        fs::remove_file(harness.beta.join(path)).unwrap();
        harness.cycle_ok();

        match mode {
            SyncMode::TwoWaySafe => {
                assert!(
                    !harness.alpha.join(path).exists(),
                    "deletion should propagate to alpha"
                );
                harness.assert_trees_equal("beta deletion");
            }
            SyncMode::OneWayReplica => {
                assert!(
                    harness.beta.join(path).exists(),
                    "replica must restore beta deletions"
                );
                harness.assert_trees_equal("replica restoration");
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn divergent_edits_conflict_in_safe_mode_and_resolve_in_resolved_mode() {
    for (mode, alpha_wins) in [(SyncMode::TwoWaySafe, false), (SyncMode::TwoWayResolved, true)] {
        let mut harness = Harness::new(mode, Transport::Agent);
        build_tree(&harness.alpha);
        harness.cycle_ok();

        let path = "dir3/nested/file1.txt";
        fs::write(harness.alpha.join(path), "alpha version").unwrap();
        fs::write(harness.beta.join(path), "beta version").unwrap();
        let report = harness.cycle_ok();

        if alpha_wins {
            assert!(report.conflicts.is_empty());
            let beta_content = fs::read_to_string(harness.beta.join(path)).unwrap();
            assert_eq!(beta_content, "alpha version");
            harness.assert_trees_equal("resolved conflict");
        } else {
            assert!(!report.conflicts.is_empty(), "expected a conflict");
            let alpha_content = fs::read_to_string(harness.alpha.join(path)).unwrap();
            let beta_content = fs::read_to_string(harness.beta.join(path)).unwrap();
            assert_eq!(alpha_content, "alpha version");
            assert_eq!(beta_content, "beta version");
        }
    }
}

#[test]
fn ignored_content_stays_local_to_each_side() {
    for mode in [SyncMode::TwoWaySafe, SyncMode::OneWayReplica] {
        let mut harness =
            Harness::new(mode, Transport::Agent).with_ignores(&["scratch", "*.log"]);
        build_tree(&harness.alpha);
        fs::create_dir_all(harness.alpha.join("scratch")).unwrap();
        fs::write(harness.alpha.join("scratch/alpha-only.txt"), "alpha scratch").unwrap();
        fs::write(harness.alpha.join("dir0/debug.log"), "alpha log").unwrap();
        fs::create_dir_all(harness.beta.join("scratch")).unwrap();
        fs::write(harness.beta.join("scratch/beta-only.txt"), "beta scratch").unwrap();

        harness.cycle_ok();

        // Ignored content is untouched on both sides and never transferred.
        assert!(harness.alpha.join("scratch/alpha-only.txt").exists());
        assert!(harness.beta.join("scratch/beta-only.txt").exists());
        assert!(!harness.beta.join("scratch/alpha-only.txt").exists());
        assert!(!harness.beta.join("dir0/debug.log").exists());
        // Non-ignored content synchronized normally.
        assert!(harness.beta.join("dir0/nested/file0.txt").exists());
    }
}

#[test]
fn large_file_delta_update_over_agent() {
    let mut harness = Harness::new(SyncMode::TwoWaySafe, Transport::Agent);
    fs::create_dir_all(&harness.alpha).unwrap();

    // Build a ~2MB pseudo-random file, sync, then make small edits at the
    // head, middle, and tail (exercising delta reuse of unchanged blocks).
    let mut data = Vec::with_capacity(2 * 1024 * 1024);
    let mut state: u64 = 0x2545F4914F6CDD1D;
    while data.len() < 2 * 1024 * 1024 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        data.extend_from_slice(&state.to_le_bytes());
    }
    fs::write(harness.alpha.join("big.bin"), &data).unwrap();
    harness.cycle_ok();
    harness.assert_trees_equal("large file initial");

    data[0] ^= 0xFF;
    let middle = data.len() / 2;
    data[middle] ^= 0xFF;
    data.extend_from_slice(b"appended tail");
    fs::write(harness.alpha.join("big.bin"), &data).unwrap();
    harness.cycle_ok();
    harness.assert_trees_equal("large file delta update");
    assert_eq!(fs::read(harness.beta.join("big.bin")).unwrap(), data);
}

#[test]
fn executability_and_symlink_changes_propagate() {
    let mut harness = Harness::new(SyncMode::TwoWaySafe, Transport::Agent);
    build_tree(&harness.alpha);
    harness.cycle_ok();

    // Flip executability without changing content.
    let target = harness.alpha.join("dir0/nested/file0.txt");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    // Delete a symlink.
    fs::remove_file(harness.alpha.join("dir1/nested/link")).unwrap();
    harness.cycle_ok();
    harness.assert_trees_equal("executability and symlink changes");
    let beta_mode = fs::metadata(harness.beta.join("dir0/nested/file0.txt"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(beta_mode & 0o111, 0, "executability should propagate");
}

#[test]
fn root_deletion_halts_for_safety() {
    let mut harness = Harness::new(SyncMode::TwoWaySafe, Transport::Agent);
    build_tree(&harness.alpha);
    harness.cycle_ok();

    fs::remove_dir_all(&harness.alpha).unwrap();
    let error = harness.cycle().expect_err("root deletion must halt");
    let halt = error
        .downcast_ref::<SafetyHalt>()
        .expect("expected a safety halt");
    assert!(matches!(
        halt,
        SafetyHalt::RootDeletion | SafetyHalt::RootEmptied
    ));

    // Beta must be untouched.
    assert!(harness.beta.join("dir0/nested/file0.txt").exists());
}

#[test]
fn emptied_root_halts_for_safety() {
    let mut harness = Harness::new(SyncMode::TwoWaySafe, Transport::Agent);
    build_tree(&harness.alpha);
    harness.cycle_ok();

    // Empty (but keep) the alpha root.
    for entry in fs::read_dir(&harness.alpha).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            fs::remove_dir_all(path).unwrap();
        } else {
            fs::remove_file(path).unwrap();
        }
    }
    let error = harness.cycle().expect_err("emptied root must halt");
    assert!(error.downcast_ref::<SafetyHalt>().is_some());
    assert!(harness.beta.join("dir0/nested/file0.txt").exists());
}

#[test]
fn interleaved_bidirectional_activity_converges() {
    let mut harness = Harness::new(SyncMode::TwoWaySafe, Transport::Agent);
    build_tree(&harness.alpha);
    harness.cycle_ok();

    // Disjoint concurrent activity on both sides across several cycles.
    fs::write(harness.alpha.join("dir0/nested/alpha-new.txt"), "a1").unwrap();
    fs::write(harness.beta.join("dir1/nested/beta-new.txt"), "b1").unwrap();
    harness.cycle_ok();
    harness.assert_trees_equal("first interleaving");

    fs::remove_file(harness.alpha.join("dir1/nested/beta-new.txt")).unwrap();
    fs::write(harness.beta.join("dir0/nested/alpha-new.txt"), "b2").unwrap();
    harness.cycle_ok();
    harness.assert_trees_equal("second interleaving");
    assert_eq!(
        fs::read_to_string(harness.alpha.join("dir0/nested/alpha-new.txt")).unwrap(),
        "b2"
    );
    assert!(!harness.beta.join("dir1/nested/beta-new.txt").exists());
}
