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

mod area;
mod buffer;

use crate::surface::*;

use crate::words::{count as counted, fill, t};

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
    Groups,
    Conflicts,
    Config,
    Log,
    Hosts,
}

impl Pane {
    fn title(self) -> &'static str {
        match self {
            Pane::Groups => t("pane.groups"),
            Pane::Conflicts => t("pane.conflicts"),
            Pane::Config => t("pane.config"),
            Pane::Log => t("pane.log"),
            Pane::Hosts => t("pane.hosts"),
        }
    }

    /// The line under the title: what this pane is for.
    fn about(self) -> &'static str {
        match self {
            Pane::Groups => t("pane.groups_about"),
            Pane::Conflicts => t("pane.conflicts_about"),
            Pane::Config => t("pane.config_about"),
            Pane::Log => t("pane.log_about"),
            Pane::Hosts => t("pane.hosts_about"),
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
    /// Both sides of the open conflict, read when it was opened.
    sides: Option<(Side, Side)>,
    diff: Option<String>,
    /// The configuration file as the editor holds it, and which part of
    /// it the form is showing.
    sheet: Option<Sheet>,
    section: Section,
    /// The field being edited, if any.
    editing: Option<Edit>,
    /// Whether the loader's complaints are unfolded. Shut by default:
    /// the line beside Save says there is one, which is all most of
    /// them need to say.
    showing_faults: bool,
    /// The complaints that belong to a field, by the field they belong
    /// to. Worked out once a frame, where the loader's verdict is read.
    at_fields: std::collections::HashMap<(Section, String), At>,
    /// Whether this window is showing what it keeps back.
    ///
    /// Five clicks on the wordmark. Not a secret and not a password —
    /// a door that does not open by leaning on it, for settings that
    /// are a different kind of question rather than a dangerous one.
    unlocked: bool,
    /// Clicks so far, and when the last one landed: a run that stops
    /// for a moment is somebody clicking about, not asking for this.
    knocks: u8,
    knocked_at: Option<Instant>,
    /// The shape of the file, from the structs the parser reads it into.
    shape: serde_json::Value,
    /// Where keys go, so a window that is being typed into hears them.
    focus: gpui::FocusHandle,
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





/// A field being edited, and the block of text doing the editing.
struct Edit {
    at: Spot,
    area: gpui::Entity<area::Area>,
    /// Whether the value is a list, one entry to a line.
    list: bool,
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

        let Some(directory) = shots.clone() else {
            open_window(config.clone(), state_root.clone(), None, cx).ok();
            // The item in the menu bar belongs to the process, not to
            // the window: closing the window leaves it there, the way
            // the tray was always there, and "Open the window" brings
            // the window back. The tray and the window were two
            // processes polling the same files and saying the same
            // things; now they are one.
            match crate::menubar::Bar::start(config.clone(), state_root.clone(), || {}) {
                Ok(mut bar) => {
                    bar.window = true;
                    bar.appear();
                    cx.set_global(Menubar(bar));
                    watch_the_bar(config.clone(), state_root.clone(), cx);
                }
                Err(error) => {
                    eprintln!("{}", fill("status.no_menu_bar", &[("error", &format!("{error:#}"))]));
                    // Without one, the window is the whole app again.
                    cx.on_window_closed(|cx| {
                        if cx.windows().is_empty() {
                            cx.quit();
                        }
                    })
                    .detach();
                }
            }
            return;
        };

        // Photographing: a pane at a time, each in a window of its own.
        // A window that has just opened has certainly drawn its first
        // frame; a window that has been told to show a different pane
        // has only certainly been told, and macOS may not have drawn it
        // again — which is how three identical photographs happen.
        let config = config.clone();
        let state_root = state_root.clone();
        cx.spawn(async move |cx: &mut gpui::AsyncApp| {
            for (pane, name) in [
                (Pane::Groups, "groups"),
                (Pane::Conflicts, "conflicts"),
                (Pane::Config, "config"),
                (Pane::Log, "log"),
                (Pane::Hosts, "hosts"),
            ] {
                let window = cx.update(|cx| {
                    open_window(config.clone(), state_root.clone(), Some(pane), cx)
                })??;
                gpui::Timer::after(Duration::from_millis(900)).await;
                let path = directory.join(format!("desk-{name}.png"));
                window.update(cx, |_, window, _| {
                    let bounds = window.bounds();
                    let frame = crate::camera::grab(
                        bounds.origin.x.to_f64(),
                        bounds.origin.y.to_f64(),
                        bounds.size.width.to_f64(),
                        bounds.size.height.to_f64(),
                    );
                    match frame.and_then(|frame| Ok(frame.save(&path)?)) {
                        Ok(()) => println!("{}", path.display()),
                        Err(error) => eprintln!("unable to photograph {name}: {error:#}"),
                    }
                    window.remove_window();
                })?;
                gpui::Timer::after(Duration::from_millis(200)).await;
            }
            cx.update(|cx| cx.quit())?;
            anyhow::Ok(())
        })
        .detach();
    });
    Ok(())
}

/// The menu bar item, kept by the process rather than by a window.
struct Menubar(crate::menubar::Bar);

impl gpui::Global for Menubar {}

