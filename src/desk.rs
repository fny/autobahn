//! Autobahn Desk: a window over the fleet.
//!
//! Personal, unshipped, and deliberately undocumented — it lives beside
//! `apps/personal/build.sh` and nothing in `docs/` mentions it.
//!
//! The rule it is built around: Desk reconciles nothing, scans nothing,
//! and writes into no synchronized tree. Every fact comes from the same
//! `status --json` document the shop and the tray read, and every action
//! is either a control request or the CLI invocation a terminal would
//! run. When Desk and the command line disagree, Desk is wrong.
//!
//! It therefore holds no state of its own beyond what a window needs:
//! which pane is open, what is selected, the filters, and the last
//! report it read.
//!
//! The window is drawn with gpui, so the layout below is written the way
//! a web page is: rows and columns with a gap between them. Two rules
//! keep it aligned. Every measurement comes from [`STEP`] — 4 points,
//! doubled and tripled — and every column of numbers has a fixed width
//! and is right-aligned in it, so cycles line up under cycles and ages
//! under ages however long the host name beside them runs.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui::prelude::*;
use gpui::{
    div, px, rgb, rgba, size, AnyElement, App, Application, Bounds, Context, Div, FontWeight,
    KeyBinding, Menu, MenuItem, Rgba, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};

use crate::supervisor::{status_report, GroupReport, SessionReport, StatusReport};

/// How often the fleet is re-read when nothing is working, and when
/// something is. A session mid-scan reports progress that is worth
/// watching; a quiet fleet is not, and a window that repaints twice a
/// second on battery is the thing MAC-7 complains about.
const POLL_AT_REST: Duration = Duration::from_secs(2);
const POLL_WHILE_WORKING: Duration = Duration::from_millis(500);

// ── the design ───────────────────────────────────────────────────────

/// The one spacing unit. Every gap, pad and row height below is `STEP`
/// times a small whole number, which is what keeps the panes looking
/// like one app rather than four.
const STEP: f32 = 4.0;

/// Grounds, from the back of the window forward.
const GROUND: u32 = 0x10141a;
const RAIL: u32 = 0x0b0e13;
const PANEL: u32 = 0x171c23;
const SUNK: u32 = 0x12161c;
const RAISED: u32 = 0x1e242d;
const LINE: u32 = 0x282f39;
const HAIR: u32 = 0x1d232b;

/// Ink, from the thing you read first to the thing you read last.
const INK: u32 = 0xe7ebf0;
const DIM: u32 = 0x9ba6b2;
const FAINT: u32 = 0x69727e;

/// The four state colours, the same ones the shop and the tray use.
const GREEN: u32 = 0x3fb97a;
const AMBER: u32 = 0xe0ae42;
const RED: u32 = 0xe8796a;
const BLUE: u32 = 0x6ea8f0;

/// The type scale, in points.
const T_PILL: f32 = 10.5;
const T_META: f32 = 11.0;
const T_DATA: f32 = 12.0;
const T_ROW: f32 = 12.5;
const T_BODY: f32 = 13.0;
const T_GROUP: f32 = 14.5;
const T_TITLE: f32 = 16.5;

/// The fixed columns of a session row, so every card measures the same.
const W_MODE: f32 = 88.0;
const W_CYCLES: f32 = 100.0;
const W_AGE: f32 = 64.0;
const W_STATE: f32 = 148.0;

fn step(n: f32) -> gpui::Pixels {
    px(STEP * n)
}

/// A colour at an alpha, for the ground behind a pill.
fn tint(colour: u32, alpha: u32) -> Rgba {
    rgba((colour << 8) | alpha)
}

/// The panes, in the order the rail lists them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Fleet,
    Conflicts,
    Log,
    Hosts,
}

impl Pane {
    fn title(self) -> &'static str {
        match self {
            Pane::Fleet => "Fleet",
            Pane::Conflicts => "Conflicts",
            Pane::Log => "Log",
            Pane::Hosts => "Hosts",
        }
    }

    /// The line under the title: what this pane is for.
    fn about(self) -> &'static str {
        match self {
            Pane::Fleet => "every group, every session, and what each one last did",
            Pane::Conflicts => "the paths waiting on a person",
            Pane::Log => "the supervisor's own account of itself",
            Pane::Hosts => "the machines the fleet talks to, and the bundle they run",
        }
    }
}

/// What the window is showing, and what it last did.
pub struct Desk {
    config: Option<PathBuf>,
    state_root: PathBuf,
    pane: Pane,
    report: Option<StatusReport>,
    read_at: Option<Instant>,
    /// The session the fleet pane has open, as (group, session key).
    selected: Option<(String, crate::supervisor::control::SessionKey)>,
    /// The conflict the conflicts pane has open, and its diff once read.
    conflict: Option<Conflict>,
    diff: Option<String>,
    log: Vec<String>,
    /// Which of the supervisor's two log files the lines came from.
    log_path: Option<PathBuf>,
    errors_only: bool,
    /// The last thing an action said, kept until the next one.
    said: Option<String>,
    /// The two faces: one to read words in, one to line numbers up in.
    sans: SharedString,
    mono: SharedString,
}

#[derive(Clone, PartialEq, Eq)]
struct Conflict {
    group: String,
    host: String,
    path: String,
    blocked: bool,
}

/// Runs the window until it is closed.
pub fn run(config: Option<PathBuf>, state_root: PathBuf) -> Result<()> {
    run_with(config, state_root, None)
}

/// The same window, told to photograph itself into `directory` and quit.
/// Personal tooling for a personal app: it is how the design document's
/// screenshots are made. gpui hands back no frame of its own, so the
/// window asks the window server for the rectangle it occupies — which a
/// process may do for its own windows without being granted the screen
/// recording a `screencapture` of the whole display would need.
pub fn shoot(config: Option<PathBuf>, state_root: PathBuf, directory: PathBuf) -> Result<()> {
    run_with(config, state_root, Some(directory))
}

gpui::actions!(desk, [Quit]);

fn run_with(config: Option<PathBuf>, state_root: PathBuf, shots: Option<PathBuf>) -> Result<()> {
    Application::new().run(move |cx: &mut App| {
        cx.activate(true);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.set_menus(vec![Menu {
            name: "Autobahn Desk".into(),
            items: vec![MenuItem::action("Quit", Quit)],
        }]);
        // A window over the fleet is the whole app: when it closes, the
        // app has nothing left to be.
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1240.), px(820.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Autobahn Desk".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let config = config.clone();
        let state_root = state_root.clone();
        let shots = shots.clone();
        let window = cx.open_window(options, |_, cx| {
            cx.new(|cx| Desk::new(config, state_root, cx))
        });
        let Ok(window) = window else { return };
        if let Some(directory) = shots {
            window
                .update(cx, |desk, window, cx| desk.photograph(directory, window, cx))
                .ok();
        }
    });
    Ok(())
}

