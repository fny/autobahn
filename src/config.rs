//! The groups configuration: a declarative description of synchronization
//! sessions, organized as groups that fan one local directory (the alpha)
//! out to any number of destinations (the betas).
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

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::paths::{expand_tilde, resolve_for_identity};
use crate::scan::{IgnoreSet, SymlinkMode};
use crate::session::session_identifier;
use crate::tree::SyncMode;

/// The synchronization interval used when neither a group nor the defaults
/// specify one.
pub const DEFAULT_INTERVAL_SECONDS: u64 = 5;

/// The parsed configuration file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Settings inherited by every group.
    #[serde(default)]
    pub defaults: Defaults,
    /// Hosts excluded from every group's betas.
    #[serde(default)]
    pub disabled: Vec<String>,
    /// The synchronization groups, keyed by name.
    #[serde(default)]
    pub groups: BTreeMap<String, Group>,
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
    /// The default interval, in seconds, between synchronization cycles.
    pub interval: Option<u64>,
    /// The default symbolic link treatment (`ignore`, `portable`, or `raw`).
    pub symlink_mode: Option<String>,
    /// The default permission bits (octal) for created files.
    pub file_mode: Option<String>,
    /// The default permission bits (octal) for created directories.
    pub directory_mode: Option<String>,
}

/// One synchronization group: a local alpha directory fanned out to one or
/// more beta destinations.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    /// The alpha synchronization root (a local path; `~` is expanded).
    pub alpha: String,
    /// The beta destinations (remote `[user@]host[:path]` or local paths).
    #[serde(default)]
    pub betas: Vec<String>,
    /// The synchronization mode (falls back to the defaults).
    pub mode: Option<String>,
    /// Ignore patterns, appended to the defaults' patterns.
    #[serde(default)]
    pub ignores: Vec<String>,
    /// The interval, in seconds, between cycles (falls back to the defaults).
    pub interval: Option<u64>,
    /// The symbolic link treatment (`ignore`, `portable`, or `raw`; falls
    /// back to the defaults).
    pub symlink_mode: Option<String>,
    /// The permission bits (octal) for created files (falls back to the
    /// defaults).
    pub file_mode: Option<String>,
    /// The permission bits (octal) for created directories (falls back to
    /// the defaults).
    pub directory_mode: Option<String>,
    /// Advanced: connect this group's remote betas through this command
    /// (whitespace split into argv) instead of SSH. Used for testing and
    /// custom transports; the beta's host is then informational only.
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
    /// The alpha root, tilde-expanded.
    pub alpha: PathBuf,
    /// The alpha root as written in the configuration (used for session
    /// identity and for remote path inheritance).
    pub alpha_spec: String,
    /// The beta destination.
    pub beta: BetaTarget,
    /// The synchronization mode.
    pub mode: SyncMode,
    /// The combined ignore patterns (defaults first, then the group's).
    pub ignores: Vec<String>,
    /// The interval between synchronization cycles.
    pub interval: Duration,
    /// The symbolic link treatment.
    pub symlink_mode: SymlinkMode,
    /// The permission bits for created files (`None` for the endpoint
    /// default).
    pub file_mode: Option<u32>,
    /// The permission bits for created directories (`None` for the endpoint
    /// default).
    pub directory_mode: Option<u32>,
    /// The stable identifier isolating this session's state, derived from
    /// the *resolved* endpoint identities (see
    /// [`resolve_for_identity`](crate::paths::resolve_for_identity)) so that
    /// textual aliases of the same roots — across configurations, or between
    /// a supervisor and a manual `sync` — share one identity and therefore
    /// one state lock.
    identifier: String,
}

/// A resolved beta destination.
#[derive(Clone, Debug, PartialEq)]
pub enum BetaTarget {
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
            BetaTarget::Local(path) => path.to_string_lossy().into_owned(),
            BetaTarget::Remote {
                destination, path, ..
            } => format!("{destination}:{path}"),
        }
    }

    /// Returns the stable identifier isolating this session's state.
    pub fn identifier(&self) -> String {
        self.identifier.clone()
    }
}

