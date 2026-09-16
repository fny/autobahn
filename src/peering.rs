//! Peering: the state a host keeps so that a beta can take the lead.
//!
//! Experimental. A group in a peering mode has one leader — the alpha,
//! until it is away long enough — and every other member follows. What
//! makes a follower able to lead later is state the leader pushes to it
//! on every cycle, all of it under `~/.autobahn/peering/`:
//!
//! - `lease.json` — who leads, at what term, and until when. The agent
//!   refuses writes from a controller whose term is below the lease's.
//!   That refusal is the whole safety argument: no root is ever written
//!   by two controllers, whatever the network does.
//! - `name` — this host's own spec in the star, so a follower running the
//!   pushed configuration can find itself in it.
//! - `config.toml`, `ignores/` — the leader's configuration, so a
//!   follower knows the group.
//! - `ancestors/<session>/` — a copy of the leader's ancestor for each
//!   session this host is a side of, kept with the ancestor's own
//!   durability. A leader without an ancestor reconciles two trees with
//!   no history and calls every difference a conflict; this is what
//!   spares the new leader that.
//!
//! The types here are shared by the controller and the agent; the store
//! is the agent's. Nothing in reconciliation knows about any of it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::session::ancestor::AncestorStore;
use crate::tree::{Change, Node};

/// The directory under the state root.
pub const DIRECTORY: &str = "peering";

/// The lease file's name.
const LEASE_FILE: &str = "lease.json";

/// The leader's claim on a host, renewed on every cycle.
///
/// A lease is compared by term first. A higher term is a newer leadership
/// and wins outright; an equal term must come from the same leader — two
/// controllers at one term is a split, and the second one is refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Who leads: `"alpha"` for the configured alpha, otherwise the
    /// leading beta's spec as the configuration writes it.
    pub leader: String,
    /// The leadership's term. Every change of leader increases it.
    pub term: u64,
    /// When the leader last renewed, as seconds since the Unix epoch on
    /// the *leader's* clock. Staleness is judged against the follower's
    /// clock, so a skew of minutes matters; a skew of seconds does not.
    pub renewed_at: u64,
    /// How long past `renewed_at` the lease stays valid, in seconds.
    pub ttl_seconds: u64,
}

impl Lease {
    /// A fresh lease from `leader` at `term`, renewed now.
    pub fn new(leader: &str, term: u64, ttl: Duration) -> Lease {
        Lease {
            leader: leader.to_owned(),
            term,
            renewed_at: now_seconds(),
            ttl_seconds: ttl.as_secs(),
        }
    }

    /// Whether the lease has run out, judged at `now` (seconds since the
    /// epoch on the judging host).
    pub fn is_stale_at(&self, now: u64) -> bool {
        now > self.renewed_at.saturating_add(self.ttl_seconds)
    }

    /// How long the lease has been stale at `now`; zero while it is good.
    pub fn stale_for_at(&self, now: u64) -> Duration {
        Duration::from_secs(now.saturating_sub(self.renewed_at.saturating_add(self.ttl_seconds)))
    }

    /// Whether `incoming` may replace this lease: a higher term always, an
    /// equal term only from the same leader.
    pub fn admits(&self, incoming: &Lease) -> bool {
        incoming.term > self.term || (incoming.term == self.term && incoming.leader == self.leader)
    }
}

/// The agent's answer to a presented lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseAnswer {
    /// The lease was recorded; the channel may write.
    Accepted,
    /// The host holds a newer lease; the channel may not write until it
    /// presents a term at least as high as this one.
    Refused { current: Lease },
}

/// What a host holds, as reported to a controller that asks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// The lease on this host, if any was ever written.
    pub lease: Option<Lease>,
    /// The generation of this host's ancestor copy for the asking session,
    /// if it holds one.
    pub generation: Option<u64>,
}

/// The leader name the configured alpha writes into its leases. A beta
/// that leads writes its own spec instead.
pub const ALPHA: &str = "alpha";

/// What a supervisor is, in a peering group. Shared by every worker of
/// the supervisor: a fence answered on one session steps the whole
/// supervisor down, since the lease is per host, not per session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    /// No plan is in a peering mode.
    Off,
    /// This supervisor leads at `term`, and renews leases as `leader`.
    Leader { leader: String, term: u64 },
    /// Another controller holds the lead; this supervisor does not write.
    Follower { leader: String, term: u64 },
}

impl Role {
    /// The word `status` shows.
    pub fn label(&self) -> &'static str {
        match self {
            Role::Off => "",
            Role::Leader { .. } => "leader",
            Role::Follower { .. } => "follower",
        }
    }

    /// The term, when there is one.
    pub fn term(&self) -> u64 {
        match self {
            Role::Off => 0,
            Role::Leader { term, .. } | Role::Follower { term, .. } => *term,
        }
    }
}