impl Desk {
    fn new(config: Option<PathBuf>, state_root: PathBuf, cx: &mut Context<Self>) -> Self {
        let names = cx.text_system().all_font_names();
        let pick = |candidates: &[&str], fallback: &str| -> SharedString {
            for candidate in candidates {
                if names.iter().any(|name| name == candidate) {
                    return SharedString::from(candidate.to_string());
                }
            }
            SharedString::from(fallback.to_string())
        };
        let mut desk = Desk {
            config,
            state_root,
            pane: Pane::Fleet,
            report: None,
            read_at: None,
            selected: None,
            conflict: None,
            diff: None,
            log: Vec::new(),
            log_path: None,
            errors_only: false,
            said: None,
            sans: pick(&["SF Pro Text", "SF Pro Display", "Helvetica Neue"], "Helvetica"),
            mono: pick(&["SF Mono", "Menlo", "Monaco"], "Menlo"),
        };
        desk.refresh();
        // The fleet is re-read on a timer rather than per frame, so a
        // window that nobody is looking at costs nothing but the read.
        cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(POLL_WHILE_WORKING).await;
                let carried = this.update(cx, |this, cx| {
                    if this.refresh_if_due() {
                        cx.notify();
                    }
                });
                if carried.is_err() {
                    break;
                }
            }
        })
        .detach();
        desk
    }
}

impl Render for Desk {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = self.pane;
        div()
            .size_full()
            .flex()
            .font_family(self.sans.clone())
            .text_size(px(T_BODY))
            .text_color(rgb(INK))
            .bg(rgb(GROUND))
            .child(self.rail(cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(self.header())
                    .child(match pane {
                        Pane::Fleet => self.fleet(cx),
                        Pane::Conflicts => self.conflicts(cx),
                        Pane::Log => self.log_pane(cx),
                        Pane::Hosts => self.hosts(),
                    })
                    .child(self.footer()),
            )
    }
}

impl Desk {
    // ── the chrome ───────────────────────────────────────────────────

