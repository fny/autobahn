//! The groups configuration: a declarative description of synchronization
//! sessions, organized as groups that fan one root (the alpha — a local
//! directory or a remote `host:path`) out to any number of destinations
//! (the betas).
//!
//! The configuration is the source of truth: the supervisor derives its
//! session list from it on every start, so what is running is always what
//! the file says (there is no imperative session registry to drift from it).
//! A minimal configuration looks like:
//!
//! ```toml
//! # Top-level keys (like `disabled_hosts`) must precede the first section header.
//! disabled_hosts = ["flaky.example.com"]
//!
//! [defaults]
//! mode = "two-way-conflict"
//! ignores = [".git"]
//! interval = 5
//!
//! [groups.project]
//! alpha = "~/project"
//! betas = ["build.example.com", "user@lab.example.com:/srv/project"]
//!
//! [groups.dotfiles]
//! alpha = "~/.config/shell"
//! mode = "one-way-alpha"
//! betas = ["build.example.com", "/mnt/backup/shell"]
//! ```
//!
//! A beta is **remote** unless it visibly denotes a local path: an entry
//! containing a `/` before any `:`, or beginning with `.`, `/`, or `~`, is a
//! local path; anything else is `[user@]host[:path]`. A remote beta without
//! an explicit path inherits the group's alpha path *as written* (so a
//! home-relative alpha resolves against each remote host's own home).

/// A starting configuration, written by `autobahn init`.
///
/// It lives here, beside the schema it has to satisfy, so the test below
/// can hold it to that schema: a template that does not load would be a
/// poor way to meet the tool. The example group is commented out, so a
/// fresh install describes no sessions and starts nothing until someone
/// means it to.
pub const TEMPLATE: &str = r##"# autobahn — what stays in sync, and where.
# Written by `autobahn init`. Every key is explained in docs/configuration.md.
#
# An unknown key is refused when autobahn starts, rather than ignored, so a
# typo here tells you instead of quietly doing nothing.

# Run when a session needs a person: a conflict, a halt, a host that has
# been away a while. It is the only hook — which state it is in is in the
# message, not in which hook runs. Uncomment it and make it something your
# desktop shows.
# on_alert = "terminal-notifier -title autobahn -message \"$AUTOBAHN_SUMMARY\""
#
# `autobahn init` writes an example hook beside this file — it notifies
# with whatever the machine has, and a click opens the status. Point at it
# instead of writing the command here, and every quote stops being escaped
# twice. The script is experimental; the variables it reads are not.
# on_alert = "~/.autobahn/on-alert.sh"

# How much the supervisor writes to its log: quiet, normal, or debug.
# Every line carries a timestamp whichever you pick.
# log = "normal"

[defaults]
# Inherited by every group below. Any group can override any of it.

# The mode is a direction, and what happens when both sides changed one
# file. There is no default: direction is never guessed.
#
#   two-way-conflict   both ways; a clash is reported and nothing is touched
#   two-way-paranoid   as above, and a large directory that turns up empty
#                      on one side is a conflict, not a deletion to copy
#   two-way-alpha      both ways; alpha's version wins a clash, silently
#   two-way-alpha-strict  as above, and alpha's deletion of a file beta
#                      edited wins too (in two-way-alpha the edit survives)
#   one-way-conflict   alpha to beta; an edit on beta is reported, not overwritten
#   one-way-alpha      alpha to beta; beta is made identical (also spelled "mirror")
#
# Dangerously experimental: as the two-way modes, and a beta takes the lead
# while the alpha is away. Known security and collision issues are open;
# read docs/peering.md first. The alpha must be this machine.
#   peering-conflict-dangerously-experimental
#   peering-alpha-dangerously-experimental
mode = "two-way-conflict"

# Applied everywhere, in gitignore syntax: a bare name matches at any
# depth, a leading "/" anchors to the root of the group, "!" puts something
# back, and the last pattern that matches decides.
ignores = [".git", ".DS_Store", "node_modules", "target"]

# Seconds between heartbeat cycles. Both sides also watch the filesystem,
# so this is the fallback, not how fast a change travels.
interval = 5

# A group sends one source folder (the alpha) to any number of
# destinations (the betas). Each pair is its own session, and one failing
# never stops the others.
#
# This example is commented out, so a fresh install starts nothing. Edit
# the paths, uncomment it, and run `autobahn watch`.
#
# [groups.project]
# alpha = "~/project"
# betas = [
#   "build.example.com",                 # uses the alpha's path on that host
#   "user@lab.example.com:/srv/project", # or name a path
#   "/mnt/backup/project",               # a local path works too
# ]
# ignores = ["dist", "*.log"]            # added to the defaults' ignores
# disabled = true                        # turns the whole group off
"##;

/// The example hook `autobahn init` writes beside the configuration, and
/// the click target it names.
///
/// Experimental: what it notices and what it prints may change between
/// releases. The hook interface it is written against — `on_alert` and the
/// variables handed to it — does not.
///
/// A script rather than a line in the configuration, because a hook is a
/// shell command inside a TOML string: every quote in it is escaped twice,
/// and a notifier's arguments are mostly quotes. It also leaves somewhere
/// to put a second thought later, such as a different notifier per state.
pub const ON_ALERT_EXAMPLE: &str = r##"#!/bin/sh
# autobahn — run when a session needs a person. EXPERIMENTAL: an example,
# not a contract; edit it freely, and expect it to change between releases.
#
# Named by `on_alert` in config.toml. What it is handed:
#
#   $AUTOBAHN_SUMMARY      one line: the whole story, or a count
#   $AUTOBAHN_DETAIL       one indented line per session that needs you
#   $AUTOBAHN_ICON         autobahn's icon, as an absolute path
#   $AUTOBAHN_STATES       the state names present, comma separated
#   $AUTOBAHN_ALERT_COUNT  how many sessions are in the set
#   $AUTOBAHN_EVENT        "alert" the first time, "repeat" after that
#
# The service runs with a sparse PATH and a sparse environment, which is
# why commands are named in full and the bus address is worked out below.
set -eu

case "$(uname -s)" in
Darwin)
    # A click needs a terminal opened around the shop, which `open` does.
    OPEN="open -a Terminal $HOME/.autobahn/open-status"

    # terminal-notifier carries a subtitle and a click. Homebrew puts it
    # in one of two places depending on the chip.
    for notifier in \
        /opt/homebrew/bin/terminal-notifier \
        /usr/local/bin/terminal-notifier
    do
        [ -x "$notifier" ] || continue
        exec "$notifier" \
            -title autobahn -group autobahn \
            -appIcon "$AUTOBAHN_ICON" \
            -subtitle "$AUTOBAHN_DETAIL" \
            -message "$AUTOBAHN_SUMMARY" \
            -execute "$OPEN"
    done

    # Built in, and always there. It holds one line and no click. The
    # summary goes in as an argument, never as part of the AppleScript: it
    # can hold a file name someone else chose.
    exec /usr/bin/osascript \
        -e 'on run argv' \
        -e 'display notification (item 1 of argv) with title "autobahn"' \
        -e 'end run' \
        "$AUTOBAHN_SUMMARY"
    ;;
Linux)
    # notify-send talks to the desktop over the session bus. A service
    # started by the user's own systemd inherits the address; one started
    # by the system does not, so it is guessed from the user id.
    if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$(id -u)/bus"
        export DBUS_SESSION_BUS_ADDRESS
    fi
    if command -v notify-send >/dev/null 2>&1; then
        # Urgency is normal, not critical: a conflict wants attention
        # today, not a notification that refuses to go away.
        exec notify-send \
            --app-name autobahn \
            --icon "$AUTOBAHN_ICON" \
            "$AUTOBAHN_SUMMARY" \
            "$AUTOBAHN_DETAIL"
    fi
    ;;
esac

# No notifier, or a headless host: the log is still the record, and
# standard error goes to it.
echo "autobahn: $AUTOBAHN_SUMMARY" >&2
"##;

/// What the example hook opens on a click: the shop, and then a pause,
/// because a terminal closes its window as soon as the command exits.
pub const OPEN_STATUS_EXAMPLE: &str = r##"#!/bin/sh
# autobahn — opened from a notification. EXPERIMENTAL, like the hook that
# names it.
set -eu
autobahn status || true
echo
printf '[any key to close] '
read -r _
"##;

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::endpoint::StagingMode;
use crate::paths::{expand_tilde, resolve_for_identity};
use crate::scan::{IgnoreSet, SymlinkMode};
use crate::session::session_identifier;
use crate::tree::SyncMode;

/// How long a condition holds before it alerts, unless the configuration
/// says otherwise. Long enough that a dropped connection or a passing
/// permission error is never mentioned; short enough to be timely for the
/// conditions that persist.
const DEFAULT_ALERT_AFTER: Duration = Duration::from_secs(30);

/// How long each state must hold before it counts, where 30 seconds is
/// the wrong answer. These exist so that nobody has to know they exist:
/// a reader who writes only `on_alert` gets the behaviour they would have
/// arrived at themselves after a fortnight of being woken up wrongly.
///
/// A safety halt is never transient, so it waits for nothing. A laptop is
/// usually asleep rather than broken, and comes back. An error is usually
/// a staging hiccup or a dropped frame, and heals in a cycle or two.
fn built_in_after(alert: crate::alerts::Alert) -> Duration {
    use crate::alerts::Alert;
    match alert {
        Alert::Halted => Duration::ZERO,
        Alert::Unreachable => Duration::from_secs(5 * 60),
        Alert::Errored => Duration::from_secs(2 * 60),
        Alert::Conflicts | Alert::Blocked => DEFAULT_ALERT_AFTER,
    }
}

/// How long an alert hook may run before it is killed. Generous for a
/// notification, short enough that a wedged hook is noticed.
const DEFAULT_ALERT_TIMEOUT: Duration = Duration::from_secs(30);

/// The synchronization interval used when neither a group nor the defaults
/// specify one.
/// The top-level key naming hosts that are off everywhere.
pub const DISABLED_HOSTS: &str = "disabled_hosts";

pub const DEFAULT_INTERVAL_SECONDS: u64 = 5;

/// How long a grown alerting set is held before it is reported. Long
/// enough that a laptop closing — which takes its sessions one at a time,
/// as each connection times out — is one notification rather than one per
/// session. Short enough that a real halt is not sat on.
const DEFAULT_COALESCE_AFTER: Duration = Duration::from_secs(60);

/// How long everything must be clear before returning trouble is news.
/// Long enough that a file two machines are both editing is reported once
/// rather than on every return.
const DEFAULT_SETTLE_AFTER: Duration = Duration::from_secs(15 * 60);

/// The parsed configuration file.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Whether the running supervisor re-reads this file and applies an
    /// edit in place. On unless said otherwise; off, an edit lands on
    /// `restart` as it used to.
    #[serde(default = "default_reload")]
    pub reload: bool,
    /// Run when a session needs a person. The only hook — which states are
    /// alerting is in the message it is handed, not in which hook fires.
    ///
    /// Top level, because it is the one thing about alerting that anyone
    /// should have to write. It is safe bare where a `defaults` key would
    /// not be: `on_alert` is a valid key nowhere else, so one written after
    /// a `[groups.x]` header is refused rather than silently absorbed.
    pub on_alert: Option<String>,
    /// Hosts excluded from every group. A disabled beta host drops that
    /// beta; a disabled alpha host drops the whole group.
    ///
    /// Named for what it holds, because a group has a `disabled` of its
    /// own that is a flag, not a list, and one word cannot be both.
    #[serde(default)]
    pub disabled_hosts: Vec<String>,
    /// Retired. Kept only so that a configuration written against the old
    /// spelling is told what to write instead of "unknown field".
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub disabled: Option<toml::Value>,
    /// How much the supervisor writes to its log: "quiet", "normal" (the
    /// default), or "debug". `AUTOBAHN_LOG` overrides it for one run.
    pub log: Option<String>,
    /// On battery, walk each root in full every ten minutes instead of two
    /// (`crate::power`). Experimental: off unless set, and the name will
    /// change when it settles.
    #[serde(default)]
    pub power_saver_experimental: bool,
    /// Settings inherited by every group.
    #[serde(default)]
    pub defaults: Defaults,
    /// The synchronization groups, keyed by name.
    #[serde(default)]
    pub groups: BTreeMap<String, Group>,
    /// Tuning that has a correct value already.
    ///
    /// Spelled `[advanced]` until it was not. The old name is still
    /// read, so no file on any machine had to be edited for this; a
    /// file carrying both is refused as the duplicate it is.
    #[serde(default, alias = "advanced")]
    pub experimental: Advanced,
    /// What is worth saying about this configuration without refusing
    /// it, gathered once per load by [`warnings`](Self::warnings) however
    /// many times its sessions are planned.
    #[serde(skip)]
    warnings: std::sync::OnceLock<Vec<String>>,
    /// Where `ignore_files` entries are looked up when set: a peer runs the
    /// leader's pushed configuration against the pushed ignore files,
    /// not against its own `~/.autobahn/ignores`. Never read from the
    /// file itself.
    #[serde(skip)]
    pub ignore_directory: Option<PathBuf>,
    /// Retired. Kept only so that a configuration written against the old
    /// shape gets an answer rather than "unknown field `alerts`".
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub alerts: Option<toml::Value>,
}

/// The `[experimental]` section: settings whose defaults are the right
/// answer.
/// Grouped under one heading so that finding yourself here is itself the
/// message — the subsystem comes second because the warning should land
/// first.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Advanced {
    /// Alerter timing.
    #[serde(default)]
    pub alerts: AlertsAdvanced,
    /// Peering timing. The section carries the experiment's suffix as the
    /// modes do, so a configuration that names it says so on its face.
    #[serde(default, rename = "peering-dangerously-experimental")]
    pub peering: PeeringAdvanced,
    /// Renamed. Kept only so that a configuration written against the old
    /// name gets an answer rather than "unknown field".
    #[serde(default, rename = "peering-experimental")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub retired_peering: Option<toml::Value>,
    /// Let the controller run as root (see `autobahn::root`). Read by
    /// `root::config_allows_root`; here so that the key is known.
    #[serde(default)]
    pub allow_root: bool,
}

/// The `[experimental.peering-dangerously-experimental]` section: how long
/// a lease lives, and how long a peer waits past a dead lease before it
/// takes the lead.
///
/// As with the alerter's timing, the defaults are the answer. A blip must
/// never cause a failover, so the wait is long; a leader that is really
/// gone costs the wait once. Both are here for the fleet that needs them
/// moved, not for tuning.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PeeringAdvanced {
    /// How long a lease stays valid after the leader last renewed it. The
    /// leader renews on every cycle, so this is a number of missed cycles
    /// expressed as time.
    pub ttl: Option<DurationSpec>,
    /// How long a candidate waits after the lease went stale before it
    /// takes the lead. This is the blip window.
    pub failover_after: Option<DurationSpec>,
}

/// Peering timing, resolved: what the supervisor runs with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeeringPlan {
    /// How long a lease stays valid after its last renewal.
    pub ttl: Duration,
    /// How long a candidate waits past a stale lease before it leads.
    pub failover_after: Duration,
}

/// The lease lifetime as shipped: six missed five-second cycles.
pub const DEFAULT_PEERING_TTL: Duration = Duration::from_secs(30);

/// The blip window as shipped: long enough for a router restart or a
/// laptop lid closed for a minute, short enough that a leader that is
/// really gone is replaced within a few minutes.
pub const DEFAULT_PEERING_FAILOVER_AFTER: Duration = Duration::from_secs(120);

impl Config {
    /// The configured log level, if the file names a valid one.
    pub fn log_level(&self) -> Result<Option<crate::logging::Level>> {
        let Some(name) = &self.log else {
            return Ok(None);
        };
        crate::logging::Level::parse(name)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("unknown log level {name:?} (quiet, normal, or debug)"))
    }
}

/// The `[advanced.alerts]` section: how long things must hold, how long to
/// wait, and how long before a hook is given up on.
///
/// These are not preferences. They are the values that make the alerter
/// usable rather than maddening, and there is no second right answer a
/// reader would discover by trying. They are configurable because a fleet
/// somewhere will need one of them moved, not because anyone should.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AlertsAdvanced {
    /// How long a condition must hold before it counts, for states with no
    /// entry of their own.
    pub alert_after: Option<DurationSpec>,
    /// Per-state confirmation periods, keyed by state name. Overrides the
    /// built-in table, which is already tuned per state: a sleeping laptop
    /// and a safety halt do not deserve the same patience.
    #[serde(default)]
    pub after: BTreeMap<String, DurationSpec>,
    /// How long a grown alerting set is held before it is reported, so a
    /// cascade arrives as one notification rather than one per part.
    pub coalesce_after: Option<DurationSpec>,
    /// How long everything must stay clear before trouble returning counts
    /// as news rather than as the same trouble continuing.
    pub settle_after: Option<DurationSpec>,
    /// How often to fire again while the alerting set is unchanged. Absent
    /// or zero never repeats, which is the default: a notification that
    /// returns while you are already working on it teaches you to ignore
    /// it.
    pub repeat_after: Option<DurationSpec>,
    /// How long a hook may run before it is killed.
    pub timeout: Option<DurationSpec>,
}

/// A duration as written in the configuration: a plain number of seconds,
/// or a suffixed string.
#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum DurationSpec {
    /// A plain number of seconds, matching how `interval` is written.
    Seconds(u64),
    /// A suffixed string ("30s", "5m", "2h").
    Text(String),
}

/// Settings inherited by every group (each overridable per group).
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// The default synchronization mode.
    pub mode: Option<String>,
    /// Ignore patterns prepended to every group's own.
    #[serde(default)]
    pub ignores: Vec<String>,
    /// Names of files in `~/.autobahn/ignores` whose patterns are applied
    /// before every group's own, in the order written.
    #[serde(default)]
    pub ignore_files: Vec<String>,
    /// The default interval, in seconds, between synchronization cycles.
    pub interval: Option<u64>,
    /// The default durability class for the ancestor journal: "process"
    /// (the default) or "power", which syncs every record.
    pub durability: Option<String>,
    /// The default symbolic link treatment (`ignore`, `portable`, or `raw`).
    pub symlink_mode: Option<String>,
    /// The default permission bits (octal) for created files.
    pub file_mode: Option<String>,
    /// The default permission bits (octal) for created directories.
    pub directory_mode: Option<String>,
    /// The default per-file size limit: larger files are left on disk but
    /// excluded from synchronization. Accepts bytes or a suffixed string
    /// ("100MB", "2GiB").
    pub max_file_size: Option<SizeSpec>,
    /// The default limit on entries (files, directories, symlinks) per
    /// root. A scan exceeding it fails the session's cycle.
    pub max_entry_count: Option<u64>,
    /// Whether directories mounted inside a root are left alone (the
    /// default) rather than synchronized as part of it.
    pub ignore_mounts: Option<bool>,
    /// The default staging placement (`state`, `beside-root`, or
    /// `inside-root`).
    pub staging: Option<String>,
    /// The default owner (name or `id:N`) for created entries.
    pub default_owner: Option<String>,
    /// The default group (name or `id:N`) for created entries.
    pub default_group: Option<String>,
}