/// Computes a session identity string for a beta target: the resolved
/// physical path for a local target, the textual `destination:path` for a
/// remote one (whose paths can only be resolved on the remote side).
fn beta_identity(beta: &BetaTarget) -> String {
    match beta {
        BetaTarget::Local(path) => resolve_for_identity(path).to_string_lossy().into_owned(),
        BetaTarget::Remote {
            destination, path, ..
        } => format!("{destination}:{path}"),
    }
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
    pub fn plans(&self) -> Result<Vec<SessionPlan>> {
        let mut errors = Vec::new();
        let mut plans = Vec::new();
        // Two plans over the same roots would synchronize the same trees
        // concurrently (and, when textually identical, share session state),
        // so duplicates are a configuration error rather than a runtime
        // surprise. Detection compares *canonicalized* local paths, so
        // aliases — a trailing `/.`, a symlink, `~/data` versus its expanded
        // form — are caught, not just textual repeats. (Nested or otherwise
        // overlapping roots are a different hazard that no pairwise identity
        // can detect.)
        let mut identities: HashMap<(PathBuf, String), String> = HashMap::new();

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
            if group.alpha.is_empty() {
                errors.push(format!("group '{name}' has an empty alpha"));
            }
            if group.betas.is_empty() {
                errors.push(format!("group '{name}' has no betas"));
            }
            let alpha = match expand_tilde(&group.alpha) {
                // A relative alpha would resolve against whatever working
                // directory the supervisor happened to start in — a
                // different tree under a service than in a shell.
                Ok(alpha) if !group.alpha.is_empty() && !alpha.is_absolute() => {
                    errors.push(format!(
                        "group '{name}' alpha '{}' must be an absolute (or ~-relative) path",
                        group.alpha
                    ));
                    None
                }
                Ok(alpha) => Some(alpha),
                Err(error) => {
                    errors.push(format!("group '{name}': {error:#}"));
                    None
                }
            };
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

            let mut ignores = self.defaults.ignores.clone();
            ignores.extend(group.ignores.iter().cloned());
            // Compile the combined patterns now, so a bad pattern is a
            // configuration error alongside the others rather than a runtime
            // failure discovered only by the affected session's worker.
            if let Err(error) = IgnoreSet::new(&ignores) {
                errors.push(format!("group '{name}': invalid ignore pattern: {error:#}"));
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

            for beta in &group.betas {
                if beta.is_empty() {
                    errors.push(format!("group '{name}' has an empty beta"));
                    continue;
                }
                let target = match parse_beta(beta, &group.alpha, agent_command.clone()) {
                    Ok(target) => target,
                    Err(message) => {
                        errors.push(format!("group '{name}' beta '{beta}': {message}"));
                        continue;
                    }
                };
                if let BetaTarget::Local(path) = &target {
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
                    BetaTarget::Local(path) => path.to_string_lossy().into_owned(),
                    BetaTarget::Remote { destination, .. } => host_of(destination).to_owned(),
                };
                if let BetaTarget::Remote { .. } = &target {
                    if self.disabled.iter().any(|disabled| disabled == &host) {
                        continue;
                    }
                }
                let (Some(mode), Some(alpha)) = (mode, alpha.clone()) else {
                    continue;
                };
                let alpha_identity = resolve_for_identity(&alpha);
                let beta_identity = beta_identity(&target);
                let identifier =
                    session_identifier(&alpha_identity.to_string_lossy(), &beta_identity);
                let plan = SessionPlan {
                    group: name.clone(),
                    host,
                    alpha,
                    alpha_spec: group.alpha.clone(),
                    beta: target,
                    mode,
                    ignores: ignores.clone(),
                    interval,
                    symlink_mode,
                    file_mode,
                    directory_mode,
                    identifier,
                };
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

/// Parses a synchronization mode name.
pub fn parse_mode(mode: &str) -> Result<SyncMode, String> {
    match mode {
        "two-way-safe" => Ok(SyncMode::TwoWaySafe),
        "two-way-resolved" => Ok(SyncMode::TwoWayResolved),
        "one-way-safe" => Ok(SyncMode::OneWaySafe),
        "one-way-replica" => Ok(SyncMode::OneWayReplica),
        other => Err(format!(
            "unknown mode '{other}' (expected one of: one-way-replica, one-way-safe, \
             two-way-resolved, two-way-safe)"
        )),
    }
}

/// Returns the canonical name of a synchronization mode.
pub fn mode_name(mode: SyncMode) -> &'static str {
    match mode {
        SyncMode::TwoWaySafe => "two-way-safe",
        SyncMode::TwoWayResolved => "two-way-resolved",
        SyncMode::OneWaySafe => "one-way-safe",
        SyncMode::OneWayReplica => "one-way-replica",
    }
}

/// Indicates whether or not a beta entry denotes a local path (rather than a
/// remote host): it does when it visibly looks like one — a `/` before any
/// `:`, or a leading `.`, `/`, or `~`.
fn is_local(beta: &str) -> bool {
    if beta.starts_with('.') || beta.starts_with('/') || beta.starts_with('~') {
        return true;
    }
    match (beta.find('/'), beta.find(':')) {
        (Some(_), None) => true,
        (Some(slash), Some(colon)) => slash < colon,
        _ => false,
    }
}

/// Parses one beta entry against its group's alpha (whose path a remote
/// entry inherits when it specifies none).
fn parse_beta(
    beta: &str,
    alpha: &str,
    agent_command: Option<Vec<String>>,
) -> Result<BetaTarget, String> {
    if is_local(beta) {
        let path = expand_tilde(beta).map_err(|error| format!("{error:#}"))?;
        return Ok(BetaTarget::Local(path));
    }
    let (destination, path) = match beta.find(':') {
        Some(colon) => {
            let path = &beta[colon + 1..];
            if path.is_empty() {
                return Err("empty path after ':'".into());
            }
            (&beta[..colon], path.to_owned())
        }
        None => (beta, alpha.to_owned()),
    };
    if destination.is_empty() || host_of(destination).is_empty() {
        return Err("empty host".into());
    }
    Ok(BetaTarget::Remote {
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
            BetaTarget::Local(PathBuf::from("/mnt/backup/data"))
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
            BetaTarget::Remote {
                destination: "build.example.com".into(),
                path: "~/project".into(),
                agent_command: None,
            }
        );

        assert_eq!(plans[2].display(), "project@lab.example.com");
        assert_eq!(
            plans[2].beta,
            BetaTarget::Remote {
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
                parse_beta(beta, "~/x", None).expect("should parse"),
                BetaTarget::Local(_)
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
        assert!(parse_beta("host:", "~/x", None).is_err());
        assert!(parse_beta(":path", "~/x", None).is_err());
        assert!(parse_beta("user@:path", "~/x", None).is_err());
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
            BetaTarget::Remote {
                destination: "host".into(),
                path: "/a".into(),
                agent_command: Some(vec!["custom-agent".into(), "--flag".into()]),
            }
        );
        // Local betas never involve an agent.
        assert_eq!(plans[1].beta, BetaTarget::Local(PathBuf::from("/local")));
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
        // same physical location as its direct spelling.
        let config = parse(&format!(
            r#"
            [groups.one]
            alpha = "{data}"
            mode = "two-way-safe"
            betas = ["{data}/mirror/new"]

            [groups.two]
            alpha = "{alias}"
            mode = "one-way-replica"
            betas = ["{alias}/mirror/new"]
            "#,
            data = data.display(),
            alias = alias.display(),
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
        assert!(parse_mode("bidirectional").is_err());
    }
}