    /// The left rail: the name of the app, the panes, and the two facts
    /// that are true of the whole fleet whichever pane is open.
    fn rail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let waiting = self.waiting();
        let running = self.report.as_ref().map(|report| report.supervisor_running);
        let (service, colour) = match running {
            Some(true) => ("supervisor running", GREEN),
            Some(false) => ("no supervisor", RED),
            None => ("reading…", FAINT),
        };
        div()
            .w(px(212.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(RAIL))
            .border_r_1()
            .border_color(rgb(LINE))
            .child(
                // The traffic lights sit over this block, so it starts
                // below them rather than beside them.
                div()
                    .pt(step(9.))
                    .px(step(4.5))
                    .pb(step(4.))
                    .flex()
                    .flex_col()
                    .gap(step(0.5))
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(step(1.5))
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("autobahn"),
                            )
                            .child(
                                div()
                                    .text_size(px(T_ROW))
                                    .text_color(rgb(FAINT))
                                    .child("desk"),
                            ),
                    )
                    .child(
                        div()
                            .font_family(self.mono.clone())
                            .text_size(px(T_PILL))
                            .text_color(rgb(FAINT))
                            .child(crate::protocol::version()),
                    ),
            )
            .child(
                div()
                    .px(step(2.5))
                    .flex()
                    .flex_col()
                    .gap(step(0.5))
                    .child(self.nav(Pane::Fleet, None, cx))
                    .child(self.nav(Pane::Conflicts, Some(waiting), cx))
                    .child(self.nav(Pane::Log, None, cx))
                    .child(self.nav(Pane::Hosts, None, cx)),
            )
            .child(div().flex_1())
            .child(
                div()
                    .px(step(4.5))
                    .pb(step(4.))
                    .flex()
                    .flex_col()
                    .gap(step(1.5))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(step(2.))
                            .child(dot(colour))
                            .child(
                                div()
                                    .text_size(px(T_META))
                                    .text_color(rgb(DIM))
                                    .child(service),
                            ),
                    )
                    .child(
                        div()
                            .font_family(self.mono.clone())
                            .text_size(px(T_PILL))
                            .text_color(rgb(FAINT))
                            .truncate()
                            .child(self.state_root.display().to_string()),
                    ),
            )
            .into_any_element()
    }

    fn nav(&self, pane: Pane, badge: Option<usize>, cx: &mut Context<Self>) -> AnyElement {
        let chosen = self.pane == pane;
        div()
            .id(SharedString::from(format!("nav-{}", pane.title())))
            .h(step(7.5))
            .px(step(2.5))
            .rounded(px(6.))
            .flex()
            .items_center()
            .justify_between()
            .cursor_pointer()
            .text_size(px(T_ROW))
            .when(chosen, |row| row.bg(rgb(RAISED)).text_color(rgb(INK)))
            .when(!chosen, |row| {
                row.text_color(rgb(DIM)).hover(|row| row.bg(rgb(SUNK)))
            })
            .child(pane.title())
            .when_some(badge.filter(|count| *count > 0), |row, count| {
                row.child(pill(count.to_string(), AMBER))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.pane = pane;
                if pane == Pane::Log && this.log.is_empty() {
                    this.read_log();
                }
                cx.notify();
            }))
            .into_any_element()
    }

    /// The header: which pane, what it is for, and what the whole fleet
    /// adds up to. The three counts are always in the same order and the
    /// same place, so the one that matters is found without reading.
    fn header(&self) -> AnyElement {
        let (needs, away, fine) = self.tally();
        let groups = self.report.as_ref().map_or(0, |report| report.groups.len());
        let sessions = self.sessions().len();
        div()
            .h(step(14.))
            .flex_shrink_0()
            .px(step(6.))
            .flex()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(rgb(LINE))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(
                        div()
                            .text_size(px(T_TITLE))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.pane.title()),
                    )
                    .child(
                        div()
                            .text_size(px(T_META))
                            .text_color(rgb(FAINT))
                            .child(match self.report {
                                None => self.pane.about().to_owned(),
                                Some(_) => format!(
                                    "{groups} groups · {sessions} sessions · {}",
                                    self.pane.about()
                                ),
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(step(2.))
                    .child(count(needs, "need you", AMBER))
                    .child(count(away, "away", RED))
                    .child(count(fine, "synchronized", GREEN)),
            )
            .into_any_element()
    }

    /// The bottom line: what the last action said, and how fresh the
    /// numbers above it are.
    fn footer(&self) -> AnyElement {
        let age = self
            .read_at
            .map(|at| format_age(at.elapsed().as_secs()))
            .unwrap_or_else(|| "never".to_owned());
        div()
            .h(step(7.5))
            .flex_shrink_0()
            .px(step(6.))
            .flex()
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(rgb(LINE))
            .bg(rgb(RAIL))
            .font_family(self.mono.clone())
            .text_size(px(T_META))
            .child(match &self.said {
                Some(said) => div()
                    .text_color(rgb(DIM))
                    .truncate()
                    .child(crate::text::display_safe(said).to_string()),
                None => div()
                    .text_color(rgb(FAINT))
                    .child("every number here came from status --json"),
            })
            .child(
                div()
                    .flex_shrink_0()
                    .pl(step(4.))
                    .text_color(rgb(FAINT))
                    .child(format!("read {age} ago")),
            )
            .into_any_element()
    }

    // ── the fleet ────────────────────────────────────────────────────

    fn fleet(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(report) = self.report.clone() else {
            return empty("reading the fleet…");
        };
        div()
            .id("fleet")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p(step(6.))
            .flex()
            .flex_col()
            .gap(step(4.))
            .children(
                report
                    .groups
                    .iter()
                    .map(|group| self.band(group, cx))
                    .collect::<Vec<_>>(),
            )
            .into_any_element()
    }

    /// One group as one card: its name and root across the top, then a
    /// row for each session under it.
    fn band(&mut self, group: &GroupReport, cx: &mut Context<Self>) -> AnyElement {
        let (summary, colour) = summarize(&group.sessions);
        let rows: Vec<AnyElement> = group
            .sessions
            .iter()
            .enumerate()
            .map(|(index, session)| self.session_row(group, session, index > 0, cx))
            .collect();
        div()
            .flex()
            .flex_col()
            .rounded(px(8.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(LINE))
            .child(
                div()
                    .h(step(11.))
                    .px(step(4.))
                    .flex()
                    .items_center()
                    .gap(step(2.5))
                    .border_b_1()
                    .border_color(rgb(HAIR))
                    .child(
                        div()
                            .text_size(px(T_GROUP))
                            .font_weight(FontWeight::SEMIBOLD)
                            .flex_shrink_0()
                            .child(group.name.clone()),
                    )
                    .when(!group.role.is_empty(), |head| {
                        head.child(pill(
                            format!("{} · term {}", group.role, group.term),
                            BLUE,
                        ))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_family(self.mono.clone())
                            .text_size(px(T_META))
                            .text_color(rgb(FAINT))
                            .truncate()
                            .child(group.alpha.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(T_META))
                            .text_color(rgb(colour))
                            .child(summary),
                    ),
            )
            .children(rows)
            .into_any_element()
    }

    fn session_row(
        &mut self,
        group: &GroupReport,
        session: &SessionReport,
        divider: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = (group.name.clone(), session.session.clone());
        let open = self.selected.as_ref() == Some(&key);
        let severity = severity(&session.state);
        let working = session
            .progress
            .as_ref()
            .is_some_and(|progress| progress.phase.is_working());
        div()
            .flex()
            .flex_col()
            .when(divider, |band| band.border_t_1().border_color(rgb(HAIR)))
            .child(
                div()
                    .id(SharedString::from(format!("row-{}-{}", group.name, session.beta)))
                    .h(step(9.))
                    .px(step(4.))
                    .flex()
                    .items_center()
                    .gap(step(3.))
                    .cursor_pointer()
                    .when(open, |row| row.bg(rgb(RAISED)))
                    .hover(|row| row.bg(rgb(RAISED)))
                    .child(dot(colour_of(severity)))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_family(self.mono.clone())
                            .text_size(px(T_ROW))
                            .truncate()
                            .child(tilde(&crate::text::display_safe(&session.beta))),
                    )
                    .child(
                        div()
                            .w(px(W_MODE))
                            .flex_shrink_0()
                            .text_size(px(T_META))
                            .text_color(rgb(FAINT))
                            .truncate()
                            .child(session.mode.clone()),
                    )
                    .child(
                        div()
                            .w(px(W_CYCLES))
                            .flex_shrink_0()
                            .text_right()
                            .font_family(self.mono.clone())
                            .text_size(px(T_META))
                            .text_color(rgb(DIM))
                            .child(format!("{} cycles", thousands(session.cycles))),
                    )
                    .child(
                        div()
                            .w(px(W_AGE))
                            .flex_shrink_0()
                            .text_right()
                            .font_family(self.mono.clone())
                            .text_size(px(T_META))
                            .text_color(rgb(FAINT))
                            .child(match session.age_seconds {
                                Some(age) => format!("{} ago", format_age(age)),
                                None => "never".to_owned(),
                            }),
                    )
                    .child(
                        div()
                            .w(px(W_STATE))
                            .flex_shrink_0()
                            .flex()
                            .justify_end()
                            .child(pill(state_words(session), colour_of(severity))),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = match this.selected.as_ref() == Some(&key) {
                            true => None,
                            false => Some(key.clone()),
                        };
                        cx.notify();
                    })),
            )
            // A session that is working says what it is doing, because
            // that is the one thing a recorded status cannot tell you.
            .when_some(
                session.progress.as_ref().filter(|_| working),
                |band, progress| {
                    band.child(self.aside(describe(progress), BLUE))
                },
            )
            .when_some(session.error.as_ref(), |band, error| {
                band.child(self.aside(crate::text::display_safe(error).to_string(), RED))
            })
            .when(open, |band| band.child(self.detail(group, session, cx)))
            .into_any_element()
    }

    /// A line hanging under a session row, indented past its dot.
    fn aside(&self, text: String, colour: u32) -> Div {
        div()
            .pl(step(8.))
            .pr(step(4.))
            .pb(step(2.))
            .font_family(self.mono.clone())
            .text_size(px(T_META))
            .text_color(rgb(colour))
            .child(text)
    }

    /// The open session: both roots, what it last did, what is stuck, and
    /// the four things that can be asked of it.
    fn detail(
        &mut self,
        group: &GroupReport,
        session: &SessionReport,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let waiting: Vec<String> = session
            .blocked
            .iter()
            .map(|blocked| crate::text::display_safe(blocked).to_string())
            .chain(
                session
                    .conflicts
                    .iter()
                    .map(|conflict| crate::text::display_safe(&conflict.path).to_string()),
            )
            .collect();
        div()
            .bg(rgb(SUNK))
            .border_t_1()
            .border_color(rgb(HAIR))
            .p(step(4.))
            .flex()
            .gap(step(6.))
            .child(
                div()
                    .w(px(380.))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(step(1.5))
                    .child(self.pair("alpha", tilde(&group.alpha)))
                    .child(self.pair("beta", tilde(&session.beta)))
                    .child(self.pair("mode", session.mode.clone()))
                    .child(self.pair("cycles", thousands(session.cycles)))
                    .child(self.pair("state", session.state.clone()))
                    .child(
                        div()
                            .pt(step(2.))
                            .flex()
                            .gap(step(1.5))
                            .child(self.verb(group, session, Verb::Flush, cx))
                            .child(self.verb(group, session, Verb::Verify, cx))
                            .child(self.verb(group, session, Verb::Pause, cx))
                            .child(self.verb(group, session, Verb::Resume, cx)),
                    )
                    .child(
                        div()
                            .pt(step(1.))
                            .text_size(px(T_META))
                            .text_color(rgb(FAINT))
                            .child(
                                "Reset is not here on purpose: it resurrects deletions, and \
                                 wants a sentence of its own before it is offered.",
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .pl(step(6.))
                    .border_l_1()
                    .border_color(rgb(HAIR))
                    .flex()
                    .flex_col()
                    .gap(step(1.))
                    .child(label("waiting on a person"))
                    .when(waiting.is_empty(), |column| {
                        column.child(
                            div()
                                .text_size(px(T_META))
                                .text_color(rgb(FAINT))
                                .child("nothing is waiting"),
                        )
                    })
                    .children(waiting.into_iter().map(|path| {
                        div()
                            .font_family(self.mono.clone())
                            .text_size(px(T_META))
                            .text_color(rgb(AMBER))
                            .truncate()
                            .child(path)
                    })),
            )
            .into_any_element()
    }

    fn pair(&self, name: &'static str, value: String) -> Div {
        div()
            .flex()
            .items_baseline()
            .gap(step(2.5))
            .child(
                div()
                    .w(px(52.))
                    .flex_shrink_0()
                    .text_size(px(T_PILL))
                    .text_color(rgb(FAINT))
                    .child(name),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .font_family(self.mono.clone())
                    .text_size(px(T_DATA))
                    .truncate()
                    .child(value),
            )
    }

    fn verb(
        &self,
        group: &GroupReport,
        session: &SessionReport,
        verb: Verb,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = group.name.clone();
        let beta = session.beta.clone();
        let key = session.session.clone();
        button(
            format!("verb-{}-{}-{}", name, beta, verb.word()),
            verb.word(),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.control(&name, &beta, &key, verb);
            cx.notify();
        }))
        .into_any_element()
    }

    // ── the conflicts ────────────────────────────────────────────────

    /// Every conflict and blocked path in the fleet down the left, and
    /// the one that is open on the right.
    fn conflicts(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let waiting = self.waiting_list();
        if waiting.is_empty() {
            return empty("nothing needs you");
        }
        let open = self.conflict.clone();
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .child(
                div()
                    .id("queue")
                    .w(px(352.))
                    .flex_shrink_0()
                    .h_full()
                    .overflow_y_scroll()
                    .py(step(3.))
                    .px(step(3.))
                    .flex()
                    .flex_col()
                    .gap(step(0.5))
                    .border_r_1()
                    .border_color(rgb(LINE))
                    .bg(rgb(SUNK))
                    .children(
                        waiting
                            .iter()
                            .enumerate()
                            .map(|(index, item)| {
                                let chosen = open.as_ref() == Some(item);
                                let item = item.clone();
                                let name = item.path.rsplit('/').next().unwrap_or(&item.path);
                                div()
                                    .id(SharedString::from(format!("waiting-{index}")))
                                    .px(step(2.5))
                                    .py(step(1.5))
                                    .rounded(px(6.))
                                    .cursor_pointer()
                                    .flex()
                                    .flex_col()
                                    .gap(px(1.))
                                    .when(chosen, |row| row.bg(rgb(RAISED)))
                                    .hover(|row| row.bg(rgb(PANEL)))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(step(1.5))
                                            .child(dot(match item.blocked {
                                                true => RED,
                                                false => AMBER,
                                            }))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(px(0.))
                                                    .font_family(self.mono.clone())
                                                    .text_size(px(T_ROW))
                                                    .truncate()
                                                    .child(
                                                        crate::text::display_safe(name)
                                                            .to_string(),
                                                    ),
                                            )
                                            .child(pill(
                                                match item.blocked {
                                                    true => "blocked",
                                                    false => "conflict",
                                                },
                                                match item.blocked {
                                                    true => RED,
                                                    false => AMBER,
                                                },
                                            )),
                                    )
                                    .child(
                                        div()
                                            .pl(step(3.5))
                                            .font_family(self.mono.clone())
                                            .text_size(px(T_PILL))
                                            .text_color(rgb(FAINT))
                                            .truncate()
                                            .child(match item.path.rsplit_once('/') {
                                                Some((directory, _)) => format!(
                                                    "{} · {}/",
                                                    item.group,
                                                    crate::text::display_safe(directory)
                                                ),
                                                None => item.group.clone(),
                                            }),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.conflict = Some(item.clone());
                                        this.diff = None;
                                        cx.notify();
                                    }))
                            })
                            .collect::<Vec<_>>(),
                    ),
            )
            .child(match self.conflict.clone() {
                None => empty("pick one from the left"),
                Some(item) => self.conflict_detail(item, cx),
            })
            .into_any_element()
    }

    fn conflict_detail(&mut self, item: Conflict, cx: &mut Context<Self>) -> AnyElement {
        let keep_host = match item.host.contains(':') {
            true => short_name(&item.host),
            false => "beta".to_owned(),
        };
        let diff = self.diff.clone();
        div()
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .px(step(6.))
                    .pt(step(5.))
                    .pb(step(4.))
                    .flex()
                    .flex_col()
                    .gap(step(2.))
                    .border_b_1()
                    .border_color(rgb(LINE))
                    .child(label(match item.blocked {
                        true => "blocked path",
                        false => "conflict",
                    }))
                    .child(
                        div()
                            .font_family(self.mono.clone())
                            .text_size(px(T_BODY))
                            .child(crate::text::display_safe(&item.path).to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(T_META))
                            .text_color(rgb(FAINT))
                            .child(format!("{} · {}", item.group, tilde(&item.host))),
                    )
                    .when(item.blocked, |head| {
                        head.child(
                            div()
                                .pt(step(1.))
                                .text_size(px(T_META))
                                .text_color(rgb(DIM))
                                .child(
                                    "A blocked path is a filesystem to fix, not a version to \
                                     choose: something on one side cannot be written where the \
                                     other side wants it.",
                                ),
                        )
                    })
                    .when(!item.blocked, |head| {
                        let alpha = item.clone();
                        let host = item.clone();
                        let both = item.clone();
                        let shown = item.clone();
                        head.child(
                            div()
                                .pt(step(1.))
                                .flex()
                                .gap(step(1.5))
                                .child(
                                    button("keep-alpha", "Keep alpha").on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.resolve(&alpha, "alpha");
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    button("keep-beta", format!("Keep {keep_host}")).on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            let keep = host.host.clone();
                                            this.resolve(&host, &keep);
                                            cx.notify();
                                        }),
                                    ),
                                )
                                .child(button("keep-both", "Keep both").on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.resolve(&both, "both");
                                        cx.notify();
                                    },
                                )))
                                .child(
                                    button("show-diff", "Show the difference").on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.read_diff(&shown);
                                            cx.notify();
                                        }),
                                    ),
                                ),
                        )
                    }),
            )
            .when_some(diff, |column, diff| {
                column.child(
                    div()
                        .id("diff")
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_scroll()
                        .px(step(4.5))
                        .py(step(4.))
                        .font_family(self.mono.clone())
                        .text_size(px(T_META))
                        .flex()
                        .flex_col()
                        .children(diff.lines().map(|line| {
                            let (colour, ground) = match line.chars().next() {
                                Some('+') => (GREEN, tint(GREEN, 0x14)),
                                Some('-') => (RED, tint(RED, 0x14)),
                                Some('@') => (BLUE, tint(BLUE, 0x10)),
                                _ => (DIM, rgba(0x00000000)),
                            };
                            div()
                                .px(step(1.5))
                                .bg(ground)
                                .text_color(rgb(colour))
                                .whitespace_nowrap()
                                .child(crate::text::display_safe(line).to_string())
                        })),
                )
            })
            .into_any_element()
    }

    // ── the log ──────────────────────────────────────────────────────

    /// The supervisor's own account, filtered — the file is megabytes and
    /// the line anyone wants is one of hundreds of thousands.
    fn log_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let errors_only = self.errors_only;
        let lines: Vec<String> = self
            .log
            .iter()
            .filter(|line| !errors_only || is_complaint(line))
            .cloned()
            .collect();
        let shown = lines.len();
        let held = self.log.len();
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(step(11.))
                    .flex_shrink_0()
                    .px(step(6.))
                    .flex()
                    .items_center()
                    .gap(step(2.))
                    .border_b_1()
                    .border_color(rgb(LINE))
                    .bg(rgb(SUNK))
                    .child(
                        toggle("errors-only", "errors only", errors_only).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.errors_only = !this.errors_only;
                                cx.notify();
                            },
                        )),
                    )
                    .child(button("re-read", "Re-read").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.read_log();
                            cx.notify();
                        },
                    )))
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(step(2.5))
                            .font_family(self.mono.clone())
                            .text_size(px(T_META))
                            .text_color(rgb(FAINT))
                            .when_some(self.log_path.clone(), |line, path| {
                                line.child(tilde(&path.display().to_string()))
                            })
                            .child(format!("{shown} of the last {held} lines")),
                    ),
            )
            .child(
                div()
                    .id("log")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_scroll()
                    .py(step(3.))
                    .px(step(6.))
                    .font_family(self.mono.clone())
                    .text_size(px(T_META))
                    .flex()
                    .flex_col()
                    .children(lines.into_iter().map(|line| {
                        let complaint = is_complaint(&line);
                        let chatter = line.contains("debug:");
                        div()
                            .pl(step(1.5))
                            .pr(step(1.5))
                            .py(px(1.))
                            .border_l_2()
                            .border_color(match complaint {
                                true => tint(RED, 0xc0),
                                false => rgba(0x00000000),
                            })
                            .when(complaint, |row| row.bg(tint(RED, 0x0a)))
                            .text_color(rgb(match (complaint, chatter) {
                                (true, _) => RED,
                                (_, true) => FAINT,
                                _ => DIM,
                            }))
                            .whitespace_nowrap()
                            .child(crate::text::display_safe(&line).to_string())
                    })),
            )
            .into_any_element()
    }

    // ── the hosts ────────────────────────────────────────────────────

    /// Every host the fleet talks to, what it is carrying, and whether
    /// this build can talk to it.
    fn hosts(&mut self) -> AnyElement {
        let Some(report) = &self.report else {
            return empty("reading the fleet…");
        };
        let mut hosts: Vec<(String, usize, Severity, String, Option<String>)> = Vec::new();
        for group in &report.groups {
            for session in &group.sessions {
                let severity = severity(&session.state);
                match hosts.iter_mut().find(|(host, ..)| host == &session.host) {
                    Some((_, count, worst, state, error)) => {
                        *count += 1;
                        if severity > *worst {
                            *worst = severity;
                            *state = session.state.clone();
                            *error = session.error.clone();
                        }
                    }
                    None => hosts.push((
                        session.host.clone(),
                        1,
                        severity,
                        session.state.clone(),
                        session.error.clone(),
                    )),
                }
            }
        }
        hosts.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        let manifest = std::fs::read_to_string(self.state_root.join("agents").join("MANIFEST"));
        let rows: Vec<Div> = hosts
            .into_iter()
            .enumerate()
            .map(|(index, (host, count, worst, state, error))| {
                // The error if there is one, and the state word if there
                // is not, so no row is blank and none of them lies.
                let complaint = error
                    .filter(|_| worst != Severity::Fine)
                    .map(|error| {
                        crate::text::display_safe(
                            error.rsplit(": ").next().unwrap_or(error.as_str()),
                        )
                        .to_string()
                    })
                    .unwrap_or(state);
                div()
                    .h(step(9.))
                    .px(step(4.))
                    .flex()
                    .items_center()
                    .gap(step(3.))
                    .when(index > 0, |row| row.border_t_1().border_color(rgb(HAIR)))
                    .child(dot(colour_of(worst)))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_family(self.mono.clone())
                            .text_size(px(T_ROW))
                            .truncate()
                            .child(tilde(&host)),
                    )
                    .child(
                        div()
                            .w(px(320.))
                            .flex_shrink_0()
                            .font_family(self.mono.clone())
                            .text_size(px(T_META))
                            .text_color(rgb(colour_of(worst)))
                            .truncate()
                            .child(complaint),
                    )
                    .child(
                        div()
                            .w(px(88.))
                            .flex_shrink_0()
                            .text_right()
                            .text_size(px(T_META))
                            .text_color(rgb(FAINT))
                            .child(match count {
                                1 => "1 session".to_owned(),
                                count => format!("{count} sessions"),
                            }),
                    )
            })
            .collect();
        div()
            .id("hosts")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p(step(6.))
            .flex()
            .flex_col()
            .gap(step(4.))
            .child(
                div()
                    .rounded(px(8.))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(LINE))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(step(7.))
                            .px(step(4.))
                            .flex()
                            .items_center()
                            .gap(step(3.))
                            .border_b_1()
                            .border_color(rgb(HAIR))
                            .child(div().size(px(7.)).flex_shrink_0())
                            .child(div().flex_1().min_w(px(0.)).child(label("host")))
                            .child(div().w(px(320.)).flex_shrink_0().child(label(
                                "what it last said",
                            )))
                            .child(
                                div()
                                    .w(px(88.))
                                    .flex_shrink_0()
                                    .flex()
                                    .justify_end()
                                    .child(label("carrying")),
                            ),
                    )
                    .children(rows),
            )
            .child(
                div()
                    .rounded(px(8.))
                    .bg(rgb(PANEL))
                    .border_1()
                    .border_color(rgb(LINE))
                    .p(step(4.))
                    .flex()
                    .flex_col()
                    .gap(step(2.))
                    .child(label("the agent bundle"))
                    .child(match manifest {
                        Ok(manifest) => div()
                            .font_family(self.mono.clone())
                            .text_size(px(T_META))
                            .text_color(rgb(DIM))
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .children(
                                manifest
                                    .lines()
                                    .map(|line| div().child(line.to_owned()))
                                    .collect::<Vec<_>>(),
                            ),
                        Err(_) => div().text_size(px(T_META)).text_color(rgb(FAINT)).child(
                            format!(
                                "{} carries no manifest, so a stale bundle is caught by the \
                                 handshake rather than before it is sent",
                                tilde(&self.state_root.join("agents").display().to_string())
                            ),
                        ),
                    })
                    .child(
                        div()
                            .pt(step(1.))
                            .font_family(self.mono.clone())
                            .text_size(px(T_META))
                            .text_color(rgb(FAINT))
                            .child(format!("this build is {}", crate::protocol::version())),
                    ),
            )
            .into_any_element()
    }

    // ── the seam ─────────────────────────────────────────────────────

    /// Re-reads the fleet if enough time has passed, and says whether it
    /// did, so a quiet window does not repaint.
    fn refresh_if_due(&mut self) -> bool {
        let due = match self.read_at {
            None => true,
            Some(at) => {
                at.elapsed()
                    >= match self.working() {
                        true => POLL_WHILE_WORKING,
                        false => POLL_AT_REST,
                    }
            }
        };
        if !due {
            return false;
        }
        self.refresh();
        true
    }

    fn refresh(&mut self) {
        self.read_at = Some(Instant::now());
        let path = match &self.config {
            Some(path) => path.clone(),
            None => match crate::paths::default_config_path() {
                Ok(path) => path,
                Err(_) => return,
            },
        };
        match crate::supervisor::shown_plans(&path, &self.state_root) {
            Ok(shown) => {
                let selected: Vec<&crate::config::SessionPlan> = shown.plans.iter().collect();
                self.report = Some(status_report(&selected, &self.state_root));
            }
            Err(error) => {
                self.said = Some(format!("the configuration does not load: {error:#}"));
            }
        }
    }

    fn working(&self) -> bool {
        self.sessions().iter().any(|session| {
            session
                .progress
                .as_ref()
                .is_some_and(|progress| progress.phase.is_working())
        })
    }

    fn sessions(&self) -> Vec<&SessionReport> {
        self.report
            .iter()
            .flat_map(|report| report.groups.iter())
            .flat_map(|group| group.sessions.iter())
            .collect()
    }

    /// How many sessions need a person, are away, and are fine.
    fn tally(&self) -> (usize, usize, usize) {
        let mut needs = 0;
        let mut away = 0;
        let mut fine = 0;
        for session in self.sessions() {
            match severity(&session.state) {
                Severity::Bad => away += 1,
                Severity::Attention => needs += 1,
                Severity::Fine => fine += 1,
            }
        }
        (needs, away, fine)
    }

    fn waiting(&self) -> usize {
        self.waiting_list().len()
    }

    fn waiting_list(&self) -> Vec<Conflict> {
        let mut waiting = Vec::new();
        for group in self.report.iter().flat_map(|report| report.groups.iter()) {
            for session in &group.sessions {
                for conflict in &session.conflicts {
                    waiting.push(Conflict {
                        group: group.name.clone(),
                        host: session.host.clone(),
                        path: conflict.path.clone(),
                        blocked: false,
                    });
                }
                for blocked in &session.blocked {
                    waiting.push(Conflict {
                        group: group.name.clone(),
                        host: session.host.clone(),
                        path: blocked.clone(),
                        blocked: true,
                    });
                }
            }
        }
        waiting
    }

    /// A control request, for the one session the row belongs to.
    fn control(
        &mut self,
        group: &str,
        beta: &str,
        session: &crate::supervisor::control::SessionKey,
        verb: Verb,
    ) {
        let selector = crate::supervisor::control::Selector {
            group: Some(group.to_owned()),
            host: Some(beta.to_owned()),
            session: Some(session.clone()),
        };
        let request = match verb {
            Verb::Flush => crate::supervisor::control::ControlRequest::Flush(selector),
            Verb::Verify => crate::supervisor::control::ControlRequest::Verify(selector),
            Verb::Pause => crate::supervisor::control::ControlRequest::Pause(selector),
            Verb::Resume => crate::supervisor::control::ControlRequest::Resume(selector),
        };
        self.said = Some(
            match crate::supervisor::control::send(&self.state_root, &request) {
                Ok(_) => format!("{} {beta}", verb.done()),
                Err(error) => format!("{error:#}"),
            },
        );
        self.read_at = None;
    }

    /// Resolution runs the CLI, so the app cannot settle a conflict in a
    /// way a terminal could not.
    fn resolve(&mut self, item: &Conflict, keep: &str) {
        let mut command = std::process::Command::new(exe());
        command
            .arg("resolve")
            .arg(&item.group)
            .arg(&item.path)
            .arg("--keep")
            .arg(keep)
            .arg("--yes")
            .arg("--state-root")
            .arg(&self.state_root);
        if let Some(config) = &self.config {
            command.arg("--config").arg(config);
        }
        let output = command.output();
        self.said = Some(match output {
            Ok(output) if output.status.success() => {
                self.conflict = None;
                self.diff = None;
                format!("kept {keep}: {}", crate::text::display_safe(&item.path))
            }
            Ok(output) => String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            Err(error) => format!("unable to run resolve: {error}"),
        });
        self.read_at = None;
    }

    fn read_diff(&mut self, item: &Conflict) {
        let mut command = std::process::Command::new(exe());
        command
            .arg("diff")
            .arg(&item.group)
            .arg(&item.path)
            .arg("--state-root")
            .arg(&self.state_root);
        if let Some(config) = &self.config {
            command.arg("--config").arg(config);
        }
        self.diff = Some(match command.output() {
            // `diff` exits 1 when the sides differ, which is the whole
            // point, so the status is not consulted — only whether it
            // said anything.
            Ok(output) if !output.stdout.is_empty() => {
                String::from_utf8_lossy(&output.stdout).into_owned()
            }
            Ok(output) => match String::from_utf8_lossy(&output.stderr).trim() {
                "" => "the two sides read the same".to_owned(),
                complaint => complaint.to_owned(),
            },
            Err(error) => format!("unable to run diff: {error}"),
        });
    }

    fn read_log(&mut self) {
        // A supervisor started as a service writes `service.log`; one
        // started by hand writes `watch.log`. Both are the same account
        // of the same work, so either will do.
        let service = self.state_root.join("service.log");
        let watch = self.state_root.join("watch.log");
        let (path, text) = match std::fs::read_to_string(&service) {
            Ok(text) => (service, text),
            Err(_) => match std::fs::read_to_string(&watch) {
                Ok(text) => (watch, text),
                Err(_) => {
                    self.log = vec![format!(
                        "neither {} nor {} can be read",
                        service.display(),
                        watch.display()
                    )];
                    self.log_path = None;
                    return;
                }
            },
        };
        self.log_path = Some(path);
        // The tail only: the file runs to megabytes, and a window is not
        // where anyone reads the first line of it.
        self.log = text.lines().rev().take(400).map(str::to_owned).collect();
        self.log.reverse();
    }

    /// Walks the panes, photographs each, and quits. Nothing here runs
    /// unless `shoot` asked for it.
    fn photograph(&mut self, directory: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            for (pane, name) in [
                (Pane::Fleet, "fleet"),
                (Pane::Conflicts, "conflicts"),
                (Pane::Log, "log"),
                (Pane::Hosts, "hosts"),
            ] {
                this.update(cx, |this, cx| {
                    this.pane = pane;
                    match pane {
                        // Each pane is photographed as a reader would
                        // find it: with something open in it.
                        Pane::Conflicts => {
                            if this.conflict.is_none() {
                                if let Some(first) =
                                    this.waiting_list().into_iter().find(|item| !item.blocked)
                                {
                                    this.conflict = Some(first.clone());
                                    this.read_diff(&first);
                                }
                            }
                        }
                        Pane::Log => this.read_log(),
                        Pane::Fleet => {
                            if this.selected.is_none() {
                                if let Some(report) = &this.report {
                                    if let Some((group, session)) = report
                                        .groups
                                        .iter()
                                        .flat_map(|group| {
                                            group
                                                .sessions
                                                .iter()
                                                .map(move |session| (group, session))
                                        })
                                        .next()
                                    {
                                        this.selected = Some((
                                            group.name.clone(),
                                            session.session.clone(),
                                        ));
                                    }
                                }
                            }
                        }
                        Pane::Hosts => {}
                    }
                    cx.notify();
                })?;
                // The window server hands back the last frame it was
                // given, and macOS stops redrawing a window it thinks
                // nobody can see — so the window is asked to the front
                // and redrawn before the shutter, not after.
                cx.update(|window, cx| {
                    cx.activate(true);
                    window.activate_window();
                    window.refresh();
                })?;
                gpui::Timer::after(Duration::from_millis(900)).await;
                let path = directory.join(format!("desk-{name}.png"));
                cx.update(|window, _| {
                    let bounds = window.bounds();
                    match camera::photograph(
                        bounds.origin.x.to_f64(),
                        bounds.origin.y.to_f64(),
                        bounds.size.width.to_f64(),
                        bounds.size.height.to_f64(),
                        &path,
                    ) {
                        Ok(()) => println!("{}", path.display()),
                        Err(error) => eprintln!("unable to photograph {name}: {error:#}"),
                    }
                })?;
            }
            cx.update(|_, cx| cx.quit())?;
            anyhow::Ok(())
        })
        .detach();
    }
}

