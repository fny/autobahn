//! Alerts: telling someone when a session needs them.
//!
//! A supervisor running as a login service is invisible by design. It syncs
//! for weeks without being looked at, which is the point — and it is also
//! the problem, because a conflict, a permission that stopped working, or a
//! safety halt all sit there indefinitely with nobody told. `status` answers
//! the question, but only if you think to ask it.
//!
//! So the supervisor can run a command when a session starts needing you.
//! Three rules shape the design, and each of them exists because the naive
//! version is unusable:
//!
//! * **Nothing happens when nothing is wrong.** Silence is the normal state.
//! * **A condition must hold before it counts.** Alerting on the edge means
//!   a wifi handover — fifteen sessions unreachable for eight seconds —
//!   pages you, then pages you again to say it recovered, every time you
//!   walk between rooms. Waiting out a confirmation period reports nothing
//!   at all, which is correct: it fixed itself. The same wait coalesces a
//!   burst into one call for free.
//! * **An alert fires on change, not on repetition.** A session that has
//!   been in conflict for three days is not news every five seconds.
//!
//! Hooks are fire-and-forget: they run off the cycle, cannot affect
//! synchronization, and cannot delay it. See [`Dispatcher`] for the rails
//! that keep that true.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A condition that warrants telling someone.
///
/// These are exactly the states `status` prints, so the hook named in the
/// configuration is `on_` plus the word on the screen. Two of them mean the
/// cycle ran and something in the tree needs a decision; three mean the
/// cycle did not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Alert {
    /// Both sides changed the same path. Needs a decision.
    Conflicts,
    /// Paths could not be read or written. Needs a filesystem fix.
    Blocked,
    /// A safety halt. Retrying will never clear it.
    Halted,
    /// The destination could not be reached. Usually clears itself.
    Unreachable,
    /// The cycle failed for some other reason.
    Errored,
}

impl Alert {
    /// The word `status` prints, and the suffix of the hook that fires for
    /// it.
    pub fn name(self) -> &'static str {
        match self {
            Alert::Conflicts => "conflicts",
            Alert::Blocked => "blocked",
            Alert::Halted => "halted",
            Alert::Unreachable => "unreachable",
            Alert::Errored => "errored",
        }
    }

    /// Every alert, for validating configuration keys against.
    pub fn all() -> [Alert; 5] {
        [
            Alert::Conflicts,
            Alert::Blocked,
            Alert::Halted,
            Alert::Unreachable,
            Alert::Errored,
        ]
    }

    /// Parses a configuration key.
    pub fn parse(name: &str) -> Option<Alert> {
        Alert::all().into_iter().find(|alert| alert.name() == name)
    }
}

/// The resolved alert configuration.
#[derive(Clone, Debug, Default)]
pub struct AlertPlan {
    /// Run when something needs a person. The only hook.
    pub on_alert: Option<String>,
    /// How long a condition must hold before it counts, per alert.
    pub after: BTreeMap<Alert, Duration>,
    /// How long a condition must hold before it counts, for alerts with no
    /// entry of their own.
    pub default_after: Duration,
    /// How often to fire again while the alerting set is unchanged. Zero
    /// never repeats, which is the default: a notification that returns
    /// while you are already working on it teaches you to ignore it.
    pub repeat_after: Duration,
    /// How long a condition must be *gone* before its return counts as
    /// news rather than as the same trouble continuing.
    pub settle_after: Duration,
    /// How long to hold a grown set before saying anything, so a cascade
    /// arrives as one notification instead of one per part.
    ///
    /// A confirmation period only groups conditions that begin together.
    /// A closing laptop does not do that: sessions go one at a time as
    /// each connection times out, every arrival changes the set, and every
    /// change was news. This window is what makes the difference between
    /// "three hosts went away" and three notifications.
    pub coalesce_after: Duration,
    /// How long a hook may run before it is killed.
    pub timeout: Duration,
}

impl AlertPlan {
    /// Whether anything is configured to run at all. Nothing is observed,
    /// timed, or recorded when nothing would come of it.
    pub fn is_configured(&self) -> bool {
        self.on_alert.is_some()
    }

    /// How long this alert must hold before it counts.
    pub fn after(&self, alert: Alert) -> Duration {
        self.after
            .get(&alert)
            .copied()
            .unwrap_or(self.default_after)
    }
}

/// One session's alerting conditions, as the supervisor sees them.
#[derive(Clone, Debug)]
pub struct SessionAlerts {
    pub group: String,
    pub host: String,
    /// The conditions it is currently in. Empty means healthy.
    pub alerts: Vec<Alert>,
    /// How to describe it in one line ("741 conflicts, 20 blocked").
    pub summary: String,
    /// How long its conditions must hold, when longer than their own
    /// patience: a halt that clears on its own waits like an error.
    pub after: Option<Duration>,
}

impl SessionAlerts {
    fn key(&self) -> String {
        format!("{}@{}", self.group, self.host)
    }
}

/// What the alerter decided to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fire {
    /// Sessions need attention. Carries the one-line summary and which
    /// alerts are present, for the hooks to be chosen and described.
    Alert {
        /// One line: the whole story when there is one thing to say, a
        /// count of things when there are several.
        summary: String,
        /// One indented line per thing, for a hook that can show more than
        /// a headline.
        detail: String,
        alerts: BTreeSet<Alert>,
        sessions: usize,
        /// A repeat of an unchanged set rather than news.
        repeat: bool,
    },
}

/// Decides when to fire, from what it is shown.
///
/// Deliberately pure: it is handed the current conditions and the time, and
/// returns what should run. Nothing here spawns a process, so the whole
/// policy — confirmation, coalescing, repetition, recovery — is testable
/// against a clock that the test moves itself.
pub struct Alerter {
    plan: AlertPlan,
    /// When each session's alert was first seen, uninterrupted.
    seen: HashMap<(String, Alert), Instant>,
    /// The set most recently fired for, and when.
    fired: BTreeSet<(String, Alert)>,
    fired_at: Option<Instant>,
    /// When the alerting set went empty, while the last firing is still
    /// remembered. `None` means either nothing has fired or it has settled.
    cleared_at: Option<Instant>,
    /// When something new first appeared that has not been said yet. The
    /// coalescing window runs from here, not from the latest arrival, so a
    /// cascade that keeps growing still reports on time.
    pending_since: Option<Instant>,
}

impl Alerter {
    pub fn new(plan: AlertPlan) -> Alerter {
        Alerter {
            plan,
            seen: HashMap::new(),
            fired: BTreeSet::new(),
            fired_at: None,
            cleared_at: None,
            pending_since: None,
        }
    }

    /// The configuration this alerter follows.
    pub fn plan(&self) -> &AlertPlan {
        &self.plan
    }

