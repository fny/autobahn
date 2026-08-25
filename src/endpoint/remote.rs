//! The remote endpoint: a client proxy speaking the agent protocol over a
//! transport byte stream.
//!
//! Every [`Endpoint`] method is a synchronous exchange: one request frame out,
//! one response frame back. The agent answers request-level failures with
//! [`Response::Error`], which becomes an ordinary error here (prefixed to
//! keep the far side's message distinguishable from local failures), while a
//! response that doesn't correspond to the request is a protocol error —
//! version skew that slipped past the handshake, or a desynchronized stream.

use anyhow::{anyhow, Context, Result};

use super::{Endpoint, FileRequest, StagingNeed, TransferFrame, TransitionOutcome};
use crate::protocol::{Handshake, Initialize, Request, Response};
use crate::transport::{self, Connection};
use crate::tree::{Change, Snapshot};

/// A remote endpoint backed by an agent process.
///
/// Dropping the endpoint shuts the agent down (best-effort) and reaps its
/// process; [`close`](RemoteEndpoint::close) does the same with error
/// reporting for callers that want it.
pub struct RemoteEndpoint {
    /// The framed connection to the agent (`None` only after an explicit
    /// close, so that dropping doesn't shut down twice).
    connection: Option<Connection>,
}

impl RemoteEndpoint {
    /// Establishes a remote endpoint over the provided connection:
    /// exchanges handshakes (enforcing version equality) and initializes
    /// the agent with the session's root and policy.
    pub fn connect(connection: Connection, initialize: Initialize) -> Result<RemoteEndpoint> {
        let mut connection = connection;

        // Exchange handshakes. Ours goes out first (the agent does the same),
        // so neither side blocks waiting for the other to speak.
        connection
            .send(&transport::local_handshake())
            .context("unable to send handshake")?;
        let peer: Handshake = connection
            .receive()
            .context("unable to receive the agent's handshake")?;
        transport::verify_handshake(&peer)?;

        // Initialize the agent's endpoint.
        connection
            .send(&initialize)
            .context("unable to send initialization")?;
        let response: Response = connection
            .receive()
            .context("unable to receive the initialization response")?;
        match response {
            Response::Initialized => Ok(RemoteEndpoint {
                connection: Some(connection),
            }),
            Response::Error(message) => Err(remote_error(message)),
            response => Err(unexpected_response(&response, "initialization")),
        }
    }

    /// Terminates the endpoint, asking the agent to shut down before closing
    /// the connection (and reaping the agent process, if the connection owns
    /// one).
    pub fn close(mut self) -> Result<()> {
        let Some(mut connection) = self.connection.take() else {
            return Ok(());
        };
        // A failure here means the agent is already gone, which the close
        // below will diagnose more precisely.
        let _ = connection.send(&Request::Shutdown);
        connection.close()
    }

    /// Performs one request/response exchange, translating a remote failure
    /// into a local error. The returned response is guaranteed not to be
    /// [`Response::Error`].
    fn exchange(&mut self, request: Request) -> Result<Response> {
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| anyhow!("the endpoint has been closed"))?;
        connection
            .send(&request)
            .context("unable to send request to the agent")?;
        let response: Response = connection
            .receive()
            .context("unable to receive response from the agent")?;
        match response {
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
    let remote_command = transport::install::versioned_remote_command();
    let argv = Connection::ssh_argv(destination, Some(&remote_command));
    let attempt = || -> Result<RemoteEndpoint> {
        let connection = Connection::spawn(&argv)?;
        RemoteEndpoint::connect(connection, initialize.clone())
    };
    let initial = match attempt() {
        Ok(endpoint) => return Ok(endpoint),
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

impl Drop for RemoteEndpoint {
    fn drop(&mut self) {
        // Shut the agent down and reap it, best-effort: sessions construct
        // and drop endpoints per cycle, and every drop must leave neither a
        // running agent nor a zombie behind. Failures are ignored — there is
        // no useful way to report them from a destructor, and close()
        // remains available to callers that want them.
        if let Some(mut connection) = self.connection.take() {
            let _ = connection.send(&Request::Shutdown);
            let _ = connection.close();
        }
    }
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
        // The agent blocks for up to the requested timeout before answering,
        // so callers should keep individual awaits short and loop.
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

    use crate::protocol;
    use crate::transport::tests::connected_pair;

    /// Builds a test initialization for the specified root.
    fn initialize(root: &str) -> Initialize {
        Initialize {
            root: root.into(),
            session: "session-1".into(),
            ignores: vec!["*.tmp".into()],
            symlink_mode: crate::scan::SymlinkMode::Raw,
            file_mode: None,
            directory_mode: None,
        }
    }

    /// Runs a scripted agent over one end of a connected pair: the handshake
    /// and initialization, then one canned response per request, in order.
    /// This exercises the protocol without a `LocalEndpoint` (or a process).
    fn scripted_agent(
        mut connection: Connection,
        responses: Vec<Response>,
    ) -> std::thread::JoinHandle<Result<Initialize>> {
        std::thread::spawn(move || -> Result<Initialize> {
            let peer: Handshake = connection.receive()?;
            transport::verify_handshake(&peer)?;
            connection.send(&transport::local_handshake())?;
            let initialize: Initialize = connection.receive()?;
            connection.send(&Response::Initialized)?;
            for response in responses {
                let _: Request = connection.receive()?;
                connection.send(&response)?;
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

        let initialize = agent
            .join()
            .expect("agent thread panicked")
            .expect("agent failed");
        assert_eq!(initialize.root, "/home/user/project");
        assert_eq!(initialize.session, "session-1");
        assert_eq!(initialize.ignores, vec!["*.tmp".to_owned()]);
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
            let _: Initialize = agent.receive()?;
            agent.send(&Response::Error("no such directory".into()))?;
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
