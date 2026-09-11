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
//! # Top-level keys (like `disabled`) must precede the first section header.
//! disabled = ["flaky.example.com"]
//!
//! [defaults]
//! mode = "two-way-safe"
//! ignores = [".git"]
//! interval = 5
//!
//! [groups.project]
//! alpha = "~/project"
//! betas = ["build.example.com", "user@lab.example.com:/srv/project"]
//!
//! [groups.dotfiles]
//! alpha = "~/.config/shell"
//! mode = "one-way-replica"
//! betas = ["build.example.com", "/mnt/backup/shell"]
//! ```
//!
//! A beta is **remote** unless it visibly denotes a local path: an entry
//! containing a `/` before any `:`, or beginning with `.`, `/`, or `~`, is a
//! local path; anything else is `[user@]host[:path]`. A remote beta without
//! an explicit path inherits the group's alpha path *as written* (so a
//! home-relative alpha resolves against each remote host's own home).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
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
#[serde(deny_unknown_fields)]
pub struct Config {
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
    #[serde(default)]
    pub disabled: Vec<String>,
    /// How much the supervisor writes to its log: "quiet", "normal" (the
    /// default), or "debug". `AUTOBAHN_LOG` overrides it for one run.
    pub log: Option<String>,
    /// Settings inherited by every group.
    #[serde(default)]
    pub defaults: Defaults,
    /// The synchronization groups, keyed by name.
    #[serde(default)]
    pub groups: BTreeMap<String, Group>,
    /// Tuning that has a correct value already.
    #[serde(default)]
    pub advanced: Advanced,
    /// Retired. Kept only so that a configuration written against the old
    /// shape gets an answer rather than "unknown field `alerts`".
    pub alerts: Option<toml::Value>,
}

/// The `[advanced]` section: settings whose defaults are the right answer.
/// Grouped under one heading so that finding yourself here is itself the
/// message — the subsystem comes second because the warning should land
/// first.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Advanced {
    /// Alerter timing.
    #[serde(default)]
    pub alerts: AlertsAdvanced,
}

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
#[serde(untagged)]
pub enum DurationSpec {
    /// A plain number of seconds, matching how `interval` is written.
    Seconds(u64),
    /// A suffixed string ("30s", "5m", "2h").
    Text(String),
}

/// Settings inherited by every group (each overridable per group).
#[derive(Debug, Default, Deserialize)]
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
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum SizeSpec {
    /// A raw byte count.
    Bytes(u64),
    /// A suffixed size string ("100MB", "2GiB", "512K").
    Text(String),
}

/// One synchronization group: a local alpha directory fanned out to one or
/// more beta destinations.
#[derive(Debug, Deserialize)]
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
    /// The staging placement for both endpoints.
    pub staging: StagingMode,
    /// The owner for created entries (`None` to leave ownership alone).
    pub default_owner: Option<String>,
    /// The group for created entries (`None` to leave ownership alone).
    pub default_group: Option<String>,
    /// The stable identifier isolating this session's state, derived from
    /// the *resolved* endpoint identities (see
    /// [`resolve_for_identity`](crate::paths::resolve_for_identity)) so that
    /// textual aliases of the same roots — across configurations, or between
    /// a supervisor and a manual `sync` — share one identity and therefore
    /// one state lock.
    identifier: String,
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

