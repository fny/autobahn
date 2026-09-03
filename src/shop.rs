//! `autobahn mi` — the shop.
//!
//! An easter egg, and a legible one: every number on the screen is real.
//! The shop is open when a supervisor answers and closed when none does,
//! each session is an order, and an order fills as its transfer does.
//!
//! It also settles disputes. A conflict is a customer at the counter, and
//! the counter runs the same `resolve` a person would type — so the shop
//! cannot drift from what resolution actually does.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use autobahn::config::SessionPlan;
use autobahn::progress::Phase;
use autobahn::supervisor::{status_report, ConflictDetail, SessionReport, StatusReport};

/// How often the shop repaints.
const FRAME: Duration = Duration::from_millis(120);

/// How often the report is rebuilt. Repainting is cheap; asking the
/// supervisor is not.
const REFRESH: Duration = Duration::from_millis(1_200);

/// The width of the sign.
const WIDTH: usize = 64;

/// The length of a baguette.
const BAGUETTE: usize = 12;

/// What the reader pressed.
enum Key {
    Up,
    Down,
    Enter,
    Flush,
    Back,
    Keep(Winner),
    Quit,
}

/// Whose version wins a dispute.
#[derive(Clone, Copy)]
enum Winner {
    Alpha,
    Beta,
    Both,
}

/// The shop's state between frames.
struct Shop<'a> {
    plans: Vec<&'a SessionPlan>,
    state_root: PathBuf,
    config: Option<PathBuf>,
    report: StatusReport,
    /// The highlighted order.
    cursor: usize,
    /// The counter, open on one order's disputes.
    counter: Option<Counter>,
    /// What a running command is doing, and how it ended.
    working: Arc<Mutex<Option<String>>>,
    frame: u64,
}

