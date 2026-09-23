//! The implementation, held to `spec/Autobahn.tla`.
//!
//! The spec is a small board game: one alpha, N betas, a few paths, a few
//! values, and the moves a user or a cycle can make. TLC plays every
//! sequence of moves and checks the properties. This file plays the same
//! game with the *real* reconciler making the cycle's move, checks the
//! same properties in Rust, and writes each game out as a trace that TLC
//! validates against the spec — so the spec cannot drift from the code
//! without a test going red.
//!
//! Two tests: `random_runs_keep_the_invariants` always runs (a few
//! thousand random games, milliseconds each). `traces_are_behaviors_of_the_spec`
//! also writes the games out and runs TLC over them; it needs Java and
//! `tla2tools.jar` (see `spec/check.sh`), so it is skipped unless
//! `AUTOBAHN_TLC=1`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use autobahn::endpoint::{achieved_changes, TransitionOutcome};
use autobahn::tree::{apply, reconcile, Content, Digest, FileMetadata, Node, SyncMode};

/// The board: constants of the game, matching a TLC configuration.
struct Board {
    betas: usize,
    paths: Vec<&'static str>,
    values: Vec<&'static str>,
}

const PATHS: [&str; 3] = ["p1", "p2", "p3"];
const VALUES: [&str; 3] = ["v1", "v2", "v3"];

/// A side of the game: alpha, or beta number i.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Alpha,
    Beta(usize),
}

/// One flat tree: path → value (a missing path is `NoFile`).
type Tree = BTreeMap<&'static str, &'static str>;

/// The game's state, as the spec's variables.
struct Game {
    mode: SyncMode,
    alpha: Tree,
    beta: Vec<Tree>,
    ancestor: Vec<Tree>,
    conflicts: Vec<BTreeSet<&'static str>>,
    // Bookkeeping, as in the spec.
    written: BTreeSet<(Side, &'static str, &'static str)>,
    superseded: BTreeSet<(&'static str, &'static str)>,
    discarded: BTreeSet<(Side, &'static str, &'static str)>,
    resolved: BTreeSet<(Side, &'static str, &'static str)>,
    edits: usize,
    /// Every state after every step, for the trace.
    trace: Vec<String>,
    /// The steps taken, for a failure report.
    steps: Vec<String>,
}

impl PartialOrd for Side {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Side {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let key = |s: &Side| match s {
            Side::Alpha => 0,
            Side::Beta(i) => i + 1,
        };
        key(self).cmp(&key(other))
    }
}

fn digest_of(value: &str) -> Digest {
    *blake3::hash(value.as_bytes()).as_bytes()
}

/// A flat tree as the reconciler sees it: a root directory of files whose
/// digest is the value's hash.
fn to_node(tree: &Tree) -> Option<Node> {
    let children = tree
        .iter()
        .map(|(path, value)| Node {
            name: path.to_string(),
            content: Content::File {
                digest: digest_of(value),
                executable: false,
                metadata: FileMetadata {
                    mtime_seconds: 1,
                    mtime_nanos: 0,
                    size: value.len() as u64,
                    inode: 1,
                    mode: 0o100644,
                },
            },
        })
        .collect();
    Some(Node::directory("", children))
}

/// Back from the reconciler's tree to the game's, by digest.
fn from_node(node: Option<&Node>, board: &Board) -> Tree {
    let mut tree = Tree::new();
    if let Some(root) = node {
        for child in root.children() {
            if let Content::File { digest, .. } = &child.content {
                let value = board
                    .values
                    .iter()
                    .find(|v| digest_of(v) == *digest)
                    .unwrap_or_else(|| panic!("unknown digest at {}", child.name));
                let path = board
                    .paths
                    .iter()
                    .find(|p| **p == child.name)
                    .unwrap_or_else(|| panic!("unknown path {}", child.name));
                tree.insert(path, value);
            }
        }
    }
    tree
}

