//! The autobahn command line interface.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use autobahn::endpoint::local::LocalEndpoint;
use autobahn::endpoint::remote::RemoteEndpoint;
use autobahn::endpoint::Endpoint;
use autobahn::scan::IgnoreSet;
use autobahn::session::{session_identifier, CycleReport, Session};
use autobahn::transport::{serve_agent, Connection};
use autobahn::tree::SyncMode;

/// Fast, safe, SSH-focused bidirectional file synchronization.
#[derive(Parser)]
#[command(name = "autobahn", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The synchronization mode, as expressed on the command line.
#[derive(Clone, Copy, ValueEnum)]
enum ModeArgument {
    /// Bidirectional; conflicts are reported, not resolved.
    TwoWaySafe,
    /// Bidirectional; conflicts resolve in alpha's favor.
    TwoWayResolved,
    /// Alpha to beta; beta-side changes are preserved.
    OneWaySafe,
    /// Alpha to beta; beta exactly mirrors alpha.
    OneWayReplica,
}

impl From<ModeArgument> for SyncMode {
    fn from(argument: ModeArgument) -> SyncMode {
        match argument {
            ModeArgument::TwoWaySafe => SyncMode::TwoWaySafe,
            ModeArgument::TwoWayResolved => SyncMode::TwoWayResolved,
            ModeArgument::OneWaySafe => SyncMode::OneWaySafe,
            ModeArgument::OneWayReplica => SyncMode::OneWayReplica,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Synchronize two roots: a local alpha and a local or remote beta.
    ///
    /// Beta accepts a local path or an scp-style remote specification
    /// ([user@]host:path), which connects over SSH and requires autobahn (of
    /// the same version) to be installed on the remote host.
    Sync {
        /// The alpha synchronization root (a local path).
        alpha: String,
        /// The beta synchronization root (a local path or [user@]host:path).
        beta: String,
        /// The synchronization mode.
        #[arg(long, value_enum, default_value = "two-way-safe")]
        mode: ModeArgument,
        /// Ignore patterns (gitignore-style; repeatable).
        #[arg(long = "ignore")]
        ignores: Vec<String>,
        /// Keep running, synchronizing whenever content changes.
        #[arg(long)]
        watch: bool,
        /// The polling interval, in seconds, used with --watch.
        #[arg(long, default_value_t = 5)]
        interval: u64,
        /// Override the session state directory (defaults to
        /// ~/.autobahn/sessions/<session-id>).
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Advanced: connect beta through this agent command (whitespace
        /// split into argv) instead of SSH, treating BETA as the remote
        /// root path. Used for testing and custom transports.
        #[arg(long)]
        beta_agent: Option<String>,
    },
    /// Run as a synchronization agent on standard input/output (invoked on
    /// remote hosts by the sync command; not intended for interactive use).
    Agent,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Agent => serve_agent(std::io::stdin().lock(), std::io::stdout().lock()),
        Command::Sync {
            alpha,
            beta,
            mode,
            ignores,
            watch,
            interval,
            state_dir,
            beta_agent,
        } => run_sync(
            alpha, beta, mode, ignores, watch, interval, state_dir, beta_agent,
        ),
    };
    if let Err(error) = result {
        eprintln!("autobahn: {error:#}");
        std::process::exit(1);
    }
}

/// Parses an scp-style beta specification into (host, path) if it denotes a
/// remote root. A specification is remote when it contains a colon before
/// any slash (so relative and absolute local paths are never misparsed).
fn parse_remote(beta: &str) -> Option<(&str, &str)> {
    let colon = beta.find(':')?;
    if let Some(slash) = beta.find('/') {
        if slash < colon {
            return None;
        }
    }
    Some((&beta[..colon], &beta[colon + 1..]))
}

#[allow(clippy::too_many_arguments)]
fn run_sync(
    alpha: String,
    beta: String,
    mode: ModeArgument,
    ignores: Vec<String>,
    watch: bool,
    interval: u64,
    state_dir: Option<PathBuf>,
    beta_agent: Option<String>,
) -> Result<()> {
    // Resolve the alpha root.
    let alpha_root = PathBuf::from(&alpha);
    let alpha_canonical = alpha_root
        .canonicalize()
        .with_context(|| format!("unable to resolve alpha root {alpha}"))?;

    // Compute the session identity and state directory.
    let identifier = session_identifier(&alpha_canonical.to_string_lossy(), &beta);
    let state_directory = match state_dir {
        Some(directory) => directory,
        None => {
            let home = std::env::var("HOME").context("HOME is not set")?;
            PathBuf::from(home)
                .join(".autobahn")
                .join("sessions")
                .join(&identifier)
        }
    };

    // Construct the alpha endpoint.
    let ignore_set = IgnoreSet::new(&ignores)?;
    let alpha_endpoint: Box<dyn Endpoint + Send> = Box::new(LocalEndpoint::new(
        alpha_canonical,
        state_directory.join("staging-alpha"),
        ignore_set,
    )?);

    // Construct the beta endpoint: an agent connection (SSH or explicit
    // command) for remote specifications, a local endpoint otherwise.
    let beta_endpoint: Box<dyn Endpoint + Send> = if let Some(command) = beta_agent {
        let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
        if argv.is_empty() {
            bail!("empty beta agent command");
        }
        let connection = Connection::spawn(&argv)?;
        Box::new(RemoteEndpoint::connect(
            connection,
            beta.clone(),
            identifier.clone(),
            ignores.clone(),
        )?)
    } else if let Some((host, path)) = parse_remote(&beta) {
        let argv = Connection::ssh_argv(host, None);
        let connection = Connection::spawn(&argv)?;
        Box::new(RemoteEndpoint::connect(
            connection,
            path.to_owned(),
            identifier.clone(),
            ignores.clone(),
        )?)
    } else {
        let beta_root = PathBuf::from(&beta)
            .canonicalize()
            .with_context(|| format!("unable to resolve beta root {beta}"))?;
        Box::new(LocalEndpoint::new(
            beta_root,
            state_directory.join("staging-beta"),
            IgnoreSet::new(&ignores)?,
        )?)
    };

    // Create the session and run.
    let mut session = Session::new(alpha_endpoint, beta_endpoint, mode.into(), state_directory)?;
    loop {
        let report = session.run_cycle()?;
        print_report(&report);
        if report.missing_staged_files {
            // Concurrent modification during staging: run a follow-up cycle
            // immediately.
            continue;
        }
        if !watch {
            break;
        }
        std::thread::sleep(Duration::from_secs(interval.max(1)));
    }
    Ok(())
}

/// Prints a cycle report to standard output/error.
fn print_report(report: &CycleReport) {
    if report.changed() {
        println!(
            "synchronized: {} change(s) to alpha, {} change(s) to beta",
            report.alpha_transitions, report.beta_transitions
        );
    }
    for conflict in &report.conflicts {
        eprintln!("conflict at {:?} (left unresolved)", conflict.root);
    }
    for problem in report
        .alpha_scan_problems
        .iter()
        .chain(&report.alpha_transition_problems)
    {
        eprintln!("alpha problem at {:?}: {}", problem.path, problem.message);
    }
    for problem in report
        .beta_scan_problems
        .iter()
        .chain(&report.beta_transition_problems)
    {
        eprintln!("beta problem at {:?}: {}", problem.path, problem.message);
    }
}

#[cfg(test)]
mod tests {
    use super::parse_remote;

    #[test]
    fn remote_specification_parsing() {
        assert_eq!(parse_remote("host:path"), Some(("host", "path")));
        assert_eq!(
            parse_remote("user@host:/absolute/path"),
            Some(("user@host", "/absolute/path"))
        );
        assert_eq!(parse_remote("/local/path"), None);
        assert_eq!(parse_remote("relative/path:with-colon"), None);
        assert_eq!(parse_remote("plain"), None);
    }
}