/// Keeps the item up to date and answers what is chosen in it.
///
/// The same few seconds the tray used, on the application's own timer:
/// the window has one of its own for the fleet, and this outlives any
/// window.
fn watch_the_bar(config: Option<PathBuf>, state_root: PathBuf, cx: &mut App) {
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        loop {
            gpui::Timer::after(crate::menubar::POLL).await;
            let carried = cx.update(|cx| {
                let mut show = false;
                let mut quit = false;
                cx.update_global::<Menubar, ()>(|menubar, _| {
                    while let Ok(event) = muda::MenuEvent::receiver().try_recv() {
                        match menubar.0.chose(&event.id) {
                            Some(crate::menubar::Action::Quit) => quit = true,
                            Some(crate::menubar::Action::Show) => show = true,
                            _ => {}
                        }
                    }
                    menubar.0.finished();
                });
                if quit {
                    cx.quit();
                }
                if show {
                    cx.activate(true);
                    if cx.windows().is_empty() {
                        open_window(config.clone(), state_root.clone(), None, cx).ok();
                    } else {
                        for window in cx.windows() {
                            window
                                .update(cx, |_, window, _| window.activate_window())
                                .ok();
                        }
                    }
                }
            });
            if carried.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Opens the window, on `pane` when one is asked for.
fn open_window(
    config: Option<PathBuf>,
    state_root: PathBuf,
    pane: Option<Pane>,
    cx: &mut App,
) -> Result<gpui::WindowHandle<Desk>> {
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
    cx.open_window(options, |_, cx| {
        cx.new(|cx| Desk::new(config, state_root, pane, cx))
    })
    .map_err(|error| anyhow::anyhow!("unable to open the window: {error}"))
}

impl Desk {
    fn new(
        config: Option<PathBuf>,
        state_root: PathBuf,
        pane: Option<Pane>,
        cx: &mut Context<Self>,
    ) -> Self {
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
            pane: pane.unwrap_or(Pane::Groups),
            report: None,
            read_at: None,
            selected: None,
            conflict: None,
            sides: None,
            diff: None,
            sheet: None,
            section: Section::Settings,
            editing: None,
            showing_faults: false,
            at_fields: std::collections::HashMap::new(),
            unlocked: false,
            knocks: 0,
            knocked_at: None,
            shape: crate::config::schema(),
            focus: cx.focus_handle(),
            log: Vec::new(),
            log_path: None,
            errors_only: false,
            said: None,
            sans: pick(&["SF Pro Text", "SF Pro Display", "Helvetica Neue"], "Helvetica"),
            mono: pick(&["SF Mono", "Menlo", "Monaco"], "Menlo"),
        };
        desk.refresh();
        if let Some(pane) = pane {
            desk.settle(pane);
        }
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
            .id("desk")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::typed))
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
                        Pane::Groups => self.groups(cx),
                        Pane::Conflicts => self.conflicts(cx),
                        Pane::Config => self.config_pane(cx),
                        Pane::Log => self.log_pane(cx),
                        Pane::Hosts => self.hosts(cx),
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
                            .id("wordmark")
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
                                    .text_size(px(T_ROW))
                                    .text_color(match self.unlocked {
                                        true => rgb(BLUE),
                                        false => rgb(FAINT),
                                    })
                                    .child(t("app.surface")),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.knock();
                                cx.notify();
                            })),
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
                    .child(self.nav(Pane::Groups, None, cx))
                    .child(self.nav(Pane::Conflicts, Some(waiting), cx))
                    .child(self.nav(Pane::Config, Some(self.pending()), cx))
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
                this.settle(pane);
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

    /// The bottom line: what the last action said, and how fresh the
    /// numbers above it are.
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
            .font_family(self.mono.clone())
            .text_size(px(T_META))
            .child(match &self.said {
                Some(said) => div()
                    .text_color(rgb(DIM))
                    .truncate()
                    .child(crate::text::display_safe(said).to_string()),
                None => div()
                    .text_color(rgb(FAINT))
                    .child(t("fleet.provenance")),
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

    // ── the fleet ────────────────────────────────────────────────────

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
                            .child(tilde(&group.alpha)),
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
                            .child(counted(
                                "fleet.cycles",
                                session.cycles as usize,
                                &[("count", &thousands(session.cycles))],
                            )),
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
                                Some(age) => fill("fleet.ago", &[("age", &format_age(age))]),
                                None => t("fleet.never").to_owned(),
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
                    band.child(self.aside(
                        format!("doing-{}-{}", group.name, session.beta),
                        describe(progress),
                        BLUE,
                        cx,
                    ))
                },
            )
            .when_some(session.error.as_ref(), |band, error| {
                band.child(self.aside(
                    format!("error-{}-{}", group.name, session.beta),
                    crate::text::display_safe(error).to_string(),
                    RED,
                    cx,
                ))
            })
            // What is waiting reads across the card, like the error
            // above it, rather than into a column half its width.
            .when(open, |band| {
                let mut band = band;
                for (index, (heading, paths)) in
                    waiting_groups(session).into_iter().enumerate()
                {
                    band = band.child(
                        div()
                            .pl(step(8.))
                            .pr(step(4.))
                            .pb(step(0.5))
                            .text_size(px(T_META))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(AMBER))
                            .child(heading),
                    );
                    for (at, path) in paths.into_iter().enumerate() {
                        band = band.child(
                            div().pl(step(10.)).pr(step(4.)).pb(step(0.5)).child(
                                self.copyable(
                                    format!(
                                        "waiting-{}-{}-{index}-{at}",
                                        group.name, session.beta
                                    ),
                                    path,
                                    DIM,
                                    cx,
                                ),
                            ),
                        );
                    }
                    band = band.child(div().h(step(1.5)));
                }
                band
            })
            .when(open, |band| band.child(self.detail(group, session, cx)))
            .into_any_element()
    }

    /// A line hanging under a session row, indented past its dot. It
    /// runs the whole width of the card and scrolls sideways rather
    /// than losing its end, because the end of one of these lines is
    /// the reason — "refusing to remove unsynchronizable content" — and
    /// the beginning is only a path.
    fn aside(&self, id: String, text: String, colour: u32, cx: &mut Context<Self>) -> Div {
        div()
            .pl(step(8.))
            .pr(step(4.))
            .pb(step(2.))
            .child(self.copyable(id, text, colour, cx))
    }

    /// Text a person can take away: gpui draws no selection, so a click
    /// puts the whole line on the clipboard instead.
    fn copyable(
        &self,
        id: String,
        text: String,
        colour: u32,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let taken = text.clone();
        div()
            .id(SharedString::from(id))
            .overflow_x_scroll()
            .whitespace_nowrap()
            .cursor_pointer()
            .font_family(self.mono.clone())
            .text_size(px(T_META))
            .text_color(rgb(colour))
            .hover(|line| line.bg(rgb(RAISED)))
            .tooltip(tip(t("tip.copy")))
            .child(text)
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(taken.clone()));
                this.said = Some(fill("status.copied", &[("text", &cap(&taken, 90))]));
                cx.notify();
            }))
    }

    /// The open session: the two roots, and the four things that can be
    /// asked of it. Everything the row above already says — the mode,
    /// the cycles, the state — is not said again here.
    fn detail(
        &mut self,
        group: &GroupReport,
        session: &SessionReport,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .bg(rgb(SUNK))
            .border_t_1()
            .border_color(rgb(HAIR))
            .p(step(4.))
            .flex()
            .flex_col()
            .gap(step(1.5))
            .child(self.pair(t("fleet.alpha"), tilde(&group.alpha), cx))
            .child(self.pair(t("fleet.beta"), tilde(&session.beta), cx))
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
            .into_any_element()
    }

    fn pair(&self, name: &'static str, value: String, cx: &mut Context<Self>) -> Div {
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
                    .child(self.copyable(format!("pair-{name}-{value}"), value, INK, cx)),
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
        .tooltip(tip(verb.about()))
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
            return empty(t("conflicts.none"));
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
                                        this.open_conflict(item.clone());
                                        cx.notify();
                                    }))
                            })
                            .collect::<Vec<_>>(),
                    ),
            )
            .child(match self.conflict.clone() {
                None => empty(t("conflicts.pick")),
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
        let sides = self.sides.clone();
        // A file with a NUL in it has no difference anyone can read, so
        // the window compares the two files instead of their text.
        let binary = sides
            .as_ref()
            .is_some_and(|(alpha, beta)| alpha.binary || beta.binary);
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
                        true => t("conflicts.blocked"),
                        false => t("conflicts.conflict"),
                    }))
                    .child(self.copyable(
                        format!("conflict-path-{}", item.path),
                        crate::text::display_safe(&item.path).to_string(),
                        INK,
                        cx,
                    ))
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
                                .child(t("conflicts.blocked_about")),
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
                                    button("keep-alpha", t("conflicts.keep_alpha"))
                                        .tooltip(tip(t("tip.keep_alpha")))
                                        .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.resolve(&alpha, "alpha");
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    button("keep-beta", fill("conflicts.keep_beta", &[("name", &keep_host)]))
                                        .tooltip(tip(t("tip.keep_beta")))
                                        .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            let keep = host.host.clone();
                                            this.resolve(&host, &keep);
                                            cx.notify();
                                        }),
                                    ),
                                )
                                .child(button("keep-both", t("conflicts.keep_both"))
                                    .tooltip(tip(t("tip.keep_both")))
                                    .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.resolve(&both, "both");
                                        cx.notify();
                                    },
                                )))
                                .when(!binary, |row| {
                                    row.child(
                                        button("show-diff", t("conflicts.show_difference"))
                                            .tooltip(tip(t("tip.show_difference")))
                                            .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.read_diff(&shown);
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                }),
                        )
                    }),
            )
            .when_some(sides.filter(|_| binary), |column, (alpha, beta)| {
                column.child(self.binary_card(&item, &alpha, &beta, cx))
            })
            .when_some(diff.filter(|_| !binary), |column, diff| {
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

    /// Two files that cannot be diffed, side by side: how big each one
    /// is, when it was last written, what it hashes to, and a way to go
    /// and look at it.
    fn binary_card(
        &self,
        item: &Conflict,
        alpha: &Side,
        beta: &Side,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let newer = match (alpha.modified, beta.modified) {
            (Some(a), Some(b)) if a > b => Some("alpha"),
            (Some(a), Some(b)) if b > a => Some("beta"),
            _ => None,
        };
        let same = alpha.digest.is_some() && alpha.digest == beta.digest;
        let suffix = item
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&item.path)
            .to_owned();
        div()
            .id("binary")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .px(step(6.))
            .py(step(5.))
            .flex()
            .flex_col()
            .gap(step(3.))
            .child(label(t("conflicts.binary")))
            .child(
                div()
                    .max_w(px(680.))
                    .text_size(px(T_META))
                    .text_color(rgb(DIM))
                    .child(t("conflicts.binary_about")),
            )
            .child(
                div()
                    .flex()
                    .gap(step(4.))
                    .child(self.side_card(alpha, newer == Some("alpha"), cx))
                    .child(self.side_card(beta, newer == Some("beta"), cx)),
            )
            .when(same, |card| {
                card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(step(2.))
                        .child(dot(GREEN))
                        .child(
                            div()
                                .text_size(px(T_META))
                                .text_color(rgb(GREEN))
                                .child(fill("conflicts.same", &[("name", &suffix)])),
                        ),
                )
            })
            .into_any_element()
    }

    /// One side of that comparison.
    fn side_card(&self, side: &Side, newer: bool, cx: &mut Context<Self>) -> Div {
        let file = side.file.clone();
        div()
            .flex_1()
            .min_w(px(0.))
            .rounded(px(8.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(LINE))
            .p(step(3.5))
            .flex()
            .flex_col()
            .gap(step(1.5))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(step(2.))
                    .child(
                        div()
                            .text_size(px(T_ROW))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(side.name),
                    )
                    .when(newer, |head| head.child(pill(t("conflicts.written_last"), BLUE)))
                    .when_some(side.trouble.clone(), |head, trouble| {
                        head.child(pill(trouble, AMBER))
                    }),
            )
            .child(
                div()
                    .font_family(self.mono.clone())
                    .text_size(px(T_META))
                    .text_color(rgb(FAINT))
                    .truncate()
                    .child(tail(&side.root, 3)),
            )
            .child(div().h(step(0.5)))
            .child(self.pair(
                t("conflicts.size"),
                side.size
                    .map(human_size)
                    .unwrap_or_else(|| t("conflicts.unknown").to_owned()),
                cx,
            ))
            .child(self.pair(
                t("conflicts.written"),
                side.modified
                    .map(|at| crate::logging::stamp(at as libc::time_t))
                    .unwrap_or_else(|| t("conflicts.unknown").to_owned()),
                cx,
            ))
            .child(self.pair(
                t("conflicts.digest"),
                match (&side.digest, side.size) {
                    (Some(digest), _) => digest.chars().take(16).collect::<String>(),
                    (None, Some(size)) if size > HASH_LIMIT => {
                        t("conflicts.too_large").to_owned()
                    }
                    _ => t("conflicts.unknown").to_owned(),
                },
                cx,
            ))
            .when_some(file, |card, file| {
                card.child(
                    div().pt(step(1.5)).flex().child(
                        button(format!("reveal-{}", side.name), t("conflicts.reveal"))
                            .tooltip(tip(t("tip.reveal")))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.reveal(&file);
                            cx.notify();
                        })),
                    ),
                )
            })
    }

    // ── the configuration ────────────────────────────────────────────

    /// The file: its sections down the left, the fields of the open one
    /// on the right. Every field is drawn from the schema the parser's
    /// own structs generate, so a key the file may hold is a key this
    /// form shows, and the words a key accepts are the words it offers.
    fn config_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sheet) = &self.sheet else {
            return empty(t("config.none"));
        };
        let path = sheet.path.clone();
        // While the loader is behind the typing its last word is about a
        // document nobody is looking at any more, so nothing is drawn
        // from it: the line beside Save says it is checking instead.
        let checking = sheet.checking();
        let refused = match checking {
            true => None,
            false => sheet.refused.clone(),
        };
        let faults = refused.as_deref().map(faults).unwrap_or_default();
        let blamed = refused.as_deref().and_then(blamed);
        // Every fault that names a key goes under that key. What is
        // left belongs to the file rather than to a field, and that is
        // what the line beside Save is for.
        self.at_fields = faults
            .iter()
            .filter_map(|fault| fault_at(fault))
            .map(|at| ((at.section.clone(), at.key.clone()), at))
            .collect();
        let homeless = faults.len() - self.at_fields.len();
        let showing = self.showing_faults && refused.is_some();
        let pending = sheet.pending();
        let sections = self.sections();
        let open = self.section.clone();
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .child(
                div()
                    .id("sections")
                    .w(px(260.))
                    .flex_shrink_0()
                    .h_full()
                    .overflow_y_scroll()
                    .p(step(3.))
                    .flex()
                    .flex_col()
                    .gap(step(0.5))
                    .border_r_1()
                    .border_color(rgb(LINE))
                    .bg(rgb(SUNK))
                    .child(
                        div()
                            .px(step(2.5))
                            .pb(step(1.))
                            .font_family(self.mono.clone())
                            .text_size(px(T_PILL))
                            .text_color(rgb(FAINT))
                            .truncate()
                            .child(tail(&tilde(&path.display().to_string()), 3)),
                    )
                    .child(
                        div()
                            .px(step(2.5))
                            .pb(step(2.))
                            .text_size(px(T_PILL))
                            .text_color(rgb(FAINT))
                            .child(t("config.held")),
                    )
                    .child(
                        div()
                            .px(step(2.))
                            .pb(step(2.5))
                            .flex()
                            .flex_wrap()
                            .gap(step(1.5))
                            .child(
                                button("save-config", t("config.save"))
                                    .tooltip(tip(t("tip.save")))
                                    .when(pending == 0 || refused.is_some(), |save| {
                                        save.opacity(0.45)
                                    })
                                    .when(pending > 0 && refused.is_none(), |save| {
                                        save.bg(tint(GREEN, 0x30))
                                            .border_color(tint(GREEN, 0x70))
                                            .text_color(rgb(GREEN))
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save();
                                        cx.notify();
                                    })),
                            )
                            .when(pending > 0, |row| {
                                row.child(button("revert-config", t("config.revert"))
                                    .tooltip(tip(t("tip.revert")))
                                    .on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.revert();
                                        cx.notify();
                                    }),
                                ))
                            })
                            .when(pending == 0, |row| {
                                row.child(button("re-read-config", t("config.re_read"))
                                    .tooltip(tip(t("tip.config_re_read")))
                                    .on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.editing = None;
                                        this.read_sheet();
                                        cx.notify();
                                    }),
                                ))
                            }),
                    )
                    .when(pending > 0, |column| {
                        column.child(
                            div()
                                .px(step(2.5))
                                .pb(step(1.))
                                .text_size(px(T_PILL))
                                .text_color(rgb(AMBER))
                                .child(counted("config.pending", pending, &[])),
                        )
                    })
                    // What the loader thinks, in one line where the Save
                    // it is refusing is — not a wall above the form.
                    .when(checking, |column| {
                        column.child(
                            div()
                                .px(step(2.5))
                                .pb(step(2.5))
                                .text_size(px(T_PILL))
                                .text_color(rgb(FAINT))
                                .child(t("config.checking")),
                        )
                    })
                    .when(homeless > 0, |column| {
                        let where_ = blamed
                            .as_ref()
                            .map(Section::title)
                            .unwrap_or_else(|| t("config.the_file").to_owned());
                        column.child(
                            div()
                                .id("faults")
                                .px(step(2.5))
                                .pb(step(2.5))
                                .flex()
                                .items_baseline()
                                .gap(step(1.5))
                                .cursor_pointer()
                                .child(
                                    div()
                                        .text_size(px(T_PILL))
                                        .text_color(rgb(FAINT))
                                        .child(match showing {
                                            true => "\u{25be}",
                                            false => "\u{25b8}",
                                        }),
                                )
                                .child(
                                    div()
                                        .text_size(px(T_PILL))
                                        .text_color(rgb(RED))
                                        .child(counted("config.faults", homeless, &[])),
                                )
                                .child(
                                    div()
                                        .font_family(self.mono.clone())
                                        .text_size(px(T_PILL))
                                        .text_color(rgb(FAINT))
                                        .truncate()
                                        .child(where_),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.showing_faults = !this.showing_faults;
                                    cx.notify();
                                })),
                        )
                    })
                    .children(sections.into_iter().map(|section| {
                        let chosen = section == open;
                        let label = section.title();
                        let group = matches!(section, Section::Group(_));
                        div()
                            .id(SharedString::from(format!("section-{label}")))
                            .px(step(2.5))
                            .py(step(1.5))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .gap(step(1.5))
                            .text_size(px(T_ROW))
                            .when(chosen, |row| row.bg(rgb(RAISED)).text_color(rgb(INK)))
                            .when(!chosen, |row| {
                                row.text_color(rgb(DIM)).hover(|row| row.bg(rgb(PANEL)))
                            })
                            .when(group, |row| row.child(dot(BLUE)))
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.section = section.clone();
                                this.editing = None;
                                cx.notify();
                            }))
                    })),
            )
            .child(
                div()
                    .id("fields")
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .overflow_y_scroll()
                    .px(step(6.))
                    .py(step(5.))
                    .flex()
                    .flex_col()
                    .gap(step(4.))
                    // Only when it was asked for, and never taller than
                    // a third of the pane: the form is what this column
                    // is for.
                    .when(showing && homeless > 0, |column| {
                        column.child(
                            div()
                                .id("faults-detail")
                                .max_h(px(220.))
                                .overflow_y_scroll()
                                .flex_shrink_0()
                                .rounded(px(8.))
                                .bg(tint(RED, 0x10))
                                .border_1()
                                .border_color(tint(RED, 0x44))
                                .p(step(3.5))
                                .flex()
                                .flex_col()
                                .gap(step(1.5))
                                .child(
                                    div()
                                        .text_size(px(T_META))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(rgb(RED))
                                        .child(t("config.refused")),
                                )
                                .children(
                                    faults
                                        .iter()
                                        .filter(|fault| fault_at(fault).is_none())
                                        .map(|fault| {
                                            div()
                                                .font_family(self.mono.clone())
                                                .text_size(px(T_META))
                                                .text_color(rgb(DIM))
                                                .child(crate::text::display_block(fault))
                                        }),
                                ),
                        )
                    })
                    .children(self.form(cx)),
            )
            .into_any_element()
    }


    /// The fields of the open section, from the schema.
    ///
    /// A section is usually one table, and then this draws one run of
    /// fields. `[experimental]` is three, so it draws three, each under
    /// its own heading, and every field carries the table it belongs to
    /// rather than taking the pane's.
    ///
    /// The experimental keys of an ordinary section come last, under a
    /// heading of their own, and only once the window has been let in.
    fn form(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let parts = drawn_with(&self.section);
        let mut drawn = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                drawn.push(self.heading(part.title()));
            }
            drawn.extend(self.run(part, cx));
        }
        if self.unlocked {
            let kept = self.kept_back(cx);
            if !kept.is_empty() {
                drawn.push(self.heading(t("config.experimental").to_owned()));
                drawn.extend(kept);
            }
        }
        drawn
    }

    /// The experimental keys of the open section, in the file's order.
    ///
    /// Drawn apart from the rest rather than filtered back in, so that
    /// turning the key on does not shuffle the settings somebody was
    /// already reading.
    fn kept_back(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        // Only the two sections that hold session settings have any:
        // the top of the file has no session keys, and reaching the
        // experimental tables at all is already the door.
        let shape = match &self.section {
            Section::Defaults => self.shape["$defs"]["Defaults"].get("properties").cloned(),
            Section::Group(_) => self.shape["$defs"]["Group"].get("properties").cloned(),
            _ => None,
        };
        let Some(serde_json::Value::Object(properties)) = shape else {
            return Vec::new();
        };
        properties
            .iter()
            .filter(|(key, _)| EXPERIMENTAL.contains(&key.as_str()))
            .map(|(key, field)| self.field(&self.section, key, field, cx))
            .collect()
    }

    /// A line naming the table the fields under it are written to.
    fn heading(&self, title: String) -> AnyElement {
        div()
            .pt(step(3.))
            .font_family(self.mono.clone())
            .text_size(px(T_META))
            .text_color(rgb(DIM))
            .child(title)
            .into_any_element()
    }

    /// The fields of one table, in the order the file writes them.
    fn run(&self, part: &Section, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let properties = match part {
            Section::Settings => self.shape.get("properties").cloned(),
            Section::Defaults => self.shape["$defs"]["Defaults"].get("properties").cloned(),
            Section::Advanced => self.shape["$defs"]["Advanced"].get("properties").cloned(),
            Section::Alerts => self.shape["$defs"]["AlertsAdvanced"]
                .get("properties")
                .cloned(),
            Section::Peering => self.shape["$defs"]["PeeringAdvanced"]
                .get("properties")
                .cloned(),
            Section::Group(_) => self.shape["$defs"]["Group"].get("properties").cloned(),
        };
        let Some(serde_json::Value::Object(properties)) = properties else {
            return vec![empty(t("config.no_shape"))];
        };
        let silent: &[&str] = match part {
            Section::Settings => SILENT_AT_THE_TOP,
            Section::Advanced => SILENT_IN_ADVANCED,
            _ => &[],
        };
        // The kept-back keys are written to this same table — that is
        // where the file wants them — but the form draws them at the
        // foot of the section, together, and only when it is let in.
        properties
            .iter()
            .filter(|(key, _)| !silent.contains(&key.as_str()))
            .filter(|(key, _)| !EXPERIMENTAL.contains(&key.as_str()))
            .map(|(key, field)| self.field(part, key, field, cx))
            .collect()
    }

    /// One field: its name, what it holds now, and the control for it.
    fn field(
        &self,
        part: &Section,
        key: &str,
        field: &serde_json::Value,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let about = field["description"].as_str().unwrap_or_default().to_owned();
        // One line of subtext, not two. The unit says what a valid
        // value looks like and the widget word says what kind of thing
        // it is; where there is a unit it has already said both.
        let hint = match field["x-unit"].as_str() {
            Some(unit) => unit.to_owned(),
            None => field["x-widget"].as_str().unwrap_or_default().to_owned(),
        };
        let held = self.held(part, key);
        let touched = self
            .sheet
            .as_ref()
            .is_some_and(|sheet| sheet.changed(part, key));
        let fault = self.at_fields.get(&(part.clone(), key.to_owned())).map(|at| At {
            section: at.section.clone(),
            key: at.key.clone(),
            said: at.said.clone(),
            instead: at.instead.clone(),
        });
        // A window that has not been let in is not offered the
        // experimental words — unless the file already holds one, in
        // which case hiding it would offer to change the setting to
        // something else and call that the only choice.
        let now = held
            .as_ref()
            .and_then(|item| item.as_str())
            .unwrap_or_default()
            .to_owned();
        let unlocked = self.unlocked;
        let words: Vec<(String, String)> = field["x-words"]
            .as_array()
            .map(|words| {
                words
                    .iter()
                    .filter(|word| {
                        let kept = word["experimental"].as_bool().unwrap_or(false);
                        !kept || unlocked || word["word"].as_str() == Some(now.as_str())
                    })
                    .map(|word| {
                        (
                            word["word"].as_str().unwrap_or_default().to_owned(),
                            word["about"].as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        div()
            .flex()
            .gap(step(4.))
            .child(
                div()
                    .w(px(210.))
                    .flex_shrink_0()
                    .pt(step(1.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(step(1.5))
                            .child(
                                div()
                                    .font_family(self.mono.clone())
                                    .text_size(px(T_ROW))
                                    // A key that differs from the file
                                    // says so where the key is read, not
                                    // only in a count at the top — and a
                                    // key the loader is refusing says so
                                    // louder than one merely edited.
                                    .text_color(match (fault.is_some(), touched) {
                                        (true, _) => rgb(RED),
                                        (false, true) => rgb(AMBER),
                                        (false, false) => rgb(INK),
                                    })
                                    .child(key.to_owned()),
                            )
                            .when(touched || fault.is_some(), |row| {
                                row.child(dot(match fault.is_some() {
                                    true => RED,
                                    false => AMBER,
                                }))
                            }),
                    )
                    .when(!hint.is_empty(), |column| {
                        column.child(
                            div()
                                .text_size(px(T_PILL))
                                .text_color(rgb(FAINT))
                                .child(hint.clone()),
                        )
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap(step(1.5))
                    .child(self.widget(part, key, field, &words, held, cx))
                    // The loader's complaint about this value, under the
                    // value: the mistake and the fix in one place.
                    .when_some(fault, |column, fault: At| {
                        let spot = Spot {
                            section: part.clone(),
                            key: key.to_owned(),
                            item: None,
                        };
                        column
                            .child(
                                div()
                                    .max_w(px(620.))
                                    .flex()
                                    .gap(step(1.5))
                                    .text_size(px(T_META))
                                    .text_color(rgb(RED))
                                    .child(div().flex_shrink_0().child("\u{26a0}"))
                                    .child(div().child(fault.said.clone())),
                            )
                            .when(!fault.instead.is_empty(), |column| {
                                column.child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .gap(step(1.5))
                                        .child(
                                            div()
                                                .text_size(px(T_PILL))
                                                .text_color(rgb(FAINT))
                                                .child(t("config.there_is")),
                                        )
                                        .children(fault.instead.iter().map(|word| {
                                            let taken = word.clone();
                                            let spot = spot.clone();
                                            div()
                                                .id(SharedString::from(format!(
                                                    "instead-{key}-{word}"
                                                )))
                                                .px(step(2.))
                                                .py(px(2.))
                                                .rounded(px(999.))
                                                .border_1()
                                                .border_color(rgb(LINE))
                                                .bg(rgb(SUNK))
                                                .cursor_pointer()
                                                .font_family(self.mono.clone())
                                                .text_size(px(T_PILL))
                                                .text_color(rgb(DIM))
                                                .hover(|chip| {
                                                    chip.bg(rgb(RAISED)).text_color(rgb(INK))
                                                })
                                                .child(word.clone())
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    let mut array = toml_edit::Array::new();
                                                    array.push(taken.clone());
                                                    this.put(&spot, Some(toml_edit::value(array)));
                                                    cx.notify();
                                                }))
                                        })),
                                )
                            })
                    })
                    .when(!about.is_empty(), |column| {
                        column.child(
                            div()
                                .max_w(px(620.))
                                .text_size(px(T_META))
                                .text_color(rgb(FAINT))
                                .child(first_sentence(&about)),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The control a field gets: words to choose from, a switch, a list,
    /// or a line of text.
    fn widget(
        &self,
        part: &Section,
        key: &str,
        field: &serde_json::Value,
        words: &[(String, String)],
        held: Option<toml_edit::Item>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section = part.clone();
        if !words.is_empty() {
            let now = held
                .as_ref()
                .and_then(|item| item.as_str())
                .unwrap_or_default()
                .to_owned();
            let set = held.is_some();
            return div()
                .flex()
                .flex_col()
                .gap(step(1.5))
                .child(div()
                .flex()
                .flex_wrap()
                .gap(step(1.5))
                .children(words.iter().map(|(word, about)| {
                    let chosen = *word == now;
                    let word = word.clone();
                    let writing = word.clone();
                    let key = key.to_owned();
                    let section = section.clone();
                    let about = SharedString::from(match chosen {
                        true => fill("tip.word_chosen", &[("about", about)]),
                        false => about.clone(),
                    });
                    div()
                        .id(SharedString::from(format!("word-{key}-{word}")))
                        .px(step(2.))
                        .py(step(1.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .border_1()
                        .text_size(px(T_META))
                        .when(chosen, |chip| {
                            chip.bg(tint(BLUE, 0x22))
                                .border_color(tint(BLUE, 0x60))
                                .text_color(rgb(BLUE))
                        })
                        .when(!chosen, |chip| {
                            chip.bg(rgb(RAISED))
                                .border_color(rgb(LINE))
                                .text_color(rgb(DIM))
                                .hover(|chip| chip.bg(rgb(0x252c36)))
                        })
                        .child(word.clone())
                        .tooltip(move |_, cx| {
                            let about = about.clone();
                            cx.new(|_| Tip { text: about }).into()
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            // Clicking the chosen word again takes the key
                            // out of the file, which is how a group goes
                            // back to inheriting one.
                            this.put(
                                &Spot {
                                    section: section.clone(),
                                    key: key.clone(),
                                    item: None,
                                },
                                (!chosen).then(|| toml_edit::value(writing.clone())),
                            );
                            cx.notify();
                        }))
                }))
                )
                .when(!set, |column| {
                    column.child(
                        div()
                            .text_size(px(T_META))
                            .text_color(rgb(FAINT))
                            .child(t("config.inherited")),
                    )
                })
                .into_any_element();
        }
        let default = &field["default"];
        match holds(field) {
            Holds::Switch => {
                let now = held
                    .as_ref()
                    .and_then(|item| item.as_bool())
                    .or_else(|| default.as_bool())
                    .unwrap_or(false);
                let set = held.is_some();
                div()
                    .flex()
                    .items_center()
                    .gap(step(2.5))
                    .child(self.switch(part, key, now, cx))
                    .when(!set, |row| {
                        row.child(
                            div()
                                .text_size(px(T_META))
                                .text_color(rgb(FAINT))
                                .child(match default.as_bool() {
                                    Some(true) => t("config.absent_on"),
                                    Some(false) => t("config.absent_off"),
                                    None => t("config.absent"),
                                }),
                        )
                    })
                    .into_any_element()
            }
            Holds::List => {
                let entries: Vec<String> = held
                    .as_ref()
                    .and_then(|item| item.as_array())
                    .map(|array| {
                        array
                            .iter()
                            .map(|value| value.as_str().unwrap_or_default().to_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                self.list(part, key, entries, cx)
            }
            Holds::Line => {
                let text = held.as_ref().map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| item.to_string().trim().to_owned())
                });
                self.line(part, key, None, text, cx)
            }
        }
    }

    fn switch(&self, part: &Section, key: &str, on: bool, cx: &mut Context<Self>) -> AnyElement {
        let section = part.clone();
        let key = key.to_owned();
        toggle_switch(SharedString::from(format!("switch-{key}")), on)
            .tooltip(match on {
                true => tip(t("tip.switch_on")),
                false => tip(t("tip.switch_off")),
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.put(
                    &Spot {
                        section: section.clone(),
                        key: key.clone(),
                        item: None,
                    },
                    Some(toml_edit::value(!on)),
                );
                cx.notify();
            }))
            .into_any_element()
    }

    /// A list of strings, edited as what it is: lines.
    ///
    /// Twenty ignore patterns are twenty lines, not twenty little boxes
    /// with a cross beside each. Reading them, pasting a few in, taking
    /// one out — all of that is what a block of text is for.
    fn list(
        &self,
        part: &Section,
        key: &str,
        entries: Vec<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let spot = Spot {
            section: part.clone(),
            key: key.to_owned(),
            item: None,
        };
        if let Some(edit) = self.editing.as_ref().filter(|edit| edit.at == spot) {
            return self.editor(edit, t("config.one_to_a_line"), cx);
        }
        let start = entries.join("\n");
        let count = entries.len();
        div()
            .flex()
            .flex_col()
            .gap(step(1.5))
            .child(
                div()
                    .id(SharedString::from(format!("list-{key}")))
                    .min_w(px(280.))
                    .max_w(px(620.))
                    .px(step(2.))
                    .py(step(1.5))
                    .rounded(px(6.))
                    .bg(rgb(RAISED))
                    .border_1()
                    .border_color(rgb(LINE))
                    .cursor_pointer()
                    .hover(|list| list.border_color(rgb(0x39424e)))
                    .tooltip(tip(t("tip.list")))
                    .font_family(self.mono.clone())
                    .text_size(px(T_META))
                    .flex()
                    .flex_col()
                    .when(count == 0, |list| {
                        list.child(div().text_color(rgb(FAINT)).child(t("config.nothing")))
                    })
                    .children(
                        entries
                            .into_iter()
                            .map(|entry| div().whitespace_nowrap().child(entry)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_edit(spot.clone(), start.clone(), true, window, cx);
                        cx.notify();
                    })),
            )
            .when(count > 1, |column| {
                column.child(
                    div()
                        .text_size(px(T_PILL))
                        .text_color(rgb(FAINT))
                        .child(counted("config.entries", count, &[])),
                )
            })
            .into_any_element()
    }

    /// The block of text itself, with the two things that end it.
    fn editor(&self, edit: &Edit, about: &'static str, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(step(1.5))
            .child(
                div()
                    .min_w(px(280.))
                    .max_w(px(620.))
                    .px(step(2.))
                    .py(step(1.5))
                    .rounded(px(6.))
                    .bg(rgb(SUNK))
                    .border_1()
                    .border_color(tint(BLUE, 0x80))
                    .child(edit.area.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(step(1.5))
                    .child(
                        button("edit-keep", t("config.done"))
                            .tooltip(tip(t("tip.keep_value")))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.finish_edit(true, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        button("edit-leave", t("config.cancel"))
                            .tooltip(tip(t("tip.leave_value")))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.finish_edit(false, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(T_PILL))
                            .text_color(rgb(FAINT))
                            .child(about),
                    ),
            )
            .into_any_element()
    }

    /// One value of text: what it holds, or the block editing it.
    fn line(
        &self,
        part: &Section,
        key: &str,
        item: Option<usize>,
        held: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let spot = Spot {
            section: part.clone(),
            key: key.to_owned(),
            item,
        };
        if let Some(edit) = self.editing.as_ref().filter(|edit| edit.at == spot) {
            return self.editor(edit, t("config.enter_keeps"), cx);
        }
        let start = held.clone().unwrap_or_default();
        div()
            .id(SharedString::from(format!("line-{}-{key}", part.title())))
            .h(step(7.))
            .px(step(2.))
            .min_w(px(280.))
            .max_w(px(620.))
            .rounded(px(6.))
            .border_1()
            .flex()
            .items_center()
            .cursor_pointer()
            .font_family(self.mono.clone())
            .text_size(px(T_META))
            .bg(rgb(RAISED))
            .border_color(rgb(LINE))
            .hover(|line| line.border_color(rgb(0x39424e)))
            .tooltip(tip(t("tip.line")))
            .text_color(match held.is_some() {
                true => rgb(INK),
                false => rgb(FAINT),
            })
            .child(match &held {
                Some(text) if text.is_empty() => t("config.empty").to_owned(),
                Some(text) => text.clone(),
                None => t("config.not_set").to_owned(),
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.start_edit(spot.clone(), start.clone(), false, window, cx);
                cx.notify();
            }))
            .into_any_element()
    }

    // ── what the editor does to the file ─────────────────────────────




    /// Opens a block of text over a value.
    fn start_edit(
        &mut self,
        at: Spot,
        text: String,
        list: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ink = area::Ink {
            text: INK,
            caret: BLUE,
            selection: BLUE,
        };
        let font = self.mono.clone();
        // A list shows ten lines and scrolls; a single value is one
        // line and always was.
        let rows = match list {
            true => 10,
            false => 1,
        };
        let area = cx.new(|cx| area::Area::new(text, list, rows, font, px(T_META), ink, cx));
        area.read(cx).focus(window);
        cx.subscribe(&area, |this, _, said, cx| {
            this.finish_edit(*said == area::Said::Keep, cx);
            cx.notify();
        })
        .detach();
        self.editing = Some(Edit { at, area, list });
    }

    /// Closes it, keeping what was typed or leaving the value alone.
    fn finish_edit(&mut self, keep: bool, cx: &mut Context<Self>) {
        let Some(edit) = self.editing.take() else {
            return;
        };
        if !keep {
            return;
        }
        let text = edit.area.read(cx).text().to_owned();
        let value = match edit.list {
            true => {
                let mut list = toml_edit::Array::new();
                for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
                    list.push(line);
                }
                Some(toml_edit::value(list))
            }
            false => match text.trim() {
                // An emptied value is not an empty value: the key comes
                // out of the file and whatever it inherits applies.
                "" => None,
                text => Some(number_or_text(text)),
            },
        };
        self.put(&edit.at, value);
    }

    /// The one key the window itself reads: everything else belongs to
    /// the block being typed into, which has the focus while it is open.
    fn typed(&mut self, event: &gpui::KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.platform && event.keystroke.key == "s" {
            self.save();
            cx.notify();
        }
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
                        toggle("errors-only", t("log.errors_only"), errors_only).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.errors_only = !this.errors_only;
                                cx.notify();
                            },
                        )),
                    )
                    .child(button("re-read", t("log.re_read"))
                        .tooltip(tip(t("tip.log_re_read")))
                        .on_click(cx.listener(
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
                            .child(counted(
                                "log.counted",
                                held,
                                &[("shown", &shown.to_string()), ("held", &held.to_string())],
                            )),
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
                    .children(lines.into_iter().enumerate().map(|(index, line)| {
                        let complaint = is_complaint(&line);
                        let chatter = line.contains("debug:");
                        let taken = line.clone();
                        div()
                            .id(SharedString::from(format!("log-{index}")))
                            .cursor_pointer()
                            .tooltip(tip(t("tip.copy")))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    taken.clone(),
                                ));
                                this.said =
                                    Some(fill("status.copied", &[("text", &cap(&taken, 90))]));
                                cx.notify();
                            }))
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
    fn hosts(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(report) = &self.report else {
            return empty(t("fleet.reading_fleet"));
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
                            .child(self.copyable(
                                format!("host-{index}"),
                                complaint,
                                colour_of(worst),
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .w(px(88.))
                            .flex_shrink_0()
                            .text_right()
                            .text_size(px(T_META))
                            .text_color(rgb(FAINT))
                            .child(counted("hosts.session", count, &[])),
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
                            .child(div().flex_1().min_w(px(0.)).child(label(t("hosts.host"))))
                            .child(
                                div()
                                    .w(px(320.))
                                    .flex_shrink_0()
                                    .child(label(t("hosts.said"))),
                            )
                            .child(
                                div()
                                    .w(px(88.))
                                    .flex_shrink_0()
                                    .flex()
                                    .justify_end()
                                    .child(label(t("hosts.carrying"))),
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
                    .child(label(t("hosts.bundle")))
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
                            fill(
                                "hosts.no_manifest",
                                &[(
                                    "path",
                                    &tilde(
                                        &self.state_root.join("agents").display().to_string(),
                                    ),
                                )],
                            ),
                        ),
                    })
                    .child(
                        div()
                            .pt(step(1.))
                            .font_family(self.mono.clone())
                            .text_size(px(T_META))
                            .text_color(rgb(FAINT))
                            .child(fill(
                                "hosts.build",
                                &[("version", &crate::protocol::version())],
                            )),
                    ),
            )
            .into_any_element()
    }

    // ── the seam ─────────────────────────────────────────────────────

    /// The window's own way of asking the sheet to do something, and of
    /// saying what came of it.
    fn read_sheet(&mut self) {
        match Sheet::read(self.config.as_deref()) {
            Ok(sheet) => self.sheet = Some(sheet),
            Err(complaint) => self.said = Some(complaint),
        }
    }

    fn held(&self, part: &Section, key: &str) -> Option<toml_edit::Item> {
        self.sheet.as_ref()?.held(part, key)
    }

    fn sections(&self) -> Vec<Section> {
        // The experimental tables are not listed at all until the
        // window has been let in; nothing points at a door either.
        let unlocked = self.unlocked;
        self.sheet
            .as_ref()
            .map(|sheet| sheet.sections())
            .unwrap_or_default()
            .into_iter()
            .filter(|section| unlocked || *section != Section::Advanced)
            .collect()
    }

    /// One click on the wordmark. Five in a row open the door.
    ///
    /// In a row, and within a few seconds of each other: a person who
    /// clicks the title twice today and three times tomorrow has not
    /// asked for anything. Five more shut it again, so a window that
    /// was opened to read one setting can be put back.
    fn knock(&mut self) {
        const RUN: Duration = Duration::from_secs(2);
        const ENOUGH: u8 = 5;
        let carried = self.knocked_at.is_some_and(|at| at.elapsed() < RUN);
        self.knocks = match carried {
            true => self.knocks + 1,
            false => 1,
        };
        self.knocked_at = Some(Instant::now());
        if self.knocks < ENOUGH {
            return;
        }
        self.knocks = 0;
        self.knocked_at = None;
        self.unlocked = !self.unlocked;
        // A section that is about to stop being listed must not stay
        // open underneath the list.
        if !self.unlocked && matches!(self.section, Section::Advanced) {
            self.section = Section::Settings;
        }
        self.said = Some(
            match self.unlocked {
                true => t("status.unlocked"),
                false => t("status.locked"),
            }
            .to_owned(),
        );
    }

    fn pending(&self) -> usize {
        self.sheet.as_ref().map_or(0, |sheet| sheet.pending())
    }

    fn put(&mut self, at: &Spot, value: Option<toml_edit::Item>) {
        let Some(sheet) = &mut self.sheet else { return };
        if let Some(said) = sheet.put(at, value) {
            self.said = Some(said);
        }
    }

    fn save(&mut self) {
        let Some(sheet) = &mut self.sheet else { return };
        if let Some(said) = sheet.save() {
            self.said = Some(said);
        }
    }

    fn revert(&mut self) {
        self.editing = None;
        self.read_sheet();
        self.said = Some(t("config.reverted").to_owned());
    }

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
                // One line, and the first fault rather than every
                // group's copy of it: the status bar has one line.
                let said = format!("{error:#}");
                let first = faults(&said)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| first_line(&said));
                self.said = Some(fill("status.config_refused", &[("error", &first)]));
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
                        alpha_root: group.alpha.clone(),
                        beta_root: session.beta.clone(),
                    });
                }
                for blocked in &session.blocked {
                    waiting.push(Conflict {
                        group: group.name.clone(),
                        host: session.host.clone(),
                        path: blocked.clone(),
                        blocked: true,
                        alpha_root: group.alpha.clone(),
                        beta_root: session.beta.clone(),
                    });
                }
            }
        }
        waiting
    }

    /// Opens a conflict: both sides are read once, here, so the window
    /// never touches a file on its way to a frame.
    fn open_conflict(&mut self, item: Conflict) {
        self.sides = Some((
            inspect("alpha", &item.alpha_root, &item.path),
            inspect("beta", &item.beta_root, &item.path),
        ));
        self.conflict = Some(item);
        self.diff = None;
    }

    /// Shows a file to the Finder, which is the one thing a window can
    /// do that a terminal cannot do better.
    fn reveal(&mut self, file: &std::path::Path) {
        let shown = std::process::Command::new("open").arg("-R").arg(file).status();
        self.said = Some(match shown {
            Ok(status) if status.success() => {
                fill("status.revealed", &[("path", &tilde(&file.display().to_string()))])
            }
            Ok(status) => fill("status.finder_refused", &[("status", &status.to_string())]),
            Err(error) => fill("status.finder_unreachable", &[("error", &error.to_string())]),
        });
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
                Ok(_) => fill("status.control_done", &[("done", verb.done()), ("beta", beta)]),
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
                self.sides = None;
                self.diff = None;
                fill(
                    "status.kept",
                    &[("keep", keep), ("path", &crate::text::display_safe(&item.path))],
                )
            }
            Ok(output) => String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            Err(error) => fill("status.resolve_failed", &[("error", &error.to_string())]),
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
                "" => t("status.diff_same").to_owned(),
                complaint => complaint.to_owned(),
            },
            Err(error) => fill("status.diff_failed", &[("error", &error.to_string())]),
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
                    self.log = vec![fill(
                        "log.unreadable",
                        &[
                            ("service", &service.display().to_string()),
                            ("watch", &watch.display().to_string()),
                        ],
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

    /// Opens a pane the way a reader would find it: with something in
    /// it. Used when a window opens straight onto one pane.
    fn settle(&mut self, pane: Pane) {
        match pane {
            Pane::Groups => {
                if self.selected.is_none() {
                    // The one that needs a person, if any: a window
                    // that opens on a session with nothing to say has
                    // wasted the only choice it gets to make.
                    self.selected = self.report.as_ref().and_then(|report| {
                        let sessions = || {
                            report.groups.iter().flat_map(|group| {
                                group.sessions.iter().map(move |session| (group, session))
                            })
                        };
                        // What needs a person first, then what is away,
                        // then whatever is first: a window that opens
                        // on a session with nothing to say has wasted
                        // the only choice it gets to make.
                        let wanted = sessions()
                            .find(|(_, session)| {
                                severity(&session.state) == Severity::Attention
                            })
                            .or_else(|| {
                                sessions().find(|(_, session)| {
                                    severity(&session.state) == Severity::Bad
                                })
                            })
                            .or_else(|| sessions().next());
                        wanted.map(|(group, session)| {
                            (group.name.clone(), session.session.clone())
                        })
                    });
                }
            }
            Pane::Conflicts => {
                if self.conflict.is_none() {
                    if let Some(first) =
                        self.waiting_list().into_iter().find(|item| !item.blocked)
                    {
                        self.open_conflict(first.clone());
                        let binary = self
                            .sides
                            .as_ref()
                            .is_some_and(|(alpha, beta)| alpha.binary || beta.binary);
                        if !binary {
                            self.read_diff(&first);
                        }
                    }
                }
            }
            Pane::Config => {
                if self.sheet.is_none() {
                    self.read_sheet();
                }
            }
            Pane::Log => {
                if self.log.is_empty() {
                    self.read_log();
                }
            }
            Pane::Hosts => {}
        }
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










/// A switch, for a key that is either on or off.
fn toggle_switch(id: SharedString, on: bool) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .w(px(44.))
        .h(step(6.))
        .rounded(px(12.))
        .p(px(3.))
        .cursor_pointer()
        .flex()
        .items_center()
        .when(on, |track| track.bg(tint(GREEN, 0x60)).justify_end())
        .when(!on, |track| track.bg(rgb(RAISED)).justify_start())
        .border_1()
        .border_color(match on {
            true => tint(GREEN, 0x80),
            false => rgb(LINE),
        })
        .child(
            div()
                .size(px(16.))
                .rounded_full()
                .bg(rgb(match on {
                    true => GREEN,
                    false => FAINT,
                })),
        )
}

/// What a button does, shown while the pointer rests on it.
struct Tip {
    text: SharedString,
}

impl Render for Tip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(step(2.))
            .py(step(1.))
            .rounded(px(6.))
            .bg(rgb(RAISED))
            .border_1()
            .border_color(rgb(LINE))
            .shadow_md()
            .text_size(px(T_META))
            .text_color(rgb(INK))
            .max_w(px(320.))
            .child(self.text.clone())
    }
}