/// A size limit as written in the configuration: a raw byte count or a
/// suffixed string.
#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum SizeSpec {
    /// A raw byte count.
    Bytes(u64),
    /// A suffixed size string ("100MB", "2GiB", "512K").
    Text(String),
}

/// One synchronization group: a local alpha directory fanned out to one or
/// more beta destinations.
#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Group {
    /// The alpha synchronization root: a local path (`~` is expanded) or a
    /// remote `[user@]host:path` specification.
    pub alpha: String,
    /// The beta destinations (remote `[user@]host[:path]` or local paths).
    #[serde(default)]
    pub betas: Vec<String>,
    /// The synchronization mode (falls back to the defaults).
    pub mode: Option<String>,
    /// Ignore patterns, appended to the defaults' patterns.
    #[serde(default)]
    pub ignores: Vec<String>,
    /// Names of files in `~/.autobahn/ignores`, applied after the
    /// defaults' patterns and before this group's own.
    #[serde(default)]
    pub ignore_files: Vec<String>,
    /// The interval, in seconds, between cycles (falls back to the defaults).
    pub interval: Option<u64>,
    /// The durability class for the ancestor journal (falls back to the
    /// defaults): "process" or "power".
    pub durability: Option<String>,
    /// The symbolic link treatment (`ignore`, `portable`, or `raw`; falls
    /// back to the defaults).
    pub symlink_mode: Option<String>,
    /// The permission bits (octal) for created files (falls back to the
    /// defaults).
    pub file_mode: Option<String>,
    /// The permission bits (octal) for created directories (falls back to
    /// the defaults).
    pub directory_mode: Option<String>,
    /// The per-file size limit (falls back to the defaults): larger files
    /// are left on disk but excluded from synchronization.
    pub max_file_size: Option<SizeSpec>,
    /// The limit on entries per root (falls back to the defaults). A scan
    /// exceeding it fails the session's cycle.
    pub max_entry_count: Option<u64>,
    /// Whether mounts inside the roots are left alone (falls back to the
    /// defaults, then to `true`).
    pub ignore_mounts: Option<bool>,
    /// The staging placement: `state` (the session state directory),
    /// `beside-root` (a sibling of the synchronization root, guaranteeing
    /// same-filesystem renames), or `inside-root` (within the root itself,
    /// for roots on otherwise unwritable-home hosts). Falls back to the
    /// defaults.
    pub staging: Option<String>,
    /// The owner (name or `id:N`) for created entries, applied by each
    /// endpoint on its own host (falls back to the defaults). Requires the
    /// endpoint to have chown rights.
    pub default_owner: Option<String>,
    /// The group (name or `id:N`) for created entries, applied by each
    /// endpoint on its own host (falls back to the defaults).
    pub default_group: Option<String>,
    /// Turns the whole group off: it plans no sessions at all, as though
    /// it were not written. Its state and its status records stay where
    /// they are, so turning it back on resumes rather than starts over.
    #[serde(default)]
    pub disabled: bool,
    /// Silences the warning about a root that holds credentials (`.ssh`,
    /// `.aws` and the like): the group means to synchronize them.
    #[serde(default)]
    pub acknowledge_secrets: bool,
    /// Advanced: connect this group's remote endpoints through this command
    /// (whitespace split into argv) instead of SSH. Used for testing and
    /// custom transports; the endpoint's host is then informational only.
    pub agent_command: Option<String>,
}

/// One planned session: a fully resolved (group, beta) pair, ready for the
/// supervisor to run.
#[derive(Clone, Debug)]
pub struct SessionPlan {
    /// The name of the group the session belongs to.
    pub group: String,
    /// The destination label: the remote host, or the local beta path.
    pub host: String,
    /// The alpha endpoint (a tilde-expanded local path, or a remote root).
    pub alpha: EndpointTarget,
    /// The alpha root as written in the configuration (used for display
    /// and for remote path inheritance).
    pub alpha_spec: String,
    /// The beta destination.
    pub beta: EndpointTarget,
    /// The alpha endpoint's resolved identity, frozen at plan time. The
    /// worker that later connects re-resolves the live path and refuses to
    /// proceed if the two disagree: between planning and connecting a
    /// symlink can be retargeted, and a session that resolved one tree at
    /// plan time must not bind another tree to the first one's ancestor.
    pub alpha_identity: String,
    /// The beta endpoint's resolved identity, frozen at plan time.
    pub beta_identity: String,
    /// The synchronization mode.
    pub mode: SyncMode,
    /// The combined ignore patterns (defaults first, then the group's).
    pub ignores: Vec<String>,
    /// The interval between synchronization cycles.
    pub interval: Duration,
    /// Whether the ancestor journal syncs every record before
    /// acknowledging it (power-loss durability).
    pub power_durability: bool,
    /// The symbolic link treatment.
    pub symlink_mode: SymlinkMode,
    /// The permission bits for created files (`None` for the endpoint
    /// default).
    pub file_mode: Option<u32>,
    /// The permission bits for created directories (`None` for the endpoint
    /// default).
    pub directory_mode: Option<u32>,
    /// The per-file size limit in bytes (`None` for unlimited).
    pub max_file_size: Option<u64>,
    /// The per-root entry limit (`None` for unlimited).
    pub max_entry_count: Option<u64>,
    /// Whether directories on another device than their root — mount
    /// points — are left alone rather than synchronized.
    pub ignore_mounts: bool,
    /// The staging placement for both endpoints.
    pub staging: StagingMode,
    /// The owner for created entries (`None` to leave ownership alone).
    pub default_owner: Option<String>,
    /// The group for created entries (`None` to leave ownership alone).
    pub default_group: Option<String>,
    /// Peering, when the mode asks for it: the betas can take the lead
    /// while the alpha is away, with this timing. `None` for the plain
    /// modes. Reconciliation never looks at this; `mode` says all it needs.
    pub peering: Option<PeeringPlan>,
    /// The stable identifier isolating this session's state, derived from
    /// the *resolved* endpoint identities (see
    /// [`resolve_for_identity`]) so that
    /// textual aliases of the same roots — across configurations, or between
    /// a supervisor and a manual `sync` — share one identity and therefore
    /// one state lock.
    identifier: String,
    /// Whether the group said it means to synchronize credentials, which
    /// silences [`secret_warnings`] for its roots.
    pub acknowledge_secrets: bool,
    /// The beta path, shown in the label when another plan of the group
    /// shares this one's host: `group@host` alone would name both.
    shown_path: Option<String>,
}

/// A resolved synchronization endpoint: either side of a session.
#[derive(Clone, Debug, PartialEq)]
pub enum EndpointTarget {
    /// A local directory (tilde-expanded).
    Local(PathBuf),
    /// A remote root reached through an agent.
    Remote {
        /// The SSH destination (`host` or `user@host`).
        destination: String,
        /// The root path on the remote side (possibly home-relative; the
        /// agent expands it against its own home).
        path: String,
        /// An agent command overriding SSH (whitespace-split argv).
        agent_command: Option<Vec<String>>,
    },
}

impl EndpointTarget {
    /// The target `autobahn sync ALPHA BETA` builds for one side: the
    /// frozen resolution of a local root, or the remote root an agent
    /// command or SSH reaches. What the topology checks compare.
    pub fn manual(spec: &str, agent: Option<&str>, frozen: Option<&Path>) -> EndpointTarget {
        if let Some(path) = frozen {
            return EndpointTarget::Local(path.to_owned());
        }
        let (destination, path) = match agent {
            Some(_) => ("", spec),
            None => spec.split_once(':').unwrap_or(("", spec)),
        };
        EndpointTarget::Remote {
            destination: destination.to_owned(),
            path: path.to_owned(),
            agent_command: agent
                .map(|command| command.split_whitespace().map(str::to_owned).collect()),
        }
    }
}

impl SessionPlan {
    /// Returns the display name of the session: `group@host`, or
    /// `group@host:path` when another beta of the group is on the same
    /// host. For people to read; sessions are told apart by
    /// [`identifier`](Self::identifier).
    pub fn display(&self) -> String {
        match &self.shown_path {
            Some(path) => format!("{}@{}:{path}", self.group, self.host),
            None => format!("{}@{}", self.group, self.host),
        }
    }

    /// Returns the beta specification string used for session identity.
    pub fn beta_spec(&self) -> String {
        match &self.beta {
            EndpointTarget::Local(path) => path.to_string_lossy().into_owned(),
            EndpointTarget::Remote {
                destination, path, ..
            } => format!("{destination}:{path}"),
        }
    }

    /// Returns the stable identifier isolating this session's state.
    pub fn identifier(&self) -> String {
        self.identifier.clone()
    }

    /// Peering: the session between the configured alpha and this host,
    /// as a beta that leads runs it — the alpha reached by attachment and
    /// still the alpha of the pair, this host's own root (the alpha of
    /// `self`) as the beta, under the identifier the leader pushed so it
    /// is the same session the leader ran.
    pub(crate) fn attached_alpha(&self, alpha_path: &str, identifier: String) -> SessionPlan {
        let own = match &self.alpha {
            EndpointTarget::Local(path) => path.clone(),
            EndpointTarget::Remote { path, .. } => PathBuf::from(path),
        };
        let alpha = EndpointTarget::Remote {
            destination: crate::peering::attached_destination(crate::peering::ALPHA),
            path: alpha_path.to_owned(),
            agent_command: None,
        };
        let beta = EndpointTarget::Local(own);
        SessionPlan {
            host: crate::peering::ALPHA.to_owned(),
            alpha_identity: target_identity(&alpha),
            beta_identity: target_identity(&beta),
            alpha_spec: alpha_path.to_owned(),
            alpha,
            beta,
            identifier,
            shown_path: None,
            ..self.clone()
        }
    }

    /// The mode as the configuration spells it: the peering spelling for
    /// a peering plan, the canonical grid name otherwise. What `status`
    /// shows, so a reader sees the word they wrote.
    pub fn mode_name(&self) -> &'static str {
        match (self.peering, self.mode) {
            (Some(_), SyncMode::TwoWayResolved) => "peering-alpha-dangerously-experimental",
            (Some(_), _) => "peering-conflict-dangerously-experimental",
            (None, mode) => mode_name(mode),
        }
    }
}

/// Computes a session identity string for an endpoint target: the resolved
/// physical path for a local target, the textual `destination:path` for a
/// remote one (whose paths can only be resolved on the remote side).
fn target_identity(target: &EndpointTarget) -> String {
    match target {
        EndpointTarget::Local(path) => resolve_for_identity(path).to_string_lossy().into_owned(),
        EndpointTarget::Remote {
            destination, path, ..
        } => format!("{destination}:{path}"),
    }
}

/// Where an endpoint's tree sits, for comparing it with another: the
/// scope in which paths are comparable at all (`None` for this machine,
/// the destination and transport for a remote), and the path split into
/// components. Local identities are resolved physical paths, split by
/// [`Path::components`]. Remote identities can only be compared
/// textually, and only within one scope, so a remote path is normalized
/// just enough that spelling never matters where it cannot mean
/// anything: empty and `.` components are dropped, `..` is kept as it
/// is, and a leading `/` is kept as a component of its own, since
/// `/tree` and a home-relative `tree` are different trees.
#[derive(Debug, PartialEq)]
struct Place {
    scope: Option<(String, Option<Vec<String>>)>,
    components: Vec<String>,
}

impl Place {
    fn of(target: &EndpointTarget, identity: &str) -> Place {
        match target {
            EndpointTarget::Local(_) => Place {
                scope: None,
                components: Path::new(identity)
                    .components()
                    .filter(|component| !matches!(component, Component::CurDir))
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect(),
            },
            EndpointTarget::Remote {
                destination,
                path,
                agent_command,
            } => Place {
                scope: Some((destination.clone(), agent_command.clone())),
                components: path
                    .starts_with('/')
                    .then(|| "/".to_owned())
                    .into_iter()
                    .chain(
                        path.split('/')
                            .filter(|component| !component.is_empty() && *component != ".")
                            .map(str::to_owned),
                    )
                    .collect(),
            },
        }
    }

    /// The path of `inner` relative to this place, when this place
    /// strictly contains it: the question an ignore pattern answers.
    fn contains(&self, inner: &Place) -> Option<String> {
        (self.scope == inner.scope
            && self.components.len() < inner.components.len()
            && inner.components.starts_with(&self.components))
        .then(|| inner.components[self.components.len()..].join("/"))
    }
}

/// Refuses a session whose two sides are one tree, or one inside the
/// other: the session would consume its own output. Reconciliation sees
/// the copy as divergence and, in replica mode, deletes the alpha root
/// through the beta path. This was reproduced, not hypothesized, first
/// from a configuration and then from `autobahn sync ALPHA BETA`, so every
/// way of building a session asks this before any endpoint opens.
///
/// The identities are the resolved ones the session is identified by, so
/// a symlink alias or a trailing `/.` is the same tree. Relays, where one
/// session's beta feeds another's alpha, are a different question and
/// stay legal: only overlap within a single session is self-referential.
pub fn check_session_topology(
    alpha: &EndpointTarget,
    beta: &EndpointTarget,
    alpha_identity: &str,
    beta_identity: &str,
) -> Result<(), String> {
    let alpha = Place::of(alpha, alpha_identity);
    let beta = Place::of(beta, beta_identity);
    let how = if alpha == beta {
        "the same tree"
    } else if alpha.contains(&beta).is_some() {
        "a tree inside the alpha"
    } else if beta.contains(&alpha).is_some() {
        "a tree containing the alpha"
    } else {
        return Ok(());
    };
    Err(format!(
        "the beta is {how}; a session cannot synchronize a tree with itself or \
         with a tree that contains it"
    ))
}

/// Whether a root-relative directory is kept out of the synchronization by
/// these ignore patterns: it, or a directory above it, is ignored, and no
/// negation reaches back inside what is ignored.
fn excluded_by(ignores: &IgnoreSet, relative: &str) -> bool {
    let mut prefix = String::new();
    relative.split('/').any(|component| {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        ignores.ignored(&prefix, true) && !ignores.holds_a_re_inclusion(&prefix)
    })
}

/// Autobahn's own directories on this machine, which no local root may
/// hold: the state root (the ancestors, locks, status and installed
/// agents, and by default the configuration and the alert hook), the
/// default state root when another is in use (the endpoint-pair locks
/// live there whatever the override), and the directory the
/// configuration file really lives in. Synchronized, they change under
/// the session that is using them, and a peer that edits its copy of
/// `config.toml` or `on-alert.sh` chooses what runs here next.
///
/// Only local endpoints are checked: the controller cannot see a remote
/// host's resolved state root, and the agent guards its own.
pub struct OwnState {
    /// Each directory as resolved, with what it holds.
    places: Vec<(String, &'static str)>,
}

impl OwnState {
    /// The directories for a run over this state root, and this
    /// configuration file when there is one.
    pub fn new(state_root: &Path, config: Option<&Path>) -> OwnState {
        let mut places: Vec<(String, &'static str)> = Vec::new();
        let mut add = |path: &Path, what: &'static str| {
            let identity = resolve_for_identity(path).to_string_lossy().into_owned();
            if !places.iter().any(|(known, _)| *known == identity) {
                places.push((identity, what));
            }
        };
        add(state_root, "own state");
        if let Ok(default) = crate::paths::default_state_root() {
            add(&default, "own state");
        }
        if let Some(directory) =
            config.and_then(|path| resolve_for_identity(path).parent().map(Path::to_owned))
        {
            add(&directory, "configuration");
        }
        OwnState { places }
    }

    /// Refuses a local root that is, or holds, one of the directories,
    /// unless the root's ignore patterns keep that directory out of the
    /// synchronization, as `ignores = [".autobahn"]` does for a root of
    /// `~`.
    pub fn check(
        &self,
        target: &EndpointTarget,
        identity: &str,
        ignores: &[String],
    ) -> Result<(), String> {
        if !matches!(target, EndpointTarget::Local(_)) {
            return Ok(());
        }
        let root = Place::of(target, identity);
        for (own, what) in &self.places {
            let place = Place::of(&EndpointTarget::Local(PathBuf::from(own)), own);
            if root == place {
                return Err(format!(
                    "the root {identity} is autobahn's {what}; choose another root"
                ));
            }
            let Some(relative) = root.contains(&place) else {
                continue;
            };
            if IgnoreSet::new(ignores).is_ok_and(|ignores| excluded_by(&ignores, &relative)) {
                continue;
            }
            return Err(format!(
                "the root {identity} contains autobahn's {what} at {own}; add it to \
                 ignores, or choose a narrower root"
            ));
        }
        Ok(())
    }

    /// Checks both sides of a session.
    pub fn check_session(
        &self,
        (alpha, alpha_identity): (&EndpointTarget, &str),
        (beta, beta_identity): (&EndpointTarget, &str),
        ignores: &[String],
    ) -> Result<(), String> {
        self.check(alpha, alpha_identity, ignores)?;
        self.check(beta, beta_identity, ignores)
    }

