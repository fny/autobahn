//! The autobahn command line interface.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use autobahn::config::Config;
use autobahn::endpoint::local::{EndpointOptions, LocalEndpoint};
use autobahn::endpoint::remote::RemoteEndpoint;
use autobahn::endpoint::Endpoint;
use autobahn::paths;
use autobahn::protocol::Initialize;
use autobahn::scan::{IgnoreSet, SymlinkMode};
use autobahn::session::{session_identifier, CycleReport, Session};
use autobahn::supervisor::control::{ControlRequest, ControlResponse, Selector};
use autobahn::supervisor::{read_status, SessionStatus, Supervisor};
use autobahn::transport::{serve_agent, Connection};
use autobahn::tree::SyncMode;

/// The help styling: clap's defaults, minus the underline it puts on
/// section headings. Bold alone separates them, and underlines render
/// inconsistently across terminals — some draw them through descenders,
/// some ignore them, some use them for links.
const HELP_STYLES: clap::builder::Styles = clap::builder::Styles::styled()
    .header(clap::builder::styling::Style::new().bold())
    .usage(clap::builder::styling::Style::new().bold())
    .literal(clap::builder::styling::Style::new().bold())
    .placeholder(clap::builder::styling::Style::new());

/// Fast, safe, SSH-focused bidirectional file synchronization.
#[derive(Parser)]
#[command(name = "autobahn", version, about, styles = HELP_STYLES)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The synchronization mode, as expressed on the command line.
#[derive(Clone, Copy, ValueEnum)]
enum ModeArgument {
    /// Both directions; a file changed on both sides is a conflict,
    /// reported and left alone.
    #[value(name = "two-way-conflict", alias = "two-way-safe")]
    TwoWaySafe,
    /// Both directions; a file changed on both sides takes alpha's
    /// version, silently.
    #[value(name = "two-way-alpha", alias = "two-way-resolved")]
    TwoWayResolved,
    /// Alpha to beta; a change beta made itself is kept and reported as a
    /// conflict.
    #[value(name = "one-way-conflict", alias = "one-way-safe")]
    OneWaySafe,
    /// Alpha to beta; beta becomes an exact copy, its own changes
    /// discarded.
    #[value(name = "one-way-alpha", aliases = ["one-way-replica", "mirror"])]
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

/// The symbolic link mode, as expressed on the command line.
#[derive(Clone, Copy, ValueEnum)]
enum SymlinkModeArgument {
    /// Symbolic links are invisible to synchronization.
    Ignore,
    /// Only portable symbolic links (relative, within the root) synchronize.
    Portable,
    /// Symbolic links synchronize verbatim.
    Raw,
}

impl From<SymlinkModeArgument> for SymlinkMode {
    fn from(argument: SymlinkModeArgument) -> SymlinkMode {
        match argument {
            SymlinkModeArgument::Ignore => SymlinkMode::Ignore,
            SymlinkModeArgument::Portable => SymlinkMode::Portable,
            SymlinkModeArgument::Raw => SymlinkMode::Raw,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Synchronize two roots, each a local path or a remote specification.
    ///
    /// Either root accepts a local path or an scp-style remote specification
    /// ([user@]host:path), which connects over SSH (installing the matching
    /// agent on the remote host on first contact).
    Sync {
        /// The alpha synchronization root (a local path or [user@]host:path).
        /// With no roots at all, every session in the configuration is
        /// synchronized once instead.
        alpha: Option<String>,
        /// The beta synchronization root (a local path or [user@]host:path).
        beta: Option<String>,
        /// The configuration file, for the no-roots form (defaults to
        /// ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the state root, for the no-roots form (defaults to
        /// ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
        /// The synchronization mode.
        #[arg(long, value_enum, default_value = "two-way-conflict")]
        mode: ModeArgument,
        /// Ignore patterns (gitignore-style; repeatable).
        #[arg(long = "ignore")]
        ignores: Vec<String>,
        /// Symbolic link handling: ignore, portable, or raw.
        #[arg(long, value_enum, default_value = "raw")]
        symlink_mode: SymlinkModeArgument,
        /// Permission bits (octal) for created files (default 0600).
        #[arg(long)]
        file_mode: Option<String>,
        /// Permission bits (octal) for created directories (default 0700).
        #[arg(long)]
        directory_mode: Option<String>,
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
        /// Advanced: connect alpha through this agent command (whitespace
        /// split into argv) instead of SSH, treating ALPHA as the remote
        /// root path. Used for testing and custom transports.
        #[arg(long)]
        alpha_agent: Option<String>,
    },
    /// Run every configured session here, in this terminal, until
    /// interrupted. On a terminal the display is a live `autobahn status`;
    /// when the output is a file or a pipe, one line is logged per event.
    ///
    /// The configuration fans groups of one local alpha directory out to
    /// any number of local or remote betas; see the documentation for the
    /// format. Sessions run in parallel, and a session whose destination is
    /// unreachable backs off and heals automatically — it never blocks the
    /// others. To keep this running when no terminal is, see `install`.
    Watch {
        /// The configuration file (defaults to ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
        /// In the live display, list every conflicting path rather than a
        /// count and an example.
        #[arg(long)]
        conflicts: bool,
        /// Log one line per event even on a terminal, instead of the live
        /// display.
        #[arg(long)]
        log: bool,
    },
    /// Show the recorded status of every configured session, grouped by
    /// group (optionally filtered by group and host).
    Status {
        /// The configuration file (defaults to
        /// ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
        /// Filter to a group, by its name or by its folder — an absolute
        /// path, a `~` path, or `.` for the working directory. A folder
        /// inside a synchronized root selects the group that covers it,
        /// so `autobahn status .` answers "what syncs where I am?".
        group: Option<String>,
        /// Filter to a destination host (or local beta path) within the
        /// group.
        host: Option<String>,
        /// List every conflicting path rather than a count and an example.
        #[arg(long)]
        conflicts: bool,
    },
    /// Wake configured sessions in a running supervisor for an immediate
    /// synchronization cycle.
    Flush {
        /// Filter to a group.
        group: Option<String>,
        /// Filter to a destination within the group.
        host: Option<String>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Reset sessions in a running supervisor: their synchronization
    /// baselines are discarded, so the next cycle merges both sides
    /// additively (resurrecting deletions). The group is required — a
    /// reset is deliberate, never a default.
    Reset {
        /// The group to reset.
        group: String,
        /// Filter to a destination within the group.
        host: Option<String>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Register the supervisor as a login service — launchd on macOS, a
    /// systemd user unit on Linux — and start it now. It then runs across
    /// logouts and reboots, restarting if it exits, logging to
    /// ~/.autobahn/service.log.
    Install {
        /// Bake this configuration file into the service (defaults to
        /// ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Bake this state root into the service (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Stop the login service and unregister it.
    Uninstall,
    /// Start the installed login service. With none installed, this
    /// refuses and points at `install` (or `watch`, to run here instead).
    Start,
    /// Stop the login service. It stays registered and returns at the next
    /// login; `uninstall` makes it stay gone.
    Stop,
    /// Stop and start the login service — after a configuration edit, or an
    /// upgrade.
    Restart,
    /// Remove state left behind by sessions the configuration no longer
    /// describes: their ancestors, status records, staged content, and
    /// endpoint locks. State for a running session is never touched, and
    /// the files in the synchronized trees are never touched by anything.
    Clean {
        /// The configuration file (defaults to ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
        /// Show what would be removed without removing it.
        #[arg(long)]
        dry_run: bool,
        /// Also remove staged content this machine holds *as an agent* for
        /// sessions driven from other machines, when it has not been
        /// touched for this many days. Such content is a transfer cache
        /// whose owner cannot be identified from here, so it is left alone
        /// unless asked.
        #[arg(long, value_name = "DAYS")]
        agent_staging_older_than: Option<u64>,
    },
    /// Re-read every file's content on the sessions' next cycle, making
    /// content that changed without its metadata moving (restored
    /// timestamps, reproducible-build rewrites) visible and synchronized.
    Verify {
        /// Filter to a group (defaults to every session).
        group: Option<String>,
        /// Filter to a destination within the group.
        host: Option<String>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Run as a synchronization agent on standard input/output (invoked on
    /// remote hosts by the sync command; not intended for interactive use).
    Agent,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Agent => serve_agent(std::io::stdin().lock(), std::io::stdout()),
        Command::Watch {
            config,
            state_root,
            conflicts,
            log,
        } => run_watch(config, state_root, conflicts, log),
        Command::Install { config, state_root } => {
            autobahn::service::install(config.as_deref(), state_root.as_deref()).map(|()| {
                println!("installed and started the login service");
            })
        }
        Command::Uninstall => autobahn::service::uninstall().map(|()| {
            println!("stopped and unregistered the login service");
        }),
        Command::Start => autobahn::service::start().map(|()| {
            println!("started the login service");
        }),
        Command::Stop => autobahn::service::stop().map(|()| {
            println!(
                "stopped the login service (it returns at the next login; `uninstall` \
                 removes it)"
            );
        }),
        Command::Restart => autobahn::service::restart().map(|()| {
            println!("restarted the login service");
        }),
        Command::Status {
            config,
            state_root,
            group,
            host,
            conflicts,
        } => run_status(config, state_root, group, host, conflicts),
        Command::Flush {
            group,
            host,
            state_root,
        } => run_control(
            ControlRequest::Flush(Selector { group, host }),
            state_root,
            "flushed",
        ),
        Command::Verify {
            group,
            host,
            state_root,
        } => run_control(
            ControlRequest::Verify(Selector { group, host }),
            state_root,
            "verifying",
        ),
        Command::Reset {
            group,
            host,
            state_root,
        } => run_control(
            ControlRequest::Reset(Selector {
                group: Some(group),
                host,
            }),
            state_root,
            "reset",
        ),
        Command::Clean {
            config,
            state_root,
            dry_run,
            agent_staging_older_than,
        } => run_clean(config, state_root, dry_run, agent_staging_older_than),
        Command::Sync {
            alpha: None,
            beta: None,
            config,
            state_root,
            ..
        } => run_sync_config(config, state_root),
        Command::Sync { alpha: None, .. } | Command::Sync { beta: None, .. } => {
            Err(anyhow::anyhow!(
                "sync takes two roots, or none to synchronize every configured session once"
            ))
        }
        Command::Sync {
            alpha: Some(alpha),
            beta: Some(beta),
            config: _,
            state_root: _,
            mode,
            ignores,
            symlink_mode,
            file_mode,
            directory_mode,
            watch,
            interval,
            state_dir,
            beta_agent,
            alpha_agent,
        } => parse_policy(symlink_mode, file_mode, directory_mode).and_then(|policy| {
            run_sync(
                alpha,
                beta,
                mode,
                ignores,
                policy,
                watch,
                interval,
                state_dir,
                beta_agent,
                alpha_agent,
            )
        }),
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

/// The endpoint policy assembled from command-line arguments.
#[derive(Clone, Copy)]
struct Policy {
    /// The symbolic link treatment.
    symlink_mode: SymlinkMode,
    /// The permission bits for created files, if overridden.
    file_mode: Option<u32>,
    /// The permission bits for created directories, if overridden.
    directory_mode: Option<u32>,
}

/// Parses the policy arguments of the sync command.
fn parse_policy(
    symlink_mode: SymlinkModeArgument,
    file_mode: Option<String>,
    directory_mode: Option<String>,
) -> Result<Policy> {
    let parse = |mode: Option<String>, directory: bool| -> Result<Option<u32>> {
        mode.map(|mode| {
            autobahn::config::parse_permission_mode(&mode, directory)
                .map_err(|message| anyhow::anyhow!(message))
        })
        .transpose()
    };
    Ok(Policy {
        symlink_mode: symlink_mode.into(),
        file_mode: parse(file_mode, false)?,
        directory_mode: parse(directory_mode, true)?,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_sync(
    alpha: String,
    beta: String,
    mode: ModeArgument,
    ignores: Vec<String>,
    policy: Policy,
    watch: bool,
    interval: u64,
    state_dir: Option<PathBuf>,
    beta_agent: Option<String>,
    alpha_agent: Option<String>,
) -> Result<()> {
    // Compute the session identity and state directory. Local paths are
    // resolved to their physical identity, so a session created here shares
    // its identity (and therefore its state lock) with any supervisor
    // session over the same roots, even when the two spell them differently.
    //
    // The resolution is performed exactly once per side and *frozen*: the
    // same PathBuf serves the identity string, the pair lock, and the
    // endpoint root. A second canonicalization at endpoint construction —
    // as this once did — reopens the window in which a retargeted symlink
    // binds a different tree than the state and lock identify, letting a
    // stale ancestor authorize writes into the wrong tree.
    let frozen_of = |spec: &str, agent: &Option<String>| -> Option<PathBuf> {
        if agent.is_some() || parse_remote(spec).is_some() {
            None
        } else {
            Some(paths::resolve_for_identity(&PathBuf::from(spec)))
        }
    };
    let alpha_frozen = frozen_of(&alpha, &alpha_agent);
    let beta_frozen = frozen_of(&beta, &beta_agent);
    let identity_from = |spec: &str, frozen: &Option<PathBuf>| -> String {
        match frozen {
            Some(path) => path.to_string_lossy().into_owned(),
            None => spec.to_owned(),
        }
    };
    let alpha_identity = identity_from(&alpha, &alpha_frozen);
    let beta_identity = identity_from(&beta, &beta_frozen);
    let identifier = session_identifier(&alpha_identity, &beta_identity);
    let state_directory = match state_dir {
        Some(directory) => directory,
        None => paths::default_state_root()?
            .join("sessions")
            .join(&identifier),
    };

    // Construct the endpoints: an agent connection (SSH or explicit
    // command) for remote specifications, a local endpoint otherwise.
    let options = || -> Result<EndpointOptions> {
        Ok(EndpointOptions {
            ignores: IgnoreSet::new(&ignores)?,
            symlink_mode: policy.symlink_mode,
            file_mode: policy.file_mode,
            directory_mode: policy.directory_mode,
            max_file_size: None,
            max_entry_count: None,
            default_owner: None,
            default_group: None,
        })
    };
    let initialize = |root: String, side: &str| Initialize {
        root,
        session: identifier.clone(),
        ignores: ignores.clone(),
        symlink_mode: policy.symlink_mode,
        file_mode: policy.file_mode,
        directory_mode: policy.directory_mode,
        side: side.to_owned(),
        staging: Default::default(),
        max_file_size: None,
        max_entry_count: None,
        default_owner: None,
        default_group: None,
    };
    let endpoint = |spec: &str,
                    agent: Option<String>,
                    frozen: Option<&PathBuf>,
                    side: &str|
     -> Result<Box<dyn Endpoint + Send>> {
        if let Some(command) = agent {
            let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
            if argv.is_empty() {
                bail!("empty {side} agent command");
            }
            let connection = Connection::spawn(&argv)?;
            return Ok(Box::new(RemoteEndpoint::connect(
                connection,
                initialize(spec.to_owned(), side),
            )?));
        }
        if let Some((host, path)) = parse_remote(spec) {
            return Ok(Box::new(autobahn::endpoint::remote::connect_ssh(
                host,
                initialize(path.to_owned(), side),
            )?));
        }
        // The frozen resolution computed for the session identity above —
        // never a second canonicalization of the original spelling, and
        // never the raw lexical path, either of which would reopen the
        // retarget window the freeze closes. A missing *beta* root is a
        // legitimate state (the transition creates it, under the frozen
        // resolved parent); a missing alpha stays an error, since a
        // mistyped source combined with a mirroring mode would otherwise
        // empty the destination.
        let root = frozen
            .expect("local endpoints carry a frozen resolution")
            .clone();
        if side == "alpha" && std::fs::symlink_metadata(&root).is_err() {
            bail!("unable to resolve {side} root {spec}");
        }
        Ok(Box::new(LocalEndpoint::new(
            root,
            state_directory.join(format!("staging-{side}")),
            options()?,
        )?))
    };
    let alpha_endpoint = endpoint(&alpha, alpha_agent, alpha_frozen.as_ref(), "alpha")?;
    let beta_endpoint = endpoint(&beta, beta_agent, beta_frozen.as_ref(), "beta")?;

    // Create the session and run.
    let mut session = Session::new(alpha_endpoint, beta_endpoint, mode.into(), state_directory)?;
    // Exclusivity over the *trees*, not just the chosen state directory: a
    // manual sync with --state-dir must not run beside a supervisor that
    // owns the same pair under different state.
    session.hold(autobahn::session::EndpointPairLock::acquire(
        &alpha_identity,
        &beta_identity,
    )?);
    let mut follow_ups = 0u32;
    loop {
        let report = session.run_cycle()?;
        print_report(&report);
        if report.missing_staged_files {
            // Content changed between staging and transition. A follow-up
            // usually settles it, but a tree under continuous writing can
            // sustain this indefinitely — so the follow-ups are bounded.
            // Past the bound a watching run falls through to its watcher,
            // which paces the next attempt on an actual change rather than
            // spinning, and a single pass reports that it did not finish.
            follow_ups += 1;
            if follow_ups <= autobahn::supervisor::MAXIMUM_FOLLOW_UP_CYCLES {
                continue;
            }
            if !watch {
                bail!(
                    "staged content was still missing after {follow_ups} cycles; source \
                     content is changing faster than it can be transferred"
                );
            }
        } else {
            follow_ups = 0;
        }
        if !watch {
            break;
        }
        // Wait for a change on either side (with the interval as the
        // heartbeat), then let a short settle window coalesce write bursts.
        // An await failure is deliberately ignored here: the next cycle
        // surfaces the underlying problem with full context.
        if let Ok(true) = session.await_change(Duration::from_secs(interval.max(1))) {
            session.settle(Duration::from_millis(100), Duration::from_millis(20));
        }
    }
    Ok(())
}

/// Sends a control request to the running supervisor and reports the result.
fn run_control(request: ControlRequest, state_root: Option<PathBuf>, verb: &str) -> Result<()> {
    let state_root = resolve_state_root(state_root)?;
    match autobahn::supervisor::control::send(&state_root, &request)? {
        ControlResponse::Applied { sessions } => {
            println!("{verb} {sessions} session(s)");
            Ok(())
        }
        ControlResponse::Error(message) => bail!(message),
    }
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
/// One pass over every configured session, then exit — non-zero if any
/// session failed.
fn run_sync_config(config: Option<PathBuf>, state_root: Option<PathBuf>) -> Result<()> {
    let plans = load_config(config)?.plans()?;
    if plans.is_empty() {
        bail!("the configuration describes no sessions");
    }
    let state_root = resolve_state_root(state_root)?;
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
    Ok(())
}

/// Runs every configured session in the foreground until interrupted.
///
/// On a terminal, the supervisor is silent and the screen is a live
/// `autobahn status`, redrawn as sessions report. Off a terminal — a log
/// file under the login service, a pipe — the supervisor logs one line
/// per event instead, which is what a log wants.
fn run_watch(
    config: Option<PathBuf>,
    state_root: Option<PathBuf>,
    expand_conflicts: bool,
    log: bool,
) -> Result<()> {
    let plans = load_config(config)?.plans()?;
    if plans.is_empty() {
        bail!("the configuration describes no sessions");
    }
    let state_root = resolve_state_root(state_root)?;
    let live_display = !log && unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;

    if !live_display {
        println!(
            "supervising {} session(s); status is available via `autobahn status`",
            plans.len()
        );
        let supervisor = Supervisor::new(plans, state_root, true);
        // Runs until the process is terminated: agent processes exit when
        // their connection streams close, so no explicit cleanup is needed.
        let stop = std::sync::atomic::AtomicBool::new(false);
        return supervisor.run_watch(&stop);
    }

    // The supervisor runs on its own thread and writes status records as
    // it goes; this thread reads them back and repaints. The records are
    // the same ones `autobahn status` reads, so the two never disagree.
    let display_plans = plans.clone();
    let display_root = state_root.clone();
    std::thread::spawn(move || {
        let supervisor = Supervisor::new(plans, state_root, false);
        let stop = std::sync::atomic::AtomicBool::new(false);
        if let Err(error) = supervisor.run_watch(&stop) {
            // Leave the display and say why, since the loop below would
            // otherwise keep repainting a supervisor that no longer exists.
            leave_display();
            eprintln!("autobahn: {error:#}");
            std::process::exit(1);
        }
    });

    // The display lives on the alternate screen, like a pager: it takes
    // the terminal over while it runs and gives it back — scrollback and
    // all — on exit, including an interrupt. Both signals set a flag the
    // loop checks, so the screen is restored on the loop's own terms
    // rather than by a handler racing a half-painted frame.
    static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    extern "C" fn interrupt(_: libc::c_int) {
        INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    unsafe {
        libc::signal(libc::SIGINT, interrupt as libc::sighandler_t);
        libc::signal(libc::SIGTERM, interrupt as libc::sighandler_t);
    }
    enter_display();

    let mut previous = String::new();
    while !INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
        let mut frame = String::new();
        let selected: Vec<&autobahn::config::SessionPlan> = display_plans.iter().collect();
        render_status(&selected, &display_root, expand_conflicts, &mut frame);
        let frame = fit_to_terminal(frame);
        if frame != previous {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "\x1b[H{frame}\x1b[J");
            let _ = out.flush();
            previous = frame;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    leave_display();
    Ok(())
}

/// Switches to the alternate screen and hides the cursor.
fn enter_display() {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\x1b[?1049h\x1b[?25l\x1b[H\x1b[2J");
    let _ = out.flush();
}

/// Restores the main screen and the cursor.
fn leave_display() {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\x1b[?25h\x1b[?1049l");
    let _ = out.flush();
}

/// The terminal's size in (rows, columns), when it can be determined.
fn terminal_size() -> Option<(usize, usize)> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } != 0 {
        return None;
    }
    if size.ws_row == 0 || size.ws_col == 0 {
        return None;
    }
    Some((size.ws_row as usize, size.ws_col as usize))
}

/// Trims a frame to what the terminal can show, so a configuration longer
/// than the screen does not scroll the top away on every repaint. What is
/// hidden is counted in the footer rather than silently lost.
fn fit_to_terminal(frame: String) -> String {
    let Some((rows, columns)) = terminal_size() else {
        return frame + "\n\x1b[2mwatching · Ctrl-C to stop\x1b[0m\n";
    };
    // A line wider than the terminal wraps and eats a second row; counting
    // that keeps the footer on screen.
    let visual_rows = |line: &str| {
        let width = strip_escapes(line).chars().count();
        if width == 0 {
            1
        } else {
            width.div_ceil(columns)
        }
    };
    let lines: Vec<&str> = frame.lines().collect();
    let total: usize = lines.iter().map(|line| visual_rows(line)).sum();
    let budget = rows.saturating_sub(2); // the footer and a margin
    let mut out = String::new();
    if total <= budget {
        for line in &lines {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("\n\x1b[2mwatching · Ctrl-C to stop\x1b[0m\n");
        return out;
    }
    let mut used = 0;
    let mut shown = 0;
    for line in &lines {
        let needed = visual_rows(line);
        if used + needed > budget.saturating_sub(1) {
            break;
        }
        out.push_str(line);
        out.push('\n');
        used += needed;
        shown += 1;
    }
    out.push_str(&format!(
        "\x1b[2m… {} more lines — enlarge the terminal, or `autobahn status` · Ctrl-C to stop\x1b[0m\n",
        lines.len() - shown
    ));
    out
}

/// Removes ANSI escape sequences, for measuring visible width.
fn strip_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Removes state belonging to sessions the configuration no longer
/// describes.
///
/// "Stale" is decided against the configuration, never against age: a
/// session that is configured but has not run in a year is not stale, and
/// one removed from the configuration this morning is. Everything a session
/// owns is keyed by its identifier — its directory under `sessions/`, its
/// status record, and (on this machine as an agent) its staging — so the
/// live set is the set of identifiers the configuration produces. Endpoint
/// locks are keyed by the pair of endpoint identities instead, and the
/// live set of those is computed the same way.
///
/// A session that is *running* holds its lock, and a lock that cannot be
/// acquired means the state behind it is in use; such state is skipped and
/// reported rather than removed from under a live process.
fn run_clean(
    config: Option<PathBuf>,
    state_root: Option<PathBuf>,
    dry_run: bool,
    agent_staging_older_than: Option<u64>,
) -> Result<()> {
    use autobahn::session::{EndpointPairLock, SessionLock};
    use std::collections::HashSet;

    let plans = load_config(config)?.plans()?;
    let state_root = resolve_state_root(state_root)?;
    let live_sessions: HashSet<String> = plans.iter().map(|plan| plan.identifier()).collect();
    let live_locks: HashSet<String> = plans
        .iter()
        .map(|plan| EndpointPairLock::key(&plan.alpha_identity, &plan.beta_identity))
        .collect();

    let verb = if dry_run { "would remove" } else { "removed" };
    let mut removed = 0usize;
    let mut bytes = 0u64;
    let mut in_use = 0usize;

    let mut remove = |path: &Path, what: &str| -> Result<()> {
        let size = directory_size(path);
        println!("{verb} {what} {} ({})", path.display(), format_size(size));
        if !dry_run {
            if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            }
            .with_context(|| format!("unable to remove {}", path.display()))?;
        }
        removed += 1;
        bytes += size;
        Ok(())
    };

    // Sessions: the ancestor, the controller-side staging, and the lock.
    // The lock is taken first — and held while the directory is removed —
    // so a session that starts in the meantime cannot open state that is
    // half gone.
    let sessions = state_root.join("sessions");
    let mut retired: HashSet<String> = HashSet::new();
    for entry in list_directory(&sessions)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if live_sessions.contains(&name) {
            continue;
        }
        let path = entry.path();
        match SessionLock::acquire(path.clone()) {
            Ok(_lock) => {
                remove(&path, "session")?;
                retired.insert(name);
            }
            Err(_) => {
                println!("skipped session {} (in use)", path.display());
                in_use += 1;
            }
        }
    }

    // Status records are written by the supervisor for the sessions it
    // runs; one without a configured session describes nothing.
    for entry in list_directory(&state_root.join("status"))? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(identifier) = name.strip_suffix(".json") else {
            continue;
        };
        if !live_sessions.contains(identifier) {
            remove(&entry.path(), "status record")?;
        }
    }

    // Endpoint locks live in the *default* state root regardless of any
    // override, because their job is to catch sessions that disagree about
    // the state root. A held lock belongs to a running session somewhere
    // and is left alone.
    let locks = paths::default_state_root()?.join("endpoint-locks");
    for entry in list_directory(&locks)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if live_locks.contains(&name) {
            continue;
        }
        let path = entry.path();
        match SessionLock::acquire(path.clone()) {
            Ok(_lock) => remove(&path, "endpoint lock")?,
            Err(_) => {
                println!("skipped endpoint lock {} (in use)", path.display());
                in_use += 1;
            }
        }
    }

    // Agent-side staging: `<session>-<side>` directories and their scan
    // caches, written when this machine serves as the agent for a session
    // driven from *some* controller. When the session is one this
    // configuration describes and it is stale, the staging is certainly
    // stale too. Otherwise the owner is another machine and cannot be
    // consulted, so age is the only available test and it is opt-in.
    let staging = paths::default_state_root()?.join("staging");
    for entry in list_directory(&staging)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stem = name.trim_end_matches(".scancache");
        let Some((identifier, _side)) = stem.rsplit_once('-') else {
            continue;
        };
        if live_sessions.contains(identifier) {
            continue;
        }
        let path = entry.path();
        // Staging for a session retired above is certainly stale. Anything
        // else here belongs to a controller elsewhere.
        let known_stale = retired.contains(identifier);
        let old_enough = agent_staging_older_than.is_some_and(|days| {
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age.as_secs() >= days * 86_400)
        });
        if known_stale || old_enough {
            remove(&path, "staged content")?;
        }
    }

    if removed == 0 && in_use == 0 {
        println!("nothing to clean");
    } else {
        println!(
            "{verb} {removed} item(s), {}{}",
            format_size(bytes),
            if in_use > 0 {
                format!("; {in_use} in use and left alone")
            } else {
                String::new()
            }
        );
    }
    Ok(())
}

/// Lists a directory's entries, treating a missing directory as empty.
fn list_directory(path: &Path) -> Result<Vec<std::fs::DirEntry>> {
    match std::fs::read_dir(path) {
        Ok(entries) => entries
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("unable to list {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("unable to list {}", path.display())),
    }
}

/// The total size of a file or directory tree, best-effort.
fn directory_size(path: &Path) -> u64 {
    fn walk(path: &Path, total: &mut u64) {
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return;
        };
        if metadata.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    walk(&entry.path(), total);
                }
            }
        } else {
            *total += metadata.len();
        }
    }
    let mut total = 0;
    walk(path, &mut total);
    total
}

/// Formats a byte count for display.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Shows the recorded status of the configured sessions.
fn run_status(
    config: Option<PathBuf>,
    state_root: Option<PathBuf>,
    group: Option<String>,
    host: Option<String>,
    expand_conflicts: bool,
) -> Result<()> {
    let plans = load_config(config)?.plans()?;
    let state_root = resolve_state_root(state_root)?;

    // The selector is a group name or a folder. A folder is resolved to its
    // physical identity and matched against each group's alpha — by
    // containment, not equality, so the selector can be a directory *inside*
    // a synchronized root. That is what makes `autobahn status .` useful
    // from anywhere in a project rather than only at its top.
    let folder = group.as_deref().and_then(|selector| {
        let looks_like_path = selector.starts_with('.')
            || selector.starts_with('/')
            || selector.starts_with('~')
            || std::path::Path::new(selector).is_dir();
        if !looks_like_path {
            return None;
        }
        let expanded = paths::expand_tilde(selector).ok()?;
        Some(paths::resolve_for_identity(&expanded))
    });
    let covers = |plan: &autobahn::config::SessionPlan, folder: &std::path::Path| -> bool {
        let alpha = std::path::Path::new(&plan.alpha_identity);
        folder == alpha || folder.starts_with(alpha)
    };

    let selected: Vec<_> = plans
        .iter()
        .filter(|plan| match (&folder, group.as_deref()) {
            (Some(folder), _) => covers(plan, folder),
            (None, Some(group)) => plan.group == group,
            (None, None) => true,
        })
        .filter(|plan| host.as_deref().is_none_or(|host| plan.host == host))
        .collect();
    if selected.is_empty() {
        match (&folder, &group, &host) {
            (Some(folder), _, _) => bail!(
                "no configured group synchronizes {} (or anything containing it)",
                folder.display()
            ),
            (None, Some(group), Some(host)) => {
                bail!("no configured session matches {group}@{host}")
            }
            (None, Some(group), None) => bail!("no configured group named '{group}'"),
            _ => bail!("the configuration describes no sessions"),
        }
    }

    let mut out = String::new();
    render_status(&selected, &state_root, expand_conflicts, &mut out);
    print!("{out}");
    Ok(())
}

/// Renders the status of the selected sessions, as `status` prints it and
/// `watch` repaints it.
fn render_status(
    selected: &[&autobahn::config::SessionPlan],
    state_root: &Path,
    expand_conflicts: bool,
    out: &mut String,
) {
    use std::fmt::Write;

    let rows: Vec<(&autobahn::config::SessionPlan, Option<SessionStatus>)> = selected
        .iter()
        .map(|plan| {
            (
                *plan,
                read_status(state_root, &plan.identifier()).ok().flatten(),
            )
        })
        .collect();

    // Everything below is *recorded* state, read from disk. Without saying
    // whether a supervisor is running, a session whose supervisor exited an
    // hour ago still reads as "synchronized" — the command's most
    // misleading possible output, since nothing is synchronizing at all.
    // The service's state says what to do about it.
    if !autobahn::supervisor::control::supervisor_is_running(state_root) {
        let remedy = match autobahn::service::state() {
            Ok(autobahn::service::ServiceState::NotInstalled) => {
                "run `autobahn watch` here, or `autobahn install` for a login service"
            }
            Ok(autobahn::service::ServiceState::Stopped) => {
                "the login service is installed but stopped; run `autobahn start`"
            }
            Ok(autobahn::service::ServiceState::Running) => {
                "the login service reports running, but is not answering"
            }
            Err(_) => "run `autobahn watch`",
        };
        let _ = writeln!(
            out,
            "\x1b[33mno supervisor is running\x1b[0m; what follows is the state \
             last recorded, not what is happening now\n{remedy}\n"
        );
    }

    let mut current_group: Option<&str> = None;
    let mut index = 0;
    while index < rows.len() {
        let group = rows[index].0.group.as_str();
        let end = rows[index..]
            .iter()
            .position(|(plan, _)| plan.group != group)
            .map(|offset| index + offset)
            .unwrap_or(rows.len());
        let block = &rows[index..end];

        if current_group.is_some() {
            out.push('\n');
        }
        current_group = Some(group);
        let plan = block[0].0;
        // The folder leads, because that is what the reader is thinking
        // about; the group name follows because that is what reset and
        // status take as an argument.
        let _ = writeln!(
            out,
            "\x1b[1m{}\x1b[0m \x1b[2m{}\x1b[0m",
            plan.alpha_spec, plan.group
        );

        for (plan, status) in block {
            render_status_entry(
                &plan.beta_spec(),
                autobahn::config::mode_name(plan.mode),
                status.as_ref(),
                expand_conflicts,
                out,
            );
        }
        index = end;
    }
}

/// Renders one destination and the labelled facts about it.
///
/// Labelled rather than columnar: a column's width is set by the widest
/// entry in its block, so blocks with different destinations line their
/// values up at different offsets and the page reads as jagged. A label
/// carries its own meaning and needs no alignment to be found.
fn render_status_entry(
    destination: &str,
    mode: &str,
    status: Option<&SessionStatus>,
    expand_conflicts: bool,
    out: &mut String,
) {
    use std::fmt::Write;
    // Only the folder is emphasised. Indentation already separates the
    // destinations from it, and bolding both levels leaves neither leading.
    let _ = writeln!(out, "  {destination}");
    let Some(status) = status else {
        let _ = writeln!(out, "    status: \x1b[2mnever run\x1b[0m");
        let _ = writeln!(out, "    mode: {mode}");
        return;
    };

    // A connection failure is its own state: "why is this not running" is
    // answered by the word, not by reading a paragraph of error text.
    let unreachable = status
        .error
        .as_deref()
        .is_some_and(|error| error.contains("unable to synchronize with"));
    let (label, colour) = match status.state.as_str() {
        _ if unreachable => ("unreachable", "\x1b[31m"),
        "synchronized" => ("synchronized", ""),
        "error" => ("error", "\x1b[31m"),
        other => (other, "\x1b[33m"),
    };
    let reset = if colour.is_empty() { "" } else { "\x1b[0m" };
    let progress = if status.cycles == 0 {
        "never run".to_owned()
    } else if status.cycles == 1 {
        "1 cycle".to_owned()
    } else {
        format!("{} cycles", status.cycles)
    };
    let _ = writeln!(
        out,
        "    status: {colour}{label}{reset}, {progress}, {}",
        format_age(status.updated_at)
    );
    let _ = writeln!(out, "    mode: {mode}");

    // Conflicts collapse to a count and an example: a session with forty of
    // them is one fact ("this pair disagrees"), not forty.
    //
    // Only the state carries colour. A page where every detail line is
    // painted has no emphasis left to spend: the reader scans the status
    // words to find what needs attention, then reads the plain lines under
    // whichever one they stopped at.
    match (status.conflicts.len(), expand_conflicts) {
        (0, _) => {}
        (1, _) => {
            let _ = writeln!(out, "    conflicts: 1, {}", status.conflicts[0]);
        }
        (count, false) => {
            let _ = writeln!(out, "    conflicts: {count}, first {}", status.conflicts[0]);
        }
        (count, true) => {
            let _ = writeln!(out, "    conflicts: {count}");
            for root in &status.conflicts {
                let _ = writeln!(out, "      {root}");
            }
        }
    }
    match status.problems.len() {
        0 => {}
        1 => {
            let _ = writeln!(out, "    problems: 1, {}", status.problems[0]);
        }
        count => {
            let _ = writeln!(out, "    problems: {count}, first {}", status.problems[0]);
        }
    }
    if let Some(error) = &status.error {
        // The innermost cause is the diagnosis; the wrapping context repeats
        // the destination this block already names.
        let detail = error.rsplit(": ").next().unwrap_or(error);
        let _ = writeln!(out, "    error: {detail}");
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