impl SessionPlan {
    /// Returns the display name of the session (`group@host`).
    pub fn display(&self) -> String {
        format!("{}@{}", self.group, self.host)
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

/// Reports how two endpoint identities overlap on disk, when that is
/// determinable: equal, or one containing the other. Local identities are
/// resolved physical paths, so containment is a path-prefix test on a
/// component boundary. Remote identities can only be compared textually,
/// and only against the same destination; a remote path that reaches the
/// same tree through a different spelling is undetectable from here.
fn overlap(alpha: &str, beta: &str) -> Option<&'static str> {
    if alpha == beta {
        return Some("the same tree");
    }
    let contains = |outer: &str, inner: &str| {
        inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    if contains(alpha, beta) {
        return Some("a tree inside the alpha");
    }
    if contains(beta, alpha) {
        return Some("a tree containing the alpha");
    }
    None
}

impl Config {
    /// Loads and parses a configuration file.
    pub fn load(path: &std::path::Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("unable to read configuration {}", path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("unable to parse configuration {}", path.display()))
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

        let advanced = &self.advanced.alerts;
        let duration = |spec: &Option<DurationSpec>, what: &str, fallback: Duration| match spec {
            None => Ok(fallback),
            Some(spec) => parse_duration(spec).map_err(|message| {
                anyhow!("invalid configuration:\n  advanced.alerts.{what}: {message}")
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
                    "invalid configuration:\n  advanced.alerts.after.{name}: unknown state \
                     (expected one of: {})",
                    known.join(", ")
                );
            };
            after.insert(
                alert,
                parse_duration(spec).map_err(|message| {
                    anyhow!("invalid configuration:\n  advanced.alerts.after.{name}: {message}")
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

    pub fn plans(&self) -> Result<Vec<SessionPlan>> {
        let mut errors = Vec::new();
        // Ignore files live under the *default* state root, not under an
        // override: they are the reader's own library of patterns, shared
        // by every configuration, and not part of any session's state.
        // Nothing is read unless a group names a file, so a reader who
        // does not use the directory never pays for it — or notices that
        // it is missing.
        let ignore_directory = crate::paths::default_state_root()
            .map(|root| root.join(crate::scan::ignorefile::DIRECTORY))
            .unwrap_or_default();
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
            let mode = match group.mode.as_deref().or(self.defaults.mode.as_deref()) {
                Some(mode) => match parse_mode(mode) {
                    Ok(mode) => Some(mode),
                    Err(message) => {
                        errors.push(format!("group '{name}': {message}"));
                        None
                    }
                },
                None => {
                    errors.push(format!(
                        "group '{name}' has no mode and the defaults specify none"
                    ));
                    None
                }
            };
            let power_durability = match group
                .durability
                .as_deref()
                .or(self.defaults.durability.as_deref())
                .unwrap_or("process")
            {
                "process" => false,
                "power" => true,
                other => {
                    errors.push(format!(
                        "group '{name}': unknown durability '{other}' \
                         (expected 'process' or 'power')"
                    ));
                    false
                }
            };
            if group.alpha.is_empty() {
                errors.push(format!("group '{name}' has an empty alpha"));
            }
            if group.betas.is_empty() {
                errors.push(format!("group '{name}' has no betas"));
            }
            let agent_command = match &group.agent_command {
                None => None,
                Some(command) => {
                    let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
                    if argv.is_empty() {
                        errors.push(format!("group '{name}' has an empty agent_command"));
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
                            "group '{name}' alpha '{}' must be an absolute (or ~-relative) path",
                            group.alpha
                        ));
                        None
                    }
                    Ok(target) => Some(target),
                    Err(message) => {
                        errors.push(format!("group '{name}' alpha '{}': {message}", group.alpha));
                        None
                    }
                }
            };
            // A disabled alpha host takes the whole group with it: every
            // session of the group flows through that endpoint.
            if let Some(EndpointTarget::Remote { destination, .. }) = &alpha {
                if self.disabled.iter().any(|d| d == host_of(destination)) {
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
            for (source, names) in [
                ("the defaults'", &self.defaults.ignore_files),
                ("its own", &group.ignore_files),
            ] {
                for file in names {
                    match crate::scan::ignorefile::read(&ignore_directory, file) {
                        Ok(patterns) => ignores.extend(patterns),
                        Err(error) => {
                            ignore_errors.push(format!("{source} ignore_files: {error:#}"))
                        }
                    }
                }
                if source == "the defaults'" {
                    ignores.extend(self.defaults.ignores.iter().cloned());
                }
            }
            ignores.extend(group.ignores.iter().cloned());
            for error in ignore_errors {
                errors.push(format!("group '{name}': {error}"));
            }
            // Compile the combined patterns now, so a bad pattern is a
            // configuration error alongside the others rather than a runtime
            // failure discovered only by the affected session's worker.
            match IgnoreSet::new(&ignores) {
                Err(error) => {
                    errors.push(format!("group '{name}': invalid ignore pattern: {error:#}"))
                }
                // A line that cannot ever do anything is a mistake worth
                // refusing, not a preference: combining ignore files
                // written independently is exactly how they appear, and
                // the reader cannot see it by looking at either file.
                Ok(compiled) => errors.extend(
                    compiled
                        .dead_negations()
                        .into_iter()
                        .map(|dead| format!("group '{name}': {dead}")),
                ),
            }
            let interval = Duration::from_secs(
                group
                    .interval
                    .or(self.defaults.interval)
                    .unwrap_or(DEFAULT_INTERVAL_SECONDS)
                    .max(1),
            );
            let symlink_mode = match group
                .symlink_mode
                .as_deref()
                .or(self.defaults.symlink_mode.as_deref())
            {
                None => SymlinkMode::default(),
                Some(mode) => match parse_symlink_mode(mode) {
                    Ok(mode) => mode,
                    Err(message) => {
                        errors.push(format!("group '{name}': {message}"));
                        SymlinkMode::default()
                    }
                },
            };
            let mut permission = |value: Option<&str>, directory: bool| match value {
                None => None,
                Some(mode) => match parse_permission_mode(mode, directory) {
                    Ok(bits) => Some(bits),
                    Err(message) => {
                        errors.push(format!("group '{name}': {message}"));
                        None
                    }
                },
            };
            let file_mode = permission(
                group
                    .file_mode
                    .as_deref()
                    .or(self.defaults.file_mode.as_deref()),
                false,
            );
            let directory_mode = permission(
                group
                    .directory_mode
                    .as_deref()
                    .or(self.defaults.directory_mode.as_deref()),
                true,
            );
            let max_file_size = match group
                .max_file_size
                .as_ref()
                .or(self.defaults.max_file_size.as_ref())
            {
                None => None,
                Some(spec) => match parse_size(spec) {
                    Ok(bytes) => Some(bytes),
                    Err(message) => {
                        errors.push(format!("group '{name}': {message}"));
                        None
                    }
                },
            };
            let max_entry_count = group.max_entry_count.or(self.defaults.max_entry_count);
            let staging = match group
                .staging
                .as_deref()
                .or(self.defaults.staging.as_deref())
            {
                None => StagingMode::default(),
                Some(mode) => match parse_staging_mode(mode) {
                    Ok(mode) => mode,
                    Err(message) => {
                        errors.push(format!("group '{name}': {message}"));
                        StagingMode::default()
                    }
                },
            };
            let mut ownership = |value: Option<&str>, kind: &str| match value {
                None => None,
                Some("") => {
                    errors.push(format!("group '{name}' has an empty {kind}"));
                    None
                }
                Some(spec) => Some(spec.to_owned()),
            };
            let default_owner = ownership(
                group
                    .default_owner
                    .as_deref()
                    .or(self.defaults.default_owner.as_deref()),
                "default_owner",
            );
            let default_group = ownership(
                group
                    .default_group
                    .as_deref()
                    .or(self.defaults.default_group.as_deref()),
                "default_group",
            );

            for beta in &group.betas {
                if beta.is_empty() {
                    errors.push(format!("group '{name}' has an empty beta"));
                    continue;
                }
                let target =
                    match parse_endpoint(beta, Some(&inherited_path), agent_command.clone()) {
                        Ok(target) => target,
                        Err(message) => {
                            errors.push(format!("group '{name}' beta '{beta}': {message}"));
                            continue;
                        }
                    };
                if let EndpointTarget::Local(path) = &target {
                    // The same working-directory hazard as a relative alpha,
                    // and the trap that catches unexpanded `~user` forms.
                    if !path.is_absolute() {
                        errors.push(format!(
                            "group '{name}' beta '{beta}' must be an absolute (or ~-relative) \
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
                    if self.disabled.iter().any(|disabled| disabled == &host) {
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
                    staging,
                    default_owner: default_owner.clone(),
                    default_group: default_group.clone(),
                    identifier,
                };
                // A beta that is the alpha, or nested either way around,
                // makes the session consume its own output: reconciliation
                // sees the copy as divergence and, in replica mode,
                // deletes the alpha root through the beta path. This was
                // reproduced, not hypothesized — the check is load-bearing.
                // (Relays — one session's beta feeding another's alpha —
                // remain legal: only overlap within a single session is
                // self-referential.)
                let comparable = matches!(
                    (&plan.alpha, &plan.beta),
                    (EndpointTarget::Local(_), EndpointTarget::Local(_))
                ) || matches!(
                    (&plan.alpha, &plan.beta),
                    (
                        EndpointTarget::Remote { destination: a, .. },
                        EndpointTarget::Remote { destination: b, .. },
                    ) if a == b
                );
                if comparable {
                    if let Some(how) = overlap(&alpha_identity, &beta_identity) {
                        errors.push(format!(
                            "session '{}': the beta is {how}; a session cannot \
                             synchronize a tree with itself or with a tree that \
                             contains it",
                            plan.display()
                        ));
                        continue;
                    }
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
        let writable = |plan: &SessionPlan, alpha: bool| -> bool {
            if alpha {
                matches!(plan.mode, SyncMode::TwoWaySafe | SyncMode::TwoWayResolved)
            } else {
                true
            }
        };
        let mut endpoints: Vec<(String, bool, String)> = Vec::new();
        for plan in &plans {
            endpoints.push((
                plan.alpha_identity.clone(),
                writable(plan, true),
                plan.display(),
            ));
            endpoints.push((
                plan.beta_identity.clone(),
                writable(plan, false),
                plan.display(),
            ));
        }
        for (index, (identity, writes, owner)) in endpoints.iter().enumerate() {
            for (offset, (other_identity, other_writes, other_owner)) in
                endpoints.iter().enumerate().skip(index + 1)
            {
                if owner == other_owner {
                    continue; // within-session overlap is checked above
                }
                if identity == other_identity {
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
                let contains = |outer: &str, inner: &str| {
                    inner
                        .strip_prefix(outer)
                        .is_some_and(|rest| rest.starts_with('/'))
                };
                // Which one contains which decides both the message and,
                // below, whose ignore patterns are consulted.
                let (outer, inner, outer_index) = if contains(identity, other_identity) {
                    (identity, other_identity, index)
                } else if contains(other_identity, identity) {
                    (other_identity, identity, offset)
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
                // Endpoints are pushed two per plan, alpha then beta, so an
                // endpoint at index i belongs to plan i / 2.
                let outer_plan = &plans[outer_index / 2];
                let excluded = std::path::Path::new(inner)
                    .strip_prefix(outer)
                    .ok()
                    .and_then(|relative| relative.to_str())
                    .is_some_and(|relative| {
                        IgnoreSet::new(&outer_plan.ignores)
                            .is_ok_and(|ignores| ignores.ignored(relative, true))
                    });
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
            bail!("invalid configuration:\n  {}", errors.join("\n  "));
        }
        Ok(plans)
    }
}

/// Parses a symbolic link mode name.
pub fn parse_symlink_mode(mode: &str) -> Result<SymlinkMode, String> {
    match mode {
        "ignore" => Ok(SymlinkMode::Ignore),
        "portable" => Ok(SymlinkMode::Portable),
        "raw" | "posix-raw" => Ok(SymlinkMode::Raw),
        other => Err(format!(
            "unknown symlink mode '{other}' (expected one of: ignore, portable, raw)"
        )),
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
    match mode {
        "state" => Ok(StagingMode::State),
        "beside-root" => Ok(StagingMode::BesideRoot),
        "inside-root" => Ok(StagingMode::InsideRoot),
        other => Err(format!(
            "unknown staging placement '{other}' (expected one of: state, beside-root, \
             inside-root)"
        )),
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

/// Parses a synchronization mode name.
pub fn parse_mode(mode: &str) -> Result<SyncMode, String> {
    // The names are a grid: direction, then what happens when the two
    // sides disagree about a file — it is reported as a conflict, or alpha
    // wins. The older names (safe, resolved, replica) described the same
    // four modes without exposing that structure; they stay accepted so
    // existing configurations keep working.
    match mode {
        "two-way-conflict" | "two-way-safe" => Ok(SyncMode::TwoWaySafe),
        "two-way-alpha" | "two-way-resolved" => Ok(SyncMode::TwoWayResolved),
        "one-way-conflict" | "one-way-safe" => Ok(SyncMode::OneWaySafe),
        // "mirror" is what everyone calls this shape (rsync --delete), so
        // it is accepted too.
        "one-way-alpha" | "one-way-replica" | "mirror" => Ok(SyncMode::OneWayReplica),
        other => Err(format!(
            "unknown mode '{other}' (expected one of: two-way-conflict, two-way-alpha, \
             one-way-conflict, one-way-alpha)"
        )),
    }
}

/// Returns the canonical name of a synchronization mode.
pub fn mode_name(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::TwoWaySafe => "two-way-conflict",
        SyncMode::TwoWayResolved => "two-way-alpha",
        SyncMode::OneWaySafe => "one-way-conflict",
        SyncMode::OneWayReplica => "one-way-alpha",
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

/// Extracts the host from an SSH destination (`host` or `user@host`).
fn host_of(destination: &str) -> &str {
    match destination.rfind('@') {
        Some(at) => &destination[at + 1..],
        None => destination,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        toml::from_str(text).expect("configuration should parse")
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

    #[test]
    fn the_advanced_section_overrides_the_built_in_table() {
        use crate::alerts::Alert;
        let config = parse(
            r#"
            on_alert = "notify me"

            [advanced.alerts]
            coalesce_after = "5s"

            [advanced.alerts.after]
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
            disabled = ["build.example.com"]

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
            disabled = ["down.example.com"]

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
        assert!(error.contains("'first' has no betas"), "{error}");
        assert!(error.contains("'second' has an empty alpha"), "{error}");
        assert!(
            error.contains("'second' has no mode and the defaults specify none"),
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
            error.contains("alpha 'relative/alpha' must be an absolute"),
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
}
