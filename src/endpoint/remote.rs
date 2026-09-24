//! The remote endpoint: a client proxy speaking the agent protocol over one
//! channel of a (possibly shared) multiplexed agent connection.
//!
//! Every [`Endpoint`] method is a synchronous exchange on the endpoint's
//! channel: one request frame out, one response frame back. The agent
//! answers request-level failures with [`Response::Error`], which becomes an
//! ordinary error here (prefixed to keep the far side's message
//! distinguishable from local failures), while a response that doesn't
//! correspond to the request is a protocol error. Channels on the same
//! connection interleave freely — see [`crate::transport::mux`].

use std::sync::{mpsc, Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};

use super::{Endpoint, FileRequest, StagingNeed, TransferFrame, TransitionOutcome};
use crate::protocol::{Initialize, Request, Response, ScanDelta};
use crate::transport::mux::{AgentChannel, AgentConnection, AgentPool};
use crate::transport::{self, Connection};
use crate::tree::{Change, Node, Snapshot};

/// The failure raised when a destination cannot be reached at all: the
/// host is down, asleep, or refusing the connection.
///
/// Its own error type because it is the one failure that reliably clears
/// itself, and both the status vocabulary and the alerter treat it
/// differently for that reason.
#[derive(Debug, thiserror::Error)]
#[error("unable to synchronize with {destination}")]
pub struct Unreachable {
    /// The destination that could not be reached.
    pub destination: String,
}

/// A remote endpoint backed by one channel of an agent connection.
///
/// Dropping the endpoint closes its channel; when that channel was the
/// connection's last, the connection shuts the agent down and reaps its
/// process.
/// The number of staging push batches kept in flight before waiting for an
/// acknowledgement. With batches sized toward
/// [`SUPPLY_TARGET_BYTES`](crate::endpoint::local::SUPPLY_TARGET_BYTES),
/// this bounds unacknowledged data in transit while hiding the round-trip
/// latency that a strict push-ack-push cadence would pay per batch.
const PUSH_WINDOW: usize = 4;

pub struct RemoteEndpoint {
    /// The endpoint's channel.
    channel: AgentChannel,
    /// The watch on the agent's side, when the connection could open a
    /// second channel for it. See [`RemoteWatch`].
    watch: Option<RemoteWatch>,
    /// The generation of the agent's root the last scan or transition
    /// left `last_snapshot` describing, as the agent reported it.
    seen: Option<u64>,
    /// When the last real scan was answered; a cached snapshot is trusted
    /// only for so long, so the agent's periodic full walk still happens.
    scanned_at: Option<std::time::Instant>,
    /// Whether the agent said it was watching its root, the last time it
    /// answered a wait.
    watching: bool,
    /// A staging request sent without waiting for its answer, and the
    /// answer once it arrives — which may be while push acknowledgements
    /// are being collected, since it comes first on the channel.
    stage_begin_pending: bool,
    stage_begin_answer: Option<Vec<StagingNeed>>,
    /// The number of staging pushes sent but not yet acknowledged.
    pending_pushes: usize,
    /// The controller's model of the agent's snapshot: the last one
    /// received, with each subsequent transition's results folded in by the
    /// same fold the agent applies. Retaining it is what lets an unchanged
    /// rescan cost nothing on the wire.
    last_snapshot: Option<Snapshot>,
    /// Where this endpoint's scans report that they are running.
    progress: Option<Arc<crate::progress::SideProgress>>,
}

/// Holds a side in its scanning state until the scan returns, however it
/// returns: a scan that fails partway must not leave the side looking as
/// though it is still running.
struct ScanGuard {
    progress: Option<Arc<crate::progress::SideProgress>>,
}

impl ScanGuard {
    /// Ends the scan with the total it established.
    fn finish(&mut self, total: u64) {
        if let Some(progress) = self.progress.take() {
            progress.end(Some(total));
        }
    }
}

impl Drop for ScanGuard {
    fn drop(&mut self) {
        // Still held means the scan did not reach `finish`: it failed. The
        // side stops scanning, and the next one is measured against
        // whatever the last *successful* scan established.
        if let Some(progress) = self.progress.take() {
            progress.end(None);
        }
    }
}

/// Ends a remote scan, leaving a successful snapshot's own total behind for
/// the next scan to be measured against.
fn finish_scan(guard: Option<ScanGuard>, snapshot: &Result<Snapshot>) {
    if let (Some(mut guard), Ok(snapshot)) = (guard, snapshot) {
        guard.finish(snapshot.directories + snapshot.files + snapshot.symlinks);
    }
}

impl RemoteEndpoint {
    /// Marks this side as scanning for as long as the returned guard
    /// lives.
    ///
    /// A remote scan happens inside a single request on the far side; one
    /// that runs long reports its count as it goes (`ScanProgress`), which
    /// `request_scan` folds in. The completed snapshot's own total is left
    /// behind for the next scan to be measured against.
    fn scanning(&self) -> Option<ScanGuard> {
        let progress = self.progress.clone()?;
        progress.begin(false);
        Some(ScanGuard {
            progress: Some(progress),
        })
    }

    /// Asks the agent for a scan and receives the snapshot, however it
    /// chose to send it.
    fn request_scan(&mut self, request: Request, what: &'static str) -> Result<Snapshot> {
        self.scanned_at = Some(std::time::Instant::now());
        let mut response = self.exchange(request)?;
        // A long scan reports its count as it goes, ahead of its answer.
        while let Response::ScanProgress { entries, bytes } = response {
            if let Some(progress) = &self.progress {
                progress.report(entries, bytes);
            }
            response = match self.channel.receive_response()? {
                Response::Error(message) => return Err(remote_error(message)),
                response => response,
            };
        }
        match response {
            // Every scan now answers as a delta or "unchanged"; this arm
            // goes at the next epoch bump. Until then what arrives whole is
            // held to the same hierarchy check as what is reassembled.
            Response::Scan(snapshot) => {
                check_hierarchy(&snapshot)?;
                self.seen = None;
                self.last_snapshot = Some(snapshot.clone());
                Ok(snapshot)
            }
            Response::ScanDelta(header) => {
                self.seen = Some(header.generation);
                self.receive_snapshot(header, what)
            }
            // The agent reports "unchanged" only against a snapshot it has
            // actually sent, so having nothing to reproduce means the two
            // sides disagree about what was transmitted. That is a protocol
            // defect, and silently rescanning would paper over it.
            Response::ScanUnchanged { generation } => {
                self.seen = Some(generation);
                self.last_snapshot.clone().ok_or_else(|| {
                    anyhow!("the agent reported an unchanged scan before sending one")
                })
            }
            response => Err(unexpected_response(&response, what)),
        }
    }