impl Game {
    fn new(mode: SyncMode, board: &Board) -> Game {
        let mut game = Game {
            mode,
            alpha: Tree::new(),
            beta: vec![Tree::new(); board.betas],
            ancestor: vec![Tree::new(); board.betas],
            conflicts: vec![BTreeSet::new(); board.betas],
            written: BTreeSet::new(),
            superseded: BTreeSet::new(),
            discarded: BTreeSet::new(),
            resolved: BTreeSet::new(),
            edits: 0,
            trace: Vec::new(),
            steps: Vec::new(),
        };
        game.record(board);
        game
    }

    fn held(&self, side: Side, path: &'static str) -> Option<&'static str> {
        match side {
            Side::Alpha => self.alpha.get(path).copied(),
            Side::Beta(i) => self.beta[i].get(path).copied(),
        }
    }

    fn tree_mut(&mut self, side: Side) -> &mut Tree {
        match side {
            Side::Alpha => &mut self.alpha,
            Side::Beta(i) => &mut self.beta[i],
        }
    }

    /// The spec's `Write`.
    fn write(&mut self, side: Side, path: &'static str, value: &'static str) -> bool {
        if self.held(side, path) == Some(value) {
            return false;
        }
        if let Some(old) = self.held(side, path) {
            self.superseded.insert((path, old));
        }
        self.tree_mut(side).insert(path, value);
        self.written.insert((side, path, value));
        self.edits += 1;
        self.steps.push(format!("write {side:?} {path} {value}"));
        true
    }

    /// The spec's `Remove`.
    fn remove(&mut self, side: Side, path: &'static str) -> bool {
        let Some(old) = self.held(side, path) else {
            return false;
        };
        self.superseded.insert((path, old));
        self.tree_mut(side).remove(path);
        self.edits += 1;
        self.steps.push(format!("remove {side:?} {path}"));
        true
    }

    /// The spec's `Resolve`.
    fn resolve(&mut self, i: usize, path: &'static str, keep_alpha: bool) -> bool {
        if !self.conflicts[i].contains(path) || self.alpha.get(path) == self.beta[i].get(path) {
            return false;
        }
        if keep_alpha {
            if let Some(lost) = self.beta[i].get(path) {
                self.resolved.insert((Side::Beta(i), path, lost));
            }
            match self.alpha.get(path).copied() {
                Some(v) => self.beta[i].insert(path, v),
                None => self.beta[i].remove(path),
            };
        } else {
            if let Some(lost) = self.alpha.get(path) {
                self.resolved.insert((Side::Alpha, path, lost));
            }
            match self.beta[i].get(path).copied() {
                Some(v) => self.alpha.insert(path, v),
                None => self.alpha.remove(path),
            };
        }
        self.conflicts[i].remove(path);
        self.edits += 1;
        self.steps.push(format!(
            "resolve b{} {path} keep {}",
            i + 1,
            if keep_alpha { "alpha" } else { "beta" }
        ));
        true
    }

    /// The spec's `Cycle`, with the real reconciler making the move.
    fn cycle(&mut self, i: usize, board: &Board) {
        let ancestor = to_node(&self.ancestor[i]);
        let alpha = to_node(&self.alpha);
        let beta = to_node(&self.beta[i]);
        let r = reconcile(ancestor.as_ref(), alpha.as_ref(), beta.as_ref(), self.mode);

        // A perfect transition achieves exactly what it was asked.
        let achieved = |transitions: &[autobahn::tree::Change]| {
            let outcome = TransitionOutcome {
                results: transitions.iter().map(|c| c.new.clone()).collect(),
                problems: Vec::new(),
                missing_staged_files: false,
                missing_staged: Vec::new(),
            };
            achieved_changes(transitions, &outcome)
        };
        let alpha_after = apply(alpha.as_ref(), &r.alpha_transitions).expect("alpha applies");
        let beta_after = apply(beta.as_ref(), &r.beta_transitions).expect("beta applies");
        let mut ancestor_changes = r.ancestor_changes.clone();
        ancestor_changes.extend(achieved(&r.beta_transitions));
        ancestor_changes.extend(achieved(&r.alpha_transitions));
        let ancestor_after = apply(ancestor.as_ref(), &ancestor_changes).expect("ancestor applies");

        let alpha_before = self.alpha.clone();
        let beta_before = self.beta[i].clone();
        let anc_before = self.ancestor[i].clone();
        self.alpha = from_node(alpha_after.as_ref(), board);
        self.beta[i] = from_node(beta_after.as_ref(), board);
        self.ancestor[i] = from_node(ancestor_after.as_ref(), board);
        self.conflicts[i] = r
            .conflicts
            .iter()
            .map(|c| {
                *board
                    .paths
                    .iter()
                    .find(|p| **p == c.root)
                    .unwrap_or_else(|| panic!("conflict at an unknown root {:?}", c.root))
            })
            .collect();

        // What the cycle took from a side without carrying it anywhere.
        // Recorded operationally — from the trees, not from the rules — so
        // this is a check on the reconciler, not a restatement of it.
        for &path in &board.paths {
            let y = beta_before.get(path).copied();
            if let Some(y) = y {
                let changed = anc_before.get(path).copied() != Some(y);
                let gone = self.beta[i].get(path).copied() != Some(y)
                    && self.alpha.get(path).copied() != Some(y);
                if changed && gone {
                    self.discarded.insert((Side::Beta(i), path, y));
                }
            }
            if let Some(x) = alpha_before.get(path).copied() {
                let changed = anc_before.get(path).copied() != Some(x);
                let gone = self.alpha.get(path).copied() != Some(x)
                    && self.beta[i].get(path).copied() != Some(x);
                if changed && gone {
                    self.discarded.insert((Side::Alpha, path, x));
                }
            }
        }
        self.steps.push(format!("cycle b{}", i + 1));

        // LevelledAfterCycle.
        if self.conflicts[i].is_empty() {
            for &path in &board.paths {
                assert_eq!(
                    self.ancestor[i].get(path),
                    self.alpha.get(path),
                    "pair {} not levelled at {path} after a clean cycle\n{}",
                    i + 1,
                    self.report()
                );
                assert_eq!(
                    self.ancestor[i].get(path),
                    self.beta[i].get(path),
                    "pair {} not levelled at {path} after a clean cycle\n{}",
                    i + 1,
                    self.report()
                );
            }
        }
    }

    /// The invariants, as the spec states them.
    fn check(&self, board: &Board) {
        // Accounted.
        for &(side, path, value) in &self.written {
            let present = self.alpha.get(path) == Some(&value)
                || self.beta.iter().any(|b| b.get(path) == Some(&value));
            let accounted = present
                || self.superseded.contains(&(path, value))
                || self.discarded.iter().any(|d| d.1 == path && d.2 == value)
                || self.resolved.iter().any(|d| d.1 == path && d.2 == value);
            assert!(
                accounted,
                "{value} written on {side:?} at {path} is gone and unaccounted for\n{}",
                self.report()
            );
        }
        // NeverDiscards.
        if self.mode == SyncMode::TwoWaySafe {
            assert!(
                self.discarded.is_empty(),
                "conflict mode discarded {:?}\n{}",
                self.discarded,
                self.report()
            );
        }
        // AlphaKeeps.
        for d in &self.discarded {
            assert!(
                d.0 != Side::Alpha,
                "alpha lost {} at {} to a beta\n{}",
                d.2,
                d.1,
                self.report()
            );
        }
        let _ = board;
    }

    fn report(&self) -> String {
        format!(
            "mode {:?}\nsteps:\n  {}\nalpha {:?}\nbetas {:?}\nancestors {:?}\nconflicts {:?}",
            self.mode,
            self.steps.join("\n  "),
            self.alpha,
            self.beta,
            self.ancestor,
            self.conflicts
        )
    }

    /// The state as a TLA+ record, appended to the trace.
    fn record(&mut self, board: &Board) {
        let tree = |t: &Tree| -> String {
            board
                .paths
                .iter()
                .map(|p| format!("{p} :> {}", t.get(p).copied().unwrap_or("NoFile")))
                .collect::<Vec<_>>()
                .join(" @@ ")
        };
        let per_beta = |f: &dyn Fn(usize) -> String| -> String {
            (0..board.betas)
                .map(|i| format!("b{} :> {}", i + 1, f(i)))
                .collect::<Vec<_>>()
                .join(" @@ ")
        };
        let mut s = String::new();
        write!(s, "[alpha |-> ({}), ", tree(&self.alpha)).unwrap();
        write!(s, "beta |-> ({}), ", per_beta(&|i| format!("({})", tree(&self.beta[i])))).unwrap();
        write!(s, "ancestor |-> ({}), ", per_beta(&|i| format!("({})", tree(&self.ancestor[i])))).unwrap();
        write!(
            s,
            "conflicts |-> ({})]",
            per_beta(&|i| format!(
                "{{{}}}",
                self.conflicts[i].iter().copied().collect::<Vec<_>>().join(", ")
            ))
        )
        .unwrap();
        self.trace.push(s);
    }
}

/// A small deterministic generator, so a failing game can be replayed by
/// its seed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Plays one random game of `moves` user actions (writes, removes,
/// resolutions) interleaved with cycles, checking after every step, and
/// ending with enough cycles for everything to settle.
fn play(seed: u64, mode: SyncMode, board: &Board, moves: usize) -> Game {
    let mut rng = Rng(seed | 1);
    let mut game = Game::new(mode, board);
    let sides: Vec<Side> = std::iter::once(Side::Alpha)
        .chain((0..board.betas).map(Side::Beta))
        .collect();
    while game.edits < moves {
        let acted = match rng.below(4) {
            0 | 1 => {
                let side = sides[rng.below(sides.len())];
                let path = board.paths[rng.below(board.paths.len())];
                let value = board.values[rng.below(board.values.len())];
                game.write(side, path, value)
            }
            2 => {
                let side = sides[rng.below(sides.len())];
                let path = board.paths[rng.below(board.paths.len())];
                game.remove(side, path)
            }
            _ => {
                let i = rng.below(board.betas);
                let path = board.paths[rng.below(board.paths.len())];
                game.resolve(i, path, rng.below(2) == 0)
            }
        };
        if !acted {
            continue;
        }
        game.record(board);
        game.check(board);
        // Some cycles between user actions, in any order.
        for _ in 0..rng.below(3) {
            let i = rng.below(board.betas);
            game.cycle(i, board);
            game.record(board);
            game.check(board);
        }
    }
    // Settle: every pair cycles until nothing moves.
    for _ in 0..3 {
        for i in 0..board.betas {
            game.cycle(i, board);
            game.record(board);
            game.check(board);
        }
    }
    // Converges: level everywhere but at reported conflicts, and in the
    // alpha modes, level everywhere.
    for i in 0..board.betas {
        for &path in &board.paths {
            let level = game.alpha.get(path) == game.beta[i].get(path);
            assert!(
                level || game.conflicts[i].contains(path),
                "pair {} did not converge at {path}\n{}",
                i + 1,
                game.report()
            );
            if mode != SyncMode::TwoWaySafe {
                assert!(level, "pair {} did not converge at {path}\n{}", i + 1, game.report());
            }
        }
    }
    game
}

const MODES: [SyncMode; 3] = [SyncMode::TwoWaySafe, SyncMode::TwoWayResolved, SyncMode::TwoWayStrict];

#[test]
fn random_runs_keep_the_invariants() {
    let board = Board {
        betas: 3,
        paths: PATHS.to_vec(),
        values: VALUES.to_vec(),
    };
    for mode in MODES {
        for seed in 1..=1500u64 {
            play(seed * 7919, mode, &board, 8);
        }
    }
}

/// Writes a game out as a TLA+ module TLC can check against the spec, and
/// the configuration that goes with it.
fn write_trace(dir: &std::path::Path, index: usize, game: &Game, board: &Board, mode: SyncMode) {
    let name = format!("Trace{index}");
    let mode_name = match mode {
        SyncMode::TwoWaySafe => "conflict",
        SyncMode::TwoWayResolved => "alpha",
        SyncMode::TwoWayStrict => "strict",
        _ => unreachable!(),
    };
    let symbols: Vec<String> = (1..=board.betas)
        .map(|i| format!("b{i}"))
        .chain(board.paths.iter().map(|p| p.to_string()))
        .chain(board.values.iter().map(|v| v.to_string()))
        .collect();
    let module = [
        format!("---- MODULE {name} ----"),
        "EXTENDS Autobahn, Sequences, TLC".to_string(),
        format!("CONSTANTS {}", symbols.join(", ")),
        format!("Trace == <<\n  {}\n>>", game.trace.join(",\n  ")),
        "VARIABLE i".to_string(),
        // The logged trees and conflicts must be what the spec's own step
        // produces; the bookkeeping variables are the spec's to choose.
        "Match == /\\ alpha' = Trace[i + 1].alpha /\\ beta' = Trace[i + 1].beta".to_string(),
        "         /\\ ancestor' = Trace[i + 1].ancestor /\\ conflicts' = Trace[i + 1].conflicts".to_string(),
        "TInit == Init /\\ i = 1".to_string(),
        // A step the spec does not allow leaves no successor: a deadlock,
        // which the configuration turns into a rejection. The end of the
        // trace stutters, so a complete trace never deadlocks.
        "TNext == \\/ i < Len(Trace) /\\ Next /\\ Match /\\ i' = i + 1".to_string(),
        "         \\/ i = Len(Trace) /\\ UNCHANGED <<vars, i>>".to_string(),
        "TSpec == TInit /\\ [][TNext]_<<vars, i>>".to_string(),
        "====".to_string(),
    ]
    .join("\n");
    std::fs::write(dir.join(format!("{name}.tla")), module).unwrap();
    let cfg = format!(
        "SPECIFICATION TSpec\nCONSTANTS\n{}    Betas = {{{}}}\n    Paths = {{{}}}\n    Values = {{{}}}\n    \
         NoFile = NoFile\n    Mode = \"{mode_name}\"\n    MaxEdits = {}\nCHECK_DEADLOCK TRUE\n",
        symbols.iter().map(|s| format!("    {s} = {s}\n")).collect::<String>(),
        (1..=board.betas).map(|i| format!("b{i}")).collect::<Vec<_>>().join(", "),
        board.paths.join(", "),
        board.values.join(", "),
        game.edits
    );
    std::fs::write(dir.join(format!("{name}.cfg")), cfg).unwrap();
}

#[test]
fn traces_are_behaviors_of_the_spec() {
    if std::env::var("AUTOBAHN_TLC").is_err() {
        eprintln!("skipped: set AUTOBAHN_TLC=1 to validate traces with TLC");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // The spec's configured board, so the trace constants match its.
    let board = Board {
        betas: 2,
        paths: PATHS[..2].to_vec(),
        values: VALUES[..2].to_vec(),
    };
    let per_mode: usize = std::env::var("AUTOBAHN_TLC_TRACES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(8);
    let mut index = 0;
    for mode in MODES {
        for seed in 1..=per_mode as u64 {
            let game = play(seed * 104_729, mode, &board, 5);
            write_trace(dir.path(), index, &game, &board, mode);
            index += 1;
        }
    }
    let check = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/check.sh");
    let status = std::process::Command::new(check)
        .arg("--traces")
        .arg(dir.path())
        .status()
        .expect("spec/check.sh runs");
    // Kept for inspection when asked, or when TLC rejected something.
    let path = if std::env::var("AUTOBAHN_TLC_KEEP").is_ok() || !status.success() {
        let kept = dir.keep();
        eprintln!("traces kept in {}", kept.display());
        kept
    } else {
        dir.path().to_path_buf()
    };
    assert!(status.success(), "TLC rejected a trace; see the logs in {}", path.display());
}