    /// Checks every planned session, reporting each offending root once.
    pub fn check_plans(&self, plans: &[SessionPlan]) -> Result<()> {
        let mut problems: Vec<String> = Vec::new();
        for plan in plans {
            if let Err(problem) = self.check_session(
                (&plan.alpha, &plan.alpha_identity),
                (&plan.beta, &plan.beta_identity),
                &plan.ignores,
            ) {
                let problem = format!("group '{}': {problem}", plan.group);
                if !problems.contains(&problem) {
                    problems.push(problem);
                }
            }
        }
        if !problems.is_empty() {
            bail!("invalid configuration:\n  {}", problems.join("\n  "));
        }
        Ok(())
    }
}

/// Whether a mode writes its alpha. A `match` with no wildcard arm, so
/// a mode added later is classified by whoever adds it: counted read-only
/// by default, it would escape the cross-session nesting check.
fn alpha_is_written(mode: SyncMode) -> bool {
    match mode {
        SyncMode::TwoWaySafe
        | SyncMode::TwoWayParanoid
        | SyncMode::TwoWayResolved
        | SyncMode::TwoWayStrict => true,
        SyncMode::OneWaySafe | SyncMode::OneWayReplica => false,
    }
}

/// The directories under a root that hold credentials, which a root
/// covering a home directory would send to every destination. Not ignored
/// by default, since someone may mean to synchronize them; warned about
/// instead.
const SECRET_PATHS: [&str; 5] = [".ssh", ".aws", ".gnupg", ".config/gcloud", ".kube"];

/// Describes the credentials a local root would synchronize, if any: the
/// credential directories it holds that its ignores leave in, or, for a
/// home directory, every one they leave in, since a home that does not
/// hold one yet soon may. `None` when there is nothing to say.
pub fn secrets_warning(identity: &str, ignores: &[String]) -> Option<String> {
    let home = std::env::var_os("HOME").map(|home| resolve_for_identity(Path::new(&home)));
    secrets_warning_in(identity, ignores, home.as_deref())
}

fn secrets_warning_in(identity: &str, ignores: &[String], home: Option<&Path>) -> Option<String> {
    let root = Path::new(identity);
    let is_home = home == Some(root);
    let ignores = IgnoreSet::new(ignores).ok();
    let found: Vec<&str> = SECRET_PATHS
        .into_iter()
        .filter(|path| is_home || root.join(path).symlink_metadata().is_ok())
        .filter(|path| {
            !ignores
                .as_ref()
                .is_some_and(|ignores| excluded_by(ignores, path))
        })
        .collect();
    if found.is_empty() {
        return None;
    }
    let home = match is_home {
        true => " is your home directory, and",
        false => "",
    };
    Some(format!(
        "the root {identity}{home} holds credentials ({}) that every destination will \
         receive",
        found.join(", ")
    ))
}

/// The credential warnings for a set of plans: one per local root, however
/// many sessions share it, skipping the groups that set
/// `acknowledge_secrets`. Part of [`Config::warnings`], said once per load.
pub fn secret_warnings(plans: &[SessionPlan]) -> Vec<String> {
    let mut seen: Vec<&str> = Vec::new();
    let mut warnings = Vec::new();
    for plan in plans.iter().filter(|plan| !plan.acknowledge_secrets) {
        for (target, identity) in [
            (&plan.alpha, &plan.alpha_identity),
            (&plan.beta, &plan.beta_identity),
        ] {
            if !matches!(target, EndpointTarget::Local(_)) || seen.contains(&identity.as_str()) {
                continue;
            }
            seen.push(identity);
            if let Some(warning) = secrets_warning(identity, &plan.ignores) {
                warnings.push(format!(
                    "group '{}': {warning}; add them to the group's `ignores`, or set \
                     `acknowledge_secrets = true` on it if that is meant",
                    plan.group
                ));
            }
        }
    }
    warnings
}

fn default_reload() -> bool {
    true
}

impl Config {
    /// Loads and parses a configuration file.
    pub fn load(path: &std::path::Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("unable to read configuration {}", path.display()))?;
        // It names commands autobahn runs. Warned, not refused, so a
        // setup that worked before keeps working.
        if let Some(problem) = crate::paths::loose_write_permissions(path) {
            crate::complain!(
                "warning: {problem}; whoever can change the configuration can run commands as you"
            );
        }
        Config::parse(path, &text)
    }

    /// Parses a configuration already read from `path`, which only names
    /// it in errors.
    pub fn parse(path: &std::path::Path, text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text)
            .with_context(|| format!("unable to parse configuration {}", path.display()))?;
        // A group is named on command lines — its own, and the ones
        // autobahn suggests — where a leading dash reads as an option.
        if let Some(name) = config.groups.keys().find(|name| name.starts_with('-')) {
            bail!(
                "invalid configuration {}:\n  group {name:?}: a group name cannot start with \
                 '-', which commands would read as an option",
                path.display()
            );
        }
        Ok(config)
    }

    /// Every host this configuration names, in configuration order: each
    /// group's alpha when it is remote, and every remote beta. What `disable`
    /// checks a name against, so a typo is refused rather than written into
    /// the file and quietly ignored.
    pub fn known_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = Vec::new();
        for group in self.groups.values() {
            for spec in std::iter::once(&group.alpha).chain(group.betas.iter()) {
                let destination = spec.split(':').next().unwrap_or(spec);
                if destination.starts_with('/') || destination.starts_with('~') {
                    continue;
                }
                let host = host_of(destination).to_owned();
                if !host.is_empty() && !hosts.contains(&host) {
                    hosts.push(host);
                }
            }
        }
        hosts
    }

    /// The groups whose alpha is this host. Disabling one of these takes the
    /// whole group with it, which is worth saying out loud before it happens.
    pub fn groups_led_by(&self, host: &str) -> Vec<String> {
        self.groups
            .iter()
            .filter(|(_, group)| {
                let destination = group.alpha.split(':').next().unwrap_or(&group.alpha);
                !destination.starts_with('/')
                    && !destination.starts_with('~')
                    && host_of(destination) == host
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Derives the session plans this configuration describes, excluding
    /// disabled hosts. Every problem in the configuration is reported, not
    /// just the first.
    /// Resolves the `[alerts]` section into the plan the supervisor
    /// follows.
    ///
    /// Validated here rather than at the moment something goes wrong: a
    /// misspelled state name or an unreadable duration must be a
    /// configuration error at startup, not a silent no-op discovered on the
    /// night the alert was supposed to fire.
    pub fn alert_plan(&self) -> Result<crate::alerts::AlertPlan> {
        use crate::alerts::Alert;

        // A configuration written against the old shape is answered, not
        // merely rejected: the keys moved, and the reader should be told
        // where to rather than left with "unknown field".
        if self.alerts.is_some() {
            bail!(
                "invalid configuration:\n  [alerts] has moved: put `on_alert` at the top \
                 level, and anything else that was in [alerts] under [advanced.alerts]. \
                 The per-state hold times are now built in, so [alerts.after] can usually \
                 just be deleted."
            );
        }

        let advanced = &self.experimental.alerts;
        let duration = |spec: &Option<DurationSpec>, what: &str, fallback: Duration| match spec {
            None => Ok(fallback),
            Some(spec) => parse_duration(spec).map_err(|message| {
                anyhow!("invalid configuration:\n  experimental.alerts.{what}: {message}")
            }),
        };

        // Three layers, narrowest last. The built-in table gives every
        // state a considered value. `alert_after`, when written, is a
        // statement about the whole thing — "hold everything this long" —
        // so it replaces the built-ins rather than sitting behind them,
        // which would have made it dead the moment every state had an
        // entry. `[after]` then overrides individual states.
        let default_after = duration(&advanced.alert_after, "alert_after", DEFAULT_ALERT_AFTER)?;
        let mut after: BTreeMap<Alert, Duration> = Alert::all()
            .into_iter()
            .map(|alert| {
                let hold = match advanced.alert_after {
                    Some(_) => default_after,
                    None => built_in_after(alert),
                };
                (alert, hold)
            })
            .collect();
        for (name, spec) in &advanced.after {
            let Some(alert) = Alert::parse(name) else {
                let known: Vec<&str> = Alert::all().iter().map(|alert| alert.name()).collect();
                bail!(
                    "invalid configuration:\n  experimental.alerts.after.{name}: unknown state \
                     (expected one of: {})",
                    known.join(", ")
                );
            };
            after.insert(
                alert,
                parse_duration(spec).map_err(|message| {
                    anyhow!("invalid configuration:\n  experimental.alerts.after.{name}: {message}")
                })?,
            );
        }

        Ok(crate::alerts::AlertPlan {
            on_alert: self.on_alert.clone(),
            after,
            default_after,
            repeat_after: duration(&advanced.repeat_after, "repeat_after", Duration::ZERO)?,
            settle_after: duration(&advanced.settle_after, "settle_after", DEFAULT_SETTLE_AFTER)?,
            coalesce_after: duration(
                &advanced.coalesce_after,
                "coalesce_after",
                DEFAULT_COALESCE_AFTER,
            )?,
            timeout: duration(&advanced.timeout, "timeout", DEFAULT_ALERT_TIMEOUT)?,
        })
    }

    /// The peering timing, from `[advanced.peering-dangerously-experimental]` and the
    /// built-in defaults. Resolved whether or not any group is in a
    /// peering mode: a bad value is a configuration error either way.
    pub fn peering_plan(&self) -> Result<PeeringPlan> {
        if self.experimental.retired_peering.is_some() {
            bail!(
                "invalid configuration:\n  [advanced.peering-experimental] was renamed to \
                 [experimental.peering-dangerously-experimental]: peering has known security \
                 and collision issues. Read docs/peering.md before enabling it."
            );
        }
        let advanced = &self.experimental.peering;
        let duration = |spec: &Option<DurationSpec>, what: &str, fallback: Duration| {
            match spec {
            None => Ok(fallback),
            Some(spec) => parse_duration(spec).map_err(|message| {
                anyhow!("invalid configuration:\n  experimental.peering-dangerously-experimental.{what}: {message}")
            }),
        }
        };
        let ttl = duration(&advanced.ttl, "ttl", DEFAULT_PEERING_TTL)?;
        let failover_after = duration(
            &advanced.failover_after,
            "failover_after",
            DEFAULT_PEERING_FAILOVER_AFTER,
        )?;
        // A lease has to be stale before anyone may act on it; a wait
        // shorter than the lease would mean acting on a lease that is
        // still good.
        if failover_after < ttl {
            bail!(
                "invalid configuration:\n  experimental.peering-dangerously-experimental.failover_after \
                 ({}s) is shorter than ttl ({}s); a peer must not take the lead while the \
                 lease is still valid",
                failover_after.as_secs(),
                ttl.as_secs()
            );
        }
        if ttl.is_zero() {
            bail!("invalid configuration:\n  experimental.peering-dangerously-experimental.ttl must not be zero");
        }
        Ok(PeeringPlan {
            ttl,
            failover_after,
        })
    }

    pub fn plans(&self) -> Result<Vec<SessionPlan>> {
        let (plans, mut warnings) = self.plans_and_warnings()?;
        // Nothing is said here: planning happens many times per load, and
        // the warnings are gathered for a caller to say once.
        self.warnings.get_or_init(|| {
            warnings.extend(secret_warnings(&plans));
            warnings
        });
        Ok(plans)
    }

    /// What is worth saying about this configuration without refusing it:
    /// a wildcard negation that can have no effect, and a root that holds
    /// credentials nothing ignores. Gathered once, by the first planning,
    /// so a caller that says them once per load says each once however
    /// often the sessions are planned. Empty for a configuration that does
    /// not plan, whose error is what to say instead.
    pub fn warnings(&self) -> &[String] {
        if self.warnings.get().is_none() && self.plans().is_err() {
            return &[];
        }
        self.warnings.get().map(Vec::as_slice).unwrap_or_default()
    }

    /// The sessions, and the ineffective negations found planning them.
    fn plans_and_warnings(&self) -> Result<(Vec<SessionPlan>, Vec<String>)> {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        if self.disabled.is_some() {
            errors.push(
                "`disabled` at the top level is now `disabled_hosts`. A group has a \
                 `disabled = true` of its own, and one word cannot be both a list of \
                 hosts and a switch."
                    .to_owned(),
            );
        }
        let peering = match self.peering_plan() {
            Ok(plan) => Some(plan),
            Err(error) => {
                errors.push(format!("{error:#}"));
                None
            }
        };
        // Ignore files live under the *default* state root, not under an
        // override: they are the reader's own library of patterns, shared
        // by every configuration, and not part of any session's state.
        // Nothing is read unless a group names a file, so a reader who
        // does not use the directory never pays for it — or notices that
        // it is missing.
        let ignore_directory = match &self.ignore_directory {
            Some(directory) => directory.clone(),
            None => crate::paths::default_state_root()
                .map(|root| root.join(crate::scan::ignorefile::DIRECTORY))
                .unwrap_or_default(),
        };
        let mut plans = Vec::new();
        // Two plans over the same roots would synchronize the same trees
        // concurrently (and, when textually identical, share session state),
        // so duplicates are a configuration error rather than a runtime
        // surprise. Detection compares *canonicalized* local paths, so
        // aliases — a trailing `/.`, a symlink, `~/data` versus its expanded
        // form — are caught, not just textual repeats. (Nested or otherwise
        // overlapping roots are a different hazard that no pairwise identity
        // can detect.)
        let mut identities: HashMap<(String, String), String> = HashMap::new();

        for (name, group) in &self.groups {
            // A group that is off plans nothing: not its sessions, and not
            // the errors its settings would otherwise raise. Turning a
            // group off is exactly how a reader silences a group whose
            // host is gone, so it must not still be refused for it.
            if group.disabled {
                continue;
            }
            // A complaint names the place somebody would go to fix it.
            // For a value this group wrote that is the group; for one it
            // inherited it is the defaults, and saying "group 'aws'"
            // about a line in `[defaults]` sends them to the wrong table
            // — once per group that inherits it.
            let blame = |mine: bool, key: &str, message: &dyn std::fmt::Display| match mine {
                true => format!("group '{name}': {key}: {message}"),
                false => format!("the defaults' {key}: {message}"),
            };
            let (mode, peers) = match inherited(
                group.mode.as_deref(),
                self.defaults.mode.as_deref(),
            ) {
                Some((mode, mine)) => match parse_mode_spec(mode) {
                    Ok((mode, peers)) => (Some(mode), peers),
                    Err(message) => {
                        errors.push(blame(mine, "mode", &message));
                        (None, false)
                    }
                },
                None => {
                    errors.push(format!(
                        "group '{name}': mode: none here, and the defaults specify none either"
                    ));
                    (None, false)
                }
            };
            // Peering is a property of the plan, not of reconciliation:
            // the mode word carries it, the timing comes from the section.
            let peering = match (peers, peering) {
                (true, Some(plan)) => Some(plan),
                _ => None,
            };
            let power_durability = match parse_word(
                DURABILITY,
                "durability",
                group
                    .durability
                    .as_deref()
                    .or(self.defaults.durability.as_deref())
                    .unwrap_or("process"),
            ) {
                Ok(word) => word.word == "power",
                Err(complaint) => {
                    errors.push(format!("group '{name}': durability: {complaint}"));
                    false
                }
            };
            if group.alpha.is_empty() {
                errors.push(format!("group '{name}': alpha: cannot be empty"));
            }
            if group.betas.is_empty() {
                errors.push(format!("group '{name}': betas: a group needs at least one"));
            }
            let agent_command = match &group.agent_command {
                None => None,
                Some(command) => {
                    let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
                    if argv.is_empty() {
                        errors.push(format!("group '{name}': agent_command: cannot be empty"));
                        None
                    } else {
                        Some(argv)
                    }
                }
            };

            // The alpha side accepts the same specifications as a beta,
            // except that a remote alpha must carry an explicit path (there
            // is nothing for it to inherit one from).
            let alpha = if group.alpha.is_empty() {
                None
            } else {
                match parse_endpoint(&group.alpha, None, agent_command.clone()) {
                    Ok(EndpointTarget::Local(path)) if !path.is_absolute() => {
                        // A relative alpha would resolve against whatever
                        // working directory the supervisor happened to start
                        // in — a different tree under a service than in a
                        // shell.
                        errors.push(format!(
                            "group '{name}': alpha: '{}' must be an absolute (or ~-relative) \
                             path",
                            group.alpha
                        ));
                        None
                    }
                    Ok(target) => Some(target),
                    Err(message) => {
                        errors.push(format!(
                            "group '{name}': alpha: '{}': {message}",
                            group.alpha
                        ));
                        None
                    }
                }
            };
            // A disabled alpha host takes the whole group with it: every
            // session of the group flows through that endpoint.
            if let Some(EndpointTarget::Remote { destination, .. }) = &alpha {
                if self
                    .disabled_hosts
                    .iter()
                    .any(|d| d == host_of(destination))
                {
                    continue;
                }
            }
            // Peering assumes the alpha is the machine this configuration
            // runs on: it is the one member that is never dialed, so it
            // has to be the one doing the dialing. A remote alpha would
            // mean a supervisor on a third machine, which the lease and
            // the handoff do not model.
            if peering.is_some() {
                if let Some(EndpointTarget::Remote { .. }) = &alpha {
                    errors.push(format!(
                        "group '{name}': mode: a peering mode needs a local alpha; '{}' is \
                         remote",
                        group.alpha
                    ));
                    continue;
                }
            }
            // The path a remote beta inherits when it names none: the
            // alpha's path portion, as written.
            let inherited_path = match &alpha {
                Some(EndpointTarget::Remote { path, .. }) => path.clone(),
                _ => group.alpha.clone(),
            };

            // Widest first, narrowest last, because the last matching
            // pattern decides: the defaults' files, then the defaults'
            // own patterns, then the group's files, then the group's own.
            // A group can therefore re-include something a shared file
            // excluded, which is the point of having both.
            let mut ignores = Vec::new();
            let mut ignore_errors = Vec::new();
            for (mine, names) in [
                (false, &self.defaults.ignore_files),
                (true, &group.ignore_files),
            ] {
                for file in names {
                    match crate::scan::ignorefile::read(&ignore_directory, file) {
                        Ok(patterns) => ignores.extend(patterns),
                        Err(error) => {
                            ignore_errors.push(blame(mine, "ignore_files", &format!("{error:#}")))
                        }
                    }
                }
                if !mine {
                    ignores.extend(self.defaults.ignores.iter().cloned());
                }
            }
            ignores.extend(group.ignores.iter().cloned());
            errors.extend(ignore_errors);
            // Compile the combined patterns now, so a bad pattern is a
            // configuration error alongside the others rather than a runtime
            // failure discovered only by the affected session's worker.
            match IgnoreSet::new(&ignores) {
                Err(error) => {
                    errors.push(format!("group '{name}': ignores: invalid pattern: {error:#}"))
                }
                // A line that cannot ever do anything is a mistake worth
                // refusing, not a preference: combining ignore files
                // written independently is exactly how they appear, and
                // the reader cannot see it by looking at either file.
                Ok(compiled) => {
                    // A wildcard negation under an ignored directory is
                    // said, not refused: such a list ran before, just
                    // without the effect it meant.
                    for warning in compiled.ineffective_negations() {
                        warnings.push(format!("group '{name}': {warning}"));
                    }
                    errors.extend(
                        compiled
                            .dead_negations()
                            .into_iter()
                            .map(|dead| format!("group '{name}': ignores: {dead}")),
                    )
                }
            }
            let interval = Duration::from_secs(
                group
                    .interval
                    .or(self.defaults.interval)
                    .unwrap_or(DEFAULT_INTERVAL_SECONDS)
                    .max(1),
            );
            // The lease is renewed once per cycle, and an idle session
            // cycles once per interval: a lease that lives less than two
            // intervals goes stale between renewals and reads as a dead
            // leader every few seconds.
            if let Some(plan) = peering {
                if plan.ttl < interval.saturating_mul(2) {
                    errors.push(format!(
                        "group '{name}': experimental.peering-dangerously-experimental.ttl ({}s) must be at \
                         least twice the interval ({}s); the lease is renewed once per cycle",
                        plan.ttl.as_secs(),
                        interval.as_secs()
                    ));
                    continue;
                }
            }
            let symlink_mode = match inherited(
                group.symlink_mode.as_deref(),
                self.defaults.symlink_mode.as_deref(),
            ) {
                None => SymlinkMode::default(),
                Some((mode, mine)) => match parse_symlink_mode(mode) {
                    Ok(mode) => mode,
                    Err(message) => {
                        errors.push(blame(mine, "symlink_mode", &message));
                        SymlinkMode::default()
                    }
                },
            };
            let mut permission = |held: Option<(&str, bool)>, directory: bool| {
                let key = match directory {
                    true => "directory_mode",
                    false => "file_mode",
                };
                match held {
                    None => None,
                    Some((mode, mine)) => match parse_permission_mode(mode, directory) {
                        Ok(bits) => Some(bits),
                        Err(message) => {
                            errors.push(blame(mine, key, &message));
                            None
                        }
                    },
                }
            };
            let file_mode = permission(
                inherited(
                    group.file_mode.as_deref(),
                    self.defaults.file_mode.as_deref(),
                ),
                false,
            );
            let directory_mode = permission(
                inherited(
                    group.directory_mode.as_deref(),
                    self.defaults.directory_mode.as_deref(),
                ),
                true,
            );
            // The same question, for a value that is not a string:
            // whichever table has it is the one to blame.
            let size = match (&group.max_file_size, &self.defaults.max_file_size) {
                (Some(spec), _) => Some((spec, true)),
                (None, Some(spec)) => Some((spec, false)),
                (None, None) => None,
            };
            let max_file_size = match size {
                None => None,
                Some((spec, mine)) => match parse_size(spec) {
                    Ok(bytes) => Some(bytes),
                    Err(message) => {
                        errors.push(blame(mine, "max_file_size", &message));
                        None
                    }
                },
            };
            let max_entry_count = group.max_entry_count.or(self.defaults.max_entry_count);
            let ignore_mounts = group
                .ignore_mounts
                .or(self.defaults.ignore_mounts)
                .unwrap_or(true);
            let staging = match inherited(
                group.staging.as_deref(),
                self.defaults.staging.as_deref(),
            ) {
                None => StagingMode::default(),
                Some((mode, mine)) => match parse_staging_mode(mode) {
                    Ok(mode) => mode,
                    Err(message) => {
                        errors.push(blame(mine, "staging", &message));
                        StagingMode::default()
                    }
                },
            };
            let mut ownership = |held: Option<(&str, bool)>, key: &str| match held {
                None => None,
                Some(("", mine)) => {
                    errors.push(blame(mine, key, &"cannot be empty"));
                    None
                }
                Some((spec, _)) => Some(spec.to_owned()),
            };
            let default_owner = ownership(
                inherited(
                    group.default_owner.as_deref(),
                    self.defaults.default_owner.as_deref(),
                ),
                "default_owner",
            );
            let default_group = ownership(
                inherited(
                    group.default_group.as_deref(),
                    self.defaults.default_group.as_deref(),
                ),
                "default_group",
            );

            for beta in &group.betas {
                if beta.is_empty() {
                    errors.push(format!("group '{name}': betas: one of them is empty"));
                    continue;
                }
                let target =
                    match parse_endpoint(beta, Some(&inherited_path), agent_command.clone()) {
                        Ok(target) => target,
                        Err(message) => {
                            errors.push(format!("group '{name}': betas: '{beta}': {message}"));
                            continue;
                        }
                    };
                if let EndpointTarget::Local(path) = &target {
                    // A peer is a machine that can take the lead. A local
                    // path is this machine again, and this machine is the
                    // alpha already.
                    if peering.is_some() {
                        errors.push(format!(
                            "group '{name}': betas: '{beta}': a peering mode needs every beta on \
                             another host"
                        ));
                        continue;
                    }
                    // The same working-directory hazard as a relative alpha,
                    // and the trap that catches unexpanded `~user` forms.
                    if !path.is_absolute() {
                        errors.push(format!(
                            "group '{name}': betas: '{beta}' must be an absolute (or ~-relative) \
                             local path"
                        ));
                        continue;
                    }
                }
                let host = match &target {
                    EndpointTarget::Local(path) => path.to_string_lossy().into_owned(),
                    EndpointTarget::Remote { destination, .. } => host_of(destination).to_owned(),
                };
                if let EndpointTarget::Remote { .. } = &target {
                    if self.disabled_hosts.iter().any(|disabled| disabled == &host) {
                        continue;
                    }
                }
                let (Some(mode), Some(alpha)) = (mode, alpha.clone()) else {
                    continue;
                };
                let alpha_identity = target_identity(&alpha);
                let beta_identity = target_identity(&target);
                let identifier = session_identifier(&alpha_identity, &beta_identity);
                let plan = SessionPlan {
                    group: name.clone(),
                    host,
                    alpha,
                    alpha_spec: group.alpha.clone(),
                    beta: target,
                    alpha_identity: alpha_identity.clone(),
                    beta_identity: beta_identity.clone(),
                    mode,
                    ignores: ignores.clone(),
                    interval,
                    power_durability,
                    symlink_mode,
                    file_mode,
                    directory_mode,
                    max_file_size,
                    max_entry_count,
                    ignore_mounts,
                    staging,
                    default_owner: default_owner.clone(),
                    default_group: default_group.clone(),
                    peering,
                    identifier,
                    acknowledge_secrets: group.acknowledge_secrets,
                    shown_path: None,
                };
                if let Err(problem) =
                    check_session_topology(&plan.alpha, &plan.beta, &alpha_identity, &beta_identity)
                {
                    errors.push(format!("session '{}': {problem}", plan.display()));
                    continue;
                }
                if let Some(previous) =
                    identities.insert((alpha_identity, beta_identity), plan.display())
                {
                    errors.push(format!(
                        "sessions '{previous}' and '{}' describe the same alpha and beta; \
                         they would synchronize the same trees concurrently",
                        plan.display()
                    ));
                    continue;
                }
                plans.push(plan);
            }
        }

        // Two betas of one group on one host would read alike, in status,
        // in the logs and in the messages below; they show their paths.
        let shared: Vec<usize> = (0..plans.len())
            .filter(|&index| {
                plans.iter().enumerate().any(|(other, plan)| {
                    other != index
                        && plan.group == plans[index].group
                        && plan.host == plans[index].host
                })
            })
            .collect();
        for index in shared {
            if let EndpointTarget::Remote { path, .. } = &plans[index].beta {
                plans[index].shown_path = Some(path.clone());
            }
        }

        // Across sessions, a writable endpoint nested inside another
        // session's endpoint means two sessions mutate one tree region from
        // independent ancestors: each can read the other's writes as user
        // edits and propagate them back, and a check/use window lets a
        // losing write travel. Containment is refused when either endpoint
        // involved is writable. *Equality* is different: identical shared
        // endpoints are the fan-out, star, and relay topologies — pinned
        // legal by tests and in ordinary use — so a shared writable
        // endpoint that is exactly equal warns instead of failing. Two
        // configurations in separate processes are outside what this can
        // see; the endpoint-pair lock covers the identical pair there, and
        // anything else is documented as unsupported.
        // A beta is always written; an alpha is written by the two-way
        // modes.
        let writable = |plan: &SessionPlan, alpha: bool| !alpha || alpha_is_written(plan.mode);
        let mut endpoints: Vec<(String, Place, bool, String)> = Vec::new();
        for plan in &plans {
            endpoints.push((
                plan.alpha_identity.clone(),
                Place::of(&plan.alpha, &plan.alpha_identity),
                writable(plan, true),
                plan.display(),
            ));
            endpoints.push((
                plan.beta_identity.clone(),
                Place::of(&plan.beta, &plan.beta_identity),
                writable(plan, false),
                plan.display(),
            ));
        }
        for (index, (identity, place, writes, owner)) in endpoints.iter().enumerate() {
            for (offset, (other_identity, other_place, other_writes, other_owner)) in
                endpoints.iter().enumerate().skip(index + 1)
            {
                // Endpoints are pushed two per plan, alpha then beta, so an
                // endpoint at index i belongs to plan i / 2. Plans, not
                // labels: two betas of one group on one host share a label
                // and are still two sessions.
                if index / 2 == offset / 2 {
                    continue; // within-session overlap is checked above
                }
                if place == other_place {
                    // Sessions sharing an endpoint *exactly* are the
                    // fan-out, star and relay topologies. They are not
                    // remarked on: they share one observer and one scan of
                    // that root, every write is validated against the scan
                    // it was reconciled from, and a collision reaches the
                    // user as an ordinary conflict — the same one that a
                    // single session produces when both its sides change a
                    // file, which is likewise not warned about. What the
                    // *mode* does with that collision is a property of the
                    // mode, documented with the modes.
                    continue;
                }
                // Which one contains which decides both the message and,
                // below, whose ignore patterns are consulted.
                let (outer, inner, outer_index, relative) =
                    if let Some(relative) = place.contains(other_place) {
                        (identity, other_identity, index, relative)
                    } else if let Some(relative) = other_place.contains(place) {
                        (other_identity, identity, offset, relative)
                    } else {
                        continue;
                    };
                if !*writes && !*other_writes {
                    continue; // two read-only sources cannot disagree
                }
                // A nested endpoint the outer session *ignores* is not
                // shared with it at all: the outer never scans, never
                // writes, and never records a thing beneath that path. The
                // check compares roots, so without consulting the ignores
                // it refuses configurations that do not actually overlap —
                // "synchronize this project, and ship its build output
                // somewhere else" being the ordinary one.
                let outer_plan = &plans[outer_index / 2];
                let excluded = IgnoreSet::new(&outer_plan.ignores)
                    .is_ok_and(|ignores| ignores.ignored(&relative, true));
                if excluded {
                    continue;
                }
                errors.push(format!(
                    "sessions '{owner}' and '{other_owner}': endpoint {inner} is nested \
                     inside {outer} and at least one of them is written; two sessions \
                     cannot safely write one tree region from independent ancestors. \
                     Add it to the outer group's `ignores` if the outer session should \
                     leave that subtree alone"
                ));
            }
        }

        if !errors.is_empty() {
            let errors = fold(errors);
            bail!("invalid configuration:\n  {}", errors.join("\n  "));
        }
        Ok((plans, warnings))
    }
}

/// A value and whether this group is where it was written.
///
/// Every session setting falls back to `[defaults]`, and a complaint
/// about an inherited value belongs to the defaults: that is where it
/// is written and where it would be fixed. Saying "group 'aws'" about
/// a line in `[defaults]` sends a person to the wrong table, once for
/// every group that inherits it.
fn inherited<'a>(own: Option<&'a str>, shared: Option<&'a str>) -> Option<(&'a str, bool)> {
    match own {
        Some(value) => Some((value, true)),
        None => shared.map(|value| (value, false)),
    }
}

/// Says each fault once, however many groups ran into it.
///
/// Every group is planned in turn, so a value in `[defaults]` is read
/// once per group that inherits it. Each of those now blames the
/// defaults rather than the group ([`inherited`]), which makes the
/// eight copies identical — and identical is what this removes. A
/// fault that really is a group's own keeps its group, and stays.
fn fold(errors: Vec<String>) -> Vec<String> {
    let mut said: Vec<String> = Vec::new();
    for line in errors {
        if !said.contains(&line) {
            said.push(line);
        }
    }
    said
}

/// Parses a symbolic link mode name.
pub fn parse_symlink_mode(mode: &str) -> Result<SymlinkMode, String> {
    match parse_word(SYMLINK_MODES, "symlink mode", mode)?.word {
        "ignore" => Ok(SymlinkMode::Ignore),
        "portable" => Ok(SymlinkMode::Portable),
        _ => Ok(SymlinkMode::Raw),
    }
}

/// Parses octal permission bits for created files or directories. The owner
/// must retain enough access for synchronization itself to function: read
/// and write for files, read, write, and traverse for directories.
pub fn parse_permission_mode(mode: &str, directory: bool) -> Result<u32, String> {
    let digits = mode.strip_prefix("0o").unwrap_or(mode);
    let bits = u32::from_str_radix(digits, 8)
        .map_err(|_| format!("invalid octal permission mode '{mode}'"))?;
    if bits > 0o777 {
        return Err(format!(
            "permission mode '{mode}' carries bits outside the permission range"
        ));
    }
    let required = if directory { 0o700 } else { 0o600 };
    if bits & required != required {
        return Err(format!(
            "permission mode '{mode}' denies the owner access that \
             synchronization itself requires (at least {required:03o})"
        ));
    }
    Ok(bits)
}

/// Parses a staging placement name.
pub fn parse_staging_mode(mode: &str) -> Result<StagingMode, String> {
    match parse_word(STAGING_MODES, "staging placement", mode)?.word {
        "state" => Ok(StagingMode::State),
        "beside-root" => Ok(StagingMode::BesideRoot),
        _ => Ok(StagingMode::InsideRoot),
    }
}

/// Parses a size limit: a raw byte count, or an integer with a decimal
/// (`KB`, `MB`, `GB`, `TB`) or binary (`K`/`KiB`, `M`/`MiB`, `G`/`GiB`,
/// `T`/`TiB`) suffix. Matching is case-insensitive.
pub fn parse_size(spec: &SizeSpec) -> Result<u64, String> {
    let text = match spec {
        SizeSpec::Bytes(bytes) => return Ok(*bytes),
        SizeSpec::Text(text) => text.trim(),
    };
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, suffix) = text.split_at(split);
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("invalid size '{text}'"))?;
    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1000,
        "mb" => 1000_u64.pow(2),
        "gb" => 1000_u64.pow(3),
        "tb" => 1000_u64.pow(4),
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        other => return Err(format!("invalid size suffix '{other}' in '{text}'")),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size '{text}' overflows"))
}

