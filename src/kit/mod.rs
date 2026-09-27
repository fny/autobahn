//! The window over the fleet, drawn with GPUI Kit.
//!
//! The same four panes over the same seam as `crate::desk`, and the same
//! words — but on the kit's own GPUI, with its components underneath:
//! text that selects, a field that behaves like every other field on
//! this machine, and a theme to hang the palette on.
//!
//! Nothing below the window is duplicated. The fleet still comes from
//! `supervisor::status_report`, the actions still go through the control
//! socket and the CLI, the form is still generated from
//! `config::schema`, and every line of English still comes from
//! `assets/words/en.toml`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui_kit::component::Root;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::supervisor::{status_report, GroupReport, SessionReport, StatusReport};
use crate::words::{count as counted, fill, t};

/// How often the fleet is re-read when nothing is working, and when
/// something is.
const POLL_AT_REST: Duration = Duration::from_secs(2);
const POLL_WHILE_WORKING: Duration = Duration::from_millis(500);

/// The palette, as the other window has it.
const GROUND: u32 = 0x10141a;
const RAIL: u32 = 0x0b0e13;
const PANEL: u32 = 0x171c23;
const SUNK: u32 = 0x12161c;
const RAISED: u32 = 0x1e242d;
const LINE: u32 = 0x282f39;
const HAIR: u32 = 0x1d232b;
const INK: u32 = 0xe7ebf0;
const DIM: u32 = 0x9ba6b2;
const FAINT: u32 = 0x69727e;
const GREEN: u32 = 0x3fb97a;
const AMBER: u32 = 0xe0ae42;
const RED: u32 = 0xe8796a;
const BLUE: u32 = 0x6ea8f0;

/// The one spacing unit, in points.
const STEP: f32 = 4.0;

fn step(n: f32) -> Pixels {
    px(STEP * n)
}

fn tint(colour: u32, alpha: u32) -> Rgba {
    rgba((colour << 8) | alpha)
}

/// The panes, in the order the rail lists them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Groups,
    Conflicts,
    Log,
    Hosts,
}

impl Pane {
    fn title(self) -> &'static str {
        match self {
            Pane::Groups => t("pane.groups"),
            Pane::Conflicts => t("pane.conflicts"),
            Pane::Log => t("pane.log"),
            Pane::Hosts => t("pane.hosts"),
        }
    }

    fn about(self) -> &'static str {
        match self {
            Pane::Groups => t("pane.groups_about"),
            Pane::Conflicts => t("pane.conflicts_about"),
            Pane::Log => t("pane.log_about"),
            Pane::Hosts => t("pane.hosts_about"),
        }
    }
}

/// What the window is showing.
pub struct Desk {
    config: Option<PathBuf>,
    /// The face anything a person compares is set in.
    mono: SharedString,
    state_root: PathBuf,
    pane: Pane,
    report: Option<StatusReport>,
    read_at: Option<Instant>,
    selected: Option<(String, crate::supervisor::control::SessionKey)>,
    said: Option<String>,
}

/// Runs the window until it is closed.
pub fn run(config: Option<PathBuf>, state_root: PathBuf) -> Result<()> {
    run_with(config, state_root, None)
}

/// The same window, photographed into `directory` and closed again.
pub fn shoot(config: Option<PathBuf>, state_root: PathBuf, directory: PathBuf) -> Result<()> {
    run_with(config, state_root, Some(directory))
}

