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

/// The star as a follower sees it: the plans it would run as leader, and
/// where it stands in the order of succession.
#[derive(Debug)]
pub struct FollowerStar {
    /// This host's own spec, as the leader named it.
    pub name: String,
    /// The plans this host runs when it leads: itself as the alpha, every
    /// other beta as a beta. The configured alpha is not among them — it
    /// is never dialed; it dials, and attaches (a later phase).
    pub plans: Vec<crate::config::SessionPlan>,
    /// The position in the order of succession: the alpha is 0, the
    /// first beta 1, and so on. A candidate at position *n* waits *n − 1*
    /// extra lease lifetimes before it acts, so the first live beta acts
    /// first without anyone being asked.
    pub position: usize,
    /// The heartbeat interval, from the plans.
    pub interval: Duration,
    /// The peering timing, from the pushed configuration.
    pub timing: crate::config::PeeringPlan,
}

/// The files a follower runs from, read from its peering directory.
pub fn pushed_configuration(directory: &Path) -> Result<Option<(String, String)>> {
    let Some(name) = read_pushed_file(directory, "name")? else {
        return Ok(None);
    };
    let Some(config) = read_pushed_file(directory, "config.toml")? else {
        return Ok(None);
    };
    Ok(Some((
        String::from_utf8(config).context("the pushed configuration is not UTF-8")?,
        String::from_utf8(name).context("the pushed name is not UTF-8")?,
    )))
}

/// Derives a follower's star from the leader's configuration and the name
/// the leader gave this host.
///
/// The pushed configuration is the leader's star: a local alpha and remote
/// betas, one of which is this host. Turned around, this host is the
/// alpha — its own root, as a local path — and the other betas stay as
/// they were. Groups not in a peering mode are the alpha's business and
/// are dropped.
pub fn derive_star(configuration: &str, name: &str, directory: &Path) -> Result<FollowerStar> {
    let mut config: crate::config::Config =
        toml::from_str(configuration).context("unable to parse the pushed configuration")?;
    config.ignore_directory = Some(directory.join(crate::scan::ignorefile::DIRECTORY));
    let timing = config.peering_plan()?;

    // The name is matched against each beta entry as the leader would
    // have spelled it in full: an entry without a path inherits the
    // alpha's, which is how the leader's plans named this host.
    let full = |entry: &str, alpha: &str| -> String {
        let host_end = entry.find(':').unwrap_or(entry.len());
        if host_end < entry.len() {
            entry.to_owned()
        } else {
            format!("{entry}:{alpha}")
        }
    };
    let mut position: Option<usize> = None;
    let mut groups = std::collections::BTreeMap::new();
    for (group_name, group) in &config.groups {
        let mode = group.mode.as_deref().or(config.defaults.mode.as_deref());
        let peering = match mode {
            Some(mode) => crate::config::parse_mode_spec(mode)
                .map(|(_, peering)| peering)
                .unwrap_or(false),
            None => false,
        };
        if !peering {
            continue;
        }
        let Some(index) = group
            .betas
            .iter()
            .position(|entry| full(entry, &group.alpha) == name)
        else {
            continue;
        };
        let own_path = name
            .rsplit_once(':')
            .map(|(_, path)| path.to_owned())
            .unwrap_or_else(|| name.to_owned());
        let mut turned = crate::config::Group {
            alpha: own_path,
            betas: group
                .betas
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != index)
                .map(|(_, entry)| full(entry, &group.alpha))
                .collect(),
            ..group.clone()
        };
        // The mode spelling is kept as written, so the plans say
        // "peering" and carry the timing.
        turned.mode = Some(mode.unwrap_or_default().to_owned());
        groups.insert(group_name.clone(), turned);
        position = Some(match position {
            Some(existing) => existing.min(index + 1),
            None => index + 1,
        });
    }
    let Some(position) = position else {
        bail!("{name:?} is not a beta of any peering group in the pushed configuration");
    };
    config.groups = groups;
    let plans = config
        .plans()
        .context("unable to plan the follower's star")?;
    let interval = plans
        .iter()
        .map(|plan| plan.interval)
        .min()
        .unwrap_or(Duration::from_secs(5));
    Ok(FollowerStar {
        name: name.to_owned(),
        plans,
        position,
        interval,
        timing,
    })
}

/// How long a candidate at `position` waits past a stale lease before it
/// acts: the configured wait, plus one lease lifetime for every member
/// ahead of it in the order of succession other than the alpha.
pub fn takeover_wait(position: usize, timing: &crate::config::PeeringPlan) -> Duration {
    let ahead = position.saturating_sub(1) as u32;
    timing.failover_after + timing.ttl.saturating_mul(ahead)
}

#[cfg(test)]
mod star_tests {
    use super::*;

    const PUSHED: &str = r#"
        [advanced.peering-experimental]
        ttl = "10s"
        failover_after = "20s"

        [defaults]
        mode = "two-way-conflict"

        [groups.plain]
        alpha = "/tmp/plain"
        betas = ["x@h:/tmp/plain"]

        [groups.g]
        mode = "peering-conflict-experimental"
        alpha = "/home/faraz/Workspace/Voltai"
        betas = ["ubuntu@vm", "box2:/srv/ws"]
        ignores = ["target"]
    "#;

    #[test]
    fn a_follower_turns_the_star_around() {
        let keep = tempfile::tempdir().expect("tempdir");
        let star = derive_star(
            PUSHED,
            "ubuntu@vm:/home/faraz/Workspace/Voltai",
            keep.path(),
        )
        .expect("a star");
        assert_eq!(star.position, 1);
        assert_eq!(
            star.plans.len(),
            1,
            "the plain group is dropped: {:?}",
            star.plans
        );
        let plan = &star.plans[0];
        assert_eq!(plan.group, "g");
        assert_eq!(plan.beta_spec(), "box2:/srv/ws");
        assert!(
            matches!(&plan.alpha, crate::config::EndpointTarget::Local(path)
            if path == std::path::Path::new("/home/faraz/Workspace/Voltai"))
        );
        assert!(plan.peering.is_some());
        assert!(plan.ignores.iter().any(|p| p == "target"));
        assert_eq!(star.timing.ttl, Duration::from_secs(10));
        assert_eq!(
            takeover_wait(star.position, &star.timing),
            Duration::from_secs(20)
        );

        let second = derive_star(PUSHED, "box2:/srv/ws", keep.path()).expect("a star");
        assert_eq!(second.position, 2);
        assert_eq!(
            second.plans[0].beta_spec(),
            "ubuntu@vm:/home/faraz/Workspace/Voltai"
        );
        assert_eq!(
            takeover_wait(second.position, &second.timing),
            Duration::from_secs(30)
        );

        assert!(derive_star(PUSHED, "nobody:/x", keep.path()).is_err());
    }
}