/// Parses a duration: a plain number of seconds, or an integer with an
/// `s`, `m`, `h`, or `d` suffix. Matching is case-insensitive.
pub fn parse_duration(spec: &DurationSpec) -> Result<Duration, String> {
    let text = match spec {
        DurationSpec::Seconds(seconds) => return Ok(Duration::from_secs(*seconds)),
        DurationSpec::Text(text) => text.trim(),
    };
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, suffix) = text.split_at(split);
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("invalid duration '{text}'"))?;
    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3600,
        "d" => 86_400,
        other => return Err(format!("invalid duration suffix '{other}' in '{text}'")),
    };
    value
        .checked_mul(multiplier)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("duration '{text}' overflows"))
}

/// Every synchronization mode this build reads: its canonical name, the
/// older spellings still accepted for it, the reconciliation it runs and
/// whether the name asks for peering.
///
/// One table, read by the parser, by the error a bad name gets, by
/// [`mode_name`] and by the schema the editor builds its form from. A
/// mode is added by adding a row, so none of those four can fall behind
/// the others.
pub const MODES: &[ModeName] = &[
    ModeName {
        name: "two-way-conflict",
        also: &["two-way-safe"],
        mode: SyncMode::TwoWaySafe,
        peering: false,
        about: "Both ways. A file both sides changed is reported, never chosen between.",
    },
    ModeName {
        name: "two-way-paranoid",
        also: &[],
        mode: SyncMode::TwoWayParanoid,
        peering: false,
        about: "As two-way-conflict, and a large directory that goes empty or missing on one side is disbelieved rather than propagated.",
    },
    ModeName {
        name: "two-way-alpha",
        also: &["two-way-resolved"],
        mode: SyncMode::TwoWayResolved,
        peering: false,
        about: "Both ways, and alpha wins a collision — except that a deletion never beats an edit.",
    },
    ModeName {
        name: "two-way-alpha-strict",
        also: &[],
        mode: SyncMode::TwoWayStrict,
        peering: false,
        about: "As two-way-alpha with that exception removed: alpha's deletion beats beta's edit.",
    },
    ModeName {
        name: "one-way-conflict",
        also: &["one-way-safe"],
        mode: SyncMode::OneWaySafe,
        peering: false,
        about: "Alpha to beta only. A file changed on beta is reported rather than overwritten.",
    },
    ModeName {
        name: "one-way-alpha",
        also: &["one-way-replica", "mirror"],
        mode: SyncMode::OneWayReplica,
        peering: false,
        about: "Alpha to beta only, and beta is made to match — what rsync --delete does.",
    },
    ModeName {
        name: "peering-conflict-dangerously-experimental",
        also: &[],
        mode: SyncMode::TwoWaySafe,
        peering: true,
        about: "two-way-conflict, and a beta may take the lead while alpha is away. Known security and collision issues: read docs/peering.md first.",
    },
    ModeName {
        name: "peering-alpha-dangerously-experimental",
        also: &[],
        mode: SyncMode::TwoWayResolved,
        peering: true,
        about: "two-way-alpha, and a beta may take the lead while alpha is away. Known security and collision issues: read docs/peering.md first.",
    },
];

/// Names that were renamed, and what they were renamed to. A
/// configuration that uses one is told why rather than merely told the
/// name is unknown.
pub const RENAMED_MODES: &[(&str, &str, &str)] = &[
    (
        "peering-conflict-experimental",
        "peering-conflict-dangerously-experimental",
        "peering has known security and collision issues. Read docs/peering.md before enabling it",
    ),
    (
        "peering-alpha-experimental",
        "peering-alpha-dangerously-experimental",
        "peering has known security and collision issues. Read docs/peering.md before enabling it",
    ),
];

/// One row of [`MODES`].
#[derive(Debug)]
pub struct ModeName {
    /// What this mode is called now, and what `status` prints.
    pub name: &'static str,
    /// Older spellings of the same mode, still read.
    pub also: &'static [&'static str],
    /// The reconciliation it runs.
    pub mode: SyncMode,
    /// Whether the name asks for peering.
    pub peering: bool,
    /// One sentence, for a reader choosing between them.
    pub about: &'static str,
}

/// One word a configuration key accepts, and what it means.
///
/// The same shape as a row of [`MODES`] without the reconciliation: the
/// parser reads it, the error a wrong word gets is written from it, and
/// the editor's form offers exactly these.
#[derive(Debug)]
pub struct Word {
    /// The canonical spelling.
    pub word: &'static str,
    /// Older or looser spellings that mean the same thing.
    pub also: &'static [&'static str],
    /// One line, for a person choosing between them.
    pub about: &'static str,
}

/// What `symlink_mode` accepts.
pub const SYMLINK_MODES: &[Word] = &[
    Word {
        word: "ignore",
        also: &[],
        about: "Symbolic links are left where they are and never carried.",
    },
    Word {
        word: "portable",
        also: &[],
        about: "Links that stay inside the root are carried as links; anything else is refused rather than followed.",
    },
    Word {
        word: "raw",
        also: &["posix-raw"],
        about: "Every link is carried exactly as written, including one that points outside the root.",
    },
];

