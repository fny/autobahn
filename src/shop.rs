//! `autobahn mi` — the shop.
//!
//! An easter egg, and a legible one: every number on the screen is real.
//! The shop is open when a supervisor answers and closed when none does,
//! each session is an order, and an order fills as its transfer does.
//!
//! The counter is the useful half. It opens on an order and shows that
//! order's issues as a tree — cause, then place, then path — and every
//! level can be acted on, so one keypress settles a whole directory or a
//! single file.
//!
//! Every frame is built as an exact grid of rows, each padded to the
//! terminal's width. Nothing is appended and nothing reflows: a column
//! that shifts because a word changed length is the difference between a
//! display and a flicker.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use autobahn::config::SessionPlan;
use autobahn::progress::Phase;
use autobahn::supervisor::{status_report, SessionReport, StatusReport};

/// How often the shop repaints.
const FRAME: Duration = Duration::from_millis(120);

/// How often the report is rebuilt. Repainting is cheap; asking the
/// supervisor is not.
const REFRESH: Duration = Duration::from_millis(1_200);

/// The length of a baguette.
const BAGUETTE: usize = 12;

/// Lines of history kept under the counter.
const TICKER: usize = 3;

/// What the reader pressed.
#[derive(Debug, PartialEq)]
enum Key {
    Up,
    Down,
    Open,
    Close,
    Flush,
    Keep(Winner),
    Copy,
    Quit,
}

/// Whose version wins a dispute.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Winner {
    Alpha,
    Beta,
    Both,
}

/// What a row of the counter can be acted on with.
#[derive(Clone)]
enum Act {
    /// Conflicting paths, settled by choosing a side.
    Conflicts(Vec<String>),
    /// Blocked paths, which autobahn cannot clear itself. The commands
    /// that would clear them go to the clipboard instead: they are `sudo`
    /// over ssh, and a password prompt has nowhere to appear here.
    Blocked(Vec<String>),
}

/// One line of the counter's tree.
#[derive(Clone)]
struct Row {
    depth: usize,
    /// Stable across frames, so an opened branch stays open.
    key: String,
    label: String,
    detail: String,
    children: bool,
    act: Act,
}

/// The counter, open on one order.
struct Counter {
    group: String,
    host: String,
    cursor: usize,
}

/// A sampled transfer rate.
#[derive(Default)]
struct Rate {
    last: u64,
    at: Option<Instant>,
    per_second: f64,
}

impl Rate {
    fn sample(&mut self, total: u64) {
        let now = Instant::now();
        let Some(at) = self.at else {
            self.last = total;
            self.at = Some(now);
            return;
        };
        let elapsed = now.duration_since(at).as_secs_f64();
        if elapsed < 1.0 {
            return;
        }
        // Only forward movement counts. Every cycle resets the counters it
        // reports, and a transfer ending is not a negative rate.
        self.per_second = total.saturating_sub(self.last) as f64 / elapsed;
        self.last = total;
        self.at = Some(now);
    }
}

/// The shop's state between frames.
struct Shop<'a> {
    plans: Vec<&'a SessionPlan>,
    state_root: PathBuf,
    config: Option<PathBuf>,
    report: StatusReport,
    cursor: usize,
    counter: Option<Counter>,
    expanded: BTreeSet<String>,
    working: Arc<Mutex<Option<String>>>,
    rate: Rate,
    ticker: Vec<String>,
    frame: u64,
}

/// Runs the shop until the reader closes up.
pub fn run(selected: &[&SessionPlan], state_root: &Path, config: Option<PathBuf>) -> Result<()> {
    if unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1 {
        anyhow::bail!("the shop needs a terminal");
    }
    let _terminal = crate::pager::Terminal::enter()?;

    let mut shop = Shop {
        plans: selected.to_vec(),
        state_root: state_root.to_path_buf(),
        config,
        report: status_report(selected, state_root),
        cursor: 0,
        counter: None,
        expanded: BTreeSet::new(),
        working: Arc::default(),
        rate: Rate::default(),
        ticker: Vec::new(),
        frame: 0,
    };
    let mut refreshed = Instant::now();
    let mut painted = String::new();

    while !crate::pager::interrupted() {
        for key in keys() {
            if shop.press(key) {
                return Ok(());
            }
        }
        if refreshed.elapsed() >= REFRESH {
            let plans: Vec<&SessionPlan> = shop.plans.clone();
            shop.report = status_report(&plans, &shop.state_root);
            shop.ticker = recent_log(TICKER);
            refreshed = Instant::now();
        }
        let moved = shop.transferred();
        shop.rate.sample(moved);

        let drawn = shop.draw();
        if drawn != painted {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let _ = write!(out, "\x1b[H{drawn}\x1b[J");
            let _ = out.flush();
            painted = drawn;
        }
        shop.frame += 1;
        std::thread::sleep(FRAME);
    }
    Ok(())
}

// ── state ──────────────────────────────────────────────────────────────

