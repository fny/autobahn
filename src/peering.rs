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

/// When this host received the lease it holds, on this host's clock:
/// `{"leader", "term", "received_at"}`. Kept beside the lease rather than
/// in it, because the lease crosses the wire and this does not. A write is
/// refused once the lease has gone unrenewed for its lifetime by this
/// clock, so no skew between the leader's clock and this one can let a
/// lapsed leader write, or stop a live one.
const RECEIPT_FILE: &str = "lease.received";

/// The lock every lease decision takes: exclusively to admit a lease,
/// shared for as long as a write checked against it runs. So a takeover
/// cannot land between a write's check and the write, and two leases
/// presented at once are decided one after the other.
const LOCK_FILE: &str = "lease.lock";

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

/// Writes a lease, atomically and durably: a controller that reads it sees
/// the old lease or the new one, never a torn file, and one it was told was
/// accepted survives a crash. Takes no lock and checks nothing; the agent
/// admits leases with [`admit_lease`].
pub fn write_lease(directory: &Path, lease: &Lease) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(lease).context("unable to encode the lease")?;
    write_file(directory, LEASE_FILE, &bytes)?;
    write_receipt(directory, lease)
}

/// The lease lock, held until dropped.
#[derive(Debug)]
pub struct LeaseLock {
    _file: std::fs::File,
}

/// Takes the lease lock of a peering directory, creating both on demand.
fn lock(directory: &Path, exclusive: bool) -> Result<LeaseLock> {
    use std::os::unix::io::AsRawFd;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("unable to create {}", directory.display()))?;
    let path = directory.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("unable to open {}", path.display()))?;
    let operation = match exclusive {
        true => libc::LOCK_EX,
        false => libc::LOCK_SH,
    };
    loop {
        // Safety: a valid descriptor, owned by `file` for the lock's life;
        // closing it releases the lock.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(LeaseLock { _file: file });
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).with_context(|| format!("unable to lock {}", path.display()));
        }
    }
}

/// The agent's answer to a presented lease, decided under the exclusive
/// lock: the read, the check and the write are one step, so two leases
/// presented at once — at one term, or a delayed lower term after a higher
/// one — are decided in turn, and exactly one of two rivals is admitted.
pub fn admit_lease(directory: &Path, lease: &Lease) -> Result<LeaseAnswer> {
    let _lock = lock(directory, true)?;
    match read_lease(directory)? {
        Some(held) if !held.admits(lease) => Ok(LeaseAnswer::Refused { current: held }),
        _ => {
            write_lease(directory, lease)?;
            Ok(LeaseAnswer::Accepted)
        }
    }
}

/// Renews a lease this host holds for itself, under the exclusive lock,
/// only while the host still holds it at that leader and term: a handoff or
/// a takeover that wrote a different lease is never renewed over. Whether
/// it renewed.
pub fn renew_own_lease(directory: &Path, lease: &Lease) -> Result<bool> {
    let _lock = lock(directory, true)?;
    let held = read_lease(directory)?;
    if !held.is_some_and(|held| held.leader == lease.leader && held.term == lease.term) {
        return Ok(false);
    }
    write_lease(directory, lease)?;
    Ok(true)
}

/// Why a write checked against the lease was refused.
#[derive(Debug, thiserror::Error)]
pub enum WriteRefused {
    /// Another leadership took the host since this channel's lease.
    #[error(
        "fenced: this host's lease is held by {} at term {}; the lease this channel was \
         accepted at ({} at term {}) no longer holds",
        current.leader, current.term, accepted.leader, accepted.term
    )]
    Superseded { current: Lease, accepted: Lease },
    /// The lease went unrenewed for its lifetime, by this host's clock.
    #[error(
        "fenced: the lease of {} at term {} lapsed {}s ago on this host without renewal; \
         present it again to write",
        accepted.leader, accepted.term, lapsed.as_secs()
    )]
    Lapsed { accepted: Lease, lapsed: Duration },
}

