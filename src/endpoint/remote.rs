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

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};

use super::{Endpoint, FileRequest, StagingNeed, TransferFrame, TransitionOutcome};
use crate::protocol::{Initialize, Request, Response, ScanDelta};
use crate::transport::mux::{AgentChannel, AgentConnection, AgentPool};
use crate::transport::{self, Connection};
use crate::tree::{Change, Snapshot};

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
    /// A remote scan happens inside a single request on the far side, so
    /// there is nothing to count while it runs: the guard reports that the
    /// side is scanning and how long it has been, and the completed
    /// snapshot's own total is left behind for the next scan to be
    /// measured against. Live counts would need the agent to report them
    /// mid-request, which the protocol does not carry.
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
        match self.exchange(request)? {
            Response::Scan(snapshot) => {
                self.last_snapshot = Some(snapshot.clone());
                Ok(snapshot)
            }
            Response::ScanDelta(header) => self.receive_snapshot(header, what),
            // The agent reports "unchanged" only against a snapshot it has
            // actually sent, so having nothing to reproduce means the two
            // sides disagree about what was transmitted. That is a protocol
            // defect, and silently rescanning would paper over it.
            Response::ScanUnchanged => self
                .last_snapshot
                .clone()
                .ok_or_else(|| anyhow!("the agent reported an unchanged scan before sending one")),
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

        let mut output = Vec::with_capacity(header.length as usize);
        let mut base = Cursor::new(base);
        loop {
            let ops = match self.exchange(Request::ScanPull)? {
                Response::ScanOps(ops) => ops,
                response => return Err(unexpected_response(&response, "scan pull")),
            };
            if ops.is_empty() {
                break;
            }
            for op in &ops {
                crate::rsync::patch(&mut base, &signature, op, &mut output)
                    .context("unable to apply a snapshot delta operation")?;
            }
            if output.len() as u64 > header.length {
                bail!("the snapshot delta reassembled to more than its declared length");
            }
        }
        if *blake3::hash(&output).as_bytes() != header.digest {
            bail!("the reassembled snapshot does not match the agent's digest");
        }
        let snapshot: Snapshot =
            bincode::deserialize(&output).context("unable to decode the reassembled snapshot")?;
        if let Some(root) = snapshot.root.as_ref() {
            root.validate(false).map_err(|message| {
                anyhow!("the reassembled snapshot is not a valid hierarchy: {message}")
            })?;
        }
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
        let channel = connection.open(initialize)?;
        Ok(RemoteEndpoint::from_channel(channel))
    }

    /// Wraps an already-open channel (the pooled path).
    pub(crate) fn from_channel(channel: AgentChannel) -> RemoteEndpoint {
        RemoteEndpoint {
            channel,
            pending_pushes: 0,
            last_snapshot: None,
            progress: None,
        }
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
    let channel = connection.open(initialize)?;
    Ok(RemoteEndpoint::from_channel(channel))
}

