//! The implementation's fence, held to `spec/Peering.tla`.
//!
//! The peering spec's safety rests on one decision — whether a host admits
//! the lease a controller presents — and on the order betas take over in.
//! This file plays the spec's leadership game with the real decision: every
//! host is a directory with a real lease file, presented leases go through
//! `read_lease` / `Lease::admits` / `write_lease` exactly as the agent's
//! `Request::Lease` handler does, staleness is the real `is_stale_at` on a
//! simulated clock, and the reconciler makes every cycle's move. The same
//! invariants the spec checks are asserted in Rust over random games, and
//! the games are written out as traces that TLC validates against the spec
//! (`#[ignore]`d: run with `AUTOBAHN_TLC=1` and `-- --ignored`).
//!
//! Not replayed: the follower/leader loop in `supervisor/peer.rs`, which is
//! sleeps and ssh around these decisions, and the ancestor copy's files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use autobahn::endpoint::{achieved_changes, TransitionOutcome};
use autobahn::peering::{read_lease, write_lease, Lease};
use autobahn::tree::{apply, reconcile, Change, Content, Digest, FileMetadata, Node, SyncMode};

const PATHS: [&str; 2] = ["p", "q"];
const VALUES: [&str; 2] = ["v1", "v2"];
const TTL: Duration = Duration::from_secs(30);

/// A host of the star: the alpha, or beta number i (0-based).
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
enum Host {
    Alpha,
    Beta(usize),
}

impl Host {
    fn name(self) -> String {
        match self {
            Host::Alpha => "alpha".into(),
            Host::Beta(i) => format!("b{}", i + 1),
        }
    }
    fn tla(self) -> String {
        match self {
            Host::Alpha => "\"alpha\"".into(),
            Host::Beta(i) => format!("b{}", i + 1),
        }
    }
}

type Tree = BTreeMap<&'static str, &'static str>;

/// The game's state: the spec's variables, with every host's lease on disk.
struct Game {
    mode: SyncMode,
    betas: usize,
    dirs: Vec<PathBuf>,
    tree: Vec<Tree>,
    up: Vec<bool>,
    leading: Vec<bool>,
    myterm: Vec<u64>,
    /// (alpha, b) session ancestors: what each host holds as leader (`own`)
    /// and as a replica (`copy`), each with a generation.
    own: Vec<Vec<(Tree, u64)>>,
    copy: Vec<Vec<(Tree, u64)>>,
    /// A leading beta's ancestors with the other betas.
    bb: Vec<Vec<Tree>>,
    conflicts: Vec<Vec<BTreeSet<&'static str>>>,
    writers: Vec<BTreeSet<(String, u64)>>,
    written: BTreeSet<(Host, &'static str, &'static str)>,
    superseded: BTreeSet<(&'static str, &'static str)>,
    discarded: BTreeSet<(Host, &'static str, &'static str)>,
    edits: usize,
    failures: usize,
    changes: usize,
    clock: u64,
    trace: Vec<String>,
    steps: Vec<String>,
}

fn digest_of(value: &str) -> Digest {
    *blake3::hash(value.as_bytes()).as_bytes()
}

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

fn from_node(node: Option<&Node>) -> Tree {
    let mut tree = Tree::new();
    if let Some(root) = node {
        for child in root.children() {
            if let Content::File { digest, .. } = &child.content {
                let value = VALUES
                    .iter()
                    .find(|v| digest_of(v) == *digest)
                    .expect("known digest");
                let path = PATHS
                    .iter()
                    .find(|p| **p == child.name)
                    .expect("known path");
                tree.insert(path, value);
            }
        }
    }
    tree
}