    /// Shows the alerter what every session is currently in, and asks what
    /// should run.
    pub fn observe(&mut self, sessions: &[SessionAlerts], now: Instant) -> Option<Fire> {
        // What is true this instant.
        let mut present: BTreeSet<(String, Alert)> = BTreeSet::new();
        let mut patience: BTreeMap<String, Duration> = BTreeMap::new();
        for session in sessions {
            for alert in &session.alerts {
                present.insert((session.key(), *alert));
            }
            if let Some(after) = session.after {
                patience.insert(session.key(), after);
            }
        }

        // A condition that lapsed, even for one cycle, starts its clock
        // again: it must hold *continuously* to count, or a host flapping
        // just below the confirmation period would eventually alert.
        self.seen.retain(|key, _| present.contains(key));
        for key in &present {
            self.seen.entry(key.clone()).or_insert(now);
        }

        // What has held long enough to be worth saying.
        let confirmed: BTreeSet<(String, Alert)> = present
            .iter()
            .filter(|key| {
                self.seen.get(*key).is_some_and(|since| {
                    let after = patience.get(&key.0).map_or(self.plan.after(key.1), |own| {
                        (*own).max(self.plan.after(key.1))
                    });
                    now.duration_since(*since) >= after
                })
            })
            .cloned()
            .collect();

        if confirmed.is_empty() {
            // Everything cleared. Nothing is said — an all-clear is a
            // notification that asks for nothing, and a stream of them is
            // what teaches someone to stop reading the ones that do.
            //
            // But the record is not dropped yet. A conflict on a file two
            // machines are both editing appears, clears, and returns all
            // day; clearing here made every return count as news, and one
            // flapping session produced a notification a minute. It has to
            // stay gone for `settle_after` before its return is news
            // again, which is the difference between "this is still going
            // on" and "this has come back".
            self.pending_since = None;
            match self.cleared_at {
                Some(at) if now.duration_since(at) >= self.plan.settle_after => {
                    self.fired.clear();
                    self.fired_at = None;
                    self.cleared_at = None;
                }
                None if !self.fired.is_empty() => self.cleared_at = Some(now),
                _ => {}
            }
            return None;
        }
        // Something is present again, so it never settled.
        self.cleared_at = None;

        // Only growth is news. `fired` is the high-water mark of the
        // episode, not the last set seen, so a session recovering while
        // others are still in trouble says nothing, and the same session
        // failing again says nothing either — it is the trouble that was
        // already reported, coming and going. A cascade recovering one
        // host at a time used to notify on the way back up as loudly as on
        // the way down.
        let grown = confirmed.difference(&self.fired).next().is_some();

        if !grown {
            // Nothing new. Silence, unless a repeat was asked for.
            if self.plan.repeat_after.is_zero() {
                return None;
            }
            let due = self
                .fired_at
                .is_some_and(|at| now.duration_since(at) >= self.plan.repeat_after);
            if !due {
                return None;
            }
            self.fired_at = Some(now);
            return Some(self.describe(sessions, &confirmed, true));
        }

        // Something new. Hold it, and let anything else arriving inside the
        // window join it. The clock runs from the first unreported arrival,
        // so a cascade that keeps growing is still reported one window
        // after it began rather than being deferred for as long as it lasts.
        let waiting_since = *self.pending_since.get_or_insert(now);
        if now.duration_since(waiting_since) < self.plan.coalesce_after {
            return None;
        }

        self.pending_since = None;
        self.fired.extend(confirmed.iter().cloned());
        self.fired_at = Some(now);
        Some(self.describe(sessions, &confirmed, false))
    }

    /// Builds the description of a firing from the sessions it covers.
    ///
    /// Two shapes of trouble, told two ways. A path-level condition — a
    /// conflict, a blocked path, a halt — belongs to a session and gets a
    /// line naming it, source to destination. A host-level condition —
    /// the machine is asleep, or refused the key — belongs to the host,
    /// and every session on it says the same thing; five lines for one
    /// sleeping laptop is noise, so they collapse to one line per host that
    /// says how many groups are waiting on it.
    fn describe(
        &self,
        sessions: &[SessionAlerts],
        confirmed: &BTreeSet<(String, Alert)>,
        repeat: bool,
    ) -> Fire {
        let keys: BTreeSet<&String> = confirmed.iter().map(|(key, _)| key).collect();
        let alerting: Vec<&SessionAlerts> = sessions
            .iter()
            .filter(|session| keys.contains(&session.key()))
            .collect();
        let hosts: Vec<&str> = alerting.iter().map(|s| s.host.as_str()).collect();

        // Host first, so a host's sessions collapse; the reason is the
        // first session's, and they all carry the same one.
        let mut away: std::collections::BTreeMap<&str, (usize, &str)> =
            std::collections::BTreeMap::new();
        let mut lines = Vec::new();
        let mut groups = BTreeSet::new();
        for session in &alerting {
            if session.alerts.contains(&Alert::Unreachable) {
                away.entry(session.host.as_str())
                    .or_insert((0, session.summary.as_str()))
                    .0 += 1;
            } else {
                groups.insert(session.group.as_str());
                lines.push(hook_line(&format!(
                    "{} → {}: {}",
                    session.group,
                    short_host(&session.host, &hosts),
                    session.summary
                )));
            }
        }
        let host_lines: Vec<String> = away
            .iter()
            .map(|(host, (count, reason))| {
                hook_line(&format!(
                    "{} {reason} — {} paused",
                    short_host(host, &hosts),
                    plural(*count, "group")
                ))
            })
            .collect();

        // One thing: say it. Several: count them, and let the detail name
        // them.
        let summary = match (lines.len(), host_lines.len()) {
            (1, 0) => lines[0].clone(),
            (0, 1) => host_lines[0].clone(),
            _ => {
                let mut parts = Vec::new();
                if !groups.is_empty() {
                    let verb = if groups.len() == 1 { "needs" } else { "need" };
                    parts.push(format!("{} {verb} you", plural(groups.len(), "group")));
                }
                if !host_lines.is_empty() {
                    parts.push(format!("{} away", plural(host_lines.len(), "host")));
                }
                parts.join(", ")
            }
        };
        // Each line is made safe as it is composed: a session's summary
        // carries error text, and error text carries names the other side
        // chose.
        let indented: Vec<String> = lines
            .iter()
            .chain(host_lines.iter())
            .map(|line| format!("  {line}"))
            .collect();
        let detail = hook_lines(indented.iter().map(String::as_str));
        Fire::Alert {
            summary,
            detail,
            alerts: confirmed.iter().map(|(_, alert)| *alert).collect(),
            sessions: keys.len(),
            repeat,
        }
    }

