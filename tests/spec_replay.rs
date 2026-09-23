//! The implementation, held to `spec/Autobahn.tla`.
//!
//! The spec is a small board game: one alpha, N betas, a small hierarchy
//! of paths, a few file values, and the moves a user or a cycle can make.
//! TLC plays every sequence of moves and checks the properties. This file
//! plays the same game with the *real* reconciler making the cycle's move,
//! checks the same properties in Rust, and writes each game out as a trace
//! that TLC validates against the spec — so the spec cannot drift from the
//! code without a test going red.
//!
//! Two tests: `random_runs_keep_the_invariants` always runs (a few
//! thousand random games, milliseconds each). `traces_are_behaviors_of_the_spec`
//! also writes the games out and runs TLC over them; it needs Java and
//! `tla2tools.jar` (see `spec/check.sh`), so it is skipped unless
//! `AUTOBAHN_TLC=1`.

use std::collections::BTreeSet;

use autobahn::endpoint::{achieved_changes, TransitionOutcome};
use autobahn::tree::{apply, reconcile, Change, Content, Digest, FileMetadata, Node, SyncMode};

/// The board: the constants of the game, matching a TLC configuration.
/// Paths are sequences of names from the root, prefix-closed, and are
/// referred to by index.
struct Board {
    betas: usize,
    paths: Vec<Vec<&'static str>>,
    values: Vec<&'static str>,
}

impl Board {
    /// The spec's model: `d`, `d/a`, `d/b`, `f`.
    fn small() -> Board {
        Board {
            betas: 2,
            paths: vec![vec!["d"], vec!["d", "a"], vec!["d", "b"], vec!["f"]],
            values: vec!["v1", "v2"],
        }
    }

    /// A wider board for the Rust-only runs: three betas, a nested
    /// directory, three values.
    fn wide() -> Board {
        Board {
            betas: 3,
            paths: vec![
                vec!["d"],
                vec!["d", "a"],
                vec!["d", "b"],
                vec!["d", "e"],
                vec!["d", "e", "x"],
                vec!["f"],
            ],
            values: vec!["v1", "v2", "v3"],
        }
    }

    fn parent(&self, p: usize) -> Option<usize> {
        let path = &self.paths[p];
        if path.len() == 1 {
            return None;
        }
        let parent = &path[..path.len() - 1];
        self.paths.iter().position(|q| q == parent)
    }

    /// Whether q is p or above it.
    fn is_prefix(&self, q: usize, p: usize) -> bool {
        self.paths[p].starts_with(&self.paths[q])
    }

    fn subtree(&self, q: usize) -> Vec<usize> {
        (0..self.paths.len())
            .filter(|&p| self.is_prefix(q, p))
            .collect()
    }

    fn joined(&self, p: usize) -> String {
        self.paths[p].join("/")
    }

    fn index_of(&self, joined: &str) -> Option<usize> {
        (0..self.paths.len()).find(|&p| self.joined(p) == joined)
    }