impl Game {
    fn new(mode: SyncMode, betas: usize, keep: &tempfile::TempDir) -> Game {
        let hosts = betas + 1;
        let dirs: Vec<PathBuf> = (0..hosts)
            .map(|h| {
                let dir = keep.path().join(format!("host{h}"));
                std::fs::create_dir_all(&dir).unwrap();
                dir
            })
            .collect();
        let mut game = Game {
            mode,
            betas,
            dirs,
            tree: vec![Tree::new(); hosts],
            up: vec![true; hosts],
            leading: (0..hosts).map(|h| h == 0).collect(),
            myterm: (0..hosts).map(|h| if h == 0 { 1 } else { 0 }).collect(),
            own: vec![vec![(Tree::new(), 0); betas]; hosts],
            copy: vec![vec![(Tree::new(), 0); betas]; hosts],
            bb: vec![vec![Tree::new(); betas]; betas],
            conflicts: vec![vec![BTreeSet::new(); hosts]; hosts],
            writers: vec![BTreeSet::new(); hosts],
            written: BTreeSet::new(),
            superseded: BTreeSet::new(),
            discarded: BTreeSet::new(),
            edits: 0,
            failures: 0,
            changes: 0,
            clock: 1_000,
            trace: Vec::new(),
            steps: Vec::new(),
        };
        // Every host starts under the alpha's lease at term 1, as the
        // leader's first cycles leave it.
        for h in 0..hosts {
            game.put_lease(h, &game.lease_of(Host::Alpha, 1));
        }
        game.record();
        game
    }

    fn index(host: Host) -> usize {
        match host {
            Host::Alpha => 0,
            Host::Beta(i) => i + 1,
        }
    }
    fn host(index: usize) -> Host {
        if index == 0 {
            Host::Alpha
        } else {
            Host::Beta(index - 1)
        }
    }

    fn lease_of(&self, leader: Host, term: u64) -> Lease {
        Lease {
            leader: leader.name(),
            term,
            renewed_at: self.clock,
            ttl_seconds: TTL.as_secs(),
        }
    }
    fn put_lease(&self, h: usize, lease: &Lease) {
        write_lease(&self.dirs[h], lease).expect("lease written");
    }
    fn lease(&self, h: usize) -> Lease {
        read_lease(&self.dirs[h])
            .expect("lease read")
            .expect("every host holds a lease")
    }
    fn leader_of(&self, h: usize) -> Host {
        let lease = self.lease(h);
        if lease.leader == "alpha" {
            Host::Alpha
        } else {
            let i: usize = lease.leader[1..].parse().unwrap();
            Host::Beta(i - 1)
        }
    }

    /// The agent's `Request::Lease` handling, exactly: admit and write, or
    /// refuse with what is held.
    fn present(&mut self, h: usize, presented: &Lease) -> bool {
        let held = self.lease(h);
        if !held.admits(presented) {
            return false;
        }
        self.put_lease(h, presented);
        self.writers[h].insert((presented.leader.clone(), presented.term));
        true
    }

    // ---------------------------------------------------------------- users

    fn write(&mut self, host: Host, path: &'static str, value: &'static str) -> bool {
        let h = Self::index(host);
        if !self.up[h] || self.tree[h].get(path) == Some(&value) {
            return false;
        }
        if let Some(old) = self.tree[h].get(path) {
            self.superseded.insert((path, old));
        }
        self.tree[h].insert(path, value);
        self.written.insert((host, path, value));
        self.edits += 1;
        self.steps
            .push(format!("write {} {path} {value}", host.name()));
        true
    }

    fn remove(&mut self, host: Host, path: &'static str) -> bool {
        let h = Self::index(host);
        if !self.up[h] {
            return false;
        }
        let Some(old) = self.tree[h].remove(path) else {
            return false;
        };
        self.superseded.insert((path, old));
        self.edits += 1;
        self.steps.push(format!("remove {} {path}", host.name()));
        true
    }

    // ------------------------------------------------------------- failure

    fn crash(&mut self, h: usize) -> bool {
        if !self.up[h] {
            return false;
        }
        self.up[h] = false;
        self.failures += 1;
        self.steps.push(format!("crash {}", Self::host(h).name()));
        true
    }

    fn recover(&mut self, h: usize) -> bool {
        if self.up[h] {
            return false;
        }
        self.up[h] = true;
        let lease = self.lease(h);
        self.leading[h] = lease.leader == Self::host(h).name();
        self.myterm[h] = lease.term;
        self.steps.push(format!("recover {}", Self::host(h).name()));
        true
    }

    // ---------------------------------------------------------- leadership