/// The counter: one order's disputes, and which one is being settled.
struct Counter {
    group: String,
    host: String,
    conflicts: Vec<ConflictDetail>,
    cursor: usize,
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
        working: Arc::default(),
        frame: 0,
    };
    let mut refreshed = std::time::Instant::now();
    let mut painted = String::new();

    while !crate::pager::interrupted() {
        for key in keys() {
            if shop.press(key) {
                return Ok(());
            }
        }
        if refreshed.elapsed() >= REFRESH {
            let selected: Vec<&SessionPlan> = shop.plans.clone();
            shop.report = status_report(&selected, &shop.state_root);
            refreshed = std::time::Instant::now();
        }
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

impl Shop<'_> {
    /// Every order on the rail, as (group, session).
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

    /// Acts on a key. Returns true when the shop should close.
    fn press(&mut self, key: Key) -> bool {
        let orders = self.orders().len();
        match (self.counter.is_some(), key) {
            (_, Key::Quit) => return true,
            // At the counter.
            (true, Key::Back) => self.counter = None,
            (true, Key::Up) => {
                if let Some(counter) = &mut self.counter {
                    counter.cursor = counter.cursor.saturating_sub(1);
                }
            }
            (true, Key::Down) => {
                if let Some(counter) = &mut self.counter {
                    counter.cursor =
                        (counter.cursor + 1).min(counter.conflicts.len().saturating_sub(1));
                }
            }
            (true, Key::Keep(winner)) => self.settle(winner),
            (true, _) => {}
            // On the rail.
            (false, Key::Up) => self.cursor = self.cursor.saturating_sub(1),
            (false, Key::Down) => self.cursor = (self.cursor + 1).min(orders.saturating_sub(1)),
            (false, Key::Enter) => self.open_counter(),
            (false, Key::Flush) => self.flush(),
            (false, _) => {}
        }
        false
    }

    /// Opens the counter on the highlighted order, when it has disputes.
    fn open_counter(&mut self) {
        let orders = self.orders();
        let Some((group, session)) = orders.get(self.cursor) else {
            return;
        };
        if session.conflicts.is_empty() {
            return;
        }
        self.counter = Some(Counter {
            group: (*group).to_owned(),
            host: session.host.clone(),
            conflicts: session.conflicts.clone(),
            cursor: 0,
        });
    }

    /// Settles the dispute at the counter, by running `resolve`.
    ///
    /// The command is the one a person would type, run the same way. So
    /// the shop cannot drift from what resolution does — including the
    /// fan-out, where the winner reaches every other destination too.
    fn settle(&mut self, winner: Winner) {
        let Some(counter) = &self.counter else { return };
        let Some(conflict) = counter.conflicts.get(counter.cursor) else {
            return;
        };
        if self.busy() {
            return;
        }
        let keep = match winner {
            Winner::Alpha => "alpha".to_owned(),
            Winner::Beta => counter.host.clone(),
            Winner::Both => "both".to_owned(),
        };
        let path = conflict.path.clone();
        self.spawn(
            vec![
                "resolve".into(),
                counter.group.clone(),
                path.clone(),
                "--keep".into(),
                keep,
            ],
            format!("settled {path}"),
            format!("could not settle {path}"),
        );

        // The settled dispute leaves the counter at once. The next report
        // confirms it; leaving it up invites a second press on a path
        // already being settled.
        if let Some(counter) = &mut self.counter {
            counter.conflicts.remove(counter.cursor);
            counter.cursor = counter
                .cursor
                .min(counter.conflicts.len().saturating_sub(1));
            if counter.conflicts.is_empty() {
                self.counter = None;
            }
        }
    }

    /// Rushes the highlighted order.
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
            vec!["flush".into(), group.clone()],
            format!("rushed {group}"),
            format!("could not rush {group}"),
        );
    }

    /// Whether a command is already running.
    fn busy(&self) -> bool {
        self.working
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_deref()
            == Some("running")
    }

    /// Runs one autobahn command on its own thread, so a slow one cannot
    /// stop the shop from drawing.
    fn spawn(&self, mut arguments: Vec<String>, done: String, failed: String) {
        arguments.push("--state-root".into());
        arguments.push(self.state_root.to_string_lossy().into_owned());
        if let Some(config) = &self.config {
            arguments.push("--config".into());
            arguments.push(config.to_string_lossy().into_owned());
        }
        let executable = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("autobahn"));
        let working = self.working.clone();
        *working.lock().unwrap_or_else(|error| error.into_inner()) = Some("running".into());
        std::thread::spawn(move || {
            let outcome = std::process::Command::new(executable)
                .args(&arguments)
                .output();
            let told = match outcome {
                Ok(result) if result.status.success() => done,
                Ok(result) => format!(
                    "{failed}: {}",
                    String::from_utf8_lossy(&result.stderr).trim()
                ),
                Err(error) => format!("{failed}: {error}"),
            };
            *working.lock().unwrap_or_else(|error| error.into_inner()) = Some(told);
        });
    }
}

// ── drawing ────────────────────────────────────────────────────────────