/// Asking the window server for the rectangle this window occupies. A
/// process may photograph its own windows without the screen-recording
/// permission a capture of the whole display would need, which is what
/// makes the screenshots in the design document possible at all.
#[cfg(target_os = "macos")]
mod camera {
    use std::ffi::c_void;
    use std::path::Path;

    use anyhow::{anyhow, Result};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    /// On screen, excluding the desktop's own furniture.
    const ON_SCREEN_ONLY: u32 = 1;
    const EXCLUDE_DESKTOP: u32 = 16;
    /// Eight bits a channel, R G B A in that order in memory.
    const ALPHA_PREMULTIPLIED_LAST: u32 = 1;
    const BYTE_ORDER_32_BIG: u32 = 4 << 12;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCreateImage(
            rect: CGRect,
            option: u32,
            window: u32,
            image_option: u32,
        ) -> *mut c_void;
        fn CGImageGetWidth(image: *mut c_void) -> usize;
        fn CGImageGetHeight(image: *mut c_void) -> usize;
        fn CGImageRelease(image: *mut c_void);
        fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
        fn CGColorSpaceRelease(space: *mut c_void);
        fn CGBitmapContextCreate(
            data: *mut c_void,
            width: usize,
            height: usize,
            bits_per_component: usize,
            bytes_per_row: usize,
            space: *mut c_void,
            info: u32,
        ) -> *mut c_void;
        fn CGContextDrawImage(context: *mut c_void, rect: CGRect, image: *mut c_void);
        fn CGContextRelease(context: *mut c_void);
    }

    pub fn photograph(x: f64, y: f64, width: f64, height: f64, path: &Path) -> Result<()> {
        let rect = CGRect {
            origin: CGPoint { x, y },
            size: CGSize { width, height },
        };
        unsafe {
            let image =
                CGWindowListCreateImage(rect, ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0, 0);
            if image.is_null() {
                return Err(anyhow!("the window server handed back no image"));
            }
            let width = CGImageGetWidth(image);
            let height = CGImageGetHeight(image);
            let mut pixels = vec![0u8; width * height * 4];
            let space = CGColorSpaceCreateDeviceRGB();
            let context = CGBitmapContextCreate(
                pixels.as_mut_ptr().cast(),
                width,
                height,
                8,
                width * 4,
                space,
                ALPHA_PREMULTIPLIED_LAST | BYTE_ORDER_32_BIG,
            );
            if context.is_null() {
                CGColorSpaceRelease(space);
                CGImageRelease(image);
                return Err(anyhow!("no bitmap to draw the frame into"));
            }
            CGContextDrawImage(
                context,
                CGRect {
                    origin: CGPoint { x: 0., y: 0. },
                    size: CGSize {
                        width: width as f64,
                        height: height as f64,
                    },
                },
                image,
            );
            CGContextRelease(context);
            CGColorSpaceRelease(space);
            CGImageRelease(image);
            let buffer = image::RgbaImage::from_raw(width as u32, height as u32, pixels)
                .ok_or_else(|| anyhow!("the frame did not fit its own dimensions"))?;
            buffer.save(path)?;
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod camera {
    use std::path::Path;

    use anyhow::{anyhow, Result};

    pub fn photograph(_x: f64, _y: f64, _w: f64, _h: f64, _path: &Path) -> Result<()> {
        Err(anyhow!("photographing the window is macOS only"))
    }
}

// ── the small pieces ─────────────────────────────────────────────────

/// A round state light. The colour is the only thing it says, so it is
/// always beside words that say the same thing.
fn dot(colour: u32) -> Div {
    div()
        .size(px(7.))
        .flex_shrink_0()
        .rounded_full()
        .bg(rgb(colour))
}

/// A word or two on its own tinted ground: a state, a count, a role.
fn pill(text: impl Into<SharedString>, colour: u32) -> Div {
    div()
        .px(step(1.75))
        .py(px(2.))
        .rounded(px(5.))
        .bg(tint(colour, 0x20))
        .text_color(rgb(colour))
        .text_size(px(T_PILL))
        .font_weight(FontWeight::MEDIUM)
        .whitespace_nowrap()
        .child(text.into())
}

/// One of the three numbers in the header: the figure, then the word.
/// A zero keeps its place and loses its colour, so the row does not
/// move about as the fleet changes.
fn count(n: usize, word: &'static str, colour: u32) -> Div {
    let lit = n > 0;
    div()
        .flex()
        .items_center()
        .gap(step(1.5))
        .px(step(2.))
        .py(step(1.))
        .rounded(px(6.))
        .bg(match lit {
            true => tint(colour, 0x1a),
            false => rgba(0x00000000),
        })
        .child(dot(match lit {
            true => colour,
            false => HAIR,
        }))
        .child(
            div()
                .text_size(px(T_ROW))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(match lit {
                    true => colour,
                    false => FAINT,
                }))
                .child(n.to_string()),
        )
        .child(
            div()
                .text_size(px(T_META))
                .text_color(rgb(match lit {
                    true => DIM,
                    false => FAINT,
                }))
                .child(word),
        )
}

/// The small grey heading over a block of facts.
fn label(text: &'static str) -> Div {
    div()
        .text_size(px(T_PILL))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(FAINT))
        .child(text)
}