    /// The spec's `Takeover`, with the real staleness test on a clock the
    /// game advances: a beta acts when the lease it holds is stale and
    /// every beta ahead of it in the failover order is down or is the
    /// leader it is replacing.
    fn takeover(&mut self, i: usize) -> bool {
        let h = Self::index(Host::Beta(i));
        if !self.up[h] || self.leading[h] {
            return false;
        }
        let held = self.lease(h);
        if !held.is_stale_at(self.clock) {
            return false;
        }
        let leader = self.leader_of(h);
        for e in 0..i {
            let eh = Self::index(Host::Beta(e));
            if self.up[eh] && Host::Beta(e) != leader {
                return false;
            }
        }
        let term = held.term + 1;
        self.myterm[h] = term;
        let lease = self.lease_of(Host::Beta(i), term);
        self.put_lease(h, &lease);
        self.leading[h] = true;
        // adopt_newer_copy: the copy stands in for what is held only if it
        // is newer.
        if self.copy[h][i].1 > self.own[h][i].1 {
            self.own[h][i] = self.copy[h][i].clone();
        }
        self.bb[i] = vec![Tree::new(); self.betas];
        self.changes += 1;
        self.steps.push(format!("takeover b{} term {term}", i + 1));
        true
    }

    /// The spec's `Handoff`: the alpha, following a leading beta it is
    /// level with, takes the lead back at the next term.
    fn handoff(&mut self) -> bool {
        if !self.up[0] || self.leading[0] {
            return false;
        }
        let leader = self.leader_of(0);
        if !matches!(leader, Host::Beta(_)) {
            return false;
        }
        let lh = Self::index(leader);
        if !self.up[lh] || !self.leading[lh] || self.tree[0] != self.tree[lh] {
            return false;
        }
        let term = self.lease(0).term + 1;
        self.myterm[0] = term;
        let lease = self.lease_of(Host::Alpha, term);
        self.put_lease(0, &lease);
        self.leading[0] = true;
        for b in 0..self.betas {
            if self.copy[0][b].1 > self.own[0][b].1 {
                self.own[0][b] = self.copy[0][b].clone();
            }
        }
        self.changes += 1;
        self.steps.push(format!("handoff to alpha term {term}"));
        true
    }

    /// Time passes: leases held go stale unless renewed by a cycle.
    fn tick(&mut self) {
        self.clock += TTL.as_secs() + 1;
        self.steps.push("tick".into());
    }

    // ------------------------------------------------------------ sessions

    /// The spec's `Cycle(c, h)`: present the lease; if admitted, reconcile
    /// with the real reconciler, the alpha side being the configured alpha
    /// wherever it is involved.
    fn cycle(&mut self, c: usize, h: usize) -> bool {
        if !self.leading[c] || !self.up[c] || !self.up[h] || c == h {
            return false;
        }
        let presented = self.lease_of(Self::host(c), self.myterm[c]);
        if !self.present(h, &presented) {
            self.leading[c] = false;
            self.steps.push(format!(
                "cycle {}→{}: fenced, steps down",
                Self::host(c).name(),
                Self::host(h).name()
            ));
            return true;
        }
        let (alpha_side, beta_side) = if h == 0 { (0, c) } else { (c, h) };
        let ancestor = if c == 0 {
            self.own[0][h - 1].0.clone()
        } else if h == 0 {
            self.own[c][c - 1].0.clone()
        } else {
            self.bb[c - 1][h - 1].clone()
        };
        let a = to_node(&ancestor);
        let x = to_node(&self.tree[alpha_side]);
        let y = to_node(&self.tree[beta_side]);
        let r = reconcile(a.as_ref(), x.as_ref(), y.as_ref(), self.mode);
        let achieved = |t: &[Change]| {
            let outcome = TransitionOutcome {
                results: t.iter().map(|c| c.new.clone()).collect(),
                problems: Vec::new(),
                missing_staged_files: false,
                missing_staged: Vec::new(),
            };
            achieved_changes(t, &outcome).expect("one result per transition")
        };
        let x2 = from_node(
            apply(x.as_ref(), &r.alpha_transitions)
                .expect("alpha applies")
                .as_ref(),
        );
        let y2 = from_node(
            apply(y.as_ref(), &r.beta_transitions)
                .expect("beta applies")
                .as_ref(),
        );
        let mut anc_changes = r.ancestor_changes.clone();
        anc_changes.extend(achieved(&r.beta_transitions));
        anc_changes.extend(achieved(&r.alpha_transitions));
        let a2 = from_node(
            apply(a.as_ref(), &anc_changes)
                .expect("ancestor applies")
                .as_ref(),
        );

        let x_before = self.tree[alpha_side].clone();
        let y_before = self.tree[beta_side].clone();
        self.tree[alpha_side] = x2.clone();
        self.tree[beta_side] = y2.clone();
        let bump = |store: &mut (Tree, u64)| {
            if store.0 != a2 {
                *store = (a2.clone(), store.1 + 1);
            }
        };
        if c == 0 {
            bump(&mut self.own[0][h - 1]);
        } else if h == 0 {
            bump(&mut self.own[c][c - 1]);
        } else {
            self.bb[c - 1][h - 1] = a2.clone();
        }
        self.conflicts[c][h] = r
            .conflicts
            .iter()
            .map(|k| *PATHS.iter().find(|p| **p == k.root).expect("known root"))
            .collect();
        // Lost: a side's own change, gone from both sides of the pair.
        for &p in &PATHS {
            for (side, before, after, other) in [
                (Self::host(beta_side), &y_before, &y2, &x2),
                (Self::host(alpha_side), &x_before, &x2, &y2),
            ] {
                if let Some(v) = before.get(p) {
                    let changed = ancestor.get(p) != Some(v);
                    let gone = after.get(p) != Some(v) && other.get(p) != Some(v);
                    if changed && gone {
                        self.discarded.insert((side, p, v));
                    }
                }
            }
        }
        self.steps.push(format!(
            "cycle {}→{}",
            Self::host(c).name(),
            Self::host(h).name()
        ));
        true
    }

