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

mod ink;

use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
    Editor, EditorState, InputEvent, InputHighlighter, Textarea, TextareaState,
};
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Icon, IconName, IndexPath, Root, Sizable as _, Theme, ThemeMode};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::supervisor::{status_report, GroupReport, SessionReport, StatusReport};
use crate::surface::{
    self, first_sentence, holds, tilde, Conflict, Holds, Section, Sheet, Side, Spot,
    EXPERIMENTAL, SILENT_AT_THE_TOP, SILENT_IN_ADVANCED,
};
use crate::words::{count as counted, fill, t};

/// How often the fleet is re-read when nothing is working, and when
/// something is.
const POLL_AT_REST: Duration = Duration::from_secs(2);
const POLL_WHILE_WORKING: Duration = Duration::from_millis(500);

/// How long after the last keystroke the loader is asked what it makes
/// of the document. Long enough not to run mid-word, short enough that
/// a mistake is pointed at while you are still looking at it.
const SETTLE_AFTER: Duration = Duration::from_millis(400);

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

/// How much room the window has, which is the only thing the layout
/// below asks about.
///
/// Three answers rather than a number: a pane that asks "how wide am
/// I" in pixels ends up with a different threshold in every corner of
/// the file, and they drift. The measurements are where a column stops
/// fitting, not where a device is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Room {
    /// Under 720 points: one column, and the rail becomes a row.
    Tight,
    /// Under 1080: the rail stays, the least useful columns go.
    Snug,
    /// Everything fits.
    Wide,
}

impl Room {
    fn of(window: &Window) -> Room {
        let width = window.viewport_size().width;
        if width < px(720.) {
            Room::Tight
        } else if width < px(1080.) {
            Room::Snug
        } else {
            Room::Wide
        }
    }
}

/// The panes, in the order the rail lists them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Groups,
    Hosts,
    Conflicts,
    Log,
    Config,
}

impl Pane {
    fn title(self) -> &'static str {
        match self {
            Pane::Groups => t("pane.groups"),
            Pane::Hosts => t("pane.hosts"),
            Pane::Conflicts => t("pane.conflicts"),
            Pane::Log => t("pane.log"),
            Pane::Config => t("pane.config"),
        }
    }

    /// The mark beside its name. From the kit's bundled Lucide set, so
    /// there is nothing to draw and nothing to ship.
    fn icon(self) -> IconName {
        match self {
            Pane::Groups => IconName::Folder,
            Pane::Hosts => IconName::Network,
            Pane::Conflicts => IconName::TriangleAlert,
            Pane::Log => IconName::FileText,
            Pane::Config => IconName::Settings,
        }
    }

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

/// Opens a window, on the pane it is asked for.
fn open_window(
    config: Option<PathBuf>,
    state_root: PathBuf,
    pane: Option<String>,
    cx: &mut App,
) -> gpui_kit::WindowHandle<Root> {
    // A width can be asked for, which is how the narrow layouts are
    // looked at without a hand on the window's edge.
    let asked = |name: &str, fallback: f32| {
        std::env::var(name)
            .ok()
            .and_then(|size| size.parse::<f32>().ok())
            .unwrap_or(fallback)
    };
    let bounds = Bounds::centered(
        None,
        size(
            px(asked("AUTOBAHN_DESK_WIDTH", 1240.)),
            px(asked("AUTOBAHN_DESK_HEIGHT", 820.)),
        ),
        cx,
    );
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
            let desk = cx.new(|cx| {
                let mut desk = Desk::new(config, state_root, cx);
                if let Some(pane) = pane.as_deref() {
                    // `config:defaults` opens the configuration on one of
                    // its sections, which is the only way a picture of a
                    // section can be taken without a hand on the mouse.
                    let (pane, section) = match pane.split_once(':') {
                        Some((pane, section)) => (pane, Some(section)),
                        None => (pane, None),
                    };
                    desk.pane = match pane {
                        "conflicts" => Pane::Conflicts,
                        "config" => Pane::Config,
                        "log" => Pane::Log,
                        "hosts" => Pane::Hosts,
                        _ => Pane::Groups,
                    };
                    if let Some(section) = section {
                        desk.section = match section {
                            "defaults" => Section::Defaults,
                            "advanced" => Section::Advanced,
                            "alerts" => Section::Alerts,
                            "peering" => Section::Peering,
                            "settings" => Section::Settings,
                            name => Section::Group(name.to_owned()),
                        };
                    }
                    // A window pointed straight at an experimental table
                    // is a window that was let in: the list would deny a
                    // section the pane is already showing otherwise. The
                    // environment says so too, which is how a picture of
                    // one gets taken without a hand on the mouse.
                    desk.unlocked = matches!(
                        desk.section,
                        Section::Advanced | Section::Alerts | Section::Peering
                    ) || std::env::var("AUTOBAHN_DESK_EXPERIMENTAL").is_ok();
                    desk.settle(desk.pane, window, cx);
                }
                desk
            });
            cx.new(|cx| Root::new(desk, window, cx))
        })
        .expect("unable to open the window");
    // The theme is the window's, not only the application's.
    window
        .update(cx, |_, window, cx| {
            Theme::change(ThemeMode::Dark, Some(window), cx);
        })
        .ok();
    window
}

/// The menu bar item, kept by the process rather than by a window.
struct Menubar(crate::menubar::Bar);

impl Global for Menubar {}

/// Keeps the item up to date and answers what is chosen in it.
fn watch_the_bar(config: Option<PathBuf>, state_root: PathBuf, cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            let sleep = cx.background_executor().timer(crate::menubar::POLL);
            sleep.await;
            cx.update(|cx| {
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
                        open_window(config.clone(), state_root.clone(), None, cx);
                    } else {
                        for window in cx.windows() {
                            window
                                .update(cx, |_, window, _| window.activate_window())
                                .ok();
                        }
                    }
                }
            });
        }
    })
    .detach();
}

/// One of the words a setting will take, and what choosing it means.
///
/// A row of chips said what the choices were but not what they did: the
/// reason each one exists only fitted in a tooltip, which is a place a
/// person finds by accident. In a list there is room for the sentence
/// beside the word.
#[derive(Clone)]
struct Choice {
    word: String,
    about: String,
    mono: SharedString,
}

impl SearchableListItem for Choice {
    type Value = String;

    fn title(&self) -> SharedString {
        self.word.clone().into()
    }

    fn value(&self) -> &String {
        &self.word
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(1.))
            .child(
                div()
                    .font_family(self.mono.clone())
                    .text_size(px(12.5))
                    .child(self.word.clone()),
            )
            .when(!self.about.is_empty(), |row| {
                row.child(
                    div()
                        .text_size(px(10.5))
                        .text_color(rgb(FAINT))
                        .child(self.about.clone()),
                )
            })
    }
}