fn run_with(
    config: Option<PathBuf>,
    state_root: PathBuf,
    shots: Option<PathBuf>,
) -> Result<()> {
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        cx.activate(true);
        let config = config.clone();
        let state_root = state_root.clone();
        let bounds = Bounds::centered(None, size(px(1240.), px(820.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(t("app.window").into()),
                appears_transparent: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let window = cx
            .open_window(options, |window, cx| {
                let desk = cx.new(|cx| Desk::new(config, state_root, cx));
                cx.new(|cx| Root::new(desk, window, cx))
            })
            .expect("unable to open the window");
        let Some(directory) = shots.clone() else { return };
        cx.spawn(async move |cx| {
            let sleep = cx.background_executor().timer(Duration::from_millis(900));
            sleep.await;
            let path = directory.join("desk-kit.png");
            let taken = window.update(cx, |_, window, _| {
                let bounds = window.bounds();
                crate::camera::grab(
                    bounds.origin.x.to_f64(),
                    bounds.origin.y.to_f64(),
                    bounds.size.width.to_f64(),
                    bounds.size.height.to_f64(),
                )
                .and_then(|frame| Ok(frame.save(&path)?))
            });
            match taken {
                Ok(Ok(())) => println!("{}", path.display()),
                Ok(Err(error)) => eprintln!("unable to photograph: {error:#}"),
                Err(error) => eprintln!("the window went away: {error}"),
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    Ok(())
}

impl Desk {
    fn new(config: Option<PathBuf>, state_root: PathBuf, cx: &mut Context<Self>) -> Self {
        let names = cx.text_system().all_font_names();
        let mono = ["SF Mono", "Menlo", "Monaco"]
            .into_iter()
            .find(|name| names.iter().any(|known| known == name))
            .unwrap_or("Menlo");
        let mut desk = Desk {
            config,
            mono: SharedString::from(mono.to_owned()),
            state_root,
            pane: Pane::Groups,
            report: None,
            read_at: None,
            selected: None,
            said: None,
        };
        desk.refresh();
        cx.spawn(async move |this, cx| {
            loop {
                // The kit's GPUI hands out its timers through the
                // executor rather than as a free type.
                let sleep = cx.background_executor().timer(POLL_WHILE_WORKING);
                sleep.await;
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

    // ── the seam, which is the other window's ────────────────────────

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
                self.said = Some(fill(
                    "status.config_refused",
                    &[("error", &format!("{error:#}"))],
                ));
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
        self.report
            .iter()
            .flat_map(|report| report.groups.iter())
            .flat_map(|group| group.sessions.iter())
            .map(|session| session.conflicts.len() + session.blocked.len())
            .sum()
    }
}

impl Render for Desk {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = self.pane;
        div()
            .size_full()
            .flex()
            .text_size(px(13.))
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
                        Pane::Groups => self.groups(cx),
                        _ => empty(pane.about()),
                    })
                    .child(self.footer()),
            )
    }
}

impl Desk {
    fn rail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let waiting = self.waiting();
        let running = self.report.as_ref().map(|report| report.supervisor_running);
        let (service, colour) = match running {
            Some(true) => (t("fleet.supervisor_running"), GREEN),
            Some(false) => (t("fleet.supervisor_missing"), RED),
            None => (t("fleet.reading"), FAINT),
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
                                    .child(t("app.name")),
                            )
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(rgb(FAINT))
                                    .child(t("app.surface")),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
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
                    .child(self.nav(Pane::Groups, None, cx))
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
                            .child(div().text_size(px(11.)).text_color(rgb(DIM)).child(service)),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
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
            .text_size(px(12.5))
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
                cx.notify();
            }))
            .into_any_element()
    }

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
                            .text_size(px(16.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.pane.title()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(FAINT))
                            .child(match self.report {
                                None => self.pane.about().to_owned(),
                                Some(_) => fill(
                                    "pane.counted",
                                    &[
                                        ("groups", &counted("pane.group", groups, &[])),
                                        ("sessions", &counted("pane.session", sessions, &[])),
                                        ("about", self.pane.about()),
                                    ],
                                ),
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(step(2.))
                    .child(count(needs, counted("fleet.needs_you", needs, &[]), AMBER))
                    .child(count(away, counted("fleet.away", away, &[]), RED))
                    .child(count(fine, counted("fleet.synchronized", fine, &[]), GREEN)),
            )
            .into_any_element()
    }

    fn footer(&self) -> AnyElement {
        let age = self
            .read_at
            .map(|at| format_age(at.elapsed().as_secs()))
            .unwrap_or_else(|| t("fleet.never").to_owned());
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
            .text_size(px(11.))
            .child(match &self.said {
                Some(said) => div()
                    .text_color(rgb(DIM))
                    .truncate()
                    .child(crate::text::display_safe(said).to_string()),
                None => div().text_color(rgb(FAINT)).child(t("fleet.provenance")),
            })
            .child(
                div()
                    .flex_shrink_0()
                    .pl(step(4.))
                    .text_color(rgb(FAINT))
                    .child(fill("fleet.read_ago", &[("age", &age)])),
            )
            .into_any_element()
    }

    fn groups(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(report) = self.report.clone() else {
            return empty(t("fleet.reading_fleet"));
        };
        div()
            .id("groups")
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
                            .text_size(px(14.5))
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
                            .text_size(px(11.))
                            .text_color(rgb(FAINT))
                            .truncate()
                            .child(tilde(&group.alpha)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
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
        div()
            .flex()
            .flex_col()
            .when(divider, |band| band.border_t_1().border_color(rgb(HAIR)))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "row-{}-{}",
                        group.name, session.beta
                    )))
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
                            .text_size(px(12.5))
                            .truncate()
                            .child(tilde(&crate::text::display_safe(&session.beta))),
                    )
                    .child(
                        div()
                            .w(px(100.))
                            .flex_shrink_0()
                            .font_family(self.mono.clone())
                            .text_size(px(11.))
                            .text_color(rgb(FAINT))
                            .truncate()
                            .child(session.mode.clone()),
                    )
                    .child(
                        div()
                            .w(px(100.))
                            .flex_shrink_0()
                            .text_right()
                            .font_family(self.mono.clone())
                            .text_size(px(11.))
                            .text_color(rgb(DIM))
                            .child(counted(
                                "fleet.cycles",
                                session.cycles as usize,
                                &[("count", &thousands(session.cycles))],
                            )),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .flex_shrink_0()
                            .text_right()
                            .font_family(self.mono.clone())
                            .text_size(px(11.))
                            .text_color(rgb(FAINT))
                            .child(match session.age_seconds {
                                Some(age) => fill("fleet.ago", &[("age", &format_age(age))]),
                                None => t("fleet.never").to_owned(),
                            }),
                    )
                    .child(
                        div()
                            .w(px(148.))
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
            .when_some(session.error.as_ref(), |band, error| {
                band.child(
                    div()
                        .pl(step(8.))
                        .pr(step(4.))
                        .pb(step(2.))
                        .font_family(self.mono.clone())
                        .text_size(px(11.))
                        .text_color(rgb(RED))
                        .child(crate::text::display_safe(error).to_string()),
                )
            })
            .into_any_element()
    }
}