    /// The spec's `Replicate`: the leader of an (alpha, b) session pushes
    /// its ancestor to the other side.
    fn replicate(&mut self, c: usize, h: usize) -> bool {
        if !self.leading[c] || !self.up[c] || !self.up[h] || c == h || (c != 0 && h != 0) {
            return false;
        }
        if self.leader_of(h) != Self::host(c) {
            return false;
        }
        let b = if h == 0 { c - 1 } else { h - 1 };
        self.copy[h][b] = self.own[c][b].clone();
        self.steps.push(format!(
            "replicate {}→{}",
            Self::host(c).name(),
            Self::host(h).name()
        ));
        true
    }

    // --------------------------------------------------------- properties

    fn present_anywhere(&self, p: &str, v: &str) -> bool {
        self.tree.iter().any(|t| t.get(p) == Some(&v))
    }

    fn check(&self, context: &str) {
        // Fenced: one writer per host per term.
        for (h, writers) in self.writers.iter().enumerate() {
            let mut by_term: BTreeMap<u64, &str> = BTreeMap::new();
            for (leader, term) in writers {
                if let Some(other) = by_term.insert(*term, leader) {
                    assert_eq!(
                        other,
                        leader,
                        "{context}: host {} written by two controllers at term {term}\n{}",
                        Self::host(h).name(),
                        self.report()
                    );
                }
            }
        }
        // Accounted.
        for &(host, p, v) in &self.written {
            let ok = self.present_anywhere(p, v)
                || self.superseded.contains(&(p, v))
                || self.discarded.iter().any(|d| d.1 == p && d.2 == v);
            assert!(
                ok,
                "{context}: {v} written on {} at {p} is gone and unaccounted for\n{}",
                host.name(),
                self.report()
            );
        }
        if self.mode == SyncMode::TwoWaySafe {
            assert!(
                self.discarded.is_empty(),
                "{context}: conflict mode discarded {:?}\n{}",
                self.discarded,
                self.report()
            );
        }
        for d in &self.discarded {
            assert!(
                d.0 != Host::Alpha,
                "{context}: alpha lost {} at {}\n{}",
                d.2,
                d.1,
                self.report()
            );
        }
    }