/// A thing to press. The caller hangs the action on it.
fn button(id: impl Into<SharedString>, text: impl Into<SharedString>) -> gpui::Stateful<Div> {
    div()
        .id(SharedString::from(id.into()))
        .h(step(6.5))
        .px(step(2.5))
        .rounded(px(6.))
        .bg(rgb(RAISED))
        .border_1()
        .border_color(rgb(LINE))
        .flex()
        .items_center()
        .cursor_pointer()
        .text_size(px(T_META))
        .text_color(rgb(INK))
        .whitespace_nowrap()
        .hover(|button| button.bg(rgb(0x252c36)))
        .child(text.into())
}

/// A button that is either on or off, and shows which.
fn toggle(id: &'static str, text: &'static str, on: bool) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .h(step(6.5))
        .px(step(2.5))
        .rounded(px(6.))
        .flex()
        .items_center()
        .gap(step(1.5))
        .cursor_pointer()
        .border_1()
        .text_size(px(T_META))
        .when(on, |button| {
            button
                .bg(tint(AMBER, 0x1c))
                .border_color(tint(AMBER, 0x40))
                .text_color(rgb(AMBER))
        })
        .when(!on, |button| {
            button
                .bg(rgb(RAISED))
                .border_color(rgb(LINE))
                .text_color(rgb(DIM))
                .hover(|button| button.bg(rgb(0x252c36)))
        })
        .child(dot(match on {
            true => AMBER,
            false => FAINT,
        }))
        .child(text)
}