impl Shop<'_> {
    fn orders(&self) -> Vec<(&str, &SessionReport)> {
        self.report
            .groups
            .iter()
            .flat_map(|group| {
                group
                    .sessions
                    .iter()
                    .map(move |session| (group.name.as_str(), session))
            })
            .collect()
    }

    /// Bytes moved so far by every transfer in flight.
    fn transferred(&self) -> u64 {
        self.orders()
            .iter()
            .filter_map(|(_, session)| session.progress.as_ref())
            .map(|progress| progress.staged_bytes)
            .sum()
    }

    /// The order the counter is open on.
    fn at_counter(&self) -> Option<(&str, &SessionReport)> {
        let counter = self.counter.as_ref()?;
        self.orders()
            .into_iter()
            .find(|(group, session)| *group == counter.group && session.host == counter.host)
    }

    fn press(&mut self, key: Key) -> bool {
        let open = self.counter.is_some();
        match (open, key) {
            (_, Key::Quit) => return true,
            (true, Key::Close) => {
                // Collapse first, close second. Stepping out of an open
                // branch loses the reader's place in it.
                let here = self
                    .selected()
                    .map(|row| row.key)
                    .filter(|key| self.expanded.contains(key));
                match here {
                    Some(key) => {
                        self.expanded.remove(&key);
                    }
                    None => self.counter = None,
                }
            }
            (true, Key::Open) => {
                if let Some(row) = self.selected() {
                    if row.children && !self.expanded.insert(row.key.clone()) {
                        self.expanded.remove(&row.key);
                    }
                }
            }
            (true, Key::Up) => {
                if let Some(counter) = &mut self.counter {
                    counter.cursor = counter.cursor.saturating_sub(1);
                }
            }
            (true, Key::Down) => {
                let last = self.rows().len().saturating_sub(1);
                if let Some(counter) = &mut self.counter {
                    counter.cursor = (counter.cursor + 1).min(last);
                }
            }
            (true, Key::Keep(winner)) => self.settle(winner),
            (true, Key::Copy) => self.copy_fix(),
            (true, _) => {}
            (false, Key::Up) => self.cursor = self.cursor.saturating_sub(1),
            (false, Key::Down) => {
                self.cursor = (self.cursor + 1).min(self.orders().len().saturating_sub(1))
            }
            (false, Key::Open) => self.open_counter(),
            (false, Key::Flush) => self.flush(),
            (false, _) => {}
        }
        false
    }

    fn open_counter(&mut self) {
        let orders = self.orders();
        let Some((group, session)) = orders.get(self.cursor) else {
            return;
        };
        if session.conflicts.is_empty() && session.blocked.is_empty() {
            return;
        }
        self.counter = Some(Counter {
            group: (*group).to_owned(),
            host: session.host.clone(),
            cursor: 0,
        });
        self.expanded.clear();
    }

    fn selected(&self) -> Option<Row> {
        let cursor = self.counter.as_ref()?.cursor;
        self.rows().into_iter().nth(cursor)
    }

    fn settle(&mut self, winner: Winner) {
        let Some(row) = self.selected() else { return };
        let Act::Conflicts(paths) = row.act else {
            return;
        };
        let Some(counter) = &self.counter else { return };
        if paths.is_empty() || self.busy() {
            return;
        }
        let keep = match winner {
            Winner::Alpha => "alpha".to_owned(),
            Winner::Beta => counter.host.clone(),
            Winner::Both => "both".to_owned(),
        };
        // One invocation per path. `resolve` takes a folder, but the tree
        // has already decided exactly which paths this row covers, and a
        // folder would sweep in whatever appeared since.
        let group = counter.group.clone();
        let told = match paths.len() {
            1 => format!("settled {}", paths[0]),
            many => format!("settled {many} paths"),
        };
        let commands = paths
            .iter()
            .map(|path| {
                vec![
                    "resolve".to_owned(),
                    group.clone(),
                    path.clone(),
                    "--keep".into(),
                    keep.clone(),
                    "--yes".into(),
                ]
            })
            .collect();
        self.spawn(commands, told, "could not settle".to_owned());
    }

    /// Puts the commands that would clear a blocked row on the clipboard.
    fn copy_fix(&mut self) {
        let Some(row) = self.selected() else { return };
        let Act::Blocked(fixes) = row.act else { return };
        if fixes.is_empty() {
            return;
        }
        let told = match copy_to_clipboard(&fixes.join("\n")) {
            true => format!("copied: {}", shorten(&fixes[0], 60)),
            false => "nothing here can reach a clipboard".to_owned(),
        };
        self.say(told);
    }

    fn flush(&mut self) {
        if self.busy() {
            return;
        }
        let orders = self.orders();
        let Some((group, _)) = orders.get(self.cursor) else {
            return;
        };
        let group = (*group).to_owned();
        self.spawn(
            vec![vec!["flush".to_owned(), group.clone()]],
            format!("rushed {group}"),
            format!("could not rush {group}"),
        );
    }

    fn say(&self, told: String) {
        *self
            .working
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(told);
    }

    fn busy(&self) -> bool {
        self.working
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_deref()
            == Some("running")
    }

    /// Runs autobahn commands on their own thread, so a slow one cannot
    /// stop the shop drawing.
    fn spawn(&self, commands: Vec<Vec<String>>, done: String, failed: String) {
        let state_root = self.state_root.to_string_lossy().into_owned();
        let config = self.config.clone();
        let executable = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("autobahn"));
        let working = self.working.clone();
        *working.lock().unwrap_or_else(|error| error.into_inner()) = Some("running".into());
        std::thread::spawn(move || {
            let mut told = done;
            for mut arguments in commands {
                arguments.push("--state-root".into());
                arguments.push(state_root.clone());
                if let Some(config) = &config {
                    arguments.push("--config".into());
                    arguments.push(config.to_string_lossy().into_owned());
                }
                match std::process::Command::new(&executable)
                    .args(&arguments)
                    .output()
                {
                    Ok(result) if result.status.success() => {}
                    Ok(result) => {
                        told = format!(
                            "{failed}: {}",
                            String::from_utf8_lossy(&result.stderr).trim()
                        );
                        break;
                    }
                    Err(error) => {
                        told = format!("{failed}: {error}");
                        break;
                    }
                }
            }
            *working.lock().unwrap_or_else(|error| error.into_inner()) = Some(told);
        });
    }
}