/// What a session presents to its beta on every cycle when its supervisor
/// leads: the claim, with the lease lifetime the plan carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leadership {
    /// Who leads, as the lease will say.
    pub leader: String,
    /// The term.
    pub term: u64,
    /// The lease lifetime.
    pub ttl: Duration,
}

impl Leadership {
    /// A lease renewed now.
    pub fn lease(&self) -> Lease {
        Lease::new(&self.leader, self.term, self.ttl)
    }
}

/// The error a cycle ends with when the host refused its lease: another
/// controller leads there. The worker steps the supervisor down on it.
#[derive(Debug, thiserror::Error)]
#[error(
    "fenced: {} at term {} holds the lease on the beta; this supervisor stepped down",
    current.leader,
    current.term
)]
pub struct Fenced {
    /// The lease the host holds.
    pub current: Lease,
}

/// The term a supervisor that is the configured alpha resumes at: the
/// one in its own lease file when that names the alpha, and otherwise a
/// fresh first term. A lease naming another leader means a beta led
/// while this machine was away, and this machine must not lead until
/// that is settled — the caller decides what to do with that.
pub fn alpha_term(directory: &Path) -> Result<AlphaStart> {
    match read_lease(directory)? {
        None => Ok(AlphaStart::Lead { term: 1 }),
        Some(lease) if lease.leader == ALPHA => Ok(AlphaStart::Lead { term: lease.term }),
        Some(lease) => Ok(AlphaStart::Follow { lease }),
    }
}

/// How the alpha starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlphaStart {
    /// Lead, at this term.
    Lead { term: u64 },
    /// Another member led while the alpha was away; follow it.
    Follow { lease: Lease },
}

/// Seconds since the Unix epoch, on this host's clock.
pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// The peering directory under the default state root: `$AUTOBAHN_HOME`
/// or `~/.autobahn`, the same place a follower's `watch` will look.
pub fn directory() -> Result<PathBuf> {
    Ok(crate::paths::default_state_root()?.join(DIRECTORY))
}

/// Reads the lease a host holds, `None` when none was ever written.
pub fn read_lease(directory: &Path) -> Result<Option<Lease>> {
    let path = directory.join(LEASE_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("unable to read the lease at {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("unable to read {}", path.display())),
    }
}

/// Writes a lease, atomically: a controller that reads it sees the old
/// lease or the new one, never a torn file.
pub fn write_lease(directory: &Path, lease: &Lease) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(lease).context("unable to encode the lease")?;
    write_file(directory, LEASE_FILE, &bytes)
}

/// Writes one of the files the leader pushes: `config.toml`, `name`, or
/// `ignores/<file>`. The name is checked against the short list of what a
/// follower needs, so the request can never be used to write anywhere
/// else on the host.
pub fn write_pushed_file(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    if !is_pushable(name) {
        bail!("{name:?} is not a file peering pushes");
    }
    write_file(directory, name, bytes)
}

/// Whether a pushed file name is one of the few peering knows about.
pub fn is_pushable(name: &str) -> bool {
    match name {
        "config.toml" | "name" => true,
        other => match other.strip_prefix("ignores/") {
            Some(file) => !file.is_empty() && !file.contains('/') && file != "." && file != "..",
            None => false,
        },
    }
}

/// Writes a file under the peering directory by way of a temporary and a
/// rename. The directory (and `ignores/`) is created on demand.
fn write_file(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let path = directory.join(name);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("unable to create {}", parent.display()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&temporary, bytes)
        .with_context(|| format!("unable to write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .with_context(|| format!("unable to move {} into place", path.display()))?;
    Ok(())
}

/// Reads a pushed file, `None` when the leader never pushed it.
pub fn read_pushed_file(directory: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let path = directory.join(name);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("unable to read {}", path.display())),
    }
}

/// Where a session's ancestor copy lives on a peer.
pub fn ancestor_copy_path(directory: &Path, session: &str) -> PathBuf {
    directory.join("ancestors").join(session).join("ancestor")
}

/// A host's copy of one session's ancestor: the leader's journal records,
/// replayed onto the same store type the leader uses, so the copy has the
/// leader's durability and can be opened as an ancestor by a leader later.
pub(crate) struct AncestorCopy {
    store: AncestorStore,
    ancestor: Option<Node>,
}