    /// Reassembles a snapshot sent as a delta, retrying in full when the
    /// baseline the agent named cannot be reproduced here.
    fn receive_snapshot(&mut self, header: ScanDelta, what: &str) -> Result<Snapshot> {
        match self.reassemble(&header) {
            Ok(snapshot) => {
                self.last_snapshot = Some(snapshot.clone());
                Ok(snapshot)
            }
            Err(error) if header.baseline.is_some() => {
                // The baseline disagreement is a performance event, not a
                // protocol failure: the delta stream is drained already (or
                // was never valid), and the agent re-sends against nothing.
                eprintln!(
                    "note: the agent's snapshot baseline could not be reproduced \
                     ({error:#}); requesting it in full"
                );
                let header = match self.exchange(Request::ScanFull)? {
                    Response::ScanDelta(header) => header,
                    response => return Err(unexpected_response(&response, what)),
                };
                if header.baseline.is_some() {
                    bail!("the agent answered a full-scan request with a delta");
                }
                let snapshot = self.reassemble(&header)?;
                self.last_snapshot = Some(snapshot.clone());
                Ok(snapshot)
            }
            Err(error) => Err(error),
        }
    }

    /// Pulls a delta's operations and applies them to the baseline this
    /// endpoint holds, verifying the result against the header's digest.
    fn reassemble(&mut self, header: &ScanDelta) -> Result<Snapshot> {
        use std::io::Cursor;

        check_delta_header(header)?;

        // The base is the encoding of the snapshot this endpoint last
        // received — re-encoded now, so nothing is held between scans. Its
        // digest must be the one the agent computed the delta against.
        let (base, signature) = match header.baseline {
            Some(expected) => {
                let last = self
                    .last_snapshot
                    .as_ref()
                    .ok_or_else(|| anyhow!("no previous snapshot to serve as the baseline"))?;
                let base = crate::transport::encode_snapshot(last)?;
                let actual = *blake3::hash(&base).as_bytes();
                if actual != expected {
                    // The stream must still be drained, or the next request
                    // on this channel would be answered with its leftovers.
                    self.drain_delta()?;
                    bail!("the baseline encoding here differs from the agent's");
                }
                let signature = crate::rsync::signature(Cursor::new(&base), header.block_size)
                    .context("unable to sign the baseline snapshot")?;
                (base, signature)
            }
            None => (Vec::new(), crate::rsync::Signature::default()),
        };

        let mut output = Vec::with_capacity(header.length.min(REASSEMBLY_PREALLOCATION) as usize);
        let mut base = Cursor::new(base);
        loop {
            let ops = match self.exchange(Request::ScanPull)? {
                Response::ScanOps(ops) => ops,
                response => return Err(unexpected_response(&response, "scan pull")),
            };
            if ops.is_empty() {
                break;
            }
            apply_delta_ops(&mut base, &signature, &ops, &mut output, header.length)?;
        }
        if *blake3::hash(&output).as_bytes() != header.digest {
            bail!("the reassembled snapshot does not match the agent's digest");
        }
        let snapshot: Snapshot =
            bincode::deserialize(&output).context("unable to decode the reassembled snapshot")?;
        check_hierarchy(&snapshot)?;
        Ok(snapshot)
    }

    /// Discards the rest of a delta stream.
    fn drain_delta(&mut self) -> Result<()> {
        loop {
            match self.exchange(Request::ScanPull)? {
                Response::ScanOps(ops) if ops.is_empty() => return Ok(()),
                Response::ScanOps(_) => continue,
                response => return Err(unexpected_response(&response, "scan pull")),
            }
        }
    }

    /// Establishes a remote endpoint as the sole session over a dedicated
    /// connection: exchanges handshakes (enforcing version equality) and
    /// opens one channel with the session's root and policy.
    pub fn connect(connection: Connection, initialize: Initialize) -> Result<RemoteEndpoint> {
        let connection = AgentConnection::connect(connection)?;
        let channel = connection.open(initialize.clone())?;
        let watch = connection.open(initialize)?;
        Ok(RemoteEndpoint::from_channel(channel).with_watch(watch))
    }

    /// Wraps an already-open channel (the pooled path).
    pub(crate) fn from_channel(channel: AgentChannel) -> RemoteEndpoint {
        RemoteEndpoint {
            channel,
            watch: None,
            seen: None,
            scanned_at: None,
            watching: false,
            stage_begin_pending: false,
            stage_begin_answer: None,
            pending_pushes: 0,
            last_snapshot: None,
            progress: None,
        }
    }

    /// Gives the endpoint a second channel to the same agent, over which
    /// it watches for changes while the first is free for everything else.
    /// Without one, watching degrades to the heartbeat.
    pub(crate) fn with_watch(mut self, channel: AgentChannel) -> RemoteEndpoint {
        self.watch = Some(RemoteWatch::start(channel));
        self
    }

    /// Terminates the endpoint, reporting failures that the silent drop
    /// path would swallow: an unsendable close, a shutdown the agent
    /// ignored, or a non-successful agent exit.
    pub fn close(self) -> Result<()> {
        self.channel.close()
    }

    /// Consumes one owed staging push acknowledgement. The pending count
    /// decreases even on failure: either a response was consumed or the
    /// channel itself failed (in which case nothing further will arrive).
    fn drain_push_ack(&mut self) -> Result<()> {
        let response = self.channel.receive_response();
        // A staging request sent ahead of the pushes answers ahead of their
        // acknowledgements: its answer is kept for `stage_begin_finish` and
        // is not a push's.
        if self.stage_begin_pending {
            if let Ok(Response::StageBegin(needs)) = &response {
                self.stage_begin_answer = Some(needs.clone());
                self.stage_begin_pending = false;
                return self.drain_push_ack();
            }
        }
        self.pending_pushes -= 1;
        match response? {
            Response::StagePushed => Ok(()),
            Response::Error(message) => Err(remote_error(message)),
            response => Err(unexpected_response(&response, "stage push")),
        }
    }