    /// The command a firing should run. One hook, so at most one command;
    /// which states are alerting is in the summary the hook is handed, not
    /// in which hook is chosen.
    pub fn commands(&self, _fire: &Fire) -> Vec<String> {
        self.plan.on_alert.iter().cloned().collect()
    }
}

/// `1 conflict`, `2 conflicts`. The one that used to read "1 conflicts" in
/// every alert.
pub fn plural(count: usize, word: &str) -> String {
    match count {
        1 => format!("1 {word}"),
        _ => format!("{count} {word}s"),
    }
}

/// A host name cut to its first label when that is enough to tell it from
/// the others in the same alert. `fny.voltai.party` reads as `fny`; a local
/// destination path is left alone, and so is a name whose first label
/// another host shares.
pub fn short_host(host: &str, all: &[&str]) -> String {
    // Only a name with a domain behind its first label has anything to
    // cut: `fny.voltai.party` is `fny`, but `faraz.vip` is already the
    // name, and cut to `faraz` it would read as a person.
    if host.contains('/') || host.matches('.').count() < 2 {
        return host.to_owned();
    }
    let Some((first, _)) = host.split_once('.') else {
        return host.to_owned();
    };
    let ambiguous = all
        .iter()
        .any(|other| *other != host && other.split('.').next() == Some(first));
    match ambiguous {
        true => host.to_owned(),
        false => first.to_owned(),
    }
}

/// Runs hook commands, off the cycle and under three rails.
///
/// A hook is someone else's program: it can hang, it can be slow, and it
/// can be wrong. None of that may reach synchronization, so a hook runs on
/// its own thread, is killed if it outstays the timeout, and is skipped
/// entirely while a previous one is still running — a hook that takes
/// longer than the interval must not accumulate a process per cycle. Its
/// failure is reported and then forgotten; there is no retry, because the
/// next change will fire again anyway.
pub struct Dispatcher {
    /// Whether a hook is running right now.
    running: Arc<AtomicBool>,
}