// ── the small pieces ─────────────────────────────────────────────────

fn dot(colour: u32) -> Div {
    div()
        .size(px(7.))
        .flex_shrink_0()
        .rounded_full()
        .bg(rgb(colour))
}

fn pill(text: impl Into<SharedString>, colour: u32) -> Div {
    div()
        .px(step(1.75))
        .py(px(2.))
        .rounded(px(5.))
        .bg(tint(colour, 0x20))
        .text_color(rgb(colour))
        .text_size(px(10.5))
        .font_weight(FontWeight::MEDIUM)
        .whitespace_nowrap()
        .child(text.into())
}

fn count(n: usize, word: String, colour: u32) -> Div {
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
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(match lit {
                    true => colour,
                    false => FAINT,
                }))
                .child(n.to_string()),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(match lit {
                    true => DIM,
                    false => FAINT,
                }))
                .child(word),
        )
}

fn empty(text: &'static str) -> AnyElement {
    div()
        .flex_1()
        .min_h(px(0.))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(13.))
        .text_color(rgb(FAINT))
        .child(text)
        .into_any_element()
}

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

fn format_age(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

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
        parts.push(counted("fleet.needs_you", attention, &[]));
        let last = parts.len() - 1;
        parts[last] = format!("{attention} {}", parts[last]);
    }
    if bad > 0 {
        parts.push(format!("{bad} {}", counted("fleet.away", bad, &[])));
    }
    if fine > 0 {
        parts.push(format!(
            "{fine} {}",
            counted("fleet.synchronized", fine, &[])
        ));
    }
    let colour = match (attention, bad) {
        (0, 0) => GREEN,
        (0, _) => RED,
        _ => AMBER,
    };
    (parts.join(" · "), colour)
}