    /// Drains staging push acknowledgements until at most `target` remain.
    /// On any failure, every remaining acknowledgement is drained as well —
    /// so the channel never carries stale staging responses into later
    /// requests — and the first error is returned.
    fn drain_pushes_to(&mut self, target: usize) -> Result<()> {
        let mut first_error = None;
        while self.pending_pushes > 0 {
            if first_error.is_none() && self.pending_pushes <= target {
                break;
            }
            if let Err(error) = self.drain_push_ack() {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Performs one exchange, translating a remote failure into a local
    /// error. The returned response is guaranteed not to be
    /// [`Response::Error`].
    fn exchange(&mut self, request: Request) -> Result<Response> {
        // Acknowledgements still owed for pushes come first: the channel
        // answers in order, so the next response on it is theirs, not this
        // request's. (Only a transition sends before collecting them; see
        // `transition`.)
        self.drain_pushes_to(0)?;
        match self.channel.exchange(request)? {
            Response::Error(message) => Err(remote_error(message)),
            response => Ok(response),
        }
    }
}

/// Connects to an agent on a remote SSH host, installing this version's
/// agent and retrying once when the first attempt fails.
///
/// The remote command always names the *versioned* agent path
/// (`~/.autobahn/bin/autobahn-<version>`), so hosts are upgraded
/// automatically: a controller that was just upgraded finds its versioned
/// agent missing, installs it, and proceeds — no fleet-wide lockstep
/// required, and older controllers keep using their own older agents
/// untouched.
pub fn connect_ssh(destination: &str, initialize: Initialize) -> Result<RemoteEndpoint> {
    let connection = establish_ssh(destination)?;
    let channel = connection.open(initialize.clone())?;
    let watch = connection.open(initialize)?;
    Ok(RemoteEndpoint::from_channel(channel).with_watch(watch))
}

/// Establishes a multiplexed connection to a remote SSH host, with the
/// install-and-retry behavior of [`connect_ssh`].
fn establish_ssh(destination: &str) -> Result<AgentConnection> {
    use anyhow::Context;
    let remote_command = transport::install::versioned_remote_command();
    let argv = Connection::ssh_argv(destination, Some(&remote_command));
    let attempt = |quiet: bool| -> Result<AgentConnection> {
        let connection = if quiet {
            Connection::spawn_relayed(&argv, destination)?
        } else {
            Connection::spawn(&argv)?
        };
        AgentConnection::connect(connection)
    };
    // The first attempt is speculative — on a host that has never been
    // synchronized, or one whose agent predates an upgrade, it is *expected*
    // to fail — so its noise is suppressed. The retry after installation is
    // not speculative, and is allowed to speak.
    let initial = match attempt(true) {
        Ok(connection) => return Ok(connection),
        Err(error) => error,
    };
    // The versioned agent is missing or unusable; install it and retry
    // once. (If the failure was something else — authentication, an
    // unreachable host — installation fails the same way and both failures
    // surface together.)
    // When installation fails, *its* error is the diagnosis — the host is
    // unreachable, or authentication failed, and the installer's probe
    // carries ssh's own words. The speculative connection's failure says
    // only "connection closed", which is what a missing agent always looks
    // like, so it is kept for the case where installation succeeds and the
    // retry still fails.
    // Typed, not merely worded: `status` and the alerter both need to know
    // that this failure is "the host is not there" rather than "something
    // went wrong", and deciding that by searching the message for a phrase
    // means any rewording of the message silently reclassifies the session.
    let installed = transport::install::ensure_agent(destination).map_err(|error| {
        anyhow::Error::new(Unreachable {
            destination: destination.to_owned(),
        })
        .context(format!("{error:#}"))
    })?;
    attempt(false).with_context(|| {
        // A version mismatch *here* is not a host that needs upgrading —
        // the agent it is complaining about was installed seconds ago, by
        // the line above. It means the bundle that agent was copied from
        // holds an older build than this controller, and the name it was
        // given (which carries the controller's version) says nothing
        // about the code inside it. Nothing on this side can read the
        // version out of a binary built for another platform, so the
        // handshake is the first thing that can notice — and without this
        // the message blames the remote for a file that is stale here.
        format!(
            "the agent just installed on {destination} does not match this build. \
             The {} bundle it was copied from is stale: {}. Rebuild it, or remove \
             it so a matching one is used. (initial failure: {initial:#})",
            installed.platform,
            installed.provenance(),
        )
    })
}

/// Opens a remote endpoint through a connection pool: sessions with the
/// same spawn command share one connection (one SSH process, one
/// authentication, one entry against any per-IP connection limit), each on
/// its own channel.
pub fn connect_pooled(
    pool: &AgentPool,
    destination: Option<&str>,
    argv: &[String],
    initialize: Initialize,
) -> Result<RemoteEndpoint> {
    let establish = || match destination {
        // The SSH path installs the agent on first contact; the pool lets
        // one session per host establish at a time, so concurrent sessions
        // for one host wait for this single bootstrap instead of racing
        // their own.
        Some(destination) => establish_ssh(destination),
        None => AgentConnection::connect(Connection::spawn(argv)?),
    };
    let channel = pool.channel(argv, initialize.clone(), establish)?;
    // The watch rides the same pooled connection: the connection exists
    // now, so this opens a channel on it rather than establishing again.
    let watch = pool.channel(argv, initialize, establish)?;
    Ok(RemoteEndpoint::from_channel(channel).with_watch(watch))
}

impl Endpoint for RemoteEndpoint {
    fn is_remote(&self) -> bool {
        true
    }

    fn set_scan_progress(&mut self, progress: Arc<crate::progress::SideProgress>) {
        self.progress = Some(progress);
    }

    fn scan(&mut self) -> Result<Snapshot> {
        let scanning = self.scanning();
        let snapshot = self.request_scan(Request::Scan, "scan");
        finish_scan(scanning, &snapshot);
        snapshot
    }

    fn read_file(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
        match self.exchange(Request::ReadFile(path.to_owned()))? {
            Response::File(content) => Ok(content),
            response => Err(unexpected_response(&response, "read file")),
        }
    }

    fn lease(&mut self, lease: &crate::peering::Lease) -> Result<crate::peering::LeaseAnswer> {
        match self.exchange(Request::Lease(lease.clone()))? {
            Response::Lease(answer) => Ok(answer),
            response => Err(unexpected_response(&response, "lease")),
        }
    }

    fn ancestor_record(&mut self, generation: u64, changes: &[Change]) -> Result<u64> {
        let request = Request::AncestorRecord {
            generation,
            changes: changes.to_vec(),
        };
        match self.exchange(request)? {
            Response::Recorded { generation } => Ok(generation),
            response => Err(unexpected_response(&response, "recorded")),
        }
    }

    fn ancestor_checkpoint(&mut self, generation: u64, ancestor: Option<&Node>) -> Result<u64> {
        let request = Request::AncestorCheckpoint {
            generation,
            ancestor: ancestor.cloned(),
        };
        match self.exchange(request)? {
            Response::Recorded { generation } => Ok(generation),
            response => Err(unexpected_response(&response, "recorded")),
        }
    }

    fn put_peering_file(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        let request = Request::PutPeeringFile {
            name: name.to_owned(),
            bytes: bytes.to_vec(),
        };
        match self.exchange(request)? {
            Response::Written => Ok(()),
            response => Err(unexpected_response(&response, "written")),
        }
    }

    fn peering_state(&mut self) -> Result<crate::peering::State> {
        match self.exchange(Request::PeeringState)? {
            Response::PeeringState(state) => Ok(state),
            response => Err(unexpected_response(&response, "peering state")),
        }
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        match self.exchange(Request::Rename(from.to_owned(), to.to_owned()))? {
            Response::Written => Ok(()),
            response => Err(unexpected_response(&response, "move entry")),
        }
    }

    fn scan_verified(&mut self) -> Result<Snapshot> {
        let scanning = self.scanning();
        let snapshot = self.request_scan(Request::ScanVerified, "verified scan");
        finish_scan(scanning, &snapshot);
        snapshot
    }

    fn stage_begin(&mut self, files: Vec<FileRequest>) -> Result<Vec<StagingNeed>> {
        match self.exchange(Request::StageBegin(files))? {
            Response::StageBegin(needs) => Ok(needs),
            response => Err(unexpected_response(&response, "stage begin")),
        }
    }

    fn stage_begin_nowait(&mut self, files: Vec<FileRequest>) -> Result<Option<Vec<StagingNeed>>> {
        // Owed acknowledgements first, so the request's answer is the
        // next response on the channel after them.
        self.drain_pushes_to(0)?;
        self.channel.send_only(Request::StageBegin(files))?;
        self.stage_begin_pending = true;
        self.stage_begin_answer = None;
        Ok(None)
    }

    fn stage_begin_finish(&mut self) -> Result<Vec<StagingNeed>> {
        if let Some(needs) = self.stage_begin_answer.take() {
            return Ok(needs);
        }
        if !self.stage_begin_pending {
            bail!("no staging request is awaiting an answer");
        }
        // Sent before any push, so answered before any acknowledgement.
        self.stage_begin_pending = false;
        match self.channel.receive_response()? {
            Response::StageBegin(needs) => Ok(needs),
            Response::Error(message) => Err(remote_error(message)),
            response => Err(unexpected_response(&response, "stage begin")),
        }
    }

    fn supply_open(&mut self, needs: Vec<StagingNeed>) -> Result<()> {
        match self.exchange(Request::SupplyOpen(needs))? {
            Response::SupplyOpened => Ok(()),
            response => Err(unexpected_response(&response, "supply open")),
        }
    }

    fn supply_pull(&mut self, max_frames: usize) -> Result<Vec<TransferFrame>> {
        match self.exchange(Request::SupplyPull(max_frames))? {
            Response::SupplyPull(frames) => Ok(frames),
            response => Err(unexpected_response(&response, "supply pull")),
        }
    }

    fn stage_push(&mut self, frames: Vec<TransferFrame>) -> Result<()> {
        match self.exchange(Request::StagePush(frames))? {
            Response::StagePushed => Ok(()),
            response => Err(unexpected_response(&response, "stage push")),
        }
    }

    fn stage_push_nowait(&mut self, frames: Vec<TransferFrame>) -> Result<()> {
        // Keep at most a window of unacknowledged pushes in flight; each
        // ack drained here corresponds (in order) to an earlier push. A
        // failure drains the whole window before surfacing, leaving the
        // channel free of staging responses.
        self.drain_pushes_to(PUSH_WINDOW - 1)?;
        if let Err(error) = self.channel.send_only(Request::StagePush(frames)) {
            // A locally-failed send (e.g. an oversized batch) can leave the
            // connection healthy, so the in-flight window must still settle.
            let _ = self.drain_pushes_to(0);
            return Err(error);
        }
        self.pending_pushes += 1;
        Ok(())
    }

    fn stage_finish(&mut self) -> Result<()> {
        // The pushes' acknowledgements are left owed on the channel, for
        // the transition that follows to collect: it then goes out right
        // behind the last push, and the agent takes the pushes and the
        // transition in one pass — one round trip for all of them, rather
        // than one to learn the staging landed and another to use it. A
        // push that failed still fails the cycle, at the transition, which
        // collects the acknowledgements before reading its own answer; and
        // any other request collects them first as well (see `exchange`),
        // so nothing ever reads a push's answer as its own. A connection
        // that died is refused at the next send regardless.
        Ok(())
    }

    fn transition(&mut self, transitions: Vec<Change>) -> Result<TransitionOutcome> {
        // Sent before the owed push acknowledgements are collected, so the
        // request is on the wire while they are in flight; the answers are
        // then read in order, theirs and then this one. A failed push
        // reports first, so a transition applied over incomplete staging
        // is never taken for a clean one — the agent applies it regardless
        // and reports what was missing, and the controller cycles again.
        // The transitions are cloned because the fold below needs them
        // after the request has consumed them.
        self.channel
            .send_only(Request::Transition(transitions.clone()))?;
        let pushes = self.drain_pushes_to(0);
        let response = self.channel.receive_response();
        pushes?;
        match response? {
            Response::Error(message) => Err(remote_error(message)),
            Response::Transition {
                outcome,
                generation,
            } => {
                self.seen = Some(generation);
                // Model the agent's own fold of the achieved results, so the
                // cached snapshot keeps describing what the agent holds. The
                // two sides run the same fold over the same inputs; a fold
                // this side cannot perform simply drops the cache, and the
                // agent then resends in full.
                self.last_snapshot = self
                    .last_snapshot
                    .as_ref()
                    .and_then(|snapshot| super::fold_transition(snapshot, &transitions, &outcome));
                Ok(outcome)
            }
            response => Err(unexpected_response(&response, "transition")),
        }
    }

    fn await_change(&mut self, timeout: std::time::Duration) -> Result<bool> {
        // The agent blocks this channel for up to the requested timeout
        // before answering (other channels proceed), so callers keep
        // individual awaits short and loop.
        let request = Request::AwaitChanges {
            milliseconds: timeout.as_millis() as u64,
            since: self.seen,
        };
        match self.exchange(request)? {
            Response::AwaitChanges { changed, watching } => {
                self.watching = watching;
                Ok(changed)
            }
            response => Err(unexpected_response(&response, "await changes")),
        }
    }

    fn generation(&self) -> Option<u64> {
        self.seen
    }

    fn unchanged_since_scan(&mut self) -> bool {
        // A watch begun from the generation the last scan (or transition)
        // left, still standing with no answer, on a root the agent said it
        // was watching, within the time a cached snapshot is trusted.
        let Some(seen) = self.seen else {
            return false;
        };
        let fresh = self
            .scanned_at
            .is_some_and(|at| at.elapsed() < CACHED_SNAPSHOT_MAX_AGE);
        self.watching
            && fresh
            && self.last_snapshot.is_some()
            && self
                .watch
                .as_ref()
                .is_some_and(|watch| watch.standing_since() == Some(seen))
    }

    fn cached_snapshot(&self) -> Option<Snapshot> {
        self.last_snapshot.clone()
    }

    fn watch_begin(
        &mut self,
        timeout: std::time::Duration,
        signal: Arc<crate::endpoint::WakeSignal>,
    ) -> Result<()> {
        let since = self.seen;
        match &mut self.watch {
            Some(watch) => watch.begin(timeout, signal, since),
            None => Ok(()),
        }
    }

    fn watch_poll(&mut self) -> Result<Option<bool>> {
        match &mut self.watch {
            Some(watch) => match watch.poll()? {
                Some((changed, watching)) => {
                    self.watching = watching;
                    Ok(Some(changed))
                }
                None => Ok(None),
            },
            None => Ok(Some(false)),
        }
    }
}

/// How long a cycle may reuse the last scan's snapshot on the strength of
/// a standing watch. The agent's observer walks the whole root every so
/// often to catch what a watcher can miss; a scan is what runs that walk,
/// so scans are not skipped for longer than this.
const CACHED_SNAPSHOT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// The longest one watch request holds the agent's channel. A session's
/// wait can be as long as its heartbeat; the request is kept shorter so
/// that a watch outliving its endpoint — the thread below is joined by
/// nobody — releases its channel soon after.
const WATCH_REQUEST_MAX: std::time::Duration = std::time::Duration::from_secs(2);

/// A change watch on the agent's side, served over a channel of its own.
///
/// The controller's channel to an agent is strict request/response, so a
/// wait that blocks it would block the session's next scan with it. The
/// watch therefore has its own channel and its own thread: the thread
/// sends the wait, blocks for the answer, records it, and raises the
/// session's signal. Meanwhile the session sleeps on that signal with the
/// other endpoint's watch under way too, and whichever raises it first is
/// the one that ends the wait.
///
/// A watch left outstanding — the other side changed first — still means
/// what it says: nothing had changed on this side when it was asked. It is
/// simply polled again on the next wait rather than asked again. When it
/// answers late with a change, the next wait ends at once and the cycle
/// that follows finds whatever it was.
struct RemoteWatch {
    /// Waits to run: the timeout, the signal to raise when the wait ends,
    /// and the generation to wait from.
    requests: mpsc::Sender<(
        std::time::Duration,
        Arc<crate::endpoint::WakeSignal>,
        Option<u64>,
    )>,
    /// The last wait's answer — changed, watching — until it is polled.
    verdict: Arc<Mutex<Option<WatchVerdict>>>,
    /// The generation the outstanding wait was begun from, while one is
    /// sent and not yet polled.
    outstanding: Option<Option<u64>>,
}

/// A wait's answer: whether anything changed, and whether the agent's
/// watch is standing.
type WatchVerdict = Result<(bool, bool)>;

impl RemoteWatch {
    fn start(mut channel: AgentChannel) -> RemoteWatch {
        let (requests, waits) = mpsc::channel::<(
            std::time::Duration,
            Arc<crate::endpoint::WakeSignal>,
            Option<u64>,
        )>();
        let verdict: Arc<Mutex<Option<WatchVerdict>>> = Arc::default();
        let recorded = Arc::clone(&verdict);
        std::thread::Builder::new()
            .name("autobahn-watch".into())
            .spawn(move || {
                // Ends when the endpoint is dropped: the sender goes with
                // it, and the channel is closed by this drop.
                while let Ok((timeout, signal, since)) = waits.recv() {
                    let request = Request::AwaitChanges {
                        milliseconds: timeout.as_millis() as u64,
                        since,
                    };
                    let answer = channel
                        .exchange(request)
                        .and_then(|response| match response {
                            Response::AwaitChanges { changed, watching } => Ok((changed, watching)),
                            Response::Error(message) => Err(remote_error(message)),
                            response => Err(unexpected_response(&response, "await changes")),
                        });
                    *recorded.lock().unwrap_or_else(|e| e.into_inner()) = Some(answer);
                    signal.raise();
                }
            })
            .expect("unable to spawn the watch thread");
        RemoteWatch {
            requests,
            verdict,
            outstanding: None,
        }
    }

    fn begin(
        &mut self,
        timeout: std::time::Duration,
        signal: Arc<crate::endpoint::WakeSignal>,
        since: Option<u64>,
    ) -> Result<()> {
        if self.outstanding.is_some() {
            return Ok(());
        }
        *self.verdict.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.requests
            .send((timeout.min(WATCH_REQUEST_MAX), signal, since))
            .map_err(|_| anyhow!("the watch thread has ended"))?;
        self.outstanding = Some(since);
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<(bool, bool)>> {
        let answer = self
            .verdict
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match answer {
            Some(answer) => {
                self.outstanding = None;
                answer.map(Some)
            }
            None => Ok(None),
        }
    }

    /// The generation the outstanding wait was begun from, if one is out
    /// and has not answered — that is, nothing has changed on the agent's
    /// side since that generation, as far as the agent has said.
    fn standing_since(&self) -> Option<u64> {
        let answered = self
            .verdict
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        match self.outstanding {
            Some(Some(since)) if !answered => Some(since),
            _ => None,
        }
    }
}

/// Wraps a failure reported by the agent, marking it as having happened on
/// the far side.
fn remote_error(message: String) -> anyhow::Error {
    anyhow!("remote error: {message}")
}

/// Builds the error for a response that doesn't answer the request that was
/// sent.
fn unexpected_response(response: &Response, expected: &str) -> anyhow::Error {
    anyhow!(
        "protocol error: expected a {expected} response from the agent, but received a {} response",
        response_kind(response)
    )
}

/// Returns a human-readable name for a response variant.
fn response_kind(response: &Response) -> &'static str {
    match response {
        Response::Initialized => "initialized",
        Response::Scan(_) => "scan",
        Response::ScanUnchanged { .. } => "scan (unchanged)",
        Response::StageBegin(_) => "stage begin",
        Response::SupplyOpened => "supply opened",
        Response::SupplyPull(_) => "supply pull",
        Response::StagePushed => "stage pushed",
        Response::Transition { .. } => "transition",
        Response::AwaitChanges { .. } => "await changes",
        Response::Error(_) => "error",
        Response::ScanDelta(_) => "scan delta",
        Response::ScanOps(_) => "scan operations",
        Response::File(_) => "file",
        Response::Written => "written",
        Response::Lease(_) => "lease",
        Response::Recorded { .. } => "recorded",
        Response::PeeringState(_) => "peering state",
        Response::ScanProgress { .. } => "scan progress",
    }
}

/// Refuses a snapshot from the agent whose hierarchy breaks the ordering
/// and naming every merge relies on.
fn check_hierarchy(snapshot: &Snapshot) -> Result<()> {
    if let Some(root) = snapshot.root.as_ref() {
        root.validate(false).map_err(|message| {
            anyhow!("the agent's snapshot is not a valid hierarchy: {message}")
        })?;
    }
    Ok(())
}

/// The most a reassembly buffer is sized for up front. The declared length
/// is the agent's word, so beyond this the buffer grows as data arrives.
const REASSEMBLY_PREALLOCATION: u64 = 8 * 1024 * 1024;

/// Refuses a scan delta header whose values could not have come from a
/// genuine agent, before any of them sizes an allocation or a signature.
fn check_delta_header(header: &ScanDelta) -> Result<()> {
    let maximum = transport::MAXIMUM_MESSAGE_SIZE as u64;
    if header.length > maximum {
        bail!(
            "the snapshot delta's declared length ({}) exceeds the largest message ({maximum})",
            header.length
        );
    }
    let range = crate::rsync::MINIMUM_BLOCK_SIZE..=crate::rsync::MAXIMUM_BLOCK_SIZE;
    if header.baseline.is_some() && !range.contains(&header.block_size) {
        bail!(
            "the snapshot delta's block size ({}) is outside {}..={}",
            header.block_size,
            range.start(),
            range.end()
        );
    }
    Ok(())
}

/// Applies one batch of snapshot delta operations, refusing each one that
/// would take the output past the declared length before it is applied —
/// a single `Blocks` operation can otherwise expand enormously.
fn apply_delta_ops(
    base: &mut std::io::Cursor<Vec<u8>>,
    signature: &crate::rsync::Signature,
    ops: &[crate::rsync::Op],
    output: &mut Vec<u8>,
    length: u64,
) -> Result<()> {
    for op in ops {
        let adds = match op {
            crate::rsync::Op::Data(data) => data.len() as u64,
            crate::rsync::Op::Blocks { start, count } => {
                // An out-of-range run adds nothing here; `patch` refuses it.
                let blocks = signature.hashes.len() as u64;
                match start.checked_add(*count) {
                    Some(end) if *count > 0 && end <= blocks => {
                        let block_size = u64::from(signature.block_size);
                        let short = if end == blocks {
                            block_size - u64::from(signature.last_block_size)
                        } else {
                            0
                        };
                        count.saturating_mul(block_size) - short
                    }
                    _ => 0,
                }
            }
        };
        if (output.len() as u64).saturating_add(adds) > length {
            bail!("the snapshot delta reassembles to more than its declared length ({length})");
        }
        crate::rsync::patch(base, signature, op, output)
            .context("unable to apply a snapshot delta operation")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::protocol::{self, Handshake, MuxRequest, MuxResponse};
    use crate::scan::SymlinkMode;
    use crate::transport::tests::connected_pair;

    /// Builds a test initialization for the specified root, under a
    /// session identifier of the form a genuine controller sends.
    fn initialize(root: &str) -> Initialize {
        Initialize {
            root: root.into(),
            session: crate::session::session_identifier(root, "remote-test"),
            ignores: vec!["*.tmp".into()],
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
        }
    }

    /// Runs a scripted agent over one end of a connected pair: the
    /// handshake, then one canned response per request, in order, on
    /// whichever channel the request arrives. Channel opens are answered
    /// as they come — an endpoint opens two, one to work on and one to
    /// watch on — and closes are taken in stride; neither consumes a
    /// scripted response. This exercises the protocol without a
    /// `LocalEndpoint` (or a process). The first open's initialization
    /// is returned.
    fn scripted_agent(
        mut connection: Connection,
        responses: Vec<Response>,
    ) -> std::thread::JoinHandle<Result<Initialize>> {
        std::thread::spawn(move || -> Result<Initialize> {
            let peer: Handshake = connection.receive()?;
            transport::verify_handshake(&peer)?;
            connection.send(&transport::local_handshake())?;
            let mut first: Option<Initialize> = None;
            let mut responses = responses.into_iter();
            loop {
                match connection.receive()? {
                    MuxRequest::Open {
                        channel,
                        initialize,
                    } => {
                        first.get_or_insert(initialize);
                        connection.send(&MuxResponse::Response {
                            channel,
                            response: Response::Initialized,
                        })?;
                        if responses.len() == 0 {
                            break;
                        }
                    }
                    MuxRequest::Request { channel, .. } => {
                        let Some(response) = responses.next() else {
                            anyhow::bail!("a request beyond the script");
                        };
                        connection.send(&MuxResponse::Response { channel, response })?;
                        if responses.len() == 0 {
                            break;
                        }
                    }
                    MuxRequest::Close { .. } => {}
                    MuxRequest::Shutdown => break,
                }
            }
            first.ok_or_else(|| anyhow::anyhow!("no channel was opened"))
        })
    }

    #[test]
    fn remote_endpoint_proxies_requests_and_failures() {
        let (client, agent) = connected_pair();
        let agent = scripted_agent(
            agent,
            vec![
                Response::Scan(Snapshot {
                    files: 3,
                    directories: 1,
                    ..Snapshot::default()
                }),
                Response::Error("permission denied".into()),
                // An answer that doesn't correspond to the request.
                Response::StagePushed,
            ],
        );

        let mut endpoint = RemoteEndpoint::connect(client, initialize("/home/user/project"))
            .expect("unable to connect");

        // A successful exchange.
        let snapshot = endpoint.scan().expect("unable to scan");
        assert_eq!(snapshot.files, 3);
        assert_eq!(snapshot.directories, 1);

        // A remote failure surfaces as an error, without ending the session.
        let error = endpoint
            .supply_open(Vec::new())
            .expect_err("expected a remote error");
        let message = format!("{error:#}");
        assert!(
            message.contains("remote error: permission denied"),
            "unexpected error: {message}"
        );

        // A mismatched response is a protocol error.
        let error = endpoint.scan().expect_err("expected a protocol error");
        let message = format!("{error:#}");
        assert!(
            message.contains("protocol error"),
            "unexpected error: {message}"
        );
        assert!(message.contains("scan"), "unexpected error: {message}");

        drop(endpoint);
        let received = agent
            .join()
            .expect("agent thread panicked")
            .expect("agent failed");
        assert_eq!(received.root, "/home/user/project");
        assert_eq!(
            received.session,
            crate::session::session_identifier("/home/user/project", "remote-test")
        );
        assert_eq!(received.ignores, vec!["*.tmp".to_owned()]);
    }

    #[test]
    fn an_unchanged_scan_reuses_the_cached_snapshot() {
        let (client, agent) = connected_pair();
        let snapshot = Snapshot {
            files: 7,
            ..Snapshot::default()
        };
        let agent = scripted_agent(
            agent,
            vec![
                Response::Scan(snapshot.clone()),
                Response::ScanUnchanged { generation: 1 },
                Response::ScanUnchanged { generation: 1 },
            ],
        );
        let mut endpoint =
            RemoteEndpoint::connect(client, initialize("/root")).expect("unable to connect");
        let first = endpoint.scan().expect("the first scan should succeed");
        assert_eq!(first.files, 7);
        // Two unchanged reports reproduce the same snapshot without the
        // agent resending it.
        for _ in 0..2 {
            assert_eq!(endpoint.scan().expect("scan should succeed").files, 7);
        }
        drop(endpoint);
        agent.join().expect("agent thread panicked").expect("agent");
    }

    #[test]
    fn an_unchanged_report_without_a_cached_snapshot_is_an_error() {
        let (client, agent) = connected_pair();
        let agent = scripted_agent(agent, vec![Response::ScanUnchanged { generation: 1 }]);
        let mut endpoint =
            RemoteEndpoint::connect(client, initialize("/root")).expect("unable to connect");
        // Reporting "unchanged" before anything was sent means the two
        // sides disagree about what was transmitted; that must be loud
        // rather than silently resolved by rescanning.
        let error = format!("{:#}", endpoint.scan().expect_err("the scan must fail"));
        assert!(error.contains("unchanged scan before"), "{error}");
        drop(endpoint);
        let _ = agent.join();
    }

    #[test]
    fn a_scan_answered_whole_with_an_invalid_hierarchy_is_refused() {
        let link = |name: &str| Node {
            name: name.into(),
            content: crate::tree::Content::Symlink {
                target: "elsewhere".into(),
            },
        };
        let unsorted = Snapshot {
            root: Some(Node {
                name: String::new(),
                content: crate::tree::Content::Directory(std::sync::Arc::new(vec![
                    link("b"),
                    link("a"),
                ])),
            }),
            ..Snapshot::default()
        };
        let (client, agent) = connected_pair();
        let agent = scripted_agent(
            agent,
            vec![
                Response::Scan(unsorted),
                Response::ScanUnchanged { generation: 1 },
            ],
        );
        let mut endpoint =
            RemoteEndpoint::connect(client, initialize("/root")).expect("unable to connect");
        let error = format!("{:#}", endpoint.scan().expect_err("the scan must fail"));
        assert!(error.contains("not a valid hierarchy"), "{error}");
        // Nor is it kept as the baseline an unchanged report reproduces.
        let error = format!("{:#}", endpoint.scan().expect_err("nothing was kept"));
        assert!(error.contains("unchanged scan before"), "{error}");
        drop(endpoint);
        let _ = agent.join();
    }

    /// A snapshot small enough to script, with a file so its encoding
    /// spans more than one block.
    fn delta_fixture() -> (Snapshot, Vec<u8>) {
        let snapshot = Snapshot {
            files: 9,
            directories: 2,
            ..Snapshot::default()
        };
        let encoded = transport::encode_snapshot(&snapshot).expect("encodes");
        (snapshot, encoded)
    }

    /// A full (baseline-free) scan delta carrying the encoding whole.
    fn full_delta(encoded: &[u8]) -> Vec<Response> {
        vec![
            Response::ScanDelta(ScanDelta {
                baseline: None,
                digest: *blake3::hash(encoded).as_bytes(),
                length: encoded.len() as u64,
                block_size: 0,
                generation: 1,
            }),
            Response::ScanOps(vec![crate::rsync::Op::Data(encoded.to_vec())]),
            Response::ScanOps(Vec::new()),
        ]
    }

    #[test]
    fn a_scan_delta_declaring_an_impossible_length_is_refused() {
        let (client, agent) = connected_pair();
        let agent = scripted_agent(
            agent,
            vec![Response::ScanDelta(ScanDelta {
                baseline: None,
                digest: [0; 32],
                length: u64::MAX,
                block_size: 0,
                generation: 1,
            })],
        );
        let mut endpoint =
            RemoteEndpoint::connect(client, initialize("/root")).expect("unable to connect");
        // Refused from the header alone, before any allocation sized by it
        // (which would abort the process) and before any operation is pulled.
        let error = format!("{:#}", endpoint.scan().expect_err("the scan must fail"));
        assert!(error.contains("declared length"), "{error}");
        drop(endpoint);
        let _ = agent.join();
    }

    #[test]
    fn a_scan_delta_with_an_out_of_range_block_size_is_refused() {
        let (snapshot, encoded) = delta_fixture();
        for block_size in [0, 1, u32::MAX] {
            let (client, agent) = connected_pair();
            // The first scan establishes the baseline. The delta against
            // it names a block size outside the rsync module's range; it
            // must be refused before a single operation is pulled, so the
            // next request is the fallback's full scan, answered in full.
            // Had the header been accepted, the endpoint's pull would have
            // been answered by the full stream's header, a protocol error.
            let mut script = vec![
                Response::Scan(snapshot.clone()),
                Response::ScanDelta(ScanDelta {
                    baseline: Some(*blake3::hash(&encoded).as_bytes()),
                    digest: *blake3::hash(&encoded).as_bytes(),
                    length: encoded.len() as u64,
                    block_size,
                    generation: 1,
                }),
            ];
            script.extend(full_delta(&encoded));
            let agent = scripted_agent(agent, script);
            let mut endpoint =
                RemoteEndpoint::connect(client, initialize("/root")).expect("unable to connect");
            endpoint.scan().expect("the first scan should succeed");
            let second = endpoint
                .scan()
                .unwrap_or_else(|error| panic!("block size {block_size}: {error:#}"));
            assert_eq!(second.files, 9);
            drop(endpoint);
            agent.join().expect("agent thread panicked").expect("agent");
        }
    }

    #[test]
    fn delta_operations_that_expand_past_the_declared_length_are_refused_before_applying() {
        let base = vec![7u8; 64 * 1024];
        let signature = crate::rsync::signature(std::io::Cursor::new(&base), 1024).unwrap();
        let blocks = signature.hashes.len() as u64;
        let mut cursor = std::io::Cursor::new(base.clone());
        // One batch: the whole base, many times over, against a declared
        // length of one copy and a little.
        let ops = vec![
            crate::rsync::Op::Blocks {
                start: 0,
                count: blocks,
            };
            1000
        ];
        let length = base.len() as u64 + 10;
        let mut output = Vec::new();
        let error = apply_delta_ops(&mut cursor, &signature, &ops, &mut output, length)
            .expect_err("the batch must be refused");
        assert!(
            format!("{error:#}").contains("declared length"),
            "{error:#}"
        );
        assert!(
            output.len() as u64 <= length,
            "the refused operation was applied ({} bytes)",
            output.len()
        );

        // Data counts too, and an exact fit is accepted.
        let mut output = Vec::new();
        let exact = vec![
            crate::rsync::Op::Blocks {
                start: 0,
                count: blocks,
            },
            crate::rsync::Op::Data(vec![1; 10]),
        ];
        apply_delta_ops(&mut cursor, &signature, &exact, &mut output, length)
            .expect("an exact fit is accepted");
        assert_eq!(output.len() as u64, length);
        let error = apply_delta_ops(
            &mut cursor,
            &signature,
            &[crate::rsync::Op::Data(vec![1])],
            &mut output,
            length,
        )
        .expect_err("one byte more is refused");
        assert!(
            format!("{error:#}").contains("declared length"),
            "{error:#}"
        );
        assert_eq!(output.len() as u64, length);
    }

    #[test]
    fn staging_pushes_pipeline_with_a_bounded_window() {
        let (client, agent) = connected_pair();
        // Six pushes, all acknowledged positively.
        let agent = scripted_agent(agent, vec![Response::StagePushed; 6]);
        let mut endpoint =
            RemoteEndpoint::connect(client, initialize("/root")).expect("unable to connect");
        for _ in 0..6 {
            endpoint
                .stage_push_nowait(Vec::new())
                .expect("push should send");
        }
        endpoint.stage_finish().expect("all pushes should complete");
        drop(endpoint);
        agent.join().expect("agent thread panicked").expect("agent");
    }

    #[test]
    fn a_failed_push_surfaces_no_later_than_the_transition() {
        let (client, agent) = connected_pair();
        let agent = scripted_agent(
            agent,
            vec![
                Response::StagePushed,
                Response::Error("disk full".into()),
                Response::StagePushed,
                Response::Transition {
                    outcome: TransitionOutcome {
                        results: Vec::new(),
                        problems: Vec::new(),
                        missing_staged_files: false,
                        missing_staged: Vec::new(),
                    },
                    generation: 2,
                },
            ],
        );
        let mut endpoint =
            RemoteEndpoint::connect(client, initialize("/root")).expect("unable to connect");
        // The window lets these sends succeed before their acks arrive, and
        // stage_finish leaves the acks owed for the transition to collect;
        // the failure must surface by the time the transition answers.
        let mut failed = None;
        for _ in 0..3 {
            if let Err(error) = endpoint.stage_push_nowait(Vec::new()) {
                failed = Some(error);
                break;
            }
        }
        let error = match failed {
            Some(error) => error,
            None => {
                endpoint
                    .stage_finish()
                    .expect("staging completes without waiting");
                endpoint
                    .transition(Vec::new())
                    .expect_err("the failed push must surface at the transition")
            }
        };
        assert!(
            format!("{error:#}").contains("disk full"),
            "unexpected error: {error:#}"
        );
        // The failure must leave no acknowledgements queued on the channel:
        // a later request would otherwise consume a stale staging response.
        assert_eq!(endpoint.pending_pushes, 0);
        drop(endpoint);
        let _ = agent.join();
    }

    #[test]
    fn owed_push_acknowledgements_are_collected_before_any_other_request() {
        let (client, agent) = connected_pair();
        let snapshot = Snapshot {
            files: 3,
            ..Snapshot::default()
        };
        let agent = scripted_agent(
            agent,
            vec![
                Response::StagePushed,
                Response::StagePushed,
                Response::Scan(snapshot),
            ],
        );
        let mut endpoint =
            RemoteEndpoint::connect(client, initialize("/root")).expect("unable to connect");
        for _ in 0..2 {
            endpoint.stage_push_nowait(Vec::new()).expect("push");
        }
        endpoint
            .stage_finish()
            .expect("staging completes without waiting");
        assert_eq!(endpoint.pending_pushes, 2);
        // A scan's answer is the scan's, not a push's.
        assert_eq!(endpoint.scan().expect("scan").files, 3);
        assert_eq!(endpoint.pending_pushes, 0);
        drop(endpoint);
        let _ = agent.join();
    }

    /// A staging request sent without waiting answers first on the channel,
    /// ahead of the acknowledgements of the pushes sent behind it. Whether
    /// the answer is met by the push window's drain or by
    /// `stage_begin_finish`, it is the request's, and every push is still
    /// acknowledged.
    #[test]
    fn a_staging_answer_is_kept_apart_from_push_acknowledgements() {
        let (client, agent) = connected_pair();
        let agent = scripted_agent(
            agent,
            vec![
                Response::StageBegin(Vec::new()),
                Response::StagePushed,
                Response::StagePushed,
                Response::StagePushed,
                Response::StagePushed,
                Response::StagePushed,
            ],
        );
        let mut endpoint =
            RemoteEndpoint::connect(client, initialize("/root")).expect("unable to connect");
        assert!(endpoint
            .stage_begin_nowait(Vec::new())
            .expect("sent")
            .is_none());
        // Five pushes overflow the window, so a drain meets the answer.
        for _ in 0..5 {
            endpoint.stage_push_nowait(Vec::new()).expect("push");
        }
        let needs = endpoint.stage_begin_finish().expect("the answer");
        assert!(needs.is_empty());
        endpoint.stage_finish().expect("finish");
        // Everything owed is collected by the next exchange.
        assert!(
            endpoint.stage_begin_finish().is_err(),
            "nothing is pending now"
        );
        drop(endpoint);
        let _ = agent.join();
    }

    #[test]
    fn a_watch_answers_on_its_own_channel() {
        let (client, agent) = connected_pair();
        let agent = scripted_agent(
            agent,
            vec![Response::AwaitChanges {
                changed: true,
                watching: true,
            }],
        );
        let mut endpoint =
            RemoteEndpoint::connect(client, initialize("/root")).expect("unable to connect");
        let signal = Arc::new(crate::endpoint::WakeSignal::default());
        endpoint
            .watch_begin(std::time::Duration::from_secs(5), Arc::clone(&signal))
            .expect("watch begins");
        signal.wait(std::time::Duration::from_secs(5));
        assert_eq!(endpoint.watch_poll().expect("watch polls"), Some(true));
        // Ended and collected: the next poll has nothing outstanding.
        assert_eq!(endpoint.watch_poll().expect("watch polls"), None);
        drop(endpoint);
        let _ = agent.join();
    }

    #[test]
    fn remote_endpoint_rejects_version_mismatches() {
        let (client, mut agent) = connected_pair();
        let agent = std::thread::spawn(move || -> Result<()> {
            let _: Handshake = agent.receive()?;
            agent.send(&Handshake {
                magic: protocol::MAGIC,
                version: "0.0.0-ancient".into(),
            })?;
            Ok(())
        });

        let error = RemoteEndpoint::connect(client, initialize("/root"))
            .err()
            .expect("expected a version rejection");
        let message = format!("{error:#}");
        assert!(
            message.contains("version mismatch") && message.contains("0.0.0-ancient"),
            "unexpected error: {message}"
        );

        agent
            .join()
            .expect("agent thread panicked")
            .expect("agent failed");
    }

    #[test]
    fn remote_endpoint_reports_initialization_failures() {
        let (client, mut agent) = connected_pair();
        let agent = std::thread::spawn(move || -> Result<()> {
            let _: Handshake = agent.receive()?;
            agent.send(&transport::local_handshake())?;
            let MuxRequest::Open { channel, .. } = agent.receive()? else {
                anyhow::bail!("expected a channel open");
            };
            agent.send(&MuxResponse::Response {
                channel,
                response: Response::Error("no such directory".into()),
            })?;
            Ok(())
        });

        let error = RemoteEndpoint::connect(client, initialize("/missing"))
            .err()
            .expect("expected an initialization failure");
        assert!(
            format!("{error:#}").contains("remote error: no such directory"),
            "unexpected error: {error:#}"
        );

        agent
            .join()
            .expect("agent thread panicked")
            .expect("agent failed");
    }
}