impl Default for Dispatcher {
    fn default() -> Dispatcher {
        Dispatcher {
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Dispatcher {
    /// Runs the commands for a firing. Returns false when a previous hook
    /// was still running and this one was therefore skipped.
    pub fn dispatch(
        &self,
        commands: Vec<String>,
        environment: Vec<(String, String)>,
        document: String,
        timeout: Duration,
    ) -> bool {
        if commands.is_empty() {
            return true;
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return false;
        }
        let running = self.running.clone();
        let environment = hook_environment(environment);
        std::thread::spawn(move || {
            for command in commands {
                if let Err(error) = run(&command, &environment, &document, timeout) {
                    eprintln!("alert hook failed: {error:#}");
                }
            }
            running.store(false, Ordering::SeqCst);
        });
        true
    }
}

/// Runs one hook command through the shell, giving it the report on
/// standard input and killing it if it outstays its welcome.
fn run(
    command: &str,
    environment: &[(String, String)],
    document: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("unable to run the alert hook {command:?}"))?;

    // The document goes in and the pipe closes, so a hook that reads to end
    // of file is not left waiting for one.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(document.as_bytes());
    }

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => anyhow::bail!("the alert hook {command:?} exited with {status}"),
            Ok(None) => {}
            Err(error) => return Err(error).context("unable to wait for the alert hook"),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "the alert hook {command:?} was killed after {} seconds",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The longest line a hook is handed, in bytes. A notification shows a
/// line or two; an error that runs to kilobytes says nothing more there.
pub const HOOK_LINE_MAX: usize = 400;

/// The most lines `AUTOBAHN_DETAIL` holds. The rest are counted, so a
/// hundred sessions failing together stay well inside what one
/// environment variable may hold.
pub const HOOK_DETAIL_LINES: usize = 50;

/// One line of text for a hook: control characters escaped, so it stays
/// one line and cannot steer a terminal that shows it, and cut to
/// [`HOOK_LINE_MAX`]. Quotes are left alone: a hook that passes the text
/// as data, as it should, would only see them garbled.
pub fn hook_line(text: &str) -> String {
    let safe = crate::text::display_safe(text);
    crate::text::cap_line(&safe, HOOK_LINE_MAX).into_owned()
}

/// Several lines for a hook, each made safe by [`hook_line`], and no more
/// than [`HOOK_DETAIL_LINES`] of them.
pub fn hook_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    let lines: Vec<&str> = lines.into_iter().collect();
    let mut kept: Vec<String> = lines
        .iter()
        .take(HOOK_DETAIL_LINES)
        .map(|line| hook_line(line))
        .collect();
    if lines.len() > HOOK_DETAIL_LINES {
        kept.push(format!("  … and {} more", lines.len() - HOOK_DETAIL_LINES));
    }
    kept.join("\n")
}

/// The environment a hook is handed, with the summary and detail made safe
/// whoever composed them. The alerter already composes them safely; this
/// holds for every other caller, such as the refused-configuration alert.
pub fn hook_environment(environment: Vec<(String, String)>) -> Vec<(String, String)> {
    environment
        .into_iter()
        .map(|(key, value)| {
            let value = match key.as_str() {
                "AUTOBAHN_SUMMARY" => hook_line(&value),
                "AUTOBAHN_DETAIL" => hook_lines(value.split('\n')),
                _ => value,
            };
            (key, value)
        })
        .collect()
}

/// What [`refresh_example_hook`] found at the hook `on_alert` names.
#[derive(Debug, PartialEq, Eq)]
pub enum ExampleHook {
    /// An example exactly as an earlier `init` wrote it, now replaced by
    /// the current one.
    Rewritten(std::path::PathBuf),
    /// A script of someone's own that still has the example's unsafe
    /// AppleScript line. Left alone, and worth a warning.
    Unsafe(std::path::PathBuf),
    /// An example that should have been replaced and could not be.
    Failed(std::path::PathBuf, String),
    /// Anything else: the current example, someone's own script, a
    /// command line, or nothing at all.
    Other,
}

/// Brings an example hook written by an earlier `init` up to date.
///
/// Early examples put `$AUTOBAHN_SUMMARY` inside AppleScript source, so a
/// file name chosen by the other side could run a command. `init` never
/// replaces a script, and a hook is a script its owner may have made their
/// own; but one that matches a shipped example byte for byte was never
/// edited, and is replaced. Anything that differs is left alone.
pub fn refresh_example_hook(on_alert: &str) -> ExampleHook {
    match std::env::var_os("HOME") {
        Some(home) => refresh_example_hook_in(on_alert, std::path::Path::new(&home)),
        None => refresh_example_hook_in(on_alert, std::path::Path::new("")),
    }
}

/// [`refresh_example_hook`], with `~` meaning `home`.
fn refresh_example_hook_in(on_alert: &str, home: &std::path::Path) -> ExampleHook {
    use std::path::{Path, PathBuf};

    // Only a hook that is one path, as `init`'s configuration names it,
    // can be the example; a command line is someone's own.
    let command = on_alert.trim();
    if command.is_empty() || command.contains(char::is_whitespace) {
        return ExampleHook::Other;
    }
    let path: PathBuf = match command.strip_prefix("~/") {
        Some(rest) if home.is_absolute() => home.join(rest),
        Some(_) => return ExampleHook::Other,
        None if Path::new(command).is_absolute() => PathBuf::from(command),
        None => return ExampleHook::Other,
    };
    let Ok(contents) = std::fs::read(&path) else {
        return ExampleHook::Other;
    };
    let current = crate::config::ON_ALERT_EXAMPLE.as_bytes();
    if contents == current {
        return ExampleHook::Other;
    }
    if !SHIPPED_ON_ALERT_EXAMPLES
        .iter()
        .any(|shipped| shipped.as_bytes() == contents)
    {
        return match contents
            .windows(UNSAFE_OSASCRIPT_LINE.len())
            .any(|window| window == UNSAFE_OSASCRIPT_LINE.as_bytes())
        {
            true => ExampleHook::Unsafe(path),
            false => ExampleHook::Other,
        };
    }
    match replace_file(&path, current) {
        Ok(()) => ExampleHook::Rewritten(path),
        Err(error) => ExampleHook::Failed(path, format!("{error:#}")),
    }
}

/// Replaces the file at `path`, or the file a link there points at, with
/// `contents`, keeping its permissions. Written beside it and renamed, so
/// a hook starting meanwhile runs one version or the other, never half.
fn replace_file(path: &std::path::Path, contents: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;

    let target = std::fs::canonicalize(path)
        .with_context(|| format!("unable to resolve {}", path.display()))?;
    let permissions = std::fs::metadata(&target)
        .with_context(|| format!("unable to read {}", target.display()))?
        .permissions();
    let name = target
        .file_name()
        .context("the hook has no file name")?
        .to_string_lossy();
    let temporary = target.with_file_name(format!(".{name}.autobahn-new"));
    let _ = std::fs::remove_file(&temporary);
    let result = (|| -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("unable to create {}", temporary.display()))?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::set_permissions(&temporary, permissions)?;
        std::fs::rename(&temporary, &target)
            .with_context(|| format!("unable to move {} into place", temporary.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// At startup: brings the configured hook up to date if it is an unedited
/// shipped example, and warns once if it is someone's own script with the
/// example's unsafe line in it.
pub fn refresh_configured_example_hook(plan: &AlertPlan) {
    let Some(command) = plan.on_alert.as_deref() else {
        return;
    };
    match refresh_example_hook(command) {
        ExampleHook::Rewritten(path) => crate::note!(
            "rewrote the alert hook {}: it was an earlier example, which put the summary \
             inside AppleScript",
            path.display()
        ),
        ExampleHook::Unsafe(path) => crate::complain!(
            "warning: the alert hook {} puts $AUTOBAHN_SUMMARY inside AppleScript source, \
             where a file name can run a command; hand it to osascript as an argument, as \
             `autobahn init`'s example now does",
            path.display()
        ),
        ExampleHook::Failed(path, error) => crate::complain!(
            "unable to rewrite the alert hook {}, an earlier example with an unsafe \
             AppleScript line: {error}",
            path.display()
        ),
        ExampleHook::Other => {}
    }
}

/// The line of the early example hooks that made the summary AppleScript.
const UNSAFE_OSASCRIPT_LINE: &str = r#"-e "display notification \"$AUTOBAHN_SUMMARY\""#;

/// Every example hook `init` has written, exactly, oldest first. A file
/// matching one byte for byte is ours to replace; see
/// [`refresh_example_hook`]. Never edit these: add the next one.
const SHIPPED_ON_ALERT_EXAMPLES: &[&str] = &[
    // 0.4: interpolated the summary into AppleScript source.
    r##"#!/bin/sh
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

    # Built in, and always there. It holds one line and no click.
    exec /usr/bin/osascript \
        -e "display notification \"$AUTOBAHN_SUMMARY\" with title \"autobahn\""
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
"##,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> AlertPlan {
        AlertPlan {
            on_alert: Some("notify".into()),
            default_after: Duration::from_secs(30),
            timeout: Duration::from_secs(10),
            settle_after: Duration::from_secs(15 * 60),
            // Existing cases predate coalescing and test the other rules;
            // the window has its own tests below.
            coalesce_after: Duration::ZERO,
            ..AlertPlan::default()
        }
    }

    fn session(host: &str, alerts: &[Alert]) -> SessionAlerts {
        session_in("work", host, alerts, "summary")
    }

    fn session_in(group: &str, host: &str, alerts: &[Alert], summary: &str) -> SessionAlerts {
        SessionAlerts {
            after: None,
            group: group.into(),
            host: host.into(),
            alerts: alerts.to_vec(),
            summary: summary.into(),
        }
    }

    /// Five sessions on one sleeping laptop are one fact, not five.
    #[test]
    fn a_host_that_is_away_is_one_line_however_many_groups_wait_on_it() {
        let mut alerter = Alerter::new(plan());
        let start = Instant::now();
        let sessions = [
            session_in("aws", "boite", &[Alert::Unreachable], "is unreachable"),
            session_in("shared", "boite", &[Alert::Unreachable], "is unreachable"),
            session_in("vibe", "boite", &[Alert::Unreachable], "is unreachable"),
        ];
        alerter.observe(&sessions, start);
        let Some(Fire::Alert {
            summary,
            detail,
            sessions,
            ..
        }) = alerter.observe(&sessions, start + Duration::from_secs(31))
        else {
            panic!("expected an alert");
        };
        assert_eq!(summary, "boite is unreachable — 3 groups paused");
        assert_eq!(detail, "  boite is unreachable — 3 groups paused");
        assert_eq!(sessions, 3);
    }

    /// A single thing is said outright; several are counted, and the
    /// detail names each one, source to destination, with the host cut to
    /// what tells it apart.
    #[test]
    fn several_things_are_counted_in_the_headline_and_named_in_the_detail() {
        let mut alerter = Alerter::new(plan());
        let start = Instant::now();
        let sessions = [
            session_in(
                "voltai",
                "fny.voltai.party",
                &[Alert::Conflicts],
                "1 conflict",
            ),
            session_in("vibe", "faraz.vip", &[Alert::Conflicts], "57 conflicts"),
            session_in("aws", "boite", &[Alert::Unreachable], "refused the key"),
        ];
        alerter.observe(&sessions, start);
        let Some(Fire::Alert {
            summary, detail, ..
        }) = alerter.observe(&sessions, start + Duration::from_secs(31))
        else {
            panic!("expected an alert");
        };
        assert_eq!(summary, "2 groups need you, 1 host away");
        assert_eq!(
            detail,
            "  voltai → fny: 1 conflict\n  vibe → faraz.vip: 57 conflicts\n  boite refused the key — 1 group paused"
        );

        // And one thing alone is the whole headline.
        let mut alerter = Alerter::new(plan());
        let one = [session_in(
            "voltai",
            "fny.voltai.party",
            &[Alert::Blocked],
            "21 blocked paths",
        )];
        alerter.observe(&one, start);
        let Some(Fire::Alert { summary, .. }) =
            alerter.observe(&one, start + Duration::from_secs(31))
        else {
            panic!("expected an alert");
        };
        assert_eq!(summary, "voltai → fny: 21 blocked paths");
    }

    #[test]
    fn words_are_counted_correctly_and_hosts_are_cut_only_when_unambiguous() {
        assert_eq!(plural(1, "conflict"), "1 conflict");
        assert_eq!(plural(2, "conflict"), "2 conflicts");
        assert_eq!(plural(0, "group"), "0 groups");
        assert_eq!(
            short_host("fny.voltai.party", &["fny.voltai.party", "boite"]),
            "fny"
        );
        assert_eq!(short_host("boite", &["boite"]), "boite");
        assert_eq!(short_host("faraz.vip", &["faraz.vip"]), "faraz.vip");
        assert_eq!(short_host("/Users/x/beta.d", &[]), "/Users/x/beta.d");
        // Two hosts sharing a first label keep their full names.
        assert_eq!(
            short_host("fny.voltai.party", &["fny.voltai.party", "fny.example.org"]),
            "fny.voltai.party"
        );
    }

    #[test]
    fn nothing_wrong_means_nothing_happens() {
        let mut alerter = Alerter::new(plan());
        let now = Instant::now();
        // Healthy, and healthy for a long time, is silence — and a
        // recovery is never announced for an alert that never fired.
        for minutes in 0..10 {
            let now = now + Duration::from_secs(minutes * 60);
            assert_eq!(alerter.observe(&[session("a", &[])], now), None);
        }
    }

    #[test]
    fn a_condition_must_hold_before_it_counts() {
        let mut alerter = Alerter::new(plan());
        let start = Instant::now();
        let sessions = [session("a", &[Alert::Unreachable])];

        // Inside the confirmation period, nothing is said.
        assert_eq!(alerter.observe(&sessions, start), None);
        assert_eq!(
            alerter.observe(&sessions, start + Duration::from_secs(29)),
            None
        );
        // Once it has held, it fires — once.
        let fire = alerter
            .observe(&sessions, start + Duration::from_secs(30))
            .expect("a held condition alerts");
        assert!(matches!(
            fire,
            Fire::Alert {
                sessions: 1,
                repeat: false,
                ..
            }
        ));
        assert_eq!(
            alerter.observe(&sessions, start + Duration::from_secs(60)),
            None
        );
        assert_eq!(
            alerter.observe(&sessions, start + Duration::from_secs(600)),
            None
        );
    }

    #[test]
    fn a_blip_shorter_than_the_confirmation_period_is_never_mentioned() {
        // The case the confirmation period exists for: a wifi handover
        // takes every session unreachable for a few seconds. Alerting on
        // the edge would page, then page again to say it recovered, every
        // time someone walks between rooms.
        let mut alerter = Alerter::new(plan());
        let start = Instant::now();
        let down = [session("a", &[Alert::Unreachable])];
        let up = [session("a", &[])];

        assert_eq!(alerter.observe(&down, start), None);
        assert_eq!(alerter.observe(&down, start + Duration::from_secs(8)), None);
        assert_eq!(alerter.observe(&up, start + Duration::from_secs(9)), None);
        // And nothing lingers: the next outage starts its own clock rather
        // than inheriting the last one's.
        assert_eq!(
            alerter.observe(&down, start + Duration::from_secs(10)),
            None
        );
        assert_eq!(
            alerter.observe(&down, start + Duration::from_secs(39)),
            None
        );
        assert!(alerter
            .observe(&down, start + Duration::from_secs(40))
            .is_some());
    }

    fn coalescing_plan(window: Duration) -> AlertPlan {
        AlertPlan {
            coalesce_after: window,
            ..plan()
        }
    }

    /// The closing-laptop case. Sessions do not go together: each one goes
    /// when its own connection times out, seconds apart. Every arrival
    /// changed the set, and every change was news, so one closing lid
    /// produced a notification per session.
    #[test]
    fn a_cascade_that_arrives_over_time_is_one_notification() {
        let mut alerter = Alerter::new(coalescing_plan(Duration::from_secs(60)));
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let away = |hosts: &[&str]| -> Vec<SessionAlerts> {
            hosts
                .iter()
                .map(|host| session(host, &[Alert::Unreachable]))
                .collect()
        };

        // Hosts go one at a time, each confirmed 30 seconds after it goes.
        assert_eq!(alerter.observe(&away(&["a"]), at(0)), None);
        assert_eq!(
            alerter.observe(&away(&["a"]), at(31)),
            None,
            "held, not sent"
        );
        assert_eq!(alerter.observe(&away(&["a", "b"]), at(40)), None);
        assert_eq!(alerter.observe(&away(&["a", "b", "c"]), at(50)), None);

        // One notification, one window after the first arrival, naming all
        // three — including the ones that arrived while it was held.
        let fire = alerter
            .observe(&away(&["a", "b", "c"]), at(91))
            .expect("the window closes");
        let Fire::Alert {
            sessions, summary, ..
        } = fire;
        assert_eq!(sessions, 3);
        assert_eq!(summary, "3 hosts away");

        // And nothing more for the same trouble.
        assert_eq!(alerter.observe(&away(&["a", "b", "c"]), at(200)), None);
    }

    /// Waking the laptop brings the sessions back one at a time, the same
    /// way they left. A shrinking set asks nothing of anyone, so it says
    /// nothing — and a host that drops again is the trouble already
    /// reported, not new trouble.
    #[test]
    fn recovery_is_silent_and_a_relapse_is_not_news() {
        let mut alerter = Alerter::new(coalescing_plan(Duration::ZERO));
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let away = |hosts: &[&str]| -> Vec<SessionAlerts> {
            hosts
                .iter()
                .map(|host| session(host, &[Alert::Unreachable]))
                .collect()
        };

        alerter.observe(&away(&["a", "b"]), at(0));
        assert!(alerter.observe(&away(&["a", "b"]), at(31)).is_some());

        // Coming back, one at a time: silence.
        assert_eq!(alerter.observe(&away(&["a"]), at(40)), None);
        // Going again, before everything has settled: still the same
        // trouble, so still silence.
        assert_eq!(alerter.observe(&away(&["a", "b"]), at(50)), None);
        assert_eq!(alerter.observe(&away(&["a", "b"]), at(90)), None);
    }

    /// The window groups what is already happening; it does not defer a
    /// notification behind a cascade that keeps going. The clock runs from
    /// the first arrival that has not been reported, not from the latest,
    /// so a steady trickle cannot hold it back indefinitely.
    #[test]
    fn the_window_runs_from_the_first_arrival_not_the_latest() {
        let mut alerter = Alerter::new(coalescing_plan(Duration::from_secs(60)));
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let away = |count: usize| -> Vec<SessionAlerts> {
            ["a", "b", "c", "d", "e"][..count]
                .iter()
                .map(|host| session(host, &[Alert::Unreachable]))
                .collect()
        };

        // A host goes every 20 seconds and none of them come back. With
        // `alert_after` at 30s, the first is confirmed at t=40, which is
        // when the window opens.
        assert_eq!(alerter.observe(&away(1), at(0)), None);
        assert_eq!(alerter.observe(&away(2), at(20)), None);
        assert_eq!(alerter.observe(&away(3), at(40)), None, "window opens here");
        assert_eq!(alerter.observe(&away(4), at(60)), None);
        assert_eq!(alerter.observe(&away(5), at(80)), None);

        // It closes 60 seconds after it opened, even though hosts are
        // still arriving, and reports everything confirmed by then.
        let fire = alerter
            .observe(&away(5), at(101))
            .expect("the window closes");
        let Fire::Alert { sessions, .. } = fire;
        assert_eq!(sessions, 4, "the four confirmed by t=101");
    }

    #[test]
    fn a_burst_is_one_alert_rather_than_fifteen() {
        // A laptop sleeping takes every session with it. What arrives
        // inside one confirmation period is one call describing all of it.
        let mut alerter = Alerter::new(plan());
        let start = Instant::now();
        let all: Vec<SessionAlerts> = ["a", "b", "c"]
            .iter()
            .map(|host| session(host, &[Alert::Unreachable]))
            .collect();

        assert_eq!(alerter.observe(&all, start), None);
        let fire = alerter
            .observe(&all, start + Duration::from_secs(31))
            .expect("the batch alerts");
        match fire {
            Fire::Alert {
                sessions,
                summary,
                detail,
                ..
            } => {
                assert_eq!(sessions, 3);
                // Three hosts away at once: counted in the headline, one
                // line each in the detail.
                assert_eq!(summary, "3 hosts away");
                assert_eq!(detail.lines().count(), 3);
            }
        }
    }

    #[test]
    fn a_changed_set_is_news_and_an_unchanged_one_is_not() {
        let mut alerter = Alerter::new(plan());
        let start = Instant::now();
        let one = [session("a", &[Alert::Conflicts])];
        let two = [
            session("a", &[Alert::Conflicts]),
            session("b", &[Alert::Halted]),
        ];

        assert!(alerter
            .observe(&one, start + Duration::from_secs(30))
            .is_none());
        assert!(alerter
            .observe(&one, start + Duration::from_secs(31))
            .is_none_or(|_| true));
        // First alert.
        let mut alerter = Alerter::new(plan());
        alerter.observe(&one, start);
        assert!(alerter
            .observe(&one, start + Duration::from_secs(31))
            .is_some());
        // Unchanged: silence.
        assert_eq!(alerter.observe(&one, start + Duration::from_secs(60)), None);
        // A second session joining is new information.
        alerter.observe(&two, start + Duration::from_secs(61));
        let fire = alerter
            .observe(&two, start + Duration::from_secs(95))
            .expect("a new session in trouble is news");
        match fire {
            Fire::Alert {
                sessions, alerts, ..
            } => {
                assert_eq!(sessions, 2);
                assert!(alerts.contains(&Alert::Halted));
            }
        }
    }

    /// A conflict on a file two machines are both editing appears, clears,
    /// and returns all day. Reported once: the second arrival is the same
    /// trouble continuing, not news. Before this, one flapping session
    /// produced a notification a minute.
    #[test]
    fn trouble_that_comes_and_goes_is_reported_once() {
        let mut alerter = Alerter::new(plan());
        let start = Instant::now();
        let down = [session("boite", &[Alert::Conflicts])];
        let up = [session("boite", &[])];
        let at = |seconds: u64| start + Duration::from_secs(seconds);

        alerter.observe(&down, at(0));
        assert!(
            alerter.observe(&down, at(31)).is_some(),
            "the first is news"
        );
        // Three full flaps inside the settling period say nothing.
        for cycle in 0..3 {
            let base = 60 + cycle * 120;
            assert_eq!(alerter.observe(&up, at(base)), None);
            alerter.observe(&down, at(base + 30));
            assert_eq!(
                alerter.observe(&down, at(base + 61)),
                None,
                "flap {cycle} was announced again"
            );
        }
        // Gone long enough to have settled, its return is news again.
        assert_eq!(alerter.observe(&up, at(1_000)), None);
        alerter.observe(&down, at(2_000));
        assert!(
            alerter.observe(&down, at(2_031)).is_some(),
            "trouble returning after it settled is news"
        );
    }

    #[test]
    fn clearing_says_nothing_and_resets_only_once_it_has_settled() {
        let mut alerter = Alerter::new(plan());
        let start = Instant::now();
        let down = [session("a", &[Alert::Conflicts])];
        let up = [session("a", &[])];

        alerter.observe(&down, start);
        assert!(alerter
            .observe(&down, start + Duration::from_secs(31))
            .is_some());
        // Clearing says nothing. An all-clear asks for no action, and a
        // stream of notifications that ask for nothing is what teaches
        // someone to stop reading the ones that do.
        assert_eq!(alerter.observe(&up, start + Duration::from_secs(40)), None);
        assert_eq!(alerter.observe(&up, start + Duration::from_secs(50)), None);
        // And the state is reset once it has stayed clear — trouble that
        // returns before then is the same trouble, and says nothing. See
        // `trouble_that_comes_and_goes_is_reported_once`.
        alerter.observe(&down, start + Duration::from_secs(1_000));
        assert!(alerter
            .observe(&down, start + Duration::from_secs(1_031))
            .is_some());
    }

    #[test]
    fn repeating_is_off_unless_it_is_asked_for() {
        let start = Instant::now();
        let down = [session("a", &[Alert::Halted])];

        // The default never nags.
        let mut quiet = Alerter::new(plan());
        quiet.observe(&down, start);
        assert!(quiet
            .observe(&down, start + Duration::from_secs(31))
            .is_some());
        assert_eq!(
            quiet.observe(&down, start + Duration::from_secs(3_600)),
            None
        );

        // Asked for, it returns on schedule and says it is a repeat.
        let mut nagging = Alerter::new(AlertPlan {
            repeat_after: Duration::from_secs(600),
            ..plan()
        });
        nagging.observe(&down, start);
        assert!(nagging
            .observe(&down, start + Duration::from_secs(31))
            .is_some());
        assert_eq!(
            nagging.observe(&down, start + Duration::from_secs(300)),
            None
        );
        assert!(matches!(
            nagging.observe(&down, start + Duration::from_secs(700)),
            Some(Fire::Alert { repeat: true, .. })
        ));
    }

    #[test]
    fn each_alert_can_hold_for_its_own_period() {
        // An unreachable host is usually a sleeping laptop and deserves
        // patience; a safety halt is never transient and deserves none.
        let mut alerter = Alerter::new(AlertPlan {
            after: BTreeMap::from([
                (Alert::Unreachable, Duration::from_secs(300)),
                (Alert::Halted, Duration::ZERO),
            ]),
            ..plan()
        });
        let start = Instant::now();
        let both = [
            session("a", &[Alert::Unreachable]),
            session("b", &[Alert::Halted]),
        ];

        // The halt alerts immediately, alone.
        let fire = alerter
            .observe(&both, start)
            .expect("the halt alerts at once");
        match fire {
            Fire::Alert {
                alerts, sessions, ..
            } => {
                assert_eq!(sessions, 1);
                assert_eq!(alerts, BTreeSet::from([Alert::Halted]));
            }
        }
        // The unreachable host joins only once its own period has passed.
        assert_eq!(
            alerter.observe(&both, start + Duration::from_secs(299)),
            None
        );
        assert!(matches!(
            alerter.observe(&both, start + Duration::from_secs(301)),
            Some(Fire::Alert { sessions: 2, .. })
        ));
    }

    #[test]
    fn one_hook_runs_whatever_the_alert_is() {
        let alerter = Alerter::new(plan());
        let fire = |alert| Fire::Alert {
            summary: String::new(),
            detail: String::new(),
            alerts: BTreeSet::from([alert]),
            sessions: 1,
            repeat: false,
        };
        // Which states are alerting is in the summary the hook is handed,
        // not in which hook is chosen. There is only the one.
        assert_eq!(alerter.commands(&fire(Alert::Halted)), vec!["notify"]);
        assert_eq!(alerter.commands(&fire(Alert::Conflicts)), vec!["notify"]);
        // And nothing at all when none is configured.
        let quiet = Alerter::new(AlertPlan::default());
        assert!(quiet.commands(&fire(Alert::Halted)).is_empty());
    }

    #[test]
    fn a_hook_that_hangs_is_killed_rather_than_waited_on() {
        let started = Instant::now();
        let error = run("sleep 60", &[], "", Duration::from_millis(300))
            .expect_err("a hook that outstays its timeout fails");
        assert!(format!("{error:#}").contains("killed"), "{error:#}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout must not wait for the hook"
        );
    }

    #[test]
    fn a_hook_receives_the_summary_and_the_report() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let output = directory.path().join("out");
        run(
            &format!(
                "cat > {path}.stdin; printf '%s' \"$AUTOBAHN_SUMMARY\" > {path}",
                path = output.display()
            ),
            &[("AUTOBAHN_SUMMARY".into(), "work@a: 3 conflicts".into())],
            "{\"version\":2}",
            Duration::from_secs(10),
        )
        .expect("the hook runs");
        assert_eq!(
            std::fs::read_to_string(&output).expect("the hook wrote"),
            "work@a: 3 conflicts"
        );
        assert_eq!(
            std::fs::read_to_string(output.with_extension("stdin")).expect("the hook read stdin"),
            "{\"version\":2}"
        );
    }

    #[test]
    fn a_hook_still_running_is_not_launched_again() {
        // A hook slower than the cycle must not accumulate one process per
        // cycle; the firing is dropped instead, and the next change fires
        // again anyway.
        let dispatcher = Dispatcher::default();
        assert!(dispatcher.dispatch(
            vec!["sleep 2".into()],
            Vec::new(),
            String::new(),
            Duration::from_secs(10)
        ));
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !dispatcher.dispatch(
                vec!["true".into()],
                Vec::new(),
                String::new(),
                Duration::from_secs(10)
            ),
            "a second hook must be skipped while the first runs"
        );
    }

    #[test]
    fn a_session_that_asks_for_patience_waits_for_it() {
        // A halt is announced at once; a missing alpha, which is a halt
        // that clears on its own, waits as long as its session asks.
        let mut alerter = Alerter::new(AlertPlan {
            after: BTreeMap::from([(Alert::Halted, Duration::ZERO)]),
            ..plan()
        });
        let mut waiting = session("a", &[Alert::Halted]);
        waiting.after = Some(Duration::from_secs(120));
        let start = Instant::now();
        assert!(alerter.observe(&[waiting.clone()], start).is_none());
        assert!(alerter
            .observe(&[waiting.clone()], start + Duration::from_secs(119))
            .is_none());
        assert!(alerter
            .observe(&[waiting], start + Duration::from_secs(120))
            .is_some());

        let mut alerter = Alerter::new(AlertPlan {
            after: BTreeMap::from([(Alert::Halted, Duration::ZERO)]),
            ..plan()
        });
        assert!(alerter
            .observe(&[session("b", &[Alert::Halted])], start)
            .is_some());
    }

    /// Runs the example hook's macOS fallback, with `osascript` replaced
    /// by a stub that writes each argument it receives, NUL-terminated.
    fn osascript_arguments(summary: &str) -> Vec<String> {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let stub = directory.path().join("osascript");
        let received = directory.path().join("received");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done > {}\n",
                crate::text::shell_quote(&received.display().to_string())
            ),
        )
        .expect("the stub is written");
        std::fs::set_permissions(&stub, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("the stub is executable");
        // Pretend to be a Mac with no terminal-notifier. Each substitution
        // must find its target, or the test is not running the example.
        let mut script = crate::config::ON_ALERT_EXAMPLE.to_owned();
        for (from, to) in [
            ("\"$(uname -s)\"", "Darwin".to_owned()),
            ("/usr/bin/osascript", stub.display().to_string()),
            (
                "/opt/homebrew/bin/terminal-notifier",
                directory.path().join("absent-1").display().to_string(),
            ),
            (
                "/usr/local/bin/terminal-notifier",
                directory.path().join("absent-2").display().to_string(),
            ),
        ] {
            assert!(script.contains(from), "the example no longer names {from}");
            script = script.replace(from, &to);
        }
        let hook = directory.path().join("on-alert.sh");
        std::fs::write(&hook, script).expect("the hook is written");
        let status = std::process::Command::new("sh")
            .arg(&hook)
            .current_dir(directory.path())
            .env("AUTOBAHN_SUMMARY", summary)
            .env("AUTOBAHN_DETAIL", "  detail")
            .env("AUTOBAHN_ICON", "/nonexistent/icon.png")
            .status()
            .expect("the hook runs");
        assert!(status.success(), "the hook failed: {status}");
        let received = std::fs::read(&received).expect("the stub ran");
        String::from_utf8(received)
            .expect("utf-8 arguments")
            .split_terminator('\0')
            .map(str::to_owned)
            .collect()
    }

