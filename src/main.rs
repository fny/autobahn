//! The autobahn command line interface.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use autobahn::config::Config;
use autobahn::endpoint::local::LocalEndpoint;
use autobahn::endpoint::remote::RemoteEndpoint;
use autobahn::endpoint::Endpoint;
use autobahn::paths;
use autobahn::scan::IgnoreSet;
use autobahn::session::{session_identifier, CycleReport, Session};
use autobahn::supervisor::{read_status, SessionStatus, Supervisor};
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
    /// Run every session the groups configuration describes, supervising
    /// them continuously (or once with --once).
    ///
    /// The configuration fans groups of one local alpha directory out to
    /// any number of local or remote betas; see the documentation for the
    /// format. Sessions run in parallel, and a session whose destination is
    /// unreachable backs off and heals automatically — it never blocks the
    /// others.
    Up {
        /// The configuration file (defaults to
        /// $XDG_CONFIG_HOME/autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Run a single pass over every session and exit (non-zero if any
        /// session failed) instead of supervising continuously.
        #[arg(long)]
        once: bool,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Show the recorded status of every configured session, grouped by
    /// group (optionally filtered by group and host).
    Status {
        /// The configuration file (defaults to
        /// $XDG_CONFIG_HOME/autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
        /// Filter to a group.
        group: Option<String>,
        /// Filter to a destination host (or local beta path) within the
        /// group.
        host: Option<String>,
    },
    /// Run as a synchronization agent on standard input/output (invoked on
    /// remote hosts by the sync command; not intended for interactive use).
    Agent,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Agent => serve_agent(std::io::stdin().lock(), std::io::stdout().lock()),
        Command::Up {
            config,
            once,
            state_root,
        } => run_up(config, once, state_root),
        Command::Status {
            config,
            state_root,
            group,
            host,
        } => run_status(config, state_root, group, host),
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

/// Loads the groups configuration from an explicit or default path.
fn load_config(path: Option<PathBuf>) -> Result<Config> {
    let path = match path {
        Some(path) => path,
        None => paths::default_config_path()?,
    };
    Config::load(&path)
}

/// Resolves the state root from an explicit override or the default.
fn resolve_state_root(state_root: Option<PathBuf>) -> Result<PathBuf> {
    match state_root {
        Some(root) => Ok(root),
        None => paths::default_state_root(),
    }
}

/// Runs the supervisor over the configured sessions.
fn run_up(config: Option<PathBuf>, once: bool, state_root: Option<PathBuf>) -> Result<()> {
    let plans = load_config(config)?.plans()?;
    if plans.is_empty() {
        bail!("the configuration describes no sessions");
    }
    let state_root = resolve_state_root(state_root)?;

    if once {
        let supervisor = Supervisor::new(plans, state_root, false);
        let outcomes = supervisor.run_once();
        let mut failures = 0usize;
        for outcome in &outcomes {
            match &outcome.result {
                Ok(digest) => {
                    let mut summary = format!(
                        "{} change(s) to alpha, {} change(s) to beta",
                        digest.alpha_transitions, digest.beta_transitions
                    );
                    if digest.conflicts > 0 {
                        summary.push_str(&format!(", {} conflict(s)", digest.conflicts));
                    }
                    if digest.problems > 0 {
                        summary.push_str(&format!(", {} problem(s)", digest.problems));
                    }
                    println!("[{}] synchronized: {summary}", outcome.display);
                }
                Err(error) => {
                    failures += 1;
                    eprintln!("[{}] failed: {error}", outcome.display);
                }
            }
        }
        if failures > 0 {
            bail!("{failures} session(s) failed");
        }
        return Ok(());
    }

    println!(
        "supervising {} session(s); status is available via `autobahn status`",
        plans.len()
    );
    let supervisor = Supervisor::new(plans, state_root, true);
    // Watch mode runs until the process is terminated: agent processes exit
    // when their connection streams close, so no explicit cleanup is needed.
    let stop = std::sync::atomic::AtomicBool::new(false);
    supervisor.run_watch(&stop);
    Ok(())
}

/// Shows the recorded status of the configured sessions.
fn run_status(
    config: Option<PathBuf>,
    state_root: Option<PathBuf>,
    group: Option<String>,
    host: Option<String>,
) -> Result<()> {
    let plans = load_config(config)?.plans()?;
    let state_root = resolve_state_root(state_root)?;

    let selected: Vec<_> = plans
        .iter()
        .filter(|plan| group.as_deref().is_none_or(|group| plan.group == group))
        .filter(|plan| host.as_deref().is_none_or(|host| plan.host == host))
        .collect();
    if selected.is_empty() {
        match (&group, &host) {
            (Some(group), Some(host)) => bail!("no configured session matches {group}@{host}"),
            (Some(group), None) => bail!("no configured group named '{group}'"),
            _ => bail!("the configuration describes no sessions"),
        }
    }

    let mut current_group: Option<&str> = None;
    for plan in selected {
        if current_group != Some(plan.group.as_str()) {
            if current_group.is_some() {
                println!();
            }
            current_group = Some(plan.group.as_str());
            println!("\x1b[1m{}\x1b[0m  {}", plan.group, plan.alpha_spec);
        }
        match read_status(&state_root, &plan.identifier())? {
            Some(status) => print_status_entry(plan.host.as_str(), &status),
            None => println!("  \x1b[2m{}  never run\x1b[0m", plan.host),
        }
    }
    Ok(())
}

/// Prints one session's status line (and any conflict, problem, or error
/// detail beneath it).
fn print_status_entry(host: &str, status: &SessionStatus) {
    let age = format_age(status.updated_at);
    let state = match status.state.as_str() {
        "synchronized" => status.state.clone(),
        "error" => format!("\x1b[31m{}\x1b[0m", status.state),
        _ => format!("\x1b[33m{}\x1b[0m", status.state),
    };
    println!(
        "  \x1b[1m{host}\x1b[0m  {state}  {} cycle(s)  ({age})",
        status.cycles
    );
    // For a local beta the destination is the host label itself, so a
    // detail line would just repeat it.
    if status.beta != host {
        println!("    {} [{}]", status.beta, status.mode);
    } else {
        println!("    [{}]", status.mode);
    }
    for root in &status.conflicts {
        println!("    conflict at {root:?}");
    }
    for problem in &status.problems {
        println!("    problem: {problem}");
    }
    if let Some(error) = &status.error {
        println!("    {error}");
    }
}

/// Formats the age of a status timestamp for display.
fn format_age(updated_at: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let elapsed = now.saturating_sub(updated_at);
    if elapsed < 60 {
        format!("{elapsed}s ago")
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else {
        format!("{}h ago", elapsed / 3600)
    }
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