/// A pane with nothing in it yet says so in the middle, once.
fn empty(text: &'static str) -> AnyElement {
    div()
        .flex_1()
        .min_h(px(0.))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(T_BODY))
        .text_color(rgb(FAINT))
        .child(text)
        .into_any_element()
}

#[derive(Clone, Copy)]
enum Verb {
    Flush,
    Verify,
    Pause,
    Resume,
}

impl Verb {
    fn word(self) -> &'static str {
        match self {
            Verb::Flush => "Flush",
            Verb::Verify => "Verify",
            Verb::Pause => "Pause",
            Verb::Resume => "Resume",
        }
    }

    fn done(self) -> &'static str {
        match self {
            Verb::Flush => "flushed",
            Verb::Verify => "will verify",
            Verb::Pause => "paused",
            Verb::Resume => "resumed",
        }
    }
}

/// Whether a log line is the supervisor complaining.
fn is_complaint(line: &str) -> bool {
    line.contains(" error:") || line.contains("refused")
}

/// What a working session is doing, in one line: the phase, how long it
/// has been at it, and how far along when the numbers allow an honest
/// answer. The same rule `status` follows, so the two never disagree.
fn describe(progress: &crate::progress::ProgressSnapshot) -> String {
    let mut line = format!(
        "{} · {}",
        progress.phase.label(),
        format_age(progress.seconds)
    );
    let side = &progress.alpha;
    match (side.entries, side.expected) {
        // A tree nothing has counted yet reports what it has walked and
        // no estimate, which is the honest answer on a first scan.
        (walked, Some(expected)) if walked > 0 && expected > 0 => line.push_str(&format!(
            " · alpha {} of ~{}",
            thousands(walked),
            thousands(expected)
        )),
        (walked, None) if walked > 0 => {
            line.push_str(&format!(" · alpha {}", thousands(walked)))
        }
        _ => {}
    }
    if let Some(left) = progress.remaining_seconds.filter(|left| *left > 0) {
        line.push_str(&format!(" · about {} left", format_age(left)));
    }
    line
}