impl AncestorCopy {
    /// Opens (or starts) the copy for `session` under `directory`.
    pub(crate) fn open(directory: &Path, session: &str) -> Result<AncestorCopy> {
        let path = ancestor_copy_path(directory, session);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("unable to create {}", parent.display()))?;
        }
        let (store, ancestor, _unresolved) = AncestorStore::open(&path)
            .with_context(|| format!("unable to open the ancestor copy at {}", path.display()))?;
        Ok(AncestorCopy { store, ancestor })
    }

    /// The generation the copy stands at; zero for a copy that holds
    /// nothing yet.
    pub(crate) fn generation(&self) -> u64 {
        self.store.generation()
    }

    /// Applies the leader's record for `generation`: the changes that took
    /// the leader's ancestor from `generation - 1` to `generation`. A copy
    /// at any other generation cannot apply it and says where it stands,
    /// so the leader can send a checkpoint instead.
    pub(crate) fn record(&mut self, generation: u64, changes: &[Change]) -> Result<u64> {
        if self.store.generation() + 1 != generation {
            return Ok(self.store.generation());
        }
        let next = crate::tree::apply(self.ancestor.as_ref(), changes)
            .map_err(|message| anyhow::anyhow!("unable to apply the ancestor record: {message}"))?;
        self.store.record(changes, next.as_ref())?;
        self.ancestor = next;
        Ok(self.store.generation())
    }

    /// Replaces the copy with the leader's whole ancestor at `generation`.
    pub(crate) fn checkpoint(&mut self, generation: u64, ancestor: Option<Node>) -> Result<u64> {
        self.store.checkpoint_at(generation, ancestor.as_ref())?;
        self.ancestor = ancestor;
        Ok(self.store.generation())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_admits_a_higher_term_or_the_same_leader() {
        let held = Lease::new("alpha", 7, Duration::from_secs(30));
        assert!(held.admits(&Lease::new("alpha", 7, Duration::from_secs(30))));
        assert!(held.admits(&Lease::new("u@h:/x", 8, Duration::from_secs(30))));
        assert!(!held.admits(&Lease::new("u@h:/x", 7, Duration::from_secs(30))));
        assert!(!held.admits(&Lease::new("alpha", 6, Duration::from_secs(30))));
    }

    #[test]
    fn staleness_is_judged_against_the_ttl() {
        let lease = Lease {
            leader: "alpha".into(),
            term: 1,
            renewed_at: 1_000,
            ttl_seconds: 30,
        };
        assert!(!lease.is_stale_at(1_030));
        assert!(lease.is_stale_at(1_031));
        assert_eq!(lease.stale_for_at(1_000), Duration::ZERO);
        assert_eq!(lease.stale_for_at(1_100), Duration::from_secs(70));
    }

    #[test]
    fn only_the_files_a_follower_needs_can_be_pushed() {
        assert!(is_pushable("config.toml"));
        assert!(is_pushable("name"));
        assert!(is_pushable("ignores/node"));
        assert!(!is_pushable("ignores/"));
        assert!(!is_pushable("ignores/../x"));
        assert!(!is_pushable("ignores/a/b"));
        assert!(!is_pushable("lease.json"));
        assert!(!is_pushable("../config.toml"));
        assert!(!is_pushable("/etc/passwd"));
    }

    #[test]
    fn the_lease_and_pushed_files_round_trip_on_disk() {
        let keep = tempfile::tempdir().expect("a temporary directory");
        let directory = keep.path().join(DIRECTORY);
        assert_eq!(read_lease(&directory).unwrap(), None);
        let lease = Lease::new("alpha", 3, Duration::from_secs(30));
        write_lease(&directory, &lease).unwrap();
        assert_eq!(read_lease(&directory).unwrap(), Some(lease));

        write_pushed_file(&directory, "ignores/node", b"node_modules\n").unwrap();
        assert_eq!(
            read_pushed_file(&directory, "ignores/node")
                .unwrap()
                .as_deref(),
            Some(&b"node_modules\n"[..])
        );
        assert!(write_pushed_file(&directory, "../escape", b"x").is_err());
        assert_eq!(read_pushed_file(&directory, "name").unwrap(), None);
    }

    #[test]
    fn an_ancestor_copy_follows_records_and_takes_checkpoints() {
        let keep = tempfile::tempdir().expect("a temporary directory");
        let directory = keep.path().join(DIRECTORY);
        let file = |name: &str| Node::directory(name, Vec::new());
        let mut copy = AncestorCopy::open(&directory, "s1").unwrap();
        assert_eq!(copy.generation(), 0);

        // The first record carries the whole tree, as a leader's does.
        let root = Node::directory("", vec![file("a")]);
        let creation = Change {
            path: String::new(),
            old: None,
            new: Some(root.clone()),
        };
        assert_eq!(copy.record(1, &[creation]).unwrap(), 1);

        // A record for a generation the copy is not at is not applied,
        // and the copy says where it stands.
        assert_eq!(copy.record(5, &[]).unwrap(), 1);

        // A checkpoint moves it anywhere.
        let bigger = Node::directory("", vec![file("a"), file("b")]);
        assert_eq!(copy.checkpoint(9, Some(bigger.clone())).unwrap(), 9);
        drop(copy);

        // And it all survives a reopen, as an ancestor a leader could use.
        let copy = AncestorCopy::open(&directory, "s1").unwrap();
        assert_eq!(copy.generation(), 9);
        assert_eq!(copy.ancestor.as_ref().map(|n| n.children().len()), Some(2));
    }
}
