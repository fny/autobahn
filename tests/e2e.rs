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

use autobahn::endpoint::local::{EndpointOptions, LocalEndpoint};
use autobahn::endpoint::remote::RemoteEndpoint;
use autobahn::endpoint::Endpoint;
use autobahn::protocol::Initialize;
use autobahn::scan::{IgnoreSet, SymlinkMode};
use autobahn::session::{CyclePoint, CycleReport, SafetyHalt, Session};
use autobahn::transport::Connection;
use autobahn::tree::SyncMode;
mod common;

/// The transports a scenario can run over.
#[derive(Clone, Copy, Debug, PartialEq)]
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
        common::isolate_home();
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
        self.session()?.run_cycle()
    }

    /// Runs one cycle with `action` performed at `point` of it — the seam
    /// through which a collision is placed exactly where it is dangerous.
    fn cycle_at(
        &mut self,
        point: CyclePoint,
        mut action: impl FnMut() + Send + 'static,
    ) -> anyhow::Result<CycleReport> {
        let mut session = self.session()?;
        session.set_cycle_hook(Box::new(move |at| {
            if at == point {
                action();
            }
        }));
        session.run_cycle()
    }

    /// Cycles until a cycle changes nothing, and returns how many it took.
    /// A collision can leave a cycle with a refusal to redo, or content to
    /// fetch again; this is what the supervisor would do about it.
    fn settle(&mut self, context: &str) -> usize {
        for cycles in 1..=5 {
            let report = self
                .cycle()
                .unwrap_or_else(|e| panic!("{context}: cycle {cycles}: {e:#}"));
            if !report.changed()
                && report.beta_transition_problems.is_empty()
                && report.alpha_transition_problems.is_empty()
                && !report.missing_staged_files
            {
                return cycles;
            }
        }
        panic!("{context}: not settled after five cycles");
    }

    fn session(&mut self) -> anyhow::Result<Session> {
        let (beta, state) = (self.beta.clone(), self.state.clone());
        self.session_to(&beta, &state)
    }

    /// A second destination beside the first, sharing the alpha — another
    /// session of a fan-out — with state of its own.
    fn second_beta(&self) -> (PathBuf, PathBuf) {
        let beta = self._keep.path().join("beta2");
        fs::create_dir_all(&beta).unwrap();
        (beta, self.state.join("second"))
    }

    fn session_to(&mut self, beta_root: &Path, state: &Path) -> anyhow::Result<Session> {
        self.session_counter += 1;
        let alpha_endpoint: Box<dyn Endpoint + Send> = Box::new(
            LocalEndpoint::new(
                self.alpha.clone(),
                state.join("staging-alpha"),
                EndpointOptions {
                    ignores: IgnoreSet::new(&self.ignores)?,
                    ..EndpointOptions::default()
                },
            )
            .expect("alpha endpoint"),
        );
        let beta_endpoint: Box<dyn Endpoint + Send> = match self.transport {
            Transport::Local => Box::new(
                LocalEndpoint::new(
                    beta_root.to_path_buf(),
                    state.join("staging-beta"),
                    EndpointOptions {
                        ignores: IgnoreSet::new(&self.ignores)?,
                        ..EndpointOptions::default()
                    },
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
                        Initialize {
                            root: beta_root.to_string_lossy().into_owned(),
                            session: autobahn::session::session_identifier(
                                &state.to_string_lossy(),
                                "e2e",
                            ),
                            ignores: self.ignores.clone(),
                            symlink_mode: SymlinkMode::Raw,
                            file_mode: None,
                            directory_mode: None,
                            side: "beta".into(),
                            staging: Default::default(),
                            max_file_size: None,
                            max_entry_count: None,
                            ignore_mounts: true,
                            default_owner: None,
                            default_group: None,
                        },
                    )
                    .expect("connect agent"),
                )
            }
        };
        Session::new(
            alpha_endpoint,
            beta_endpoint,
            self.mode,
            state.to_path_buf(),
        )
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
const ALL_MODES: [SyncMode; 6] = [
    SyncMode::TwoWaySafe,
    SyncMode::TwoWayParanoid,
    SyncMode::TwoWayResolved,
    SyncMode::TwoWayStrict,
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
        assert!(
            !report.changed(),
            "steady state should be a no-op ({mode:?})"
        );
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
            SyncMode::TwoWaySafe
            | SyncMode::TwoWayParanoid
            | SyncMode::TwoWayResolved
            | SyncMode::TwoWayStrict => {
                assert!(alpha_added.exists(), "{mode:?}: addition should propagate");
                harness.assert_trees_equal("beta addition");
            }
            SyncMode::OneWaySafe => {
                assert!(
                    !alpha_added.exists(),
                    "one-way-safe must not reverse-propagate"
                );
                assert!(
                    beta_added.exists(),
                    "one-way-safe must preserve beta additions"
                );
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
            SyncMode::TwoWaySafe
            | SyncMode::TwoWayParanoid
            | SyncMode::TwoWayResolved
            | SyncMode::TwoWayStrict => {
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
    for (mode, alpha_wins) in [
        (SyncMode::TwoWaySafe, false),
        (SyncMode::TwoWayResolved, true),
    ] {
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
        let mut harness = Harness::new(mode, Transport::Agent).with_ignores(&["scratch", "*.log"]);
        build_tree(&harness.alpha);
        fs::create_dir_all(harness.alpha.join("scratch")).unwrap();
        fs::write(
            harness.alpha.join("scratch/alpha-only.txt"),
            "alpha scratch",
        )
        .unwrap();
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

// ── the reconnect harness: cutting the wire at every frame boundary ──
//
// A byte-cutting proxy rides between the controller and a real agent
// process: it forwards the stdio byte stream while parsing the frame
// structure, and dies at a configured point — at the boundary after the
// Nth frame, or two bytes into the frame after it. The sweep advances the
// cut through the entire canonical exchange in both directions. After
// every cut, a fresh session over the same state must recover to a safe
// tree: cleanly converged, or conflicted only on the cut cycle's own
// paths with both sides holding legitimate content. Each cycle builds a
// fresh connection, so no response from a dead connection can satisfy a
// new request — the cut connection object dies with its session.

/// An incremental parser for the wire's length-prefixed frame structure,
/// fed the bytes that pass through a cut stream.
struct FrameParser {
    prefix: [u8; 4],
    prefix_got: usize,
    payload_left: u64,
    complete: usize,
}

impl FrameParser {
    fn new() -> FrameParser {
        FrameParser {
            prefix: [0; 4],
            prefix_got: 0,
            payload_left: 0,
            complete: 0,
        }
    }

    /// Bytes until the next parsing milestone; never spans a boundary.
    fn next_chunk(&self) -> u64 {
        if self.payload_left > 0 {
            self.payload_left
        } else {
            (4 - self.prefix_got) as u64
        }
    }

    fn advance(&mut self, bytes: &[u8]) {
        let mut index = 0;
        while index < bytes.len() {
            if self.payload_left == 0 {
                let take = (4 - self.prefix_got).min(bytes.len() - index);
                self.prefix[self.prefix_got..self.prefix_got + take]
                    .copy_from_slice(&bytes[index..index + take]);
                self.prefix_got += take;
                index += take;
                if self.prefix_got == 4 {
                    self.payload_left = u32::from_le_bytes(self.prefix) as u64;
                    if self.payload_left == 0 {
                        self.prefix_got = 0;
                        self.complete += 1;
                    }
                }
            } else {
                let take = self.payload_left.min((bytes.len() - index) as u64) as usize;
                self.payload_left -= take as u64;
                index += take;
                if self.payload_left == 0 {
                    self.prefix_got = 0;
                    self.complete += 1;
                }
            }
        }
    }
}

/// Which half of the exchange the cut severs.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CutDirection {
    /// The agent's responses stop arriving.
    FromAgent,
    /// The controller's requests stop getting through.
    ToAgent,
}

/// A stream that forwards until its cut point, then dies.
struct CutStream<T> {
    inner: T,
    parser: FrameParser,
    frames: usize,
    extra: u64,
    fired: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<T> CutStream<T> {
    fn new(
        inner: T,
        frames: usize,
        extra: u64,
        fired: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> CutStream<T> {
        CutStream {
            inner,
            parser: FrameParser::new(),
            frames,
            extra,
            fired,
        }
    }

    /// How many more bytes may pass, `0` meaning the cut fires now.
    fn budget(&self) -> u64 {
        if self.parser.complete < self.frames {
            self.parser.next_chunk()
        } else {
            self.extra
        }
    }

    fn account(&mut self, bytes: &[u8]) {
        if self.parser.complete < self.frames {
            self.parser.advance(bytes);
        } else {
            self.extra -= bytes.len() as u64;
        }
    }

    fn fire(&self) {
        self.fired.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl<T: std::io::Read> std::io::Read for CutStream<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let budget = self.budget();
        if budget == 0 {
            self.fire();
            return Ok(0);
        }
        let cap = buf.len().min(budget as usize);
        let got = self.inner.read(&mut buf[..cap])?;
        self.account(&buf[..got]);
        Ok(got)
    }
}

impl<T: std::io::Write> std::io::Write for CutStream<T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let budget = self.budget();
        if budget == 0 {
            self.fire();
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "cut: the connection died here",
            ));
        }
        let cap = buf.len().min(budget as usize);
        let wrote = self.inner.write(&buf[..cap])?;
        self.account(&buf[..wrote]);
        Ok(wrote)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Harness {
    /// One cycle over an agent whose connection dies at the configured
    /// point. Returns the cycle's outcome and whether the cut engaged.
    fn agent_cycle_with_cut(
        &mut self,
        direction: CutDirection,
        frames: usize,
        extra: u64,
    ) -> (anyhow::Result<CycleReport>, bool) {
        use std::io::{Read, Write};
        let binary = env!("CARGO_BIN_EXE_autobahn");
        let mut child = std::process::Command::new(binary)
            .arg("agent")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn agent");
        let stdin = child.stdin.take().expect("agent stdin");
        let stdout = child.stdout.take().expect("agent stdout");
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (reader, writer): (Box<dyn Read + Send>, Box<dyn Write + Send>) = match direction {
            CutDirection::FromAgent => (
                Box::new(CutStream::new(stdout, frames, extra, fired.clone())),
                Box::new(stdin),
            ),
            CutDirection::ToAgent => (
                Box::new(stdout),
                Box::new(CutStream::new(stdin, frames, extra, fired.clone())),
            ),
        };
        let connection = Connection::from_streams(reader, writer);
        let result = (|| -> anyhow::Result<CycleReport> {
            let beta: Box<dyn Endpoint + Send> = Box::new(RemoteEndpoint::connect(
                connection,
                Initialize {
                    root: self.beta.to_string_lossy().into_owned(),
                    session: autobahn::session::session_identifier(
                        &self.state.to_string_lossy(),
                        "e2e",
                    ),
                    ignores: self.ignores.clone(),
                    symlink_mode: SymlinkMode::Raw,
                    file_mode: None,
                    directory_mode: None,
                    side: "beta".into(),
                    staging: Default::default(),
                    max_file_size: None,
                    max_entry_count: None,
                    ignore_mounts: true,
                    default_owner: None,
                    default_group: None,
                },
            )?);
            let alpha: Box<dyn Endpoint + Send> = Box::new(LocalEndpoint::new(
                self.alpha.clone(),
                self.state.join("staging-alpha"),
                EndpointOptions {
                    ignores: IgnoreSet::new(&self.ignores)?,
                    ..EndpointOptions::default()
                },
            )?);
            let mut session = Session::new(alpha, beta, self.mode, self.state.clone())?;
            session.run_cycle()
        })();
        let _ = child.kill();
        let _ = child.wait();
        (result, fired.load(std::sync::atomic::Ordering::SeqCst))
    }
}

/// The sweep: for both directions, cut at the boundary after every frame
/// of the canonical exchange (and two bytes into the frame after it),
/// recover, and hold the safety line every time. The sweep is self-
/// terminating — it ends when a cut point lies beyond the whole exchange.
#[test]
fn every_cut_connection_recovers_to_a_safe_tree() {
    let old_bytes: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
    let new_bytes: Vec<u8> = (0..80 * 1024u32).map(|i| (i % 241) as u8).collect();
    let created: Vec<u8> = (0..96 * 1024u32).map(|i| (i % 239) as u8).collect();

    for direction in [CutDirection::FromAgent, CutDirection::ToAgent] {
        let mut cut_points = 0usize;
        let mut conflicted_points = 0usize;
        'sweep: for frames in 0.. {
            for extra in [0u64, 2] {
                let mut harness = Harness::new(SyncMode::TwoWaySafe, Transport::Agent);
                fs::write(harness.alpha.join("stable.txt"), b"stable").unwrap();
                fs::write(harness.alpha.join("modify.txt"), &old_bytes).unwrap();
                harness.cycle_ok();
                harness.cycle_ok();
                harness.assert_trees_equal("pre-cut convergence");

                // The change the cut cycle carries.
                fs::write(harness.alpha.join("modify.txt"), &new_bytes).unwrap();
                fs::write(harness.alpha.join("created.bin"), &created).unwrap();

                let (result, fired) = harness.agent_cycle_with_cut(direction, frames, extra);
                if !fired {
                    // The cut point lies beyond the whole exchange: the
                    // cycle must have completed untouched, and the sweep
                    // is done for this direction.
                    result.expect("an uncut exchange completes");
                    if extra == 0 {
                        break 'sweep;
                    }
                    continue;
                }
                cut_points += 1;

                // Recovery: ordinary sessions over the same state.
                let mut last: Option<CycleReport> = None;
                for _ in 0..6 {
                    let report = harness.cycle().expect("recovery cycles run");
                    let settled = report.alpha_transitions == 0
                        && report.beta_transitions == 0
                        && !report.missing_staged_files;
                    last = Some(report);
                    if settled {
                        break;
                    }
                }
                let report = last.expect("at least one recovery cycle");

                // The safety line: nothing torn, conflicts only on the cut
                // cycle's paths, and full agreement without them.
                let candidate = |root: &Path, name: &str, allowed: &[Option<&[u8]>]| {
                    let actual = fs::read(root.join(name)).ok();
                    assert!(
                        allowed.contains(&actual.as_deref()),
                        "{direction:?} frames={frames} extra={extra}: {name} holds \
                         none of its legitimate versions ({:?} bytes)",
                        actual.map(|bytes| bytes.len())
                    );
                };
                for root in [&harness.alpha, &harness.beta] {
                    candidate(root, "stable.txt", &[Some(b"stable")]);
                    candidate(root, "modify.txt", &[Some(&old_bytes), Some(&new_bytes)]);
                    candidate(root, "created.bin", &[None, Some(&created)]);
                }
                if report.conflicts.is_empty() {
                    harness.assert_trees_equal(&format!(
                        "{direction:?} frames={frames} extra={extra}: recovery"
                    ));
                } else {
                    conflicted_points += 1;
                    for conflict in &report.conflicts {
                        assert!(
                            ["modify.txt", "created.bin"].contains(&conflict.root.as_str()),
                            "{direction:?} frames={frames} extra={extra}: conflict off \
                             the cut cycle's paths: {}",
                            conflict.root
                        );
                    }
                }
            }
        }
        eprintln!(
            "cut sweep {direction:?}: {cut_points} cut points, \
             {conflicted_points} recovered with conflicts"
        );
        assert!(cut_points > 0, "the sweep never engaged a cut");
    }
}

/// Peering, against a real agent: a channel that presents a lease term
/// below the host's is fenced — every write refused, reads still answered
/// — until it presents a term at least as high; the ancestor copy follows
/// records and takes a checkpoint when it cannot.
#[test]
fn peering_fence_and_ancestor_copy_over_the_wire() {
    use autobahn::peering::{Lease, LeaseAnswer};
    use autobahn::tree::{Change, Node};
    use std::time::Duration;

    common::isolate_home();
    let keep = tempfile::tempdir().expect("tempdir");
    let root = keep.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let binary = env!("CARGO_BIN_EXE_autobahn").to_owned();
    let connect = || {
        let connection =
            Connection::spawn(&[binary.clone(), "agent".to_owned()]).expect("spawn agent");
        RemoteEndpoint::connect(
            connection,
            Initialize {
                root: root.to_string_lossy().into_owned(),
                session: autobahn::session::session_identifier(
                    &root.to_string_lossy(),
                    "peering-e2e",
                ),
                ignores: Vec::new(),
                symlink_mode: SymlinkMode::Raw,
                file_mode: None,
                directory_mode: None,
                side: "beta".into(),
                staging: Default::default(),
                max_file_size: None,
                max_entry_count: None,
                ignore_mounts: true,
                default_owner: None,
                default_group: None,
            },
        )
        .expect("connect agent")
    };
    let ttl = Duration::from_secs(30);

    // A first lease on a host that holds none is accepted, and a renewal
    // at the same term from the same leader too.
    let mut endpoint = connect();
    assert_eq!(
        endpoint.lease(&Lease::new("alpha", 5, ttl)).unwrap(),
        LeaseAnswer::Accepted
    );
    assert_eq!(
        endpoint.lease(&Lease::new("alpha", 5, ttl)).unwrap(),
        LeaseAnswer::Accepted
    );
    let state = endpoint.peering_state().unwrap();
    assert_eq!(
        state.lease.as_ref().map(|l| (l.term, l.leader.as_str())),
        Some((5, "alpha"))
    );
    assert_eq!(state.generation, None, "no ancestor copy yet");

    // A newer leader takes the host; the old leader, on its own channel,
    // is then refused and fenced: it can scan, it cannot write.
    let mut newer = connect();
    assert_eq!(
        newer.lease(&Lease::new("u@h:/x", 6, ttl)).unwrap(),
        LeaseAnswer::Accepted
    );
    match endpoint.lease(&Lease::new("alpha", 5, ttl)).unwrap() {
        LeaseAnswer::Refused { current } => {
            assert_eq!(current.term, 6);
            assert_eq!(current.leader, "u@h:/x");
        }
        other => panic!("the old leader should be refused, got {other:?}"),
    }
    endpoint.scan().expect("reads still answer while fenced");
    let error = endpoint
        .put_peering_file("name", b"u@h:/x")
        .expect_err("a write while fenced is refused");
    assert!(format!("{error:#}").contains("fenced"), "{error:#}");
    let error = endpoint
        .transition(vec![Change {
            path: "new".into(),
            old: None,
            new: Some(Node::directory("new", Vec::new())),
        }])
        .expect_err("a transition while fenced is refused");
    assert!(format!("{error:#}").contains("fenced"), "{error:#}");
    // A same-term claim by a *different* leader is a split and is refused.
    let mut split = connect();
    assert!(matches!(
        split.lease(&Lease::new("other", 6, ttl)).unwrap(),
        LeaseAnswer::Refused { .. }
    ));
    // Presenting a higher term lifts the fence.
    assert_eq!(
        endpoint.lease(&Lease::new("alpha", 7, ttl)).unwrap(),
        LeaseAnswer::Accepted
    );
    endpoint
        .put_peering_file("name", b"u@h:/x")
        .expect("writes again");

    // The ancestor copy: the first record carries the tree; a record for
    // a generation the copy is not at is answered with where it stands;
    // a checkpoint moves it there; the next record then applies.
    let tree = |names: &[&str]| {
        Node::directory(
            "",
            names
                .iter()
                .map(|n| Node::directory(*n, Vec::new()))
                .collect(),
        )
    };
    let creation = Change {
        path: String::new(),
        old: None,
        new: Some(tree(&["a"])),
    };
    assert_eq!(endpoint.ancestor_record(1, &[creation]).unwrap(), 1);
    assert_eq!(
        endpoint.ancestor_record(4, &[]).unwrap(),
        1,
        "a record the copy cannot apply reports the copy's generation"
    );
    assert_eq!(
        endpoint
            .ancestor_checkpoint(3, Some(&tree(&["a", "b"])))
            .unwrap(),
        3
    );
    let addition = Change {
        path: "c".into(),
        old: None,
        new: Some(Node::directory("c", Vec::new())),
    };
    assert_eq!(endpoint.ancestor_record(4, &[addition]).unwrap(), 4);
    assert_eq!(endpoint.peering_state().unwrap().generation, Some(4));

    // A fresh connection sees what the host holds: the copy survived.
    let mut again = connect();
    let state = again.peering_state().unwrap();
    assert_eq!(state.generation, Some(4));
    assert_eq!(state.lease.map(|l| l.term), Some(7));

    // Only the files a follower needs can be pushed.
    let error = again
        .put_peering_file("../escape", b"x")
        .expect_err("an unknown file name is refused");
    assert!(
        format!("{error:#}").contains("not a file peering pushes"),
        "{error:#}"
    );
}

/// Collisions: a write landing on one side while a cycle is part-way
/// through moving content there. Every scenario places the write at an
/// exact point of the cycle through the session's hook, over both
/// transports, and asserts the contract — the late write is never
/// silently overwritten; what happens to it next is the mode's rule.
mod collisions {
    use super::*;

    const BOTH: [Transport; 2] = [Transport::Local, Transport::Agent];
    const PATH: &str = "dir1/nested/file1.txt";

    fn read(root: &Path) -> String {
        fs::read_to_string(root.join(PATH)).unwrap_or_else(|e| panic!("{}: {e}", root.display()))
    }

    /// Alpha's edit is on its way to beta; beta is edited after its scan
    /// and before the publish. The publish must refuse — the file on disk
    /// is not the one the transition was validated against — and the
    /// next cycle sees two edits of one file: a conflict in safe mode,
    /// alpha's version in resolved mode.
    #[test]
    fn a_write_on_beta_before_its_publish_is_refused_not_overwritten() {
        for transport in BOTH {
            for point in [CyclePoint::AfterScans, CyclePoint::BeforeBetaTransition] {
                for (mode, alpha_wins) in [
                    (SyncMode::TwoWaySafe, false),
                    (SyncMode::TwoWayResolved, true),
                ] {
                    let context = format!("{transport:?}/{point:?}/{mode:?}");
                    let mut harness = Harness::new(mode, transport);
                    build_tree(&harness.alpha);
                    harness.cycle_ok();

                    fs::write(harness.alpha.join(PATH), "alpha v2").unwrap();
                    let beta = harness.beta.clone();
                    let report = harness
                        .cycle_at(point, move || {
                            fs::write(beta.join(PATH), "beta late").unwrap()
                        })
                        .unwrap_or_else(|e| panic!("{context}: {e:#}"));

                    // The late write survived the cycle that raced it.
                    assert_eq!(read(&harness.beta), "beta late", "{context}: overwritten");
                    assert_eq!(read(&harness.alpha), "alpha v2", "{context}");
                    assert!(
                        report
                            .beta_transition_problems
                            .iter()
                            .any(|p| p.path == PATH),
                        "{context}: the refusal was not reported: {:?}",
                        report.beta_transition_problems
                    );

                    // Then the mode decides.
                    let report = harness.cycle_ok();
                    if alpha_wins {
                        assert!(report.conflicts.is_empty(), "{context}");
                        harness.settle(&context);
                        assert_eq!(read(&harness.beta), "alpha v2", "{context}");
                        harness.assert_trees_equal(&context);
                    } else {
                        assert!(
                            report.conflicts.iter().any(|c| c.root == PATH),
                            "{context}: expected a conflict at {PATH}, got {:?}",
                            report.conflicts
                        );
                        assert_eq!(read(&harness.alpha), "alpha v2", "{context}");
                        assert_eq!(read(&harness.beta), "beta late", "{context}");
                    }
                }
            }
        }
    }

    /// The mirror image: beta's edit is on its way to alpha, and alpha is
    /// edited before the publish.
    #[test]
    fn a_write_on_alpha_before_its_publish_is_refused_not_overwritten() {
        for transport in BOTH {
            for (mode, alpha_wins) in [
                (SyncMode::TwoWaySafe, false),
                (SyncMode::TwoWayResolved, true),
            ] {
                let context = format!("{transport:?}/{mode:?}");
                let mut harness = Harness::new(mode, transport);
                build_tree(&harness.alpha);
                harness.cycle_ok();

                fs::write(harness.beta.join(PATH), "beta v2").unwrap();
                let alpha = harness.alpha.clone();
                let report = harness
                    .cycle_at(CyclePoint::BeforeAlphaTransition, move || {
                        fs::write(alpha.join(PATH), "alpha late").unwrap()
                    })
                    .unwrap_or_else(|e| panic!("{context}: {e:#}"));

                assert_eq!(read(&harness.alpha), "alpha late", "{context}: overwritten");
                assert_eq!(read(&harness.beta), "beta v2", "{context}");
                assert!(
                    report
                        .alpha_transition_problems
                        .iter()
                        .any(|p| p.path == PATH),
                    "{context}: the refusal was not reported"
                );

                let report = harness.cycle_ok();
                if alpha_wins {
                    assert!(report.conflicts.is_empty(), "{context}");
                    harness.settle(&context);
                    assert_eq!(read(&harness.beta), "alpha late", "{context}");
                    harness.assert_trees_equal(&context);
                } else {
                    assert!(
                        report.conflicts.iter().any(|c| c.root == PATH),
                        "{context}: expected a conflict, got {:?}",
                        report.conflicts
                    );
                    assert_eq!(read(&harness.alpha), "alpha late", "{context}");
                    assert_eq!(read(&harness.beta), "beta v2", "{context}");
                }
            }
        }
    }

    /// Alpha is edited again after its scan, while the cycle is carrying
    /// the earlier edit. The bytes pulled are the newer ones and the
    /// transition names the older digest; the two must never be paired.
    /// Whatever the cycle does with that, the next cycles carry the newer
    /// edit and the sides end equal.
    #[test]
    fn a_second_write_on_alpha_after_its_scan_is_carried_not_mislabeled() {
        for transport in BOTH {
            let context = format!("{transport:?}");
            let mut harness = Harness::new(SyncMode::TwoWaySafe, transport);
            build_tree(&harness.alpha);
            harness.cycle_ok();

            fs::write(harness.alpha.join(PATH), "alpha v2").unwrap();
            let alpha = harness.alpha.clone();
            let outcome = harness.cycle_at(CyclePoint::AfterScans, move || {
                fs::write(alpha.join(PATH), "alpha v3, longer").unwrap()
            });
            // A refused or re-fetched transfer is fine; a halt is not.
            if let Err(error) = &outcome {
                assert!(
                    error.downcast_ref::<SafetyHalt>().is_none(),
                    "{context}: halted: {error:#}"
                );
            }
            // Beta never holds content the ancestor would misdescribe:
            // either the old version, or the new one, never a mix.
            let now = read(&harness.beta);
            assert!(
                now == "content 1/1" || now == "alpha v3, longer",
                "{context}: beta holds {now:?}"
            );

            harness.settle(&context);
            assert_eq!(read(&harness.beta), "alpha v3, longer", "{context}");
            harness.assert_trees_equal(&context);
            let report = harness.cycle_ok();
            assert!(
                report.conflicts.is_empty(),
                "{context}: {:?}",
                report.conflicts
            );
        }
    }

    /// Alpha deleted the file; beta edits it before the deletion is
    /// applied. The deletion must be refused, and the edit then wins over
    /// the deletion — the reconciler's rule for a modification against a
    /// deletion — landing back on alpha.
    #[test]
    fn a_write_on_beta_racing_a_deletion_keeps_the_edit() {
        for transport in BOTH {
            for (mode, strict) in [
                (SyncMode::TwoWaySafe, false),
                (SyncMode::TwoWayResolved, false),
                (SyncMode::TwoWayStrict, true),
            ] {
                let context = format!("{transport:?}/{mode:?}");
                let mut harness = Harness::new(mode, transport);
                build_tree(&harness.alpha);
                harness.cycle_ok();

                fs::remove_file(harness.alpha.join(PATH)).unwrap();
                let beta = harness.beta.clone();
                let report = harness
                    .cycle_at(CyclePoint::BeforeBetaTransition, move || {
                        fs::write(beta.join(PATH), "beta edit").unwrap()
                    })
                    .unwrap_or_else(|e| panic!("{context}: {e:#}"));
                // Refused in every mode: the racing cycle never deletes
                // what it did not validate. The mode decides next cycle.
                assert_eq!(
                    read(&harness.beta),
                    "beta edit",
                    "{context}: the edit was deleted"
                );
                assert!(
                    report
                        .beta_transition_problems
                        .iter()
                        .any(|p| p.path == PATH),
                    "{context}: the refusal was not reported"
                );

                harness.settle(&context);
                if strict {
                    assert!(
                        !exists(&harness.alpha, PATH),
                        "{context}: the deletion was undone"
                    );
                    assert!(!exists(&harness.beta, PATH), "{context}: the edit survived");
                } else {
                    assert_eq!(read(&harness.alpha), "beta edit", "{context}");
                    assert_eq!(read(&harness.beta), "beta edit", "{context}");
                }
                harness.assert_trees_equal(&context);
            }
        }
    }

    const DIR: &str = "dir2/nested";

    fn exists(root: &Path, path: &str) -> bool {
        root.join(path).exists()
    }

    /// Alpha turned the file into a directory; beta edits the file before
    /// the replacement lands. The replacement is refused — the file is not
    /// the one validated against — and the next cycle sees a file edited
    /// on one side and replaced by a directory on the other: a conflict in
    /// safe mode, the directory in resolved mode.
    #[test]
    fn a_file_edited_on_beta_while_alpha_replaces_it_with_a_directory() {
        for transport in BOTH {
            for (mode, alpha_wins) in [
                (SyncMode::TwoWaySafe, false),
                (SyncMode::TwoWayResolved, true),
            ] {
                let context = format!("{transport:?}/{mode:?}");
                let mut harness = Harness::new(mode, transport);
                build_tree(&harness.alpha);
                harness.cycle_ok();

                fs::remove_file(harness.alpha.join(PATH)).unwrap();
                fs::create_dir(harness.alpha.join(PATH)).unwrap();
                fs::write(harness.alpha.join(PATH).join("inner.txt"), "inner").unwrap();
                let beta = harness.beta.clone();
                let report = harness
                    .cycle_at(CyclePoint::BeforeBetaTransition, move || {
                        fs::write(beta.join(PATH), "beta late").unwrap()
                    })
                    .unwrap_or_else(|e| panic!("{context}: {e:#}"));
                assert_eq!(read(&harness.beta), "beta late", "{context}: overwritten");
                assert!(
                    report
                        .beta_transition_problems
                        .iter()
                        .any(|p| p.path == PATH),
                    "{context}: the refusal was not reported"
                );

                let report = harness.cycle_ok();
                if alpha_wins {
                    assert!(report.conflicts.is_empty(), "{context}");
                    harness.settle(&context);
                    assert!(harness.beta.join(PATH).is_dir(), "{context}");
                    harness.assert_trees_equal(&context);
                } else {
                    assert!(
                        report.conflicts.iter().any(|c| c.root == PATH),
                        "{context}: expected a conflict, got {:?}",
                        report.conflicts
                    );
                    assert!(harness.alpha.join(PATH).is_dir(), "{context}");
                    assert_eq!(read(&harness.beta), "beta late", "{context}");
                }
            }
        }
    }

    /// The other way round: alpha's edit is on its way, and beta turns the
    /// file into a directory before it lands. The edit is refused — there
    /// is no file to replace — and the next cycle sees the same two-sided
    /// change: a conflict, or alpha's file back in place of the directory.
    #[test]
    fn a_file_replaced_by_a_directory_on_beta_while_alpha_edits_it() {
        for transport in BOTH {
            for (mode, alpha_wins) in [
                (SyncMode::TwoWaySafe, false),
                (SyncMode::TwoWayResolved, true),
            ] {
                let context = format!("{transport:?}/{mode:?}");
                let mut harness = Harness::new(mode, transport);
                build_tree(&harness.alpha);
                harness.cycle_ok();

                fs::write(harness.alpha.join(PATH), "alpha v2").unwrap();
                let beta = harness.beta.clone();
                let report = harness
                    .cycle_at(CyclePoint::BeforeBetaTransition, move || {
                        fs::remove_file(beta.join(PATH)).unwrap();
                        fs::create_dir(beta.join(PATH)).unwrap();
                        fs::write(beta.join(PATH).join("inner.txt"), "beta inner").unwrap();
                    })
                    .unwrap_or_else(|e| panic!("{context}: {e:#}"));
                assert!(
                    harness.beta.join(PATH).is_dir(),
                    "{context}: the directory was replaced"
                );
                assert!(
                    report
                        .beta_transition_problems
                        .iter()
                        .any(|p| p.path == PATH),
                    "{context}: the refusal was not reported"
                );

                let report = harness.cycle_ok();
                if alpha_wins {
                    assert!(report.conflicts.is_empty(), "{context}");
                    harness.settle(&context);
                    assert_eq!(read(&harness.beta), "alpha v2", "{context}");
                    harness.assert_trees_equal(&context);
                } else {
                    assert!(
                        report.conflicts.iter().any(|c| c.root == PATH),
                        "{context}: expected a conflict, got {:?}",
                        report.conflicts
                    );
                    assert_eq!(read(&harness.alpha), "alpha v2", "{context}");
                    assert!(harness.beta.join(PATH).is_dir(), "{context}");
                }
            }
        }
    }

    /// Both sides create the same new name, alpha as a directory and beta
    /// as a file, beta's landing while alpha's is on its way. The creation
    /// is refused — something is already there — and the next cycle sees
    /// two creations: a conflict, or alpha's directory.
    #[test]
    fn a_name_created_as_a_file_on_beta_while_alpha_creates_a_directory() {
        const NEW: &str = "dir0/nested/fresh";
        for transport in BOTH {
            for (mode, alpha_wins) in [
                (SyncMode::TwoWaySafe, false),
                (SyncMode::TwoWayResolved, true),
            ] {
                let context = format!("{transport:?}/{mode:?}");
                let mut harness = Harness::new(mode, transport);
                build_tree(&harness.alpha);
                harness.cycle_ok();

                fs::create_dir(harness.alpha.join(NEW)).unwrap();
                fs::write(harness.alpha.join(NEW).join("inner.txt"), "inner").unwrap();
                let beta = harness.beta.clone();
                let report = harness
                    .cycle_at(CyclePoint::BeforeBetaTransition, move || {
                        fs::write(beta.join(NEW), "beta file").unwrap()
                    })
                    .unwrap_or_else(|e| panic!("{context}: {e:#}"));
                assert!(
                    harness.beta.join(NEW).is_file(),
                    "{context}: the file was replaced"
                );
                assert!(
                    report
                        .beta_transition_problems
                        .iter()
                        .any(|p| p.path == NEW),
                    "{context}: the refusal was not reported: {:?}",
                    report.beta_transition_problems
                );

                let report = harness.cycle_ok();
                if alpha_wins {
                    assert!(report.conflicts.is_empty(), "{context}");
                    harness.settle(&context);
                    assert!(harness.beta.join(NEW).is_dir(), "{context}");
                    harness.assert_trees_equal(&context);
                } else {
                    assert!(
                        report.conflicts.iter().any(|c| c.root == NEW),
                        "{context}: expected a conflict, got {:?}",
                        report.conflicts
                    );
                    assert!(harness.alpha.join(NEW).is_dir(), "{context}");
                    assert!(harness.beta.join(NEW).is_file(), "{context}");
                }
            }
        }
    }

    /// Alpha renamed the file — a deletion at the old name and a creation
    /// at the new one — and beta edits the old name before the deletion
    /// lands. The deletion is refused, the creation goes through, and the
    /// next cycle carries the edit back to alpha: an edit against a
    /// deletion keeps the edit. Both names end up on both sides.
    #[test]
    fn a_file_edited_on_beta_while_alpha_renames_it() {
        const RENAMED: &str = "dir1/nested/file1-renamed.txt";
        for transport in BOTH {
            for (mode, strict) in [
                (SyncMode::TwoWaySafe, false),
                (SyncMode::TwoWayResolved, false),
                (SyncMode::TwoWayStrict, true),
            ] {
                let context = format!("{transport:?}/{mode:?}");
                let mut harness = Harness::new(mode, transport);
                build_tree(&harness.alpha);
                harness.cycle_ok();

                fs::rename(harness.alpha.join(PATH), harness.alpha.join(RENAMED)).unwrap();
                let beta = harness.beta.clone();
                let report = harness
                    .cycle_at(CyclePoint::BeforeBetaTransition, move || {
                        fs::write(beta.join(PATH), "beta edit").unwrap()
                    })
                    .unwrap_or_else(|e| panic!("{context}: {e:#}"));
                // The racing cycle refuses in every mode: the edit is not
                // the file the deletion was validated against.
                assert_eq!(
                    read(&harness.beta),
                    "beta edit",
                    "{context}: the edit was deleted"
                );
                assert_eq!(
                    fs::read_to_string(harness.beta.join(RENAMED)).unwrap(),
                    "content 1/1",
                    "{context}: the new name did not arrive"
                );
                assert!(
                    report
                        .beta_transition_problems
                        .iter()
                        .any(|p| p.path == PATH),
                    "{context}: the refusal was not reported"
                );

                harness.settle(&context);
                if strict {
                    // Alpha's deletion is final: the rename stands, the
                    // edit is gone.
                    assert!(
                        !exists(&harness.alpha, PATH),
                        "{context}: the rename was undone"
                    );
                    assert!(!exists(&harness.beta, PATH), "{context}: the edit survived");
                } else {
                    // The edit beats the deletion: the rename is undone.
                    assert_eq!(read(&harness.alpha), "beta edit", "{context}");
                }
                assert!(
                    exists(&harness.alpha, RENAMED) && exists(&harness.beta, RENAMED),
                    "{context}"
                );
                harness.assert_trees_equal(&context);
            }
        }
    }

    /// Alpha's edit is on its way, and beta renames the file away before
    /// it lands. Whatever the racing cycle makes of a replacement with
    /// nothing to replace, the edit must end up under the old name on both
    /// sides and the renamed copy under the new one — no version of the
    /// file is lost, and nothing conflicts.
    #[test]
    fn a_file_renamed_on_beta_while_alpha_edits_it() {
        const RENAMED: &str = "dir1/nested/file1-moved.txt";
        for transport in BOTH {
            let context = format!("{transport:?}");
            let mut harness = Harness::new(SyncMode::TwoWaySafe, transport);
            build_tree(&harness.alpha);
            harness.cycle_ok();

            fs::write(harness.alpha.join(PATH), "alpha v2").unwrap();
            let beta = harness.beta.clone();
            let outcome = harness.cycle_at(CyclePoint::BeforeBetaTransition, move || {
                fs::rename(beta.join(PATH), beta.join(RENAMED)).unwrap()
            });
            if let Err(error) = &outcome {
                assert!(
                    error.downcast_ref::<SafetyHalt>().is_none(),
                    "{context}: halted: {error:#}"
                );
            }
            assert_eq!(
                fs::read_to_string(harness.beta.join(RENAMED)).unwrap(),
                "content 1/1",
                "{context}: the renamed copy was touched"
            );

            harness.settle(&context);
            assert_eq!(read(&harness.alpha), "alpha v2", "{context}");
            assert_eq!(read(&harness.beta), "alpha v2", "{context}");
            assert_eq!(
                fs::read_to_string(harness.alpha.join(RENAMED)).unwrap(),
                "content 1/1",
                "{context}: the renamed copy did not arrive"
            );
            harness.assert_trees_equal(&context);
            let report = harness.cycle_ok();
            assert!(
                report.conflicts.is_empty(),
                "{context}: {:?}",
                report.conflicts
            );
        }
    }

    /// Alpha renamed a whole directory, and beta writes a new file into the
    /// old one before its removal lands. The removal is refused (the
    /// directory no longer holds what was validated), the new name arrives
    /// beside it, and the next cycle keeps the new file: a creation inside
    /// a directory the other side deleted wins over the deletion, which —
    /// by the reconciler's rule — brings the whole directory back to alpha.
    /// Nothing is lost; the rename is undone rather than the file.
    #[test]
    fn a_file_added_on_beta_inside_a_directory_alpha_renames() {
        const RENAMED: &str = "dir2/nested-renamed";
        for transport in BOTH {
            let context = format!("{transport:?}");
            let mut harness = Harness::new(SyncMode::TwoWaySafe, transport);
            build_tree(&harness.alpha);
            harness.cycle_ok();

            fs::rename(harness.alpha.join(DIR), harness.alpha.join(RENAMED)).unwrap();
            let beta = harness.beta.clone();
            let report = harness
                .cycle_at(CyclePoint::BeforeBetaTransition, move || {
                    fs::write(beta.join(DIR).join("added.txt"), "added on beta").unwrap()
                })
                .unwrap_or_else(|e| panic!("{context}: {e:#}"));
            assert!(
                exists(&harness.beta, "dir2/nested/added.txt"),
                "{context}: the added file was deleted"
            );
            assert!(
                exists(&harness.beta, RENAMED),
                "{context}: the renamed directory did not arrive"
            );
            assert!(
                !report.beta_transition_problems.is_empty(),
                "{context}: the refusal was not reported"
            );

            harness.settle(&context);
            assert!(exists(&harness.alpha, "dir2/nested/added.txt"), "{context}");
            assert!(exists(&harness.alpha, RENAMED), "{context}");
            harness.assert_trees_equal(&context);
            let report = harness.cycle_ok();
            assert!(
                report.conflicts.is_empty(),
                "{context}: {:?}",
                report.conflicts
            );
        }
    }

    /// A write on beta right after alpha's edit was published there. The
    /// cycle completes as a clean propagation; the write is a fresh beta
    /// edit that the next cycle carries to alpha — which is only true if
    /// the endpoint re-announces the paths it just wrote, so the scan
    /// after does not adopt the tree it published.
    #[test]
    fn a_write_on_beta_right_after_the_publish_is_seen_next_cycle() {
        for transport in BOTH {
            let context = format!("{transport:?}");
            let mut harness = Harness::new(SyncMode::TwoWaySafe, transport);
            build_tree(&harness.alpha);
            harness.cycle_ok();

            fs::write(harness.alpha.join(PATH), "alpha v2").unwrap();
            let beta = harness.beta.clone();
            let report = harness
                .cycle_at(CyclePoint::AfterBetaTransition, move || {
                    fs::write(beta.join(PATH), "beta after").unwrap()
                })
                .unwrap_or_else(|e| panic!("{context}: {e:#}"));
            assert!(report.beta_transition_problems.is_empty(), "{context}");
            assert_eq!(read(&harness.beta), "beta after", "{context}");

            let report = harness.cycle_ok();
            assert!(
                report.conflicts.is_empty(),
                "{context}: {:?}",
                report.conflicts
            );
            assert_eq!(read(&harness.alpha), "beta after", "{context}: not carried");
            harness.assert_trees_equal(&context);
        }
    }
}

/// A watch begun after a cycle and still standing when the next cycle
/// runs means nothing changed on that side, so the cycle reuses the last
/// snapshot instead of asking the agent again. Over the agent transport,
/// where the skipped scan is a round trip.
#[test]
fn a_standing_watch_lets_the_next_cycle_skip_the_beta_scan() {
    let mut harness = Harness::new(SyncMode::TwoWaySafe, Transport::Agent);
    build_tree(&harness.alpha);
    let mut session = harness.session().expect("session");
    let report = session.run_cycle().expect("initial cycle");
    assert!(
        !report.beta_scan_skipped,
        "the first cycle has nothing to reuse"
    );
    // Quiet: a wait that returns false. Late watcher events for the trees
    // just built can wake the first waits; each wake is a cycle that finds
    // nothing. A false answer also means the agent has answered a watch
    // request, which is when the controller learns the root is watched.
    let mut quiet = false;
    for _ in 0..6 {
        if !session
            .await_change(std::time::Duration::from_secs(3))
            .expect("wait")
        {
            quiet = true;
            break;
        }
        session.run_cycle().expect("settling cycle");
    }
    assert!(quiet, "the pair never went quiet");

    fs::write(
        harness.alpha.join("dir0/nested/file0.txt"),
        "edited on alpha",
    )
    .unwrap();
    assert!(
        session
            .await_change(std::time::Duration::from_secs(5))
            .expect("wait"),
        "alpha's edit wakes the wait"
    );
    let report = session.run_cycle().expect("cycle");
    assert!(
        report.beta_scan_skipped,
        "beta's watch was standing: its scan is skipped"
    );
    assert!(!report.alpha_scan_skipped, "alpha changed: it is scanned");
    assert_eq!(report.beta_transitions, 1);
    harness.assert_trees_equal("after the skipped scan");

    // Beta changes: its watch answers, and a cycle scans it. Not always
    // the very next one — a wake from alpha's side can land before beta's
    // watch has fired for the edit, and that cycle still reuses beta's
    // snapshot; the watch fires moments later and the cycle after scans.
    // Never lost, at most one cycle later.
    fs::write(harness.beta.join("dir1/nested/file1.txt"), "edited on beta").unwrap();
    let mut scanned = false;
    for _ in 0..4 {
        assert!(session
            .await_change(std::time::Duration::from_secs(5))
            .expect("wait"));
        let report = session.run_cycle().expect("cycle");
        if !report.beta_scan_skipped {
            scanned = true;
            break;
        }
    }
    assert!(scanned, "beta's edit never brought a scan");
    assert_eq!(
        fs::read_to_string(harness.alpha.join("dir1/nested/file1.txt")).unwrap(),
        "edited on beta"
    );
    drop(session);
    harness.assert_trees_equal("after beta's edit");
}

/// Two sessions of a fan-out, over one alpha, running at the same time.
/// One is held by the cycle hook at the point where it is about to write
/// alpha while the other runs a whole cycle through the same alpha, then
/// released. Its write must be refused — alpha is no longer what it was
/// validated against — never landed over the other's. Over both
/// transports; the alpha is always local and shared in-process, which is
/// the observer's shared-root path under real contention.
mod fan_out_races {
    use super::*;
    use std::sync::mpsc;

    const BOTH: [Transport; 2] = [Transport::Local, Transport::Agent];
    const PATH: &str = "dir1/nested/file1.txt";
    const OTHER: &str = "dir2/nested/file2.txt";

    fn read(root: &Path, path: &str) -> String {
        fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{}: {e}", root.display()))
    }

    /// Makes the next cycle of each session read its trees in full, so it
    /// sees writes just made however far behind the watchers are.
    ///
    /// These tests are about what a race comes to, not about how fast a
    /// watcher reports, and an incremental scan sees only what has been
    /// reported. A pause lost that race about a third of the time with the
    /// three tests side by side; waiting for "a change" was no better, since
    /// a session's own initial copy leaves events of its own that arrive
    /// late and answer the wait first. The supervisor is untroubled by
    /// either — the next event wakes it again — but an interleaved test
    /// holds one particular cycle and needs it to see everything.
    fn look_again(sessions: &mut [&mut Session]) {
        for session in sessions {
            session.request_verify();
        }
    }

    /// Runs `held`'s cycle on a thread, stopped at `point` until `run`
    /// has been called on the main thread; returns the held cycle's report.
    fn interleave(
        held: &mut Session,
        point: CyclePoint,
        run: impl FnOnce(),
    ) -> anyhow::Result<CycleReport> {
        let (reached, at_point) = mpsc::channel::<()>();
        let (release, released) = mpsc::channel::<()>();
        held.set_cycle_hook(Box::new(move |at| {
            if at == point {
                let _ = reached.send(());
                let _ = released.recv();
            }
        }));
        std::thread::scope(|scope| {
            let cycle = scope.spawn(|| held.run_cycle());
            at_point.recv().expect("the held cycle reaches the point");
            run();
            release.send(()).expect("the held cycle is waiting");
            cycle.join().expect("the held cycle does not panic")
        })
    }

    /// Both betas edit the same file. The second session to reach alpha
    /// finds it already changed by the first and is refused; nothing is
    /// overwritten, and the pair is a conflict on its next cycle.
    #[test]
    fn two_betas_edit_one_file_and_the_later_write_is_refused() {
        for transport in BOTH {
            let context = format!("{transport:?}");
            let mut harness = Harness::new(SyncMode::TwoWaySafe, transport);
            build_tree(&harness.alpha);
            let (beta2, state2) = harness.second_beta();
            let mut s1 = harness.session().expect("session 1");
            let mut s2 = harness.session_to(&beta2, &state2).expect("session 2");
            s1.run_cycle().expect("initial 1");
            s2.run_cycle().expect("initial 2");

            fs::write(harness.beta.join(PATH), "from beta1").unwrap();
            fs::write(beta2.join(PATH), "from beta2").unwrap();
            look_again(&mut [&mut s1, &mut s2]);
            let report1 = interleave(&mut s1, CyclePoint::BeforeAlphaTransition, || {
                s2.run_cycle().expect("session 2's cycle");
            })
            .unwrap_or_else(|e| panic!("{context}: {e:#}"));

            assert_eq!(
                read(&harness.alpha, PATH),
                "from beta2",
                "{context}: overwritten"
            );
            assert_eq!(read(&harness.beta, PATH), "from beta1", "{context}");
            assert!(
                report1
                    .alpha_transition_problems
                    .iter()
                    .any(|p| p.path == PATH),
                "{context}: the refusal was not reported: {:?}",
                report1.alpha_transition_problems
            );
            let report1 = s1.run_cycle().expect("session 1 again");
            assert!(
                report1.conflicts.iter().any(|c| c.root == PATH),
                "{context}: expected a conflict, got {:?}",
                report1.conflicts
            );
            assert_eq!(read(&harness.alpha, PATH), "from beta2", "{context}");
            assert_eq!(read(&harness.beta, PATH), "from beta1", "{context}");
            assert_eq!(read(&beta2, PATH), "from beta2", "{context}");
        }
    }

    /// The betas edit different files. Both land on alpha, in either
    /// order, and each beta then gets the other's through alpha.
    #[test]
    fn two_betas_edit_different_files_and_both_land() {
        for transport in BOTH {
            let context = format!("{transport:?}");
            let mut harness = Harness::new(SyncMode::TwoWaySafe, transport);
            build_tree(&harness.alpha);
            let (beta2, state2) = harness.second_beta();
            let mut s1 = harness.session().expect("session 1");
            let mut s2 = harness.session_to(&beta2, &state2).expect("session 2");
            s1.run_cycle().expect("initial 1");
            s2.run_cycle().expect("initial 2");

            fs::write(harness.beta.join(PATH), "from beta1").unwrap();
            fs::write(beta2.join(OTHER), "from beta2").unwrap();
            look_again(&mut [&mut s1, &mut s2]);
            let report1 = interleave(&mut s1, CyclePoint::BeforeAlphaTransition, || {
                s2.run_cycle().expect("session 2's cycle");
            })
            .unwrap_or_else(|e| panic!("{context}: {e:#}"));
            assert!(
                report1.alpha_transition_problems.is_empty(),
                "{context}: {:?}",
                report1.alpha_transition_problems
            );
            assert_eq!(read(&harness.alpha, PATH), "from beta1", "{context}");
            assert_eq!(read(&harness.alpha, OTHER), "from beta2", "{context}");

            // Each pair levels on its next cycles.
            for _ in 0..3 {
                s1.run_cycle().expect("1");
                s2.run_cycle().expect("2");
            }
            for root in [&harness.alpha, &harness.beta, &beta2] {
                assert_eq!(
                    read(root, PATH),
                    "from beta1",
                    "{context}: {}",
                    root.display()
                );
                assert_eq!(
                    read(root, OTHER),
                    "from beta2",
                    "{context}: {}",
                    root.display()
                );
            }
            drop(s1);
            drop(s2);
            harness.assert_trees_equal(&context);
            assert_eq!(
                hash_tree(&harness.alpha),
                hash_tree(&beta2),
                "{context}: beta2 differs"
            );
        }
    }

    /// Alpha changes under a session between its scan and its transitions
    /// — another session lands an edit there — and the held session's own
    /// beta-bound transition is unaffected; the next cycle carries the
    /// other's edit on, with no conflict.
    #[test]
    fn an_edit_landing_on_alpha_between_a_scan_and_its_transition_is_carried_next_cycle() {
        for transport in BOTH {
            let context = format!("{transport:?}");
            let mut harness = Harness::new(SyncMode::TwoWaySafe, transport);
            build_tree(&harness.alpha);
            let (beta2, state2) = harness.second_beta();
            let mut s1 = harness.session().expect("session 1");
            let mut s2 = harness.session_to(&beta2, &state2).expect("session 2");
            s1.run_cycle().expect("initial 1");
            s2.run_cycle().expect("initial 2");

            // Session 1 carries an alpha edit to beta1; while its scans are
            // done and before it moves anything, session 2 lands beta2's
            // edit of another file on alpha.
            fs::write(harness.alpha.join(PATH), "alpha edit").unwrap();
            fs::write(beta2.join(OTHER), "from beta2").unwrap();
            look_again(&mut [&mut s1, &mut s2]);
            let report1 = interleave(&mut s1, CyclePoint::AfterScans, || {
                s2.run_cycle().expect("session 2's cycle");
            })
            .unwrap_or_else(|e| panic!("{context}: {e:#}"));
            assert!(
                report1.conflicts.is_empty(),
                "{context}: {:?}",
                report1.conflicts
            );
            assert_eq!(read(&harness.beta, PATH), "alpha edit", "{context}");

            // What session 2 landed on alpha reaches beta1 on the cycles
            // that follow, as the watcher reports it, and never as a
            // conflict. (Session 1's own writes may wake it first, so it
            // cycles on each wake until the edit arrives.)
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while read(&harness.beta, OTHER) != "from beta2" {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{context}: not carried"
                );
                let _ = s1.await_change(std::time::Duration::from_millis(500));
                let report1 = s1.run_cycle().expect("session 1 again");
                assert!(
                    report1.conflicts.is_empty(),
                    "{context}: {:?}",
                    report1.conflicts
                );
            }
            for _ in 0..2 {
                s2.run_cycle().expect("2");
            }
            assert_eq!(read(&beta2, PATH), "alpha edit", "{context}");
        }
    }
}

/// What an unreadable ancestor comes to, by whether the two sides match.
mod unreadable_ancestor {
    use super::*;

    /// An ancestor in this build's own format whose bytes no longer match
    /// their digest: damage, not a format another build wrote. Early on the
    /// ancestor is all journal, so the journal's last byte is the one
    /// flipped when there is a journal.
    fn damage(harness: &Harness) {
        let journal = harness.state.join("ancestor.journal");
        let path = match fs::metadata(&journal) {
            Ok(metadata) if metadata.len() > 0 => journal,
            _ => harness.state.join("ancestor"),
        };
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&path, bytes).unwrap();
    }

    fn set_aside(harness: &Harness) -> usize {
        fs::read_dir(&harness.state)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".unreadable-"))
            .count()
    }

    #[test]
    fn matching_sides_rebuild_it_and_keep_the_old_one() {
        let mut harness = Harness::new(SyncMode::TwoWaySafe, Transport::Local);
        build_tree(&harness.alpha);
        harness.cycle_ok();
        damage(&harness);

        let report = harness.cycle().expect("matching sides rebuild");
        assert!(!report.changed(), "nothing to carry: {report:?}");
        assert!(set_aside(&harness) >= 1, "the unreadable one is kept");
        assert!(harness.state.join("ancestor.rebuilt").exists());

        // The rebuilt ancestor is a real one: a deletion propagates as a
        // deletion, not as a file to bring back.
        fs::remove_file(harness.alpha.join("dir0/nested/file0.txt")).unwrap();
        harness.cycle_ok();
        assert!(!harness.beta.join("dir0/nested/file0.txt").exists());
        harness.assert_trees_equal("after the rebuild");
    }

    #[test]
    fn differing_sides_halt_and_nothing_moves() {
        let mut harness = Harness::new(SyncMode::TwoWaySafe, Transport::Local);
        build_tree(&harness.alpha);
        harness.cycle_ok();
        damage(&harness);
        // A deletion the lost ancestor knew about: without it, the file on
        // beta would look new and come back.
        fs::remove_file(harness.alpha.join("dir0/nested/file0.txt")).unwrap();

        let error = harness.cycle().expect_err("differing sides must halt");
        assert!(
            matches!(
                error.downcast_ref::<SafetyHalt>(),
                Some(SafetyHalt::AncestorUnreadable(_))
            ),
            "{error:#}"
        );
        assert!(
            !harness.alpha.join("dir0/nested/file0.txt").exists(),
            "not resurrected"
        );
        assert!(
            harness.beta.join("dir0/nested/file0.txt").exists(),
            "not deleted either"
        );
        assert_eq!(set_aside(&harness), 0, "nothing set aside while it halts");

        // Settled by hand, it rebuilds on its own.
        fs::remove_file(harness.beta.join("dir0/nested/file0.txt")).unwrap();
        harness.cycle_ok();
        assert!(set_aside(&harness) >= 1);
    }

    #[test]
    fn damage_a_second_time_is_not_rebuilt_until_a_reset() {
        let mut harness = Harness::new(SyncMode::TwoWaySafe, Transport::Local);
        build_tree(&harness.alpha);
        harness.cycle_ok();
        damage(&harness);
        harness.cycle_ok();
        damage(&harness);
        let error = harness.cycle().expect_err("damaged twice");
        assert!(
            matches!(
                error.downcast_ref::<SafetyHalt>(),
                Some(SafetyHalt::AncestorDamagedAgain(_))
            ),
            "{error:#}"
        );
    }

    #[test]
    fn a_format_from_another_build_rebuilds_without_counting_as_damage() {
        let mut harness = Harness::new(SyncMode::TwoWaySafe, Transport::Local);
        build_tree(&harness.alpha);
        harness.cycle_ok();
        // A checkpoint stating a format no build has written yet.
        let mut future = b"ABAHNAN2".to_vec();
        future.extend_from_slice(&999u16.to_le_bytes());
        future.extend_from_slice(&[0; 32]);
        for _ in 0..2 {
            fs::write(harness.state.join("ancestor"), &future).unwrap();
            let _ = fs::remove_file(harness.state.join("ancestor.journal"));
            harness.cycle().expect("a format is rebuilt, every time");
        }
        assert!(!harness.state.join("ancestor.rebuilt").exists());
    }
}