    /// A summary is text, and the example hands it to AppleScript as an
    /// argument: a name built to close the string and run a shell command
    /// arrives whole, and the script it is shown by is the same whatever
    /// it says.
    #[test]
    fn the_example_hook_hands_the_summary_to_applescript_as_data() {
        let hostile = r#"x" & (do shell script "touch pwned") & "\ back"#;
        let arguments = osascript_arguments(hostile);
        let plain = osascript_arguments("3 conflicts");
        assert_eq!(arguments.last().map(String::as_str), Some(hostile));
        assert_eq!(plain.last().map(String::as_str), Some("3 conflicts"));
        assert_eq!(
            arguments[..arguments.len() - 1],
            plain[..plain.len() - 1],
            "the AppleScript must not change with the summary"
        );
        assert!(arguments[..arguments.len() - 1]
            .iter()
            .all(|argument| !argument.contains("do shell script")));
    }

    /// A session's text comes partly from the other side. What the hook is
    /// handed has no control characters left in it, so a hook that echoes
    /// it into a terminal or a log cannot be steered by it, and it is
    /// bounded, however long the error was.
    #[test]
    fn a_composed_alert_carries_no_control_characters_and_is_bounded() {
        let mut alerter = Alerter::new(plan());
        let start = Instant::now();
        let hostile = format!(
            "errored: a\u{1b}]52;c;Zm9v\u{7}b\nforged line{}",
            "x".repeat(1_000)
        );
        let sessions = [
            session_in("work", "boite", &[Alert::Errored], &hostile),
            session_in("play\u{1b}[2J", "boite", &[Alert::Errored], "fine"),
        ];
        alerter.observe(&sessions, start);
        let Some(Fire::Alert {
            summary, detail, ..
        }) = alerter.observe(&sessions, start + Duration::from_secs(31))
        else {
            panic!("expected an alert");
        };
        assert!(!summary.chars().any(char::is_control), "{summary:?}");
        let lines: Vec<&str> = detail.split('\n').collect();
        assert_eq!(lines.len(), 2, "one line per session: {detail:?}");
        for line in lines {
            assert!(!line.chars().any(char::is_control), "{line:?}");
            assert!(line.len() <= HOOK_LINE_MAX, "{} bytes", line.len());
        }
        assert!(detail.contains("\\x1b"), "escaped, not dropped: {detail:?}");

        // A single session is the whole headline, and bounded the same way.
        let mut alerter = Alerter::new(plan());
        let one = [session_in("work", "boite", &[Alert::Errored], &hostile)];
        alerter.observe(&one, start);
        let Some(Fire::Alert { summary, .. }) =
            alerter.observe(&one, start + Duration::from_secs(31))
        else {
            panic!("expected an alert");
        };
        assert!(!summary.chars().any(char::is_control), "{summary:?}");
        assert!(summary.len() <= HOOK_LINE_MAX, "{} bytes", summary.len());
    }