/// The list behind one of those fields.
type Choices = SelectState<SearchableVec<Choice>>;

/// What the window is showing.
pub struct Desk {
    config: Option<PathBuf>,
    /// The face anything a person compares is set in.
    mono: SharedString,
    state_root: PathBuf,
    pane: Pane,
    /// How much room the last frame had, for the parts that are built
    /// before the frame knows.
    room: Room,
    report: Option<StatusReport>,
    read_at: Option<Instant>,
    selected: Option<(String, crate::supervisor::control::SessionKey)>,
    /// The conflict the conflicts pane has open, its two sides, and the
    /// difference when it has been asked for.
    conflict: Option<Conflict>,
    sides: Option<(Side, Side)>,
    diff: Option<String>,
    /// The log, in the kit's own code editor: selectable, searchable
    /// with control-F, and set in the monospace the theme names.
    log: Option<Entity<EditorState>>,
    log_path: Option<PathBuf>,
    log_lines: usize,
    /// How many lines the tail holds, before the filter.
    log_held: usize,
    /// The text last shown, so a tail that has nothing new leaves the
    /// block — and its selection, and its search — alone.
    log_text: String,
    /// Whether the log follows the file, re-reading and staying at the end.
    log_tail: bool,
    log_read_at: Option<Instant>,
    /// Whether the log shows only what the supervisor complained about.
    errors_only: bool,
    /// The configuration file, which section is open, and the block
    /// editing one of its values.
    sheet: Option<Sheet>,
    section: Section,
    /// One live block per value of the file, made as it is first drawn.
    fields: std::collections::HashMap<(Section, String), Entity<TextareaState>>,
    /// The same, for the fields that take one of a fixed set of words.
    choices: std::collections::HashMap<(Section, String), Entity<Choices>>,
    /// Which of those blocks hold a list rather than one value.
    lists: std::collections::HashSet<(Section, String)>,
    /// When the last keystroke landed, so the loader is asked once the
    /// typing stops rather than on every letter.
    typed_at: Option<Instant>,
    /// The form's own scroll, so Save can put the refusal in view.
    form: ScrollHandle,
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
    shape: serde_json::Value,
    said: Option<String>,
}

/// Runs the window until it is closed.
pub fn run(config: Option<PathBuf>, state_root: PathBuf) -> Result<()> {
    run_with(config, state_root, None)
}

/// The same window, photographed into `directory` and closed again.
pub fn shoot(
    config: Option<PathBuf>,
    state_root: PathBuf,
    directory: PathBuf,
    pane: Option<String>,
) -> Result<()> {
    run_with(config, state_root, Some((directory, pane)))
}