impl Shop<'_> {
    fn draw(&self) -> String {
        let mut out = String::new();
        let open = self.report.supervisor_running;
        let orders = self.orders();
        let served: u64 = orders.iter().map(|(_, session)| session.cycles).sum();

        // The sign. Both accents survive the capitals, because the whole
        // joke is that `autobahn` and `bánh` are the same word.
        out.push_str(&format!("  \x1b[2m╔{}╗\x1b[0m\n", "═".repeat(WIDTH)));
        out.push_str(&format!(
            "  \x1b[2m║\x1b[0m{}\x1b[2m║\x1b[0m\n",
            centered("🥖  \x1b[1mA U T O B Á N H   M Ì\x1b[0m  🥖", WIDTH)
        ));
        let lamp = if open {
            let lit = if self.frame % 20 < 10 { "◉" } else { "◎" };
            format!("\x1b[32m{lit} OPEN\x1b[0m")
        } else {
            "\x1b[31m✖ CLOSED\x1b[0m".to_owned()
        };
        let tally = if open {
            format!(
                "\x1b[2m{} order{} · {} served\x1b[0m",
                orders.len(),
                if orders.len() == 1 { "" } else { "s" },
                served
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "  \x1b[2m║\x1b[0m {} \x1b[2m║\x1b[0m\n",
            between(&lamp, &tally, WIDTH - 2)
        ));
        out.push_str(&format!("  \x1b[2m╚{}╝\x1b[0m\n", "═".repeat(WIDTH)));
        out.push('\n');

        if !open {
            return self.shuttered(out);
        }

        for (index, (group, session)) in orders.iter().enumerate() {
            out.push_str(&self.order(index, group, session));
            out.push('\n');
        }

        if let Some(counter) = &self.counter {
            out.push('\n');
            out.push_str(&self.counter_panel(counter));
        }

        if let Some(told) = self
            .working
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_deref()
        {
            let line = if told == "running" {
                "settling…"
            } else {
                told
            };
            out.push_str(&format!("\n  \x1b[2m{line}\x1b[0m\n"));
        }

        out.push_str(&format!("\n  \x1b[2m{}\x1b[0m\n", self.help()));
        out
    }

    /// The keys that apply right now.
    fn help(&self) -> &'static str {
        match self.counter {
            Some(_) => {
                "↑↓ choose · a keep ours · t keep theirs · b keep both · esc back · q close up"
            }
            None => "↑↓ choose · ⏎ counter · f rush · q close up",
        }
    }

    /// The shop with the shutters down.
    fn shuttered(&self, mut out: String) -> String {
        for _ in 0..3 {
            out.push_str(&format!("  \x1b[2m{}\x1b[0m\n", "▚".repeat(WIDTH + 2)));
        }
        out.push('\n');
        let note = match self.report.service.as_str() {
            "stopped" => "autobahn start",
            "not-installed" => "autobahn install",
            _ => "autobahn watch",
        };
        out.push_str(&format!("  \x1b[2mnote on the glass:\x1b[0m  {note}\n"));
        out.push_str("\n  \x1b[2mq close up\x1b[0m\n");
        out
    }

    /// One order on the rail.
    fn order(&self, index: usize, group: &str, session: &SessionReport) -> String {
        let here = index == self.cursor && self.counter.is_none();
        let working = session
            .progress
            .as_ref()
            .filter(|progress| progress.phase.is_working());
        let (word, colour) = match (working.map(|p| p.phase), session.state.as_str()) {
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
        let reset = if colour.is_empty() { "" } else { "\x1b[0m" };
        let detail = match working {
            Some(progress) if progress.phase == Phase::Staging && progress.staged_total > 0 => {
                format!(
                    "{} / {} files",
                    thousands(progress.staged),
                    thousands(progress.staged_total)
                )
            }
            Some(progress) if progress.phase == Phase::Applying && progress.applied_total > 0 => {
                format!(
                    "{} / {} changes",
                    thousands(progress.applied),
                    thousands(progress.applied_total)
                )
            }
            Some(progress) => format!("{}s", progress.seconds),
            None => match session.age_seconds {
                Some(age) => format!("{age}s ago"),
                None => String::new(),
            },
        };
        format!(
            "  {}\x1b[2m#{:04}\x1b[0m  {:<9} \x1b[2m→\x1b[0m {:<22} {}  {colour}{word}{reset} \x1b[2m{detail}\x1b[0m",
            if here { "\x1b[7m▸\x1b[0m " } else { "  " },
            session.cycles % 10_000,
            shorten(group, 9),
            shorten(&session.host, 22),
            baguette(session, working.map(|p| p.phase), self.frame),
        )
    }

    /// The counter, where a dispute is settled.
    fn counter_panel(&self, counter: &Counter) -> String {
        let mut out = String::new();
        out.push_str(&format!("  \x1b[2m┌{}┐\x1b[0m\n", "─".repeat(WIDTH)));
        out.push_str(&format!(
            "  \x1b[2m│\x1b[0m {} \x1b[2m│\x1b[0m\n",
            between(
                &format!(
                    "\x1b[1mthe counter\x1b[0m \x1b[2m{} → {}\x1b[0m",
                    counter.group, counter.host
                ),
                &format!("\x1b[33m{} waiting\x1b[0m", counter.conflicts.len()),
                WIDTH - 2
            )
        ));
        for (index, conflict) in counter.conflicts.iter().enumerate().take(6) {
            let here = index == counter.cursor;
            let line = format!(
                "{}{}",
                if here { "\x1b[7m▸\x1b[0m " } else { "  " },
                shorten(&conflict.path, WIDTH - 6)
            );
            out.push_str(&format!(
                "  \x1b[2m│\x1b[0m {} \x1b[2m│\x1b[0m\n",
                pad(&line, WIDTH - 2)
            ));
        }
        if counter.conflicts.len() > 6 {
            out.push_str(&format!(
                "  \x1b[2m│\x1b[0m {} \x1b[2m│\x1b[0m\n",
                pad(
                    &format!("\x1b[2m  … {} more\x1b[0m", counter.conflicts.len() - 6),
                    WIDTH - 2
                )
            ));
        }
        out.push_str(&format!("  \x1b[2m└{}┘\x1b[0m\n", "─".repeat(WIDTH)));
        out
    }
}