    /// Whoever composed them, the summary and detail reach a hook clean:
    /// the dispatcher sanitizes them too, keeping the detail's line breaks.
    #[test]
    fn the_dispatcher_hands_a_hook_clean_text() {
        let environment = hook_environment(vec![
            ("AUTOBAHN_SUMMARY".into(), "a\u{1b}[31m\nb".into()),
            ("AUTOBAHN_DETAIL".into(), "  one\u{7}\n  two".into()),
            ("AUTOBAHN_ICON".into(), "/icon.png".into()),
        ]);
        assert_eq!(
            environment,
            vec![
                ("AUTOBAHN_SUMMARY".to_owned(), "a\\x1b[31m\\nb".to_owned()),
                ("AUTOBAHN_DETAIL".to_owned(), "  one\\x07\n  two".to_owned()),
                ("AUTOBAHN_ICON".to_owned(), "/icon.png".to_owned()),
            ]
        );
    }

    /// The example hook exactly as `init` wrote it before its AppleScript
    /// took the summary as data.
    fn shipped_example() -> &'static str {
        SHIPPED_ON_ALERT_EXAMPLES[0]
    }

    #[test]
    fn a_shipped_example_hook_is_rewritten_and_an_edited_one_is_not() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("a temporary directory");
        let hook = directory.path().join("on-alert.sh");
        let command = hook.display().to_string();

        // The shipped copy, untouched: replaced, and still executable.
        std::fs::write(&hook, shipped_example()).expect("written");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o750)).expect("chmod");
        assert_eq!(
            refresh_example_hook(&command),
            ExampleHook::Rewritten(hook.clone())
        );
        assert_eq!(
            std::fs::read_to_string(&hook).expect("read"),
            crate::config::ON_ALERT_EXAMPLE
        );
        assert_eq!(
            std::fs::metadata(&hook).expect("stat").permissions().mode() & 0o777,
            0o750
        );
        // Now current, so left alone.
        assert_eq!(refresh_example_hook(&command), ExampleHook::Other);

        // Named through `~`, as `init`'s configuration names it: the same.
        let under_home = format!("~/{}", hook.file_name().unwrap().to_string_lossy());
        std::fs::write(&hook, shipped_example()).expect("written");
        assert_eq!(
            refresh_example_hook_in(&under_home, directory.path()),
            ExampleHook::Rewritten(hook.clone())
        );

        // Edited, even by one byte: never touched, but warned about.
        let edited = format!("{}# mine\n", shipped_example());
        std::fs::write(&hook, &edited).expect("written");
        assert_eq!(
            refresh_example_hook(&command),
            ExampleHook::Unsafe(hook.clone())
        );
        assert_eq!(std::fs::read_to_string(&hook).expect("read"), edited);

        // A hook that is a command line rather than a file: not ours.
        assert_eq!(
            refresh_example_hook("terminal-notifier -message \"$AUTOBAHN_SUMMARY\""),
            ExampleHook::Other
        );
        assert_eq!(
            refresh_example_hook(&directory.path().join("absent").display().to_string()),
            ExampleHook::Other
        );
    }

    /// The copy kept for recognition is the one `init` wrote.
    #[test]
    fn the_shipped_example_is_the_unsafe_one() {
        assert!(shipped_example().contains(UNSAFE_OSASCRIPT_LINE));
        assert!(!crate::config::ON_ALERT_EXAMPLE.contains(UNSAFE_OSASCRIPT_LINE));
    }
}