    fn report(&self) -> String {
        format!(
            "mode {:?}\nsteps:\n  {}\ntrees {:?}\nup {:?}\nleading {:?}\nleases {:?}",
            self.mode,
            self.steps.join("\n  "),
            self.tree,
            self.up,
            self.leading,
            (0..self.tree.len())
                .map(|h| {
                    let l = self.lease(h);
                    format!("{}@{}", l.leader, l.term)
                })
                .collect::<Vec<_>>()
        )
    }

    /// The state as a TLA+ record: the trees, who is up, every host's
    /// lease and every controller's role. The spec fills in the rest.
    fn record(&mut self) {
        let hosts: Vec<Host> = (0..self.tree.len()).map(Self::host).collect();
        let tree = |t: &Tree| -> String {
            PATHS
                .iter()
                .map(|p| format!("<<\"{p}\">> :> {}", t.get(p).copied().unwrap_or("NoFile")))
                .collect::<Vec<_>>()
                .join(" @@ ")
        };
        let per_host = |f: &dyn Fn(usize) -> String| -> String {
            hosts
                .iter()
                .enumerate()
                .map(|(h, host)| format!("{} :> {}", host.tla(), f(h)))
                .collect::<Vec<_>>()
                .join(" @@ ")
        };
        let leases: Vec<Lease> = (0..self.tree.len()).map(|h| self.lease(h)).collect();
        let lease_tla = |l: &Lease| -> String {
            let leader = if l.leader == "alpha" {
                "\"alpha\"".to_string()
            } else {
                l.leader.clone()
            };
            format!("[leader |-> {leader}, term |-> {}]", l.term)
        };
        self.trace.push(format!(
            "[tree |-> ({}), up |-> ({}), lease |-> ({}), role |-> ({})]",
            per_host(&|h| format!("({})", tree(&self.tree[h]))),
            per_host(&|h| if self.up[h] {
                "TRUE".into()
            } else {
                "FALSE".into()
            }),
            per_host(&|h| lease_tla(&leases[h])),
            per_host(&|h| if self.leading[h] {
                "\"leading\"".into()
            } else {
                "\"following\"".into()
            }),
        ));
    }
}

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

/// Plays one random game: user edits, crashes and recoveries, ticks of
/// the clock that let leases go stale, takeovers and handoffs, cycles and
/// replication, all within the spec's budgets; checked after every step.
fn play(
    seed: u64,
    mode: SyncMode,
    betas: usize,
    budgets: (usize, usize, usize),
    steps: usize,
) -> Game {
    let keep = tempfile::tempdir().unwrap();
    let mut rng = Rng(seed | 1);
    let mut game = Game::new(mode, betas, &keep);
    let hosts = betas + 1;
    let (max_edits, max_failures, max_changes) = budgets;
    for _ in 0..steps {
        let acted = match rng.below(12) {
            0 | 1 if game.edits < max_edits => {
                let host = Game::host(rng.below(hosts));
                game.write(host, PATHS[rng.below(2)], VALUES[rng.below(2)])
            }
            2 if game.edits < max_edits => {
                game.remove(Game::host(rng.below(hosts)), PATHS[rng.below(2)])
            }
            3 if game.failures < max_failures => game.crash(rng.below(hosts)),
            4 => game.recover(rng.below(hosts)),
            5 => {
                // Time is not a step of the spec — staleness is a
                // nondeterministic judgement there — so a tick is not
                // recorded, and only what it enables is.
                game.tick();
                false
            }
            6 if game.changes < max_changes => game.takeover(rng.below(betas)),
            7 if game.changes < max_changes => game.handoff(),
            8..=10 => game.cycle(rng.below(hosts), rng.below(hosts)),
            11 => game.replicate(rng.below(hosts), rng.below(hosts)),
            _ => false,
        };
        if acted {
            game.record();
            game.check(&format!("seed {seed}"));
        }
    }
    keep.close().unwrap();
    game
}

#[test]
fn random_games_keep_the_fence_and_the_accounting() {
    for mode in [SyncMode::TwoWaySafe, SyncMode::TwoWayResolved] {
        for seed in 1..=800u64 {
            play(seed * 7919, mode, 3, (6, 3, 4), 60);
        }
    }
}

