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

use anyhow::{anyhow, Result};

use super::{Endpoint, FileRequest, StagingNeed, TransferFrame, TransitionOutcome};
use crate::protocol::{Initialize, Request, Response};
use crate::transport::mux::{AgentChannel, AgentConnection, AgentPool};
use crate::transport::{self, Connection};
use crate::tree::{Change, Snapshot};

/// A remote endpoint backed by one channel of an agent connection.
///
/// Dropping the endpoint closes its channel; when that channel was the
/// connection's last, the connection shuts the agent down and reaps its
/// process.
pub struct RemoteEndpoint {
    /// The endpoint's channel.
    channel: AgentChannel,
}

impl RemoteEndpoint {
    /// Establishes a remote endpoint as the sole session over a dedicated
    /// connection: exchanges handshakes (enforcing version equality) and
    /// opens one channel with the session's root and policy.
    pub fn connect(connection: Connection, initialize: Initialize) -> Result<RemoteEndpoint> {
        let connection = AgentConnection::connect(connection)?;
        let channel = connection.open(initialize)?;
        Ok(RemoteEndpoint { channel })
    }

    /// Wraps an already-open channel (the pooled path).
    pub(crate) fn from_channel(channel: AgentChannel) -> RemoteEndpoint {
        RemoteEndpoint { channel }
    }

    /// Terminates the endpoint, reporting failures that the silent drop
    /// path would swallow: an unsendable close, a shutdown the agent
    /// ignored, or a non-successful agent exit.
    pub fn close(self) -> Result<()> {
        self.channel.close()
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
    let attempt =
        || -> Result<AgentConnection> { AgentConnection::connect(Connection::spawn(&argv)?) };
    let initial = match attempt() {
        Ok(connection) => return Ok(connection),
        Err(error) => error,
    };
    // The versioned agent is missing or unusable; install it and retry
    // once. (If the failure was something else — authentication, an
    // unreachable host — installation fails the same way and both failures
    // surface together.)
    transport::install::ensure_agent(destination).with_context(|| {
        format!("unable to connect to {destination} ({initial:#}), and agent installation failed")
    })?;
    attempt().with_context(|| {
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
    fn scan(&mut self) -> Result<Snapshot> {
        match self.exchange(Request::Scan)? {
            Response::Scan(snapshot) => Ok(snapshot),
            response => Err(unexpected_response(&response, "scan")),
        }
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

    fn transition(&mut self, transitions: Vec<Change>) -> Result<TransitionOutcome> {
        match self.exchange(Request::Transition(transitions))? {
            Response::Transition(outcome) => Ok(outcome),
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
        Response::StageBegin(_) => "stage begin",
        Response::SupplyOpened => "supply opened",
        Response::SupplyPull(_) => "supply pull",
        Response::StagePushed => "stage pushed",
        Response::Transition(_) => "transition",
        Response::AwaitChanges(_) => "await changes",
        Response::Error(_) => "error",
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