/// Establishes a multiplexed connection to a remote SSH host, with the
/// install-and-retry behavior of [`connect_ssh`].
fn establish_ssh(destination: &str) -> Result<AgentConnection> {
    use anyhow::Context;
    let remote_command = transport::install::versioned_remote_command();
    let argv = Connection::ssh_argv(destination, Some(&remote_command));
    let attempt = |quiet: bool| -> Result<AgentConnection> {
        let connection = if quiet {
            Connection::spawn_quiet(&argv)?
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
    transport::install::ensure_agent(destination).map_err(|error| {
        anyhow::Error::new(Unreachable {
            destination: destination.to_owned(),
        })
        .context(format!("{error:#}"))
    })?;
    attempt(false).with_context(|| {
        format!(
            "unable to connect to {destination} even after installing the agent \
             (initial failure: {initial:#})"
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
    let channel = pool.channel(argv, initialize, || match destination {
        // The SSH path installs the agent on first contact; under the
        // pool's per-key lock, concurrent sessions for one host wait for
        // this single bootstrap instead of racing their own.
        Some(destination) => establish_ssh(destination),
        None => AgentConnection::connect(Connection::spawn(argv)?),
    })?;
    Ok(RemoteEndpoint::from_channel(channel))
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

    fn write_file(&mut self, path: &str, content: Option<&[u8]>) -> Result<()> {
        match self.exchange(Request::WriteFile(
            path.to_owned(),
            content.map(|c| c.to_vec()),
        ))? {
            Response::Written => Ok(()),
            response => Err(unexpected_response(&response, "write file")),
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
        self.drain_pushes_to(0)
    }

    fn transition(&mut self, transitions: Vec<Change>) -> Result<TransitionOutcome> {
        // The transitions are cloned because the fold below needs them
        // after the request has consumed them.
        match self.exchange(Request::Transition(transitions.clone()))? {
            Response::Transition(outcome) => {
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
        match self.exchange(Request::AwaitChanges(timeout.as_millis() as u64))? {
            Response::AwaitChanges(changed) => Ok(changed),
            response => Err(unexpected_response(&response, "await changes")),
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
        Response::ScanUnchanged => "scan (unchanged)",
        Response::StageBegin(_) => "stage begin",
        Response::SupplyOpened => "supply opened",
        Response::SupplyPull(_) => "supply pull",
        Response::StagePushed => "stage pushed",
        Response::Transition(_) => "transition",
        Response::AwaitChanges(_) => "await changes",
        Response::Error(_) => "error",
        Response::ScanDelta(_) => "scan delta",
        Response::ScanOps(_) => "scan operations",
        Response::File(_) => "file",
        Response::Written => "written",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::protocol::{self, Handshake, MuxRequest, MuxResponse};
    use crate::scan::SymlinkMode;
    use crate::transport::tests::connected_pair;

    /// Builds a test initialization for the specified root.
    fn initialize(root: &str) -> Initialize {
        Initialize {
            root: root.into(),
            session: "session-1".into(),
            ignores: vec!["*.tmp".into()],
            symlink_mode: SymlinkMode::Raw,
            file_mode: None,
            directory_mode: None,
            side: "beta".into(),
            staging: Default::default(),
            max_file_size: None,
            max_entry_count: None,
            default_owner: None,
            default_group: None,
        }
    }

    /// Runs a scripted agent over one end of a connected pair: the
    /// handshake, a channel open, then one canned response per request, in
    /// order. This exercises the protocol without a `LocalEndpoint` (or a
    /// process).
    fn scripted_agent(
        mut connection: Connection,
        responses: Vec<Response>,
    ) -> std::thread::JoinHandle<Result<Initialize>> {
        std::thread::spawn(move || -> Result<Initialize> {
            let peer: Handshake = connection.receive()?;
            transport::verify_handshake(&peer)?;
            connection.send(&transport::local_handshake())?;
            let MuxRequest::Open {
                channel,
                initialize,
            } = connection.receive()?
            else {
                anyhow::bail!("expected a channel open");
            };
            connection.send(&MuxResponse {
                channel,
                response: Response::Initialized,
            })?;
            for response in responses {
                let MuxRequest::Request { channel, .. } = connection.receive()? else {
                    anyhow::bail!("expected a channel request");
                };
                connection.send(&MuxResponse { channel, response })?;
            }
            Ok(initialize)
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
        assert_eq!(received.session, "session-1");
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
                Response::ScanUnchanged,
                Response::ScanUnchanged,
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
        let agent = scripted_agent(agent, vec![Response::ScanUnchanged]);
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
    fn a_failed_push_surfaces_no_later_than_stage_finish() {
        let (client, agent) = connected_pair();
        let agent = scripted_agent(
            agent,
            vec![
                Response::StagePushed,
                Response::Error("disk full".into()),
                Response::StagePushed,
            ],
        );
        let mut endpoint =
            RemoteEndpoint::connect(client, initialize("/root")).expect("unable to connect");
        // The window lets these sends succeed before their acks arrive; the
        // failure must surface by the time staging completes.
        let mut failed = None;
        for _ in 0..3 {
            if let Err(error) = endpoint.stage_push_nowait(Vec::new()) {
                failed = Some(error);
                break;
            }
        }
        let error = match failed {
            Some(error) => error,
            None => endpoint
                .stage_finish()
                .expect_err("the failed push must surface"),
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
            agent.send(&MuxResponse {
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