/// Hangs a sentence on a control.
fn tip(text: &'static str) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    move |_, cx| cx.new(|_| Tip { text: text.into() }).into()
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
            Verb::Flush => t("verb.flush"),
            Verb::Verify => t("verb.verify"),
            Verb::Pause => t("verb.pause"),
            Verb::Resume => t("verb.resume"),
        }
    }

    /// What pressing it does, for the tooltip.
    fn about(self) -> &'static str {
        match self {
            Verb::Flush => t("tip.flush"),
            Verb::Verify => t("tip.verify"),
            Verb::Pause => t("tip.pause"),
            Verb::Resume => t("tip.resume"),
        }
    }

    fn done(self) -> &'static str {
        match self {
            Verb::Flush => t("verb.flushed"),
            Verb::Verify => t("verb.will_verify"),
            Verb::Pause => t("verb.paused"),
            Verb::Resume => t("verb.resumed"),
        }
    }
}










// ── how a state looks ────────────────────────────────────────────────



fn colour_of(severity: Severity) -> u32 {
    match severity {
        Severity::Fine => GREEN,
        Severity::Attention => AMBER,
        Severity::Bad => RED,
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
        parts.push(format!(
            "{attention} {}",
            counted("fleet.needs_you", attention, &[])
        ));
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


#[cfg(test)]
mod tests {
    use super::*;

    /// The fleet sorts and colours by how much a state needs a person,


    /// A root either names a directory this process can open or a
    /// machine it can only talk to, and the window says different things

    /// An edit through the form is an edit to the file the person wrote:
    /// their comments and their order survive it, and the parser — the
    /// same function the supervisor loads the file with — is what says

    /// The form asks the loader, not the parser: a document that serde
    /// takes but the supervisor would refuse must not reach the disk,
    /// because the supervisor re-reads the file within seconds of it

    /// A section the file has not got yet is made when something is

    /// A path that will not fit keeps its end, which is the part that

    /// Both answers to "how big": the one a person compares at a glance

    /// One pass over a file answers both questions, and the digest is
    /// the one the scanner records — so a side that matches here matches

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