/// What `staging` accepts.
pub const STAGING_MODES: &[Word] = &[
    Word {
        word: "state",
        also: &[],
        about: "Staged content lives under the state root, off the synchronized tree.",
    },
    Word {
        word: "beside-root",
        also: &[],
        about: "Staged content lives next to the root, for a root on a different filesystem from the state.",
    },
    Word {
        word: "inside-root",
        also: &[],
        about: "Staged content lives inside the root itself: the last resort, and it shows up in the tree while it is there.",
    },
];

/// What `durability` accepts.
pub const DURABILITY: &[Word] = &[
    Word {
        word: "process",
        also: &[],
        about: "The journal survives this process dying. The default.",
    },
    Word {
        word: "power",
        also: &[],
        about: "Every record is flushed to the disk, so the journal survives the power going out. Slower.",
    },
];

/// What `log` accepts.
pub const LOG_LEVELS: &[Word] = &[
    Word {
        word: "quiet",
        also: &["error", "errors"],
        about: "Only what went wrong.",
    },
    Word {
        word: "normal",
        also: &["info"],
        about: "What went wrong, and what changed. The default.",
    },
    Word {
        word: "debug",
        also: &["verbose", "trace"],
        about: "Every decision, named file by file. Megabytes a day.",
    },
];

/// The words a configuration key accepts, or `None` where the value is
/// free text. Keyed by the key as it is written in the file, so the
/// editor can ask about a field it only knows the name of.
pub fn vocabulary(key: &str) -> Option<&'static [Word]> {
    match key {
        "symlink_mode" => Some(SYMLINK_MODES),
        "staging" => Some(STAGING_MODES),
        "durability" => Some(DURABILITY),
        "log" => Some(LOG_LEVELS),
        _ => None,
    }
}

/// Reads one word from a table, or says what was expected. The canonical
/// spellings are listed; the older ones are accepted without being
/// advertised.
pub fn parse_word<'a>(words: &'a [Word], what: &str, given: &str) -> Result<&'a Word, String> {
    if let Some(word) = words
        .iter()
        .find(|word| word.word == given || word.also.contains(&given))
    {
        return Ok(word);
    }
    let expected: Vec<&str> = words.iter().map(|word| word.word).collect();
    Err(format!(
        "unknown {what} '{given}' (expected one of: {})",
        expected.join(", ")
    ))
}

/// The order the keys of a section are worth reading in.
///
/// A schema is a map, so a form built straight from one is alphabetical,
/// which puts `acknowledge_secrets` above `alpha` and `disabled` nowhere
/// near the mode it qualifies. This is the reading order; anything not
/// named here follows, alphabetically.
pub const ORDER: &[&str] = &[
    // The top of the file.
    "reload",
    "log",
    "on_alert",
    "disabled_hosts",
    "power_saver_experimental",
    // A group, and the defaults that stand behind one.
    "alpha",
    "betas",
    "mode",
    "disabled",
    "ignores",
    "ignore_files",
    "ignore_mounts",
    "max_file_size",
    "max_entry_count",
    "symlink_mode",
    // The experimental tail of a section, in the order a window that
    // has been let in draws it: the cycle, then how the machinery
    // works, then how created entries land and who owns them.
    "interval",
    "durability",
    "staging",
    "agent_command",
    "acknowledge_secrets",
    "file_mode",
    "directory_mode",
    "default_owner",
    "default_group",
    // The timings.
    "alert_after",
    "after",
    "coalesce_after",
    "settle_after",
    "repeat_after",
    "timeout",
    "ttl",
    "failover_after",
    "allow_root",
];

/// Where a key sits in [`ORDER`], for a form to sort by.
pub fn order_of(key: &str) -> usize {
    ORDER
        .iter()
        .position(|known| *known == key)
        .unwrap_or(ORDER.len())
}

/// The unit a number or a duration is written in, for a form to put
/// beside the box rather than leave a person guessing.
pub fn unit(key: &str) -> Option<&'static str> {
    Some(match key {
        "interval" => "whole seconds",
        "max_entry_count" => "a whole number of entries",
        "max_file_size" => "bytes, or a size: 100MB · 2GiB · 500K",
        "file_mode" | "directory_mode" => "octal permissions: 0600 · 0644 · 0755",
        "ttl" | "timeout" => "a length of time: 30s · 5m · 2h",
        key if key.ends_with("_after") => "a length of time: 30s · 5m · 2h",
        "alpha" => "a path, or user@host:path",
        "betas" | "disabled_hosts" | "ignores" | "ignore_files" => "one to a line",
        "on_alert" | "agent_command" => "a shell command",
        _ => return None,
    })
}

/// What a key means when the file does not say, where that answer is a
/// constant in this program rather than something the schema carries.
///
/// A form that says "not set" and stops has told a person nothing: the
/// question they have is what happens then. Everything here is the very
/// constant the code falls back to, named beside it so the two cannot
/// drift without this table looking wrong.
pub fn fallback(key: &str) -> Option<String> {
    Some(match key {
        "interval" => DEFAULT_INTERVAL_SECONDS.to_string(),
        // `endpoint::local`, which is where created entries get their bits.
        "file_mode" => "0600".to_owned(),
        "directory_mode" => "0700".to_owned(),
        "symlink_mode" => "raw".to_owned(),
        "durability" => "process".to_owned(),
        "staging" => "state".to_owned(),
        // `mode` has no fallback on purpose: a group without one, and
        // with no default behind it, is refused rather than guessed at.
        "max_file_size" | "max_entry_count" => "no limit".to_owned(),
        "alert_after" => format!("{}s", DEFAULT_ALERT_AFTER.as_secs()),
        "coalesce_after" => format!("{}s", DEFAULT_COALESCE_AFTER.as_secs()),
        "settle_after" => format!("{}m", DEFAULT_SETTLE_AFTER.as_secs() / 60),
        "timeout" => format!("{}s", DEFAULT_ALERT_TIMEOUT.as_secs()),
        "repeat_after" => "never".to_owned(),
        "ttl" => format!("{}s", DEFAULT_PEERING_TTL.as_secs()),
        "failover_after" => format!("{}s", DEFAULT_PEERING_FAILOVER_AFTER.as_secs()),
        _ => return None,
    })
}

/// What a value is for, where the type alone does not say: a widget hint
/// for a form, keyed by the key as it is written.
pub fn widget(key: &str) -> Option<&'static str> {
    Some(match key {
        "alpha" => "endpoint",
        "betas" => "endpoints",
        "ignores" | "ignore_files" => "patterns",
        "disabled_hosts" => "hosts",
        "on_alert" => "command",
        "agent_command" => "command",
        "file_mode" | "directory_mode" => "octal",
        "max_file_size" => "size",
        "interval" => "seconds",
        "ttl" | "timeout" => "duration",
        key if key.ends_with("_after") => "duration",
        _ => return None,
    })
}

/// The configuration's own shape, for anything that builds a form from
/// it.
///
/// The fields and their types come from the very structs the parser
/// deserializes into, so a field cannot be in one and not the other. The
/// words a field accepts come from the tables above, which the parsers
/// also read. The widget hints say what a value is for. Nothing here is
/// a second list of anything, which is the whole point.
#[cfg(feature = "schema")]
pub fn schema() -> serde_json::Value {
    let mut document =
        serde_json::to_value(schemars::schema_for!(Config)).expect("a schema is JSON");
    annotate(&mut document);
    document
}

/// Walks the derived schema and writes what the tables know onto it:
/// `enum` for a validator, `x-words` for a form that wants to say what
/// each word means, and `x-widget` for one that wants the right control.
#[cfg(feature = "schema")]
fn annotate(node: &mut serde_json::Value) {
    use serde_json::{json, Value};

    if let Value::Object(map) = node {
        if let Some(Value::Object(properties)) = map.get_mut("properties") {
            for (key, property) in properties.iter_mut() {
                // The third of each triple says the word is experimental:
                // a peering mode is one, and a form that has not been
                // let in does not offer it. The list is here rather than
                // in the form because this is where the words are.
                let words: Option<Vec<(&str, &str, bool)>> = match key.as_str() {
                    "mode" => Some(
                        MODES
                            .iter()
                            .map(|row| (row.name, row.about, row.peering))
                            .collect(),
                    ),
                    key => vocabulary(key).map(|words| {
                        words
                            .iter()
                            .map(|word| (word.word, word.about, false))
                            .collect()
                    }),
                };
                let Value::Object(property) = property else {
                    continue;
                };
                if let Some(words) = words {
                    property.insert(
                        "enum".to_owned(),
                        Value::Array(words.iter().map(|(word, _, _)| json!(word)).collect()),
                    );
                    property.insert(
                        "x-words".to_owned(),
                        Value::Array(
                            words
                                .iter()
                                .map(|(word, about, kept)| {
                                    json!({"word": word, "about": about, "experimental": kept})
                                })
                                .collect(),
                        ),
                    );
                }
                if let Some(widget) = widget(key) {
                    property.insert("x-widget".to_owned(), json!(widget));
                }
                if let Some(unit) = unit(key) {
                    property.insert("x-unit".to_owned(), json!(unit));
                }
                if let Some(fallback) = fallback(key) {
                    property.insert("x-default".to_owned(), json!(fallback));
                }
                property.insert("x-order".to_owned(), json!(order_of(key)));
            }
        }
        for (_, value) in map.iter_mut() {
            annotate(value);
        }
    } else if let Value::Array(items) = node {
        items.iter_mut().for_each(annotate);
    }
}

/// Parses a synchronization mode name to what reconciliation runs.
///
/// A peering spelling parses to the reconciliation mode it wraps; the
/// peering itself is a property of the plan, read by [`parse_mode_spec`].
pub fn parse_mode(mode: &str) -> Result<SyncMode, String> {
    parse_mode_spec(mode).map(|(mode, _)| mode)
}

/// Parses a synchronization mode name: the reconciliation mode, and
/// whether the name asks for peering.
pub fn parse_mode_spec(mode: &str) -> Result<(SyncMode, bool), String> {
    // The names are a grid: direction, then what happens when the two
    // sides disagree about a file — it is reported as a conflict, or alpha
    // wins. The older names (safe, resolved, replica) described the same
    // four modes without exposing that structure; they stay accepted so
    // existing configurations keep working.
    if let Some(row) = MODES
        .iter()
        .find(|row| row.name == mode || row.also.contains(&mode))
    {
        return Ok((row.mode, row.peering));
    }
    // The old spellings are answered, not merely unknown: the rename is
    // the point, and a configuration that used them should be told why
    // rather than quietly carried across.
    if let Some((_, now, why)) = RENAMED_MODES.iter().find(|(was, ..)| *was == mode) {
        return Err(format!("mode '{mode}' was renamed to '{now}': {why}"));
    }
    let expected: Vec<&str> = MODES.iter().map(|row| row.name).collect();
    Err(format!(
        "unknown mode '{mode}' (expected one of: {})",
        expected.join(", ")
    ))
}

/// Returns the canonical name of a synchronization mode.
pub fn mode_name(mode: SyncMode) -> &'static str {
    match MODES
        .iter()
        .find(|row| row.mode == mode && !row.peering)
    {
        Some(row) => row.name,
        // Unreachable while every mode has a row of its own: the table
        // is the list of modes, not a view of it.
        None => "unknown",
    }
}

/// Indicates whether or not an endpoint entry denotes a local path (rather
/// than a remote host): it does when it visibly looks like one — a `/`
/// before any `:`, or a leading `.`, `/`, or `~`.
fn is_local(spec: &str) -> bool {
    if spec.starts_with('.') || spec.starts_with('/') || spec.starts_with('~') {
        return true;
    }
    match (spec.find('/'), spec.find(':')) {
        (Some(_), None) => true,
        (Some(slash), Some(colon)) => slash < colon,
        _ => false,
    }
}

/// Parses one endpoint entry. A remote entry naming no path inherits
/// `inherit_path` when one is given (the beta case), and is an error
/// otherwise (the alpha case, which has nothing to inherit from).
fn parse_endpoint(
    spec: &str,
    inherit_path: Option<&str>,
    agent_command: Option<Vec<String>>,
) -> Result<EndpointTarget, String> {
    if is_local(spec) {
        let path = expand_tilde(spec).map_err(|error| format!("{error:#}"))?;
        return Ok(EndpointTarget::Local(path));
    }
    let (destination, path) = match spec.find(':') {
        Some(colon) => {
            let path = &spec[colon + 1..];
            if path.is_empty() {
                return Err("empty path after ':'".into());
            }
            (&spec[..colon], path.to_owned())
        }
        None => match inherit_path {
            Some(inherited) => (spec, inherited.to_owned()),
            None => return Err("a remote alpha must include a path (host:path)".into()),
        },
    };
    if destination.is_empty() || host_of(destination).is_empty() {
        return Err("empty host".into());
    }
    // A destination beginning with `-` could reach ssh looking like an
    // option; the transport also passes an option terminator, but no such
    // destination is legitimate in the first place.
    if destination.starts_with('-') {
        return Err("host begins with '-'".into());
    }
    Ok(EndpointTarget::Remote {
        destination: destination.to_owned(),
        path,
        agent_command,
    })
}

/// Adds or removes a host in the top-level `disabled_hosts` list, editing
/// the text rather than rewriting the file: a configuration is mostly
/// comments, and a parse-and-serialize round trip would drop every one of
/// them. Returns the new text and whether anything changed.
///
/// The list is created when it is missing, before the first section
/// header — where TOML requires a bare key to be.
pub fn set_host_disabled(text: &str, host: &str, disabled: bool) -> Result<(String, bool)> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .context("unable to parse the configuration for editing")?;
    if !document.contains_key(DISABLED_HOSTS) {
        if !disabled {
            return Ok((text.to_owned(), false));
        }
        document[DISABLED_HOSTS] = toml_edit::value(toml_edit::Array::new());
    }
    let list = document[DISABLED_HOSTS]
        .as_array_mut()
        .ok_or_else(|| anyhow!("{DISABLED_HOSTS} is not a list of hosts"))?;
    let at = list.iter().position(|entry| entry.as_str() == Some(host));
    let changed = match (disabled, at) {
        (true, None) => {
            list.push(host);
            true
        }
        (false, Some(at)) => {
            list.remove(at);
            true
        }
        _ => false,
    };
    Ok((document.to_string(), changed))
}

/// Sets or clears `disabled` on one group, the same way. Turning a group
/// back on removes the key rather than writing `disabled = false`: the
/// absence is the default, and a file full of explicit defaults is a file
/// nobody reads.
pub fn set_group_disabled(text: &str, group: &str, disabled: bool) -> Result<(String, bool)> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .context("unable to parse the configuration for editing")?;
    let table = document
        .get_mut("groups")
        .and_then(|groups| groups.as_table_like_mut())
        .and_then(|groups| groups.get_mut(group))
        .and_then(|group| group.as_table_like_mut())
        .ok_or_else(|| anyhow!("no group named {group:?} in the configuration"))?;
    let was = table
        .get("disabled")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if was == disabled {
        return Ok((text.to_owned(), false));
    }
    match disabled {
        true => {
            table.insert("disabled", toml_edit::value(true));
        }
        false => {
            table.remove("disabled");
        }
    }
    Ok((document.to_string(), true))
}

/// Extracts the host from an SSH destination (`host` or `user@host`).
fn host_of(destination: &str) -> &str {
    match destination.rfind('@') {
        Some(at) => &destination[at + 1..],
        None => destination,
    }
}

#[cfg(test)]
mod tests {

    /// Every word in every table is a word its parser takes, and every
    /// alias means what the row it sits on means. The tables are what
    /// the error messages and the schema are written from, so a row
    /// nobody can parse would be an offer the file cannot accept.
    #[test]
    fn every_word_the_tables_offer_is_a_word_a_parser_takes() {
        for row in MODES {
            assert_eq!(
                parse_mode_spec(row.name).unwrap(),
                (row.mode, row.peering),
                "{}",
                row.name
            );
            for also in row.also {
                assert_eq!(
                    parse_mode_spec(also).unwrap(),
                    (row.mode, row.peering),
                    "{also}"
                );
            }
            assert!(!row.about.is_empty(), "{} says nothing", row.name);
        }
        for (was, now, _) in RENAMED_MODES {
            let complaint = parse_mode_spec(was).unwrap_err();
            assert!(complaint.contains(now), "{complaint}");
            assert!(parse_mode_spec(now).is_ok(), "{now}");
        }
        for word in SYMLINK_MODES {
            assert!(parse_symlink_mode(word.word).is_ok(), "{}", word.word);
            for also in word.also {
                assert_eq!(
                    parse_symlink_mode(also).unwrap(),
                    parse_symlink_mode(word.word).unwrap()
                );
            }
        }
        for word in STAGING_MODES {
            assert!(parse_staging_mode(word.word).is_ok(), "{}", word.word);
        }
        for word in LOG_LEVELS {
            assert_eq!(
                crate::logging::Level::parse(word.word).map(|level| level.name()),
                Some(word.word),
                "{}",
                word.word
            );
            for also in word.also {
                assert_eq!(
                    crate::logging::Level::parse(also).map(|level| level.name()),
                    Some(word.word),
                    "{also}"
                );
            }
        }
        // Durability is read where the plans are built, through the same
        // table reader, so the table is checked against that reader.
        for word in DURABILITY {
            assert_eq!(
                parse_word(DURABILITY, "durability", word.word).unwrap().word,
                word.word
            );
        }
        let complaint = parse_word(DURABILITY, "durability", "sometimes").unwrap_err();
        assert!(complaint.contains("process, power"), "{complaint}");
    }

    /// Prints what the schema says about a few keys, for a person
    /// building a form against it.
    #[cfg(feature = "schema")]
    #[test]
    #[ignore = "a look at the schema, not a check"]
    fn show_the_schema() {
        let document = schema();
        for key in ["reload", "disabled_hosts", "log", "on_alert"] {
            println!("{key}: {}", document["properties"][key]);
        }
        println!("betas: {}", document["$defs"]["Group"]["properties"]["betas"]);
        println!("ignores: {}", document["$defs"]["Group"]["properties"]["ignores"]);
    }


    /// The schema is the structs the parser uses, with the tables
    /// written onto it — not a second description that can fall behind.
    #[cfg(feature = "schema")]
    #[test]
    fn the_schema_describes_the_same_file_the_parser_reads() {
        let document = schema();
        let groups = &document["properties"]["groups"];
        assert!(groups.is_object(), "the schema names the groups map");
        let defaults = &document["$defs"]["Defaults"]["properties"];
        assert!(
            defaults.get("interval").is_some(),
            "a field of the struct is a field of the schema"
        );
        assert_eq!(defaults["interval"]["x-widget"], "seconds");

        // Every word the schema offers anywhere is a word the file takes.
        fn walk(node: &serde_json::Value, found: &mut usize) {
            if let Some(map) = node.as_object() {
                if let Some(words) = map.get("x-words").and_then(|words| words.as_array()) {
                    *found += 1;
                    for word in words {
                        let word = word["word"].as_str().expect("a word is text");
                        assert!(!word.is_empty());
                    }
                }
                for value in map.values() {
                    walk(value, found);
                }
            } else if let Some(items) = node.as_array() {
                items.iter().for_each(|item| walk(item, found));
            }
        }
        let mut found = 0;
        walk(&document, &mut found);
        assert!(found >= 4, "the tables reached the schema: {found}");

        let mode = &document["$defs"]["Group"]["properties"]["mode"];
        let offered: Vec<&str> = mode["enum"]
            .as_array()
            .expect("mode offers words")
            .iter()
            .map(|word| word.as_str().expect("a word is text"))
            .collect();
        assert_eq!(
            offered,
            MODES.iter().map(|row| row.name).collect::<Vec<_>>()
        );
        for word in offered {
            assert!(parse_mode(word).is_ok(), "{word}");
        }
    }
    use super::*;