/// Digits a person can read at a glance.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A path under the home directory, written the way a person writes it.
/// The rows in one card then measure against each other rather than
/// against the length of a home directory nobody is reading.
fn tilde(path: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_owned();
    };
    let home = home.to_string_lossy().into_owned();
    match path.strip_prefix(&home) {
        Some(rest) => format!("~{rest}"),
        None => path.to_owned(),
    }
}

/// The short name of a beta: the host it is on, or the last part of the
/// path when it is a directory on this machine. Long enough to tell two
/// apart, short enough to sit on a button.
fn short_name(beta: &str) -> String {
    match beta.split_once(':') {
        Some((host, _)) if !host.starts_with('/') && !host.starts_with('~') => host.to_owned(),
        _ => beta.rsplit('/').next().unwrap_or(beta).to_owned(),
    }
}

fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("autobahn"))
}

// ── how a state looks ────────────────────────────────────────────────

/// How much a state needs a person, in the order the fleet sorts by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Fine,
    Attention,
    Bad,
}

fn severity(state: &str) -> Severity {
    match state {
        "halted" | "unreachable" | "errored" => Severity::Bad,
        "conflicts" | "blocked" => Severity::Attention,
        _ => Severity::Fine,
    }
}

fn colour_of(severity: Severity) -> u32 {
    match severity {
        Severity::Fine => GREEN,
        Severity::Attention => AMBER,
        Severity::Bad => RED,
    }
}