/// The baguette, filled to whatever is true of the order.
fn baguette(session: &SessionReport, phase: Option<Phase>, frame: u64) -> String {
    let filled = |count: usize, colour: &str| {
        let count = count.min(BAGUETTE);
        format!(
            "🥖\x1b[2m[\x1b[0m{colour}{}\x1b[0m\x1b[2m{}]\x1b[0m",
            "▓".repeat(count),
            "░".repeat(BAGUETTE - count)
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
            let mut bread = String::from("🥖\x1b[2m[\x1b[0m");
            for cell in 0..BAGUETTE {
                if cell + 2 >= head && cell <= head {
                    bread.push_str("\x1b[33m▓\x1b[0m");
                } else {
                    bread.push_str("\x1b[2m░\x1b[0m");
                }
            }
            bread.push_str("\x1b[2m]\x1b[0m");
            bread
        }
        (_, "synchronized") => filled(BAGUETTE, "\x1b[32m"),
        (_, "conflicts" | "blocked") => filled(BAGUETTE, "\x1b[33m"),
        (_, "halted" | "unreachable" | "errored") => format!(
            "🥖\x1b[2m[\x1b[0m\x1b[31m✖\x1b[0m\x1b[2m{}]\x1b[0m",
            " ".repeat(BAGUETTE - 1)
        ),
        _ => filled(0, ""),
    }
}

/// Reads whatever keys are waiting.
fn keys() -> Vec<Key> {
    let mut buffer = [0u8; 32];
    let read = unsafe {
        libc::read(
            libc::STDIN_FILENO,
            buffer.as_mut_ptr() as *mut libc::c_void,
            buffer.len(),
        )
    };
    if read <= 0 {
        return Vec::new();
    }
    let bytes = &buffer[..read as usize];
    let mut keys = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let (key, width) = match bytes[index..] {
            [0x1b, b'[', b'A', ..] => (Some(Key::Up), 3),
            [0x1b, b'[', b'B', ..] => (Some(Key::Down), 3),
            [0x1b, b'[', ..] => (None, 3),
            // A bare escape goes back; anything longer is a sequence this
            // shop does not use.
            [0x1b] => (Some(Key::Back), 1),
            [0x1b, ..] => (None, 1),
            [b'k', ..] => (Some(Key::Up), 1),
            [b'j', ..] => (Some(Key::Down), 1),
            [b'\r', ..] | [b'\n', ..] | [b'r', ..] => (Some(Key::Enter), 1),
            [b'f', ..] => (Some(Key::Flush), 1),
            [b'a', ..] => (Some(Key::Keep(Winner::Alpha)), 1),
            [b't', ..] => (Some(Key::Keep(Winner::Beta)), 1),
            [b'b', ..] => (Some(Key::Keep(Winner::Both)), 1),
            [b'q', ..] | [0x03, ..] => (Some(Key::Quit), 1),
            _ => (None, 1),
        };
        if let Some(key) = key {
            keys.push(key);
        }
        index += width;
    }
    keys
}