/// Checks a write against the lease its channel was accepted at, under the
/// shared lease lock, which the returned guard holds until the write is
/// done: no lease can be admitted while it runs. Refused when the host's
/// lease has moved on to another leader or term, or when it went unrenewed
/// for its lifetime by this host's clock.
pub fn check_write(directory: &Path, accepted: &Lease) -> Result<LeaseLock> {
    let guard = lock(directory, false)?;
    let held = read_lease(directory)?;
    let current = match held {
        Some(held) if held.leader == accepted.leader && held.term == accepted.term => held,
        Some(held) => {
            return Err(WriteRefused::Superseded {
                current: held,
                accepted: accepted.clone(),
            }
            .into())
        }
        None => bail!(
            "the lease this channel was accepted at is gone from {}",
            directory.display()
        ),
    };
    if let Some(receipt) = read_receipt(directory)? {
        if receipt.leader == current.leader && receipt.term == current.term {
            let lapsed = now_seconds()
                .saturating_sub(receipt.received_at.saturating_add(current.ttl_seconds));
            if lapsed > 0 {
                return Err(WriteRefused::Lapsed {
                    accepted: accepted.clone(),
                    lapsed: Duration::from_secs(lapsed),
                }
                .into());
            }
        }
    }
    Ok(guard)
}

/// When this host received its lease, by its own clock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Receipt {
    leader: String,
    term: u64,
    received_at: u64,
}

fn write_receipt(directory: &Path, lease: &Lease) -> Result<()> {
    let receipt = Receipt {
        leader: lease.leader.clone(),
        term: lease.term,
        received_at: now_seconds(),
    };
    let bytes = serde_json::to_vec(&receipt).context("unable to encode the lease receipt")?;
    write_file(directory, RECEIPT_FILE, &bytes)
}

fn read_receipt(directory: &Path) -> Result<Option<Receipt>> {
    let path = directory.join(RECEIPT_FILE);
    match std::fs::read(&path) {
        // A receipt that does not parse is treated as absent: it only ever
        // adds a refusal, never an admission.
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("unable to read {}", path.display())),
    }
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
    let plain = |file: &str| !file.is_empty() && !file.contains('/') && file != "." && file != "..";
    match name {
        "config.toml" | "name" => true,
        other => ["ignores/", "sessions/", "names/"]
            .iter()
            .find_map(|prefix| other.strip_prefix(prefix))
            .is_some_and(plain),
    }
}