/// The state, with what it is waiting on when that is the point.
fn state_words(session: &SessionReport) -> String {
    match (session.conflicts.len(), session.blocked.len()) {
        (0, 0) => session.state.clone(),
        (c, 0) => format!("{} · {c}", session.state),
        (0, b) => format!("{} · {b}", session.state),
        (c, b) => format!("{} · {c} + {b}", session.state),
    }
}

fn summarize(sessions: &[SessionReport]) -> (String, u32) {
    let mut fine = 0;
    let mut attention = 0;
    let mut bad = 0;
    for session in sessions {
        match severity(&session.state) {
            Severity::Fine => fine += 1,
            Severity::Attention => attention += 1,
            Severity::Bad => bad += 1,
        }
    }
    let mut parts = Vec::new();
    if attention > 0 {
        parts.push(format!("{attention} need you"));
    }
    if bad > 0 {
        parts.push(format!("{bad} away"));
    }
    if fine > 0 {
        parts.push(format!("{fine} synchronized"));
    }
    let colour = match (attention, bad) {
        (0, 0) => GREEN,
        (0, _) => RED,
        _ => AMBER,
    };
    (parts.join(" · "), colour)
}

fn format_age(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fleet sorts and colours by how much a state needs a person,
    /// and the three tiers are the ones every other surface uses.
    #[test]
    fn severity_matches_the_words_every_other_surface_uses() {
        assert_eq!(severity("synchronized"), Severity::Fine);
        assert_eq!(severity("paused"), Severity::Fine);
        assert_eq!(severity("conflicts"), Severity::Attention);
        assert_eq!(severity("blocked"), Severity::Attention);
        assert_eq!(severity("halted"), Severity::Bad);
        assert_eq!(severity("unreachable"), Severity::Bad);
        assert_eq!(severity("errored"), Severity::Bad);
        assert!(Severity::Bad > Severity::Attention && Severity::Attention > Severity::Fine);
    }

    /// An age is read at a glance or not at all.
    #[test]
    fn an_age_is_one_unit() {
        assert_eq!(format_age(3), "3s");
        assert_eq!(format_age(59), "59s");
        assert_eq!(format_age(60), "1m");
        assert_eq!(format_age(3_599), "59m");
        assert_eq!(format_age(3_600), "1h");
        assert_eq!(format_age(90_000), "1d");
    }

    /// Every measurement in the window is a whole number of steps, and a
    /// tint is the state colour with an alpha behind it — not a second
    /// palette that can drift from the first.
    #[test]
    fn the_spacing_scale_and_the_tints_come_from_one_place() {
        assert_eq!(step(4.), px(16.));
        assert_eq!(step(1.5), px(6.));
        assert_eq!(tint(GREEN, 0x20), rgba(0x3fb97a20));
    }
}