    /// A group name that starts with `-` reads as an option to every
    /// command that takes one, and to every command autobahn suggests.
    #[test]
    fn a_group_name_starting_with_a_dash_is_refused() {
        let path = std::path::Path::new("config.toml");
        let error = Config::parse(
            path,
            "[groups.\"-rf\"]\nmode = \"two-way-safe\"\nalpha = \"/a\"\nbetas = [\"/b\"]\n",
        )
        .expect_err("a dash-led group name is refused");
        let message = format!("{error:#}");
        assert!(message.contains("\"-rf\""), "{message}");
        assert!(message.contains("cannot start with '-'"), "{message}");

        // A dash elsewhere in the name is fine.
        Config::parse(
            path,
            "[groups.my-group]\nmode = \"two-way-safe\"\nalpha = \"/a\"\nbetas = [\"/b\"]\n",
        )
        .expect("a dash inside a name is accepted");
    }

    /// The template `autobahn init` writes has to satisfy the schema in
    /// this file, and say only true things about it.
    #[test]
    fn the_starting_template_loads_and_describes_nothing() {
        let config = parse(TEMPLATE);
        assert_eq!(config.defaults.mode.as_deref(), Some("two-way-conflict"));
        assert!(config
            .defaults
            .ignores
            .iter()
            .any(|pattern| pattern == ".git"));
        assert_eq!(
            config.plans().expect("the template plans").len(),
            0,
            "the example group is commented out, so a fresh install starts nothing"
        );
    }

    /// The template's comment names every mode, and only real ones.
    ///
    /// A comment is documentation that cannot be compiled, so this compiles
    /// it. The expected list is taken from the parser's own error rather
    /// than written here as a count: a mode added without a line in the
    /// template then fails this test instead of quietly making the file lie,
    /// which is exactly how `two-way-paranoid` was first missed.
    #[test]
    fn the_template_names_every_mode() {
        let is_mode = |word: &&str| word.contains("-way-") || word.starts_with("peering-");
        let named: Vec<&str> = TEMPLATE
            .lines()
            .filter_map(|line| line.strip_prefix("# "))
            .filter_map(|line| line.split_whitespace().next())
            .filter(is_mode)
            .collect();
        let advertised = parse_mode("not-a-mode").expect_err("an unknown mode is refused");
        let canonical: Vec<&str> = advertised
            .split(['(', ')', ':', ',', ' ', '\'', '\n'])
            .filter(is_mode)
            .collect();
        assert_eq!(
            named, canonical,
            "the template must name every mode the parser accepts, in its order"
        );
        for mode in named {
            parse_mode(mode).unwrap_or_else(|error| panic!("{mode}: {error}"));
        }
    }

    /// The pre-rename peering spellings are refused with the new name and
    /// the reason, not as an unknown mode or field.
    #[test]
    fn the_old_peering_names_are_answered_with_the_rename() {
        for old in [
            "peering-conflict-experimental",
            "peering-alpha-experimental",
        ] {
            let error = parse_mode_spec(old).expect_err("the old mode name is refused");
            let new = old.replace("-experimental", "-dangerously-experimental");
            assert!(error.contains(&new), "{error}");
            assert!(error.contains("docs/peering.md"), "{error}");
        }
        let error = format!(
            "{:#}",
            parse(
                r#"
                [advanced.peering-experimental]
                ttl = "30s"

                [groups.g]
                alpha = "/tmp/a"
                betas = ["u@h:/tmp/b"]
                "#,
            )
            .plans()
            .expect_err("the old section name is refused")
        );
        assert!(
            error.contains("peering-dangerously-experimental"),
            "{error}"
        );
    }

    fn parse(text: &str) -> Config {
        toml::from_str(text).expect("configuration should parse")
    }

    /// The power saver is off unless asked for, and is a top-level key.
    #[test]
    fn the_power_saver_is_off_unless_asked_for() {
        assert!(!parse("").power_saver_experimental);
        assert!(parse("power_saver_experimental = true").power_saver_experimental);
        assert!(toml::from_str::<Config>("power_saver = true").is_err());
    }