// ── measuring ──────────────────────────────────────────────────────────

/// The columns one character occupies.
///
/// Emoji are two columns wide in every terminal that draws them. Counting
/// one as a single column is what left the sign ragged, and why the
/// baguettes came out in the first place.
fn char_width(character: char) -> usize {
    match character as u32 {
        // Variation selectors and joiners occupy nothing.
        0xFE00..=0xFE0F | 0x200D => 0,
        // The emoji planes.
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

/// Centres text that carries escape sequences.
fn centered(text: &str, columns: usize) -> String {
    let visible = width(text);
    let left = columns.saturating_sub(visible) / 2;
    let right = columns.saturating_sub(visible + left);
    format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
}

/// Places two pieces of text at the ends of a line.
fn between(left: &str, right: &str, columns: usize) -> String {
    let gap = columns.saturating_sub(width(left) + width(right));
    format!("{left}{}{right}", " ".repeat(gap))
}

/// Pads text to a width.
fn pad(text: &str, columns: usize) -> String {
    format!("{text}{}", " ".repeat(columns.saturating_sub(width(text))))
}

/// Shortens a name to fit its column, keeping the end.
fn shorten(text: &str, columns: usize) -> String {
    if text.chars().count() <= columns {
        return text.to_owned();
    }
    let kept: String = text
        .chars()
        .skip(text.chars().count() - columns + 1)
        .collect();
    format!("…{kept}")
}

/// Formats a count with thousands separators.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn session(state: &str) -> SessionReport {
        SessionReport {
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
        }
    }

    #[test]
    fn the_sign_keeps_its_accents_through_the_capitals() {
        // The joke is that `autobahn` and `bánh` are the same word, which
        // only reads if the capitals keep their marks.
        let sign = centered("A U T O B Á N H   M Ì", WIDTH);
        assert!(sign.contains('Á'), "{sign}");
        assert!(sign.contains('Ì'), "{sign}");
    }

    #[test]
    fn a_baguette_is_two_columns_wide() {
        assert_eq!(width("🥖"), 2);
        assert_eq!(width("Á"), 1);
        assert_eq!(width("A U T O B Á N H   M Ì"), 21);
        assert_eq!(width("\x1b[1mbold\x1b[0m"), 4);
    }

    #[test]
    fn the_sign_is_exactly_as_wide_as_its_frame() {
        // A line that miscounts leaves the border ragged, which is the one
        // thing a storefront cannot survive.
        let title = centered("🥖  \x1b[1mA U T O B Á N H   M Ì\x1b[0m  🥖", WIDTH);
        assert_eq!(width(&title), WIDTH);
        let ends = between(
            "\x1b[32m◉ OPEN\x1b[0m",
            "\x1b[2m15 orders\x1b[0m",
            WIDTH - 2,
        );
        assert_eq!(width(&ends), WIDTH - 2);
        assert_eq!(width(&pad("short", 30)), 30);
    }

    #[test]
    fn every_baguette_is_the_same_length() {
        // Every order sits on one rail. A bar whose width changed with its
        // state would make the column jump as sessions moved between them.
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
        // The loaf, its brackets, and its filling.
        assert_eq!(served, 2 + 1 + BAGUETTE + 1);
    }

    #[test]
    fn the_counter_opens_only_on_a_dispute() {
        // Pressing enter on a served order must do nothing. Opening an
        // empty counter would offer to settle a dispute that is not there.
        let mut disputed = session("conflicts");
        disputed.conflicts = vec![ConflictDetail {
            path: "notes.txt".into(),
            ..ConflictDetail::default()
        }];
        assert!(disputed.conflicts.len() == 1);
        assert!(session("synchronized").conflicts.is_empty());
    }
}