// ── the tree ───────────────────────────────────────────────────────────

impl Shop<'_> {
    /// The counter's visible lines: the issue tree, with opened branches
    /// expanded.
    ///
    /// Rebuilt every frame from the report, so it follows what the
    /// supervisor finds without any state of its own beyond which keys are
    /// open. A branch that disappears takes its expansion with it.
    fn rows(&self) -> Vec<Row> {
        let Some((_, session)) = self.at_counter() else {
            return Vec::new();
        };
        let mut rows = Vec::new();

        if !session.conflicts.is_empty() {
            let paths: Vec<&str> = session
                .conflicts
                .iter()
                .map(|conflict| conflict.path.as_str())
                .collect();
            self.branch(
                &mut rows,
                "conflicts".to_owned(),
                format!("{} conflict", plural(paths.len())),
                "both sides changed these".to_owned(),
                &paths,
                &|paths| Act::Conflicts(paths),
            );
        }

        // Blocked paths, grouped by what stopped them. The cause is the
        // innermost message; the wrapping context repeats each file's own
        // path, so twenty files stopped by one thing would otherwise read
        // as twenty reasons.
        let mut causes: Vec<(&str, &str, Vec<&str>)> = Vec::new();
        for entry in &session.blocked {
            let (side, path, cause) = crate::blocked_parts(entry);
            match causes
                .iter_mut()
                .find(|(other_side, other, _)| *other_side == side && *other == cause)
            {
                Some((_, _, paths)) => paths.push(path),
                None => causes.push((side, cause, vec![path])),
            }
        }
        for (side, cause, paths) in causes {
            let plan = self.plan_for_counter();
            let fixes = plan
                .map(|plan| crate::blocked_fix(side, cause, &crate::common_prefix(&paths), plan))
                .unwrap_or_default();
            self.branch(
                &mut rows,
                format!("blocked/{side}/{cause}"),
                format!("{} blocked on {side}", plural(paths.len())),
                cause.to_owned(),
                &paths,
                &|_| Act::Blocked(fixes.clone()),
            );
        }
        rows
    }

    /// The plan behind the open counter, for the commands that clear its
    /// blocked paths.
    fn plan_for_counter(&self) -> Option<&SessionPlan> {
        let counter = self.counter.as_ref()?;
        self.plans
            .iter()
            .copied()
            .find(|plan| plan.group == counter.group && plan.host == counter.host)
    }

    /// Emits one heading and, when it is open, the places under it.
    fn branch(
        &self,
        rows: &mut Vec<Row>,
        key: String,
        label: String,
        detail: String,
        paths: &[&str],
        act: &dyn Fn(Vec<String>) -> Act,
    ) {
        let all: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
        let open = self.expanded.contains(&key);
        rows.push(Row {
            depth: 0,
            key: key.clone(),
            label,
            detail,
            children: true,
            act: act(all),
        });
        if !open {
            return;
        }
        // A cause can cover unrelated places. Clustering separates them
        // before the directory they share is named.
        for (prefix, count) in crate::clusters(paths) {
            let here: Vec<&str> = paths
                .iter()
                .copied()
                .filter(|path| path == &prefix || path.starts_with(&format!("{prefix}/")))
                .collect();
            let place = format!("{key}/{prefix}");
            let opened = self.expanded.contains(&place);
            // A place holding one path names the path. The folder above
            // it is a level nobody needs to open to reach one file.
            let single = here.len() == 1;
            rows.push(Row {
                depth: 1,
                key: place.clone(),
                label: match single {
                    true => here[0].to_owned(),
                    false => format!("{prefix}/"),
                },
                detail: match single {
                    true => String::new(),
                    false => format!("{count}"),
                },
                children: !single,
                act: act(here.iter().map(|path| path.to_string()).collect()),
            });
            if !opened {
                continue;
            }
            for path in here {
                rows.push(Row {
                    depth: 2,
                    key: format!("{place}//{path}"),
                    label: path
                        .strip_prefix(&format!("{prefix}/"))
                        .unwrap_or(path)
                        .to_owned(),
                    detail: String::new(),
                    children: false,
                    act: act(vec![path.to_owned()]),
                });
            }
        }
    }
}