    /// The whole point of the built-in table: a reader who writes only the
    /// hook gets per-state patience they never had to know about.
    #[test]
    fn one_line_of_configuration_gets_considered_hold_times() {
        use crate::alerts::Alert;
        let config = parse(r#"on_alert = "notify me""#);
        let plan = config.alert_plan().expect("a plan");

        assert_eq!(plan.on_alert.as_deref(), Some("notify me"));
        assert_eq!(plan.after(Alert::Halted), Duration::ZERO, "never transient");
        assert_eq!(plan.after(Alert::Unreachable), Duration::from_secs(300));
        assert_eq!(plan.after(Alert::Errored), Duration::from_secs(120));
        assert_eq!(plan.after(Alert::Conflicts), Duration::from_secs(30));
        assert_eq!(plan.after(Alert::Blocked), Duration::from_secs(30));
        // And the rest of the timing, which nobody should have to write.
        assert_eq!(plan.coalesce_after, DEFAULT_COALESCE_AFTER);
        assert_eq!(plan.settle_after, DEFAULT_SETTLE_AFTER);
        assert_eq!(plan.repeat_after, Duration::ZERO, "a nag is opt-in");
    }

    /// `alert_after` is a statement about all of it. Without this it was
    /// dead on arrival: every state had a built-in entry, so the map
    /// lookup always hit and the global value was never consulted.
    #[test]
    fn a_written_alert_after_replaces_the_built_in_table() {
        use crate::alerts::Alert;
        let config = parse(
            r#"
            on_alert = "notify me"

            [advanced.alerts]
            alert_after = "1s"
            "#,
        );
        let plan = config.alert_plan().expect("a plan");
        for alert in Alert::all() {
            assert_eq!(
                plan.after(alert),
                Duration::from_secs(1),
                "{} should follow the written value",
                alert.name()
            );
        }
    }

    /// The section was called `[advanced]` first. Every configuration on
    /// every machine says that, and none of them was going to be edited
    /// for a word, so the old spelling still loads and still means the
    /// same table. A file that writes both is a file that says one thing
    /// twice, and is refused.
    #[test]
    fn the_name_that_section_had_before_still_reads() {
        use crate::alerts::Alert;
        let config = parse(
            r#"
            on_alert = "notify me"

            [advanced]
            allow_root = true

            [advanced.alerts]
            coalesce_after = "5s"
            "#,
        );
        assert!(config.experimental.allow_root);
        let plan = config.alert_plan().expect("a plan");
        assert_eq!(plan.coalesce_after, Duration::from_secs(5));
        // And the built-in table is still underneath it.
        assert_eq!(plan.after(Alert::Errored), Duration::from_secs(120));

        let both = Config::parse(
            std::path::Path::new("config.toml"),
            "[experimental]\nallow_root = true\n\n[advanced]\nallow_root = false\n",
        )
        .expect_err("a file that writes the section twice is refused");
        assert!(format!("{both:#}").contains("duplicate field"), "{both:#}");
    }

    /// One bad value in `[defaults]` is one fault, however many groups
    /// inherit it — the file used to come back saying the same sentence
    /// once per group with a different name in front.
    #[test]
    fn a_fault_the_defaults_cause_is_reported_once() {
        let error = format!(
            "{:#}",
            parse(
                r#"
                [defaults]
                mode = "two-way-conflict"
                max_file_size = "asdf"

                [groups.a]
                alpha = "/tmp/a"
                betas = ["/tmp/b"]

                [groups.b]
                alpha = "/tmp/c"
                betas = ["/tmp/d"]

                [groups.c]
                alpha = "/tmp/e"
                betas = ["/tmp/f"]
                "#,
            )
            .plans()
            .expect_err("a bad size is refused")
        );
        let said = error.matches("invalid size").count();
        assert_eq!(said, 1, "{error}");
        // And the key it is about is named, so a form can put it where
        // the value was typed.
        assert!(error.contains("max_file_size: invalid size"), "{error}");
    }

    /// A fault a group wrote itself keeps its group; only the copies
    /// an inherited value makes are identical, and those collapse.
    #[test]
    fn a_groups_own_fault_keeps_its_group() {
        let folded = fold(vec![
            "the defaults' mode: unknown mode 'sideways'".to_owned(),
            "the defaults' mode: unknown mode 'sideways'".to_owned(),
            "group 'c': mode: unknown mode 'elsewhere'".to_owned(),
        ]);
        assert_eq!(folded.len(), 2, "{folded:?}");
        assert_eq!(folded[0], "the defaults' mode: unknown mode 'sideways'");
        assert_eq!(folded[1], "group 'c': mode: unknown mode 'elsewhere'");
    }

    /// An inherited value is the defaults' to answer for, whichever
    /// group happened to read it.
    #[test]
    fn an_inherited_fault_names_the_defaults_not_the_group() {
        let error = format!(
            "{:#}",
            parse(
                r#"
                [defaults]
                mode = "two-way-conflict"
                symlink_mode = "sideways"

                [groups.a]
                alpha = "/tmp/a"
                betas = ["/tmp/b"]

                [groups.b]
                alpha = "/tmp/c"
                betas = ["/tmp/d"]
                "#,
            )
            .plans()
            .expect_err("a bad symlink mode is refused")
        );
        assert!(error.contains("the defaults' symlink_mode:"), "{error}");
        assert!(!error.contains("group 'a'"), "{error}");
        assert_eq!(error.matches("unknown symlink mode").count(), 1, "{error}");

        // And a group that writes its own bad value answers for it.
        let its_own = format!(
            "{:#}",
            parse(
                r#"
                [defaults]
                mode = "two-way-conflict"

                [groups.a]
                alpha = "/tmp/a"
                betas = ["/tmp/b"]
                symlink_mode = "sideways"
                "#,
            )
            .plans()
            .expect_err("a bad symlink mode is refused")
        );
        assert!(its_own.contains("group 'a': symlink_mode:"), "{its_own}");
    }

    #[test]
    fn the_experimental_section_overrides_the_built_in_table() {
        use crate::alerts::Alert;
        let config = parse(
            r#"
            on_alert = "notify me"

            [experimental.alerts]
            coalesce_after = "5s"

            [experimental.alerts.after]
            unreachable = "1m"
            "#,
        );
        let plan = config.alert_plan().expect("a plan");
        assert_eq!(plan.after(Alert::Unreachable), Duration::from_secs(60));
        assert_eq!(plan.coalesce_after, Duration::from_secs(5));
        // Untouched states keep their built-in value rather than falling
        // back to a single global one.
        assert_eq!(plan.after(Alert::Errored), Duration::from_secs(120));
    }

    /// A configuration written against the old shape is answered, not
    /// merely rejected.
    #[test]
    fn the_old_alerts_section_says_where_everything_went() {
        let config = parse(
            r#"
            [alerts]
            on_alert = "notify me"
            alert_after = "30s"
            "#,
        );
        let error = format!("{:#}", config.alert_plan().expect_err("retired"));
        assert!(error.contains("[alerts] has moved"), "{error}");
        assert!(error.contains("top level"), "{error}");
        assert!(error.contains("[advanced.alerts]"), "{error}");
    }

    #[test]
    fn an_unknown_state_is_refused_with_the_known_ones() {
        let config = parse(
            r#"
            on_alert = "notify me"

            [advanced.alerts.after]
            exploded = "1m"
            "#,
        );
        let error = format!("{:#}", config.alert_plan().expect_err("unknown state"));
        assert!(error.contains("exploded"), "{error}");
        assert!(error.contains("unreachable"), "{error}");
    }

    #[test]
    fn a_remote_alpha_fans_out_and_shares_its_path_with_bare_betas() {
        let config = parse(
            r#"
            [groups.pull]
            alpha = "build.example.com:/srv/artifacts"
            mode = "one-way-safe"
            betas = ["/data/artifacts", "mirror.example.com"]
            "#,
        );
        let plans = config.plans().expect("plans should derive");
        assert_eq!(plans.len(), 2);
        assert_eq!(
            plans[0].alpha,
            EndpointTarget::Remote {
                destination: "build.example.com".into(),
                path: "/srv/artifacts".into(),
                agent_command: None,
            }
        );
        // A bare remote beta inherits the remote alpha's *path*.
        assert_eq!(
            plans[1].beta,
            EndpointTarget::Remote {
                destination: "mirror.example.com".into(),
                path: "/srv/artifacts".into(),
                agent_command: None,
            }
        );
    }

    #[test]
    fn a_remote_alpha_requires_an_explicit_path() {
        let config = parse(
            r#"
            [groups.pull]
            alpha = "build.example.com"
            mode = "two-way-safe"
            betas = ["/data"]
            "#,
        );
        let error = format!("{:#}", config.plans().expect_err("plans must fail"));
        assert!(error.contains("must include a path"), "{error}");
    }

    #[test]
    fn a_disabled_alpha_host_drops_the_whole_group() {
        let config = parse(
            r#"
            disabled_hosts = ["build.example.com"]

            [groups.pull]
            alpha = "build.example.com:/srv/artifacts"
            mode = "two-way-safe"
            betas = ["/data/artifacts", "mirror.example.com:/srv/artifacts"]
            "#,
        );
        assert!(config.plans().expect("plans should derive").is_empty());
    }

    #[test]
    fn limits_staging_and_ownership_resolve_with_group_precedence() {
        let config = parse(
            r#"
            [defaults]
            mode = "two-way-safe"
            max_file_size = "100MB"
            max_entry_count = 500000
            staging = "state"
            default_owner = "www-data"

            [groups.data]
            alpha = "/data"
            betas = ["host.example.com:/data"]
            max_file_size = "2GiB"
            staging = "beside-root"
            default_group = "id:33"
            "#,
        );
        let plans = config.plans().expect("plans should derive");
        assert_eq!(plans[0].max_file_size, Some(2 << 30));
        assert_eq!(plans[0].max_entry_count, Some(500_000));
        assert_eq!(plans[0].staging, StagingMode::BesideRoot);
        assert_eq!(plans[0].default_owner.as_deref(), Some("www-data"));
        assert_eq!(plans[0].default_group.as_deref(), Some("id:33"));
    }

    #[test]
    fn size_specifications_parse_in_both_notations() {
        assert_eq!(parse_size(&SizeSpec::Bytes(1234)).unwrap(), 1234);
        assert_eq!(
            parse_size(&SizeSpec::Text("100MB".into())).unwrap(),
            100_000_000
        );
        assert_eq!(parse_size(&SizeSpec::Text("2GiB".into())).unwrap(), 2 << 30);
        assert_eq!(
            parse_size(&SizeSpec::Text("512K".into())).unwrap(),
            512 << 10
        );
        assert_eq!(parse_size(&SizeSpec::Text("64".into())).unwrap(), 64);
        assert!(parse_size(&SizeSpec::Text("10 furlongs".into())).is_err());
        assert!(parse_size(&SizeSpec::Text("".into())).is_err());
    }

    #[test]
    fn staging_placements_parse_by_name() {
        assert_eq!(parse_staging_mode("state").unwrap(), StagingMode::State);
        assert_eq!(
            parse_staging_mode("beside-root").unwrap(),
            StagingMode::BesideRoot
        );
        assert_eq!(
            parse_staging_mode("inside-root").unwrap(),
            StagingMode::InsideRoot
        );
        assert!(parse_staging_mode("neighboring").is_err());
    }

    #[test]
    fn a_full_configuration_produces_the_expected_plans() {
        let config = parse(
            r#"
            disabled_hosts = ["down.example.com"]

            [defaults]
            mode = "two-way-safe"
            ignores = [".git"]
            interval = 30

            [groups.project]
            alpha = "~/project"
            ignores = ["*.tmp"]
            betas = [
                "build.example.com",
                "user@lab.example.com:/srv/project",
                "down.example.com",
            ]

            [groups.backup]
            alpha = "/data"
            mode = "one-way-replica"
            interval = 300
            betas = ["/mnt/backup/data"]
            "#,
        );
        let plans = config.plans().expect("plans should derive");
        assert_eq!(plans.len(), 3);

        // Groups iterate in name order (backup before project).
        assert_eq!(plans[0].display(), "backup@/mnt/backup/data");
        assert_eq!(plans[0].mode, SyncMode::OneWayReplica);
        assert_eq!(plans[0].interval, Duration::from_secs(300));
        assert_eq!(
            plans[0].beta,
            EndpointTarget::Local(PathBuf::from("/mnt/backup/data"))
        );
        // The defaults' ignores apply even where the group adds none.
        assert_eq!(plans[0].ignores, vec![".git".to_owned()]);

        assert_eq!(plans[1].display(), "project@build.example.com");
        assert_eq!(plans[1].mode, SyncMode::TwoWaySafe);
        assert_eq!(plans[1].interval, Duration::from_secs(30));
        assert_eq!(
            plans[1].ignores,
            vec![".git".to_owned(), "*.tmp".to_owned()]
        );
        // A remote beta without a path inherits the alpha as written, so it
        // resolves against the remote home.
        assert_eq!(
            plans[1].beta,
            EndpointTarget::Remote {
                destination: "build.example.com".into(),
                path: "~/project".into(),
                agent_command: None,
            }
        );

        assert_eq!(plans[2].display(), "project@lab.example.com");
        assert_eq!(
            plans[2].beta,
            EndpointTarget::Remote {
                destination: "user@lab.example.com".into(),
                path: "/srv/project".into(),
                agent_command: None,
            }
        );

        // The disabled host appears in no plan.
        assert!(plans.iter().all(|plan| plan.host != "down.example.com"));
    }

    #[test]
    fn beta_entries_are_classified_as_local_or_remote() {
        let local = |beta: &str| {
            matches!(
                parse_endpoint(beta, Some("~/x"), None).expect("should parse"),
                EndpointTarget::Local(_)
            )
        };
        assert!(local("/absolute/path"));
        assert!(local("./relative"));
        assert!(local("~/home/relative"));
        assert!(local("some/relative/path"));
        assert!(!local("host"));
        assert!(!local("host.example.com"));
        assert!(!local("user@host"));
        assert!(!local("host:/path"));
        assert!(!local("user@host:path/with/slashes"));
    }

    #[test]
    fn malformed_beta_entries_are_rejected() {
        assert!(parse_endpoint("host:", Some("~/x"), None).is_err());
        assert!(parse_endpoint(":path", Some("~/x"), None).is_err());
        assert!(parse_endpoint("user@:path", Some("~/x"), None).is_err());
        // A destination that could read as an SSH option is never a host.
        assert!(parse_endpoint("-oProxyCommand=evil:path", Some("~/x"), None).is_err());
        assert!(parse_endpoint("-host", Some("~/x"), None).is_err());
    }

    #[test]
    fn session_identity_is_stable_and_distinct() {
        let config = parse(
            r#"
            [groups.a]
            alpha = "~/x"
            mode = "two-way-safe"
            betas = ["host1", "host2"]
            "#,
        );
        let plans = config.plans().expect("plans should derive");
        assert_eq!(plans[0].identifier(), plans[0].identifier());
        assert_ne!(plans[0].identifier(), plans[1].identifier());
    }

    #[test]
    fn every_configuration_problem_is_reported_at_once() {
        let config = parse(
            r#"
            [groups.first]
            alpha = "~/x"
            mode = "sideways"
            betas = []

            [groups.second]
            alpha = ""
            betas = ["host"]
            "#,
        );
        let error = format!("{:#}", config.plans().expect_err("plans should fail"));
        assert!(error.contains("unknown mode 'sideways'"), "{error}");
        assert!(error.contains("expected one of"), "{error}");
        assert!(error.contains("'first': betas: a group needs at least one"), "{error}");
        assert!(error.contains("'second': alpha: cannot be empty"), "{error}");
        assert!(
            error.contains("'second': mode: none here, and the defaults specify none"),
            "{error}"
        );
    }

    #[test]
    fn unknown_keys_are_rejected_with_suggestions() {
        // A misspelled group key ('beta' for 'betas') fails to parse, and
        // serde's diagnostic names the valid fields.
        let error = toml::from_str::<Config>(
            r#"
            [groups.x]
            alpha = "~/x"
            mode = "two-way-safe"
            beta = ["host"]
            "#,
        )
        .expect_err("parsing should fail")
        .to_string();
        assert!(error.contains("beta"), "{error}");
        assert!(error.contains("betas"), "{error}");

        // Unknown top-level keys fail as well.
        assert!(toml::from_str::<Config>("disable = [\"host\"]").is_err());
    }

    /// The examples are run by `sh`, so they have to parse as `sh`. A
    /// broken one would only be discovered by the alert it failed to
    /// deliver.
    #[test]
    fn the_example_scripts_are_valid_shell() {
        for (name, contents) in [
            ("on-alert.sh", ON_ALERT_EXAMPLE),
            ("open-status", OPEN_STATUS_EXAMPLE),
        ] {
            let directory = tempfile::tempdir().expect("a temporary directory");
            let script = directory.path().join(name);
            std::fs::write(&script, contents).expect("the script should be writable");
            let checked = std::process::Command::new("sh")
                .arg("-n")
                .arg(&script)
                .output()
                .expect("sh should run");
            assert!(
                checked.status.success(),
                "{name}: {}",
                String::from_utf8_lossy(&checked.stderr)
            );
        }
    }

    /// `disable` edits the file people wrote, so what they wrote has to
    /// survive it: the comments, the key order, and everything the edit
    /// did not name.
    #[test]
    fn disabling_a_host_keeps_the_rest_of_the_file() {
        let original = r#"# why this file looks like this
disabled_hosts = ["soros"]

[defaults]
mode = "two-way-conflict"

# the work group
[groups.work]
alpha = "/tmp/alpha"        # the source
betas = ["build.example.com:/tmp/beta"]
"#;
        let (text, changed) =
            set_host_disabled(original, "build.example.com", true).expect("the edit applies");
        assert!(changed);
        assert!(text.contains("# why this file looks like this"));
        assert!(text.contains("# the work group"));
        assert!(text.contains("# the source"));
        assert!(text.contains(r#"["soros", "build.example.com"]"#), "{text}");

        // Enabling it again leaves the file as it was found.
        let (back, changed) =
            set_host_disabled(&text, "build.example.com", false).expect("the edit applies");
        assert!(changed);
        assert_eq!(back, original);

        // And a second enable is not an error, it is a no-op.
        let (again, changed) =
            set_host_disabled(&back, "build.example.com", false).expect("the edit applies");
        assert!(!changed);
        assert_eq!(again, back);
    }

    /// A file that never named the key gets it, before the first section
    /// header — the only place TOML allows a bare key.
    #[test]
    fn disabling_a_host_writes_the_list_when_it_is_missing() {
        let original = "[groups.work]\nalpha = \"/tmp/alpha\"\nbetas = [\"host:/tmp/beta\"]\n";
        let (text, changed) = set_host_disabled(original, "host", true).expect("the edit applies");
        assert!(changed);
        let parsed = parse(&text);
        assert_eq!(parsed.disabled_hosts, vec!["host".to_owned()]);
        assert!(
            text.find("disabled_hosts").unwrap() < text.find("[groups.work]").unwrap(),
            "{text}"
        );
    }

    /// Turning a group back on removes the key rather than writing the
    /// default out: a file full of explicit defaults is a file nobody
    /// reads.
    #[test]
    fn enabling_a_group_removes_the_key_it_added() {
        let original = "[defaults]\nmode = \"two-way-conflict\"\n\n[groups.work]\nalpha = \"/tmp/alpha\"\nbetas = [\"host:/tmp/beta\"]\n";
        let (off, changed) = set_group_disabled(original, "work", true).expect("the edit applies");
        assert!(changed);
        assert!(off.contains("disabled = true"), "{off}");
        assert!(parse(&off).plans().expect("it still loads").is_empty());

        let (on, changed) = set_group_disabled(&off, "work", false).expect("the edit applies");
        assert!(changed);
        assert!(!on.contains("disabled"), "{on}");
        assert_eq!(on, original);
    }

    /// A group that does not exist is a typo, and a typo is refused rather
    /// than written into the file as a line that does nothing.
    #[test]
    fn disabling_an_unknown_group_is_refused() {
        let original = "[groups.work]\nalpha = \"/tmp/alpha\"\nbetas = [\"host:/tmp/beta\"]\n";
        assert!(set_group_disabled(original, "nope", true).is_err());
    }

    /// The names `disable --host` accepts: every host the file mentions,
    /// and no local path.
    #[test]
    fn known_hosts_are_the_ones_the_groups_name() {
        let config = parse(
            r#"
            [defaults]
            mode = "two-way-conflict"

            [groups.work]
            alpha = "/tmp/alpha"
            betas = ["build.example.com:/tmp/beta", "/mnt/backup", "user@lab.example.com"]

            [groups.remote]
            alpha = "ubuntu@lead.example.com:/srv/tree"
            betas = ["build.example.com:/srv/tree"]
            "#,
        );
        assert_eq!(
            config.known_hosts(),
            vec![
                "lead.example.com".to_owned(),
                "build.example.com".to_owned(),
                "user@lab.example.com"
                    .rsplit('@')
                    .next()
                    .unwrap()
                    .to_owned(),
            ]
        );
        assert_eq!(
            config.groups_led_by("lead.example.com"),
            vec!["remote".to_owned()]
        );
        assert!(config.groups_led_by("build.example.com").is_empty());
    }

    /// The old spelling parses, so that it can be answered with what to
    /// write instead rather than with "unknown field `disabled`".
    #[test]
    fn the_old_top_level_disabled_says_what_to_write_instead() {
        let error = parse(
            r#"
            disabled = ["down.example.com"]

            [defaults]
            mode = "two-way-conflict"

            [groups.one]
            alpha = "/tmp/alpha"
            betas = ["host:/tmp/beta"]
            "#,
        )
        .plans()
        .expect_err("the retired key should be refused")
        .to_string();
        assert!(error.contains("disabled_hosts"), "{error}");
    }

    /// A group that is off contributes no sessions, and is not held to the
    /// rules its settings would otherwise have to pass.
    #[test]
    fn a_disabled_group_plans_nothing() {
        let config = parse(
            r#"
            [defaults]
            mode = "two-way-conflict"

            [groups.off]
            disabled = true
            alpha = "/tmp/alpha"
            betas = ["gone.example.com:/tmp/beta"]

            [groups.on]
            alpha = "/tmp/other"
            betas = ["host.example.com:/tmp/beta"]
            "#,
        );
        let plans = config.plans().expect("the configuration should be valid");
        let groups: Vec<&str> = plans.iter().map(|plan| plan.group.as_str()).collect();
        assert_eq!(groups, vec!["on"]);
    }

    /// Off is off even when the group could not have been planned at all:
    /// silencing a broken group is the point.
    #[test]
    fn a_disabled_group_is_not_validated() {
        let config = parse(
            r#"
            [defaults]
            mode = "two-way-conflict"

            [groups.off]
            disabled = true
            alpha = "relative/path"
            betas = []
            "#,
        );
        assert!(config.plans().expect("no errors").is_empty());
    }

    #[test]
    fn defaults_are_optional() {
        let config = parse(
            r#"
            [groups.x]
            alpha = "/a"
            mode = "two-way-safe"
            betas = ["host"]
            "#,
        );
        let plans = config.plans().expect("plans should derive");
        assert_eq!(
            plans[0].interval,
            Duration::from_secs(DEFAULT_INTERVAL_SECONDS)
        );
        assert!(plans[0].ignores.is_empty());
    }

    #[test]
    fn agent_command_applies_to_remote_betas() {
        let config = parse(
            r#"
            [groups.x]
            alpha = "/a"
            mode = "two-way-safe"
            agent_command = "custom-agent --flag"
            betas = ["host", "/local"]
            "#,
        );
        let plans = config.plans().expect("plans should derive");
        assert_eq!(
            plans[0].beta,
            EndpointTarget::Remote {
                destination: "host".into(),
                path: "/a".into(),
                agent_command: Some(vec!["custom-agent".into(), "--flag".into()]),
            }
        );
        // Local betas never involve an agent.
        assert_eq!(
            plans[1].beta,
            EndpointTarget::Local(PathBuf::from("/local"))
        );
    }

    #[test]
    fn overlapping_roots_within_a_session_are_rejected() {
        // Reproduced before it was fixed: with beta containing alpha, replica
        // mode read its own output as beta-side divergence and recursively
        // deleted the alpha root through the beta path.
        for (alpha, beta, how) in [
            ("/srv/tree/project", "/srv/tree", "containing the alpha"),
            ("/srv/tree", "/srv/tree/project", "inside the alpha"),
            ("/srv/tree", "/srv/tree", "the same tree"),
            // Remote pairs on one destination are comparable textually.
            (
                "host:/srv/tree/project",
                "host:/srv/tree",
                "containing the alpha",
            ),
        ] {
            let config = parse(&format!(
                r#"
                [groups.bad]
                alpha = "{alpha}"
                mode = "one-way-replica"
                betas = ["{beta}"]
                "#
            ));
            let error = format!("{:#}", config.plans().expect_err("plans should fail"));
            assert!(error.contains(how), "{alpha} vs {beta}: {error}");
        }

        // A prefix that is not a component boundary is a different tree.
        let config = parse(
            r#"
            [groups.fine]
            alpha = "/srv/tree"
            mode = "two-way-safe"
            betas = ["/srv/tree-backup"]
            "#,
        );
        assert_eq!(config.plans().expect("plans should build").len(), 1);

        // Relays — one session's beta feeding another's alpha — stay legal:
        // only overlap within a single session is self-referential.
        let config = parse(
            r#"
            [groups.first]
            alpha = "/srv/a"
            mode = "one-way-safe"
            betas = ["/srv/hub"]

            [groups.second]
            alpha = "/srv/hub"
            mode = "one-way-safe"
            betas = ["/srv/final"]
            "#,
        );
        assert_eq!(config.plans().expect("plans should build").len(), 2);
    }

    /// Containment compares path components, so a root of `/` contains
    /// everything, a trailing separator never matters, and a name that
    /// merely begins with another is a different tree. Remote paths are
    /// compared only within one destination.
    #[test]
    fn containment_is_decided_by_components() {
        let local = |path: &str| EndpointTarget::Local(PathBuf::from(path));
        let remote = |destination: &str, path: &str| EndpointTarget::Remote {
            destination: destination.to_owned(),
            path: path.to_owned(),
            agent_command: None,
        };
        let check = |alpha: &EndpointTarget, beta: &EndpointTarget| {
            let identity = |target: &EndpointTarget| match target {
                EndpointTarget::Local(path) => path.to_string_lossy().into_owned(),
                EndpointTarget::Remote {
                    destination, path, ..
                } => format!("{destination}:{path}"),
            };
            check_session_topology(alpha, beta, &identity(alpha), &identity(beta))
        };
        for (alpha, beta, how) in [
            (local("/"), local("/srv/project"), "inside the alpha"),
            (local("/srv/"), local("/srv/project"), "inside the alpha"),
            (
                local("/srv/project"),
                local("/srv/"),
                "containing the alpha",
            ),
            (
                local("/srv/project/"),
                local("/srv/project"),
                "the same tree",
            ),
            (
                remote("host", "/tree/"),
                remote("host", "/tree/nested"),
                "inside the alpha",
            ),
            (
                remote("host", "/tree/./nested//"),
                remote("host", "/tree/nested"),
                "the same tree",
            ),
            (
                remote("host", "/"),
                remote("host", "/tree"),
                "inside the alpha",
            ),
        ] {
            let problem = check(&alpha, &beta).expect_err("an overlap");
            assert!(problem.contains(how), "{alpha:?} vs {beta:?}: {problem}");
        }
        for (alpha, beta) in [
            (local("/srv/pro"), local("/srv/project")),
            (local("/srv/project"), local("/srv/pro")),
            (remote("host", "/tree"), remote("other", "/tree/nested")),
            // A home-relative remote path is not the absolute one.
            (remote("host", "tree"), remote("host", "/tree/nested")),
            // Nor is a remote root comparable with a local one.
            (local("/tree"), remote("host", "/tree/nested")),
            (remote("host", "/tree/.."), remote("host", "/other")),
        ] {
            assert!(check(&alpha, &beta).is_ok(), "{alpha:?} vs {beta:?}");
        }
    }

    /// The same component rule holds across sessions: a writable endpoint
    /// under a root of `/` is nested inside it.
    #[test]
    fn a_root_of_slash_contains_every_other_session() {
        let config = parse(
            r#"
            [groups.everything]
            alpha = "/"
            mode = "one-way-safe"
            betas = ["host:/backup"]

            [groups.project]
            alpha = "/srv/project"
            mode = "two-way-safe"
            betas = ["/laptop/project"]
            "#,
        );
        let error = format!("{:#}", config.plans().expect_err("plans should fail"));
        assert!(error.contains("/srv/project is nested inside /"), "{error}");
    }

    /// A local root may not be, or hold, autobahn's own directories,
    /// unless its ignores keep them out — and a negation reaching back
    /// inside what is ignored puts them back in.
    #[test]
    fn a_root_holding_autobahns_own_state_is_refused_unless_ignored() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let base = resolve_for_identity(keep.path());
        let own = OwnState::new(&base.join("home/.autobahn"), None);
        let check = |root: &Path, ignores: &[&str]| {
            let ignores: Vec<String> = ignores.iter().map(|&p| p.to_owned()).collect();
            let target = EndpointTarget::Local(root.to_owned());
            own.check(&target, &root.to_string_lossy(), &ignores)
        };
        let home = base.join("home");
        let problem = check(&home, &[]).expect_err("the home holds the state root");
        assert!(
            problem.contains("contains autobahn's own state"),
            "{problem}"
        );
        assert!(check(&home, &[".autobahn"]).is_ok());
        assert!(check(&home, &[".autobahn/"]).is_ok());
        assert!(
            check(&base, &["home"]).is_ok(),
            "an ignored parent keeps it out"
        );
        assert!(
            check(&home, &[".autobahn", "!.autobahn/config.toml"]).is_err(),
            "a re-inclusion inside the state root puts it back"
        );
        let problem = check(&home.join(".autobahn"), &[]).expect_err("the state root itself");
        assert!(problem.contains("is autobahn's own state"), "{problem}");
        assert!(check(&home.join("project"), &[]).is_ok(), "a narrower root");
        assert!(check(&base.join("hom"), &[]).is_ok());

        // A remote root is the agent's to guard.
        let remote = EndpointTarget::Remote {
            destination: "host".to_owned(),
            path: home.to_string_lossy().into_owned(),
            agent_command: None,
        };
        assert!(own.check(&remote, "host:/", &[]).is_ok());

        // The configuration's own directory, when it is not the state root.
        let own = OwnState::new(
            &base.join("state"),
            Some(&base.join("dotfiles/config.toml")),
        );
        let target = EndpointTarget::Local(base.clone());
        let problem = own
            .check(&target, &base.to_string_lossy(), &["state".to_owned()])
            .expect_err("the configuration's directory");
        assert!(problem.contains("autobahn's configuration"), "{problem}");
    }

    /// A root holding credentials is warned about, once, naming them —
    /// unless they are ignored or the group says it means it.
    #[test]
    fn a_root_holding_credentials_is_warned_about_unless_ignored_or_acknowledged() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        std::fs::create_dir_all(root.join(".ssh")).unwrap();
        std::fs::create_dir_all(root.join(".config/gcloud")).unwrap();
        std::fs::create_dir_all(root.join(".config/other")).unwrap();
        let warnings = |extra: &str| {
            let config = parse(&format!(
                r#"
                [groups.dots]
                alpha = "{root}"
                mode = "two-way-safe"
                betas = ["host:/dots", "other:/dots"]
                {extra}
                "#,
                root = root.display()
            ));
            secret_warnings(&config.plans().expect("plans should build"))
        };

        let found = warnings("");
        assert_eq!(found.len(), 1, "once per root, not per session: {found:?}");
        assert!(found[0].contains(".ssh, .config/gcloud"), "{found:?}");
        assert!(found[0].contains("acknowledge_secrets = true"), "{found:?}");
        assert!(found[0].contains("ignores"), "{found:?}");

        let found = warnings(r#"ignores = [".ssh"]"#);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(!found[0].contains(".ssh"), "{found:?}");
        assert!(warnings(r#"ignores = [".ssh", ".config/gcloud"]"#).is_empty());
        assert!(warnings(r#"ignores = [".ssh", ".config"]"#).is_empty());
        assert!(warnings("acknowledge_secrets = true").is_empty());
    }

    /// A configuration's warnings are one list, gathered once per load:
    /// planning it again says nothing more, and each warning is in the
    /// list once.
    #[test]
    fn a_configurations_warnings_are_one_list_gathered_once() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let root = keep.path().join("root");
        std::fs::create_dir_all(root.join(".ssh")).unwrap();
        let config = parse(&format!(
            r#"
            [groups.dots]
            alpha = "{root}"
            mode = "two-way-safe"
            betas = ["host:/dots", "other:/dots"]
            ignores = ["vendor", "!vendor/*.patch"]
            "#,
            root = root.display()
        ));
        config.plans().expect("plans should build");
        config.plans().expect("plans should build again");
        let warnings = config.warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("group 'dots': !vendor/*.patch has no effect"));
        assert!(warnings[1].contains("group 'dots'") && warnings[1].contains(".ssh"));
        // Gathered once: the same list, not a second pass.
        assert!(std::ptr::eq(warnings, config.warnings()));
    }

    /// A home directory is warned about for every credential directory it
    /// could hold, present yet or not, that is not ignored.
    #[test]
    fn a_home_root_is_warned_about_for_every_unignored_credential_directory() {
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let home = resolve_for_identity(keep.path());
        let identity = home.to_string_lossy();
        let found = secrets_warning_in(&identity, &[], Some(&home)).expect("a warning");
        assert!(found.contains("home directory"), "{found}");
        assert!(
            found.contains(".ssh, .aws, .gnupg, .config/gcloud, .kube"),
            "{found}"
        );
        let ignores: Vec<String> = [".ssh", ".aws", ".gnupg", ".config/gcloud", ".kube"]
            .map(str::to_owned)
            .to_vec();
        assert!(secrets_warning_in(&identity, &ignores, Some(&home)).is_none());
        let elsewhere = home.join("project");
        assert!(secrets_warning_in(&elsewhere.to_string_lossy(), &[], Some(&home)).is_none());
    }

    /// A nested endpoint the outer session ignores is not shared with it:
    /// the outer never scans, writes, or records anything beneath that
    /// path, so the two sessions do not actually overlap. Refusing these
    /// made "synchronize this project, and ship its build output
    /// elsewhere" impossible to express at all.
    #[test]
    fn an_ignored_nested_endpoint_is_not_an_overlap() {
        let config = parse(
            r#"
            [groups.project]
            alpha = "/srv/project"
            mode = "two-way-safe"
            ignores = ["dist"]
            betas = ["/backup/project"]

            [groups.dist]
            alpha = "/srv/project/dist"
            mode = "one-way-replica"
            betas = ["/web/dist"]
            "#,
        );
        let plans = config
            .plans()
            .expect("the ignored nesting is not an overlap");
        assert_eq!(plans.len(), 2);

        // Without the ignore the same pair is refused, and the message
        // names the containment in the right direction and the remedy.
        let config = parse(
            r#"
            [groups.project]
            alpha = "/srv/project"
            mode = "two-way-safe"
            betas = ["/backup/project"]

            [groups.dist]
            alpha = "/srv/project/dist"
            mode = "one-way-replica"
            betas = ["/web/dist"]
            "#,
        );
        let error = format!("{:#}", config.plans().expect_err("plans should fail"));
        assert!(
            error.contains("/srv/project/dist is nested inside /srv/project"),
            "{error}"
        );
        assert!(error.contains("ignores"), "{error}");
    }

    #[test]
    fn nested_writable_endpoints_across_sessions_are_rejected() {
        // Two sessions writing one tree region from independent ancestors
        // can each read the other's writes as user edits; nesting is
        // refused when either endpoint is written.
        let config = parse(
            r#"
            [groups.whole]
            alpha = "/srv/project"
            mode = "two-way-safe"
            betas = ["/backup/project"]

            [groups.part]
            alpha = "/srv/project/docs"
            mode = "two-way-safe"
            betas = ["/laptop/docs"]
            "#,
        );
        let error = format!("{:#}", config.plans().expect_err("plans should fail"));
        assert!(error.contains("nested inside"), "{error}");

        // Read-only nesting — two one-way sessions reading overlapping
        // sources — stays legal: nothing writes the shared region.
        let config = parse(
            r#"
            [groups.whole]
            alpha = "/srv/project"
            mode = "one-way-safe"
            betas = ["/backup/project"]

            [groups.part]
            alpha = "/srv/project/docs"
            mode = "one-way-safe"
            betas = ["/laptop/docs"]
            "#,
        );
        assert_eq!(config.plans().expect("plans should build").len(), 2);

        // Equal shared endpoints (fan-out, star, relay) stay legal too —
        // they warn rather than fail.
        let config = parse(
            r#"
            [groups.star-one]
            alpha = "/hub"
            mode = "two-way-safe"
            betas = ["/spoke-one"]

            [groups.star-two]
            alpha = "/hub"
            mode = "two-way-safe"
            betas = ["/spoke-two"]
            "#,
        );
        assert_eq!(config.plans().expect("plans should build").len(), 2);
    }

    /// Two betas of one group on one host are two sessions, with two
    /// ancestors, even though both are `group@host`: one written inside
    /// the other is refused like any other cross-session nesting. The
    /// check once told sessions apart by that label and skipped the pair.
    #[test]
    fn nested_betas_on_one_host_in_one_group_are_rejected() {
        // Every beta is written, whatever the mode, so the one-way form
        // follows the same rule.
        for mode in ["two-way-safe", "one-way-replica"] {
            let config = parse(&format!(
                r#"
                [groups.tree]
                alpha = "/srv/tree"
                mode = "{mode}"
                betas = ["host:/tree", "host:/tree/nested"]
                "#
            ));
            let error = format!("{:#}", config.plans().expect_err("plans should fail"));
            assert!(
                error.contains("host:/tree/nested is nested inside host:/tree"),
                "{mode}: {error}"
            );
        }
    }

    /// Two betas of one group on one host are told apart by their paths
    /// in every label; a beta alone on its host keeps the short label.
    #[test]
    fn betas_sharing_a_host_are_labelled_with_their_paths() {
        let config = parse(
            r#"
            [groups.tree]
            alpha = "/srv/tree"
            mode = "two-way-safe"
            betas = ["host:/tree", "host:/other", "elsewhere:/tree", "/local/tree"]
            "#,
        );
        let labels: Vec<String> = config
            .plans()
            .expect("plans should build")
            .iter()
            .map(SessionPlan::display)
            .collect();
        assert_eq!(
            labels,
            [
                "tree@host:/tree",
                "tree@host:/other",
                "tree@elsewhere",
                "tree@/local/tree"
            ]
        );
    }

    /// Every mode the parser accepts is classified as writing its alpha
    /// or not, and the two-way modes are the ones that do.
    #[test]
    fn every_mode_says_whether_it_writes_the_alpha() {
        let advertised = parse_mode("not-a-mode").expect_err("an unknown mode is refused");
        let names: Vec<&str> = advertised
            .split(['(', ')', ':', ',', ' ', '\'', '\n'])
            .filter(|word| word.contains("-way-"))
            .collect();
        assert!(names.len() >= 6, "{advertised}");
        for name in names {
            let mode = parse_mode(name).expect("an advertised mode parses");
            assert_eq!(
                alpha_is_written(mode),
                name.starts_with("two-way-"),
                "{name}"
            );
        }
    }

    #[test]
    fn duplicate_sessions_are_rejected() {
        let config = parse(
            r#"
            [groups.one]
            alpha = "/data"
            mode = "two-way-safe"
            betas = ["host:/mirror"]

            [groups.two]
            alpha = "/data"
            mode = "one-way-replica"
            betas = ["host:/mirror"]
            "#,
        );
        let error = format!("{:#}", config.plans().expect_err("plans should fail"));
        assert!(
            error.contains("describe the same alpha and beta"),
            "{error}"
        );
        assert!(
            error.contains("one@host") && error.contains("two@host"),
            "{error}"
        );

        // The same beta listed twice within one group is caught as well.
        let config = parse(
            r#"
            [groups.one]
            alpha = "/data"
            mode = "two-way-safe"
            betas = ["host", "host"]
            "#,
        );
        assert!(config.plans().is_err());
    }

    #[test]
    fn aliased_paths_are_detected_as_duplicates() {
        // Textual differences that denote the same directory — a dot
        // component, and a symlink — must not evade duplicate detection.
        let keep = tempfile::tempdir().expect("temporary directory should be creatable");
        let data = keep.path().join("data");
        std::fs::create_dir_all(&data).expect("directory should be creatable");
        let alias = keep.path().join("alias");
        std::os::unix::fs::symlink(&data, &alias).expect("symlink should be creatable");

        let config = parse(&format!(
            r#"
            [groups.direct]
            alpha = "{data}"
            mode = "two-way-safe"
            betas = ["host:/mirror"]

            [groups.dotted]
            alpha = "{data}/."
            mode = "one-way-replica"
            betas = ["host:/mirror"]

            [groups.linked]
            alpha = "{alias}"
            mode = "one-way-replica"
            betas = ["host:/mirror"]
            "#,
            data = data.display(),
            alias = alias.display(),
        ));
        let error = format!("{:#}", config.plans().expect_err("plans should fail"));
        assert!(
            error.contains("describe the same alpha and beta"),
            "{error}"
        );
        assert!(error.contains("dotted@host"), "{error}");
        assert!(error.contains("linked@host"), "{error}");

        // Betas that don't exist yet still alias when their *ancestors* do:
        // a missing mirror under a symlinked parent would be created at the
        // same physical location as its direct spelling. (The betas live
        // outside the alphas: overlap within a session is refused outright
        // before duplicate detection would see it.)
        let out = keep.path().join("out");
        std::fs::create_dir_all(&out).expect("directory should be creatable");
        let out_alias = keep.path().join("out-alias");
        std::os::unix::fs::symlink(&out, &out_alias).expect("symlink should be creatable");
        let config = parse(&format!(
            r#"
            [groups.one]
            alpha = "{data}"
            mode = "two-way-safe"
            betas = ["{out}/mirror/new"]

            [groups.two]
            alpha = "{alias}"
            mode = "one-way-replica"
            betas = ["{out_alias}/mirror/new"]
            "#,
            data = data.display(),
            alias = alias.display(),
            out = out.display(),
            out_alias = out_alias.display(),
        ));
        let error = format!("{:#}", config.plans().expect_err("plans should fail"));
        assert!(
            error.contains("describe the same alpha and beta"),
            "{error}"
        );
    }

    #[test]
    fn relative_local_roots_are_rejected() {
        let config = parse(
            r#"
            [groups.x]
            alpha = "relative/alpha"
            mode = "two-way-safe"
            betas = ["./relative-beta", "~user/beta", "/absolute/beta"]
            "#,
        );
        let error = format!("{:#}", config.plans().expect_err("plans should fail"));
        assert!(
            error.contains("alpha: 'relative/alpha' must be an absolute"),
            "{error}"
        );
        assert!(
            error.contains("'./relative-beta' must be an absolute"),
            "{error}"
        );
        // The unexpanded ~user form is relative, so it's caught by the same
        // check instead of silently becoming a working-directory child.
        assert!(
            error.contains("'~user/beta' must be an absolute"),
            "{error}"
        );
        assert!(!error.contains("/absolute/beta"), "{error}");
    }

    #[test]
    fn invalid_ignore_patterns_are_reported_with_the_other_errors() {
        let config = parse(
            r#"
            [defaults]
            ignores = ["[unclosed"]

            [groups.x]
            alpha = "/data"
            mode = "sideways"
            betas = ["host"]
            "#,
        );
        let error = format!("{:#}", config.plans().expect_err("plans should fail"));
        assert!(error.contains("invalid ignore pattern"), "{error}");
        assert!(error.contains("unknown mode"), "{error}");
    }

    #[test]
    fn symlink_and_permission_modes_resolve_through_defaults() {
        let config = parse(
            r#"
            [defaults]
            mode = "two-way-safe"
            symlink_mode = "portable"
            file_mode = "0644"
            directory_mode = "0755"

            [groups.inherits]
            alpha = "/a"
            betas = ["host"]

            [groups.overrides]
            alpha = "/b"
            symlink_mode = "ignore"
            file_mode = "600"
            betas = ["host"]
            "#,
        );
        let plans = config.plans().expect("plans should derive");
        assert_eq!(plans[0].symlink_mode, SymlinkMode::Portable);
        assert_eq!(plans[0].file_mode, Some(0o644));
        assert_eq!(plans[0].directory_mode, Some(0o755));
        assert_eq!(plans[1].symlink_mode, SymlinkMode::Ignore);
        assert_eq!(plans[1].file_mode, Some(0o600));
        assert_eq!(plans[1].directory_mode, Some(0o755));

        // Invalid values are aggregated with the other errors.
        let config = parse(
            r#"
            [groups.x]
            alpha = "/a"
            mode = "two-way-safe"
            symlink_mode = "follow"
            file_mode = "0444"
            directory_mode = "banana"
            betas = ["host"]
            "#,
        );
        let error = format!("{:#}", config.plans().expect_err("plans should fail"));
        assert!(error.contains("unknown symlink mode 'follow'"), "{error}");
        assert!(error.contains("denies the owner access"), "{error}");
        assert!(
            error.contains("invalid octal permission mode 'banana'"),
            "{error}"
        );
    }

    #[test]
    fn permission_mode_parsing() {
        assert_eq!(parse_permission_mode("0644", false).unwrap(), 0o644);
        assert_eq!(parse_permission_mode("600", false).unwrap(), 0o600);
        assert_eq!(parse_permission_mode("0o755", true).unwrap(), 0o755);
        // Bits beyond the permission range are rejected.
        assert!(parse_permission_mode("7777", false).is_err());
        // The owner must keep working access.
        assert!(parse_permission_mode("0444", false).is_err());
        assert!(parse_permission_mode("0600", true).is_err());
    }

    #[test]
    fn mode_names_round_trip() {
        for mode in [
            SyncMode::TwoWaySafe,
            SyncMode::TwoWayResolved,
            SyncMode::OneWaySafe,
            SyncMode::OneWayReplica,
        ] {
            assert_eq!(parse_mode(mode_name(mode)).unwrap(), mode);
        }
        // The older spellings, and the colloquial one, are still accepted
        // — existing configurations must not break on a rename.
        assert_eq!(parse_mode("two-way-safe").unwrap(), SyncMode::TwoWaySafe);
        assert_eq!(
            parse_mode("two-way-resolved").unwrap(),
            SyncMode::TwoWayResolved
        );
        assert_eq!(parse_mode("one-way-safe").unwrap(), SyncMode::OneWaySafe);
        assert_eq!(
            parse_mode("one-way-replica").unwrap(),
            SyncMode::OneWayReplica
        );
        assert_eq!(parse_mode("mirror").unwrap(), SyncMode::OneWayReplica);
        assert!(parse_mode("bidirectional").is_err());
    }

    /// The peering spellings parse to the two-way modes they wrap and
    /// carry the peering flag; a plan spells them back the way they were
    /// written.
    #[test]
    fn peering_modes_parse_and_print_back() {
        assert_eq!(
            parse_mode_spec("peering-conflict-dangerously-experimental").unwrap(),
            (SyncMode::TwoWaySafe, true)
        );
        assert_eq!(
            parse_mode_spec("peering-alpha-dangerously-experimental").unwrap(),
            (SyncMode::TwoWayResolved, true)
        );
        assert_eq!(
            parse_mode_spec("two-way-conflict").unwrap(),
            (SyncMode::TwoWaySafe, false)
        );
        // The refusal message advertises them, which the template test
        // also relies on.
        let advertised = parse_mode("nope").unwrap_err();
        assert!(advertised.contains("peering-conflict-dangerously-experimental"));
        assert!(advertised.contains("peering-alpha-dangerously-experimental"));

        let config = parse(
            r#"
            [groups.g]
            mode = "peering-alpha-dangerously-experimental"
            alpha = "/tmp/a"
            betas = ["u@h:/tmp/b"]
            "#,
        );
        let plans = config.plans().expect("plans");
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].mode, SyncMode::TwoWayResolved);
        assert_eq!(
            plans[0].mode_name(),
            "peering-alpha-dangerously-experimental"
        );
        let peering = plans[0].peering.expect("a peering plan");
        assert_eq!(peering.ttl, DEFAULT_PEERING_TTL);
        assert_eq!(peering.failover_after, DEFAULT_PEERING_FAILOVER_AFTER);

        let config = parse(
            r#"
            [groups.g]
            mode = "two-way-alpha"
            alpha = "/tmp/a"
            betas = ["u@h:/tmp/b"]
            "#,
        );
        let plans = config.plans().expect("plans");
        assert!(plans[0].peering.is_none());
        assert_eq!(plans[0].mode_name(), "two-way-alpha");
    }

    /// The timing section: read, defaulted, and refused when the wait is
    /// shorter than the lease it is supposed to wait for.
    #[test]
    fn peering_timing_is_read_defaulted_and_checked() {
        let config = parse(
            r#"
            [advanced.peering-dangerously-experimental]
            ttl = "10s"
            failover_after = 45
            [groups.g]
            mode = "peering-conflict-dangerously-experimental"
            alpha = "/tmp/a"
            betas = ["u@h:/tmp/b"]
            "#,
        );
        let peering = config.plans().expect("plans")[0].peering.expect("peering");
        assert_eq!(peering.ttl, Duration::from_secs(10));
        assert_eq!(peering.failover_after, Duration::from_secs(45));

        let config = parse(
            r#"
            [advanced.peering-dangerously-experimental]
            ttl = "60s"
            failover_after = "30s"
            [groups.g]
            mode = "peering-conflict-dangerously-experimental"
            alpha = "/tmp/a"
            betas = ["u@h:/tmp/b"]
            "#,
        );
        let error = config.plans().unwrap_err().to_string();
        assert!(error.contains("failover_after"), "{error}");
        assert!(error.contains("shorter than ttl"), "{error}");

        // A bad section is an error even for a configuration with no
        // peering group: an unknown key is refused everywhere.
        let result: std::result::Result<Config, _> = toml::from_str(
            r#"
            [advanced.peering-dangerously-experimental]
            lease = "10s"
            "#,
        );
        assert!(result.is_err(), "unknown keys are refused");
    }

    /// Peering names the machine the configuration runs on as the alpha,
    /// and every peer as another host.
    #[test]
    fn peering_needs_a_local_alpha_and_remote_betas() {
        let config = parse(
            r#"
            [groups.g]
            mode = "peering-conflict-dangerously-experimental"
            alpha = "u@h:/tmp/a"
            betas = ["v@k:/tmp/b"]
            "#,
        );
        let error = config.plans().unwrap_err().to_string();
        assert!(error.contains("needs a local alpha"), "{error}");

        let config = parse(
            r#"
            [groups.g]
            mode = "peering-conflict-dangerously-experimental"
            alpha = "/tmp/a"
            betas = ["/tmp/b", "u@h:/tmp/c"]
            "#,
        );
        let error = config.plans().unwrap_err().to_string();
        assert!(error.contains("every beta on another host"), "{error}");
    }
}