    fn tla_path(&self, p: usize) -> String {
        format!(
            "<<{}>>",
            self.paths[p]
                .iter()
                .map(|n| format!("\"{n}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// A side of the game: alpha, or beta number i.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
enum Side {
    Alpha,
    Beta(usize),
}

/// What a path holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cell {
    NoFile,
    Dir,
    File(&'static str),
}

impl Cell {
    fn value(self) -> Option<&'static str> {
        match self {
            Cell::File(v) => Some(v),
            _ => None,
        }
    }
    fn tla(self) -> String {
        match self {
            Cell::NoFile => "NoFile".into(),
            Cell::Dir => "Dir".into(),
            Cell::File(v) => v.into(),
        }
    }
}

/// One tree: a cell per path, by index.
type Tree = Vec<Cell>;

/// The game's state, as the spec's variables.
struct Game {
    mode: SyncMode,
    alpha: Tree,
    beta: Vec<Tree>,
    ancestor: Vec<Tree>,
    conflicts: Vec<BTreeSet<usize>>,
    written: BTreeSet<(Side, usize, &'static str)>,
    superseded: BTreeSet<(usize, &'static str)>,
    discarded: BTreeSet<(Side, usize, &'static str)>,
    resolved: BTreeSet<(Side, usize, &'static str)>,
    edits: usize,
    /// Every state after every step, for the trace.
    trace: Vec<String>,
    /// The steps taken, for a failure report.
    steps: Vec<String>,
}

fn digest_of(value: &str) -> Digest {
    *blake3::hash(value.as_bytes()).as_bytes()
}

/// A tree as the reconciler sees it: nested directories of files whose
/// digest is the value's hash.
fn to_node(tree: &Tree, board: &Board) -> Option<Node> {
    fn children_of(parent: Option<usize>, tree: &Tree, board: &Board) -> Vec<Node> {
        (0..board.paths.len())
            .filter(|&p| board.parent(p) == parent && tree[p] != Cell::NoFile)
            .map(|p| {
                let name = board.paths[p].last().unwrap().to_string();
                match tree[p] {
                    Cell::Dir => Node::directory(name, children_of(Some(p), tree, board)),
                    Cell::File(value) => Node {
                        name,
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
                    },
                    Cell::NoFile => unreachable!(),
                }
            })
            .collect()
    }
    Some(Node::directory("", children_of(None, tree, board)))
}

/// Back from the reconciler's tree to the game's, by name and digest.
fn from_node(node: Option<&Node>, board: &Board) -> Tree {
    fn walk(node: &Node, prefix: &str, tree: &mut Tree, board: &Board) {
        for child in node.children() {
            let joined = if prefix.is_empty() {
                child.name.clone()
            } else {
                format!("{prefix}/{}", child.name)
            };
            let p = board
                .index_of(&joined)
                .unwrap_or_else(|| panic!("the reconciler produced an unknown path {joined}"));
            match &child.content {
                Content::Directory(_) => {
                    tree[p] = Cell::Dir;
                    walk(child, &joined, tree, board);
                }
                Content::File { digest, .. } => {
                    let value = board
                        .values
                        .iter()
                        .find(|v| digest_of(v) == *digest)
                        .unwrap_or_else(|| panic!("unknown digest at {joined}"));
                    tree[p] = Cell::File(value);
                }
                other => panic!("unexpected content at {joined}: {other:?}"),
            }
        }
    }
    let mut tree = vec![Cell::NoFile; board.paths.len()];
    if let Some(root) = node {
        walk(root, "", &mut tree, board);
    }
    tree
}

impl Game {
    fn new(mode: SyncMode, board: &Board) -> Game {
        let empty = vec![Cell::NoFile; board.paths.len()];
        let mut game = Game {
            mode,
            alpha: empty.clone(),
            beta: vec![empty.clone(); board.betas],
            ancestor: vec![empty; board.betas],
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

    fn tree(&self, side: Side) -> &Tree {
        match side {
            Side::Alpha => &self.alpha,
            Side::Beta(i) => &self.beta[i],
        }
    }

    fn tree_mut(&mut self, side: Side) -> &mut Tree {
        match side {
            Side::Alpha => &mut self.alpha,
            Side::Beta(i) => &mut self.beta[i],
        }
    }

    fn parent_ok(&self, side: Side, p: usize, board: &Board) -> bool {
        match board.parent(p) {
            None => true,
            Some(parent) => self.tree(side)[parent] == Cell::Dir,
        }
    }

    /// The spec's `Cleared`: p becomes `cell`, everything under it goes,
    /// and every file value that was in the subtree is superseded.
    fn clear(&mut self, side: Side, p: usize, cell: Cell, board: &Board) {
        for r in board.subtree(p) {
            if let Some(v) = self.tree(side)[r].value() {
                self.superseded.insert((r, v));
            }
            self.tree_mut(side)[r] = Cell::NoFile;
        }
        self.tree_mut(side)[p] = cell;
    }

    /// The spec's `Write`.
    fn write(&mut self, side: Side, p: usize, value: &'static str, board: &Board) -> bool {
        if !self.parent_ok(side, p, board) || self.tree(side)[p] == Cell::File(value) {
            return false;
        }
        self.clear(side, p, Cell::File(value), board);
        self.written.insert((side, p, value));
        self.edits += 1;
        self.steps
            .push(format!("write {side:?} {} {value}", board.joined(p)));
        true
    }

    /// The spec's `MkDir`.
    fn mkdir(&mut self, side: Side, p: usize, board: &Board) -> bool {
        if !self.parent_ok(side, p, board) || self.tree(side)[p] == Cell::Dir {
            return false;
        }
        self.clear(side, p, Cell::Dir, board);
        self.edits += 1;
        self.steps
            .push(format!("mkdir {side:?} {}", board.joined(p)));
        true
    }

    /// The spec's `Remove`.
    fn remove(&mut self, side: Side, p: usize, board: &Board) -> bool {
        if self.tree(side)[p] == Cell::NoFile {
            return false;
        }
        self.clear(side, p, Cell::NoFile, board);
        self.edits += 1;
        self.steps
            .push(format!("remove {side:?} {}", board.joined(p)));
        true
    }

    /// The spec's `Resolve`: one side's version of the conflict unit.
    fn resolve(&mut self, i: usize, q: usize, keep_alpha: bool, board: &Board) -> bool {
        if !self.conflicts[i].contains(&q) {
            return false;
        }
        for r in board.subtree(q) {
            let (winner, loser_side) = if keep_alpha {
                (self.alpha[r], Side::Beta(i))
            } else {
                (self.beta[i][r], Side::Alpha)
            };
            let loser = self.tree(loser_side)[r];
            if let Some(v) = loser.value() {
                if winner != loser {
                    self.resolved.insert((loser_side, r, v));
                }
            }
            self.tree_mut(loser_side)[r] = winner;
        }
        self.conflicts[i].remove(&q);
        self.edits += 1;
        self.steps.push(format!(
            "resolve b{} {} keep {}",
            i + 1,
            board.joined(q),
            if keep_alpha { "alpha" } else { "beta" }
        ));
        true
    }

    /// The spec's `Cycle`, with the real reconciler making the move.
    fn cycle(&mut self, i: usize, board: &Board) {
        let ancestor = to_node(&self.ancestor[i], board);
        let alpha = to_node(&self.alpha, board);
        let beta = to_node(&self.beta[i], board);
        let r = reconcile(ancestor.as_ref(), alpha.as_ref(), beta.as_ref(), self.mode);

        // A perfect transition achieves exactly what it was asked.
        let achieved = |transitions: &[Change]| {
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
                board
                    .index_of(&c.root)
                    .unwrap_or_else(|| panic!("conflict at an unknown root {:?}", c.root))
            })
            .collect();

        // What the cycle took from a side and carried nowhere, read off
        // the trees rather than the rules — so this checks the reconciler
        // instead of restating it.
        for p in 0..board.paths.len() {
            if let Some(y) = beta_before[p].value() {
                let changed = anc_before[p] != Cell::File(y);
                let gone = self.beta[i][p] != Cell::File(y) && self.alpha[p] != Cell::File(y);
                if changed && gone {
                    self.discarded.insert((Side::Beta(i), p, y));
                }
            }
            if let Some(x) = alpha_before[p].value() {
                let changed = anc_before[p] != Cell::File(x);
                let gone = self.alpha[p] != Cell::File(x) && self.beta[i][p] != Cell::File(x);
                if changed && gone {
                    self.discarded.insert((Side::Alpha, p, x));
                }
            }
        }
        self.steps.push(format!("cycle b{}", i + 1));

        // LevelledAfterCycle.
        if self.conflicts[i].is_empty() {
            for p in 0..board.paths.len() {
                assert!(
                    self.ancestor[i][p] == self.alpha[p] && self.ancestor[i][p] == self.beta[i][p],
                    "pair {} not levelled at {} after a clean cycle\n{}",
                    i + 1,
                    board.joined(p),
                    self.report(board)
                );
            }
        }
    }

    fn in_conflict(&self, i: usize, p: usize, board: &Board) -> bool {
        self.conflicts[i].iter().any(|&q| board.is_prefix(q, p))
    }

    /// The invariants, as the spec states them.
    fn check(&self, board: &Board) {
        // WellFormed.
        let formed = |t: &Tree| {
            (0..board.paths.len()).all(|p| {
                t[p] == Cell::NoFile || board.parent(p).is_none_or(|parent| t[parent] == Cell::Dir)
            })
        };
        assert!(
            formed(&self.alpha),
            "alpha malformed\n{}",
            self.report(board)
        );
        for i in 0..board.betas {
            assert!(
                formed(&self.beta[i]),
                "beta {} malformed\n{}",
                i + 1,
                self.report(board)
            );
            assert!(
                formed(&self.ancestor[i]),
                "ancestor {} malformed\n{}",
                i + 1,
                self.report(board)
            );
        }
        // Accounted.
        for &(side, p, value) in &self.written {
            let present = self.alpha[p] == Cell::File(value)
                || self.beta.iter().any(|b| b[p] == Cell::File(value));
            let accounted = present
                || self.superseded.contains(&(p, value))
                || self.discarded.iter().any(|d| d.1 == p && d.2 == value)
                || self.resolved.iter().any(|d| d.1 == p && d.2 == value);
            assert!(
                accounted,
                "{value} written on {side:?} at {} is gone and unaccounted for\n{}",
                board.joined(p),
                self.report(board)
            );
        }
        // NeverDiscards.
        if self.mode == SyncMode::TwoWaySafe {
            assert!(
                self.discarded.is_empty(),
                "conflict mode discarded {:?}\n{}",
                self.discarded,
                self.report(board)
            );
        }
        // AlphaKeeps.
        for d in &self.discarded {
            assert!(
                d.0 != Side::Alpha,
                "alpha lost {} at {} to a beta\n{}",
                d.2,
                board.joined(d.1),
                self.report(board)
            );
        }
    }

    fn report(&self, board: &Board) -> String {
        let show = |t: &Tree| -> String {
            (0..board.paths.len())
                .map(|p| format!("{}={}", board.joined(p), t[p].tla()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        format!(
            "mode {:?}\nsteps:\n  {}\nalpha     {}\nbetas     {:?}\nancestors {:?}\nconflicts {:?}",
            self.mode,
            self.steps.join("\n  "),
            show(&self.alpha),
            self.beta.iter().map(|t| show(t)).collect::<Vec<_>>(),
            self.ancestor.iter().map(|t| show(t)).collect::<Vec<_>>(),
            self.conflicts
                .iter()
                .map(|c| c.iter().map(|&q| board.joined(q)).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        )
    }

    /// The state as a TLA+ record, appended to the trace.
    fn record(&mut self, board: &Board) {
        let tree = |t: &Tree| -> String {
            (0..board.paths.len())
                .map(|p| format!("{} :> {}", board.tla_path(p), t[p].tla()))
                .collect::<Vec<_>>()
                .join(" @@ ")
        };
        let per_beta = |f: &dyn Fn(usize) -> String| -> String {
            (0..board.betas)
                .map(|i| format!("b{} :> {}", i + 1, f(i)))
                .collect::<Vec<_>>()
                .join(" @@ ")
        };
        let conflicts = |i: usize| -> String {
            format!(
                "{{{}}}",
                self.conflicts[i]
                    .iter()
                    .map(|&q| board.tla_path(q))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        self.trace.push(format!(
            "[alpha |-> ({}), beta |-> ({}), ancestor |-> ({}), conflicts |-> ({})]",
            tree(&self.alpha),
            per_beta(&|i| format!("({})", tree(&self.beta[i]))),
            per_beta(&|i| format!("({})", tree(&self.ancestor[i]))),
            per_beta(&conflicts),
        ));
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

/// Plays one random game of `moves` user actions interleaved with cycles,
/// checking after every step, and ending with enough cycles for
/// everything to settle.
fn play(seed: u64, mode: SyncMode, board: &Board, moves: usize) -> Game {
    let mut rng = Rng(seed | 1);
    let mut game = Game::new(mode, board);
    let sides: Vec<Side> = std::iter::once(Side::Alpha)
        .chain((0..board.betas).map(Side::Beta))
        .collect();
    let mut stalls = 0;
    while game.edits < moves && stalls < 1000 {
        let side = sides[rng.below(sides.len())];
        let p = rng.below(board.paths.len());
        let acted = match rng.below(6) {
            0 | 1 => {
                let value = board.values[rng.below(board.values.len())];
                game.write(side, p, value, board)
            }
            2 => game.mkdir(side, p, board),
            3 => game.remove(side, p, board),
            _ => {
                let i = rng.below(board.betas);
                game.resolve(i, p, rng.below(2) == 0, board)
            }
        };
        if !acted {
            stalls += 1;
            continue;
        }
        game.record(board);
        game.check(board);
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
    // Converges: level everywhere but under reported conflicts, and in
    // the alpha modes, level everywhere.
    for i in 0..board.betas {
        for p in 0..board.paths.len() {
            let level = game.alpha[p] == game.beta[i][p];
            assert!(
                level || game.in_conflict(i, p, board),
                "pair {} did not converge at {}\n{}",
                i + 1,
                board.joined(p),
                game.report(board)
            );
            if mode != SyncMode::TwoWaySafe {
                assert!(
                    level,
                    "pair {} did not converge at {}\n{}",
                    i + 1,
                    board.joined(p),
                    game.report(board)
                );
            }
        }
    }
    game
}

const MODES: [SyncMode; 3] = [
    SyncMode::TwoWaySafe,
    SyncMode::TwoWayResolved,
    SyncMode::TwoWayStrict,
];

#[test]
fn random_runs_keep_the_invariants() {
    let board = Board::wide();
    for mode in MODES {
        for seed in 1..=1500u64 {
            play(seed * 7919, mode, &board, 10);
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
        .chain(board.values.iter().map(|v| v.to_string()))
        .collect();
    let paths: Vec<String> = (0..board.paths.len()).map(|p| board.tla_path(p)).collect();
    let module = [
        format!("---- MODULE {name} ----"),
        "EXTENDS Autobahn, Sequences, TLC".to_string(),
        format!("CONSTANTS {}", symbols.join(", ")),
        format!("TPaths == {{{}}}", paths.join(", ")),
        format!("Trace == <<\n  {}\n>>", game.trace.join(",\n  ")),
        "VARIABLE i".to_string(),
        // The logged trees and conflicts must be what the spec's own step
        // produces; the bookkeeping variables are the spec's to choose.
        "Match == /\\ alpha' = Trace[i + 1].alpha /\\ beta' = Trace[i + 1].beta".to_string(),
        "         /\\ ancestor' = Trace[i + 1].ancestor /\\ conflicts' = Trace[i + 1].conflicts"
            .to_string(),
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
        "SPECIFICATION TSpec\nCONSTANTS\n{}    Betas = {{{}}}\n    Paths <- TPaths\n    Values = {{{}}}\n    \
         NoFile = NoFile\n    Dir = Dir\n    Mode = \"{mode_name}\"\n    MaxEdits = {}\nCHECK_DEADLOCK TRUE\n",
        symbols.iter().map(|s| format!("    {s} = {s}\n")).collect::<String>(),
        (1..=board.betas).map(|i| format!("b{i}")).collect::<Vec<_>>().join(", "),
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
    let board = Board::small();
    let per_mode: usize = std::env::var("AUTOBAHN_TLC_TRACES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(8);
    let mut index = 0;
    for mode in MODES {
        for seed in 1..=per_mode as u64 {
            let game = play(seed * 104_729, mode, &board, 6);
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
    assert!(
        status.success(),
        "TLC rejected a trace; see the logs in {}",
        path.display()
    );
}