fn run_with(
    config: Option<PathBuf>,
    state_root: PathBuf,
    shots: Option<(PathBuf, Option<String>)>,
) -> Result<()> {
    // The icons are files the kit embeds, so the application has to be
    // told where its assets come from or every one of them draws as
    // nothing.
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx: &mut App| {
        gpui_kit::init(cx);
        cx.activate(true);
        let config = config.clone();
        let state_root = state_root.clone();
        let wanted = shots.as_ref().and_then(|(_, pane)| pane.clone());
        let window = open_window(config.clone(), state_root.clone(), wanted, cx);
        if shots.is_none() {
            // The same item in the menu bar the other window puts
            // there, from the same code: one poll, one notifier, and
            // "Open the window" when this one has been closed.
            match crate::menubar::Bar::start(config.clone(), state_root.clone(), || {}) {
                Ok(mut bar) => {
                    bar.window = true;
                    bar.appear();
                    cx.set_global(Menubar(bar));
                    watch_the_bar(config.clone(), state_root.clone(), cx);
                }
                Err(error) => eprintln!(
                    "{}",
                    fill("status.no_menu_bar", &[("error", &format!("{error:#}"))])
                ),
            }
        }
        let Some((directory, pane)) = shots.clone() else { return };
        cx.spawn(async move |cx| {
            let sleep = cx.background_executor().timer(Duration::from_millis(900));
            sleep.await;
            let name = pane.clone().unwrap_or_else(|| "groups".to_owned());
            let path = directory.join(format!("kit-{name}.png"));
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
            room: Room::Wide,
            report: None,
            read_at: None,
            selected: None,
            conflict: None,
            sides: None,
            diff: None,
            log: None,
            log_path: None,
            log_lines: 0,
            log_held: 0,
            log_text: String::new(),
            log_tail: false,
            log_read_at: None,
            errors_only: false,
            sheet: None,
            section: Section::Settings,
            fields: std::collections::HashMap::new(),
            choices: std::collections::HashMap::new(),
            lists: std::collections::HashSet::new(),
            typed_at: None,
            form: ScrollHandle::new(),
            unlocked: false,
            knocks: 0,
            knocked_at: None,
            shape: crate::config::schema(),
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
                    // A tailing log asks for a frame of its own: the reading
                    // itself needs a window, and only `render` has one.
                    if this.refresh_if_due() || (this.log_tail && this.pane == Pane::Log) {
                        cx.notify();
                    }
                    if this.quiet_for(SETTLE_AFTER) {
                        this.typed_at = None;
                        if this.sheet.as_mut().is_some_and(Sheet::settle) {
                            cx.notify();
                        }
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

    /// Whether typing has stopped for this long. False when nothing has
    /// been typed since the last time the loader caught up.
    fn quiet_for(&self, pause: Duration) -> bool {
        self.typed_at.is_some_and(|at| at.elapsed() >= pause)
    }

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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = self.pane;
        let room = Room::of(window);
        self.room = room;
        div()
            .size_full()
            .flex()
            .text_size(px(13.))
            .text_color(rgb(INK))
            .bg(rgb(GROUND))
            .when(room > Room::Tight, |shell| shell.child(self.rail(cx)))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(self.header())
                    .when(room == Room::Tight, |column| column.child(self.tabs(cx)))
                    .child(match pane {
                        Pane::Groups => self.groups(cx),
                        Pane::Conflicts => self.conflicts(cx),
                        Pane::Config => self.config_pane(window, cx),
                        Pane::Log => self.log_pane(window, cx),
                        Pane::Hosts => self.hosts(cx),
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
                                    .text_size(px(12.5))
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
                    .child(self.nav(Pane::Hosts, None, cx))
                    .child(self.nav(Pane::Conflicts, Some(waiting), cx))
                    .child(self.nav(Pane::Log, None, cx))
                    .child(self.nav(Pane::Config, Some(self.pending()), cx)),
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

    /// The panes as a row, when the window is too narrow for a rail.
    /// The rail's two facts — whether a supervisor is running, and
    /// which state root this is — move to the footer, which is the
    /// other place a person looks for them.
    fn tabs(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let waiting = self.waiting();
        let pending = self.pending();
        div()
            .id("tabs")
            .flex_shrink_0()
            .overflow_x_scroll()
            .px(step(3.))
            .py(step(1.5))
            .flex()
            .gap(step(1.))
            .border_b_1()
            .border_color(rgb(LINE))
            .bg(rgb(RAIL))
            .child(self.nav(Pane::Groups, None, cx))
            .child(self.nav(Pane::Hosts, None, cx))
            .child(self.nav(Pane::Conflicts, Some(waiting), cx))
            .child(self.nav(Pane::Log, None, cx))
            .child(self.nav(Pane::Config, Some(pending), cx))
            .into_any_element()
    }

    fn nav(&self, pane: Pane, badge: Option<usize>, cx: &mut Context<Self>) -> AnyElement {
        let chosen = self.pane == pane;
        let row = self.room == Room::Tight;
        div()
            .id(SharedString::from(format!("nav-{}", pane.title())))
            .h(step(7.5))
            .px(step(2.5))
            .rounded(px(6.))
            .flex()
            .items_center()
            .gap(step(1.5))
            .when(!row, |item| item.justify_between())
            .when(row, |item| item.flex_shrink_0())
            .cursor_pointer()
            .text_size(px(12.5))
            .when(chosen, |row| row.bg(rgb(RAISED)).text_color(rgb(INK)))
            .when(!chosen, |row| {
                row.text_color(rgb(DIM)).hover(|row| row.bg(rgb(SUNK)))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(step(2.))
                    .child(
                        Icon::new(pane.icon())
                            .size_4()
                            .text_color(rgb(match chosen {
                                true => INK,
                                false => FAINT,
                            })),
                    )
                    .child(pane.title()),
            )
            .when_some(badge.filter(|count| *count > 0), |row, count| {
                row.child(pill(count.to_string(), AMBER))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.pane = pane;
                this.settle(pane, window, cx);
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
            // With no rail, the traffic lights sit over this header, and
            // the title has to start after them.
            .when(self.room == Room::Tight, |header| header.pl(px(84.)))
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
                    // The words go before the numbers do.
                    .child(count(needs, self.word("fleet.needs_you", needs), AMBER))
                    .child(count(away, self.word("fleet.away", away), RED))
                    .child(count(fine, self.word("fleet.synchronized", fine), GREEN)),
            )
            .into_any_element()
    }

    /// The word beside a count, where there is room for one.
    fn word(&self, key: &str, n: usize) -> String {
        match self.room {
            Room::Tight => String::new(),
            _ => counted(key, n, &[]),
        }
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(step(2.))
                    .min_w(px(0.))
                    .when(self.room == Room::Tight, |line| {
                        let running = self.report.as_ref().map(|r| r.supervisor_running);
                        line.child(dot(match running {
                            Some(true) => GREEN,
                            Some(false) => RED,
                            None => FAINT,
                        }))
                    })
                    .child(match &self.said {
                        Some(said) => div()
                            .text_color(rgb(DIM))
                            .truncate()
                            .child(crate::text::display_safe(said).to_string()),
                        None => div().text_color(rgb(FAINT)).truncate().child(
                            match self.room {
                                Room::Tight => tilde(&self.state_root.display().to_string()),
                                _ => t("fleet.provenance").to_owned(),
                            },
                        ),
                    }),
            )
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
        let room = self.room;
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
                    // The mode is the first thing to go: it is the same
                    // for every session of a group nine times in ten.
                    .when(room == Room::Wide, |row| {
                        row.child(
                            div()
                                .w(px(100.))
                                .flex_shrink_0()
                                .font_family(self.mono.clone())
                                .text_size(px(11.))
                                .text_color(rgb(FAINT))
                                .truncate()
                                .child(session.mode.clone()),
                        )
                    })
                    .when(room > Room::Tight, |row| {
                        row.child(
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
                    })
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
                            .when(room > Room::Tight, |cell| cell.w(px(148.)))
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
                band.child(self.aside(crate::text::display_safe(error).to_string(), RED))
            })
            // What is waiting reads across the card, the reason first.
            .when(open, |band| {
                let mut band = band;
                for (heading, paths) in surface::waiting_groups(session) {
                    band = band.child(
                        div()
                            .pl(step(8.))
                            .pr(step(4.))
                            .pb(step(0.5))
                            .text_size(px(11.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(AMBER))
                            .child(heading),
                    );
                    for path in paths {
                        band = band.child(
                            div()
                                .pl(step(10.))
                                .pr(step(4.))
                                .pb(step(0.5))
                                .font_family(self.mono.clone())
                                .text_size(px(11.))
                                .text_color(rgb(DIM))
                                .truncate()
                                .child(path),
                        );
                    }
                    band = band.child(div().h(step(1.5)));
                }
                band
            })
            .when(open, |band| band.child(self.detail(group, session, cx)))
            .into_any_element()
    }
}


impl Desk {
    /// What a pane needs read before it is looked at.
    fn settle(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        match pane {
            Pane::Conflicts => {
                if self.conflict.is_none() {
                    if let Some(first) = self.waiting_list().into_iter().find(|it| !it.blocked) {
                        self.open_conflict(first);
                    }
                }
            }
            Pane::Log => {
                if self.log.is_none() {
                    self.read_log(window, cx);
                }
            }
            Pane::Config => {
                if self.sheet.is_none() {
                    self.read_sheet();
                }
            }
            _ => {}
        }
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

    fn open_conflict(&mut self, item: Conflict) {
        self.sides = Some((
            surface::inspect("alpha", &item.alpha_root, &item.path),
            surface::inspect("beta", &item.beta_root, &item.path),
        ));
        self.conflict = Some(item);
        self.diff = None;
    }

    /// A line hanging under a session row, indented past its dot.
    fn aside(&self, text: String, colour: u32) -> Div {
        div()
            .pl(step(8.))
            .pr(step(4.))
            .pb(step(2.))
            .font_family(self.mono.clone())
            .text_size(px(11.))
            .text_color(rgb(colour))
            .child(text)
    }

    /// The open session: the two roots, and the four things that can be
    /// asked of it. What the row above says is not said again.
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
            .child(self.pair(t("fleet.alpha"), tilde(&group.alpha)))
            .child(self.pair(t("fleet.beta"), tilde(&session.beta)))
            .child(
                div()
                    .pt(step(2.))
                    .flex()
                    .flex_wrap()
                    .gap(step(1.5))
                    .child(self.verb(group, session, Verb::Flush, cx))
                    .child(self.verb(group, session, Verb::Verify, cx))
                    .child(self.verb(group, session, Verb::Pause, cx))
                    .child(self.verb(group, session, Verb::Resume, cx)),
            )
            .into_any_element()
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
        Button::new(SharedString::from(format!(
            "verb-{name}-{beta}-{}",
            verb.word()
        )))
        .small()
        .outline()
        .label(verb.word())
        .tooltip(verb.about())
        .on_click(cx.listener(move |this, _, _, cx| {
            this.control(&name, &beta, &key, verb);
            cx.notify();
        }))
        .into_any_element()
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
                Ok(_) => fill(
                    "status.control_done",
                    &[("done", verb.done()), ("beta", beta)],
                ),
                Err(error) => format!("{error:#}"),
            },
        );
        self.read_at = None;
    }

    // ── the conflicts ────────────────────────────────────────────────

    fn conflicts(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let waiting = self.waiting_list();
        if waiting.is_empty() {
            return empty(t("conflicts.none"));
        }
        let open = self.conflict.clone();
        let room = self.room;
        // Narrow, the queue and what it opens take turns: a list that is
        // half a window wide beside a detail that is the other half is
        // two things neither of which can be read.
        let showing_detail = room == Room::Tight && open.is_some();
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .when(!showing_detail, |pane| pane.child(
                div()
                    .id("queue")
                    .when(room > Room::Tight, |queue| queue.w(px(352.)).flex_shrink_0())
                    .when(room == Room::Tight, |queue| queue.flex_1().min_w(px(0.)))
                    .h_full()
                    .overflow_y_scroll()
                    .p(step(3.))
                    .flex()
                    .flex_col()
                    .gap(step(0.5))
                    .border_r_1()
                    .border_color(rgb(LINE))
                    .bg(rgb(SUNK))
                    .children(waiting.into_iter().enumerate().map(|(index, item)| {
                        let chosen = open.as_ref() == Some(&item);
                        let name = item
                            .path
                            .rsplit('/')
                            .next()
                            .unwrap_or(&item.path)
                            .to_owned();
                        let where_ = match item.path.rsplit_once('/') {
                            Some((directory, _)) => format!("{} · {directory}/", item.group),
                            None => item.group.clone(),
                        };
                        let colour = match item.blocked {
                            true => RED,
                            false => AMBER,
                        };
                        let word = match item.blocked {
                            true => t("conflicts.blocked"),
                            false => t("conflicts.conflict"),
                        };
                        let taken = item.clone();
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
                                    .child(dot(colour))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .font_family(self.mono.clone())
                                            .text_size(px(12.5))
                                            .truncate()
                                            .child(crate::text::display_safe(&name).to_string()),
                                    )
                                    .child(pill(word, colour)),
                            )
                            .child(
                                div()
                                    .pl(step(3.5))
                                    .font_family(self.mono.clone())
                                    .text_size(px(10.5))
                                    .text_color(rgb(FAINT))
                                    .truncate()
                                    .child(where_),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_conflict(taken.clone());
                                cx.notify();
                            }))
                    })),
            ))
            .when(room > Room::Tight || showing_detail, |pane| {
                pane.child(match self.conflict.clone() {
                    None => empty(t("conflicts.pick")),
                    Some(item) => self.conflict_detail(item, cx),
                })
            })
            .into_any_element()
    }

    fn conflict_detail(&mut self, item: Conflict, cx: &mut Context<Self>) -> AnyElement {
        let sides = self.sides.clone();
        let binary = sides
            .as_ref()
            .is_some_and(|(alpha, beta)| alpha.binary || beta.binary);
        let keep_host = match item.host.contains(':') {
            true => surface::short_name(&item.host),
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
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(step(2.))
                            .when(self.room == Room::Tight, |head| {
                                head.child(
                                    Button::new("back-to-queue")
                                        .small()
                                        .ghost()
                                        .label(t("conflicts.back"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.conflict = None;
                                            this.sides = None;
                                            this.diff = None;
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(label(match item.blocked {
                                true => t("conflicts.blocked"),
                                false => t("conflicts.conflict"),
                            })),
                    )
                    .child(
                        div()
                            .font_family(self.mono.clone())
                            .text_size(px(13.))
                            .child(crate::text::display_safe(&item.path).to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(FAINT))
                            .child(format!("{} · {}", item.group, tilde(&item.host))),
                    )
                    .when(item.blocked, |head| {
                        head.child(
                            div()
                                .pt(step(1.))
                                .text_size(px(11.))
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
                                    Button::new("keep-alpha")
                                        .small()
                                        .outline()
                                        .label(t("conflicts.keep_alpha"))
                                        .tooltip(t("tip.keep_alpha"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.resolve(&alpha, "alpha");
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("keep-beta")
                                        .small()
                                        .outline()
                                        .label(fill(
                                            "conflicts.keep_beta",
                                            &[("name", &keep_host)],
                                        ))
                                        .tooltip(t("tip.keep_beta"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let keep = host.host.clone();
                                            this.resolve(&host, &keep);
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("keep-both")
                                        .small()
                                        .outline()
                                        .label(t("conflicts.keep_both"))
                                        .tooltip(t("tip.keep_both"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.resolve(&both, "both");
                                            cx.notify();
                                        })),
                                )
                                .when(!binary, |row| {
                                    row.child(
                                        Button::new("show-diff")
                                            .small()
                                            .label(t("conflicts.show_difference"))
                                            .tooltip(t("tip.show_difference"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.read_diff(&shown);
                                                cx.notify();
                                            })),
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
                        .text_size(px(11.))
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
        let name = item.path.rsplit('/').next().unwrap_or(&item.path).to_owned();
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
                    .text_size(px(11.))
                    .text_color(rgb(DIM))
                    .child(t("conflicts.binary_about")),
            )
            .child(
                div()
                    .flex()
                    .gap(step(4.))
                    .when(self.room == Room::Tight, |sides| sides.flex_col())
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
                                .text_size(px(11.))
                                .text_color(rgb(GREEN))
                                .child(fill("conflicts.same", &[("name", &name)])),
                        ),
                )
            })
            .into_any_element()
    }

    fn side_card(&self, side: &Side, newer: bool, cx: &mut Context<Self>) -> Div {
        let file = side.file.clone();
        let name = side.name;
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
                            .text_size(px(12.5))
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
                    .text_size(px(11.))
                    .text_color(rgb(FAINT))
                    .truncate()
                    .child(surface::tail(&side.root, 3)),
            )
            .child(div().h(step(0.5)))
            .child(self.pair(
                t("conflicts.size"),
                side.size
                    .map(surface::human_size)
                    .unwrap_or_else(|| t("conflicts.unknown").to_owned()),
            ))
            .child(self.pair(
                t("conflicts.written"),
                side.modified
                    .map(|at| crate::logging::stamp(at as libc::time_t))
                    .unwrap_or_else(|| t("conflicts.unknown").to_owned()),
            ))
            .child(self.pair(
                t("conflicts.digest"),
                match (&side.digest, side.size) {
                    (Some(digest), _) => digest.chars().take(16).collect::<String>(),
                    (None, Some(size)) if size > surface::HASH_LIMIT => {
                        t("conflicts.too_large").to_owned()
                    }
                    _ => t("conflicts.unknown").to_owned(),
                },
            ))
            .when_some(file, |card, file| {
                card.child(
                    div().pt(step(1.5)).flex().child(
                        Button::new(SharedString::from(format!("reveal-{name}")))
                            .small()
                            .outline()
                            .label(t("conflicts.reveal"))
                            .tooltip(t("tip.reveal"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.reveal(&file);
                                cx.notify();
                            })),
                    ),
                )
            })
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
                    .text_size(px(10.5))
                    .text_color(rgb(FAINT))
                    .child(name),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .font_family(self.mono.clone())
                    .text_size(px(12.))
                    .truncate()
                    .child(value),
            )
    }

    // ── the hosts ────────────────────────────────────────────────────

    fn hosts(&mut self, _cx: &mut Context<Self>) -> AnyElement {
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
        let room = self.room;
        let manifest = std::fs::read_to_string(self.state_root.join("agents").join("MANIFEST"));
        let rows: Vec<Div> = hosts
            .into_iter()
            .enumerate()
            .map(|(index, (host, count, worst, state, error))| {
                let said = error
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
                            .text_size(px(12.5))
                            .truncate()
                            .child(tilde(&host)),
                    )
                    .when(room > Room::Tight, |row| {
                        row.child(
                            div()
                                .w(match room {
                                    Room::Wide => px(320.),
                                    _ => px(200.),
                                })
                                .flex_shrink_0()
                                .font_family(self.mono.clone())
                                .text_size(px(11.))
                                .text_color(rgb(colour_of(worst)))
                                .truncate()
                                .child(said),
                        )
                    })
                    .child(
                        div()
                            .w(px(88.))
                            .flex_shrink_0()
                            .text_right()
                            .text_size(px(11.))
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
                            .when(room > Room::Tight, |head| {
                                head.child(
                                    div()
                                        .w(match room {
                                            Room::Wide => px(320.),
                                            _ => px(200.),
                                        })
                                        .flex_shrink_0()
                                        .child(label(t("hosts.said"))),
                                )
                            })
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
                            .text_size(px(11.))
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
                        Err(_) => div().text_size(px(11.)).text_color(rgb(FAINT)).child(fill(
                            "hosts.no_manifest",
                            &[(
                                "path",
                                &tilde(&self.state_root.join("agents").display().to_string()),
                            )],
                        )),
                    })
                    .child(
                        div()
                            .pt(step(1.))
                            .font_family(self.mono.clone())
                            .text_size(px(11.))
                            .text_color(rgb(FAINT))
                            .child(fill("hosts.build", &[("version", &crate::protocol::version())])),
                    ),
            )
            .into_any_element()
    }

    // ── the log ──────────────────────────────────────────────────────

    /// The supervisor's account, in a block that selects and searches:
    /// the kit's own text area, read-only.
    fn log_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // Tailing happens here rather than in the poll loop: building a
        // block needs a window, and this is where one is in hand. The
        // loop only says the clock has moved, by asking for a frame.
        let due = self
            .log_read_at
            .is_none_or(|at| at.elapsed() >= POLL_AT_REST);
        if self.log.is_none() || (self.log_tail && due) {
            self.read_log(window, cx);
        }
        let Some(text) = self.log.clone() else {
            return empty(t("fleet.reading_fleet"));
        };
        let path = self
            .log_path
            .as_ref()
            .map(|path| tilde(&path.display().to_string()))
            .unwrap_or_default();
        let lines = self.log_lines;
        let held = self.log_held;
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
                        Button::new("errors-only")
                            .small()
                            .when(self.errors_only, |button| button.primary())
                            .when(!self.errors_only, |button| button.outline())
                            .label(t("log.errors_only"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.errors_only = !this.errors_only;
                                this.log = None;
                                this.read_log(window, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("tail")
                            .small()
                            .when(self.log_tail, |button| button.primary())
                            .when(!self.log_tail, |button| button.outline())
                            .label(t("log.tail"))
                            .tooltip(t("tip.log_tail"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.log_tail = !this.log_tail;
                                if this.log_tail {
                                    this.log = None;
                                    this.read_log(window, cx);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("re-read")
                            .small()
                            .outline()
                            .label(t("log.re_read"))
                            .tooltip(t("tip.log_re_read"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.log = None;
                                this.read_log(window, cx);
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(step(2.5))
                            .font_family(self.mono.clone())
                            .text_size(px(11.))
                            .text_color(rgb(FAINT))
                            .child(path)
                            .child(counted(
                                "log.counted",
                                held,
                                &[("shown", &lines.to_string()), ("held", &held.to_string())],
                            )),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .p(step(3.))
                    .child(Editor::new(&text).readonly(true).h_full()),
            )
            .into_any_element()
    }

    /// Read the tail of the log into the block, and leave it at the end.
    ///
    /// The whole file is never shown: four hundred lines is what a person
    /// reads after something went wrong, and the count in the header says
    /// how many were held back.
    fn read_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.log_read_at = Some(Instant::now());
        let service = self.state_root.join("service.log");
        let watch = self.state_root.join("watch.log");
        let (path, text) = match std::fs::read_to_string(&service) {
            Ok(text) => (Some(service), text),
            Err(_) => match std::fs::read_to_string(&watch) {
                Ok(text) => (Some(watch), text),
                Err(_) => (
                    None,
                    fill(
                        "log.unreadable",
                        &[
                            ("service", &service.display().to_string()),
                            ("watch", &watch.display().to_string()),
                        ],
                    ),
                ),
            },
        };
        let held = text.lines().rev().take(400).count();
        let tail: Vec<&str> = text
            .lines()
            .rev()
            .take(400)
            .filter(|line| !self.errors_only || surface::is_complaint(line))
            .collect();
        let shown: String = tail.into_iter().rev().collect::<Vec<&str>>().join("\n");
        // A tail that found nothing new keeps the block it has, and with
        // it the selection, the search and wherever the reader had got to.
        if self.log.is_some() && shown == self.log_text {
            return;
        }
        self.log_lines = shown.lines().count();
        self.log_held = held;
        self.log_path = path;
        self.log_text = shown.clone();
        let block = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("log")
                // A log is read at its end, so the end is where it stops:
                // half a screen of nothing under the last line is room to
                // type into, which nobody does here.
                .scroll_beyond_last_line(Some(0))
                .default_value(shown);
            // The kit ships a highlighter per bundled language and none of
            // them is a log; ours is small enough to hand over directly.
            state.set_highlighter_factory(
                Rc::new(|language| match language {
                    "log" => Some(Box::new(ink::LogInk::new()) as Box<dyn InputHighlighter>),
                    _ => None,
                }),
                cx,
            );
            state
        });
        // The end of a log is the part worth reading, so that is where it
        // opens. Anything past the last line clamps to the last line.
        block.update(cx, |state, cx| {
            state.set_scroll_offset(point(px(0.), px(-1.0e9)), cx);
        });
        self.log = Some(block);
    }

    // ── the configuration ────────────────────────────────────────────

    fn read_sheet(&mut self) {
        match Sheet::read(self.config.as_deref()) {
            Ok(sheet) => self.sheet = Some(sheet),
            Err(complaint) => self.said = Some(complaint),
        }
    }

    fn held(&self, part: &Section, key: &str) -> Option<toml_edit::Item> {
        self.sheet.as_ref()?.held(part, key)
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

    fn config_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.sheet.is_none() {
            self.read_sheet();
        }
        let Some(sheet) = &self.sheet else {
            return empty(t("config.none"));
        };
        let path = sheet.path.clone();
        let refused = sheet.refused().map(str::to_owned);
        let pending = sheet.pending();
        // The experimental tables are not listed at all until the
        // window has been let in; nothing points at a door either.
        let unlocked = self.unlocked;
        let sections: Vec<Section> = sheet
            .sections()
            .into_iter()
            .filter(|section| unlocked || *section != Section::Advanced)
            .collect();
        let open = self.section.clone();
        let room = self.room;
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .when(room == Room::Tight, |pane| pane.flex_col())
            .child(
                div()
                    .id("sections")
                    .flex_shrink_0()
                    .p(step(3.))
                    .flex()
                    .gap(step(0.5))
                    .bg(rgb(SUNK))
                    // Beside the form when there is room for a column,
                    // and a row of them above it when there is not.
                    .when(room > Room::Tight, |column| {
                        column
                            .w(px(260.))
                            .h_full()
                            .overflow_y_scroll()
                            .flex_col()
                            .border_r_1()
                            .border_color(rgb(LINE))
                    })
                    .when(room == Room::Tight, |row| {
                        row.w_full()
                            .overflow_x_scroll()
                            .items_center()
                            .border_b_1()
                            .border_color(rgb(LINE))
                    })
                    .child(
                        div()
                            .px(step(2.5))
                            .flex_shrink_0()
                            .when(room > Room::Tight, |line| line.pb(step(1.)))
                            .font_family(self.mono.clone())
                            .text_size(px(10.5))
                            .text_color(rgb(FAINT))
                            .truncate()
                            .child(surface::tail(&tilde(&path.display().to_string()), 3)),
                    )
                    // The sentence about saving is worth its room only
                    // when there is room.
                    .when(room > Room::Tight, |column| {
                        column.child(
                            div()
                                .px(step(2.5))
                                .pb(step(2.))
                                .text_size(px(10.5))
                                .text_color(rgb(FAINT))
                                .child(t("config.held")),
                        )
                    })
                    .child(
                        div()
                            .px(step(2.))
                            .flex_shrink_0()
                            .when(room > Room::Tight, |row| row.pb(step(2.5)).flex_wrap())
                            .flex()
                            .gap(step(1.5))
                            .child(
                                Button::new("save-config")
                                    .small()
                                    .outline()
                                    .when(pending > 0 && refused.is_none(), |save| save.primary())
                                    .label(t("config.save"))
                                    .tooltip(t("tip.save"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.take_the_fields(cx);
                                        this.save();
                                        // Whatever the loader said is at
                                        // the top of the form, which is
                                        // not where a long section leaves
                                        // you when you press Save.
                                        this.form.set_offset(point(px(0.), px(0.)));
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("revert-config")
                                    .small()
                                    .outline()
                                    .label(match pending {
                                        0 => t("config.re_read"),
                                        _ => t("config.revert"),
                                    })
                                    .tooltip(match pending {
                                        0 => t("tip.config_re_read"),
                                        _ => t("tip.revert"),
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.fields.clear();
                                        this.choices.clear();
                                        this.lists.clear();
                                        this.sheet = None;
                                        this.read_sheet();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .when(pending > 0, |column| {
                        column.child(
                            div()
                                .px(step(2.5))
                                .flex_shrink_0()
                                .when(room > Room::Tight, |line| line.pb(step(2.5)))
                                .text_size(px(10.5))
                                .text_color(rgb(AMBER))
                                .child(counted("config.pending", pending, &[])),
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
                            .flex_shrink_0()
                            .items_center()
                            .gap(step(1.5))
                            .text_size(px(12.5))
                            .when(chosen, |row| row.bg(rgb(RAISED)).text_color(rgb(INK)))
                            .when(!chosen, |row| {
                                row.text_color(rgb(DIM)).hover(|row| row.bg(rgb(PANEL)))
                            })
                            .when(group, |row| row.child(dot(BLUE)))
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.section = section.clone();
                                cx.notify();
                            }))
                    })),
            )
            .child(
                div()
                    .id("fields")
                    .track_scroll(&self.form)
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .overflow_y_scroll()
                    .px(step(6.))
                    .py(step(5.))
                    .flex()
                    .flex_col()
                    .gap(step(4.))
                    .when_some(refused, |column, refused| {
                        column.child(
                            div()
                                .rounded(px(8.))
                                .bg(tint(AMBER, 0x14))
                                .border_1()
                                .border_color(tint(AMBER, 0x50))
                                .p(step(3.5))
                                .flex()
                                .flex_col()
                                .gap(step(1.5))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(rgb(AMBER))
                                        .child(t("config.refused")),
                                )
                                .child(
                                    div()
                                        .font_family(self.mono.clone())
                                        .text_size(px(11.))
                                        .text_color(rgb(DIM))
                                        .child(crate::text::display_block(&refused)),
                                ),
                        )
                    })
                    .children(self.form(window, cx)),
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
    fn form(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let parts = surface::drawn_with(&self.section);
        let mut drawn = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            if index > 0 {
                drawn.push(self.heading(part.title()));
            }
            drawn.extend(self.run(part, window, cx));
        }
        if self.unlocked {
            let kept = self.kept_back(window, cx);
            if !kept.is_empty() {
                drawn.push(self.heading(t("config.experimental").to_owned()));
                drawn.extend(kept);
            }
        }
        drawn
    }

    /// A line naming the table the fields under it are written to.
    fn heading(&self, title: String) -> AnyElement {
        div()
            .pt(step(3.))
            .font_family(self.mono.clone())
            .text_size(px(11.5))
            .text_color(rgb(DIM))
            .child(title)
            .into_any_element()
    }

    /// The fields of one table, in the order the file writes them.
    fn run(&mut self, part: &Section, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
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
        let mut fields: Vec<(&String, &serde_json::Value)> = properties
            .iter()
            .filter(|(key, _)| !silent.contains(&key.as_str()))
            .collect();
        // The schema is a map, so its own order is alphabetical; the
        // reading order is in the file beside the parser.
        fields.sort_by_key(|(key, field)| {
            (
                field["x-order"].as_u64().unwrap_or(u64::MAX),
                (*key).clone(),
            )
        });
        // The kept-back keys are written to this same table — that is
        // where the file wants them — but the form draws them at the
        // foot of the section, together, and only when it is let in.
        fields
            .into_iter()
            .filter(|(key, _)| !EXPERIMENTAL.contains(&key.as_str()))
            .map(|(key, field)| self.field(part, key, field, window, cx))
            .collect()
    }

    /// The experimental keys of the open section, in the file's order.
    ///
    /// Drawn apart from the rest rather than filtered back in, so that
    /// turning the key on does not shuffle the settings somebody was
    /// already reading.
    fn kept_back(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        // The experimental tables themselves have none: reaching them
        // at all is already the door.
        let shape = match &self.section {
            Section::Settings => self.shape.get("properties").cloned(),
            Section::Defaults => self.shape["$defs"]["Defaults"].get("properties").cloned(),
            Section::Group(_) => self.shape["$defs"]["Group"].get("properties").cloned(),
            _ => None,
        };
        let Some(serde_json::Value::Object(properties)) = shape else {
            return Vec::new();
        };
        let part = self.section.clone();
        let mut fields: Vec<(&String, &serde_json::Value)> = properties
            .iter()
            .filter(|(key, _)| EXPERIMENTAL.contains(&key.as_str()))
            .collect();
        fields.sort_by_key(|(key, field)| {
            (
                field["x-order"].as_u64().unwrap_or(u64::MAX),
                (*key).clone(),
            )
        });
        fields
            .into_iter()
            .map(|(key, field)| self.field(&part, key, field, window, cx))
            .collect()
    }

    fn field(
        &mut self,
        part: &Section,
        key: &str,
        field: &serde_json::Value,
        window: &mut Window,
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
        let fallback = field["x-default"].as_str().unwrap_or_default().to_owned();
        let held = self.held(part, key);
        let touched = self
            .sheet
            .as_ref()
            .is_some_and(|sheet| sheet.changed(part, key));
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
            .when(self.room == Room::Tight, |field| field.flex_col().gap(step(1.5)))
            .child(
                div()
                    .when(self.room > Room::Tight, |name| {
                        name.w(px(210.)).flex_shrink_0().pt(step(1.))
                    })
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
                                    .text_size(px(12.5))
                                    // A key that differs from the file
                                    // says so where the key is read,
                                    // not only in a count at the top.
                                    .when(touched, |name| name.text_color(rgb(AMBER)))
                                    .child(key.to_owned()),
                            )
                            .when(touched, |row| row.child(dot(AMBER))),
                    )
                    .when(!hint.is_empty(), |column| {
                        column.child(
                            div()
                                .text_size(px(10.5))
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
                    .child(self.widget(part, key, field, &words, held, &fallback, window, cx))
                    .when(!about.is_empty(), |column| {
                        column.child(
                            div()
                                .max_w(px(620.))
                                .text_size(px(11.))
                                .text_color(rgb(FAINT))
                                .child(first_sentence(&about)),
                        )
                    }),
            )
            .into_any_element()
    }

    fn widget(
        &mut self,
        part: &Section,
        key: &str,
        field: &serde_json::Value,
        words: &[(String, String)],
        held: Option<toml_edit::Item>,
        fallback: &str,
        window: &mut Window,
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
            let default = match field["default"].as_str() {
                Some(default) => default.to_owned(),
                None => fallback.to_owned(),
            };
            let list = self.choose(part, key, words, &now, window, cx);
            return div()
                .flex()
                .flex_col()
                .gap(step(1.5))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(step(1.5))
                        .child(
                            // Wide enough for the longest word any of
                            // these take, and no wider: a control the
                            // width of the pane reads as a text field.
                            div().w(px(260.)).flex_shrink_0().child(
                                Select::new(&list)
                                    .menu_width(px(580.))
                                    .placeholder(match default.is_empty() {
                                        true => t("config.absent").to_owned(),
                                        false => fill(
                                            "config.default_is",
                                            &[("default", &default)],
                                        ),
                                    }),
                            ),
                        )
                        .when(set, |row| {
                            let at = (part.clone(), key.to_owned());
                            row.child(
                                Button::new(SharedString::from(format!("unset-{key}")))
                                    .small()
                                    .ghost()
                                    .label(t("config.unset"))
                                    .tooltip(t("tip.unset"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.unset(&at);
                                        cx.notify();
                                    })),
                            )
                        }),
                )
                .when(!set, |column| {
                    column.child(
                        div()
                            .text_size(px(11.))
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
                let key = key.to_owned();
                let section = section.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(step(2.5))
                    .child(
                        Switch::new(SharedString::from(format!("switch-{key}")))
                            .checked(now)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.put(
                                    &Spot {
                                        section: section.clone(),
                                        key: key.clone(),
                                        item: None,
                                    },
                                    Some(toml_edit::value(!now)),
                                );
                                cx.notify();
                            })),
                    )
                    .when(!set, |row| {
                        row.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(FAINT))
                                .child(match default.as_bool() {
                                    Some(true) => t("config.absent_on").to_owned(),
                                    Some(false) => t("config.absent_off").to_owned(),
                                    None => t("config.absent").to_owned(),
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
                self.value(part, key, entries.join("\n"), true, fallback, window, cx)
            }
            Holds::Line => {
                let text = held
                    .as_ref()
                    .map(|item| {
                        item.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| item.to_string().trim().to_owned())
                    })
                    .unwrap_or_default();
                self.value(part, key, text, false, fallback, window, cx)
            }
        }
    }

    /// The list for a field that takes one of a fixed set of words.
    ///
    /// Built once and kept: the list owns which row is chosen, and what
    /// it hears from the person is written the moment they choose it —
    /// a word is never half-typed, so there is nothing for Save to take.
    fn choose(
        &mut self,
        part: &Section,
        key: &str,
        words: &[(String, String)],
        now: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Choices> {
        let at = (part.clone(), key.to_owned());
        if let Some(list) = self.choices.get(&at) {
            return list.clone();
        }
        let items: Vec<Choice> = words
            .iter()
            .map(|(word, about)| Choice {
                word: word.clone(),
                about: first_sentence(about),
                mono: self.mono.clone(),
            })
            .collect();
        let chosen = items
            .iter()
            .position(|choice| choice.word == now)
            .map(IndexPath::new);
        let list = cx.new(|cx| SelectState::new(SearchableVec::new(items), chosen, window, cx));
        let spot = Spot {
            section: at.0.clone(),
            key: at.1.clone(),
            item: None,
        };
        cx.subscribe(&list, move |this, _, event: &SelectEvent<_>, cx| {
            let SelectEvent::Confirm(word) = event;
            this.put(&spot, word.clone().map(toml_edit::value));
            cx.notify();
        })
        .detach();
        self.choices.insert(at, list.clone());
        list
    }

    /// Take a word back out of the file, and out of the list with it.
    fn unset(&mut self, at: &(Section, String)) {
        self.put(
            &Spot {
                section: at.0.clone(),
                key: at.1.clone(),
                item: None,
            },
            None,
        );
        // The list holds which row is chosen, so it is rebuilt rather
        // than argued with: the next frame reads the file again.
        self.choices.remove(at);
    }

    /// A value, in the kit's own text block: it selects, it takes every
    /// key this machine has taught you, and it undoes. One per field,
    /// live — there is nothing to open or close, and Save takes what
    /// they hold.
    fn value(
        &mut self,
        part: &Section,
        key: &str,
        text: String,
        list: bool,
        fallback: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // "one to a line" is said once, under the key, by the hint the
        // schema carries — not again under the box.
        let at = (part.clone(), key.to_owned());
        let block = match self.fields.get(&at) {
            Some(block) => block.clone(),
            None => {
                // A single value is a line. A list is five lines and
                // scrolls: ten was half the pane for two patterns.
                //
                // Rows only size a block that grows, which is why this
                // asks for five of them either way rather than setting
                // `rows`: a plain block takes whatever height the row it
                // sits in happens to have, which is one line.
                let shown = text.clone();
                // An empty field that says only "not set" has not
                // answered the question a person actually has, which is
                // what happens if they leave it alone.
                let empty = match fallback.is_empty() {
                    true => t("config.not_set").to_owned(),
                    false => fill("config.default_is", &[("default", fallback)]),
                };
                let block = cx.new(|cx| {
                    let block = TextareaState::new(window, cx);
                    let block = match list {
                        true => block.auto_grow(5, 5),
                        false => block,
                    };
                    block.default_value(shown).placeholder(empty)
                });
                if list {
                    self.lists.insert(at.clone());
                }
                // Typing is a change like any other: the count at the
                // top and the mark beside the key both follow it, and
                // the loader catches up once the typing stops.
                let typed = at.clone();
                cx.subscribe(&block, move |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.take_one(&typed, cx);
                        cx.notify();
                    }
                })
                .detach();
                self.fields.insert(at, block.clone());
                block
            }
        };
        div()
            .flex()
            .flex_col()
            .gap(step(1.))
            .child(
                div()
                    .max_w(px(620.))
                    .child(Textarea::new(&block).bordered(true)),
            )
            .into_any_element()
    }

    /// What the fields hold that the file does not, written in one go.
    ///
    /// A word or a switch is written as it is chosen, because there is
    /// nothing to finish typing; text is taken here, when a person says
    /// so, which is also what keeps the supervisor from being handed a
    /// path half-typed.
    fn take_the_fields(&mut self, cx: &mut Context<Self>) {
        let every: Vec<(Section, String)> = self.fields.keys().cloned().collect();
        for at in every {
            self.take_one(&at, cx);
        }
    }

    /// What one block holds, into the document — if it says something
    /// the file does not already say.
    fn take_one(&mut self, at: &(Section, String), cx: &mut Context<Self>) {
        let Some(block) = self.fields.get(at).cloned() else {
            return;
        };
        let Some(sheet) = &self.sheet else { return };
        let (section, key) = at;
        let typed = block.read(cx).value().to_string();
        let held = sheet.held(section, key);
        // Whether this is a list is the schema's answer, kept from when
        // the block was built: a one-line list is still a list, and
        // guessing from the text turns it into a string.
        let list = self.lists.contains(at)
            || held
                .as_ref()
                .is_some_and(|item| item.as_array().is_some());
        let value = match list {
            true => {
                let mut array = toml_edit::Array::new();
                for line in typed.lines().map(str::trim).filter(|line| !line.is_empty()) {
                    array.push(line);
                }
                match array.is_empty() {
                    true => None,
                    false => Some(toml_edit::value(array)),
                }
            }
            false => match typed.trim() {
                "" => None,
                text => Some(surface::number_or_text(text)),
            },
        };
        let same = match (&value, &held) {
            (Some(value), Some(held)) => value.to_string() == held.to_string(),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        let at = Spot {
            section: section.clone(),
            key: key.to_string(),
            item: None,
        };
        // Quietly: the loader is too slow to run on every keystroke.
        if let Some(sheet) = &mut self.sheet {
            if let Some(said) = sheet.later(&at, value) {
                self.said = Some(said);
            }
        }
        self.typed_at = Some(Instant::now());
    }

    // ── what the window does to the fleet ────────────────────────────

    fn resolve(&mut self, item: &Conflict, keep: &str) {
        let mut command = std::process::Command::new(surface::exe());
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
        self.said = Some(match command.output() {
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
        let mut command = std::process::Command::new(surface::exe());
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
}

/// What can be asked of a running session.
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

    fn done(self) -> &'static str {
        match self {
            Verb::Flush => t("verb.flushed"),
            Verb::Verify => t("verb.will_verify"),
            Verb::Pause => t("verb.paused"),
            Verb::Resume => t("verb.resumed"),
        }
    }

    fn about(self) -> &'static str {
        match self {
            Verb::Flush => t("tip.flush"),
            Verb::Verify => t("tip.verify"),
            Verb::Pause => t("tip.pause"),
            Verb::Resume => t("tip.resume"),
        }
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
        .when(!word.is_empty(), |chip| {
            chip.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(match lit {
                        true => DIM,
                        false => FAINT,
                    }))
                    .child(word),
            )
        })
}

fn label(text: &'static str) -> Div {
    div()
        .text_size(px(10.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(FAINT))
        .child(text)
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