/// The real failover order: a beta's wait grows with its position, so the
/// first live beta acts first — the ordering the spec's `Takeover` assumes.
#[test]
fn takeover_waits_grow_with_position() {
    let timing = autobahn::config::PeeringPlan {
        ttl: Duration::from_secs(30),
        failover_after: Duration::from_secs(120),
    };
    let waits: Vec<Duration> = (1..=4)
        .map(|position| autobahn::peering::takeover_wait(position, &timing))
        .collect();
    assert!(waits.windows(2).all(|w| w[0] < w[1]), "{waits:?}");
    assert_eq!(waits[0], Duration::from_secs(120));
    assert_eq!(waits[1], Duration::from_secs(150));
}

fn write_trace(dir: &std::path::Path, index: usize, game: &Game, mode: SyncMode) {
    let name = format!("Trace{index}");
    let mode_name = match mode {
        SyncMode::TwoWaySafe => "conflict",
        SyncMode::TwoWayResolved => "alpha",
        _ => unreachable!(),
    };
    let betas: Vec<String> = (1..=game.betas).map(|i| format!("b{i}")).collect();
    let module = [
        format!("---- MODULE {name} ----"),
        "EXTENDS Peering, Sequences, TLC".to_string(),
        format!("CONSTANTS {}, v1, v2", betas.join(", ")),
        format!(
            "TPaths == {{{}}}",
            PATHS
                .iter()
                .map(|p| format!("<<\"{p}\">>"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        format!("TOrder == <<{}>>", betas.join(", ")),
        format!("Trace == <<\n  {}\n>>", game.trace.join(",\n  ")),
        "VARIABLE i".to_string(),
        "Match == /\\ tree' = Trace[i + 1].tree /\\ up' = Trace[i + 1].up".to_string(),
        "         /\\ lease' = Trace[i + 1].lease /\\ role' = Trace[i + 1].role".to_string(),
        "TInit == Init /\\ i = 1".to_string(),
        "TNext == \\/ i < Len(Trace) /\\ Next /\\ Match /\\ i' = i + 1".to_string(),
        "         \\/ i = Len(Trace) /\\ UNCHANGED <<vars, i>>".to_string(),
        "TSpec == TInit /\\ [][TNext]_<<vars, i>>".to_string(),
        "====".to_string(),
    ]
    .join("\n");
    std::fs::write(dir.join(format!("{name}.tla")), module).unwrap();
    let cfg = format!(
        "SPECIFICATION TSpec\nCONSTANTS\n{}    v1 = v1\n    v2 = v2\n    Betas = {{{}}}\n    Order <- TOrder\n    Paths <- TPaths\n    \
         Values = {{v1, v2}}\n    NoFile = NoFile\n    Dir = Dir\n    Mode = \"{mode_name}\"\n    MaxEdits = {}\n    MaxFailures = {}\n    \
         MaxChanges = {}\n    Flaky = TRUE\nCHECK_DEADLOCK TRUE\n",
        betas.iter().map(|b| format!("    {b} = {b}\n")).collect::<String>(),
        betas.join(", "),
        game.edits,
        game.failures,
        game.changes
    );
    std::fs::write(dir.join(format!("{name}.cfg")), cfg).unwrap();
}

#[test]
#[ignore = "needs TLC: set AUTOBAHN_TLC=1 and run with --ignored"]
fn traces_are_behaviors_of_the_peering_spec() {
    // Ignored, so that a run without TLC lists it as ignored rather than
    // passed; asked for explicitly without TLC, it fails for the same
    // reason.
    assert!(
        std::env::var("AUTOBAHN_TLC").is_ok(),
        "needs TLC: set AUTOBAHN_TLC=1 to validate traces with TLC"
    );
    let dir = tempfile::tempdir().unwrap();
    let per_mode: usize = std::env::var("AUTOBAHN_TLC_TRACES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(4);
    let mut index = 0;
    for mode in [SyncMode::TwoWaySafe, SyncMode::TwoWayResolved] {
        for seed in 1..=per_mode as u64 {
            let game = play(seed * 104_729, mode, 2, (2, 1, 2), 24);
            write_trace(dir.path(), index, &game, mode);
            index += 1;
        }
    }
    let check = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/check.sh");
    let status = std::process::Command::new(check)
        .arg("--traces")
        .arg(dir.path())
        .status()
        .expect("spec/check.sh runs");
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
