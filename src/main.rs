//! The autobahn command line interface.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use autobahn::text::display_safe;
use clap::{Parser, Subcommand, ValueEnum};

mod pager;
mod shop;
mod style;

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
#[command(
    name = "autobahn",
    version,
    about,
    styles = HELP_STYLES,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The peering verbs.
#[derive(Subcommand)]
enum PeeringVerb {
    /// Bridge standard input and output to the leading supervisor's attach
    /// socket. The alpha runs this over SSH on a beta that leads; it is
    /// not for typing.
    Attach {
        /// The attach socket (default: the peering directory's).
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Hand the lead to a peer: `alpha`, or a beta's spec.
    Yield {
        /// Who leads next.
        #[arg(long)]
        to: String,
        /// The state root of the supervisor to ask.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
}

/// The synchronization mode, as expressed on the command line.
#[derive(Clone, Copy, ValueEnum)]
enum ModeArgument {
    /// Both directions; a file changed on both sides is a conflict,
    /// reported and left alone.
    #[value(name = "two-way-conflict", alias = "two-way-safe")]
    TwoWaySafe,
    /// two-way-conflict, and a large directory emptied on one side is a
    /// conflict too, rather than a deletion to propagate.
    #[value(name = "two-way-paranoid")]
    TwoWayParanoid,
    /// Both directions; a file changed on both sides takes alpha's
    /// version, silently.
    #[value(name = "two-way-alpha", alias = "two-way-resolved")]
    TwoWayResolved,
    /// two-way-alpha, and alpha's deletion of a file beta edited wins too.
    #[value(name = "two-way-alpha-strict")]
    TwoWayStrict,
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
            ModeArgument::TwoWayParanoid => SyncMode::TwoWayParanoid,
            ModeArgument::TwoWayResolved => SyncMode::TwoWayResolved,
            ModeArgument::TwoWayStrict => SyncMode::TwoWayStrict,
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
        /// Show every destination in full, including groups that are all
        /// synchronized (which otherwise take one line each).
        #[arg(long)]
        all: bool,
    },
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
    Mi {
        /// The configuration file (defaults to ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Run the menu bar app: an icon whose colour is the state of every
    /// session, a menu with the detail, and the ways to settle each
    /// conflict. Experimental. Built with the `tray` feature.
    Tray {
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
    /// Run as a synchronization agent on standard input/output (invoked on
    /// remote hosts by the sync command; not intended for interactive use).
    ///
    /// The protocol it speaks is autobahn's own and changes between
    /// releases without notice — every agent must be the controller's
    /// exact version, which the controller enforces. It is not an API,
    /// and nothing but autobahn should drive it.
    Agent,
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
        /// Also remove superseded agent binaries from the remote hosts this
        /// configuration names. Agents are installed per version and
        /// nothing has ever removed them, so a host accumulates one ~5 MB
        /// binary per version it has ever been contacted by. Off by
        /// default: everything else here is local, and this reaches out
        /// over SSH to every configured host.
        #[arg(long)]
        agents: bool,
        /// How many superseded agent binaries to leave on each host, so an
        /// older controller reconnecting still finds its agent in place.
        /// The version in use is always kept and does not count.
        #[arg(long, value_name = "N", default_value_t = 1)]
        keep_agents: usize,
        /// Also remove the state of sessions the configuration describes
        /// but has turned off. Kept by default, so enabling a group or a
        /// host resumes where it left off rather than re-merging.
        #[arg(long)]
        include_disabled: bool,
        /// Remove disabled sessions' state without asking.
        #[arg(long, requires = "include_disabled")]
        yes: bool,
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
    /// Turn a host or a group off in the configuration. A disabled host
    /// drops from every group it appears in; a disabled group runs
    /// nothing at all. Neither loses its state: enabling resumes.
    Disable {
        /// The host to disable everywhere.
        #[arg(long, group = "target")]
        host: Option<String>,
        /// The group to disable.
        #[arg(long, group = "target")]
        group: Option<String>,
        /// The configuration file (defaults to ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Turn a host or a group back on: the counterpart of `disable`.
    Enable {
        /// The host to enable.
        #[arg(long, group = "target")]
        host: Option<String>,
        /// The group to enable.
        #[arg(long, group = "target")]
        group: Option<String>,
        /// The configuration file (defaults to ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Write a starting configuration: the defaults, every mode explained,
    /// and one example group to edit. Refuses to replace one that exists.
    Init {
        /// Where to write it (defaults to ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Replace an existing configuration, keeping the old one beside it.
        #[arg(long)]
        force: bool,
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
    /// Peering (experimental): attach to a leader, or hand the lead on.
    Peering {
        #[command(subcommand)]
        verb: PeeringVerb,
    },
    /// A look at a group's sessions: what each side holds, how the two
    /// differ, whether the baseline reads, what the next cycle would do,
    /// and what a reset would do. Changes neither folder nor the baseline
    /// (a scan refreshes its scan cache, as every scan does), and runs
    /// beside a supervisor.
    Doctor {
        /// The group to look at, by its name or by its folder.
        group: String,
        /// Filter to a destination within the group.
        host: Option<String>,
        /// The configuration file (defaults to ~/.autobahn/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the state root (defaults to ~/.autobahn).
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Prints the baseline formats this build reads, for `autobahn update`
    /// to ask a downloaded build before installing it.
    #[command(hide = true)]
    Formats,
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
    /// Install the latest release over this one: the command, and the
    /// agent bundle the controller streams to remote hosts.
    ///
    /// The bundle is refreshed before the login service restarts, so a
    /// controller never comes back on a version whose agents it cannot
    /// install. The binary it replaces is kept beside the new one, and
    /// restored if the service does not come back.
    Update {
        /// Install this release rather than the latest stable one.
        /// Prereleases are never picked up by default: a tester opts in
        /// here by tag (`--version v0.5.0-dev.1`).
        #[arg(long, value_name = "TAG")]
        version: Option<String>,
        /// Install the command here (defaults to ~/.local/bin, or
        /// $AUTOBAHN_BIN_DIR).
        #[arg(long, value_name = "DIR", alias = "prefix")]
        bin_dir: Option<PathBuf>,
        /// Leave the agent bundle alone. Only safe when every host you
        /// synchronize with shares this machine's platform.
        #[arg(long)]
        no_agents: bool,
        /// Report what would be installed, and where, without changing
        /// anything.
        #[arg(long)]
        dry_run: bool,
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
        /// Write the detail needed to explain a cycle after it has gone:
        /// timings, what content was asked for and whether it arrived, and
        /// what the supervisor decided next. Equivalent to `log = "debug"`
        /// in the configuration, or `AUTOBAHN_LOG=debug`.
        #[arg(long)]
        debug: bool,
    },
}

/// Whether this process is the app bundle's executable, started with no
/// arguments — which is how macOS launches one.
///
/// The check is the path, not an environment variable: a bundle's
/// executable always sits at `…/Contents/MacOS/`, and anything that
/// reaches this binary through a shell has arguments.
fn bundled_launch() -> bool {
    if std::env::args_os().nth(1).is_some() {
        return false;
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            let parent = path.parent()?.to_owned();
            Some(parent.ends_with("Contents/MacOS"))
        })
        .unwrap_or(false)
}

fn main() {
    // Double-clicked inside the app bundle, macOS runs the executable with
    // no arguments. There is no other way for a bundle to say what its
    // binary should do, and the binary is the same one the terminal runs.
    let cli = match bundled_launch() {
        true => Cli {
            command: Command::Tray {
                config: None,
                state_root: None,
            },
        },
        false => Cli::parse(),
    };
    let result = match cli.command {
        Command::Agent => serve_agent(std::io::stdin().lock(), std::io::stdout()),
        Command::Peering { verb } => run_peering(verb),
        Command::Watch {
            config,
            state_root,
            conflicts,
            log,
            debug,
        } => run_watch(config, state_root, conflicts, log, debug),
        Command::Install { config, state_root } => {
            autobahn::service::install(config.as_deref(), state_root.as_deref()).map(|()| {
                println!("installed and started the login service");
            })
        }
        Command::Uninstall => autobahn::service::uninstall().map(|()| {
            println!("stopped and unregistered the login service");
        }),
        Command::Start => check_startable(None).and_then(|()| {
            autobahn::service::start().map(|()| {
                println!("started the login service");
            })
        }),
        Command::Stop => autobahn::service::stop().map(|()| {
            println!(
                "stopped the login service (it returns at the next login; `uninstall` \
                 removes it)"
            );
        }),
        Command::Restart => check_startable(None).and_then(|()| {
            autobahn::service::restart().map(|()| {
                println!("restarted the login service");
            })
        }),
        Command::Status {
            config,
            state_root,
            group,
            host,
            conflicts,
            live,
            json,
            all,
        } => {
            // Naming a group is asking about it: shown in full.
            let expand = all || group.is_some();
            run_status(
                config, state_root, group, host, conflicts, live, json, expand,
            )
        }
        Command::Doctor {
            group,
            host,
            config,
            state_root,
        } => run_doctor(config, state_root, &group, host.as_deref()),
        Command::Formats => {
            let (oldest, newest) = autobahn::session::ancestor::readable_formats();
            println!("ancestor {oldest} {newest}");
            Ok(())
        }
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
        Command::Disable {
            host,
            group,
            config,
        } => run_availability(config, host, group, false),
        Command::Enable {
            host,
            group,
            config,
        } => run_availability(config, host, group, true),
        Command::Init { config, force } => run_init(config, force),
        Command::Update {
            version,
            bin_dir,
            no_agents,
            dry_run,
        } => autobahn::update::run(autobahn::update::Options {
            version,
            bin_dir,
            no_agents,
            dry_run,
        }),
        Command::Clean {
            config,
            state_root,
            dry_run,
            agent_staging_older_than,
            agents,
            keep_agents,
            include_disabled,
            yes,
        } => run_clean(
            config,
            state_root,
            dry_run,
            agent_staging_older_than,
            agents,
            keep_agents,
            include_disabled,
            yes,
        ),
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
            // On a thread with room for a deep tree, not the main thread.
            autobahn::threads::run_deep(move || {
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
            })
        }),
    };
    if let Err(error) = result {
        eprintln!("autobahn: {error:#}");
        std::process::exit(match error.downcast_ref::<Unsettled>() {
            Some(_) => 2,
            None => 1,
        });
    }
}

/// A sync that finished its pass but left conflicts or blocked paths
/// behind. It exits 2 rather than 1, so a script can tell "stopped with
/// something to settle" from "could not run"; see `docs/commands.md`.
#[derive(Debug)]
struct Unsettled(String);

impl std::fmt::Display for Unsettled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Unsettled {}

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
    //
    // A leading `~` is expanded first, as a configured endpoint's is: a
    // quoted `'~/backup'` means home here too, not a directory named `~`.
    let frozen_of = |spec: &str, agent: &Option<String>| -> Result<Option<PathBuf>> {
        if agent.is_some() || parse_remote(spec).is_some() {
            Ok(None)
        } else {
            Ok(Some(paths::resolve_for_identity(&paths::expand_tilde(
                spec,
            )?)))
        }
    };
    let alpha_frozen = frozen_of(&alpha, &alpha_agent)?;
    let beta_frozen = frozen_of(&beta, &beta_agent)?;
    let identity_from = |spec: &str, frozen: &Option<PathBuf>| -> String {
        match frozen {
            Some(path) => path.to_string_lossy().into_owned(),
            None => spec.to_owned(),
        }
    };
    let alpha_identity = identity_from(&alpha, &alpha_frozen);
    let beta_identity = identity_from(&beta, &beta_frozen);
    let identifier = session_identifier(&alpha_identity, &beta_identity);
    // The topology a configured session is held to, before anything opens.
    let target = |spec: &str, agent: &Option<String>, frozen: &Option<PathBuf>| {
        autobahn::config::EndpointTarget::manual(spec, agent.as_deref(), frozen.as_deref())
    };
    let alpha_target = target(&alpha, &alpha_agent, &alpha_frozen);
    let beta_target = target(&beta, &beta_agent, &beta_frozen);
    autobahn::config::check_session_topology(
        &alpha_target,
        &beta_target,
        &alpha_identity,
        &beta_identity,
    )
    .map_err(|problem| anyhow::anyhow!("{alpha} and {beta}: {problem}"))?;
    let state_directory = match state_dir {
        Some(directory) => directory,
        None => {
            let state_root = paths::default_state_root()?;
            paths::prepare_state_root(&state_root)?;
            state_root.join("sessions").join(&identifier)
        }
    };
    autobahn::config::OwnState::new(&state_directory, None)
        .check_session(
            (&alpha_target, &alpha_identity),
            (&beta_target, &beta_identity),
            &ignores,
        )
        .map_err(|problem| anyhow::anyhow!(problem))?;
    // Credentials are synchronized as asked, and said so first.
    for (target, identity) in [
        (&alpha_target, &alpha_identity),
        (&beta_target, &beta_identity),
    ] {
        if let (autobahn::config::EndpointTarget::Local(_), Some(warning)) = (
            target,
            autobahn::config::secrets_warning(identity, &ignores),
        ) {
            eprintln!("warning: {warning}; pass --ignore for each to leave them out");
        }
    }
    // The ancestor within says which paths exist and what they hold.
    if let Some(parent) = state_directory.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("unable to create {}", parent.display()))?;
    }
    autobahn::fsutil::private_dir(&state_directory)?;

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
            // A one-shot never waits, so its roots are not watched;
            // --watch turns this into a session that does.
            one_shot: !watch,
            ignore_mounts: true,
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
        ignore_mounts: true,
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
            let conflicts = report.conflicts.len();
            let blocked = report.alpha_scan_problems.len()
                + report.beta_scan_problems.len()
                + report.alpha_transition_problems.len()
                + report.beta_transition_problems.len();
            if conflicts > 0 || blocked > 0 {
                return Err(Unsettled(format!(
                    "{conflicts} conflict(s) and {blocked} blocked path(s) remain"
                ))
                .into());
            }
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

/// Looks at a group's sessions without changing anything.
///
/// Built for the moment something looks wrong, and for the moment before a
/// `reset`: it answers whether the two sides already match, which is
/// whether a reset is free — and when they do not, what it would bring
/// back or overwrite.
fn run_doctor(
    config: Option<PathBuf>,
    state_root: Option<PathBuf>,
    group: &str,
    host: Option<&str>,
) -> Result<()> {
    use autobahn::tree::reconcile;
    let plans = load_config(config)?.plans()?;
    let state_root = resolve_state_root(state_root)?;
    let selection = select(&plans, Some(group), host)?;
    let pool = autobahn::transport::mux::AgentPool::default();
    let mut failures = 0usize;
    for plan in selection.plans {
        println!(
            "\x1b[1m{}\x1b[0m → {}  \x1b[2m{} · {}\x1b[0m",
            plan.alpha_spec,
            plan.beta_spec(),
            plan.group,
            plan.mode_name()
        );
        let scanned = (|| -> Result<_> {
            let (mut alpha, mut beta) =
                autobahn::supervisor::open_endpoints(plan, &state_root, &pool)?;
            let alpha = alpha.scan().context("unable to scan alpha")?;
            let beta = beta.scan().context("unable to scan beta")?;
            Ok((alpha, beta))
        })();
        let (alpha, beta) = match scanned {
            Ok(scanned) => scanned,
            Err(error) => {
                println!("  \x1b[31mcannot look\x1b[0m: {error:#}\n");
                failures += 1;
                continue;
            }
        };
        for (side, snapshot) in [("alpha", &alpha), ("beta", &beta)] {
            match &snapshot.root {
                None => println!("  {side}: \x1b[33mmissing\x1b[0m"),
                Some(_) => {
                    let count = |value: u64, word: &str| {
                        let plural = if value == 1 { "" } else { "s" };
                        format!("{} {word}{plural}", thousands(value))
                    };
                    println!(
                        "  {side}: {}, {}, {}, {}",
                        count(snapshot.files, "file"),
                        count(snapshot.directories, "folder"),
                        count(snapshot.symlinks, "link"),
                        format_bytes(snapshot.total_file_size)
                    )
                }
            }
        }

        // The baseline, read without writing: a supervisor may be running.
        let checkpoint = state_root
            .join("sessions")
            .join(plan.identifier())
            .join("ancestor");
        let ancestor = autobahn::session::ancestor::peek(&checkpoint);
        match &ancestor {
            Ok((None, _)) => println!("  baseline: none yet — no cycle has completed"),
            Ok((Some(_), generation)) => {
                println!(
                    "  baseline: readable, generation {}",
                    thousands(*generation)
                )
            }
            Err(error) => println!("  baseline: \x1b[31munreadable\x1b[0m — {error:#}"),
        }

        let describe = |reconciliation: &autobahn::tree::Reconciliation| -> Vec<String> {
            let mut lines = Vec::new();
            for (change, direction) in reconciliation
                .alpha_transitions
                .iter()
                .map(|change| (change, "to alpha"))
                .chain(
                    reconciliation
                        .beta_transitions
                        .iter()
                        .map(|change| (change, "to beta")),
                )
            {
                let verb = match (&change.old, &change.new) {
                    (None, Some(_)) => "copy",
                    (Some(_), None) => "delete",
                    _ => "replace",
                };
                let path = if change.path.is_empty() {
                    "(the root)"
                } else {
                    &change.path
                };
                lines.push(format!("{verb} {path} {direction}"));
            }
            for conflict in &reconciliation.conflicts {
                let path = if conflict.root.is_empty() {
                    "(the root)"
                } else {
                    &conflict.root
                };
                lines.push(format!("conflict at {path}"));
            }
            lines
        };
        let show = |heading: &str, lines: &[String]| {
            println!("  {heading}");
            for line in lines.iter().take(8) {
                println!("    {line}");
            }
            if lines.len() > 8 {
                println!("    … {} more", thousands((lines.len() - 8) as u64));
            }
        };

        // What the next cycle would do: the baseline against both sides.
        let mode = plan.mode;
        if let Ok((baseline, _)) = &ancestor {
            let next = describe(&reconcile(
                baseline.as_ref(),
                alpha.root.as_ref(),
                beta.root.as_ref(),
                mode,
            ));
            if next.is_empty() {
                println!("  the next cycle: nothing to do — in sync");
            } else {
                show(&format!("the next cycle would ({}):", next.len()), &next);
            }
        }

        // What a reset would do: the same, with no history at all.
        let reset = describe(&reconcile(
            None,
            alpha.root.as_ref(),
            beta.root.as_ref(),
            mode,
        ));
        if reset.is_empty() {
            println!("  a reset: \x1b[32mfree\x1b[0m — the two sides match");
        } else {
            show(
                &format!(
                    "a reset would ({}) — with no baseline, whatever is on one side only is \
                     copied to the other, including what was deleted on purpose:",
                    reset.len()
                ),
                &reset,
            );
        }

        // Folders populated on one side and empty or gone on the other: the
        // shape of a vanished mount, and of an emptied tree.
        let mut lopsided = Vec::new();
        lopsided_folders(alpha.root.as_ref(), beta.root.as_ref(), "", &mut lopsided);
        if lopsided.is_empty() {
            println!("  folders full on one side and empty on the other: none");
        } else {
            show(
                "folders full on one side and empty on the other:",
                &lopsided,
            );
        }
        println!();
    }
    if failures > 0 {
        bail!("{failures} session(s) could not be looked at");
    }
    Ok(())
}

/// Walks two trees together, collecting the folders that hold eight or
/// more entries on one side and are empty or absent on the other.
fn lopsided_folders(
    alpha: Option<&autobahn::tree::Node>,
    beta: Option<&autobahn::tree::Node>,
    path: &str,
    found: &mut Vec<String>,
) {
    use autobahn::tree::{Content, Node};
    fn below(node: &Node) -> usize {
        node.children().iter().map(|child| 1 + below(child)).sum()
    }
    let directory = |node: Option<&Node>| matches!(node, Some(node) if matches!(node.content, Content::Directory(_)));
    let empty =
        |node: Option<&Node>| !directory(node) || node.is_some_and(|n| n.children().is_empty());
    if !path.is_empty() && empty(alpha) != empty(beta) {
        let (populated, side) = if empty(alpha) {
            (beta, "beta")
        } else {
            (alpha, "alpha")
        };
        let count = populated.map(below).unwrap_or(0);
        if count >= 8 {
            found.push(format!(
                "{path} — {} entries on {side}, empty or gone on the other",
                thousands(count as u64)
            ));
            return;
        }
    }
    if !directory(alpha) || !directory(beta) {
        return;
    }
    let left = alpha.map(Node::children).unwrap_or(&[]);
    let right = beta.map(Node::children).unwrap_or(&[]);
    let mut names: Vec<&str> = left
        .iter()
        .chain(right.iter())
        .map(|child| child.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let child_path = if path.is_empty() {
            name.to_owned()
        } else {
            format!("{path}/{name}")
        };
        lopsided_folders(
            alpha.and_then(|node| node.child(name)),
            beta.and_then(|node| node.child(name)),
            &child_path,
            found,
        );
    }
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
        ControlResponse::Sessions(_) => bail!("the supervisor answered with its sessions"),
        // `send` turns this into an error with the remedy; kept for the
        // match to be whole.
        ControlResponse::Mismatch { supervisor } => bail!(
            autobahn::supervisor::control::mismatch_message(Some(&supervisor))
        ),
    }
}

/// Loads the groups configuration from an explicit or default path.
/// On a peer — a machine a leader pushed a name to, with no configuration
/// of its own — the plans are the leader's star turned around, and a line
/// says what the peer is doing about the lease. `None` anywhere else.
fn peer_plans(
    config: &Option<PathBuf>,
) -> Result<Option<(Vec<autobahn::config::SessionPlan>, String)>> {
    if config.is_some() {
        return Ok(None);
    }
    let directory = autobahn::peering::directory()?;
    if !autobahn::supervisor::peer::is_peer(&directory) || paths::default_config_path()?.is_file() {
        return Ok(None);
    }
    let Some((configuration, name)) = autobahn::peering::pushed_configuration(&directory)? else {
        return Ok(None);
    };
    let star = autobahn::peering::derive_star(&configuration, &name, &directory)?;
    let header = match autobahn::supervisor::peer::read_status(&directory)? {
        Some(status) => match &status.lease {
            Some(lease) => format!(
                "peer {name}: {} leads at term {}; the lease is {}{}\n",
                lease.leader,
                lease.term,
                status.standing,
                match status.standing.as_str() {
                    "fresh" => String::new(),
                    _ => format!(
                        " for {}s (this peer acts after {}s)",
                        status.stale_seconds, status.wait_seconds
                    ),
                }
            ),
            None => format!("peer {name}: no lease yet; waiting for the leader\n"),
        },
        None => format!("peer {name}: not running; start it with `autobahn watch`\n"),
    };
    Ok(Some((star.plans, header)))
}

/// `autobahn peering …`.
fn run_peering(verb: PeeringVerb) -> Result<()> {
    match verb {
        PeeringVerb::Attach { socket } => {
            let socket = match socket {
                Some(socket) => socket,
                None => autobahn::peering::directory()?.join(autobahn::peering::ATTACH_SOCKET),
            };
            let stream = std::os::unix::net::UnixStream::connect(&socket).with_context(|| {
                format!(
                    "unable to reach a leading supervisor at {} (is this host leading?)",
                    socket.display()
                )
            })?;
            // The greeting, then two pumps until either side closes.
            let mut writer = stream.try_clone().context("unable to clone the socket")?;
            std::io::Write::write_all(
                &mut writer,
                format!("{}\n", autobahn::peering::ALPHA).as_bytes(),
            )
            .context("unable to greet the supervisor")?;
            let mut reader = stream;
            let inbound = std::thread::spawn(move || {
                // Not `io::copy` into stdout: that goes through a line
                // buffer, and frames have no newlines to flush them.
                use std::io::{Read, Write};
                let mut stdout = std::io::stdout().lock();
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => {
                            if stdout.write_all(&buffer[..count]).is_err()
                                || stdout.flush().is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            });
            let mut stdin = std::io::stdin().lock();
            let _ = std::io::copy(&mut stdin, &mut writer);
            let _ = writer.shutdown(std::net::Shutdown::Write);
            let _ = inbound.join();
            Ok(())
        }
        PeeringVerb::Yield { to, state_root } => {
            let state_root = resolve_state_root(state_root)?;
            match autobahn::supervisor::control::send(
                &state_root,
                &autobahn::supervisor::control::ControlRequest::Yield { to: to.clone() },
            )? {
                autobahn::supervisor::control::ControlResponse::Applied { sessions } => {
                    println!("handing the lead to {to}; {sessions} session(s) will pass it on");
                    Ok(())
                }
                autobahn::supervisor::control::ControlResponse::Error(message) => {
                    bail!("{message}")
                }
                other => bail!("unexpected answer: {other:?}"),
            }
        }
    }
}

/// What the supervisor would refuse at startup, checked before the
/// service is started or restarted: a configuration that does not load,
/// or describes no sessions. `start` and `restart` ask the service manager
/// to run the supervisor and report success as soon as it has been asked,
/// while a supervisor that then finds a bad configuration exits into the
/// service log, unseen — and a `restart` over an edit with a typo takes
/// the running service down for it. So the same checks run here, and a
/// refusal is the same message the supervisor would have logged, with the
/// service left as it was. A peer runs a pushed configuration instead of
/// its own, and is not checked.
fn check_startable(config: Option<PathBuf>) -> Result<()> {
    if config.is_none() {
        let directory = autobahn::peering::directory()?;
        if autobahn::supervisor::peer::is_peer(&directory) {
            return Ok(());
        }
    }
    let path = match config {
        Some(path) => path,
        None => paths::default_config_path()?,
    };
    // The same checks the supervisor makes at startup. A running one
    // makes them again on every edit, but applies an edit that disables
    // every session rather than refusing it.
    let loaded = autobahn::supervisor::reload::load_for_startup(&path)
        .context("the configuration would stop the supervisor at startup")?;
    autobahn::config::OwnState::new(&paths::default_state_root()?, Some(&path))
        .check_plans(&loaded.plans)
        .context("the configuration would stop the supervisor at startup")
}

fn load_config(path: Option<PathBuf>) -> Result<Config> {
    let path = match path {
        Some(path) => path,
        None => paths::default_config_path()?,
    };
    Config::load(&path)
}

/// Resolves the state root from an explicit override or the default, and
/// makes it private (see `paths::prepare_state_root`).
fn resolve_state_root(state_root: Option<PathBuf>) -> Result<PathBuf> {
    let root = match state_root {
        Some(root) => root,
        None => paths::default_state_root()?,
    };
    autobahn::scan::exclude_state_root(&root);
    paths::prepare_state_root(&root)?;
    Ok(root)
}

/// Runs the supervisor over the configured sessions.
/// One pass over every configured session, then exit: 1 if any session
/// failed, else 2 if any left conflicts or blocked paths, else 0.
fn run_sync_config(config: Option<PathBuf>, state_root: Option<PathBuf>) -> Result<()> {
    let config = config.map_or_else(paths::default_config_path, Ok)?;
    let plans = load_config(Some(config.clone()))?.plans()?;
    if plans.is_empty() {
        bail!("the configuration describes no sessions");
    }
    let state_root = resolve_state_root(state_root)?;
    autobahn::config::OwnState::new(&state_root, Some(&config)).check_plans(&plans)?;
    for warning in autobahn::config::secret_warnings(&plans) {
        eprintln!("warning: {warning}");
    }
    let supervisor = Supervisor::new(plans, state_root, false);
    let outcomes = supervisor.run_once();
    let mut failures = 0usize;
    let mut unsettled = 0usize;
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
                if digest.conflicts > 0 || digest.problems > 0 {
                    unsettled += 1;
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
    if unsettled > 0 {
        return Err(Unsettled(format!(
            "{unsettled} session(s) left conflicts or blocked paths"
        ))
        .into());
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
    debug: bool,
) -> Result<()> {
    let config_path = match &config {
        Some(path) => path.clone(),
        None => paths::default_config_path()?,
    };
    // A peer — a machine some leader pushed a name to — runs the leader's
    // configuration, turned around, and not one of its own. The two
    // cannot run side by side yet, so a configuration of its own is
    // refused rather than quietly ignored.
    if config.is_none() {
        let directory = autobahn::peering::directory()?;
        if autobahn::supervisor::peer::is_peer(&directory) {
            if config_path.is_file() {
                bail!(
                    "this machine is a peer of another autobahn (it holds {}), and a                      configuration of its own at {} cannot run alongside that yet; move one                      of them aside",
                    directory.join("name").display(),
                    config_path.display()
                );
            }
            autobahn::logging::set_level(match debug {
                true => Some(autobahn::logging::Level::Debug),
                false => None,
            });
            let state_root = resolve_state_root(state_root)?;
            println!("following as a peer; status is available via `autobahn status`");
            let stop = std::sync::atomic::AtomicBool::new(false);
            return autobahn::supervisor::peer::run(&directory, &state_root, !log, &stop);
        }
    }
    let mut loaded = autobahn::supervisor::reload::load_for_startup(&config_path)?;
    // The file is watched while the sessions run, unless it says not to.
    let mut reloader = loaded.reload.then(|| {
        Arc::new(autobahn::supervisor::reload::Reloader::new(
            config_path.clone(),
        ))
    });

    // The level is settled before the first line is written. `--debug`
    // beats the file, and `AUTOBAHN_LOG` beats both, so a level can be
    // turned up for one run without editing anything.
    autobahn::logging::set_level(match debug {
        true => Some(autobahn::logging::Level::Debug),
        false => loaded.log_level,
    });
    let state_root = resolve_state_root(state_root)?;
    let own_state = autobahn::config::OwnState::new(&state_root, Some(&config_path));
    own_state.check_plans(&loaded.plans)?;
    for warning in autobahn::config::secret_warnings(&loaded.plans) {
        eprintln!("warning: {warning}");
    }

    // Said before the first cycle, while someone is still looking at the
    // terminal. These sessions will stop on their own anyway; the point is
    // that the reader learns it now rather than from a status page later.
    for (session, problem) in autobahn::supervisor::unreadable_ancestors(&loaded.plans, &state_root)
    {
        eprintln!("[{session}] {problem}");
    }
    let live_display = !log && unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;

    // What the display draws: replaced when an edit to the configuration
    // is applied, so the sessions it shows are the ones running.
    let shown: Arc<Mutex<Vec<autobahn::config::SessionPlan>>> =
        Arc::new(Mutex::new(loaded.plans.clone()));

    // Runs the configuration, and then every edit that loads, until the
    // process is terminated: agent processes exit when their connection
    // streams close, so no explicit cleanup is needed.
    let sessions = loaded.plans.len();
    let mut supervise = {
        let shown = shown.clone();
        let state_root = state_root.clone();
        move |verbose: bool| -> Result<()> {
            let stop = std::sync::atomic::AtomicBool::new(false);
            loop {
                // Peering, when any group asks for it. This machine is the
                // configured alpha of every such group (the configuration
                // says so), so it leads — unless its own lease file says a
                // beta led while it was away.
                let peering = loaded.plans.iter().any(|plan| plan.peering.is_some());
                if peering {
                    autobahn::supervisor::peer::run_alpha(
                        &config_path,
                        &autobahn::peering::directory()?,
                        &loaded.plans,
                        &loaded.alerts,
                        &state_root,
                        verbose,
                        &stop,
                        reloader.as_ref(),
                    )?;
                } else {
                    Supervisor::new(loaded.plans.clone(), state_root.clone(), verbose)
                        .with_alerts(loaded.alerts.clone())
                        .with_log_level(loaded.log_level)
                        .with_configuration(loaded.text.clone())
                        .with_shown(shown.clone())
                        .with_own_state(autobahn::config::OwnState::new(
                            &state_root,
                            Some(&config_path),
                        ))
                        .with_reload(reloader.clone())
                        .run_watch(&stop)?;
                }
                let Some(next) = reloader.as_ref().and_then(|reloader| reloader.take()) else {
                    return Ok(());
                };
                if let Err(error) = own_state.check_plans(&next.plans) {
                    autobahn::complain!("the edited configuration is not applied: {error:#}");
                    continue;
                }
                autobahn::logging::set_level(match debug {
                    true => Some(autobahn::logging::Level::Debug),
                    false => next.log_level,
                });
                for (session, problem) in
                    autobahn::supervisor::unreadable_ancestors(&next.plans, &state_root)
                {
                    autobahn::complain!("[{session}] {problem}");
                }
                autobahn::note!(
                    "supervising {} session(s) under the edited configuration",
                    next.plans.len()
                );
                // An edit that turns the watch off is the last one applied
                // in place; turning it back on lands on `restart`.
                if !next.reload {
                    reloader = None;
                }
                *shown.lock().unwrap_or_else(|error| error.into_inner()) = next.plans.clone();
                loaded = next;
            }
        }
    };

    if !live_display {
        // Written as the log is, so a closed standard output costs the line
        // and not the supervisor.
        use std::io::Write;
        let _ = writeln!(
            std::io::stdout(),
            "supervising {sessions} session(s); status is available via `autobahn status`"
        );
        return supervise(true);
    }

    // The supervisor runs on its own thread and writes status records as
    // it goes; this thread reads them back and repaints. The records are
    // the same ones `autobahn status` reads, so the two never disagree.
    let display_root = state_root.clone();
    // A supervisor that fails has nothing left to display, so it asks the
    // display to leave and hands its failure back here to be reported —
    // after the terminal has been restored, and by the thread that owns
    // the exit status.
    let failure: Arc<Mutex<Option<String>>> = Arc::default();
    let reported = failure.clone();
    std::thread::spawn(move || {
        if let Err(error) = supervise(false) {
            *reported.lock().unwrap_or_else(|error| error.into_inner()) =
                Some(format!("{error:#}"));
            pager::leave();
        }
    });

    pager::display("watching", || {
        let plans = shown.lock().unwrap_or_else(|error| error.into_inner());
        let selected: Vec<&autobahn::config::SessionPlan> = plans.iter().collect();
        let mut frame = String::new();
        render_status(
            &selected,
            &display_root,
            expand_conflicts,
            false,
            true,
            &mut frame,
        );
        frame
    })?;
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
    expand: bool,
    label: &str,
) -> Result<()> {
    pager::display(label, || {
        let mut frame = String::new();
        render_status(
            selected,
            state_root,
            expand_conflicts,
            expand,
            true,
            &mut frame,
        );
        frame
    })
}

/// What a selector picked out: the sessions, and — when the selector was a
/// path *inside* a synchronized root — the root-relative remainder.
struct Selection<'a> {
    plans: Vec<&'a autobahn::config::SessionPlan>,
    /// Per plan, in the order of `plans`: the path's remainder below that
    /// plan's alpha root, when the selector was a path deeper than the
    /// root itself. `Some("")` never occurs; the root itself yields `None`.
    /// Nested groups have different roots, so each keeps its own.
    relatives: Vec<Option<String>>,
    /// The remainder every selected plan agrees on — always, within one
    /// group, which has one alpha. Plans in nested groups disagree, and
    /// then this is `None` rather than any one of them.
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
    let selected: Vec<(&autobahn::config::SessionPlan, Option<String>)> = plans
        .iter()
        .filter_map(|plan| match (&folder, selector) {
            (Some(folder), _) => {
                let alpha = std::path::Path::new(&plan.alpha_identity);
                if folder == alpha {
                    Some((plan, None))
                } else if let Ok(rest) = folder.strip_prefix(alpha) {
                    Some((plan, Some(rest.to_string_lossy().into_owned())))
                } else {
                    None
                }
            }
            (None, Some(group)) => (plan.group == group).then_some((plan, None)),
            (None, None) => Some((plan, None)),
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
    let (selected, relatives): (Vec<&autobahn::config::SessionPlan>, Vec<Option<String>>) =
        selected
            .into_iter()
            .filter(|(plan, _)| {
                host.is_none_or(|host| {
                    plan.host == host
                        || plan.beta_spec() == host
                        || host_identity.as_deref() == Some(plan.beta_identity.as_str())
                })
            })
            .unzip();
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
    let relative = match relatives.split_first() {
        Some((first, rest)) if rest.iter().all(|other| other == first) => first.clone(),
        _ => None,
    };
    Ok(Selection {
        plans: selected,
        relatives,
        relative,
    })
}

/// The root-relative path a command was given: explicitly, or as the
/// remainder of a path selector. One path for every selected plan, so a
/// path selector that lies in nested groups — a different remainder under
/// each root — is refused here; see [`relative_in`].
fn relative_path(selection: &Selection, explicit: Option<String>) -> Result<String> {
    // Plans with remainders but no common one disagree about the path.
    if explicit.is_none()
        && selection.relative.is_none()
        && selection.relatives.iter().any(Option::is_some)
    {
        bail!(
            "that path lies in more than one group, nested one inside another; name the \
             group and give the path relative to its root"
        );
    }
    relative_in(selection, 0, explicit)
}

/// The root-relative path a command was given, for the selected plan at
/// `index`: explicitly, or as the remainder of a path selector under that
/// plan's own root.
fn relative_in(selection: &Selection, index: usize, explicit: Option<String>) -> Result<String> {
    match (explicit, selection.relatives.get(index).cloned().flatten()) {
        (Some(path), _) => Ok(path
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_owned()),
        (None, Some(rest)) => Ok(rest),
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

/// A root-relative path as it goes into a command to paste: bare when
/// the shell and the command line would read it as itself, quoted as one
/// word otherwise, and
/// shown escaped rather than quoted when it holds a control character,
/// since no quoting makes a newline safe to paste.
fn pasteable(path: &str) -> String {
    if path.chars().any(char::is_control) {
        return display_safe(path).into_owned();
    }
    // A leading `-` would be read as a flag, quoted or not; `./` in front
    // keeps it a path, and `resolve` takes the `./` off again.
    let path = match path.starts_with('-') {
        true => format!("./{path}"),
        false => path.to_owned(),
    };
    let plain = |c: char| c.is_alphanumeric() || "._/-+@,:%".contains(c);
    if !path.is_empty() && path.chars().all(plain) {
        path
    } else {
        autobahn::text::shell_quote(&path)
    }
}

/// `path` quoted as one shell word, except that a leading `~` stays
/// outside the quotes as `"$HOME"`, so a root written home-relative still
/// expands on the side that runs the command.
fn home_quoted(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", autobahn::text::shell_quote(rest)),
        None if path == "~" => "\"$HOME\"".to_owned(),
        None => autobahn::text::shell_quote(path),
    }
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
        // The command is meant to be pasted into a shell, and the path in
        // it is made of names the other side chose. Every value is quoted
        // as one word, so a `;`, a `$(…)` or a quote in a name is only a
        // name — and a name with a control character in it is not put in
        // a command at all, since no quoting makes a newline safe to
        // paste.
        let (remote, root) = match side {
            "beta" => {
                let spec = plan.beta_spec();
                match spec.split_once(':') {
                    Some((destination, root)) => (Some(destination.to_owned()), root.to_owned()),
                    None => (None, spec),
                }
            }
            _ => (None, plan.alpha_spec.clone()),
        };
        let path = format!("{root}/{where_}");
        if path.chars().any(char::is_control) {
            fixes.push(format!(
                "fix permissions on {} by hand",
                autobahn::text::display_safe(&path)
            ));
        } else {
            match remote {
                // Quoted twice: once for the remote shell, which runs the
                // inner command, and once for the local one, which hands
                // that command to ssh as a single argument. The user is
                // asked of the remote shell, since a destination without a
                // `user@` names only a host.
                Some(destination) => {
                    let inner = format!("sudo chown -R \"$(id -un)\" {}", home_quoted(&path));
                    fixes.push(format!(
                        "ssh {} {}",
                        autobahn::text::shell_quote(&destination),
                        autobahn::text::shell_quote(&inner)
                    ));
                }
                None => fixes.push(format!(
                    "sudo chown -R \"$(whoami)\" {}",
                    home_quoted(&path)
                )),
            }
        }
        // An ignore is only the answer for something generated. The
        // deepest hidden directory in the path is that; the last segment
        // is often a version ("0.9.10") or a real folder ("static"), and
        // ignoring either would be worse than the permissions.
        if let Some(name) = prefix.split('/').rev().find(|name| name.starts_with('.')) {
            fixes.push(format!(
                "or add \"{}\" to the group's ignores",
                autobahn::text::display_safe(name)
            ));
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
    // Every line goes out through `style`, so a pipe or `NO_COLOR` gets
    // the same words without the colour.
    macro_rules! say {
        () => {
            style::emit("\n")
        };
        ($($argument:tt)*) => {
            style::emit(&format!("{}\n", format_args!($($argument)*)))
        };
    }
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
        say!("{}", serde_json::to_string_pretty(&report)?);
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
                say!();
            }
            current_group = Some(plan.group.as_str());
            say!(
                "\x1b[1m{}\x1b[0m \x1b[2m{}\x1b[0m",
                plan.alpha_spec,
                plan.group
            );
        }
        say!("  {}", plan.beta_spec());
        total += selected.len() + blocked.len();

        if failed {
            total += 1;
            say!("\n    \x1b[31m{state}\x1b[0m");
            if let Some(error) = &status.error {
                say!(
                    "      {}",
                    display_safe(error.trim_start_matches("halted: "))
                );
            }
            if state == "halted" {
                say!("      fix: make the two sides agree, then it resumes");
            }
        }

        if selected.is_empty() && blocked.is_empty() {
            continue;
        }
        if !selected.is_empty() {
            say!(
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
                let shown = display_safe(&prefix);
                match count {
                    1 if selected.contains(&&prefix) => say!("    {shown}"),
                    1 => say!("    {shown} — 1 conflict"),
                    count => say!("    {shown} — {count} conflicts"),
                }
            }
            say!(
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
            say!("      {}", display_safe(path));
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
                say!("        alpha  {}", describe(&detail.alpha));
                say!("        {:<6} {}", plan.host, describe(&detail.beta));
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
                    say!(
                        "        {name} holds {count} synchronization cannot carry, \
                         so neither side is overwritten"
                    );
                    say!(
                        "          {} — {}",
                        display_safe(&blocking.example),
                        display_safe(&blocking.reason)
                    );
                }
            }
        }
        if !selected.is_empty() {
            // A path to paste, so quoted where the shell would read it as
            // something else, and never with a control character in it.
            let where_ = match &scope {
                Some(scope) => pasteable(scope),
                None if selected.len() == 1 => pasteable(selected[0]),
                None => "<path>".to_owned(),
            };
            say!(
                "      fix: autobahn resolve {} {where_} --keep alpha|{}|both",
                pasteable(&plan.group),
                plan.host
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
            say!(
                "\n    \x1b[33m{} on {}\x1b[0m \x1b[2m— {}\x1b[0m",
                match paths.len() {
                    1 => "1 blocked".to_owned(),
                    many => format!("{many} blocked"),
                },
                display_safe(side),
                display_safe(cause)
            );
            // One cause can still cover unrelated places. These twenty
            // are one permission problem in `azure` and another in
            // `arcturus`, and their shared prefix is nothing at all — so
            // the paths are clustered by where they are before the
            // directory they share is named.
            for (prefix, count) in clusters(paths) {
                match (count, prefix.as_str()) {
                    (1, _) => say!(
                        "      {}",
                        display_safe(
                            paths
                                .iter()
                                .find(|path| path.starts_with(&prefix))
                                .copied()
                                .unwrap_or(&prefix)
                        )
                    ),
                    (count, "") => say!("      {count} paths"),
                    (count, prefix) => {
                        say!("      {count} under {}/", display_safe(prefix))
                    }
                }
                for (index, fix) in blocked_fix(side, cause, &prefix, plan).iter().enumerate() {
                    match index {
                        0 => say!("        fix: {fix}"),
                        _ => say!("             {fix}"),
                    }
                }
            }
        }
    }
    if total == 0 {
        match (&scope, &filter) {
            (Some(scope), _) => say!("nothing needs you under {}", display_safe(scope)),
            (None, Some(pattern)) => say!("nothing needs you matching {pattern:?}"),
            (None, None) => say!("nothing needs you"),
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

/// A predicate over root-relative paths, chosen by `conflict_filter`.
type PathFilter = Box<dyn Fn(&str) -> bool>;

/// Builds the predicate behind `conflicts --filter`.
///
/// Two behaviours, chosen by what the pattern looks like, because a
/// filter is typed in a hurry: a plain word is what someone means when
/// they type `--filter arcturus`, and a glob is what they mean when they
/// type `--filter '*.ts'`. A glob without a slash is matched at any depth,
/// the way the same pattern behaves in an ignore file; one with a slash is
/// anchored to the root, since that is what writing the separator asks
/// for.
fn conflict_filter(pattern: &str) -> Result<PathFilter> {
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
    let state_root = resolve_state_root(state_root)?;
    let path = match &config {
        Some(path) => path.clone(),
        None => paths::default_config_path()?,
    };
    let plans = autobahn::supervisor::shown_plans(&path, &state_root)?.plans;
    shop::run(plans, &state_root, config)
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
        Some(scope) => println!("about to resolve {what} under {}:", display_safe(scope)),
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
            println!("  {}", display_safe(path));
        }
    } else {
        for (plan, paths) in targets {
            println!("  {}", plan.beta_spec());
            for path in paths {
                println!("    {}", display_safe(path));
            }
        }
    }
    println!();

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
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
    // Checked up front, so a missing path is refused before any scratch;
    // each session then reads it under its own root.
    relative_in(&selection, 0, path.clone())?;
    let pool = autobahn::transport::mux::AgentPool::default();

    let mut shown = 0;
    for (index, plan) in selection.plans.iter().enumerate() {
        let path = relative_in(&selection, index, path.clone())?;
        let (mut alpha, mut beta) = autobahn::supervisor::open_endpoints(plan, &state_root, &pool)?;
        let a = alpha.read_file(&path)?;
        let b = beta.read_file(&path)?;
        if a == b {
            println!("{}: identical on alpha and {}", path, plan.host);
            continue;
        }
        shown += 1;
        // Both sides go into a private directory of their own under the
        // state root, never the shared temporary directory, where another
        // user could read them, redirect the writes or swap what is
        // compared. It is removed once the diff has run.
        let scratch = autobahn::fsutil::private_tempdir_in(&state_root)?;
        let write = |name: &str, content: &Option<Vec<u8>>| -> Result<PathBuf> {
            use std::io::Write;
            let file = scratch.path().join(name);
            autobahn::fsutil::private_file(&file)?
                .write_all(content.as_deref().unwrap_or(b""))
                .with_context(|| format!("unable to write {}", file.display()))?;
            Ok(file)
        };
        let left = write("alpha", &a)?;
        // Named by side, not by host: a host is not a file name.
        let right = write("beta", &b)?;
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

/// The first entry at or beneath `node` that could not be *scanned*, as a
/// root-relative path and the reason, or `None` when there is none.
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
        // Excluded content is deliberately *not* an obstacle, exactly as
        // it is not one for an ordinary deletion: the removal leaves it
        // where it is, and what remains holds nothing synchronization can
        // see, so it becomes invisible and the conflict settles. Refusing
        // here would make `resolve` stricter than the cycle it stands in
        // for, which is the one thing it must never be.
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

#[allow(clippy::too_many_arguments)] // one parameter per command-line flag
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

    // The root is never a path to settle. Retiring it would retire every
    // synchronized thing under it, which is no one's idea of keeping a
    // version.
    if targets
        .iter()
        .any(|(_, paths)| paths.iter().any(|path| path.is_empty() || path == "."))
    {
        bail!(
            "resolve settles a path inside the root, not the root itself; \
             name the file or folder to settle"
        );
    }

    // Modes that cannot carry the kept version where it has to go. These
    // are refused before anything is read, since no path could succeed.
    match winner {
        Winner::Beta(index)
            if matches!(
                group_plans[index].mode,
                autobahn::tree::SyncMode::OneWaySafe | autobahn::tree::SyncMode::OneWayReplica
            ) =>
        {
            bail!(
                "--keep {keep} cannot work in {mode}: that mode never carries {host}'s \
                 content to alpha, so removing alpha's copy would not make it win. \
                 Nothing was changed",
                mode = group_plans[index].mode_name(),
                host = group_plans[index].host,
            )
        }
        Winner::Both => {
            if let Some((plan, _)) = targets
                .iter()
                .find(|(plan, _)| plan.mode == autobahn::tree::SyncMode::OneWayReplica)
            {
                bail!(
                    "--keep both cannot work in {mode}: {host} is made identical to alpha, \
                     so the copy moved aside there would be deleted. Nothing was changed",
                    mode = plan.mode_name(),
                    host = plan.host,
                )
            }
        }
        _ => {}
    }

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
    // A path inside another one named is settled with it; retiring both
    // would retire the inner one twice.
    let paths: std::collections::BTreeSet<String> = paths
        .iter()
        .filter(|path| {
            !paths
                .iter()
                .any(|outer| path.starts_with(&format!("{outer}/")))
        })
        .cloned()
        .collect();

    let mut settled = 0usize;
    let mut refused: Vec<(String, String)> = Vec::new();
    // Paths that cannot be settled this way at all, as opposed to ones
    // that lost a race. The two need different words: one says try again,
    // the other says this will never work.
    let mut blocked: Vec<(String, String, String, String)> = Vec::new();

    // Which side of each session loses. The winner keeps its version
    // untouched; every other copy in the group is retired, including
    // alpha's when a destination wins, since alpha is how the winning
    // content reaches the group's other destinations.
    //
    // Alpha wins (or both sides are kept, in which case alpha's copy stays
    // put and beta's moves aside): only each beta loses. A destination
    // wins, so alpha loses — and alpha is retired *there*, on the winning
    // session, for two reasons. Every session opens its own handle on
    // alpha, so the retirement has to happen through exactly one of them;
    // and that is the session that will carry the winning content back to
    // alpha, from which the group's other destinations then take it. Every
    // other destination loses too: its copy and alpha's both go, the pair
    // reads as a deletion on both sides, which clears the ancestor entry,
    // and the winner's content then arrives as ordinary new content.
    struct Loser {
        index: usize,
        alpha: bool,
        side: String,
        root: Option<autobahn::tree::Node>,
        actions: Vec<(String, Action)>,
    }
    enum Action {
        Retire(autobahn::tree::Node),
        Aside(String, autobahn::tree::Node),
    }
    // One scan per side, not one per path. A scan of a large tree is the
    // slow part of this command, so it says whose tree it is reading. On a
    // terminal the line is erased afterwards; anywhere else it is never
    // written, since a carriage return in a log file is noise.
    let scan = |endpoint: &mut Box<dyn autobahn::endpoint::Endpoint + Send>,
                side: &str|
     -> Result<Option<autobahn::tree::Node>> {
        let transient = std::io::IsTerminal::is_terminal(&std::io::stderr());
        if transient {
            eprint!("  reading {side}\r");
        }
        let snapshot = endpoint
            .scan()
            .with_context(|| format!("unable to read {side}"))?;
        if transient {
            eprint!("\r\x1b[2K");
        }
        Ok(snapshot.root)
    };
    let mut losers: Vec<Loser> = Vec::new();
    let mut here_of: Vec<Vec<String>> = Vec::new();
    for (index, plan) in group_plans.iter().enumerate() {
        let (alpha, side) = match winner {
            Winner::Beta(w) if w == index => (true, "alpha".to_owned()),
            _ => (false, plan.host.clone()),
        };
        // Only paths that actually conflict on this session are touched;
        // a destination that already agrees is left alone. Alpha is the
        // exception: its copy must go for the winning content to reach it,
        // whichever session reported the conflict.
        let here: Vec<String> = paths
            .iter()
            .filter(|path| {
                alpha
                    || targets.iter().any(|(target, paths)| {
                        target.identifier() == plan.identifier() && paths.contains(*path)
                    })
            })
            .cloned()
            .collect();
        if here.is_empty() {
            continue;
        }
        let endpoint = match alpha {
            true => &mut endpoints[index].0,
            false => &mut endpoints[index].1,
        };
        let root = scan(endpoint, &side)?;
        losers.push(Loser {
            index,
            alpha,
            side,
            root,
            actions: Vec::new(),
        });
        here_of.push(here);
    }
    if losers.is_empty() {
        println!("settled 0 of {} (nothing to retire)", paths.len());
        return Ok(());
    }

    // The winner is read too: what it holds is what the losers are
    // compared against, and what the next cycle must be seen to keep.
    let (winner_name, winner_root) = match winner {
        Winner::Alpha | Winner::Both => ("alpha".to_owned(), scan(&mut endpoints[0].0, "alpha")?),
        Winner::Beta(w) => {
            let host = group_plans[w].host.clone();
            let root = scan(&mut endpoints[w].1, &host)?;
            (host, root)
        }
    };

    // Content is compared as synchronization sees it: excluded entries
    // are invisible, and two absences agree.
    let same = |a: Option<&autobahn::tree::Node>, b: Option<&autobahn::tree::Node>| match (
        a.and_then(autobahn::tree::Node::synchronizable_subtree),
        b.and_then(autobahn::tree::Node::synchronizable_subtree),
    ) {
        (None, None) => true,
        (Some(a), Some(b)) => a.content_equal(&b, true),
        _ => false,
    };

    // What each loser would do, path by path.
    let mut agreed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (loser, here) in losers.iter_mut().zip(&here_of) {
        for path in here {
            let Some(node) = node_at(loser.root.as_ref(), path) else {
                // Already gone on this side. Nothing to retire, and the
                // cycle will carry the winner's version here anyway.
                continue;
            };
            let kept = node_at(winner_root.as_ref(), path);
            // Already what the winner holds. Retiring it would gain
            // nothing, and where the winner is also what the last sync
            // recorded it would lose the file everywhere: the removal
            // reads as a deletion against an untouched copy, and that
            // propagates. A repeated resolve, or a stale tray entry, lands
            // here.
            if same(Some(node), kept) {
                agreed.insert(path.clone());
                continue;
            }
            if winner == Winner::Both {
                // Kept, not discarded: the losing version moves to a free
                // name, from which it propagates to every side as ordinary
                // new content. A rename does this for a whole tree without
                // moving any of it, which is why it is a primitive rather
                // than a read and a write.
                let aside = free_name(loser.root.as_ref(), path, &loser.side);
                loser
                    .actions
                    .push((path.clone(), Action::Aside(aside, node.clone())));
                continue;
            }
            // Content synchronization never scanned cannot be removed by a
            // transition — it refuses, by design, because nobody decided to
            // delete what reconciliation never saw. Asking anyway does not
            // fail cleanly: the removal runs bottom-up, takes away
            // everything it *can* account for, and leaves the rest. That is
            // the worst of both outcomes, a half-deleted tree and the
            // conflict still open, so the check happens here rather than
            // being discovered midway.
            if let Some((example, reason)) = unsynchronizable_within(node, path) {
                blocked.push((path.clone(), loser.side.clone(), example, reason));
                continue;
            }
            // The *synchronizable* subtree, which is what the cycle would
            // pass. An expectation is what the removal is permitted to take
            // away, so handing it the raw scan would ask for the excluded
            // entries too — and those are refused one by one, leaving the
            // tree half-taken. Filtered, the removal takes what
            // synchronization knows about and steps over the rest, exactly
            // as an ordinary deletion does.
            let Some(expectation) = node.synchronizable_subtree() else {
                // Nothing here is synchronization's to remove.
                continue;
            };
            loser
                .actions
                .push((path.clone(), Action::Retire(expectation)));
        }
    }

    // A path blocked on one side is not settled on any: retiring the
    // other copies around content that cannot be removed would leave the
    // two sides disagreeing in a new way.
    for loser in &mut losers {
        loser
            .actions
            .retain(|(path, _)| !blocked.iter().any(|(blocked, ..)| blocked == path));
    }

    // Retiring the loser settles a path only if the next cycle then
    // carries the winner over the gap. It does not when the winner is what
    // the last sync recorded — the gap then reads as a deletion against an
    // untouched copy, and the deletion propagates — nor in a mode that
    // never carries that side's content, nor where alpha's deletion is
    // final. So before anything is touched, each affected session's next
    // cycle is worked out against its ancestor, read without writing, and
    // a path whose kept version would not survive it is refused.
    let mut unsafe_paths: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let acted: std::collections::BTreeSet<String> = losers
        .iter()
        .flat_map(|loser| loser.actions.iter().map(|(path, _)| path.clone()))
        .collect();
    if !acted.is_empty() {
        use autobahn::tree::{apply, reconcile, Change};
        let removals_of = |loser: &Loser| -> Vec<Change> {
            let mut changes = Vec::new();
            for (path, action) in &loser.actions {
                changes.push(Change {
                    path: path.clone(),
                    old: None,
                    new: None,
                });
                if let Action::Aside(aside, node) = action {
                    changes.push(Change {
                        path: aside.clone(),
                        old: None,
                        new: Some(node.clone()),
                    });
                }
            }
            changes
        };
        let alpha_loser = losers.iter().find(|loser| loser.alpha);
        for (index, plan) in group_plans.iter().enumerate() {
            let beta_loser = losers
                .iter()
                .find(|loser| loser.index == index && !loser.alpha);
            // Which paths this session's cycle decides.
            let decided: Vec<&String> = match winner {
                Winner::Beta(_) => acted.iter().collect(),
                _ => match beta_loser {
                    Some(loser) => loser.actions.iter().map(|(path, _)| path).collect(),
                    None => continue,
                },
            };
            if decided.is_empty() {
                continue;
            }
            let checkpoint = state_root
                .join("sessions")
                .join(plan.identifier())
                .join("ancestor");
            let (ancestor, _) =
                autobahn::session::ancestor::peek(&checkpoint).with_context(|| {
                    format!(
                        "unable to read what {} last synchronized, so whether resolving would \
                     delete the kept version cannot be checked; nothing was changed",
                        plan.display()
                    )
                })?;

            // Both sides as the retirement leaves them. When a destination
            // wins, alpha's copy is gone on the winning session; on every
            // other session the worst order is assumed, the one in which
            // alpha already holds the winner's version when that session
            // next runs.
            let scanned = match (winner, beta_loser) {
                (Winner::Beta(w), None) if w != index => {
                    Some(scan(&mut endpoints[index].1, &plan.host)?)
                }
                _ => None,
            };
            let simulated = (|| -> std::result::Result<_, String> {
                let (alpha_after, beta_after) = match winner {
                    Winner::Alpha | Winner::Both => {
                        let loser = beta_loser.expect("an acting session has a losing beta");
                        (
                            winner_root.clone(),
                            apply(loser.root.as_ref(), &removals_of(loser))?,
                        )
                    }
                    Winner::Beta(w) => {
                        let alpha_loser = alpha_loser.expect("the winning session retires alpha");
                        let alpha_after = if w == index {
                            apply(alpha_loser.root.as_ref(), &removals_of(alpha_loser))?
                        } else {
                            // Alpha as the winning session leaves it: holding
                            // the winner's version at every path decided,
                            // whether or not alpha had a copy to retire.
                            let replaced: Vec<Change> = decided
                                .iter()
                                .map(|path| Change {
                                    path: (*path).clone(),
                                    old: None,
                                    new: node_at(winner_root.as_ref(), path).cloned(),
                                })
                                .collect();
                            apply(alpha_loser.root.as_ref(), &replaced)?
                        };
                        let beta_after = if w == index {
                            winner_root.clone()
                        } else if let Some(loser) = beta_loser {
                            apply(loser.root.as_ref(), &removals_of(loser))?
                        } else {
                            scanned.clone().flatten()
                        };
                        (alpha_after, beta_after)
                    }
                };
                let outcome = reconcile(
                    ancestor.as_ref(),
                    alpha_after.as_ref(),
                    beta_after.as_ref(),
                    plan.mode,
                );
                Ok((
                    apply(alpha_after.as_ref(), &outcome.alpha_transitions)?,
                    apply(beta_after.as_ref(), &outcome.beta_transitions)?,
                ))
            })();
            // A tree the changes do not fit — a parent that is not there —
            // refuses the paths rather than guessing at them.
            let (alpha_final, beta_final) = match simulated {
                Ok(trees) => trees,
                Err(message) => {
                    for path in decided {
                        unsafe_paths.entry(path.clone()).or_insert_with(|| {
                            format!(
                                "whether keeping {winner_name} here would delete it cannot \
                                 be worked out ({message})"
                            )
                        });
                    }
                    continue;
                }
            };

            for path in decided {
                let kept = node_at(winner_root.as_ref(), path);
                let on_alpha = same(node_at(alpha_final.as_ref(), path), kept);
                let on_beta = same(node_at(beta_final.as_ref(), path), kept);
                // Where a destination wins, a session other than the
                // winner's needs only to leave alpha's new version alone;
                // it reaches that destination on a later cycle.
                let survives = match winner {
                    Winner::Beta(w) if w != index => on_alpha,
                    _ => on_alpha && on_beta,
                };
                let aside_lost = beta_loser.and_then(|loser| {
                    loser.actions.iter().find_map(|(at, action)| match action {
                        Action::Aside(aside, node) if at == path => {
                            (!same(node_at(beta_final.as_ref(), aside), Some(node)))
                                .then(|| aside.clone())
                        }
                        _ => None,
                    })
                });
                let reason = if !survives {
                    if matches!(winner, Winner::Beta(w) if w == index)
                        && plan.mode == autobahn::tree::SyncMode::TwoWayStrict
                    {
                        format!(
                            "keeping {winner_name} here would delete it: in {} alpha's \
                             deletion beats {winner_name}'s edit, so removing alpha's copy \
                             removes {winner_name}'s too",
                            plan.mode_name()
                        )
                    } else if kept.is_some() && same(node_at(ancestor.as_ref(), path), kept) {
                        format!(
                            "keeping {winner_name} here would delete it: {winner_name} has \
                             not changed since the last sync, so removing the other copy \
                             reads as a deletion. Edit the file on {winner_name} first, or \
                             wait for the fix to forcing a match"
                        )
                    } else {
                        format!(
                            "keeping {winner_name} here would not carry it to {} in {}",
                            plan.host,
                            plan.mode_name()
                        )
                    }
                } else if let Some(aside) = aside_lost {
                    format!(
                        "{}'s copy, moved aside to {aside}, would not survive in {}",
                        plan.host,
                        plan.mode_name()
                    )
                } else {
                    continue;
                };
                unsafe_paths.entry(path.clone()).or_insert(reason);
            }
        }
    }
    for loser in &mut losers {
        loser
            .actions
            .retain(|(path, _)| !unsafe_paths.contains_key(path));
    }

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
    for loser in losers {
        let endpoint = match loser.alpha {
            true => &mut endpoints[loser.index].0,
            false => &mut endpoints[loser.index].1,
        };
        let side = loser.side;
        let mut removals = Vec::new();
        for (path, action) in loser.actions {
            match action {
                Action::Aside(aside, _) => {
                    endpoint
                        .rename(&path, &aside)
                        .with_context(|| format!("unable to keep {side}'s {path}"))?;
                    println!(
                        "  {side}: kept {} as {}",
                        display_safe(&path),
                        display_safe(&aside)
                    );
                    settled += 1;
                }
                Action::Retire(expectation) => removals.push(autobahn::tree::Change {
                    path,
                    old: Some(expectation),
                    new: None,
                }),
            }
        }

        if removals.is_empty() {
            continue;
        }
        let outcome = endpoint
            .transition(removals)
            .with_context(|| format!("unable to retire {side}'s version"))?;
        // A refusal is not an error: the transition reports it and leaves
        // the content alone. It means the path moved between the scan and
        // the removal, which is exactly the case the validation exists to
        // catch — so it is reported, by path, and the conflict stays.
        for problem in &outcome.problems {
            refused.push((problem.path.clone(), problem.message.clone()));
        }
        // A removal succeeded when nothing synchronization knew about
        // survived it. Usually that means the path is gone; where the tree
        // held excluded entries the directory itself necessarily remains,
        // holding only them, and it is then invisible — so the conflict is
        // settled even though something is still on disk. Counting only
        // outright disappearance reports "settled 0" for a resolution that
        // fully worked.
        settled += outcome
            .results
            .iter()
            .filter(|result| match result {
                None => true,
                Some(node) => node.children().is_empty(),
            })
            .count();
    }
    // Always said, including "settled 0". A command that reports nothing
    // reads as a command that worked, and this one can legitimately settle
    // none of what it was asked to.
    let kept_word = match winner {
        Winner::Both => "both versions kept",
        _ => "one version kept",
    };
    println!("settled {settled} of {} ({kept_word})", paths.len());

    // Nothing to do, and said so: the sides already hold one version.
    for path in agreed.iter().filter(|path| !acted.contains(*path)) {
        if !blocked.iter().any(|(blocked, ..)| blocked == path) {
            println!("  {}: already the same on every side", display_safe(path));
        }
    }

    // Refused before anything was touched, because the next cycle would
    // not have kept what was asked for.
    for (path, reason) in &unsafe_paths {
        println!("  {}: not settled — {reason}.", display_safe(path));
    }

    // Blocked, and permanently: retiring this side would mean deleting
    // content synchronization never scanned, which it will not do. Saying
    // "try again" here would be a lie, so the ways out are named instead.
    for (path, side, example, reason) in &blocked {
        let path = display_safe(path);
        println!(
            "  {path}: not settled — {side} holds {} ({}),",
            display_safe(example),
            display_safe(reason)
        );
        println!("    which cannot be deleted on your behalf. Either:");
        println!("      · ignore {path} in this group, so it stops being compared, or");
        println!("      · `--keep both`, which moves the version aside instead of deleting it, or");
        println!("      · delete it on {side} by hand.");
    }

    // Refused, and possibly transient: the entry moved between the scan
    // and the removal, which is the race the validation exists to catch.
    for (path, error) in &refused {
        println!(
            "  left alone: {} — {}",
            display_safe(path),
            display_safe(error)
        );
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
/// Writes a starting configuration, and proves it loads before saying so.
///
/// A template that did not load would be a poor introduction to a tool
/// whose whole point is that the file is the source of truth — so this
/// reads back what it wrote rather than trusting it.
fn run_init(config: Option<PathBuf>, force: bool) -> Result<()> {
    let path = match config {
        Some(path) => path,
        None => {
            paths::prepare_state_root(&paths::default_state_root()?)?;
            paths::default_config_path()?
        }
    };
    if path.exists() {
        if !force {
            bail!(
                "{} already exists. `--force` replaces it, keeping the old one beside it",
                path.display()
            );
        }
        let previous = path.with_extension("toml.bak");
        std::fs::copy(&path, &previous).with_context(|| {
            format!(
                "unable to keep {} at {}",
                path.display(),
                previous.display()
            )
        })?;
        println!("kept the previous configuration at {}", previous.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("unable to create {}", parent.display()))?;
    }
    // Written beside and renamed, so an interrupted write never leaves a
    // half-written configuration where a whole one used to be; private
    // when new, and a replaced one's mode kept.
    autobahn::persist::write_atomically(&path, autobahn::config::TEMPLATE.as_bytes())
        .with_context(|| format!("unable to write {}", path.display()))?;

    let sessions = autobahn::config::Config::load(&path)
        .and_then(|config| config.plans())
        .with_context(|| {
            format!(
                "the configuration just written at {} does not load",
                path.display()
            )
        })?
        .len();
    println!("wrote {}", path.display());
    for (name, contents) in [
        ("on-alert.sh", autobahn::config::ON_ALERT_EXAMPLE),
        ("open-status", autobahn::config::OPEN_STATUS_EXAMPLE),
    ] {
        // An existing script is never replaced, not even under `--force`:
        // the configuration is autobahn's to rewrite, but a hook is a
        // script its owner may have made their own, and there is no way to
        // tell one that was edited from one that was not.
        if let Some(written) = write_example_script(path.parent(), name, contents)? {
            println!("wrote {}", written.display());
        }
    }
    println!("  it describes {sessions} session(s): edit the example group to add one");
    println!("  then `autobahn watch`, or `autobahn install` to run it as a login service");
    Ok(())
}

/// `autobahn disable` and `autobahn enable`: one word in the
/// configuration, written back with every comment around it intact.
///
/// The name is checked against the configuration first. A host that no
/// group mentions, or a group that does not exist, is a typo — and a typo
/// written into the file would be a line that reads as done and does
/// nothing, which is the failure this command exists to prevent.
fn run_availability(
    config: Option<PathBuf>,
    host: Option<String>,
    group: Option<String>,
    enable: bool,
) -> Result<()> {
    let path = match config {
        Some(path) => path,
        None => paths::default_config_path()?,
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("unable to read {}", path.display()))?;
    let configuration = autobahn::config::Config::load(&path)?;
    let verb = match enable {
        true => "enabled",
        false => "disabled",
    };

    let (updated, changed, what) = match (&host, &group) {
        (Some(host), None) => {
            let known = configuration.known_hosts();
            if !known.iter().any(|candidate| candidate == host) {
                bail!(
                    "no group mentions the host {host:?}. The configuration names: {}",
                    known.join(", ")
                );
            }
            let (updated, changed) = autobahn::config::set_host_disabled(&text, host, !enable)?;
            (updated, changed, format!("host {host}"))
        }
        (None, Some(group)) => {
            if !configuration.groups.contains_key(group) {
                bail!(
                    "no group named {group:?}. The configuration names: {}",
                    configuration
                        .groups
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            let (updated, changed) = autobahn::config::set_group_disabled(&text, group, !enable)?;
            (updated, changed, format!("group {group}"))
        }
        _ => bail!("name one of --host or --group"),
    };

    if !changed {
        println!("{what} is already {verb}; {} is unchanged", path.display());
        return Ok(());
    }

    // Written beside and renamed, so an interrupted write never leaves a
    // half-written configuration where a whole one used to be — the same
    // rule `init` follows. It keeps the mode the file had.
    let temporary = autobahn::persist::write_beside(&path, updated.as_bytes())
        .with_context(|| format!("unable to write beside {}", path.display()))?;
    // Read back before it is moved into place: a configuration this
    // command cannot load is one the supervisor would refuse at its next
    // start, which is the worst moment to find out.
    if let Err(error) = autobahn::config::Config::load(&temporary).and_then(|c| c.plans()) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("the edit would leave {} unloadable", path.display()));
    }
    std::fs::rename(&temporary, &path)
        .with_context(|| format!("unable to move {} into place", temporary.display()))?;

    let sessions = configuration
        .plans()
        .map(|plans| plans.len())
        .unwrap_or_default();
    let now = autobahn::config::Config::load(&path)
        .and_then(|c| c.plans())
        .map(|plans| plans.len())
        .unwrap_or_default();
    println!("{verb} {what} in {}", path.display());
    println!("  {} session(s) now, {} before", now, sessions);
    if let Some(host) = &host {
        let led = configuration.groups_led_by(host);
        if !led.is_empty() {
            println!(
                "  note: it is the alpha of {}, so {} group(s) {} with it",
                led.join(", "),
                led.len(),
                match enable {
                    true => "return",
                    false => "go",
                }
            );
        }
    }
    println!("  the supervisor reads the configuration at startup: `autobahn restart`");
    Ok(())
}

/// Writes one of the example scripts beside the configuration, unless a
/// file of that name is already there. Returns where it went, or nothing
/// when one was already there.
///
/// Executable, because the hook runs it as a command. The alerting it does
/// is experimental — an example to edit, not an interface — while
/// `on_alert` and the variables it is handed are not.
fn write_example_script(
    directory: Option<&std::path::Path>,
    name: &str,
    contents: &str,
) -> Result<Option<PathBuf>> {
    let Some(directory) = directory else {
        return Ok(None);
    };
    let script = directory.join(name);
    if script.exists() {
        return Ok(None);
    }
    std::fs::write(&script, contents)
        .with_context(|| format!("unable to write {}", script.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("unable to make {} executable", script.display()))?;
    Ok(Some(script))
}

#[allow(clippy::too_many_arguments)]
fn run_clean(
    config: Option<PathBuf>,
    state_root: Option<PathBuf>,
    dry_run: bool,
    agent_staging_older_than: Option<u64>,
    agents: bool,
    keep_agents: usize,
    include_disabled: bool,
    yes: bool,
) -> Result<()> {
    use autobahn::session::{EndpointPairLock, SessionLock};
    use std::collections::{HashMap, HashSet};

    let config = match config {
        Some(path) => path,
        None => paths::default_config_path()?,
    };
    let plans = Config::load(&config)?.plans()?;
    let state_root = resolve_state_root(state_root)?;
    let (disabled, unclear) = disabled_plans(&config, &plans)?;

    // What a disabled session's state is called when it goes, so a purge
    // names the sessions it lets go of.
    let disabled_labels: HashMap<String, String> = disabled
        .iter()
        .map(|plan| (plan.identifier(), format!("({}, disabled)", plan.display())))
        .collect();
    if include_disabled && !dry_run && !yes && !(disabled.is_empty() && unclear.is_empty()) {
        println!("about to remove the state of these disabled sessions:");
        for plan in &disabled {
            println!("  {}", plan.display());
        }
        for group in &unclear {
            println!("  whatever belongs to group '{group}', whose settings do not validate");
        }
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            bail!("nothing to answer the prompt; pass --yes to remove it without asking");
        }
        print!("proceed? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok();
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            bail!("nothing removed");
        }
    }

    // Kept: every session the configuration describes, on or off. A
    // disabled session's ancestor is what lets enabling it resume, so it
    // goes only when asked for.
    let kept_plans: Vec<&autobahn::config::SessionPlan> = match include_disabled {
        true => plans.iter().collect(),
        false => plans.iter().chain(&disabled).collect(),
    };
    let live_sessions: HashSet<String> = kept_plans.iter().map(|plan| plan.identifier()).collect();
    let live_locks: HashSet<String> = kept_plans
        .iter()
        .map(|plan| EndpointPairLock::key(&plan.alpha_identity, &plan.beta_identity))
        .collect();
    // A disabled group whose settings no longer validate cannot say which
    // sessions were its own, so nothing that might be is removed.
    let unattributable = !include_disabled && !unclear.is_empty();
    let keep_unattributed = |path: &Path, what: &str| {
        println!(
            "kept {what} {}: could not tell what it belongs to (disabled group(s) {} do not \
             validate)",
            path.display(),
            unclear.join(", ")
        );
    };

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
        if unattributable {
            keep_unattributed(&path, "session");
            continue;
        }
        let what = match disabled_labels.get(&name) {
            Some(label) => format!("session {label}"),
            None => "session".to_owned(),
        };
        match SessionLock::acquire(path.clone()) {
            Ok(_lock) => {
                remove(&path, &what)?;
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
        if live_sessions.contains(identifier) {
            continue;
        }
        if unattributable {
            keep_unattributed(&entry.path(), "status record");
            continue;
        }
        remove(&entry.path(), "status record")?;
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
        if unattributable {
            keep_unattributed(&path, "endpoint lock");
            continue;
        }
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

    // Superseded agent binaries on the remote hosts. Everything above is
    // local; this reaches out, so it is asked for rather than assumed, and
    // a host that cannot be reached is reported and stepped over rather
    // than failing the whole clean — one sleeping laptop must not stop the
    // rest of a fleet being tidied.
    let mut agents_removed = 0usize;
    if agents {
        use autobahn::config::EndpointTarget;
        let mut destinations: Vec<String> = plans
            .iter()
            .flat_map(|plan| [&plan.alpha, &plan.beta])
            .filter_map(|target| match target {
                // A session driven through a custom agent command does not
                // use the installed-agent path at all, so there is nothing
                // of ours on the far side to prune.
                EndpointTarget::Remote {
                    destination,
                    agent_command: None,
                    ..
                } => Some(destination.clone()),
                _ => None,
            })
            .collect();
        destinations.sort();
        destinations.dedup();

        for destination in destinations {
            match autobahn::transport::install::prune_agents(&destination, keep_agents, dry_run) {
                Ok(pruned) if pruned.removed.is_empty() => {
                    println!(
                        "{destination}: {} agent(s), nothing superseded",
                        pruned.kept.len()
                    );
                }
                Ok(pruned) => {
                    println!(
                        "{verb} {} superseded agent(s) on {destination}: {} (kept {})",
                        pruned.removed.len(),
                        pruned.removed.join(", "),
                        pruned.kept.join(", ")
                    );
                    agents_removed += pruned.removed.len();
                }
                Err(error) => {
                    println!("skipped {destination} ({error:#})");
                    in_use += 1;
                }
            }
        }
    }

    // Counted rather than measured: the bytes are on the far side, and a
    // second round of SSH to size them would cost more than the number is
    // worth. An agent binary is about 5 MB.
    if agents_removed > 0 {
        println!("{verb} {agents_removed} superseded agent binary(ies) from the remote hosts");
    }

    if removed == 0 && in_use == 0 && agents_removed == 0 {
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

/// The sessions a configuration describes but has turned off — by a
/// group's `disabled`, or by `disabled_hosts` — and the disabled groups
/// whose settings no longer validate, which cannot say what they would
/// describe.
///
/// Each group is planned alone, with every host enabled, so one broken
/// group costs only its own sessions.
fn disabled_plans(
    config: &Path,
    active: &[autobahn::config::SessionPlan],
) -> Result<(Vec<autobahn::config::SessionPlan>, Vec<String>)> {
    let active: std::collections::HashSet<String> =
        active.iter().map(|plan| plan.identifier()).collect();
    let names: Vec<String> = Config::load(config)?.groups.into_keys().collect();
    let mut disabled = Vec::new();
    let mut unclear = Vec::new();
    for name in names {
        let mut alone = Config::load(config)?;
        alone.disabled_hosts.clear();
        for (other, group) in alone.groups.iter_mut() {
            group.disabled = *other != name;
        }
        match alone.plans() {
            Ok(plans) => disabled.extend(
                plans
                    .into_iter()
                    .filter(|plan| !active.contains(&plan.identifier())),
            ),
            Err(_) => unclear.push(name),
        }
    }
    Ok((disabled, unclear))
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

/// What `status` shows when no supervisor answers and the configuration
/// does not load: the fault, and every session's state as last recorded,
/// rather than the fault alone.
fn show_recorded(state_root: &Path, error: &anyhow::Error) -> Result<()> {
    style::emit(&format!(
        "\x1b[33mthe configuration does not load\x1b[0m, so what follows is every \
         session's state as last recorded\n{}\n\n",
        autobahn::text::display_safe(&format!("{error:#}"))
    ));
    let statuses = autobahn::supervisor::recorded_statuses(state_root);
    if statuses.is_empty() {
        println!("no session has recorded a state");
    }
    for status in &statuses {
        let state = autobahn::supervisor::classify_state(status);
        // Betas of one group on one host are told apart by their paths,
        // as the configuration would label them.
        let shared = statuses
            .iter()
            .filter(|other| other.group == status.group && other.host == status.host)
            .count()
            > 1;
        let display = match shared {
            true => format!("{}@{} ({})", status.group, status.host, status.beta),
            false => format!("{}@{}", status.group, status.host),
        };
        match &status.error {
            Some(error) => println!(
                "  {}  {state}: {}",
                autobahn::text::display_safe(&display),
                autobahn::text::display_safe(error)
            ),
            None => println!("  {}  {state}", autobahn::text::display_safe(&display)),
        }
    }
    Ok(())
}

/// What `status` says when the configuration leaves nothing to run.
const NO_ACTIVE_SESSIONS: &str = "no active sessions (every group is disabled)";

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
    expand: bool,
) -> Result<()> {
    let state_root = resolve_state_root(state_root)?;
    // A running supervisor says which sessions it runs; the file on disk
    // may since have been edited into one it refused.
    let mut inventory = None;
    let plans = match peer_plans(&config)? {
        Some((plans, header)) => {
            if !json {
                println!("{header}");
            }
            plans
        }
        None => {
            let path = match config {
                Some(path) => path,
                None => paths::default_config_path()?,
            };
            match autobahn::supervisor::shown_plans(&path, &state_root) {
                Ok(shown) => {
                    inventory = shown.inventory;
                    shown.plans
                }
                Err(error) if !json => return show_recorded(&state_root, &error),
                Err(error) => return Err(error),
            }
        }
    };
    if !json
        && inventory
            .as_ref()
            .is_some_and(|inventory| inventory.logging_failed)
    {
        style::emit(
            "\x1b[33mthe supervisor could not write some of its log\x1b[0m (standard output \
             closed, or the disk under the log full); the sessions are unaffected\n\n",
        );
    }

    // Every group turned off is a state, not an error: a running
    // supervisor applies it by stopping every session, and waits.
    if plans.is_empty() && group.is_none() && host.is_none() {
        if json {
            let report = autobahn::supervisor::status_report(&[], &state_root);
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("{NO_ACTIVE_SESSIONS}");
        }
        return Ok(());
    }
    let selection = select(&plans, group.as_deref(), host.as_deref())?;
    let selected: Vec<_> = selection.plans;
    if live {
        if json {
            bail!("--live repaints a display; --json prints one document. Pick one");
        }
        if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            bail!(
                "--live repaints a terminal display, and this output is not a terminal; \
                 run `autobahn status` on a timer instead"
            );
        }
        return run_live_display(&selected, &state_root, expand_conflicts, expand, "live");
    }
    if json {
        let report = autobahn::supervisor::status_report(&selected, &state_root);
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let mut out = String::new();
    render_status(
        &selected,
        &state_root,
        expand_conflicts,
        expand,
        live,
        &mut out,
    );
    style::emit(&out);
    Ok(())
}

/// Renders the status of the selected sessions, as `status` prints it and
/// `watch` repaints it.
fn render_status(
    selected: &[&autobahn::config::SessionPlan],
    state_root: &Path,
    expand_conflicts: bool,
    expand: bool,
    live: bool,
    out: &mut String,
) {
    use autobahn::supervisor::control::progress_of;
    use std::fmt::Write;

    // One round trip answers both "is anything running" and "what is each
    // session doing"; the recorded status on disk answers "how did the last
    // cycle end". A session is described by the first when it is working
    // and by the second when it is not.
    let probe = autobahn::supervisor::control::probe(state_root);
    let mismatch = probe.mismatch_message();
    let running = probe.is_running();
    let reported = probe.progress();
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
    if let Some(mismatch) = &mismatch {
        // Running, and synchronizing, but not this build: it cannot be
        // asked what it is doing, so what follows is what it last recorded.
        let _ = writeln!(
            out,
            "\x1b[33ma supervisor of another build is running\x1b[0m; what follows is \
             the state last recorded, not what is happening now\n{mismatch}\n"
        );
    } else if !running {
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
    // The supervisor's own word on the file, when it refused an edit: the
    // sessions below run on under the configuration that last loaded.
    if let Some(notice) = running
        .then(|| autobahn::supervisor::reload::read_notice(state_root))
        .flatten()
    {
        let _ = writeln!(
            out,
            "\x1b[33mthe configuration was refused\x1b[0m; the sessions run on under \
             the last one that loaded\n{}\n",
            display_safe(&notice.message)
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
        // Peering: the role and term ride on the group line, since they
        // belong to the supervisor rather than to any one destination.
        let role = block[0]
            .1
            .as_ref()
            .filter(|status| !status.role.is_empty())
            .map(|status| format!("  {} (term {})", display_safe(&status.role), status.term))
            .unwrap_or_default();
        // A group with nothing to say is one line: every destination
        // synchronized, nothing waiting on anyone, and nothing going on long
        // enough to be worth a line. Fifteen healthy sessions were forty-five
        // lines, and the one that needed a person scrolled off the top.
        let live_progress = |plan: &autobahn::config::SessionPlan| {
            reported
                .as_deref()
                .and_then(|sessions| progress_of(sessions, &plan.identifier()))
                .cloned()
        };
        if !expand {
            if let Some(line) = collapsed_group(block, &live_progress, live) {
                let _ = writeln!(
                    out,
                    "\x1b[1m{}\x1b[0m \x1b[2m{}{role}\x1b[0m  {line}",
                    plan.alpha_spec, plan.group
                );
                index = end;
                continue;
            }
        }
        let _ = writeln!(
            out,
            "\x1b[1m{}\x1b[0m \x1b[2m{}{role}\x1b[0m",
            plan.alpha_spec, plan.group
        );

        for (plan, status) in block {
            let progress = reported
                .as_deref()
                .and_then(|sessions| progress_of(sessions, &plan.identifier()));
            render_status_entry(
                &plan.beta_spec(),
                plan.mode_name(),
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

/// The one line a group is shown as when there is nothing to say about any
/// of its destinations, or `None` when there is: any state but
/// synchronized, anything waiting on a person, a paused session, or work
/// that has gone on long enough to earn its own line. `--live` wants every
/// phase however brief, so a brief one rides on the line instead of
/// expanding the group and moving the page.
fn collapsed_group(
    block: &[(&autobahn::config::SessionPlan, Option<SessionStatus>)],
    progress_of: &dyn Fn(&autobahn::config::SessionPlan) -> Option<ProgressSnapshot>,
    live: bool,
) -> Option<String> {
    use autobahn::progress::Phase;
    let mut newest = 0u64;
    let mut doing: Option<&'static str> = None;
    for (plan, status) in block {
        let status = status.as_ref()?;
        let healthy = autobahn::supervisor::classify_state(status) == "synchronized"
            && status.conflicts.is_empty()
            && status.blocked.is_empty()
            && status.error.is_none()
            && status.cycles > 0;
        if !healthy {
            return None;
        }
        newest = newest.max(status.updated_at);
        if let Some(progress) = progress_of(plan) {
            if progress.phase == Phase::Paused {
                return None;
            }
            if progress.phase.is_working() {
                if progress.working_seconds >= SLOW_PHASE_SECONDS {
                    return None;
                }
                if live {
                    doing = Some(progress.phase.label());
                }
            }
        }
    }
    let count = block.len();
    let mut line = format!("✓ {count} synchronized · last cycle {}", format_age(newest));
    if let Some(phase) = doing {
        line.push_str(&format!(" · \x1b[2m{phase}\x1b[0m"));
    }
    Some(line)
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
            let _ = writeln!(
                out,
                "    conflicts: 1, {}",
                display_safe(&status.conflicts[0])
            );
        }
        (count, false) => {
            let _ = writeln!(
                out,
                "    conflicts: {count}, first {}",
                display_safe(&status.conflicts[0])
            );
        }
        (count, true) => {
            let _ = writeln!(out, "    conflicts: {count}");
            for root in &status.conflicts {
                let _ = writeln!(out, "      {}", display_safe(root));
            }
        }
    }
    match status.blocked.len() {
        0 => {}
        1 => {
            let _ = writeln!(out, "    blocked: 1, {}", display_safe(&status.blocked[0]));
        }
        count => {
            let _ = writeln!(
                out,
                "    blocked: {count}, first {}",
                display_safe(&status.blocked[0])
            );
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
        let _ = writeln!(out, "    {label}: {}", display_safe(detail));
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
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
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

    /// A path inside two nested groups is named relative to each group's
    /// own root, not to whichever matched last.
    #[test]
    fn a_path_in_nested_groups_is_relative_to_each_root() {
        let keep = tempfile::tempdir().expect("tempdir");
        let outer = keep.path().join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("file.txt"), "content").unwrap();
        let config = keep.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[groups.outer]\nmode = \"one-way-alpha\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n\n\
                 [groups.inner]\nmode = \"one-way-alpha\"\nalpha = \"{}\"\nbetas = [\"{}\"]\n",
                outer.display(),
                keep.path().join("b1").display(),
                inner.display(),
                keep.path().join("b2").display()
            ),
        )
        .unwrap();
        let plans = super::load_config(Some(config)).unwrap().plans().unwrap();
        let file = inner.join("file.txt").to_string_lossy().into_owned();
        let selection = super::select(&plans, Some(&file), None).unwrap();
        assert_eq!(selection.plans.len(), 2);
        for (index, plan) in selection.plans.iter().enumerate() {
            let expected = match plan.group.as_str() {
                "outer" => "inner/file.txt",
                "inner" => "file.txt",
                other => panic!("unexpected group {other}"),
            };
            assert_eq!(
                super::relative_in(&selection, index, None).unwrap(),
                expected
            );
        }
        // The two disagree, so no single remainder stands for both.
        assert_eq!(selection.relative, None);
        assert!(super::relative_path(&selection, None).is_err());
    }
    /// `start` and `restart` refuse a configuration the supervisor would
    /// refuse, before touching the service, and say why.
    #[test]
    fn start_and_restart_check_the_configuration_first() {
        let keep = tempfile::tempdir().expect("tempdir");
        let good = keep.path().join("good.toml");
        std::fs::write(
            &good,
            "[groups.g]\nmode = \"two-way-conflict\"\nalpha = \"/tmp/a\"\nbetas = [\"/tmp/b\"]\n",
        )
        .unwrap();
        assert!(super::check_startable(Some(good)).is_ok());

        let bad = keep.path().join("bad.toml");
        std::fs::write(
            &bad,
            "[groups.g]\nmode = \"sideways\"\nalpha = \"/tmp/a\"\n",
        )
        .unwrap();
        let error = super::check_startable(Some(bad)).expect_err("a bad configuration is refused");
        let message = format!("{error:#}");
        assert!(message.contains("would stop the supervisor"), "{message}");
        assert!(
            message.contains("sideways") || message.contains("mode"),
            "{message}"
        );

        let empty = keep.path().join("empty.toml");
        std::fs::write(&empty, "[defaults]\nmode = \"two-way-conflict\"\n").unwrap();
        let error = super::check_startable(Some(empty)).expect_err("no sessions is refused");
        assert!(format!("{error:#}").contains("no sessions"));
    }
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
            moved_files: 0,
            moved_bytes: 0,
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

    /// Two betas on one host in one group share a group and a host, so
    /// progress keyed by those showed each the other's. Keyed by the
    /// session, each shows its own.
    #[test]
    fn two_betas_on_one_host_show_their_own_progress() {
        use autobahn::progress::{Phase, ProgressSnapshot};
        use autobahn::supervisor::control::{progress_of, SessionProgress};

        let keep = tempfile::tempdir().expect("tempdir");
        let config = keep.path().join("config.toml");
        std::fs::write(
            &config,
            "[groups.g]\nmode = \"two-way-conflict\"\nalpha = \"/tmp/a\"\n\
             betas = [\"host:/tree\", \"host:/other\"]\n",
        )
        .unwrap();
        let plans = super::load_config(Some(config)).unwrap().plans().unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].host, plans[1].host);

        let snapshot = |phase: Phase| ProgressSnapshot {
            phase,
            seconds: 0,
            working_seconds: 0,
            alpha: side(),
            beta: side(),
            staged: 0,
            staged_total: 0,
            staged_bytes: 0,
            staged_bytes_total: 0,
            moved_files: 0,
            moved_bytes: 0,
            applied: 0,
            applied_total: 0,
            remaining_seconds: None,
        };
        let reported: Vec<SessionProgress> = plans
            .iter()
            .zip([Phase::Scanning, Phase::Paused])
            .map(|(plan, phase)| SessionProgress {
                session: plan.identifier(),
                group: plan.group.clone(),
                host: plan.host.clone(),
                progress: snapshot(phase),
            })
            .collect();
        let phase = |index: usize| {
            progress_of(&reported, &plans[index].identifier())
                .expect("each session has progress")
                .phase
        };
        assert_eq!(phase(0), Phase::Scanning);
        assert_eq!(phase(1), Phase::Paused);
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

    /// A plan whose one beta is `beta`, for the fix-command tests.
    fn fix_plan(beta: &str) -> autobahn::config::SessionPlan {
        let text = format!(
            "[groups.g]\nalpha = \"/tmp/a\"\nmode = \"two-way-conflict\"\nbetas = [\"{beta}\"]\n"
        );
        toml::from_str::<autobahn::config::Config>(&text)
            .expect("parses")
            .plans()
            .expect("plans")
            .remove(0)
    }

    /// Runs `command` through `sh` in `dir`, with `ssh` standing in for a
    /// remote shell (it drops the destination and runs the rest through
    /// `sh -c`, as sshd does) and `sudo` recording its arguments one per
    /// line in `dir/args` instead of running anything. Returns the
    /// recorded arguments.
    fn run_stubbed(dir: &std::path::Path, command: &str) -> Vec<String> {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let sudo = bin.join("sudo");
        std::fs::write(
            &sudo,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > {}\n",
                dir.join("args").display()
            ),
        )
        .unwrap();
        let ssh = bin.join("ssh");
        std::fs::write(&ssh, "#!/bin/sh\nshift\nexec sh -c \"$*\"\n").unwrap();
        for stub in [&sudo, &ssh] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(dir)
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .status()
            .expect("sh runs");
        assert!(status.success(), "{command}");
        std::fs::read_to_string(dir.join("args"))
            .expect("sudo was reached")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// A fix command pasted into a shell does what it says, whatever the
    /// names in it: the path arrives as one argument, and nothing in it
    /// runs. The names come from the other side, and the local form runs
    /// under sudo.
    #[test]
    fn a_fix_command_quotes_every_name_it_holds() {
        let prefix = "a dir/it's;$(touch pwned)/`touch pwned`";
        for beta in ["u@h:/tmp/b", "/tmp/b"] {
            let plan = fix_plan(beta);
            for side in ["alpha", "beta"] {
                let fixes = blocked_fix(side, "Permission denied (os error 13)", prefix, &plan);
                let command = &fixes[0];
                let parsed = std::process::Command::new("sh")
                    .args(["-n", "-c", command])
                    .status()
                    .expect("sh runs");
                assert!(parsed.success(), "sh -n rejects {command}");

                let dir = tempfile::tempdir().unwrap();
                let args = run_stubbed(dir.path(), command);
                let root = match (side, beta) {
                    ("beta", _) => "/tmp/b",
                    _ => "/tmp/a",
                };
                assert_eq!(
                    args.last().map(String::as_str),
                    Some(format!("{root}/{prefix}").as_str()),
                    "{command}"
                );
                assert_eq!(args[..2], ["chown", "-R"], "{command}");
                assert!(!dir.path().join("pwned").exists(), "{command} ran a name");
            }
        }
    }

    /// Without a `user@`, the remote user is whoever the remote shell runs
    /// as — never the host name, which is all the destination says.
    #[test]
    fn a_remote_fix_names_the_remote_user_not_the_host() {
        let fixes = blocked_fix(
            "beta",
            "Permission denied (os error 13)",
            "x",
            &fix_plan("h:/tmp/b"),
        );
        assert!(fixes[0].contains("$(id -un)"), "{fixes:?}");
        assert!(!fixes[0].contains("chown -R h "), "{fixes:?}");
        let dir = tempfile::tempdir().unwrap();
        let args = run_stubbed(dir.path(), &fixes[0]);
        assert_eq!(args[2], whoami(), "{fixes:?}");
    }

    fn whoami() -> String {
        let out = std::process::Command::new("id")
            .arg("-un")
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A name holding a control character is not put in a command to
    /// paste at all: it is shown escaped, with a word to do it by hand.
    #[test]
    fn a_strange_name_gets_a_manual_fix_not_a_command() {
        for beta in ["u@h:/tmp/b", "/tmp/b"] {
            let fixes = blocked_fix(
                "beta",
                "Permission denied (os error 13)",
                "evil\nrm -rf ~\x1b[2J",
                &fix_plan(beta),
            );
            assert!(fixes[0].starts_with("fix permissions on "), "{fixes:?}");
            assert!(fixes[0].ends_with(" by hand"), "{fixes:?}");
            assert!(
                !fixes[0].contains('\n') && !fixes[0].contains('\x1b'),
                "{fixes:?}"
            );
            assert!(fixes[0].contains("evil\\nrm"), "{fixes:?}");
        }
    }

    /// A root written with `~` still expands on the side it names: a
    /// quoted `~` would be a directory called `~`.
    #[test]
    fn a_fix_for_a_home_relative_root_expands_the_home() {
        let fixes = blocked_fix(
            "beta",
            "Permission denied (os error 13)",
            "x",
            &fix_plan("u@h:~/mirror"),
        );
        let dir = tempfile::tempdir().unwrap();
        let args = run_stubbed(dir.path(), &fixes[0]);
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            args.last().unwrap(),
            &format!("{home}/mirror/x"),
            "{fixes:?}"
        );
    }

    /// A path that looks like a flag, built into a `resolve` by the shop
    /// or the tray, reaches the command as a path. Checked against the
    /// real command-line definition, with the shared options put where
    /// both callers put them.
    #[test]
    fn a_path_named_like_a_flag_is_passed_as_a_path() {
        use clap::Parser;
        let paths = vec![
            "--all".to_owned(),
            "-k".to_owned(),
            "-y".to_owned(),
            "--keep=alpha".to_owned(),
        ];
        let command = autobahn::invocation::resolve_command("-g", "b1", &paths);
        let command = autobahn::invocation::with_options(
            &command,
            &["--state-root".to_owned(), "/tmp/state".to_owned()],
        );
        let cli = super::Cli::try_parse_from(std::iter::once("autobahn".to_owned()).chain(command))
            .expect("parses");
        match cli.command {
            super::Command::Resolve {
                selector,
                paths: parsed,
                keep,
                all,
                yes,
                state_root,
                ..
            } => {
                assert_eq!(selector, "-g");
                assert_eq!(parsed, paths);
                assert_eq!(keep, "b1");
                assert!(!all, "a file named --all set the flag");
                assert!(yes);
                assert_eq!(state_root, Some(std::path::PathBuf::from("/tmp/state")));
            }
            _ => panic!("not a resolve"),
        }

        // And `diff` from the tray the same way.
        let command = autobahn::invocation::diff_command("g", "--host", "h");
        let cli = super::Cli::try_parse_from(std::iter::once("autobahn".to_owned()).chain(command))
            .expect("parses");
        match cli.command {
            super::Command::Diff {
                selector,
                path,
                host,
                ..
            } => {
                assert_eq!(selector, "g");
                assert_eq!(path.as_deref(), Some("--host"));
                assert_eq!(host.as_deref(), Some("h"));
            }
            _ => panic!("not a diff"),
        }
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
        assert!(fixes[0].starts_with("ssh 'u@h' "), "{fixes:?}");
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
