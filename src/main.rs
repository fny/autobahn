//! The autobahn command line interface.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

mod pager;
mod shop;

use autobahn::config::Config;
use autobahn::endpoint::local::{EndpointOptions, LocalEndpoint};
use autobahn::endpoint::remote::RemoteEndpoint;
use autobahn::endpoint::Endpoint;
use autobahn::paths;
use autobahn::progress::ProgressSnapshot;
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
        /// Repaint continuously instead of printing once, showing every
        /// session's phase as it happens. A read-only window onto the
        /// running supervisor; `watch` is the same display, but it also
        /// does the synchronizing. Ctrl-C leaves.
        #[arg(long)]
        live: bool,
        /// Print the report as JSON — the same document every user
        /// interface reads.
        #[arg(long)]
        json: bool,
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
    /// Everything that needs you: conflicts, blocked paths, and halts,
    /// grouped by cause with the command that clears each one.
    #[command(alias = "conflicts")]
    Issues {
        /// A group name, or a folder (`.`, an absolute path, a `~` path)
        /// inside a synchronized root. Omit for every group.
        selector: Option<String>,
        /// The root-relative path to look under, when the selector was a
        /// group or a folder. Scopes the listing to that subtree.
        path: Option<String>,
        /// Filter to a destination within the group.
        #[arg(long)]
        host: Option<String>,
        /// Roll conflicts up to this many path segments and show a count
        /// for each: `--depth 1` lists the top-level folders in conflict.
        #[arg(long, value_name = "N")]
        depth: Option<usize>,
        /// Show only conflicts whose path matches. A pattern with no glob
        /// characters matches anywhere in the path, case-insensitively
        /// (`--filter arcturus`); one with them is a glob, anchored to the
        /// root when it contains a slash and matched at any depth when it
        /// does not (`--filter '*.ts'`, `--filter 'arcturus/**'`).
        #[arg(long, value_name = "PATTERN")]
        filter: Option<String>,
        /// Print as JSON: the status report, restricted to sessions in
        /// conflict.
        #[arg(long)]
        json: bool,
        /// The configuration file (defaults to ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// The shop.
    #[command(hide = true)]
    Mi {
        /// The configuration file (defaults to ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Show how the two sides of a file differ.
    ///
    /// The file may be named by a filesystem path inside a synchronized
    /// root (`autobahn diff ./src/main.rs`), or by group and root-relative
    /// path (`autobahn diff project src/main.rs`). With several
    /// destinations, name one with --host; otherwise each is shown.
    Diff {
        /// A group name, or a path — to the file itself, or to a folder
        /// inside a synchronized root.
        selector: String,
        /// The root-relative path, when the selector was a group or folder.
        path: Option<String>,
        /// The destination to compare against (defaults to every one).
        #[arg(long)]
        host: Option<String>,
        /// The configuration file (defaults to ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Resolve conflicts by choosing which side's version wins.
    ///
    /// The winner is named by what `status` calls it: `alpha`, or a
    /// destination's host (or local path). Its content is put on alpha and
    /// every other destination, so one command settles a conflict across
    /// a whole fan-out. `both` keeps the winner in place and renames the
    /// other side's version aside as `<name>.<side>` before propagating.
    ///
    /// Resolution makes the sides agree; the next cycle records the
    /// agreement and the conflict is gone. Nothing here touches the
    /// ancestor.
    Resolve {
        /// A group name, or a path — to the conflicting file itself, or to a
        /// folder inside a synchronized root.
        selector: String,
        /// The root-relative paths, when the selector was a group or
        /// folder. Several may be given; each losing side is then read once
        /// for all of them rather than once per path.
        paths: Vec<String>,
        /// Whose version wins: `alpha`, a destination host or path, or
        /// `both`.
        #[arg(long)]
        keep: String,
        /// Resolve every conflict in the selected sessions the same way.
        #[arg(long)]
        all: bool,
        /// Do not ask. Resolution overwrites a file someone edited, on
        /// every destination in the group, so it asks first by default.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Filter to a destination within the group.
        #[arg(long)]
        host: Option<String>,
        /// The configuration file (defaults to ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Run the menu bar app: an icon whose colour is the state of every
    /// session, a menu with the detail, and the ways to settle each
    /// conflict. Built with the `tray` feature.
    Tray {
        /// The configuration file (defaults to ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
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
            live,
            json,
        } => run_status(config, state_root, group, host, conflicts, live, json),
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
        Command::Issues {
            selector,
            path,
            host,
            depth,
            filter,
            json,
            config,
            state_root,
        } => run_issues(
            config, state_root, selector, path, host, depth, filter, json,
        ),
        Command::Mi { config, state_root } => run_shop(config, state_root),
        Command::Diff {
            selector,
            path,
            host,
            config,
            state_root,
        } => run_diff(config, state_root, selector, path, host),
        Command::Resolve {
            selector,
            paths,
            keep,
            all,
            yes,
            host,
            config,
            state_root,
        } => run_resolve(config, state_root, selector, paths, keep, all, yes, host),
        #[cfg(feature = "tray")]
        Command::Tray { config, state_root } => {
            resolve_state_root(state_root).and_then(|root| autobahn::tray::run(config, root))
        }
        #[cfg(not(feature = "tray"))]
        Command::Tray { .. } => Err(anyhow::anyhow!(
            "this build has no menu bar app; build one with `cargo build --release --features tray`"
        )),
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
        // `run_control` sends only the verbs; progress is asked for by
        // `status`, which reads the answer itself.
        ControlResponse::Progress(_) => bail!("the supervisor answered with progress"),
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
                    summary.push_str(&format!(", {} blocked", digest.problems));
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
    let configuration = load_config(config)?;
    let plans = configuration.plans()?;
    if plans.is_empty() {
        bail!("the configuration describes no sessions");
    }
    // Validated here so a misspelled state or an unreadable duration is a
    // startup failure, not a silent no-op discovered on the night the
    // alert was meant to fire.
    let alerts = configuration.alert_plan()?;
    let state_root = resolve_state_root(state_root)?;

    // Said before the first cycle, while someone is still looking at the
    // terminal. These sessions will stop on their own anyway; the point is
    // that the reader learns it now rather than from a status page later.
    for (session, problem) in autobahn::supervisor::unreadable_ancestors(&plans, &state_root) {
        eprintln!("[{session}] {problem}");
    }
    let live_display = !log && unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;

    if !live_display {
        println!(
            "supervising {} session(s); status is available via `autobahn status`",
            plans.len()
        );
        let supervisor = Supervisor::new(plans, state_root, true).with_alerts(alerts);
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
    // A supervisor that fails has nothing left to display, so it asks the
    // display to leave and hands its failure back here to be reported —
    // after the terminal has been restored, and by the thread that owns
    // the exit status.
    let failure: Arc<Mutex<Option<String>>> = Arc::default();
    let reported = failure.clone();
    std::thread::spawn(move || {
        let supervisor = Supervisor::new(plans, state_root, false).with_alerts(alerts);
        let stop = std::sync::atomic::AtomicBool::new(false);
        if let Err(error) = supervisor.run_watch(&stop) {
            *reported.lock().unwrap_or_else(|error| error.into_inner()) =
                Some(format!("{error:#}"));
            pager::leave();
        }
    });

    let selected: Vec<&autobahn::config::SessionPlan> = display_plans.iter().collect();
    run_live_display(&selected, &display_root, expand_conflicts, "watching")?;
    // Taken into a binding of its own, so the lock is released before the
    // match rather than held across it.
    let failure = failure
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    match failure {
        Some(error) => bail!(error),
        None => Ok(()),
    }
}

/// Repaints the status of the selected sessions until the reader leaves.
///
/// This is the display half of `watch`, which also supervises; on its own
/// it is `status --live`, a read-only window onto whatever supervisor is
/// already running — the login service, or a `watch` in another terminal.
/// Every phase is shown however brief: the reader is looking at it, and
/// movement is the point.
fn run_live_display(
    selected: &[&autobahn::config::SessionPlan],
    state_root: &Path,
    expand_conflicts: bool,
    label: &str,
) -> Result<()> {
    pager::display(label, || {
        let mut frame = String::new();
        render_status(selected, state_root, expand_conflicts, true, &mut frame);
        frame
    })
}

/// What a selector picked out: the sessions, and — when the selector was a
/// path *inside* a synchronized root — the root-relative remainder.
struct Selection<'a> {
    plans: Vec<&'a autobahn::config::SessionPlan>,
    /// The path's remainder below the alpha root, when the selector was a
    /// path deeper than the root itself. `Some("")` never occurs; the root
    /// itself yields `None`.
    relative: Option<String>,
}

/// Selects sessions by group name or by folder.
///
/// A selector that looks like a path — starting with `.`, `/`, or `~`, or
/// naming something that exists — is resolved to its physical identity and
/// matched against each group's alpha by *containment*, so a folder or
/// file inside a synchronized root selects the group covering it. That is
/// what lets every command that takes a selector answer from wherever the
/// caller happens to be standing, and what lets `diff` and `resolve` be
/// pointed at a file directly. Anything else is a group name.
fn select<'a>(
    plans: &'a [autobahn::config::SessionPlan],
    selector: Option<&str>,
    host: Option<&str>,
) -> Result<Selection<'a>> {
    let folder = selector.and_then(|selector| {
        // An explicit path marker always means a path. A bare word is a
        // group name if one matches — a group called `rr` must stay
        // addressable from a directory that also contains an `rr` — and is
        // tried as a path only when no group has that name.
        let explicit =
            selector.starts_with('.') || selector.starts_with('/') || selector.starts_with('~');
        let is_group = plans.iter().any(|plan| plan.group == selector);
        let looks_like_path = explicit || (!is_group && std::path::Path::new(selector).exists());
        if !looks_like_path {
            return None;
        }
        let expanded = paths::expand_tilde(selector).ok()?;
        Some(paths::resolve_for_identity(&expanded))
    });
    let mut relative = None;
    let selected: Vec<&autobahn::config::SessionPlan> = plans
        .iter()
        .filter(|plan| match (&folder, selector) {
            (Some(folder), _) => {
                let alpha = std::path::Path::new(&plan.alpha_identity);
                if folder == alpha {
                    true
                } else if let Ok(rest) = folder.strip_prefix(alpha) {
                    relative = Some(rest.to_string_lossy().into_owned());
                    true
                } else {
                    false
                }
            }
            (None, Some(group)) => plan.group == group,
            (None, None) => true,
        })
        .collect();
    let before_host = selected.len();
    // A destination is named as status prints it: a host name, or for a
    // local beta its path — which may be spelled with ~ or relatively, so
    // a path-looking name is compared by identity rather than text.
    let host_identity = host.and_then(|host| {
        let looks_like_path =
            host.starts_with('.') || host.starts_with('/') || host.starts_with('~');
        if !looks_like_path {
            return None;
        }
        let expanded = paths::expand_tilde(host).ok()?;
        Some(
            paths::resolve_for_identity(&expanded)
                .to_string_lossy()
                .into_owned(),
        )
    });
    let selected: Vec<&autobahn::config::SessionPlan> = selected
        .into_iter()
        .filter(|plan| {
            host.is_none_or(|host| {
                plan.host == host
                    || plan.beta_spec() == host
                    || host_identity.as_deref() == Some(plan.beta_identity.as_str())
            })
        })
        .collect();
    if selected.is_empty() {
        match (&folder, selector, host) {
            (_, _, Some(host)) if before_host > 0 => bail!(
                "no destination named {host:?} in the selected group (destinations are named \
                 as `autobahn status` shows them)"
            ),
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
    Ok(Selection {
        plans: selected,
        relative,
    })
}

/// The root-relative path a command was given: explicitly, or as the
/// remainder of a path selector.
fn relative_path(selection: &Selection, explicit: Option<String>) -> Result<String> {
    match (explicit, &selection.relative) {
        (Some(path), _) => Ok(path
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_owned()),
        (None, Some(rest)) => Ok(rest.clone()),
        (None, None) => bail!(
            "name the file: a root-relative path after the group, or a path to the file itself"
        ),
    }
}

/// The path inside a recorded blocked entry.
///
/// The entries are written as `side path: message` by the supervisor.
/// Splitting them back apart is what lets the listing group by cause and
/// scope by path.
fn blocked_path(entry: &str) -> Option<&str> {
    let rest = entry.split_once(' ')?.1;
    Some(match rest.find(": ") {
        Some(end) => &rest[..end],
        None => rest,
    })
}

/// The side, path and cause of a recorded blocked entry.
///
/// The cause is the innermost message. The wrapping context repeats the
/// file's own path, so twenty files that failed for one reason would
/// otherwise read as twenty separate reasons.
fn blocked_parts(entry: &str) -> (&str, &str, &str) {
    let (side, rest) = entry.split_once(' ').unwrap_or(("", entry));
    let (path, message) = match rest.find(": ") {
        Some(end) => (&rest[..end], &rest[end + 2..]),
        None => (rest, ""),
    };
    let cause = message.rsplit(": ").next().unwrap_or(message);
    (side, path, cause)
}

/// Groups paths by where they are, and names the directory each group
/// shares.
///
/// A single cause can cover unrelated places — one permission problem in
/// one tree and another somewhere else — and those have nothing in common
/// but the reason. Clustering by the first segment separates them, and
/// each cluster then names the deepest directory all of its paths share.
fn clusters(paths: &[&str]) -> Vec<(String, usize)> {
    let mut grouped: Vec<(&str, Vec<&str>)> = Vec::new();
    for path in paths {
        let head = path.split('/').next().unwrap_or(path);
        match grouped.iter_mut().find(|(other, _)| *other == head) {
            Some((_, members)) => members.push(path),
            None => grouped.push((head, vec![path])),
        }
    }
    grouped
        .into_iter()
        .map(|(head, members)| {
            let prefix = common_prefix(&members);
            let prefix = if prefix.is_empty() {
                head.to_owned()
            } else {
                prefix
            };
            (prefix, members.len())
        })
        .collect()
}

/// The longest directory prefix shared by every path.
fn common_prefix(paths: &[&str]) -> String {
    let Some(first) = paths.first() else {
        return String::new();
    };
    let mut prefix: Vec<&str> = first.split('/').collect();
    // A file name is never part of the shared directory.
    prefix.pop();
    for path in &paths[1..] {
        let segments: Vec<&str> = path.split('/').collect();
        let shared = prefix
            .iter()
            .zip(segments.iter())
            .take_while(|(left, right)| left == right)
            .count();
        prefix.truncate(shared);
    }
    prefix.join("/")
}

/// What to do about a group of blocked paths.
///
/// Specific where it can be. A permission problem on a remote destination
/// gets the `ssh` and the path filled in, because "check the permissions"
/// is advice and this is a command.
fn blocked_fix(
    side: &str,
    cause: &str,
    prefix: &str,
    plan: &autobahn::config::SessionPlan,
) -> Vec<String> {
    let mut fixes = Vec::new();
    let where_ = if prefix.is_empty() { "." } else { prefix };
    if cause.contains("Permission denied") {
        match side {
            "beta" => {
                let spec = plan.beta_spec();
                match spec.split_once(':') {
                    Some((destination, root)) => {
                        let user = destination.split('@').next().unwrap_or(destination);
                        fixes.push(format!(
                            "ssh {destination} 'sudo chown -R {user} {root}/{where_}'"
                        ));
                    }
                    None => fixes.push(format!("sudo chown -R \"$(whoami)\" {spec}/{where_}")),
                }
            }
            _ => fixes.push(format!(
                "sudo chown -R \"$(whoami)\" {}/{where_}",
                plan.alpha_spec
            )),
        }
        // An ignore is only the answer for something generated. The
        // deepest hidden directory in the path is that; the last segment
        // is often a version ("0.9.10") or a real folder ("static"), and
        // ignoring either would be worse than the permissions.
        if let Some(name) = prefix.split('/').rev().find(|name| name.starts_with('.')) {
            fixes.push(format!("or add \"{name}\" to the group's ignores"));
        }
    } else if cause == "unicode collision" || cause == "casing collision" {
        // The other side holds one name twice, under spellings this
        // filesystem files as a single entry. Diffing was the old
        // suggestion and it is useless: the two copies are usually the
        // same bytes, and one of them is often a PDF.
        let elsewhere = if side == "beta" {
            "alpha".to_owned()
        } else {
            plan.host.clone()
        };
        let how = if cause.starts_with("unicode") {
            "spelled two ways"
        } else {
            "cased two ways"
        };
        fixes.push(format!(
            "{elsewhere} holds this name twice, {how}. Delete either copy there"
        ));
    } else if cause.contains("refusing to create over existing content") {
        fixes.push(format!(
            "something no scan saw is at that path. Run `autobahn issues {}` \
             after the next cycle; if it stays, look at both sides",
            plan.group
        ));
    }
    fixes
}

/// Lists everything that needs a person, grouped by cause.
#[allow(clippy::too_many_arguments)]
fn run_issues(
    config: Option<PathBuf>,
    state_root: Option<PathBuf>,
    selector: Option<String>,
    path: Option<String>,
    host: Option<String>,
    depth: Option<usize>,
    filter: Option<String>,
    json: bool,
) -> Result<()> {
    let plans = load_config(config)?.plans()?;
    let state_root = resolve_state_root(state_root)?;
    let selection = select(&plans, selector.as_deref(), host.as_deref())?;
    let matches = filter.as_deref().map(conflict_filter).transpose()?;
    if let Some(0) = depth {
        bail!("--depth counts path segments, so it starts at 1");
    }
    // Where to look. `resolve` and `diff` both take a path this way, and
    // both accept it as the selector itself — `autobahn conflicts .`
    // should mean the folder you are standing in, not the whole group.
    let scope = match (&path, &selection.relative) {
        (Some(path), _) => Some(
            path.trim_start_matches("./")
                .trim_end_matches('/')
                .to_owned(),
        ),
        (None, Some(rest)) => Some(rest.clone()),
        (None, None) => None,
    };
    let within = |path: &str| match &scope {
        None => true,
        Some(scope) => path == scope || path.starts_with(&format!("{scope}/")),
    };
    // Depth counts from the scope, not from the root. Rolling up from the
    // root inside a scope would collapse everything into the scope itself.
    let below = scope
        .as_deref()
        .map(|scope| scope.split('/').count())
        .unwrap_or(0);
    if json {
        // Depth is a way of *reading* a long list, and a reader that wants
        // JSON has its own. Rolling the records up here would hand it a
        // different shape than it asked for, so it is refused rather than
        // silently ignored.
        if depth.is_some() {
            bail!("--depth is a display option; with --json, group the paths yourself");
        }
        let mut report = autobahn::supervisor::status_report(&selection.plans, &state_root);
        for group in &mut report.groups {
            for session in &mut group.sessions {
                let keep = |path: &str| {
                    within(path) && matches.as_ref().is_none_or(|matches| matches(path))
                };
                session.conflicts.retain(|conflict| keep(&conflict.path));
                session
                    .blocked
                    .retain(|blocked| keep(blocked_path(blocked).unwrap_or(blocked)));
            }
            // A session that failed has no lists to show and still needs
            // someone, so its state is what keeps it here.
            group.sessions.retain(|session| {
                !session.conflicts.is_empty()
                    || !session.blocked.is_empty()
                    || matches!(session.state.as_str(), "halted" | "unreachable" | "errored")
            });
        }
        report.groups.retain(|group| !group.sessions.is_empty());
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let mut total = 0;
    let mut current_group: Option<&str> = None;
    for plan in &selection.plans {
        let Some(status) = read_status(&state_root, &plan.identifier())? else {
            continue;
        };
        let keep =
            |path: &str| within(path) && matches.as_ref().is_none_or(|matches| matches(path));
        let selected: Vec<&String> = status.conflicts.iter().filter(|path| keep(path)).collect();
        let blocked: Vec<&String> = status
            .blocked
            .iter()
            .filter(|entry| keep(blocked_path(entry).unwrap_or(entry)))
            .collect();
        // A failure has no list of its own and still needs someone. Its
        // state is what puts it here, and a filter that excludes every
        // path does not exclude it.
        let state = autobahn::supervisor::classify_state(&status);
        let failed = matches!(state.as_str(), "halted" | "unreachable" | "errored");
        if selected.is_empty() && blocked.is_empty() && !failed {
            continue;
        }
        // The group's heading waits until something under it survived the
        // filter: a heading over nothing reads as a group in trouble.
        if current_group != Some(plan.group.as_str()) {
            if current_group.is_some() {
                println!();
            }
            current_group = Some(plan.group.as_str());
            println!(
                "\x1b[1m{}\x1b[0m \x1b[2m{}\x1b[0m",
                plan.alpha_spec, plan.group
            );
        }
        println!("  {}", plan.beta_spec());
        total += selected.len() + blocked.len();

        if failed {
            total += 1;
            println!("\n    \x1b[31m{state}\x1b[0m");
            if let Some(error) = &status.error {
                println!("      {}", error.trim_start_matches("halted: "));
            }
            if state == "halted" {
                println!("      fix: make the two sides agree, then it resumes");
            }
        }

        if selected.is_empty() && blocked.is_empty() {
            continue;
        }
        if !selected.is_empty() {
            println!(
                "\n    \x1b[33m{}\x1b[0m",
                match selected.len() {
                    1 => "1 conflict".to_owned(),
                    many => format!("{many} conflicts"),
                }
            );
        }

        // Rolled up, a conflict list becomes a map of where the trouble
        // is: seven hundred paths under one folder are one fact about that
        // folder, and the reader drills in from there.
        if let Some(depth) = depth {
            for (prefix, count) in roll_up(&selected, depth + below) {
                match count {
                    1 if selected.contains(&&prefix) => println!("    {prefix}"),
                    1 => println!("    {prefix} — 1 conflict"),
                    count => println!("    {prefix} — {count} conflicts"),
                }
            }
            println!(
                "    → `autobahn conflicts {} --depth {}` opens the next level",
                plan.group,
                depth + 1
            );
            continue;
        }

        for path in &selected {
            let detail = status
                .conflict_details
                .iter()
                .find(|detail| &&detail.path == path);
            println!("      {path}");
            if let Some(detail) = detail {
                let describe = |side: &autobahn::supervisor::ConflictSide| -> String {
                    if !side.present {
                        return "absent".to_owned();
                    }
                    match side.kind.as_str() {
                        "file" => format!(
                            "{}, modified {}",
                            format_size(side.size),
                            format_age(side.mtime_seconds.max(0) as u64)
                        ),
                        kind => kind.to_owned(),
                    }
                };
                println!("        alpha  {}", describe(&detail.alpha));
                println!("        {:<6} {}", plan.host, describe(&detail.beta));
                // Why it is a conflict at all. Two sides that merely
                // differ are reconciled; a side holding content that was
                // never scanned is not overwritten, and that refusal is
                // what the reader is actually looking at. Without it the
                // listing shows a difference and no reason for the
                // stalemate.
                for (side, name) in [(&detail.alpha, "alpha"), (&detail.beta, &plan.host)] {
                    let Some(blocking) = &side.unsynchronizable else {
                        continue;
                    };
                    let count = match blocking.entries {
                        1 => "1 entry".to_owned(),
                        many => format!("{many} entries"),
                    };
                    println!(
                        "        {name} holds {count} synchronization cannot carry, \
                         so neither side is overwritten"
                    );
                    println!("          {} — {}", blocking.example, blocking.reason);
                }
            }
        }
        if !selected.is_empty() {
            let where_ = match &scope {
                Some(scope) => scope.clone(),
                None if selected.len() == 1 => selected[0].clone(),
                None => "<path>".to_owned(),
            };
            println!(
                "      fix: autobahn resolve {} {where_} --keep alpha|{}|both",
                plan.group, plan.host
            );
        }

        // Blocked paths, grouped by what stopped them. Twenty files under
        // one directory that all failed for one reason are one problem
        // with one fix, and listing them separately hides that.
        let mut causes: Vec<(&str, &str, Vec<&str>)> = Vec::new();
        for entry in &blocked {
            let (side, path, cause) = blocked_parts(entry);
            match causes
                .iter_mut()
                .find(|(other_side, other_cause, _)| *other_side == side && *other_cause == cause)
            {
                Some((_, _, paths)) => paths.push(path),
                None => causes.push((side, cause, vec![path])),
            }
        }
        for (side, cause, paths) in &causes {
            println!(
                "\n    \x1b[33m{} on {side}\x1b[0m \x1b[2m— {cause}\x1b[0m",
                match paths.len() {
                    1 => "1 blocked".to_owned(),
                    many => format!("{many} blocked"),
                }
            );
            // One cause can still cover unrelated places. These twenty
            // are one permission problem in `azure` and another in
            // `arcturus`, and their shared prefix is nothing at all — so
            // the paths are clustered by where they are before the
            // directory they share is named.
            for (prefix, count) in clusters(paths) {
                match (count, prefix.as_str()) {
                    (1, _) => println!(
                        "      {}",
                        paths
                            .iter()
                            .find(|path| path.starts_with(&prefix))
                            .copied()
                            .unwrap_or(&prefix)
                    ),
                    (count, "") => println!("      {count} paths"),
                    (count, prefix) => println!("      {count} under {prefix}/"),
                }
                for (index, fix) in blocked_fix(side, cause, &prefix, plan).iter().enumerate() {
                    match index {
                        0 => println!("        fix: {fix}"),
                        _ => println!("             {fix}"),
                    }
                }
            }
        }
    }
    if total == 0 {
        match (&scope, &filter) {
            (Some(scope), _) => println!("nothing needs you under {scope}"),
            (None, Some(pattern)) => println!("nothing needs you matching {pattern:?}"),
            (None, None) => println!("nothing needs you"),
        }
    }
    Ok(())
}

/// Groups conflict paths by their first `depth` segments, keeping the order
/// they were reported in and counting what falls under each.
///
/// A path shorter than the depth is its own group: there is nothing further
/// to roll it up into.
fn roll_up(paths: &[&String], depth: usize) -> Vec<(String, usize)> {
    let mut grouped: Vec<(String, usize)> = Vec::new();
    for path in paths {
        let prefix = path.split('/').take(depth).collect::<Vec<_>>().join("/");
        match grouped.iter_mut().find(|(existing, _)| *existing == prefix) {
            Some((_, count)) => *count += 1,
            None => grouped.push((prefix, 1)),
        }
    }
    grouped
}

/// Builds the predicate behind `conflicts --filter`.
///
/// Two behaviours, chosen by what the pattern looks like, because a
/// filter is typed in a hurry: a plain word is what someone means when
/// they type `--filter arcturus`, and a glob is what they mean when they
/// type `--filter '*.ts'`. A glob without a slash is matched at any depth,
/// the way the same pattern behaves in an ignore file; one with a slash is
/// anchored to the root, since that is what writing the separator asks
/// for.
fn conflict_filter(pattern: &str) -> Result<Box<dyn Fn(&str) -> bool>> {
    if !pattern.contains(['*', '?', '[', '{']) {
        let needle = pattern.to_lowercase();
        return Ok(Box::new(move |path: &str| {
            path.to_lowercase().contains(&needle)
        }));
    }
    let anchored = if pattern.contains('/') {
        pattern.to_owned()
    } else {
        format!("**/{pattern}")
    };
    let glob = globset::Glob::new(&anchored)
        .with_context(|| format!("unable to read the filter {pattern:?} as a pattern"))?
        .compile_matcher();
    Ok(Box::new(move |path: &str| glob.is_match(path)))
}

/// Opens the shop.
fn run_shop(config: Option<PathBuf>, state_root: Option<PathBuf>) -> Result<()> {
    let plans = load_config(config.clone())?.plans()?;
    let state_root = resolve_state_root(state_root)?;
    let selected: Vec<&autobahn::config::SessionPlan> = plans.iter().collect();
    shop::run(&selected, &state_root, config)
}

/// Asks before resolving, and says what resolution will do.
///
/// `--yes` answers in advance. Where standard input is not a terminal
/// there is nobody to ask, so the command refuses rather than prompting
/// into a pipe: a prompt nobody can answer either hangs or reads
/// end-of-file, and both look like the tool having silently done nothing.
fn confirmed(
    targets: &[(&autobahn::config::SessionPlan, Vec<String>)],
    kept: &str,
    overwritten: &[String],
    scope: Option<&str>,
    both: bool,
    yes: bool,
) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    let paths: usize = targets.iter().map(|(_, paths)| paths.len()).sum();
    let what = match paths {
        1 => "1 conflict".to_owned(),
        many => format!("{many} conflicts"),
    };
    match scope {
        Some(scope) => println!("about to resolve {what} under {scope}:"),
        None => println!("about to resolve {what}:"),
    }

    // The two sides by their roots. Naming them "alpha" and
    // "group@destination" told the reader neither which machine nor which
    // folder, and the second reads like an ssh target, which it is not.
    println!();
    if both {
        println!("  keep       {kept}");
        for loser in overwritten {
            println!("  keep too   {loser}  (renamed aside)");
        }
    } else {
        println!("  keep       {kept}");
        for loser in overwritten {
            println!("  overwrite  {loser}");
        }
    }
    println!();

    // The paths. One destination needs no repetition of its name; several
    // do, because their conflicts differ.
    if targets.len() == 1 {
        for path in &targets[0].1 {
            println!("  {path}");
        }
    } else {
        for (plan, paths) in targets {
            println!("  {}", plan.beta_spec());
            for path in paths {
                println!("    {path}");
            }
        }
    }
    println!();

    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        bail!("nothing to answer the prompt; pass --yes to resolve without asking");
    }
    print!("proceed? [y/N] ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).ok();
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// Shows how the two sides of one file differ, with the system's diff.
fn run_diff(
    config: Option<PathBuf>,
    state_root: Option<PathBuf>,
    selector: String,
    path: Option<String>,
    host: Option<String>,
) -> Result<()> {
    let plans = load_config(config)?.plans()?;
    let state_root = resolve_state_root(state_root)?;
    let selection = select(&plans, Some(&selector), host.as_deref())?;
    let path = relative_path(&selection, path)?;
    let pool = autobahn::transport::mux::AgentPool::default();

    // A scratch directory of our own, removed on exit; the dev-dependency
    // on tempfile is not available to the binary.
    let scratch = std::env::temp_dir().join(format!("autobahn-diff-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).context("unable to create a scratch directory")?;
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let scratch = Scratch(scratch);
    let mut shown = 0;
    for plan in &selection.plans {
        let (mut alpha, mut beta) = autobahn::supervisor::open_endpoints(plan, &state_root, &pool)?;
        let a = alpha.read_file(&path)?;
        let b = beta.read_file(&path)?;
        if a == b {
            println!("{}: identical on alpha and {}", path, plan.host);
            continue;
        }
        shown += 1;
        let write = |name: &str, content: &Option<Vec<u8>>| -> Result<PathBuf> {
            let file = scratch.0.join(name);
            std::fs::write(&file, content.as_deref().unwrap_or(b""))
                .with_context(|| format!("unable to write {}", file.display()))?;
            Ok(file)
        };
        let left = write("alpha", &a)?;
        let right = write(&plan.host.replace('/', "_"), &b)?;
        // The labels name the sides, not the scratch files.
        let status = std::process::Command::new("diff")
            .args(["-u", "--label", &format!("alpha/{path}"), "--label"])
            .arg(format!("{}/{path}", plan.host))
            .arg(&left)
            .arg(&right)
            .status()
            .context("unable to run diff")?;
        // diff exits 1 when the files differ, which is the expected case;
        // 2 is a real failure.
        if status.code() == Some(2) {
            bail!("diff failed for {path}");
        }
        if a.is_none() || b.is_none() {
            println!(
                "({} on {})",
                if a.is_none() { "absent" } else { "present" },
                if a.is_none() { "alpha" } else { &plan.host }
            );
        }
    }
    let _ = shown;
    Ok(())
}

/// Resolves conflicts by putting the winner's version on every other side.
#[allow(clippy::too_many_arguments)]
/// The node a root-relative path names in a scanned tree, or `None` when
/// nothing is there.
fn node_at<'a>(
    root: Option<&'a autobahn::tree::Node>,
    path: &str,
) -> Option<&'a autobahn::tree::Node> {
    let mut node = root?;
    if path.is_empty() {
        return Some(node);
    }
    for name in path.split('/') {
        node = node.child(name)?;
    }
    Some(node)
}

/// The first entry at or beneath `node` that synchronization cannot carry,
/// as a root-relative path and the reason, or `None` when there is none.
///
/// A transition refuses to remove such an entry — content reconciliation
/// never scanned is content nobody decided to delete — and refuses
/// bottom-up, so asking anyway strips everything around it and leaves a
/// half-deleted tree behind. Knowing in advance is what turns that into a
/// refusal to start.
fn unsynchronizable_within(node: &autobahn::tree::Node, path: &str) -> Option<(String, String)> {
    use autobahn::tree::{path_join, Content};
    match &node.content {
        Content::Directory(children) => children
            .iter()
            .find_map(|child| unsynchronizable_within(child, &path_join(path, &child.name))),
        Content::Problematic { message } => Some((path.to_owned(), message.clone())),
        Content::Untracked => Some((path.to_owned(), "excluded from synchronization".to_owned())),
        _ => None,
    }
}

/// A free name beside `path` for a version being kept rather than
/// discarded, suffixed by the side it came from.
///
/// Free is checked against the scan rather than assumed: a previous
/// `--keep both` on the same path leaves the obvious name taken, and a
/// rename onto an occupied name is refused (which would abandon the
/// resolution half-done). The counter is a last resort, not a habit.
fn free_name(root: Option<&autobahn::tree::Node>, path: &str, side: &str) -> String {
    // The side named in the shortest form that still identifies it: a host
    // name as is, a local path by its last component.
    let suffix = std::path::Path::new(side)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| side.to_owned());
    let candidate = format!("{path}.{suffix}");
    if node_at(root, &candidate).is_none() {
        return candidate;
    }
    for counter in 2.. {
        let candidate = format!("{path}.{suffix}.{counter}");
        if node_at(root, &candidate).is_none() {
            return candidate;
        }
    }
    unreachable!("the counter is unbounded")
}

fn run_resolve(
    config: Option<PathBuf>,
    state_root: Option<PathBuf>,
    selector: String,
    named: Vec<String>,
    keep: String,
    all: bool,
    yes: bool,
    host: Option<String>,
) -> Result<()> {
    let plans = load_config(config)?.plans()?;
    let state_root = resolve_state_root(state_root)?;
    let selection = select(&plans, Some(&selector), host.as_deref())?;
    let group = selection.plans[0].group.clone();
    if selection.plans.iter().any(|plan| plan.group != group) {
        bail!("resolve works within one group; the selector matched several");
    }

    // The winner: alpha, both, or one of this group's destinations by the
    // name status prints for it.
    #[derive(Clone, Copy, PartialEq)]
    enum Winner {
        Alpha,
        Both,
        Beta(usize),
    }
    let winner = match keep.as_str() {
        "alpha" => Winner::Alpha,
        "both" => Winner::Both,
        other => {
            let all_in_group: Vec<&autobahn::config::SessionPlan> =
                plans.iter().filter(|plan| plan.group == group).collect();
            let matches: Vec<usize> = all_in_group
                .iter()
                .enumerate()
                .filter(|(_, plan)| plan.host == other || plan.beta_spec() == other)
                .map(|(index, _)| index)
                .collect();
            match matches.as_slice() {
                [index] => Winner::Beta(*index),
                [] => bail!(
                    "--keep {other:?} names no destination of group '{group}'; expected \
                     alpha, both, or one of: {}",
                    all_in_group
                        .iter()
                        .map(|plan| plan.host.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                _ => bail!("--keep {other:?} is ambiguous; name the destination as host:path"),
            }
        }
    };

    // Which paths, in which sessions — and the folder they came from,
    // when a folder was named.
    let mut targets: Vec<(&autobahn::config::SessionPlan, Vec<String>)> = Vec::new();
    let mut scope: Option<String> = None;
    if all {
        for plan in &selection.plans {
            if let Some(status) = read_status(&state_root, &plan.identifier())? {
                if !status.conflicts.is_empty() {
                    targets.push((plan, status.conflicts.clone()));
                }
            }
        }
        if targets.is_empty() {
            println!("no conflicts to resolve");
            return Ok(());
        }
        if winner == Winner::Both {
            bail!("--all cannot keep both: choose whose version wins");
        }
    } else {
        // Several paths may be named at once. Each losing side is read once
        // for the whole command, so settling twenty conflicts costs one
        // scan rather than twenty — which is what the shop does when it
        // settles a row.
        let named: Vec<String> = match named.is_empty() {
            true => vec![relative_path(&selection, None)?],
            false => named
                .into_iter()
                .map(|path| relative_path(&selection, Some(path)))
                .collect::<Result<_>>()?,
        };
        // A named path may be a file or a folder. Take every recorded
        // conflict at or under it, so naming a folder settles what is
        // inside it — `conflicts` scopes the same way.
        for plan in &selection.plans {
            if let Some(status) = read_status(&state_root, &plan.identifier())? {
                let under: Vec<String> = status
                    .conflicts
                    .iter()
                    .filter(|path| {
                        named
                            .iter()
                            .any(|name| *path == name || path.starts_with(&format!("{name}/")))
                    })
                    .cloned()
                    .collect();
                if !under.is_empty() {
                    targets.push((plan, under));
                }
            }
        }
        // Nothing recorded there. The paths are taken at their word, which
        // is how a file is forced to match a side without a conflict being
        // reported first.
        if targets.is_empty() {
            targets = selection
                .plans
                .iter()
                .map(|plan| (*plan, named.clone()))
                .collect();
        } else if targets
            .iter()
            .any(|(_, paths)| paths.iter().any(|path| !named.contains(path)))
        {
            // Only a folder that actually expanded is reported as one.
            // Naming a file and being told its conflict is "under" it
            // reads as though there were more inside.
            scope = Some(named.join(", "));
        }
    }

    let group_plans: Vec<&autobahn::config::SessionPlan> =
        plans.iter().filter(|plan| plan.group == group).collect();

    // Who keeps their version, and who gets overwritten — named by their
    // actual roots. "alpha" is the word `--keep` takes, and on its own it
    // says nothing about which machine or folder that is.
    let alpha_spec = group_plans
        .first()
        .map(|plan| plan.alpha_spec.clone())
        .unwrap_or_default();
    let (kept, mut overwritten) = match winner {
        Winner::Alpha | Winner::Both => (
            format!("{alpha_spec}  (alpha)"),
            targets
                .iter()
                .map(|(plan, _)| plan.beta_spec())
                .collect::<Vec<_>>(),
        ),
        Winner::Beta(index) => (
            format!(
                "{}  ({})",
                group_plans[index].beta_spec(),
                group_plans[index].host
            ),
            std::iter::once(format!("{alpha_spec}  (alpha)"))
                .chain(
                    targets
                        .iter()
                        .filter(|(plan, _)| plan.identifier() != group_plans[index].identifier())
                        .map(|(plan, _)| plan.beta_spec()),
                )
                .collect(),
        ),
    };
    overwritten.dedup();

    // One confirmation, whether the command names one path or every one.
    // Resolution overwrites a file that someone deliberately edited — that
    // is what made it a conflict — and it does so on every destination in
    // the group, not only the one named. Both facts are worth reading
    // before they happen.
    if !confirmed(
        &targets,
        &kept,
        &overwritten,
        scope.as_deref(),
        winner == Winner::Both,
        yes,
    )? {
        println!("nothing done");
        return Ok(());
    }

    // Every session in the group is opened, because the winner's content
    // must reach every destination — including sessions the selector did
    // not name, when the winner is one destination and the path conflicts
    // on another.
    let pool = autobahn::transport::mux::AgentPool::default();
    let mut endpoints = Vec::new();
    for plan in &group_plans {
        endpoints.push(autobahn::supervisor::open_endpoints(
            plan,
            &state_root,
            &pool,
        )?);
    }

    let paths: std::collections::BTreeSet<String> = targets
        .iter()
        .flat_map(|(_, paths)| paths.iter().cloned())
        .collect();

    // Resolution retires the *losing* version and lets the ordinary cycle
    // carry the winner's, rather than copying bytes across by hand.
    //
    // That is not a shortcut, it is the only approach that works for
    // everything a filesystem holds. Copying bytes can settle a file and
    // nothing else: a directory has no bytes to read, and "write no bytes"
    // means removing it, which `remove_file` refuses. Reconciliation, on
    // the other hand, already resolves this shape — when one side's change
    // is purely a deletion, the other side's content propagates over it,
    // for a file, a symbolic link, or a whole tree alike (see the comment
    // at `tree/reconcile.rs:375`, which names manual conflict resolution as
    // the reason). So the smallest honest edit is to make the losing side's
    // change a pure deletion and let the engine do what it already does.
    //
    // The removal itself goes through `transition`, not through a raw
    // recursive delete. A transition removes a directory bottom-up and
    // refuses any entry that has moved since the scan it was planned from,
    // so content that appeared while this command was running is never
    // destroyed by it. That validation is the whole reason to route through
    // the engine here rather than call `remove_dir_all`.
    let mut settled = 0usize;
    let mut refused: Vec<(String, String)> = Vec::new();
    // Paths that cannot be settled this way at all, as opposed to ones
    // that lost a race. The two need different words: one says try again,
    // the other says this will never work.
    let mut blocked: Vec<(String, String, String, String)> = Vec::new();
    for (index, plan) in group_plans.iter().enumerate() {
        // Which side of this session loses. The winner keeps its version
        // untouched; every other copy in the group is retired, including
        // alpha's when a destination wins, since alpha is how the winning
        // content reaches the group's other destinations.
        let losing: Vec<(&mut Box<dyn autobahn::endpoint::Endpoint + Send>, String)> = {
            let (alpha, beta) = &mut endpoints[index];
            match winner {
                // Alpha wins (or both sides are kept, in which case alpha's
                // copy stays put and beta's moves aside): only beta loses.
                Winner::Alpha | Winner::Both => vec![(beta, plan.host.clone())],
                // This destination wins, so alpha loses — and alpha is
                // retired *here*, on the winning session, for two reasons.
                // Every session opens its own handle on alpha, so the
                // retirement has to happen through exactly one of them; and
                // this is the session that will carry the winning content
                // back to alpha, from which the group's other destinations
                // then take it.
                Winner::Beta(w) if w == index => vec![(alpha, "alpha".to_owned())],
                // Another destination wins, so this one loses too. Its copy
                // and alpha's both go; the pair reads as a deletion on both
                // sides, which clears the ancestor entry, and the winner's
                // content then arrives as ordinary new content.
                Winner::Beta(_) => vec![(beta, plan.host.clone())],
            }
        };

        for (endpoint, side) in losing {
            // Only paths that actually conflict on this session are
            // touched; a destination that already agrees is left alone.
            // Alpha is the exception: its copy must go for the winning
            // content to reach it, whichever session reported the conflict.
            let here: Vec<&String> = paths
                .iter()
                .filter(|path| {
                    side == "alpha"
                        || targets.iter().any(|(target, paths)| {
                            target.identifier() == plan.identifier() && paths.contains(*path)
                        })
                })
                .collect();
            if here.is_empty() {
                continue;
            }

            // One scan per losing side, not one per path — and it must be
            // the scan the removals are validated against, so nothing runs
            // between it and the transition.
            //
            // A scan of a large tree is the slow part of this command, so
            // it says whose tree it is reading. On a terminal the line is
            // erased afterwards; anywhere else it is never written, since a
            // carriage return in a log file is noise.
            let transient = unsafe { libc::isatty(libc::STDERR_FILENO) } == 1;
            if transient {
                eprint!("  reading {side}\r");
            }
            let snapshot = endpoint
                .scan()
                .with_context(|| format!("unable to read {side}"))?;
            if transient {
                eprint!("\r\x1b[2K");
            }

            let mut removals = Vec::new();
            for path in here {
                let Some(node) = node_at(snapshot.root.as_ref(), path) else {
                    // Already gone on this side. Nothing to retire, and the
                    // cycle will carry the winner's version here anyway.
                    continue;
                };
                if winner == Winner::Both {
                    // Kept, not discarded: the losing version moves to a
                    // free name, from which it propagates to every side as
                    // ordinary new content. A rename does this for a whole
                    // tree without moving any of it, which is why it is a
                    // primitive rather than a read and a write.
                    let aside = free_name(snapshot.root.as_ref(), path, &side);
                    endpoint
                        .rename(path, &aside)
                        .with_context(|| format!("unable to keep {side}'s {path}"))?;
                    println!("  {side}: kept {path} as {aside}");
                    settled += 1;
                    continue;
                }
                // Content synchronization never scanned cannot be removed
                // by a transition — it refuses, by design, because nobody
                // decided to delete what reconciliation never saw. Asking
                // anyway does not fail cleanly: the removal runs
                // bottom-up, takes away everything it *can* account for,
                // and leaves the rest. That is the worst of both outcomes,
                // a half-deleted tree and the conflict still open, so the
                // check happens here rather than being discovered midway.
                if let Some((example, reason)) = unsynchronizable_within(node, path) {
                    blocked.push((path.clone(), side.clone(), example, reason));
                    continue;
                }
                removals.push(autobahn::tree::Change {
                    path: path.clone(),
                    old: Some(node.clone()),
                    new: None,
                });
            }

            if removals.is_empty() {
                continue;
            }
            let outcome = endpoint
                .transition(removals)
                .with_context(|| format!("unable to retire {side}'s version"))?;
            // A refusal is not an error: the transition reports it and
            // leaves the content alone. It means the path moved between the
            // scan and the removal, which is exactly the case the
            // validation exists to catch — so it is reported, by path, and
            // the conflict stays.
            for problem in &outcome.problems {
                refused.push((problem.path.clone(), problem.message.clone()));
            }
            settled += outcome
                .results
                .iter()
                .filter(|result| result.is_none())
                .count();
        }
    }
    // Always said, including "settled 0". A command that reports nothing
    // reads as a command that worked, and this one can legitimately settle
    // none of what it was asked to.
    let kept_word = match winner {
        Winner::Both => "both versions kept",
        _ => "one version kept",
    };
    println!("settled {settled} of {} ({kept_word})", paths.len());

    // Blocked, and permanently: retiring this side would mean deleting
    // content synchronization never scanned, which it will not do. Saying
    // "try again" here would be a lie, so the ways out are named instead.
    for (path, side, example, reason) in &blocked {
        println!("  {path}: not settled — {side} holds {example} ({reason}),");
        println!("    which cannot be deleted on your behalf. Either:");
        println!("      · ignore {path} in this group, so it stops being compared, or");
        println!("      · `--keep both`, which moves the version aside instead of deleting it, or");
        println!("      · delete it on {side} by hand.");
    }

    // Refused, and possibly transient: the entry moved between the scan
    // and the removal, which is the race the validation exists to catch.
    for (path, error) in &refused {
        println!("  left alone: {path} — {error}");
    }
    if !refused.is_empty() {
        println!(
            "{} path(s) changed while this ran and were not touched. \
             Run the command again to settle them.",
            refused.len()
        );
    }

    // The winning version has not moved yet: this command only retired the
    // losing one. The cycle carries the winner across, which is what makes
    // a directory work at all — so the flush is part of the resolution
    // here, not a courtesy to make `status` catch up sooner.
    if autobahn::supervisor::control::supervisor_is_running(&state_root) {
        let _ = autobahn::supervisor::control::send(
            &state_root,
            &ControlRequest::Flush(Selector {
                group: Some(group),
                host: None,
            }),
        );
        if settled > 0 {
            println!("copying the kept version across now");
        }
    } else if settled > 0 {
        println!(
            "the supervisor is not running, so the kept version has not \
             moved yet. Start it with `autobahn start`, or run \
             `autobahn sync` once."
        );
    }
    Ok(())
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

    // The previous generation of the service log. Nothing else prunes
    // it: launchd and systemd write to the log forever and neither
    // rotates, so autobahn rotates it and this is where the old one goes.
    if let Ok(previous) = autobahn::service::previous_log_path() {
        if previous.exists() {
            remove(&previous, "the previous service log")?;
        }
    }
    // The live log is not removed — the supervisor is writing to it — but
    // its size is worth saying, because it is the one file here that
    // grows without bound between rotations.
    if let Ok(live) = autobahn::service::log_path() {
        if let Ok(metadata) = std::fs::metadata(&live) {
            println!(
                "the service log is {} ({}); it rotates on its own",
                format_size(metadata.len()),
                live.display()
            );
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
#[allow(clippy::too_many_arguments)]
fn run_status(
    config: Option<PathBuf>,
    state_root: Option<PathBuf>,
    group: Option<String>,
    host: Option<String>,
    expand_conflicts: bool,
    live: bool,
    json: bool,
) -> Result<()> {
    let plans = load_config(config)?.plans()?;
    let state_root = resolve_state_root(state_root)?;

    let selection = select(&plans, group.as_deref(), host.as_deref())?;
    let selected: Vec<_> = selection.plans;
    if live {
        if json {
            bail!("--live repaints a display; --json prints one document. Pick one");
        }
        if unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1 {
            bail!(
                "--live repaints a terminal display, and this output is not a terminal; \
                 run `autobahn status` on a timer instead"
            );
        }
        return run_live_display(&selected, &state_root, expand_conflicts, "live");
    }
    if json {
        let report = autobahn::supervisor::status_report(&selected, &state_root);
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let mut out = String::new();
    render_status(&selected, &state_root, expand_conflicts, live, &mut out);
    print!("{out}");
    Ok(())
}

/// Renders the status of the selected sessions, as `status` prints it and
/// `watch` repaints it.
fn render_status(
    selected: &[&autobahn::config::SessionPlan],
    state_root: &Path,
    expand_conflicts: bool,
    live: bool,
    out: &mut String,
) {
    use std::fmt::Write;

    // One round trip answers both "is anything running" and "what is each
    // session doing"; the recorded status on disk answers "how did the last
    // cycle end". A session is described by the first when it is working
    // and by the second when it is not.
    let reported = autobahn::supervisor::control::query_progress(state_root);
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
    if reported.is_none() {
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
            let progress = reported.as_ref().and_then(|sessions| {
                sessions
                    .iter()
                    .find(|session| session.group == plan.group && session.host == plan.host)
                    .map(|session| &session.progress)
            });
            render_status_entry(
                &plan.beta_spec(),
                autobahn::config::mode_name(plan.mode),
                status.as_ref(),
                progress,
                live,
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
    progress: Option<&ProgressSnapshot>,
    live: bool,
    expand_conflicts: bool,
    out: &mut String,
) {
    use std::fmt::Write;
    // Only the folder is emphasised. Indentation already separates the
    // destinations from it, and bolding both levels leaves neither leading.
    let _ = writeln!(out, "  {destination}");

    // A session that is working is described by what it is doing — but
    // only once it has been working long enough for that to be the more
    // useful answer. Routine cycles finish in well under a second, and a
    // status that flickers into "scanning" every few seconds reports
    // nothing while hiding what the reader came for. The phase earns the
    // line by taking long enough that its absence would look like death,
    // which is the case it was added for: the cold sync of a large tree.
    // `--live`, and `watch`, want every phase however brief.
    let working = progress.filter(|progress| {
        progress.phase.is_working() && (live || progress.working_seconds >= SLOW_PHASE_SECONDS)
    });
    let Some(status) = status else {
        match working {
            Some(progress) => render_working(progress, out),
            None => {
                let _ = writeln!(out, "    status: \x1b[2mnever run\x1b[0m");
            }
        }
        let _ = writeln!(out, "    mode: {mode}");
        return;
    };

    // The state word comes from the same classifier the JSON report uses,
    // so the two views cannot disagree about what a session is doing.
    //
    // One word cannot say everything: a cycle that ran can leave both
    // conflicts and blocked paths, and the word has to pick. So where the
    // cycle *ran*, the counts are the headline and the word is dropped;
    // where it did not run, the word is the whole story.
    let state = autobahn::supervisor::classify_state(status);
    let failed = matches!(state.as_str(), "errored" | "unreachable" | "halted");
    let label = if failed || status.conflicts.is_empty() && status.blocked.is_empty() {
        state.clone()
    } else {
        let mut parts = Vec::new();
        if !status.conflicts.is_empty() {
            parts.push(format!(
                "{} conflicts",
                thousands(status.conflicts.len() as u64)
            ));
        }
        if !status.blocked.is_empty() {
            parts.push(format!(
                "{} blocked",
                thousands(status.blocked.len() as u64)
            ));
        }
        parts.join(", ")
    };
    let colour = match (failed, label.as_str()) {
        (_, "synchronized") => "",
        (true, _) => "\x1b[31m",
        _ => "\x1b[33m",
    };
    let reset = if colour.is_empty() { "" } else { "\x1b[0m" };
    let progress = if status.cycles == 0 {
        "never run".to_owned()
    } else if status.cycles == 1 {
        "1 cycle".to_owned()
    } else {
        format!("{} cycles", status.cycles)
    };
    match working {
        Some(working) => render_working(working, out),
        None => {
            let _ = writeln!(
                out,
                "    status: {colour}{label}{reset}, {progress}, {}",
                format_age(status.updated_at)
            );
        }
    }
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
    match status.blocked.len() {
        0 => {}
        1 => {
            let _ = writeln!(out, "    blocked: 1, {}", status.blocked[0]);
        }
        count => {
            let _ = writeln!(out, "    blocked: {count}, first {}", status.blocked[0]);
        }
    }
    if let Some(error) = &status.error {
        // The innermost cause is the diagnosis; the wrapping context repeats
        // the destination this block already names.
        let detail = error.rsplit(": ").next().unwrap_or(error);
        // While the session is working, the recorded error is the *last*
        // attempt's, not this one's. Saying so is the difference between a
        // session that is retrying and one that has given up.
        let label = if working.is_some() {
            "last error"
        } else {
            "error"
        };
        let _ = writeln!(out, "    {label}: {detail}");
    }
}

/// How long a phase must have been running before `status` reports it in
/// place of the last cycle's outcome. Comfortably longer than a routine
/// cycle, comfortably shorter than the wait that prompts someone to ask
/// whether anything is happening at all.
const SLOW_PHASE_SECONDS: u64 = 5;

/// Renders what a session is doing right now: the phase, how long it has
/// been in it, an estimate when one can honestly be made, and the counts
/// behind it.
fn render_working(progress: &ProgressSnapshot, out: &mut String) {
    use autobahn::progress::Phase;
    use std::fmt::Write;

    let mut headline = format!(
        "    status: {}, {} elapsed",
        progress.phase.label(),
        format_duration(progress.seconds)
    );
    // An estimate that has rounded to nothing says "any moment now",
    // which the moving counts already say better.
    if let Some(remaining) = progress.remaining_seconds.filter(|left| *left > 0) {
        let _ = write!(headline, ", about {} left", format_estimate(remaining));
    }
    let _ = writeln!(out, "{headline}");

    match progress.phase {
        Phase::Scanning => {
            for (name, side) in [("alpha", &progress.alpha), ("beta", &progress.beta)] {
                if !side.active {
                    continue;
                }
                let mut line = format!("      {name}: ");
                match (side.entries, side.expected) {
                    // A side that reports nothing is one whose scan runs
                    // out of reach — on the far side of an agent — so all
                    // there is to say is that it is running, and for how
                    // long. That alone is the difference between a slow
                    // scan and a stuck session.
                    (0, _) => {
                        let _ = write!(line, "scanning for {}", format_duration(side.seconds));
                    }
                    // A count with nothing to measure it against still
                    // moves, and a number that moves is the answer to "is
                    // this doing anything".
                    (entries, None) => {
                        let _ = write!(line, "{} entries so far", thousands(entries));
                    }
                    // The total is what this side's last scan found, so it
                    // is approximate — the tree has changed since — and is
                    // written as one.
                    (entries, Some(expected)) => {
                        let _ = write!(
                            line,
                            "{} of ~{} entries ({}%)",
                            thousands(entries),
                            thousands(expected),
                            (entries.saturating_mul(100) / expected.max(1)).min(99)
                        );
                    }
                }
                if let Some(remaining) = side.remaining_seconds.filter(|left| *left > 0) {
                    let _ = write!(line, ", about {} left", format_estimate(remaining));
                }
                let _ = writeln!(out, "{line}");
            }
        }
        Phase::Staging => {
            let mut line = format!(
                "      {} of {} files",
                thousands(progress.staged),
                thousands(progress.staged_total)
            );
            if progress.staged_bytes_total > 0 {
                let _ = write!(
                    line,
                    ", {} of {}",
                    format_bytes(progress.staged_bytes),
                    format_bytes(progress.staged_bytes_total)
                );
            }
            let _ = writeln!(out, "{line}");
        }
        Phase::Applying => {
            let _ = writeln!(
                out,
                "      {} of {} changes",
                thousands(progress.applied),
                thousands(progress.applied_total)
            );
        }
        _ => {}
    }
}

/// Formats an estimate, rounded to a precision it can actually support.
///
/// The rate is sampled from work in flight and genuinely varies, so a
/// linear extrapolation moves — twenty seconds out it will disagree with
/// itself by several seconds between one repaint and the next. Reporting
/// that to the second makes the number look unstable rather than
/// approximate. Rounding to a step that grows with the estimate keeps it
/// steady and says what it means: this is an estimate.
fn format_estimate(seconds: u64) -> String {
    let step = match seconds {
        0..=29 => 5,
        30..=299 => 15,
        _ => 60,
    };
    format_duration(seconds.div_ceil(step) * step)
}

/// Formats a duration for display, at two significant units.
fn format_duration(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => match (seconds / 60, seconds % 60) {
            (minutes, 0) => format!("{minutes}m"),
            (minutes, rest) => format!("{minutes}m{rest:02}s"),
        },
        _ => match (seconds / 3600, (seconds % 3600) / 60) {
            (hours, 0) => format!("{hours}h"),
            (hours, minutes) => format!("{hours}h{minutes:02}m"),
        },
    }
}

/// Formats a count with thousands separators, so six digits can be read at
/// a glance rather than counted.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Formats a byte count at three significant figures.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1 << 30, "GB"),
        (1 << 20, "MB"),
        (1 << 10, "kB"),
        (1, "bytes"),
    ];
    for (scale, unit) in UNITS {
        if bytes >= scale {
            if scale == 1 {
                return format!("{bytes} {unit}");
            }
            return format!("{:.1} {unit}", bytes as f64 / scale as f64);
        }
    }
    "0 bytes".to_owned()
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
    use super::{
        blocked_fix, blocked_parts, blocked_path, clusters, common_prefix, conflict_filter,
        format_estimate, parse_remote, render_status_entry, roll_up,
    };

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

    #[test]
    fn rolling_up_groups_by_leading_segments_and_keeps_leaves_whole() {
        let paths: Vec<String> = [
            "vulns/backend/app.py",
            "vulns/backend/db.py",
            "vulns/README.md",
            "autobahn/src/main.rs",
            "Cargo.toml",
        ]
        .iter()
        .map(|path| path.to_string())
        .collect();
        let borrowed: Vec<&String> = paths.iter().collect();

        assert_eq!(
            roll_up(&borrowed, 1),
            vec![
                ("vulns".to_owned(), 3),
                ("autobahn".to_owned(), 1),
                ("Cargo.toml".to_owned(), 1),
            ],
            "depth 1 groups by top-level folder, in the order reported"
        );
        assert_eq!(
            roll_up(&borrowed, 2),
            vec![
                ("vulns/backend".to_owned(), 2),
                ("vulns/README.md".to_owned(), 1),
                ("autobahn/src".to_owned(), 1),
                ("Cargo.toml".to_owned(), 1),
            ]
        );
        // A depth past the deepest path leaves every path its own group,
        // which is the unrolled listing.
        assert_eq!(roll_up(&borrowed, 9).len(), paths.len());
    }

    #[test]
    fn a_filter_is_a_substring_until_it_looks_like_a_glob() {
        // A plain word matches anywhere, ignoring case: what someone means
        // when they type a folder name in a hurry.
        let plain = conflict_filter("arcturus").expect("a plain filter compiles");
        assert!(plain("arcturus/frontend/app.ts"));
        assert!(plain("vendor/ARCTURUS/x"));
        assert!(!plain("vulns/backend/app.py"));

        // A glob without a slash matches at any depth.
        let extension = conflict_filter("*.ts").expect("a glob compiles");
        assert!(extension("arcturus/frontend/app.ts"));
        assert!(extension("app.ts"));
        assert!(!extension("arcturus/frontend/app.tsx"));

        // A glob with a slash is anchored to the root, because writing the
        // separator is what asks for that.
        let anchored = conflict_filter("vulns/**").expect("a glob compiles");
        assert!(anchored("vulns/backend/app.py"));
        assert!(!anchored("other/vulns/backend/app.py"));

        // A malformed pattern is reported, not silently matched.
        assert!(conflict_filter("[").is_err());
    }

    /// A routine cycle must not displace the answer the reader came for.
    ///
    /// Cycles run every few seconds and finish in well under one. If every
    /// one of them turned the status line into "scanning, 0s elapsed", the
    /// line would report nothing while hiding what the last cycle actually
    /// did — the opposite of the problem the phase was added to solve.
    #[test]
    fn a_phase_earns_the_status_line_by_taking_long_enough_to_look_stuck() {
        use autobahn::progress::{Phase, ProgressSnapshot};

        let snapshot = |phase: Phase, seconds: u64| ProgressSnapshot {
            phase,
            seconds,
            working_seconds: seconds,
            alpha: side(),
            beta: side(),
            staged: 0,
            staged_total: 0,
            staged_bytes: 0,
            staged_bytes_total: 0,
            applied: 0,
            applied_total: 0,
            remaining_seconds: None,
        };
        let shows = |progress: &ProgressSnapshot, live: bool| {
            let mut out = String::new();
            render_status_entry(
                "beta",
                "two-way-conflict",
                None,
                Some(progress),
                live,
                false,
                &mut out,
            );
            out.contains("scanning")
        };

        // A cycle that is quick about it says nothing.
        assert!(!shows(&snapshot(Phase::Scanning, 0), false));
        assert!(!shows(&snapshot(Phase::Scanning, 4), false));
        // One that has been going long enough to look dead says so.
        assert!(shows(&snapshot(Phase::Scanning, 5), false));
        assert!(shows(&snapshot(Phase::Scanning, 600), false));
        // A live view — `--live`, and `watch` — wants every phase.
        assert!(shows(&snapshot(Phase::Scanning, 0), true));
        // Waiting is never a phase to report: a session between cycles is
        // described by how the last one ended.
        assert!(!shows(&snapshot(Phase::Waiting, 600), true));
    }

    fn side() -> autobahn::progress::SideSnapshot {
        autobahn::progress::SideSnapshot {
            active: true,
            entries: 0,
            bytes: 0,
            expected: None,
            seconds: 0,
            remaining_seconds: None,
        }
    }

    /// An estimate is rounded to a precision it can support, so that a
    /// number which genuinely moves between repaints does not look broken.
    #[test]
    fn an_estimate_is_rounded_to_the_precision_it_can_support() {
        // Close in, five-second steps: the two readings that made "about
        // 20s" and "about 14s" out of the same transfer now agree.
        assert_eq!(format_estimate(14), "15s");
        assert_eq!(format_estimate(20), "20s");
        assert_eq!(format_estimate(1), "5s");
        // Further out, coarser: quarter-minutes, then minutes.
        assert_eq!(format_estimate(100), "1m45s");
        assert_eq!(format_estimate(700), "12m");
        assert_eq!(format_estimate(3_500), "59m");
        assert_eq!(format_estimate(3_600), "1h");
        // Rounding is always up: an estimate that lands early is a
        // pleasant surprise, one that overruns is a broken promise.
        assert_eq!(format_estimate(31), "45s");
    }

    /// `conflicts` takes a path where its siblings do, and counts depth
    /// from that path rather than from the root.
    ///
    /// The second positional used to be the destination, so
    /// `autobahn conflicts voltai autobahn` reported no such destination —
    /// while `resolve` and `diff`, which take a path there, had taught the
    /// opposite. Rolling up from the root inside a scope is the matching
    /// mistake: every path already shares the scope, so depth 1 would
    /// collapse the whole listing into the scope itself.
    #[test]
    fn a_scope_narrows_the_listing_and_moves_where_depth_counts_from() {
        let paths: Vec<String> = [
            "autobahn/src/main.rs",
            "autobahn/src/shop.rs",
            "autobahn/Cargo.toml",
            "vulns/backend/app.py",
        ]
        .iter()
        .map(|path| path.to_string())
        .collect();

        let within =
            |scope: &str, path: &str| path == scope || path.starts_with(&format!("{scope}/"));
        let scoped: Vec<&String> = paths
            .iter()
            .filter(|path| within("autobahn", path))
            .collect();
        assert_eq!(scoped.len(), 3, "vulns is outside the scope");

        // Depth counts from the scope: one level below `autobahn`.
        let below = "autobahn".split('/').count();
        assert_eq!(
            roll_up(&scoped, 1 + below),
            vec![
                ("autobahn/src".to_owned(), 2),
                ("autobahn/Cargo.toml".to_owned(), 1),
            ]
        );
        // Counting from the root instead would say nothing at all.
        assert_eq!(roll_up(&scoped, 1), vec![("autobahn".to_owned(), 3)]);
    }

    /// Naming a folder resolves the conflicts inside it.
    ///
    /// `resolve` writes a file's bytes to every side. A folder read that
    /// way has no content, so the write becomes a removal — which fails,
    /// but only after saying it would "resolve 1 path". Expanding the
    /// folder into the conflicts recorded under it is both what the
    /// command can do and what `autobahn resolve voltai autobahn` plainly
    /// means.
    #[test]
    fn a_named_folder_covers_the_conflicts_recorded_under_it() {
        let recorded = [
            "autobahn/src/main.rs",
            "autobahn/Cargo.toml",
            "autobahn",
            "vulns/app.py",
            "autobahn-notes.md",
        ];
        let under = |named: &str| -> Vec<&str> {
            recorded
                .iter()
                .copied()
                .filter(|path| *path == named || path.starts_with(&format!("{named}/")))
                .collect()
        };
        assert_eq!(
            under("autobahn"),
            ["autobahn/src/main.rs", "autobahn/Cargo.toml", "autobahn"],
            "the folder, and everything below it"
        );
        // A sibling whose name merely starts the same is not below it.
        assert!(!under("autobahn").contains(&"autobahn-notes.md"));
        // A file names only itself.
        assert_eq!(under("vulns/app.py"), ["vulns/app.py"]);
        // And a path with nothing recorded expands to nothing, which is
        // what sends the command back to taking the path at its word.
        assert!(under("bench").is_empty());
    }

    /// Twenty blocked paths are not twenty problems.
    ///
    /// The recorded entry carries the failing file's own path inside its
    /// message, so the messages all differ. The innermost cause is what
    /// they share, and that is what turns twenty lines into one.
    #[test]
    fn blocked_entries_are_read_back_into_side_path_and_cause() {
        let entry = "beta azure/backend/.ruff_cache/0.9.10/104972: unable to read file: \
                     unable to open /home/ubuntu/Workspace/azure/backend/.ruff_cache/0.9.10/104972: \
                     Permission denied (os error 13)";
        let (side, path, cause) = blocked_parts(entry);
        assert_eq!(side, "beta");
        assert_eq!(path, "azure/backend/.ruff_cache/0.9.10/104972");
        assert_eq!(cause, "Permission denied (os error 13)");
        assert_eq!(blocked_path(entry), Some(path));

        // A message with no wrapping context is its own cause.
        let (side, path, cause) =
            blocked_parts("alpha notes.txt: refusing to create over existing content");
        assert_eq!((side, path), ("alpha", "notes.txt"));
        assert_eq!(cause, "refusing to create over existing content");
    }

    /// One cause can cover unrelated places, and those share nothing but
    /// the reason.
    #[test]
    fn paths_cluster_by_where_they_are_before_a_directory_is_named() {
        // The real shape: sixteen under one tree, four under another, and
        // no prefix in common. Naming the shared directory of all twenty
        // would name the root, and the fix would be "chown everything".
        let mut paths: Vec<&str> = vec![
            "azure/backend/.ruff_cache/0.9.10/a",
            "azure/backend/.ruff_cache/0.9.10/b",
            "arcturus/frontend/static/images/one.png",
            "arcturus/frontend/static/logos/two.svg",
        ];
        assert_eq!(common_prefix(&paths), "", "nothing is shared by all four");
        assert_eq!(
            clusters(&paths),
            vec![
                ("azure/backend/.ruff_cache/0.9.10".to_owned(), 2),
                ("arcturus/frontend/static".to_owned(), 2),
            ]
        );

        // A single path clusters to its own directory.
        paths.truncate(1);
        assert_eq!(
            clusters(&paths),
            vec![("azure/backend/.ruff_cache/0.9.10".to_owned(), 1)]
        );
    }

    /// The ignore suggestion has to name something generated.
    #[test]
    fn an_ignore_is_suggested_only_for_a_generated_directory() {
        let plan = |group: &str| -> autobahn::config::SessionPlan {
            let text = format!(
                "[groups.{group}]\nalpha = \"/tmp/a\"\nmode = \"two-way-conflict\"\nbetas = [\"u@h:/tmp/b\"]\n"
            );
            toml::from_str::<autobahn::config::Config>(&text)
                .expect("parses")
                .plans()
                .expect("plans")
                .remove(0)
        };
        let plan = plan("g");

        // The last segment is a version. The generated directory is the
        // hidden one above it, and that is what may be ignored.
        let fixes = blocked_fix(
            "beta",
            "Permission denied (os error 13)",
            "azure/backend/.ruff_cache/0.9.10",
            &plan,
        );
        assert!(
            fixes[0].starts_with("ssh u@h 'sudo chown -R u "),
            "{fixes:?}"
        );
        assert!(fixes[1].contains("\".ruff_cache\""), "{fixes:?}");

        // Nothing hidden in the path, so no ignore is offered: ignoring a
        // real folder is worse than fixing its ownership.
        let fixes = blocked_fix(
            "beta",
            "Permission denied (os error 13)",
            "arcturus/frontend/static",
            &plan,
        );
        assert_eq!(fixes.len(), 1, "{fixes:?}");
    }

    /// A collision is shown as what it is, not as the error that carried
    /// it.
    ///
    /// The heading is the cause, and the cause is the last part of the
    /// recorded message. So the kind goes at the end: the name in front of
    /// it differs for every file, and twenty files that collided the same
    /// way have to group as one problem.
    #[test]
    fn a_name_collision_reads_as_a_collision() {
        let entry = "alpha recruiting/candidates/Jorge Suárez resume.pdf: \
                     \"Jorge Sua\u{301}rez resume.pdf\" is already here under one entry: \
                     unicode collision";
        let (side, path, cause) = blocked_parts(entry);
        assert_eq!(side, "alpha");
        assert_eq!(path, "recruiting/candidates/Jorge Suárez resume.pdf");
        assert_eq!(cause, "unicode collision", "the heading names the rule");

        // Two files colliding the same way group together, however
        // different the names in front of the cause.
        let other = "alpha recruiting/candidates/Ana Muñoz cv.pdf: \
                     \"Ana Mun\u{303}oz cv.pdf\" is already here under one entry: \
                     unicode collision";
        assert_eq!(blocked_parts(other).2, cause);

        // And the fix names what to do about it rather than offering a
        // diff, which for a PDF is no help at all.
        let plan = toml::from_str::<autobahn::config::Config>(
            "[groups.g]\nalpha = \"/tmp/a\"\nmode = \"two-way-conflict\"\nbetas = [\"u@h:/tmp/b\"]\n",
        )
        .expect("parses")
        .plans()
        .expect("plans")
        .remove(0);
        let fixes = blocked_fix("alpha", cause, "recruiting/candidates", &plan);
        assert_eq!(fixes.len(), 1);
        assert!(fixes[0].contains("holds this name twice"), "{fixes:?}");
        assert!(fixes[0].contains("spelled two ways"), "{fixes:?}");
        assert!(!fixes[0].contains("diff"), "{fixes:?}");

        // Casing says casing.
        let fixes = blocked_fix("beta", "casing collision", "docs", &plan);
        assert!(fixes[0].starts_with("alpha holds"), "{fixes:?}");
        assert!(fixes[0].contains("cased two ways"), "{fixes:?}");
    }
}