/// "1 thing" or "4 things", without the awkward parenthesis.
fn plural(count: usize) -> String {
    match count {
        1 => "1".to_owned(),
        many => format!("{many}"),
    }
}

/// The last few lines the supervisor wrote, newest last.
fn recent_log(lines: usize) -> Vec<String> {
    let Ok(path) = autobahn::service::log_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .rev()
        .take(lines)
        .map(|line| line.to_owned())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Puts text on the system clipboard, reporting whether anything took it.
fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    for (program, arguments) in [
        ("pbcopy", &[][..]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"][..]),
    ] {
        let Ok(mut child) = Command::new(program)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().map(|status| status.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}

// ── drawing ────────────────────────────────────────────────────────────

impl Shop<'_> {
    /// One frame, as exactly as many rows as the terminal has, each padded
    /// to its width.
    ///
    /// Built as a grid rather than appended to, so a longer word cannot
    /// push a column sideways and an opened panel cannot move what is
    /// above it.
    fn draw(&self) -> String {
        let (height, width) = terminal_size().unwrap_or((24, 100));
        let mut lines: Vec<String> = Vec::with_capacity(height);

        lines.extend(self.sign(width));
        lines.push(String::new());

        if !self.report.supervisor_running {
            lines.extend(self.shuttered(width));
            return grid(lines, height, width, self.footer());
        }

        // The regions below the sign, sized before anything is drawn: the
        // rail, the counter when open, and the ticker. Whatever is left
        // over is blank, so nothing moves when a panel opens.
        let orders = self.orders();
        let ticker = if self.ticker.is_empty() {
            0
        } else {
            TICKER + 1
        };
        let spare = height.saturating_sub(lines.len() + ticker + 2);
        let rail = if self.counter.is_some() {
            orders.len().min(spare / 3).max(1)
        } else {
            orders.len().min(spare)
        };

        for (index, (group, session)) in orders.iter().enumerate().take(rail) {
            lines.push(self.order(index, group, session, width));
        }
        if orders.len() > rail {
            lines.push(dim(&format!("     … {} more", orders.len() - rail)));
        }

        if self.counter.is_some() {
            lines.push(String::new());
            let room = height.saturating_sub(lines.len() + ticker + 2);
            lines.extend(self.counter_rows(room, width));
        }

        // The ticker sits at the foot of the body, above the keys. It is
        // the only view autobahn has of what it has been doing: the log is
        // written and never read.
        if ticker > 0 {
            let body = height.saturating_sub(2);
            while lines.len() + ticker < body {
                lines.push(String::new());
            }
            lines.truncate(body.saturating_sub(ticker));
            for line in &self.ticker {
                lines.push(dim(&format!(
                    "  {}",
                    shorten(line, width.saturating_sub(4))
                )));
            }
        }

        grid(lines, height, width, self.footer())
    }

    fn sign(&self, width: usize) -> Vec<String> {
        let open = self.report.supervisor_running;
        let orders = self.orders();
        let lamp = if open {
            let lit = if self.frame % 20 < 10 { "◉" } else { "◎" };
            format!("\x1b[32m{lit} OPEN\x1b[0m")
        } else {
            "\x1b[31m✖ CLOSED\x1b[0m".to_owned()
        };
        // What a shop would actually track: what moved, and how fast.
        let tally = if open {
            let filling = orders
                .iter()
                .filter(|(_, session)| {
                    session
                        .progress
                        .as_ref()
                        .is_some_and(|progress| progress.phase == Phase::Staging)
                })
                .count();
            let moved: u64 = orders
                .iter()
                .filter_map(|(_, session)| session.progress.as_ref())
                .map(|progress| progress.staged_bytes)
                .sum();
            let mut parts = vec![format!("{} orders", orders.len())];
            if filling > 0 {
                parts.push(format!("{filling} filling"));
                parts.push(format!("{}/s", bytes(self.rate.per_second as u64)));
                parts.push(bytes(moved));
            }
            dim(&parts.join(" · "))
        } else {
            String::new()
        };
        let inner = width.saturating_sub(4);
        vec![
            dim(&format!("  ╔{}╗", "═".repeat(inner))),
            format!(
                "  {}{}{}",
                dim("║"),
                centered("🥖  \x1b[1mA U T O B Á N H   M Ì\x1b[0m  🥖", inner),
                dim("║")
            ),
            format!(
                "  {} {} {}",
                dim("║"),
                between(&lamp, &tally, inner.saturating_sub(2)),
                dim("║")
            ),
            dim(&format!("  ╚{}╝", "═".repeat(inner))),
        ]
    }

    fn shuttered(&self, width: usize) -> Vec<String> {
        let note = match self.report.service.as_str() {
            "stopped" => "autobahn start",
            "not-installed" => "autobahn install",
            _ => "autobahn watch",
        };
        let mut lines: Vec<String> = (0..3)
            .map(|_| dim(&format!("  {}", "▚".repeat(width.saturating_sub(4)))))
            .collect();
        lines.push(String::new());
        lines.push(format!("  {}  {note}", dim("note on the glass:")));
        lines
    }

    /// One order on the rail, in fixed columns.
    fn order(&self, index: usize, group: &str, session: &SessionReport, width: usize) -> String {
        let here = index == self.cursor && self.counter.is_none();
        let working = session
            .progress
            .as_ref()
            .filter(|progress| progress.phase.is_working());
        let (word, colour) = match (
            working.map(|progress| progress.phase),
            session.state.as_str(),
        ) {
            (Some(Phase::Connecting), _) => ("taking the order", ""),
            (Some(Phase::Scanning), _) => ("checking the pantry", ""),
            (Some(Phase::Reconciling), _) => ("reading the ticket", ""),
            (Some(Phase::Staging), _) => ("filling", ""),
            (Some(Phase::Applying), _) => ("wrapping", ""),
            (Some(Phase::Saving), _) => ("ringing it up", ""),
            (_, "synchronized") => ("served", "\x1b[32m"),
            (_, "conflicts") => ("disputed", "\x1b[33m"),
            (_, "blocked") => ("out of stock", "\x1b[33m"),
            (_, "halted") => ("kitchen closed", "\x1b[31m"),
            (_, "unreachable") => ("supplier away", "\x1b[31m"),
            (_, "errored") => ("burnt", "\x1b[31m"),
            (_, "paused") => ("on break", "\x1b[2m"),
            _ => ("not started", "\x1b[2m"),
        };
        let waiting = session.conflicts.len() + session.blocked.len();
        let detail = match working {
            Some(progress) if progress.phase == Phase::Staging && progress.staged_total > 0 => {
                format!(
                    "{} of {}",
                    thousands(progress.staged),
                    thousands(progress.staged_total)
                )
            }
            Some(progress) => format!("{}s", progress.seconds),
            None if waiting > 0 => format!("{waiting} waiting"),
            None => match session.age_seconds {
                Some(age) => format!("{age}s ago"),
                None => String::new(),
            },
        };

        // Fixed columns. The destination takes whatever the terminal has
        // left over, so the baguette and the state word never move.
        const MARKER: usize = 2;
        const GROUP: usize = 9;
        const STATE: usize = 20;
        const DETAIL: usize = 14;
        let loaf = 2 + 1 + BAGUETTE + 1;
        let host = width
            .saturating_sub(2 + MARKER + GROUP + 3 + loaf + 2 + STATE + 1 + DETAIL)
            .clamp(8, 30);
        format!(
            "  {}{} {} {}  {}  {}{}",
            if here { "\x1b[7m▸\x1b[0m " } else { "  " },
            pad(&dim(&shorten(group, GROUP)), GROUP),
            dim("→"),
            pad(&shorten(&session.host, host), host),
            baguette(session, working.map(|progress| progress.phase), self.frame),
            pad(&format!("{colour}{word}\x1b[0m"), STATE),
            dim(&shorten(&detail, DETAIL)),
        )
    }

    /// The counter: the issue tree for one order.
    fn counter_rows(&self, room: usize, width: usize) -> Vec<String> {
        let Some(counter) = &self.counter else {
            return Vec::new();
        };
        let rows = self.rows();
        let inner = width.saturating_sub(4);
        let mut lines = vec![
            dim(&format!("  ┌{}┐", "─".repeat(inner))),
            format!(
                "  {} {} {}",
                dim("│"),
                between(
                    &format!(
                        "\x1b[1mthe counter\x1b[0m {}",
                        dim(&format!("{} → {}", counter.group, counter.host))
                    ),
                    &format!(
                        "\x1b[33m{} waiting\x1b[0m",
                        rows.first()
                            .map_or(0, |_| rows.iter().filter(|row| row.depth == 0).count())
                    ),
                    inner.saturating_sub(2)
                ),
                dim("│")
            ),
        ];

        // The window follows the cursor, so a long tree scrolls rather
        // than spilling past the bottom of the screen.
        let visible = room.saturating_sub(3).max(1);
        let first = counter.cursor.saturating_sub(visible.saturating_sub(1));
        for (index, row) in rows.iter().enumerate().skip(first).take(visible) {
            let here = index == counter.cursor;
            let marker = match (row.children, self.expanded.contains(&row.key)) {
                (true, true) => "▾",
                (true, false) => "▸",
                (false, _) => " ",
            };
            let text = format!(
                "{}{marker} {} {}",
                "  ".repeat(row.depth),
                shorten(&row.label, inner.saturating_sub(row.depth * 2 + 24)),
                dim(&row.detail),
            );
            let text = if here {
                format!("\x1b[7m{}\x1b[0m", strip(&text))
            } else {
                text
            };
            lines.push(format!(
                "  {} {} {}",
                dim("│"),
                pad(&text, inner.saturating_sub(2)),
                dim("│")
            ));
        }
        lines.push(dim(&format!("  └{}┘", "─".repeat(inner))));
        lines
    }

    /// The keys that apply right now, and whatever the last command said.
    fn footer(&self) -> String {
        let told = self
            .working
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let keys = match (&self.counter, self.selected().map(|row| row.act)) {
            (None, _) => "↑↓ choose · ⏎ counter · f rush · q close the shop",
            (Some(_), Some(Act::Conflicts(_))) => {
                "↑↓ ⏎ open · ← back · a ours · t theirs · b both · q close the shop"
            }
            (Some(_), Some(Act::Blocked(_))) => {
                "↑↓ ⏎ open · ← back · c copy the fix · q close the shop"
            }
            (Some(_), None) => "↑↓ ⏎ open · ← back · q close the shop",
        };
        match told.as_deref() {
            Some("running") => format!("{}   {}", dim(keys), "working…"),
            Some(told) => format!("{}   {}", dim(keys), dim(told)),
            None => dim(keys),
        }
    }
}

/// Pads a frame to exactly the terminal's rows, with the footer on the
/// last one.
fn grid(mut lines: Vec<String>, height: usize, width: usize, footer: String) -> String {
    let body = height.saturating_sub(2);
    lines.truncate(body);
    while lines.len() < body {
        lines.push(String::new());
    }
    lines.push(String::new());
    lines.push(footer);
    lines
        .into_iter()
        .map(|line| pad(&line, width))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The baguette, filled to whatever is true of the order.
fn baguette(session: &SessionReport, phase: Option<Phase>, frame: u64) -> String {
    let filled = |count: usize, colour: &str| {
        let count = count.min(BAGUETTE);
        format!(
            "🥖{}{colour}{}\x1b[0m{}",
            dim("["),
            "▓".repeat(count),
            dim(&format!("{}]", "░".repeat(BAGUETTE - count)))
        )
    };
    match (phase, session.state.as_str()) {
        (Some(Phase::Staging), _) => {
            let (done, total) = session
                .progress
                .as_ref()
                .map(|progress| (progress.staged, progress.staged_total.max(1)))
                .unwrap_or((0, 1));
            filled((done * BAGUETTE as u64 / total) as usize, "\x1b[33m")
        }
        // Anything else that is working gets a wave, so a long scan still
        // looks alive.
        (Some(_), _) => {
            let head = (frame as usize / 2) % (BAGUETTE + 4);
            let mut bread = format!("🥖{}", dim("["));
            for cell in 0..BAGUETTE {
                if cell + 2 >= head && cell <= head {
                    bread.push_str("\x1b[33m▓\x1b[0m");
                } else {
                    bread.push_str(&dim("░"));
                }
            }
            bread.push_str(&dim("]"));
            bread
        }
        (_, "synchronized") => filled(BAGUETTE, "\x1b[32m"),
        (_, "conflicts" | "blocked") => filled(BAGUETTE, "\x1b[33m"),
        (_, "halted" | "unreachable" | "errored") => format!(
            "🥖{}\x1b[31m✖\x1b[0m{}",
            dim("["),
            dim(&format!("{}]", " ".repeat(BAGUETTE - 1)))
        ),
        _ => filled(0, ""),
    }
}

/// Reads whatever keys are waiting.
fn keys() -> Vec<Key> {
    // An escape sequence can arrive split across two reads — three bytes
    // are not delivered atomically, and a burst larger than the buffer
    // divides wherever it runs out. A tail dropped here is a keypress the
    // reader made and the shop ignored, so it is carried over instead.
    thread_local! {
        static PENDING: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    let mut buffer = [0u8; 64];
    let read = unsafe {
        libc::read(
            libc::STDIN_FILENO,
            buffer.as_mut_ptr() as *mut libc::c_void,
            buffer.len(),
        )
    };
    let fresh = if read > 0 {
        &buffer[..read as usize]
    } else {
        &[][..]
    };
    PENDING.with(|pending| {
        let mut held = pending.borrow_mut();
        let mut bytes = std::mem::take(&mut *held);
        bytes.extend_from_slice(fresh);
        let (keys, tail) = parse(&bytes);
        *held = tail;
        keys
    })
}

/// Turns bytes into keys, returning any incomplete sequence at the end for
/// the next read to finish.
fn parse(bytes: &[u8]) -> (Vec<Key>, Vec<u8>) {
    let mut keys = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let rest = &bytes[index..];
        // An escape that could still become an arrow key waits for the
        // rest rather than being read as the characters it happens to
        // start with.
        if matches!(rest, [0x1b] | [0x1b, b'[']) {
            return (keys, rest.to_vec());
        }
        let (key, width) = match *rest {
            [0x1b, b'[', b'A', ..] => (Some(Key::Up), 3),
            [0x1b, b'[', b'B', ..] => (Some(Key::Down), 3),
            [0x1b, b'[', b'C', ..] => (Some(Key::Open), 3),
            [0x1b, b'[', b'D', ..] => (Some(Key::Close), 3),
            [0x1b, b'[', ..] => (None, 3),
            [0x1b, ..] => (Some(Key::Close), 1),
            [b'k', ..] => (Some(Key::Up), 1),
            [b'j', ..] => (Some(Key::Down), 1),
            [b'h', ..] => (Some(Key::Close), 1),
            [b'l', ..] | [b'\r', ..] | [b'\n', ..] => (Some(Key::Open), 1),
            [b'f', ..] => (Some(Key::Flush), 1),
            [b'a', ..] => (Some(Key::Keep(Winner::Alpha)), 1),
            [b't', ..] => (Some(Key::Keep(Winner::Beta)), 1),
            [b'b', ..] => (Some(Key::Keep(Winner::Both)), 1),
            [b'c', ..] => (Some(Key::Copy), 1),
            [b'q', ..] | [0x03, ..] => (Some(Key::Quit), 1),
            _ => (None, 1),
        };
        if let Some(key) = key {
            keys.push(key);
        }
        index += width;
    }
    (keys, Vec::new())
}

// ── measuring ──────────────────────────────────────────────────────────

/// The terminal's rows and columns.
fn terminal_size() -> Option<(usize, usize)> {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } != 0 {
        return None;
    }
    (size.ws_row > 0 && size.ws_col > 0).then_some((size.ws_row as usize, size.ws_col as usize))
}

fn dim(text: &str) -> String {
    format!("\x1b[2m{text}\x1b[0m")
}

/// The columns one character occupies.
///
/// Emoji are two columns wide in every terminal that draws them. Counting
/// one as a single column is what leaves a frame ragged.
fn char_width(character: char) -> usize {
    match character as u32 {
        0xFE00..=0xFE0F | 0x200D => 0,
        0x1F000..=0x1FAFF => 2,
        _ => 1,
    }
}

/// The columns a string occupies, ignoring its escape sequences.
fn width(text: &str) -> usize {
    let mut columns = 0;
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character == '\x1b' {
            for escape in characters.by_ref() {
                if escape.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        columns += char_width(character);
    }
    columns
}

/// The text without its escape sequences.
fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character == '\x1b' {
            for escape in characters.by_ref() {
                if escape.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(character);
    }
    out
}

fn centered(text: &str, columns: usize) -> String {
    let visible = width(text);
    let left = columns.saturating_sub(visible) / 2;
    let right = columns.saturating_sub(visible + left);
    format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
}

fn between(left: &str, right: &str, columns: usize) -> String {
    let gap = columns.saturating_sub(width(left) + width(right));
    format!("{left}{}{right}", " ".repeat(gap))
}

fn pad(text: &str, columns: usize) -> String {
    format!("{text}{}", " ".repeat(columns.saturating_sub(width(text))))
}

/// Shortens a name to fit its column, keeping the end — which for a path
/// is the part that identifies it.
fn shorten(text: &str, columns: usize) -> String {
    if width(text) <= columns || columns == 0 {
        return text.to_owned();
    }
    let kept: String = text
        .chars()
        .rev()
        .take(columns.saturating_sub(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{kept}")
}

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

fn bytes(count: u64) -> String {
    const UNITS: [(u64, &str); 4] = [(1 << 30, "GB"), (1 << 20, "MB"), (1 << 10, "kB"), (1, "B")];
    for (scale, unit) in UNITS {
        if count >= scale {
            return match scale {
                1 => format!("{count} B"),
                _ => format!("{:.1} {unit}", count as f64 / scale as f64),
            };
        }
    }
    "0 B".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_baguette_is_two_columns_wide() {
        assert_eq!(width("🥖"), 2);
        assert_eq!(width("Á"), 1);
        assert_eq!(width("A U T O B Á N H   M Ì"), 21);
        assert_eq!(width("\x1b[1mbold\x1b[0m"), 4);
    }

    #[test]
    fn the_sign_keeps_its_accents_through_the_capitals() {
        // The joke is that `autobahn` and `bánh` are the same word, which
        // only reads if the capitals keep their marks.
        let sign = centered("A U T O B Á N H   M Ì", 60);
        assert!(sign.contains('Á'), "{sign}");
        assert!(sign.contains('Ì'), "{sign}");
    }

    #[test]
    fn every_frame_is_exactly_the_size_of_the_terminal() {
        // The whole cure for a janky display: a frame is a grid, not a
        // string that grew. Every row is the terminal's width and there
        // are exactly as many as it has, so nothing an order says can push
        // a column sideways or move the panel below it.
        for (height, width) in [(24, 80), (40, 120), (10, 40)] {
            let frame = grid(
                vec!["short".into(), dim("dim"), "🥖 wide".into()],
                height,
                width,
                "footer".into(),
            );
            let lines: Vec<&str> = frame.split('\n').collect();
            assert_eq!(lines.len(), height, "{height}x{width}");
            for line in &lines {
                assert_eq!(super::width(line), width, "{line:?} in {height}x{width}");
            }
            assert!(lines[height - 1].starts_with("footer"));
        }
    }

    #[test]
    fn a_frame_longer_than_the_terminal_is_cut_rather_than_spilling() {
        let many: Vec<String> = (0..100).map(|n| format!("line {n}")).collect();
        let frame = grid(many, 12, 40, "footer".into());
        assert_eq!(frame.split('\n').count(), 12);
    }

    #[test]
    fn shortening_keeps_the_end_of_a_path() {
        // The end of a path is what identifies it; the front is shared
        // with everything else in the tree.
        assert_eq!(shorten("short", 10), "short", "what fits is left alone");
        // The end identifies a path; the front is shared with everything
        // else in the tree.
        assert_eq!(shorten("azure/backend/app.py", 10), "…nd/app.py");
        assert_eq!(width(&shorten("azure/backend/app.py", 10)), 10);
        assert!(width(&shorten("a/very/long/path/indeed.txt", 12)) <= 12);
    }

    #[test]
    fn every_baguette_is_the_same_length() {
        // Every order sits on one rail. A bar whose width changed with its
        // state would make the column jump as sessions moved between them.
        let session = |state: &str| SessionReport {
            host: "beta".into(),
            beta: "beta".into(),
            mode: "two-way-conflict".into(),
            state: state.into(),
            cycles: 1,
            age_seconds: Some(1),
            conflicts: Vec::new(),
            blocked: Vec::new(),
            error: None,
            progress: None,
        };
        let served = width(&baguette(&session("synchronized"), None, 0));
        for state in ["halted", "conflicts", "blocked", "unreachable", "never-run"] {
            assert_eq!(
                width(&baguette(&session(state), None, 0)),
                served,
                "{state}"
            );
        }
        for frame in 0..40 {
            assert_eq!(
                width(&baguette(
                    &session("synchronized"),
                    Some(Phase::Scanning),
                    frame
                )),
                served,
                "frame {frame}"
            );
        }
        assert_eq!(served, 2 + 1 + BAGUETTE + 1);
    }

    #[test]
    fn a_rate_counts_only_what_moved_forward() {
        // Every cycle resets the counters it reports, so the total falls
        // back to zero. That is a transfer finishing, not a negative rate.
        let mut rate = Rate::default();
        rate.sample(1_000);
        rate.at = Some(Instant::now() - Duration::from_secs(2));
        rate.sample(3_000);
        assert!(rate.per_second > 0.0);
        rate.at = Some(Instant::now() - Duration::from_secs(2));
        rate.sample(0);
        assert_eq!(rate.per_second, 0.0);
    }

    /// An arrow key split across two reads is still an arrow key.
    ///
    /// Three bytes are not delivered atomically, and a burst larger than
    /// the read buffer divides wherever it runs out. Dropping the tail
    /// loses a keypress the reader made — found by holding ↓, where two of
    /// fourteen went missing and the cursor stopped short.
    #[test]
    fn a_split_escape_sequence_is_finished_by_the_next_read() {
        // Whole, it is one key and nothing is held back.
        let (keys, tail) = parse(b"\x1b[B");
        assert_eq!(keys, vec![Key::Down]);
        assert!(tail.is_empty());

        // Cut anywhere, the incomplete part waits.
        for cut in 1..3 {
            let (keys, tail) = parse(&b"\x1b[B"[..cut]);
            assert!(keys.is_empty(), "cut at {cut}");
            assert_eq!(tail, b"\x1b[B"[..cut].to_vec(), "cut at {cut}");
            // And finishing it yields the key.
            let mut rest = tail;
            rest.extend_from_slice(&b"\x1b[B"[cut..]);
            assert_eq!(parse(&rest).0, vec![Key::Down], "cut at {cut}");
        }

        // A burst is read whole, and a trailing fragment is kept.
        let (keys, tail) = parse(b"\x1b[B\x1b[B\x1b[");
        assert_eq!(keys, vec![Key::Down, Key::Down]);
        assert_eq!(tail, b"\x1b[".to_vec());

        // A bare escape is "go back", not the start of something.
        assert_eq!(parse(b"\x1bq").0, vec![Key::Close, Key::Quit]);
        // And the letters still work.
        assert_eq!(parse(b"jkl").0, vec![Key::Down, Key::Up, Key::Open]);
        assert_eq!(parse(b"atb").0.len(), 3);
    }
}
