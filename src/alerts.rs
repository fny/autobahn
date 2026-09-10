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
    fn after(&self, alert: Alert) -> Duration {
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
}

impl Alerter {
    pub fn new(plan: AlertPlan) -> Alerter {
        Alerter {
            plan,
            seen: HashMap::new(),
            fired: BTreeSet::new(),
            fired_at: None,
            cleared_at: None,
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
        for session in sessions {
            for alert in &session.alerts {
                present.insert((session.key(), *alert));
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
                self.seen
                    .get(*key)
                    .is_some_and(|since| now.duration_since(*since) >= self.plan.after(key.1))
            })
            .cloned()
            .collect();

        if confirmed == self.fired {
            // Unchanged. Silence, unless a repeat was asked for — and
            // never a repeat of nothing.
            if confirmed.is_empty() || self.plan.repeat_after.is_zero() {
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

        self.fired = confirmed.clone();
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
                lines.push(format!(
                    "{} → {}: {}",
                    session.group,
                    short_host(&session.host, &hosts),
                    session.summary
                ));
            }
        }
        let host_lines: Vec<String> = away
            .iter()
            .map(|(host, (count, reason))| {
                format!(
                    "{} {reason} — {} paused",
                    short_host(host, &hosts),
                    plural(*count, "group")
                )
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
        let detail = lines
            .iter()
            .chain(host_lines.iter())
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> AlertPlan {
        AlertPlan {
            on_alert: Some("notify".into()),
            default_after: Duration::from_secs(30),
            timeout: Duration::from_secs(10),
            settle_after: Duration::from_secs(15 * 60),
            ..AlertPlan::default()
        }
    }

    fn session(host: &str, alerts: &[Alert]) -> SessionAlerts {
        session_in("work", host, alerts, "summary")
    }

    fn session_in(group: &str, host: &str, alerts: &[Alert], summary: &str) -> SessionAlerts {
        SessionAlerts {
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
        let mut at = |seconds: u64| start + Duration::from_secs(seconds);

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
}