/// Writes a file under the peering directory by way of a temporary and a
/// rename, durably: the file and its directory are flushed before this
/// returns. The directory (and `ignores/`) is created on demand.
///
/// The temporary is created fresh (`create_new`) under a name no other
/// writer can hold. Named by process alone, two channels of one agent
/// pushing the same file wrote into one temporary, and either rename could
/// publish the other's bytes, or half of them.
fn write_file(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = directory.join(name);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("unable to create {}", parent.display()))?;
    let (temporary, mut file) = loop {
        let candidate = parent.join(format!(
            ".{}.{}.{}.{}.tmp",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.subsec_nanos())
                .unwrap_or(0),
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("unable to create {}", candidate.display()))
            }
        }
    };
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("unable to write {}", temporary.display()));
    drop(file);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temporary, &path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("unable to move {} into place", path.display()));
    }
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("unable to flush {}", parent.display()))?;
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
pub fn ancestor_copy_path(directory: &Path, session: &str) -> Result<PathBuf> {
    // Checked here as well as where the agent takes the session off the
    // wire, so that no caller can join anything but one hex name.
    anyhow::ensure!(
        crate::protocol::is_session_identifier(session),
        "refusing session identifier {session:?}"
    );
    Ok(directory.join("ancestors").join(session).join("ancestor"))
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
        let path = ancestor_copy_path(directory, session)?;
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

    /// The ancestor copy's path is built only from a genuine session
    /// identifier, whoever the caller: a traversal names another session's
    /// store, or somewhere outside the peering directory altogether.
    #[test]
    fn an_ancestor_copy_path_needs_a_genuine_session() {
        let directory = Path::new("/state/peering");
        for session in ["..", "../sessions/x", "a/b", "", "/tmp/x"] {
            let error =
                ancestor_copy_path(directory, session).expect_err("the session must be refused");
            assert!(
                format!("{error:#}").contains("refusing session identifier"),
                "{session:?}: {error:#}"
            );
        }
        let session = crate::session::session_identifier("a", "b");
        assert_eq!(
            ancestor_copy_path(directory, &session).expect("a genuine session"),
            directory.join("ancestors").join(&session).join("ancestor")
        );
    }

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
        assert!(is_pushable("names/work"));
        assert!(!is_pushable("names/"));
        assert!(!is_pushable("names/a/b"));
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

    /// Two leases presented at once at one term, by two leaders, are
    /// decided in turn under the lock: exactly one is admitted, and the
    /// host holds the one that was.
    #[test]
    fn two_admissions_at_one_term_accept_exactly_one() {
        let keep = tempfile::tempdir().expect("a temporary directory");
        let directory = keep.path().join(DIRECTORY);
        let ttl = Duration::from_secs(30);
        admit_lease(&directory, &Lease::new(ALPHA, 1, ttl)).unwrap();
        for round in 2..40 {
            let barrier = std::sync::Barrier::new(2);
            let answers: Vec<(String, LeaseAnswer)> = std::thread::scope(|scope| {
                let rivals = ["u@one:/x", "u@two:/x"].map(|leader| {
                    let (directory, barrier) = (&directory, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        let lease = Lease::new(leader, round, ttl);
                        (leader.to_owned(), admit_lease(directory, &lease).unwrap())
                    })
                });
                rivals.map(|rival| rival.join().unwrap()).into()
            });
            let admitted: Vec<&String> = answers
                .iter()
                .filter(|(_, answer)| *answer == LeaseAnswer::Accepted)
                .map(|(leader, _)| leader)
                .collect();
            assert_eq!(admitted.len(), 1, "round {round}: {answers:?}");
            let held = read_lease(&directory).unwrap().unwrap();
            assert_eq!((&held.leader, held.term), (admitted[0], round));
        }
    }

    /// A lease presented late at a lower term — a delayed renewal from a
    /// leader that has since been replaced — is refused, and the higher
    /// lease stays in place.
    #[test]
    fn a_delayed_lower_term_never_replaces_a_higher_one() {
        let keep = tempfile::tempdir().expect("a temporary directory");
        let directory = keep.path().join(DIRECTORY);
        let ttl = Duration::from_secs(30);
        let higher = Lease::new("u@h:/x", 6, ttl);
        assert_eq!(
            admit_lease(&directory, &higher).unwrap(),
            LeaseAnswer::Accepted
        );
        assert_eq!(
            admit_lease(&directory, &Lease::new(ALPHA, 5, ttl)).unwrap(),
            LeaseAnswer::Refused {
                current: higher.clone()
            }
        );
        assert_eq!(read_lease(&directory).unwrap(), Some(higher));
    }

    /// A leader renews only its own lease: once a handoff or a takeover
    /// wrote another, the renewal leaves it be.
    #[test]
    fn a_leader_renews_only_the_lease_it_holds() {
        let keep = tempfile::tempdir().expect("a temporary directory");
        let directory = keep.path().join(DIRECTORY);
        let ttl = Duration::from_secs(30);
        assert!(!renew_own_lease(&directory, &Lease::new("u@h:/x", 3, ttl)).unwrap());
        assert_eq!(read_lease(&directory).unwrap(), None);
        admit_lease(&directory, &Lease::new("u@h:/x", 3, ttl)).unwrap();
        assert!(renew_own_lease(&directory, &Lease::new("u@h:/x", 3, ttl)).unwrap());
        let next = Lease::new(ALPHA, 4, ttl);
        admit_lease(&directory, &next).unwrap();
        assert!(!renew_own_lease(&directory, &Lease::new("u@h:/x", 3, ttl)).unwrap());
        assert_eq!(read_lease(&directory).unwrap(), Some(next));
    }

    /// A write is checked against the lease its channel was accepted at:
    /// allowed while the host holds it, refused once another leadership
    /// took the host — without the channel presenting anything — and
    /// refused once the lease went unrenewed for its lifetime by this
    /// host's clock, whatever the leader's clock wrote into it.
    #[test]
    fn a_write_is_refused_once_the_lease_it_rode_on_is_gone() {
        let keep = tempfile::tempdir().expect("a temporary directory");
        let directory = keep.path().join(DIRECTORY);
        let ttl = Duration::from_secs(30);
        let accepted = Lease::new(ALPHA, 5, ttl);
        admit_lease(&directory, &accepted).unwrap();
        drop(check_write(&directory, &accepted).expect("the lease holds"));

        // Lapsed: received 31 seconds ago on this host and never renewed,
        // though the leader's own clock says it renewed just now.
        let receipt = Receipt {
            leader: ALPHA.into(),
            term: 5,
            received_at: now_seconds() - 31,
        };
        write_file(
            &directory,
            RECEIPT_FILE,
            &serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let error = check_write(&directory, &accepted).expect_err("a lapsed lease");
        assert!(
            matches!(
                error.downcast_ref::<WriteRefused>(),
                Some(WriteRefused::Lapsed { .. })
            ),
            "{error:#}"
        );
        assert!(format!("{error:#}").starts_with("fenced:"), "{error:#}");
        // Renewed, it holds again.
        admit_lease(&directory, &accepted).unwrap();
        drop(check_write(&directory, &accepted).expect("renewed"));

        // Superseded.
        let newer = Lease::new("u@h:/x", 6, ttl);
        admit_lease(&directory, &newer).unwrap();
        let error = check_write(&directory, &accepted).expect_err("a superseded lease");
        match error.downcast_ref::<WriteRefused>() {
            Some(WriteRefused::Superseded { current, .. }) => assert_eq!(current, &newer),
            other => panic!("expected a superseded lease, got {other:?}"),
        }
        assert!(format!("{error:#}").starts_with("fenced:"), "{error:#}");
    }

    /// While a write holds the lease lock, a new lease waits for it: the
    /// write never lands after the host moved on to another leader.
    #[test]
    fn a_lease_waits_for_a_write_in_progress() {
        let keep = tempfile::tempdir().expect("a temporary directory");
        let directory = keep.path().join(DIRECTORY);
        let ttl = Duration::from_secs(30);
        let accepted = Lease::new(ALPHA, 5, ttl);
        admit_lease(&directory, &accepted).unwrap();
        let guard = check_write(&directory, &accepted).unwrap();
        let admitted = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let rival = scope.spawn(|| {
                admit_lease(&directory, &Lease::new("u@h:/x", 6, ttl)).unwrap();
                admitted.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(200));
            assert!(
                !admitted.load(std::sync::atomic::Ordering::SeqCst),
                "a lease was admitted while a write held the lock"
            );
            drop(guard);
            rival.join().unwrap();
        });
        assert!(admitted.load(std::sync::atomic::Ordering::SeqCst));
        assert!(check_write(&directory, &accepted).is_err());
    }

    /// Pushes of one file on several channels at once each write a
    /// temporary of their own: the file ends up whole, as one of them
    /// wrote it, and no temporary is left behind.
    #[test]
    fn concurrent_pushes_of_one_file_never_mix() {
        let keep = tempfile::tempdir().expect("a temporary directory");
        let directory = keep.path().join(DIRECTORY);
        let contents: Vec<Vec<u8>> = (0..8u8).map(|i| vec![b'a' + i; 256 * 1024]).collect();
        for _ in 0..10 {
            std::thread::scope(|scope| {
                for content in &contents {
                    let directory = &directory;
                    scope.spawn(move || {
                        write_pushed_file(directory, "config.toml", content).unwrap();
                    });
                }
            });
            let written = read_pushed_file(&directory, "config.toml")
                .unwrap()
                .unwrap();
            assert!(contents.contains(&written), "a mixed or partial file");
        }
        let leftovers: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn an_ancestor_copy_follows_records_and_takes_checkpoints() {
        let keep = tempfile::tempdir().expect("a temporary directory");
        let directory = keep.path().join(DIRECTORY);
        let file = |name: &str| Node::directory(name, Vec::new());
        let session = crate::session::session_identifier("a", "b");
        let mut copy = AncestorCopy::open(&directory, &session).unwrap();
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
        let copy = AncestorCopy::open(&directory, &session).unwrap();
        assert_eq!(copy.generation(), 9);
        assert_eq!(copy.ancestor.as_ref().map(|n| n.children().len()), Some(2));
    }

    /// Records `generations` small cycles into a fresh store at `path`,
    /// every one journalled: the store is a journal and no checkpoint.
    fn journal_only(path: &Path, generations: u64) {
        let (mut store, _, _) = AncestorStore::open(path).unwrap();
        let mut names = Vec::new();
        for generation in 1..=generations {
            names.push(format!("f{generation:03}"));
            let tree = Node::directory(
                "",
                names
                    .iter()
                    .map(|name| Node::directory(name, Vec::new()))
                    .collect(),
            );
            store
                .record(
                    &[Change {
                        path: String::new(),
                        old: None,
                        new: Some(tree.clone()),
                    }],
                    Some(&tree),
                )
                .unwrap();
        }
        assert!(!path.exists(), "the store should hold no checkpoint");
        assert_eq!(AncestorStore::stored_generation(path).unwrap(), generations);
    }

    /// Sets when a store was last written, checkpoint and journal alike.
    fn written_at(path: &Path, seconds_ago: u64) {
        let when = SystemTime::now() - Duration::from_secs(seconds_ago);
        for file in [path.to_path_buf(), path.with_file_name("ancestor.journal")] {
            if file.exists() {
                std::fs::File::options()
                    .write(true)
                    .open(&file)
                    .unwrap()
                    .set_modified(when)
                    .unwrap();
            }
        }
    }

    /// Adoption takes the copy when it was written after the session's own
    /// store — the later agreement — whatever the generations say, and
    /// reads either store journal and all. A copy that lagged when a beta
    /// took over carries on below the generation the alpha reached before
    /// it left; it is still the later record, and is adopted.
    #[test]
    fn adoption_takes_the_later_agreement() {
        let keep = tempfile::tempdir().expect("a temporary directory");
        let state_root = keep.path();
        let directory = state_root.join(DIRECTORY);
        let session = crate::session::session_identifier("a", "b");
        let own = state_root.join("sessions").join(&session).join("ancestor");
        let copy = ancestor_copy_path(&directory, &session).unwrap();
        std::fs::create_dir_all(own.parent().unwrap()).unwrap();
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        let generation = |path: &Path| AncestorStore::stored_generation(path).unwrap();

        // No copy: nothing to adopt. No history of its own: the copy is.
        journal_only(&own, 5);
        assert!(!adopt_newer_copy(state_root, &directory, &session).unwrap());
        AncestorStore::reset(&own).unwrap();
        journal_only(&copy, 3);
        assert!(adopt_newer_copy(state_root, &directory, &session).unwrap());
        assert_eq!(generation(&own), 3);

        // Its own, written later, is kept — journal-only, and ahead or not.
        AncestorStore::reset(&own).unwrap();
        journal_only(&own, 10);
        written_at(&copy, 60);
        written_at(&own, 30);
        assert!(!adopt_newer_copy(state_root, &directory, &session).unwrap());
        assert_eq!(generation(&own), 10);

        // The copy, written later, is adopted — even at a lower generation:
        // the histories parted, and the later one is the copy's.
        AncestorStore::reset(&copy).unwrap();
        journal_only(&copy, 9);
        written_at(&own, 60);
        written_at(&copy, 30);
        assert!(adopt_newer_copy(state_root, &directory, &session).unwrap());
        assert_eq!(generation(&own), 9);
        let (_, adopted, _) = AncestorStore::open(&own).unwrap();
        assert_eq!(adopted.map(|tree| tree.children().len()), Some(9));
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
///
/// This host's name is per group: two groups can reach it at two roots,
/// and each pushes its own under `names/<group>`. `name` is the one the
/// last group pushed, and stands in for a group that pushed none. The
/// star leads under one name — a lease is per host — the first group's.
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
    let mut leader: Option<String> = None;
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
        let name = match read_pushed_file(directory, &format!("names/{group_name}"))? {
            Some(pushed) => String::from_utf8(pushed).context("the pushed name is not UTF-8")?,
            None => name.to_owned(),
        };
        let name = name.as_str();
        let Some(index) = group
            .betas
            .iter()
            .position(|entry| full(entry, &group.alpha) == name)
        else {
            continue;
        };
        leader.get_or_insert_with(|| name.to_owned());
        let own_path = name
            .rsplit_once(':')
            .map(|(_, path)| path.to_owned())
            .unwrap_or_else(|| name.to_owned());
        // The other betas as they were, and the configured alpha as a
        // beta spec reached by attachment — so the star is never empty,
        // and the attached plan gets every setting the group carries.
        // Its sides are swapped back below.
        let mut betas: Vec<String> = group
            .betas
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .map(|(_, entry)| full(entry, &group.alpha))
            .collect();
        betas.push(format!("{}:{}", attached_destination(ALPHA), group.alpha));
        let mut turned = crate::config::Group {
            alpha: own_path,
            betas,
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
    let (Some(position), Some(leader)) = (position, leader) else {
        bail!("{name:?} is not a beta of any peering group in the pushed configuration");
    };
    config.groups = groups;
    let planned = config
        .plans()
        .context("unable to plan the follower's star")?;
    // The configured alpha's session: the alpha is reached by attachment,
    // not dialed, and it keeps its side of the pair — so the session, and
    // the ancestor copy the leader pushed under its identifier, is the
    // same one the leader ran. Without a pushed identifier the session
    // has no known past, and it is left out rather than started afresh.
    let mut plans = Vec::with_capacity(planned.len());
    for plan in planned {
        let attached = match &plan.beta {
            crate::config::EndpointTarget::Remote {
                destination, path, ..
            } if attached_name(destination).is_some() => Some(path.clone()),
            _ => None,
        };
        match attached {
            None => plans.push(plan),
            Some(alpha_path) => {
                let pushed = read_pushed_file(directory, &format!("sessions/{}", plan.group))?;
                let Some(identifier) = pushed else {
                    continue;
                };
                let identifier = String::from_utf8(identifier).context("the pushed session id")?;
                plans.push(plan.attached_alpha(&alpha_path, identifier.trim().to_owned())?);
            }
        }
    }
    let interval = plans
        .iter()
        .map(|plan| plan.interval)
        .min()
        .unwrap_or(Duration::from_secs(5));
    Ok(FollowerStar {
        name: leader,
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
        [advanced.peering-dangerously-experimental]
        ttl = "10s"
        failover_after = "20s"

        [defaults]
        mode = "two-way-conflict"

        [groups.plain]
        alpha = "/tmp/plain"
        betas = ["x@h:/tmp/plain"]

        [groups.g]
        mode = "peering-conflict-dangerously-experimental"
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

    /// A pushed session identifier names the session's directory, lock and
    /// ancestor on the follower, so it is held to the shape a leader makes:
    /// one that could name a path elsewhere refuses the whole star, naming
    /// the group, and nothing is derived from it.
    #[test]
    fn a_pushed_session_identifier_must_be_genuine() {
        let keep = tempfile::tempdir().expect("tempdir");
        let name = "ubuntu@vm:/home/faraz/Workspace/Voltai";
        for hostile in ["../../x", "/tmp/elsewhere", "a/b", "", "ABCDEF"] {
            write_pushed_file(keep.path(), "sessions/g", hostile.as_bytes()).unwrap();
            let error = derive_star(PUSHED, name, keep.path()).expect_err(hostile);
            let message = format!("{error:#}");
            assert!(
                message.contains("group g") && message.contains("not one a leader makes"),
                "{hostile:?}: {message}"
            );
        }
        let genuine = crate::session::session_identifier("a", "b");
        write_pushed_file(keep.path(), "sessions/g", format!("{genuine}\n").as_bytes()).unwrap();
        let star = derive_star(PUSHED, name, keep.path()).expect("a genuine identifier");
        assert!(
            star.plans.iter().any(|plan| plan.identifier() == genuine),
            "{:?}",
            star.plans
        );
    }

    /// Two groups reach one host at two roots. Each pushes its own name
    /// for the host, and the host-wide one is whichever pushed last; the
    /// star covers both groups, each from its own root, and fails over
    /// both.
    #[test]
    fn two_groups_on_one_host_both_fail_over() {
        const TWO: &str = r#"
            [groups.docs]
            mode = "peering-conflict-dangerously-experimental"
            alpha = "/home/f/docs"
            betas = ["box:/srv/docs", "other:/srv/docs"]

            [groups.code]
            mode = "peering-conflict-dangerously-experimental"
            alpha = "/home/f/code"
            betas = ["box:/srv/code"]
        "#;
        let pushed = |per_group: bool| {
            let keep = tempfile::tempdir().expect("tempdir");
            for group in ["docs", "code"] {
                let session = crate::session::session_identifier(group, "box");
                write_pushed_file(
                    keep.path(),
                    &format!("sessions/{group}"),
                    session.as_bytes(),
                )
                .unwrap();
                if per_group {
                    let name = format!("box:/srv/{group}");
                    write_pushed_file(keep.path(), &format!("names/{group}"), name.as_bytes())
                        .unwrap();
                }
            }
            keep
        };
        let roots = |star: &FollowerStar| -> std::collections::BTreeSet<(String, String)> {
            star.plans
                .iter()
                .map(|plan| {
                    // This host's own side is the local one: the alpha,
                    // or the beta of the session with the attached alpha.
                    let root = match (&plan.alpha, &plan.beta) {
                        (crate::config::EndpointTarget::Local(root), _)
                        | (_, crate::config::EndpointTarget::Local(root)) => root,
                        _ => panic!("no side of {plan:?} is this host"),
                    };
                    (plan.group.clone(), root.to_string_lossy().into_owned())
                })
                .collect()
        };

        // The host-wide name is the one the last group pushed.
        let keep = pushed(true);
        let star = derive_star(TWO, "box:/srv/code", keep.path()).expect("a star");
        assert_eq!(
            roots(&star),
            [("code", "/srv/code"), ("docs", "/srv/docs")]
                .map(|(group, root)| (group.to_owned(), root.to_owned()))
                .into(),
            "{:?}",
            star.plans
        );
        // It leads under one name, the first group's, and its place in
        // the order is the best either group gives it.
        assert_eq!(star.name, "box:/srv/code");
        assert_eq!(star.position, 1);

        // Without per-group names, only the group the host-wide name
        // matches fails over: what a leader from before them pushed.
        let keep = pushed(false);
        let star = derive_star(TWO, "box:/srv/code", keep.path()).expect("a star");
        assert_eq!(
            roots(&star),
            [("code".to_owned(), "/srv/code".to_owned())].into()
        );
    }
}

/// The host part of an endpoint reached by an attachment rather than by
/// dialing: the configured alpha, from a beta that leads. The destination
/// is `<name>@attached` — the user-at-host shape a spec already allows,
/// with no colon in it, so `<name>@attached:<path>` parses as any remote
/// spec does. The name is `alpha`, the one member that dials in.
pub const ATTACHED_HOST: &str = "attached";

/// The destination for an attached peer.
pub fn attached_destination(name: &str) -> String {
    format!("{name}@{ATTACHED_HOST}")
}

/// The peer name an attached destination carries, if it is one.
pub fn attached_name(destination: &str) -> Option<&str> {
    destination.strip_suffix(&format!("@{ATTACHED_HOST}"))
}

/// Which side of a session is the peer — the host the lease, the
/// records and the files go to. The beta, except for a session a beta
/// runs against the attached alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerSide {
    Alpha,
    Beta,
}

/// The name of the attach socket in a leading peer's directory.
pub const ATTACH_SOCKET: &str = "attach.sock";

/// The environment variable that replaces the command the alpha runs to
/// attach to a leader: an argv, whitespace-split, with `{destination}`
/// standing for the leader's SSH destination. For tests, and for
/// transports other than SSH.
pub const ATTACH_COMMAND_VARIABLE: &str = "AUTOBAHN_PEERING_ATTACH";

/// The command the alpha runs to attach to the leader at `destination`:
/// `ssh <destination> autobahn peering attach`, unless the environment
/// says otherwise.
pub fn attach_argv(destination: &str) -> Vec<String> {
    if let Ok(template) = std::env::var(ATTACH_COMMAND_VARIABLE) {
        let argv: Vec<String> = template
            .split_whitespace()
            .map(|word| word.replace("{destination}", destination))
            .collect();
        if !argv.is_empty() {
            return argv;
        }
    }
    crate::transport::ssh_argv_for(destination, "autobahn peering attach")
}

/// The SSH destination of a leader named by its spec (`user@host:path`).
pub fn destination_of(leader: &str) -> &str {
    leader
        .rsplit_once(':')
        .map(|(destination, _)| destination)
        .unwrap_or(leader)
}

/// Brings a session's ancestor level with the copy a leader pushed here,
/// when the copy is newer: the session directory's store is replaced by
/// the copy's files. A beta that starts to lead seeds its sessions this
/// way; an alpha that gets the lead back adopts what the beta recorded
/// meanwhile. Returns whether anything was adopted.
///
/// Newer means written later, not a higher generation. Each store records
/// the last state its session agreed on, and the later agreement is the
/// one to continue from. Generations cannot say which that is once the
/// two histories have parted: a copy that lagged when a beta took over
/// carries on from where it lagged, and a history that was reset counts
/// from one again, so either can be the later record at the lower
/// number. The times are comparable because both stores are on this host
/// and written by it — the copy by this host's agent, the session's by its
/// supervisor — so one clock stamped both. A copy with no history is never
/// adopted, and its history is read journal and all: a store may be a
/// checkpoint, a journal, or both.
pub fn adopt_newer_copy(state_root: &Path, directory: &Path, session: &str) -> Result<bool> {
    let copy = ancestor_copy_path(directory, session)?;
    if AncestorStore::stored_generation(&copy)? == 0 {
        return Ok(false);
    }
    let own = state_root.join("sessions").join(session).join("ancestor");
    let newer = AncestorStore::stored_generation(&own)? == 0
        || match (
            AncestorStore::last_written(&copy)?,
            AncestorStore::last_written(&own)?,
        ) {
            (Some(copied), Some(held)) => copied > held,
            (_, None) => true,
            (None, Some(_)) => false,
        };
    if !newer {
        return Ok(false);
    }
    if let Some(parent) = own.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("unable to create {}", parent.display()))?;
    }
    AncestorStore::copy_store(&copy, &own)?;
    Ok(true)
}
