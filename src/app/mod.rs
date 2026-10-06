//! The window over the fleet.
//!
//! A dashboard is what the gauges of a road vehicle have been called
//! since the board that stopped mud being dashed up by the horses, and
//! that is what this is: a light per session, the conflicts waiting,
//! the log, the file, and the service that drives it all.
//!
//! It draws through GPUI Kit's components — text that selects, fields
//! that behave like every other field on this machine, dialogs, a
//! theme to hang the palette on — and owns nothing underneath. The
//! fleet comes from `supervisor::status_report`, the actions go through
//! the control socket and the command, the form is generated from
//! `config::schema`, and every line of English comes from
//! `assets/words/en.toml`.

mod ink;

use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{Dialog, DialogButtonProps};
use gpui_kit::component::input::{
    Editor, EditorState, InputEvent, InputHighlighter, Textarea, TextareaState,
};
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::text::{SelectionFormat, TextView};
use gpui_kit::component::{
    Disableable as _, Icon, IconName, IndexPath, Root, Sizable as _, Theme, ThemeMode,
    WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::supervisor::{status_report, GroupReport, SessionReport, StatusReport};
use crate::surface::{
    self, first_sentence, format_age, holds, severity, state_words, thousands, tilde, Conflict,
    Holds, Section, Severity, Sheet, Side, Spot, EXPERIMENTAL, SILENT_AT_THE_TOP,
    SILENT_IN_ADVANCED,
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
// No gray darker than DIM: the quiet text under a field reads at the
// same strength as a group's name in the configuration list.
const FAINT: u32 = DIM;
const GREEN: u32 = 0x3fb97a;
const AMBER: u32 = 0xe0ae42;
const RED: u32 = 0xe8796a;
const BLUE: u32 = 0x6ea8f0;

/// The one spacing unit, in points.
const STEP: f32 = 4.0;

fn step(n: f32) -> Pixels {
    px(STEP * n)
}

fn tint(colour: u32, opacity: u32) -> Rgba {
    rgba((colour << 8) | opacity)
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
    /// Only ever listed when there is no `autobahn` to talk to, which is
    /// the one state where none of the other panes can say anything
    /// true.
    Welcome,
    Groups,
    Hosts,
    Conflicts,
    Log,
    Service,
    Config,
}

impl Pane {
    /// The name over the pane, and in the rail beside its mark.
    ///
    /// Welcome has none, like its entry in `about` below: the splash is
    /// drawn before the header exists, and the rail never lists it, so
    /// a name here would be a string nothing reads.
    fn title(self) -> &'static str {
        match self {
            Pane::Welcome => "",
            Pane::Groups => t("pane.groups"),
            Pane::Hosts => t("pane.hosts"),
            Pane::Conflicts => t("pane.conflicts"),
            Pane::Log => t("pane.log"),
            Pane::Service => t("pane.service"),
            Pane::Config => t("pane.config"),
        }
    }

    /// The mark beside its name. From the kit's bundled Lucide set, so
    /// there is nothing to draw and nothing to ship.
    fn icon(self) -> IconName {
        match self {
            Pane::Welcome => IconName::SquareTerminal,
            Pane::Groups => IconName::Folder,
            Pane::Hosts => IconName::Network,
            Pane::Conflicts => IconName::TriangleAlert,
            Pane::Log => IconName::FileText,
            Pane::Service => IconName::Cpu,
            Pane::Config => IconName::Settings,
        }
    }

    /// What a pane is for, under its name. Groups has none: a list of
    /// every group with what each one last did explains itself, and a
    /// sentence saying so is a sentence in the way.
    fn about(self) -> &'static str {
        match self {
            Pane::Welcome => "",
            Pane::Groups => "",
            Pane::Conflicts => t("pane.conflicts_about"),
            Pane::Config => t("pane.config_about"),
            Pane::Log => t("pane.log_about"),
            Pane::Service => t("pane.service_about"),
            Pane::Hosts => t("pane.hosts_about"),
        }
    }
}

/// Opens a window, on the pane it is asked for.
fn open_window(
    config: Option<PathBuf>,
    state_root: PathBuf,
    pane: Option<String>,
    shown: bool,
    speaks: bool,
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
        // A menu bar application still builds its window; it just does
        // not put it on the screen until the menu asks for it, which
        // is what "Open the window" in that menu is for.
        show: shown,
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
            // `config:defaults` opens the configuration on one of its
            // sections, and `conflicts:diff` opens the first conflict
            // with its difference already read — the only way a picture
            // of either is taken without a hand on the mouse.
            //
            // Split before the window is built, because `AutobahnApp::new`
            // reads the fleet once, and `conflicts:diff` has to be
            // true by then or that first reading passes it by.
            let asked = pane.as_deref().map(|pane| match pane.split_once(':') {
                Some((pane, section)) => (pane.to_owned(), Some(section.to_owned())),
                None => (pane.to_owned(), None),
            });
            let first_diff = asked
                .as_ref()
                .is_some_and(|(_, section)| section.as_deref() == Some("diff"));
            let dash = cx.new(|cx| {
                let mut dash = AutobahnApp::new(config, state_root, speaks, first_diff, cx);
                if let Some((pane, section)) = asked.as_ref() {
                    let (pane, section) = (pane.as_str(), section.as_deref());
                    dash.pane = match pane {
                        "welcome" => Pane::Welcome,
                        "conflicts" => Pane::Conflicts,
                        "config" => Pane::Config,
                        "log" => Pane::Log,
                        "service" => Pane::Service,
                        "hosts" => Pane::Hosts,
                        _ => Pane::Groups,
                    };
                    if let Some(section) = section.filter(|_| !first_diff) {
                        dash.section = match section {
                            "defaults" => Section::Defaults,
                            "experimental" => Section::Advanced,
                            "alerts" => Section::Alerts,
                            "p2p" => Section::P2P,
                            "settings" => Section::Settings,
                            name => Section::Group(name.to_owned()),
                        };
                    }
                    // A window pointed straight at an experimental table
                    // is a window that was let in: the list would deny a
                    // section the pane is already showing otherwise. The
                    // environment says so too, which is how a picture of
                    // one gets taken without a hand on the mouse.
                    dash.unlocked = matches!(
                        dash.section,
                        Section::Advanced | Section::Alerts | Section::P2P
                    ) || std::env::var("AUTOBAHN_DESK_EXPERIMENTAL").is_ok();
                    dash.settle(dash.pane, window, cx);
                }
                dash
            });
            cx.set_global(Desk(dash.downgrade()));
            cx.new(|cx| Root::new(dash, window, cx))
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

/// The window's state, kept by the process so the application's menu
/// can turn the window to a pane. Weak: a closed window takes it along.
struct Desk(WeakEntity<AutobahnApp>);

impl Global for Desk {}

actions!(autobahn, [OpenSettings, QuitApp]);

/// Puts the window in front, opening one when there is none: what the
/// menu bar's "Open the window" asks for, and a click on the dock icon.
fn show_the_window(config: Option<PathBuf>, state_root: PathBuf, cx: &mut App) {
    // Asking for the window is asking to be an application with one,
    // dock icon and all.
    crate::dock::in_the_dock(true);
    cx.activate(true);
    if cx.windows().is_empty() {
        open_window(config, state_root, None, true, false, cx);
    } else {
        for window in cx.windows() {
            window
                .update(cx, |_, window, _| window.activate_window())
                .ok();
        }
    }
}

/// The window on its settings: what Settings… in the application's menu
/// and ⌘, ask for.
fn open_settings(config: Option<PathBuf>, state_root: PathBuf, cx: &mut App) {
    crate::dock::in_the_dock(true);
    cx.activate(true);
    let desk = cx.try_global::<Desk>().and_then(|desk| desk.0.upgrade());
    match (desk, cx.windows().first().copied()) {
        (Some(desk), Some(window)) => {
            window
                .update(cx, |_, window, cx| {
                    window.activate_window();
                    desk.update(cx, |desk, cx| {
                        desk.pane = Pane::Config;
                        desk.section = Section::Settings;
                        desk.settle(Pane::Config, window, cx);
                        cx.notify();
                    });
                })
                .ok();
        }
        _ => {
            let pane = Some("config:settings".to_owned());
            open_window(config, state_root, pane, true, false, cx);
        }
    }
}

/// Keeps the item up to date and answers what is chosen in it.
fn watch_the_bar(config: Option<PathBuf>, state_root: PathBuf, cx: &mut App) {
    cx.spawn(async move |cx| loop {
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
                show_the_window(config.clone(), state_root.clone(), cx);
            }
        });
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
pub struct AutobahnApp {
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
    /// The hint this window opened with. Picked once: a line that
    /// changed under the eye would be read as something happening.
    hint: SharedString,
    /// The form's own scroll, so Save can put the refusal in view.
    form: ScrollHandle,
    /// How much of itself this app shows: a window, a menu bar item,
    /// or both.
    presence: crate::preferences::Presence,
    /// Whether this machine wants the app to raise notifications.
    notify: bool,
    /// Asked for with `--pane conflicts:diff`: select the first
    /// conflict and read its difference, once there is a report to
    /// take one from.
    open_first_diff: bool,
    /// Whether `on_alert` is set, which is what makes the app's own
    /// notifications a second voice saying the same thing. Read on the
    /// poll and not at startup, so a hook added while the app is open
    /// changes the warning without a restart — and not in the render,
    /// which would be a file read every frame.
    hook_set: bool,
    /// Raises them, when no menu bar item is doing it. `None` only if
    /// no alert plan could be built at all, which is a configuration
    /// too broken to say anything useful about anyway.
    notifier: Option<crate::menubar::Notifier>,
    /// The name being typed for a new group, while one is being made.
    naming: Option<Entity<TextareaState>>,
    /// The group the name in that field would rename, when it is a
    /// rename rather than a new one.
    renaming: Option<String>,
    /// Whether the loader's complaints are unfolded. Shut by default:
    /// the line beside Save says there is one, which is all most of
    /// them need to say.
    showing_faults: bool,
    /// The complaints that belong to a field, by the field they belong
    /// to. Worked out once a frame, where the loader's verdict is read.
    at_fields: std::collections::HashMap<(Section, String), Vec<surface::At>>,
    /// The same, for what the loader would take and still remark on.
    at_warned: std::collections::HashMap<(Section, String), Vec<surface::At>>,
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
    /// What kind of thing the footer's message is, which is its colour.
    tone: Tone,
    /// The message last copied from the footer, and when: the button says
    /// Copied for a moment, while that message is the one showing.
    copied: Option<String>,
    copied_at: Option<Instant>,
    /// A group's doctor report, open under its card: the group, the
    /// report, and whether a reset is being asked about, in which case
    /// the report is what to read before saying yes.
    doctor: Option<Doctored>,
    /// The group whose report is being read right now, since reading
    /// scans both sides of every session and takes as long as that takes.
    doctoring: Option<String>,
    /// Whether there is an `autobahn` to talk to. Read once at the
    /// start and again after an install, because it is the answer to
    /// "is this window of any use yet".
    ready: bool,
    /// Which build the command is. Asked when `ready` is, and again
    /// after an update, since asking runs the command.
    command_build: Option<surface::CommandBuild>,
    /// Which build the running supervisor is. Asked while the service
    /// pane is the one being looked at.
    supervisor_build: surface::SupervisorBuild,
    /// Whether an install is running. The button it came from is not
    /// offered twice, and the log is what says how it is going.
    installing: bool,
}

/// Runs the window until it is closed, opening on `pane` when one was
/// asked for — the gallery opens straight onto the view being worked on.
pub fn run(config: Option<PathBuf>, state_root: PathBuf, pane: Option<String>) -> Result<()> {
    run_with(config, state_root, pane, None)
}

/// The same window, photographed into `directory` and closed again.
pub fn shoot(
    config: Option<PathBuf>,
    state_root: PathBuf,
    directory: PathBuf,
    pane: Option<String>,
) -> Result<()> {
    run_with(config, state_root, pane, Some(directory))
}

/// A menu bar alone is useful only if its item appeared. Decide that
/// before opening the window, so a failed bar still leaves a way in.
fn run_with(
    config: Option<PathBuf>,
    state_root: PathBuf,
    pane: Option<String>,
    shot: Option<PathBuf>,
) -> Result<()> {
    // The icons are files the kit embeds, so the application has to be
    // told where its assets come from or every one of them draws as
    // nothing.
    let application = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    // A click on the dock icon while no window is showing: the window
    // was closed and the menu bar item kept the application alive, and
    // the click used to do nothing at all.
    {
        let config = config.clone();
        let state_root = state_root.clone();
        application.on_reopen(move |cx| {
            show_the_window(config.clone(), state_root.clone(), cx);
        });
    }
    application.run(move |cx: &mut App| {
        gpui_kit::init(cx);
        cx.activate(true);
        let config = config.clone();
        let state_root = state_root.clone();
        // The application's own menu: where macOS keeps Settings…
        // and Quit, and where ⌘, and ⌘Q are looked for.
        cx.bind_keys([
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("cmd-q", QuitApp, None),
        ]);
        cx.on_action(|_: &QuitApp, cx| cx.quit());
        cx.on_action({
            let config = config.clone();
            let state_root = state_root.clone();
            move |_: &OpenSettings, cx| {
                open_settings(config.clone(), state_root.clone(), cx);
            }
        });
        cx.set_menus(vec![Menu {
            name: t("app.window").into(),
            items: vec![
                MenuItem::action(t("app.menu_settings"), OpenSettings),
                MenuItem::separator(),
                MenuItem::action(t("app.menu_quit"), QuitApp),
            ],
            disabled: false,
        }]);
        let pane = pane.clone();
        let wanted = pane.clone();
        // What this machine asked for: a window, a menu bar item, or
        // both. A screenshot always wants the window, whatever the
        // file says.
        let settings = match shot.is_some() {
            true => crate::preferences::Settings::default(),
            false => crate::preferences::read(&state_root),
        };
        let presence = settings.presence;
        let wants_bar = shot.is_none() && presence.takes_the_menu_bar();
        let bar = if wants_bar {
            // The same item in the menu bar the other window puts
            // there, from the same code: one poll, one notifier, and
            // "Open the window" when this one has been closed.
            match start_the_bar(config.clone(), state_root.clone()) {
                Ok(bar) => Some(bar),
                Err(error) => {
                    eprintln!(
                        "{}",
                        fill("status.no_menu_bar", &[("error", &format!("{error:#}"))])
                    );
                    None
                }
            }
        } else {
            None
        };
        let shown = presence.opens_a_window() || (wants_bar && bar.is_none());
        crate::dock::in_the_dock(shown);
        // The bar notifies when there is one. When there is not —
        // a window-only presence, or a bar that would not start —
        // the window does it instead, so the choice of where the
        // app appears never decides whether anything tells you.
        let speaks = settings.notify && bar.is_none();
        let window = open_window(
            config.clone(),
            state_root.clone(),
            wanted,
            shown,
            speaks,
            cx,
        );
        // A login service left running the wrong program is put right
        // once, here, and the window says so. Not for a picture, and not
        // when the command or the service is being stood in for: then
        // what is registered on this machine is not this run's to touch.
        let stood_in = [surface::TOLD_WHERE, crate::service::TOLD_STATE]
            .iter()
            .any(|name| std::env::var_os(name).is_some());
        if shot.is_none() && !stood_in {
            if let Some(said) = surface::mend_service() {
                if let Some(desk) = cx.try_global::<Desk>().and_then(|desk| desk.0.upgrade()) {
                    desk.update(cx, |desk, cx| {
                        desk.say_result(said);
                        cx.notify();
                    });
                }
            }
        }
        if let Some(bar) = bar {
            cx.set_global(Menubar(bar));
            watch_the_bar(config.clone(), state_root.clone(), cx);
        }
        let Some(directory) = shot.clone() else {
            return;
        };
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

/// The window can run without the menu bar, even when a backend panics
/// instead of returning an error. Keep the whole attempt inside the
/// fence: `appear` reaches the native toolkit after `start` has returned.
/// Nothing from a failed attempt is kept for the window to poll, and
/// the usual panic hook still reports where it failed.
fn start_the_bar(config: Option<PathBuf>, state_root: PathBuf) -> Result<crate::menubar::Bar> {
    // `catch_unwind` catches the panic but does not stop the hook that
    // runs first, so a caught one still prints its message and whatever
    // backtrace is asked for — the frightening half of #12, followed by
    // a calm sentence saying it was handled. The hook is silenced for
    // the length of the call and put back after, and what the panic said
    // is recovered from the payload instead.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let started = std::panic::catch_unwind(|| {
        let mut bar = crate::menubar::Bar::start(config, state_root, || {})?;
        bar.window = true;
        bar.appear()?;
        Ok(bar)
    });
    std::panic::set_hook(hook);
    started.unwrap_or_else(|panic| Err(anyhow::anyhow!(said_by_a_panic(panic))))
}

/// What a panic said, when it said anything. A panic can carry any
/// `Send` payload, and the two that reach here in practice are a
/// `&'static str` from `assert!` and a `String` somebody built; a
/// panic with nothing to say still deserves a line of its own.
fn said_by_a_panic(panic: Box<dyn std::any::Any + Send>) -> String {
    match panic.downcast_ref::<&'static str>() {
        Some(said) => (*said).to_owned(),
        None => match panic.downcast_ref::<String>() {
            Some(said) => said.clone(),
            None => t("status.menu_bar_panicked").to_owned(),
        },
    }
}

impl AutobahnApp {
    fn new(
        config: Option<PathBuf>,
        state_root: PathBuf,
        speaks: bool,
        open_first_diff: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let names = cx.text_system().all_font_names();
        let mono = ["SF Mono", "Menlo", "Monaco"]
            .into_iter()
            .find(|name| names.iter().any(|known| known == name))
            .unwrap_or("Menlo");
        let settings = crate::preferences::read(&state_root);
        let presence = settings.presence;
        // The window's own notifier, used only when no bar took the
        // job. Its plan comes from the configuration, so it holds
        // exactly the timing the hook would; a configuration that will
        // not load yet gets the built-in plan, which is the same thing
        // minus the hook.
        let plan = config
            .as_deref()
            .map(std::path::Path::to_path_buf)
            .or_else(|| crate::paths::default_config_path().ok())
            .and_then(|path| {
                crate::config::Config::load(&path)
                    .and_then(|config| config.alert_plan())
                    .ok()
            })
            .or_else(|| crate::config::Config::default().alert_plan().ok());
        let notifier = plan.map(|plan| {
            let mut notifier = crate::menubar::Notifier::new(plan, state_root.clone());
            notifier.wanted(speaks);
            notifier
        });
        // Every other pane shells out to `autobahn` for what it shows.
        // With no command to shell out to they are all empty in the same
        // uninformative way, so the window opens on the one pane that can
        // explain why.
        let ready = surface::installed();
        let mut dash = AutobahnApp {
            config,
            mono: SharedString::from(mono.to_owned()),
            state_root,
            pane: match ready {
                true => Pane::Groups,
                false => Pane::Welcome,
            },
            ready,
            installing: false,
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
            hint: SharedString::from(crate::words::hint()),
            form: ScrollHandle::new(),
            presence,
            notify: settings.notify,
            open_first_diff,
            hook_set: false,
            notifier,
            naming: None,
            renaming: None,
            showing_faults: false,
            at_fields: std::collections::HashMap::new(),
            at_warned: std::collections::HashMap::new(),
            unlocked: false,
            knocks: 0,
            knocked_at: None,
            shape: crate::config::schema(),
            said: None,
            tone: Tone::Plain,
            copied: None,
            copied_at: None,
            doctor: None,
            doctoring: None,
            command_build: surface::command_build(),
            supervisor_build: surface::SupervisorBuild::Absent,
        };
        dash.refresh();
        cx.spawn(async move |this, cx| {
            loop {
                // The kit's GPUI hands out its timers through the
                // executor rather than as a free type.
                let sleep = cx.background_executor().timer(POLL_WHILE_WORKING);
                sleep.await;
                let carried = this.update(cx, |this, cx| {
                    // A tailing log asks for a frame of its own: the reading
                    // itself needs a window, and only `render` has one.
                    if this.installing
                        || this.refresh_if_due()
                        || (this.log_tail && this.pane == Pane::Log)
                    {
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
        dash
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

    /// Puts a line in the footer, coloured by what kind of line it is.
    fn say(&mut self, said: impl Into<String>, tone: Tone) {
        self.said = Some(said.into());
        self.tone = tone;
    }

    /// Puts an outcome in the footer: what worked in green, what did not
    /// in red.
    fn say_result(&mut self, outcome: Result<String, String>) {
        match outcome {
            Ok(said) => self.say(said, Tone::Done),
            Err(said) => self.say(said, Tone::Trouble),
        }
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
        self.say(
            match self.unlocked {
                true => t("status.unlocked"),
                false => t("status.locked"),
            },
            Tone::Plain,
        );
    }

    fn refresh(&mut self) {
        self.read_at = Some(Instant::now());
        if self.pane == Pane::Service {
            self.supervisor_build = surface::supervisor_build(&self.state_root);
        }
        let path = match &self.config {
            Some(path) => path.clone(),
            None => match crate::paths::default_config_path() {
                Ok(path) => path,
                Err(_) => return,
            },
        };
        self.hook_set = crate::config::Config::load(&path)
            .map(|config| config.on_alert.is_some())
            .unwrap_or(false);
        match crate::supervisor::shown_plans(&path, &self.state_root) {
            Ok(shown) => {
                let selected: Vec<&crate::config::SessionPlan> = shown.plans.iter().collect();
                self.report = Some(status_report(&selected, &self.state_root));
            }
            Err(error) => {
                // One line, and the first fault rather than every
                // group's copy of it: the status bar has one line.
                let said = format!("{error:#}");
                let first = surface::faults(&said)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| surface::first_line(&said));
                self.say(
                    fill("status.config_refused", &[("error", &first)]),
                    Tone::Trouble,
                );
            }
        }
        // The dock icon carries what needs a person, so a glance at it
        // answers the question the window was opened to answer.
        crate::dock::badge(self.waiting());
        // The first report is the first chance to pick a conflict.
        if self.open_first_diff {
            if let Some(first) = self.waiting_list().into_iter().find(|item| !item.blocked) {
                self.open_conflict(first.clone());
                self.read_diff(&first);
                self.open_first_diff = false;
            }
        }
        // And the notification, when no menu bar item is raising it.
        // The same report, the same rules; the notifier was told at
        // startup whether it is the one speaking.
        if let (Some(notifier), Some(report)) = (self.notifier.as_mut(), self.report.as_ref()) {
            notifier.observe(report);
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

impl Render for AutobahnApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = self.pane;
        let room = Room::of(window);
        self.room = room;
        // Nothing else in this window can say anything true without a
        // command to ask, so nothing else is drawn.
        if !self.ready || pane == Pane::Welcome {
            return div().size_full().child(self.splash(cx));
        }
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
                        Pane::Welcome => self.splash(cx),
                        Pane::Groups => self.groups(cx),
                        Pane::Conflicts => self.conflicts(cx),
                        Pane::Config => self.config_pane(window, cx),
                        Pane::Log => self.log_pane(window, cx),
                        Pane::Service => self.service_pane(cx),
                        Pane::Hosts => self.hosts(cx),
                    })
                    .child(self.footer(cx)),
            )
    }
}

impl AutobahnApp {
    fn rail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let waiting = self.waiting();
        let running = self.report.as_ref().map(|report| report.supervisor_running);
        // What the service manager says, which is not the same question
        // as whether a supervisor is answering: one can be registered
        // and stopped, or running from a terminal with none registered.
        let registered = surface::service_state();
        let (service, colour) = match running {
            Some(true) => (t("fleet.supervisor_running"), GREEN),
            Some(false) => match registered {
                Some(crate::service::ServiceState::NotInstalled) => {
                    (t("service.not_installed"), RED)
                }
                _ => (t("fleet.supervisor_missing"), RED),
            },
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
                    .child(self.nav(Pane::Service, None, cx))
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
                    // What can be done about any of it is the Service
                    // pane. A rail that held the fix as well as the
                    // complaint held an app's settings in a corner
                    // meant for the fleet's facts.
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(rgb(FAINT))
                            .truncate()
                            // Shortened, like every other path the app
                            // draws: a home directory spelled out takes
                            // the width the interesting half needs.
                            .child(tilde(&self.state_root.display().to_string())),
                    ),
            )
            .into_any_element()
    }

    /// Chooses how much of itself the app shows, now and at next launch.
    ///
    /// The dock follows at once, because that is the half a person can
    /// see happen. The menu bar item does not: taking one away and
    /// putting it back mid-session is more moving parts than the
    /// setting is worth, so it settles at the next launch and the
    /// status line says so.
    fn show_as(&mut self, presence: crate::preferences::Presence) {
        self.presence = presence;
        crate::dock::in_the_dock(presence.opens_a_window());
        match self.keep_settings() {
            Some(error) => self.say(
                fill("presence.unwritable", &[("error", &error)]),
                Tone::Trouble,
            ),
            None => self.say(t("presence.at_next_launch"), Tone::Plain),
        }
    }

    /// Turns the app's own notifications on or off.
    ///
    /// Takes effect at once, unlike the presence beside it: whichever
    /// notifier is running is told, and there is nothing to rebuild.
    fn notify_as(&mut self, wanted: bool) {
        self.notify = wanted;
        if let Some(notifier) = self.notifier.as_mut() {
            notifier.wanted(wanted);
        }
        match self.keep_settings() {
            Some(error) => self.say(
                fill("presence.unwritable", &[("error", &error)]),
                Tone::Trouble,
            ),
            None => self.say(
                match wanted {
                    true => t("presence.notify_on"),
                    false => t("presence.notify_off"),
                },
                Tone::Done,
            ),
        }
    }

    /// Writes both of this machine's choices, which share one file.
    fn keep_settings(&self) -> Option<String> {
        crate::preferences::write(
            &self.state_root,
            crate::preferences::Settings {
                presence: self.presence,
                notify: self.notify,
            },
        )
    }

    /// Asks the service manager for something, and says what came back.
    fn order(&mut self, order: surface::Order) {
        self.say_result(surface::ask(
            order,
            self.config.as_deref(),
            &self.state_root,
        ));
        // Whatever it did, the fleet is a different shape now.
        self.read_at = None;
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
            .child(self.nav(Pane::Service, None, cx))
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
                // A new pane is a new look at the window, which is the
                // moment a second hint is worth reading. Never the one
                // already there: a line that was meant to change and
                // did not reads as a click that did not land.
                this.hint = SharedString::from(crate::words::hint_besides(&this.hint));
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
                    .child(div().text_size(px(11.)).text_color(rgb(FAINT)).child(
                        match self.report {
                            None => self.pane.about().to_owned(),
                            // A pane with nothing to say about itself
                            // takes the shorter line, rather than the
                            // longer one with a separator hanging off
                            // the end of it.
                            Some(_) => fill(
                                match self.pane.about().is_empty() {
                                    true => "pane.counted_bare",
                                    false => "pane.counted",
                                },
                                &[
                                    ("groups", &counted("pane.group", groups, &[])),
                                    ("sessions", &counted("pane.session", sessions, &[])),
                                    ("about", self.pane.about()),
                                ],
                            ),
                        },
                    )),
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

    fn footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let age = self
            .read_at
            .map(|at| format_age(at.elapsed().as_secs()))
            .unwrap_or_else(|| t("fleet.never").to_owned());
        // Whether the message showing was copied a moment ago, which is
        // what the button says for that moment.
        const COPIED_FOR: Duration = Duration::from_secs(2);
        let copied = self.said.is_some()
            && self.copied == self.said
            && self.copied_at.is_some_and(|at| at.elapsed() < COPIED_FOR);
        let colour = match self.tone {
            Tone::Plain => DIM,
            Tone::Done => GREEN,
            Tone::Trouble => RED,
        };
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
                            .text_color(rgb(colour))
                            .truncate()
                            .child(crate::text::display_safe(said).to_string()),
                        // Nothing to say is a place to teach something:
                        // one of the hints, chosen when the window
                        // opened, rather than the same sentence for ever.
                        None => div()
                            .text_color(rgb(FAINT))
                            .truncate()
                            .child(match self.room {
                                Room::Tight => tilde(&self.state_root.display().to_string()),
                                _ => self.hint.to_string(),
                            }),
                    }),
            )
            // A message is something to keep: an error to paste into a
            // report, a path to paste into a terminal. It is cut short to
            // fit the line, so the button takes all of it. A hint is not
            // worth keeping, and beside one the line says how fresh the
            // window is instead.
            .child(match self.said.is_some() {
                true => div().flex_shrink_0().pl(step(4.)).child(
                    Button::new("copy-said")
                        .xsmall()
                        .outline()
                        .label(match copied {
                            true => t("status.copied"),
                            false => t("status.copy"),
                        })
                        .tooltip(t("tip.copy_said"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(said) = this.said.clone() {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    surface::for_the_clipboard(&said),
                                ));
                                this.copied = Some(said);
                                this.copied_at = Some(Instant::now());
                                cx.notify();
                                // A frame when the moment is over, so the
                                // label goes back without waiting for
                                // the next poll.
                                cx.spawn(async move |this, cx| {
                                    cx.background_executor().timer(COPIED_FOR).await;
                                    this.update(cx, |_, cx| cx.notify()).ok();
                                })
                                .detach();
                            }
                        })),
                ),
                false => div()
                    .flex_shrink_0()
                    .pl(step(4.))
                    .text_color(rgb(FAINT))
                    .child(fill("fleet.read_ago", &[("age", &age)])),
            })
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
                        head.child(pill(format!("{} · term {}", group.role, group.term), BLUE))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_family(self.mono.clone())
                            .text_size(px(11.))
                            .text_color(rgb(FAINT))
                            .truncate()
                            .child(tilde(&group.primary)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(rgb(colour))
                            .child(summary),
                    )
                    .child(self.group_buttons(&group.name, cx)),
            )
            .when_some(
                self.doctor
                    .clone()
                    .filter(|doctored| doctored.group == group.name),
                |band, doctored| band.child(self.doctor_panel(&doctored, cx)),
            )
            .children(rows)
            .into_any_element()
    }

    /// What can be asked of a whole group, on its card. The safe ones are
    /// plain buttons; the two that change the configuration or bring
    /// deleted files back are quieter, and Reset asks before it acts.
    fn group_buttons(&self, name: &str, cx: &mut Context<Self>) -> Div {
        let reading = self.doctoring.as_deref() == Some(name);
        let button = |id: &str, label: &'static str, tip: &'static str| {
            Button::new(SharedString::from(format!("group-{id}-{name}")))
                .xsmall()
                .outline()
                .label(label)
                .tooltip(tip)
        };
        let (doctor, flush, verify, reset, disable) = (
            name.to_owned(),
            name.to_owned(),
            name.to_owned(),
            name.to_owned(),
            name.to_owned(),
        );
        div()
            .flex_shrink_0()
            .flex()
            .gap(step(1.5))
            .child(
                button("doctor", t("group.doctor"), t("tip.group_doctor"))
                    .disabled(reading)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.read_doctor(&doctor, false, cx);
                        cx.notify();
                    })),
            )
            .child(
                button("flush", t("verb.flush"), t("tip.group_flush")).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.group_control(&flush, Verb::Flush);
                        cx.notify();
                    },
                )),
            )
            .child(
                button("verify", t("verb.verify"), t("tip.group_verify")).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.group_control(&verify, Verb::Verify);
                        cx.notify();
                    },
                )),
            )
            .child(
                Button::new(SharedString::from(format!("group-reset-{name}")))
                    .xsmall()
                    .ghost()
                    .label(t("group.reset"))
                    .tooltip(t("tip.group_reset"))
                    .disabled(reading)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.read_doctor(&reset, true, cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("group-disable-{name}")))
                    .xsmall()
                    .ghost()
                    .label(t("group.disable"))
                    .tooltip(t("tip.group_disable"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.disable_group(&disable);
                        cx.notify();
                    })),
            )
    }

    /// A group's doctor report, under its card. Asked about a reset, it
    /// is the thing to read first, and the Merge button sits with it.
    fn doctor_panel(&self, doctored: &Doctored, cx: &mut Context<Self>) -> Div {
        let name = doctored.group.clone();
        let title = match doctored.offer_reset {
            true => fill("group.reset_title", &[("name", &name)]),
            false => fill("group.doctor_title", &[("name", &name)]),
        };
        let report = doctored.report.clone();
        let merging = name.clone();
        div()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(rgb(HAIR))
            .bg(rgb(RAISED))
            .child(
                div()
                    .px(step(4.))
                    .py(step(2.))
                    .flex()
                    .items_center()
                    .gap(step(1.5))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(11.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(match doctored.offer_reset {
                                true => AMBER,
                                false => DIM,
                            }))
                            .truncate()
                            .child(title),
                    )
                    .when(doctored.offer_reset, |row| {
                        row.child(
                            Button::new(SharedString::from(format!("merge-{name}")))
                                .xsmall()
                                .primary()
                                .label(t("group.merge"))
                                .tooltip(t("tip.merge"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.reset_group(&merging);
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("copy-doctor-{name}")))
                            .xsmall()
                            .outline()
                            .label(t("status.copy"))
                            .tooltip(t("tip.copy_doctor"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(doctored) = &this.doctor {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        surface::for_the_clipboard(&doctored.report),
                                    ));
                                    this.say(t("group.doctor_copied"), Tone::Done);
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("close-doctor-{name}")))
                            .xsmall()
                            .ghost()
                            .label(t("group.close"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.doctor = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("doctor-{name}")))
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .px(step(4.))
                    .pb(step(3.))
                    .font_family(self.mono.clone())
                    .child(said(
                        SharedString::from(format!("doctor-text-{name}")),
                        &crate::text::display_safe(&report),
                        DIM,
                        11.,
                    )),
            )
    }

    /// Reads a group's doctor report off the main thread: it scans both
    /// sides of every session, which on a large tree takes a while. With
    /// `offer_reset`, the report opens as the question a reset asks.
    fn read_doctor(&mut self, name: &str, offer_reset: bool, cx: &mut Context<Self>) {
        if self.doctoring.is_some() {
            return;
        }
        self.doctoring = Some(name.to_owned());
        self.say(fill("group.doctoring", &[("name", name)]), Tone::Plain);
        let group = name.to_owned();
        let config = self.config.clone();
        let state_root = self.state_root.clone();
        cx.spawn(async move |this, cx| {
            let asked = group.clone();
            let done = cx
                .background_executor()
                .spawn(async move {
                    surface::report(&["doctor", &asked], config.as_deref(), &state_root)
                })
                .await;
            this.update(cx, |this, cx| {
                this.doctoring = None;
                let (report, tone) = match done {
                    Ok(report) => (report, Tone::Plain),
                    Err(report) => (report, Tone::Trouble),
                };
                this.doctor = Some(Doctored {
                    group: group.clone(),
                    report,
                    offer_reset,
                });
                this.say(fill("group.doctored", &[("name", &group)]), tone);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A control request for every session of a group.
    fn group_control(&mut self, name: &str, verb: Verb) {
        let selector = crate::supervisor::control::Selector {
            group: Some(name.to_owned()),
            host: None,
            session: None,
        };
        let request = match verb {
            Verb::Flush => crate::supervisor::control::ControlRequest::Flush(selector),
            Verb::Verify => crate::supervisor::control::ControlRequest::Verify(selector),
            Verb::Pause => crate::supervisor::control::ControlRequest::Pause(selector),
            Verb::Resume => crate::supervisor::control::ControlRequest::Resume(selector),
        };
        match crate::supervisor::control::send(&self.state_root, &request) {
            Ok(_) => self.say(
                fill("group.controlled", &[("done", verb.done()), ("name", name)]),
                Tone::Done,
            ),
            Err(error) => self.say(format!("{error:#}"), Tone::Trouble),
        }
        self.read_at = None;
    }

    /// Discards what a group's sessions last agreed on, as `autobahn
    /// reset <group>` does: the next cycle merges both sides and only
    /// adds, so deleted files come back. Only ever reached through the
    /// report that says what that means for this group.
    fn reset_group(&mut self, name: &str) {
        let request = crate::supervisor::control::ControlRequest::Reset(
            crate::supervisor::control::Selector {
                group: Some(name.to_owned()),
                host: None,
                session: None,
            },
        );
        match crate::supervisor::control::send(&self.state_root, &request) {
            Ok(_) => self.say(fill("group.reset_done", &[("name", name)]), Tone::Done),
            Err(error) => self.say(format!("{error:#}"), Tone::Trouble),
        }
        self.doctor = None;
        self.read_at = None;
    }

    /// Turns a group off in the configuration, as `autobahn disable
    /// --group` does. Its state is kept; the Configuration page is where
    /// it is turned back on, since a group that is off leaves this page.
    fn disable_group(&mut self, name: &str) {
        self.say_result(surface::ran(
            &["disable", "--group", name],
            self.config.as_deref(),
        ));
        self.read_at = None;
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
                        group.name, session.replica
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
                            .child(tilde(&crate::text::display_safe(&session.replica))),
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
                band.child(self.aside(
                    format!("session-error-{}-{}", group.name, session.replica),
                    &crate::text::display_safe(error),
                    RED,
                ))
            })
            // A halt names doctor and reset; here they are, beside it.
            .when(session.state == "halted", |band| {
                let (doctor, reset) = (group.name.clone(), group.name.clone());
                let reading = self.doctoring.as_deref() == Some(group.name.as_str());
                band.child(
                    div()
                        .pl(step(8.))
                        .pb(step(2.5))
                        .flex()
                        .gap(step(1.5))
                        .child(
                            Button::new(SharedString::from(format!(
                                "halted-doctor-{}-{}",
                                group.name, session.replica
                            )))
                            .xsmall()
                            .outline()
                            .label(t("group.doctor"))
                            .tooltip(t("tip.group_doctor"))
                            .disabled(reading)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.read_doctor(&doctor, false, cx);
                                    cx.notify();
                                },
                            )),
                        )
                        .child(
                            Button::new(SharedString::from(format!(
                                "halted-reset-{}-{}",
                                group.name, session.replica
                            )))
                            .xsmall()
                            .ghost()
                            .label(t("group.reset"))
                            .tooltip(t("tip.group_reset"))
                            .disabled(reading)
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.read_doctor(&reset, true, cx);
                                    cx.notify();
                                },
                            )),
                        ),
                )
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
                                .child(said(
                                    format!("waiting-{}-{}-{path}", group.name, session.replica),
                                    &path,
                                    DIM,
                                    11.,
                                )),
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

impl AutobahnApp {
    /// What a pane needs read before it is looked at.
    fn settle(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        match pane {
            Pane::Conflicts => {
                if self.conflict.is_none() {
                    // A conflict first — those have something to do
                    // about them — but an empty pane beside a list of
                    // one is worse than opening the one.
                    let waiting = self.waiting_list();
                    let first = waiting
                        .iter()
                        .find(|it| !it.blocked)
                        .or_else(|| waiting.first())
                        .cloned();
                    if let Some(first) = first {
                        self.open_conflict(first);
                    }
                }
            }
            Pane::Log if self.log.is_none() => self.read_log(window, cx),
            Pane::Service => {
                self.supervisor_build = surface::supervisor_build(&self.state_root);
            }
            Pane::Config if self.sheet.is_none() => self.read_sheet(),
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
                        primary_root: group.primary.clone(),
                        replica_root: session.replica.clone(),
                    });
                }
                for blocked in &session.blocked {
                    waiting.push(Conflict {
                        group: group.name.clone(),
                        host: session.host.clone(),
                        path: blocked.clone(),
                        blocked: true,
                        primary_root: group.primary.clone(),
                        replica_root: session.replica.clone(),
                    });
                }
            }
        }
        waiting
    }

    fn open_conflict(&mut self, item: Conflict) {
        self.sides = Some((
            surface::inspect("primary", &item.primary_root, &item.path),
            surface::inspect("replica", &item.replica_root, &item.path),
        ));
        self.conflict = Some(item);
        self.diff = None;
    }

    /// A line hanging under a session row, indented past its dot.
    /// What a session is complaining about, under the row that says so.
    ///
    /// Selectable, like every other complaint: this is the sentence a
    /// person takes to a search engine or a bug report, and retyping
    /// "unsolicited response on channel 9" is nobody's idea of a
    /// morning.
    fn aside(&self, id: impl Into<SharedString>, text: &str, colour: u32) -> Div {
        div()
            .pl(step(8.))
            .pr(step(4.))
            .pb(step(2.))
            .font_family(self.mono.clone())
            .child(said(id, text, colour, 11.))
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
            .child(self.pair(t("fleet.primary"), tilde(&group.primary)))
            .child(self.pair(t("fleet.replica"), tilde(&session.replica)))
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
        let replica = session.replica.clone();
        let key = session.session.clone();
        Button::new(SharedString::from(format!(
            "verb-{name}-{replica}-{}",
            verb.word()
        )))
        .small()
        .outline()
        .label(verb.word())
        .tooltip(verb.about())
        .on_click(cx.listener(move |this, _, _, cx| {
            this.control(&name, &replica, &key, verb);
            cx.notify();
        }))
        .into_any_element()
    }

    /// A control request, for the one session the row belongs to.
    fn control(
        &mut self,
        group: &str,
        replica: &str,
        session: &crate::supervisor::control::SessionKey,
        verb: Verb,
    ) {
        let selector = crate::supervisor::control::Selector {
            group: Some(group.to_owned()),
            host: Some(replica.to_owned()),
            session: Some(session.clone()),
        };
        let request = match verb {
            Verb::Flush => crate::supervisor::control::ControlRequest::Flush(selector),
            Verb::Verify => crate::supervisor::control::ControlRequest::Verify(selector),
            Verb::Pause => crate::supervisor::control::ControlRequest::Pause(selector),
            Verb::Resume => crate::supervisor::control::ControlRequest::Resume(selector),
        };
        match crate::supervisor::control::send(&self.state_root, &request) {
            Ok(_) => self.say(
                fill(
                    "status.control_done",
                    &[("done", verb.done()), ("replica", replica)],
                ),
                Tone::Done,
            ),
            Err(error) => self.say(format!("{error:#}"), Tone::Trouble),
        }
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
            .when(!showing_detail, |pane| {
                pane.child(
                    div()
                        .id("queue")
                        .when(room > Room::Tight, |queue| {
                            queue.w(px(352.)).flex_shrink_0()
                        })
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
                                                .child(
                                                    crate::text::display_safe(&name).to_string(),
                                                ),
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
                )
            })
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
            .is_some_and(|(primary, replica)| primary.binary || replica.binary);
        let keep_host = match item.host.contains(':') {
            true => surface::short_name(&item.host),
            false => "replica".to_owned(),
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
                    // The path is what a person takes to a shell, so it
                    // selects like the complaints in the form do.
                    .child(div().font_family(self.mono.clone()).child(said(
                        "conflict-path",
                        &crate::text::display_safe(&item.path),
                        INK,
                        13.,
                    )))
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
                        let primary = item.clone();
                        let host = item.clone();
                        let both = item.clone();
                        let shown = item.clone();
                        head.child(
                            div()
                                .pt(step(1.))
                                .flex()
                                .gap(step(1.5))
                                .child(
                                    Button::new("keep-primary")
                                        .small()
                                        .outline()
                                        .label(t("conflicts.keep_primary"))
                                        .tooltip(t("tip.keep_primary"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.resolve(&primary, "primary");
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("keep-replica")
                                        .small()
                                        .outline()
                                        .label(fill(
                                            "conflicts.keep_replica",
                                            &[("name", &keep_host)],
                                        ))
                                        .tooltip(t("tip.keep_replica"))
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
                                })
                                // The diff wraps to fit and is read in
                                // place; the button is how it leaves.
                                .when(!binary && self.diff.is_some(), |row| {
                                    row.child(
                                        Button::new("copy-diff")
                                            .small()
                                            .outline()
                                            .label(t("conflicts.copy_diff"))
                                            .tooltip(t("tip.copy_diff"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                if let Some(diff) = this.diff.clone() {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(
                                                            surface::for_the_clipboard(&diff),
                                                        ),
                                                    );
                                                    this.say(
                                                        t("conflicts.diff_copied"),
                                                        Tone::Done,
                                                    );
                                                    cx.notify();
                                                }
                                            })),
                                    )
                                }),
                        )
                    }),
            )
            .when_some(sides.filter(|_| binary), |column, (primary, replica)| {
                column.child(self.binary_card(&item, &primary, &replica, cx))
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
                                .child(crate::text::display_safe(line).to_string())
                        })),
                )
            })
            .into_any_element()
    }

    fn binary_card(
        &self,
        _item: &Conflict,
        primary: &Side,
        replica: &Side,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let newer = match (primary.modified, replica.modified) {
            (Some(a), Some(b)) if a > b => Some("primary"),
            (Some(a), Some(b)) if b > a => Some("replica"),
            _ => None,
        };
        // Two sides that hash the same are not "one file in two places":
        // reconciliation compares the mode too, so identical content and
        // mode never reaches this pane. Say which of the two remaining
        // cases it is.
        let agreed = surface::agreement(primary, replica);
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
                    .child(self.side_card(primary, newer == Some("primary"), cx))
                    .child(self.side_card(replica, newer == Some("replica"), cx)),
            )
            .when_some(agreed, |card, agreed| {
                // Settled is good news and reads green. A mode that
                // differs is the conflict itself, not a reassurance.
                let (colour, words) = match agreed {
                    surface::Agreement::Settled => (GREEN, t("conflicts.settled").to_owned()),
                    surface::Agreement::OnlyTheMode { executable } => (
                        AMBER,
                        fill("conflicts.only_the_mode", &[("name", executable)]),
                    ),
                };
                card.child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap(step(2.))
                        .max_w(px(680.))
                        .child(div().flex_shrink_0().pt(px(4.)).child(dot(colour)))
                        .child(
                            div()
                                .min_w(px(0.))
                                .text_size(px(11.))
                                .text_color(rgb(colour))
                                .child(words),
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
                    .when(newer, |head| {
                        head.child(pill(t("conflicts.written_last"), BLUE))
                    })
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
            .child(
                self.pair(
                    t("conflicts.size"),
                    side.size
                        .map(surface::human_size)
                        .unwrap_or_else(|| t("conflicts.unknown").to_owned()),
                ),
            )
            .child(
                self.pair(
                    t("conflicts.written"),
                    side.modified
                        .map(|at| crate::logging::stamp(at as libc::time_t))
                        .unwrap_or_else(|| t("conflicts.unknown").to_owned()),
                ),
            )
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
                            .label(surface::reveal_label())
                            .tooltip(t("tip.reveal"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.reveal(&file);
                                cx.notify();
                            })),
                    ),
                )
            })
    }

    /// A named root of the open session. The name is a label; the root
    /// is a path, and a path is for copying, so it selects.
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
                    .child(said(format!("root-{name}"), &value, INK, 12.)),
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
                    .map(|error| crate::text::display_safe(&surface::cause(&error)).to_string())
                    .unwrap_or(state);
                div()
                    .h(step(9.))
                    .px(step(4.))
                    .flex()
                    .items_center()
                    .gap(step(3.))
                    .when(index > 0, |row| row.border_t_1().border_color(rgb(HAIR)))
                    .child(dot(colour_of(worst)))
                    // The host takes what a host name needs; the message
                    // takes the rest, since it is what someone came to read.
                    .child(
                        div()
                            .w(match room {
                                Room::Wide => px(280.),
                                _ => px(200.),
                            })
                            .flex_shrink_0()
                            .font_family(self.mono.clone())
                            .text_size(px(12.5))
                            .truncate()
                            .child(tilde(&host)),
                    )
                    .when(room > Room::Tight, |row| {
                        row.child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
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
                                    .enumerate()
                                    .map(|(nth, line)| {
                                        div().child(said(format!("bundle-{nth}"), line, DIM, 11.))
                                    })
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
                            .child(said(
                                "own-build",
                                &fill("hosts.build", &[("version", &crate::protocol::version())]),
                                FAINT,
                                11.,
                            )),
                    ),
            )
            .into_any_element()
    }

    // ── the service ──────────────────────────────────────────────────

    /// The supervisor on this machine, and the app around it.
    ///
    /// Everything here is about this installation rather than the
    /// fleet: whether a supervisor is running now, whether one comes
    /// back at the next login, how much of itself the app shows, and
    /// which build it is. The other panes are about the groups.
    fn service_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let registered = surface::service_state();
        let app_build = crate::protocol::build();
        let advice = surface::advice(
            &app_build,
            self.command_build.as_ref(),
            &self.supervisor_build,
        );
        let update = Button::new("update")
            .small()
            .label(t("service.update"))
            .tooltip(t("tip.service_update"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.update(cx);
                cx.notify();
            }));
        // The button that settles the advice is the one that stands out.
        let update = match advice {
            Some(surface::Advice::UpdateCommand) => update.primary(),
            _ => update.outline(),
        };
        let running = self.report.as_ref().map(|report| report.supervisor_running);
        let installed = !matches!(registered, Some(crate::service::ServiceState::NotInstalled));
        let (state, colour) = match running {
            Some(true) => (t("fleet.supervisor_running"), GREEN),
            Some(false) if !installed => (t("service.not_installed"), RED),
            Some(false) => (t("fleet.supervisor_missing"), RED),
            None => (t("fleet.reading"), FAINT),
        };
        div()
            .id("service")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .px(step(6.))
            .py(step(5.))
            .flex()
            .flex_col()
            .gap(step(6.))
            // What it is doing now.
            .child(
                self.block(t("service.supervisor"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(step(2.))
                            .child(dot(colour))
                            .child(div().text_size(px(12.5)).text_color(rgb(DIM)).child(state)),
                    )
                    .child(
                        div().flex().flex_wrap().gap(step(1.5)).children(
                            registered
                                .map(surface::orders)
                                .unwrap_or_default()
                                .into_iter()
                                // Install and Uninstall are the switch
                                // below, said once rather than twice.
                                .filter(|order| {
                                    !matches!(
                                        order,
                                        surface::Order::Install | surface::Order::Uninstall
                                    )
                                })
                                .map(|order| {
                                    Button::new(SharedString::from(format!(
                                        "service-{}",
                                        order.label()
                                    )))
                                    .small()
                                    .outline()
                                    .label(order.label())
                                    .tooltip(order.about())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.order(order);
                                        cx.notify();
                                    }))
                                }),
                        ),
                    ),
            )
            // Whether it comes back tomorrow, which is the same question
            // as whether a login service is registered at all.
            .child(
                self.block(t("service.at_login")).child(
                    div()
                        .flex()
                        .items_center()
                        .gap(step(2.5))
                        .child(
                            Switch::new("at-login")
                                .checked(installed)
                                .tooltip(t("tip.at_login"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.order(match installed {
                                        true => surface::Order::Uninstall,
                                        false => surface::Order::Install,
                                    });
                                    cx.notify();
                                })),
                        )
                        .child(div().text_size(px(11.)).text_color(rgb(FAINT)).child(
                            match installed {
                                true => t("service.at_login_on"),
                                false => t("service.at_login_off"),
                            },
                        )),
                ),
            )
            // Whether the app says anything when a session needs a
            // person. Its own question, and not the presence below: a
            // window with no menu bar item still has something to say.
            .child(
                self.block(t("service.notifications"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(step(2.5))
                            .child(
                                Switch::new("notify")
                                    .checked(self.notify)
                                    .tooltip(t("tip.notify"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let wanted = !this.notify;
                                        this.notify_as(wanted);
                                        cx.notify();
                                    })),
                            )
                            .child(div().text_size(px(11.)).text_color(rgb(FAINT)).child(
                                match self.notify {
                                    true => t("service.notify_on"),
                                    false => t("service.notify_off"),
                                },
                            )),
                    )
                    // A hook follows the same rules, so both on means
                    // hearing everything twice. Said only when it is
                    // true of this machine right now.
                    .when(self.notify && self.hook_set, |block| {
                        block.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(AMBER))
                                .child(t("service.notify_and_hook")),
                        )
                    }),
            )
            // How much of itself the app shows.
            .child(
                self.block(t("service.showing")).child(
                    div().flex().gap(step(1.5)).children(
                        [
                            crate::preferences::Presence::Both,
                            crate::preferences::Presence::Window,
                            crate::preferences::Presence::Menubar,
                        ]
                        .into_iter()
                        .map(|presence| {
                            let chosen = presence == self.presence;
                            Button::new(SharedString::from(format!("presence-{}", presence.word())))
                                .small()
                                .when(chosen, |button| button.primary())
                                .when(!chosen, |button| button.outline())
                                .label(t(match presence {
                                    crate::preferences::Presence::Both => "presence.both",
                                    crate::preferences::Presence::Window => "presence.window",
                                    crate::preferences::Presence::Menubar => "presence.menubar",
                                }))
                                .tooltip(t(match presence {
                                    crate::preferences::Presence::Both => "tip.presence_both",
                                    crate::preferences::Presence::Window => "tip.presence_window",
                                    crate::preferences::Presence::Menubar => "tip.presence_menubar",
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.show_as(presence);
                                    cx.notify();
                                }))
                        }),
                    ),
                ),
            )
            // The things that are about the whole state root rather
            // than any one group.
            .child(
                self.block(t("service.housekeeping"))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(step(1.5))
                            .child(
                                Button::new("flush-all")
                                    .small()
                                    .outline()
                                    .label(t("service.flush_all"))
                                    .tooltip(t("tip.flush_all"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.everything(Verb::Flush);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("verify-all")
                                    .small()
                                    .outline()
                                    .label(t("service.verify_all"))
                                    .tooltip(t("tip.verify_all"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.everything(Verb::Verify);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("clean")
                                    .small()
                                    .outline()
                                    .label(t("service.clean"))
                                    .tooltip(t("tip.clean"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.ask_clean(window, cx);
                                    })),
                            ),
                    )
                    // What each of the three does, because none of them
                    // says so by its name and two of them do nothing at
                    // all without a supervisor to hear them.
                    .child(self.note(t("service.flush_all"), t("service.flush_about")))
                    .child(self.note(t("service.verify_all"), t("service.verify_about")))
                    .child(self.note(t("service.clean"), t("service.clean_about"))),
            )
            // Which build this is, and how to stop it being this one.
            .child(
                self.block(t("service.version"))
                    // Three programs, each its own build: the app, the
                    // command on disk, and the supervisor, which is the
                    // command as it was when the service last started it.
                    .child(self.build_row(t("service.build_app"), "build-app", &app_build))
                    .when_some(self.command_build.clone(), |block, command| {
                        block.child(self.build_row(
                            t("service.build_command"),
                            "build-command",
                            &fill(
                                "service.build_at",
                                &[
                                    ("version", command.version.as_str()),
                                    ("path", &tilde(&command.path.display().to_string())),
                                ],
                            ),
                        ))
                    })
                    .child(self.build_row(
                        t("service.build_supervisor"),
                        "build-supervisor",
                        &self.supervisor_build.said(),
                    ))
                    // And what to do when they are not one build.
                    .when_some(advice, |block, advice| {
                        block.child(
                            div()
                                .max_w(px(620.))
                                .text_size(px(11.5))
                                .text_color(rgb(AMBER))
                                .child(advice.said()),
                        )
                    })
                    .child(
                        div()
                            .max_w(px(620.))
                            .text_size(px(11.))
                            .text_color(rgb(FAINT))
                            .child(t("service.update_about")),
                    )
                    .child(div().flex().gap(step(1.5)).child(update).when(
                        advice == Some(surface::Advice::Restart),
                        |row| {
                            row.child(
                                Button::new("restart-build")
                                    .small()
                                    .primary()
                                    .label(surface::Order::Restart.label())
                                    .tooltip(surface::Order::Restart.about())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.order(surface::Order::Restart);
                                        cx.notify();
                                    })),
                            )
                        },
                    )),
            )
            .into_any_element()
    }

    /// One program and the build it is, under "this build".
    fn build_row(&self, name: &'static str, id: &'static str, build: &str) -> Div {
        div()
            .flex()
            .items_start()
            .gap(step(2.5))
            .child(
                div()
                    .w(px(112.))
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(DIM))
                    .child(name),
            )
            .child(
                div()
                    .font_family(self.mono.clone())
                    .child(said(id, build, DIM, 12.)),
            )
    }

    /// Wakes or re-reads every session, rather than one.
    fn everything(&mut self, verb: Verb) {
        let request = match verb {
            Verb::Flush => crate::supervisor::control::ControlRequest::Flush(
                crate::supervisor::control::Selector::default(),
            ),
            Verb::Verify => crate::supervisor::control::ControlRequest::Verify(
                crate::supervisor::control::Selector::default(),
            ),
            // Pause and Resume are a session's, not the fleet's.
            _ => return,
        };
        match crate::supervisor::control::send(&self.state_root, &request) {
            Ok(_) => self.say(
                fill("service.asked_everything", &[("done", verb.done())]),
                Tone::Done,
            ),
            Err(error) => self.say(format!("{error:#}"), Tone::Trouble),
        }
        self.read_at = None;
    }

    /// Asks before taking state away, and says what it will not touch.
    fn ask_clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dash = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let dash = dash.clone();
            alert
                .title(t("service.clean_title"))
                .description(t("service.cleaning"))
                .button_props(
                    DialogButtonProps::default()
                        .show_cancel(true)
                        .ok_text(t("service.clean")),
                )
                .on_ok(move |_, _, cx| {
                    if let Some(dash) = dash.upgrade() {
                        dash.update(cx, |dash, cx| {
                            dash.clean(cx);
                            cx.notify();
                        });
                    }
                    true
                })
        });
        cx.notify();
    }

    /// Removes the state of sessions the configuration no longer names.
    ///
    /// The command's own, not a second implementation of it: walking a
    /// state root and deciding what is nobody's is three hundred lines
    /// that already exist and are already tested.
    fn clean(&mut self, cx: &mut Context<Self>) {
        self.say(t("service.cleaning_now"), Tone::Plain);
        let config = self.config.clone();
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move { surface::ran(&["clean", "--yes"], config.as_deref()) })
                .await;
            this.update(cx, |this, cx| {
                this.say_result(done);
                this.read_at = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// One action explained: its name, and what pressing it does.
    fn note(&self, name: &'static str, words: &'static str) -> Div {
        div()
            .flex()
            // The name belongs beside the first line of what it says,
            // not beside the middle of it.
            .items_start()
            .gap(step(2.5))
            .max_w(px(700.))
            .text_size(px(11.))
            .child(
                div()
                    .w(px(112.))
                    .flex_shrink_0()
                    .text_color(rgb(DIM))
                    .child(name),
            )
            .child(div().min_w(px(0.)).text_color(rgb(FAINT)).child(words))
    }

    /// One titled block of the service pane.
    fn block(&self, title: &'static str) -> Div {
        div().flex().flex_col().gap(step(2.)).child(
            div()
                .text_size(px(10.5))
                .text_color(rgb(FAINT))
                .child(title),
        )
    }

    /// Downloads a release over this one. It takes as long as a download
    /// takes, so it happens off the main thread and the window says so
    /// while it runs.
    fn update(&mut self, cx: &mut Context<Self>) {
        self.say(t("service.updating"), Tone::Plain);
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async { surface::update() })
                .await;
            this.update(cx, |this, cx| {
                this.say_result(done);
                this.command_build = surface::command_build();
                this.read_at = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ── the splash ───────────────────────────────────────────────────

    /// The whole window, before there is anything to show in it.
    ///
    /// The icon, a greeting, and one button. Everything else people
    /// might want — where it looked, what the installer checks, how to
    /// do it by hand — is a question for somebody who hits a problem,
    /// not something to read on the way in.
    fn splash(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mark = std::sync::Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            crate::icon::IMAGE.to_vec(),
        ));
        // One line, under the button, for whatever the screen last had
        // to say: the installer talking, or an answer to the last click.
        let note = match (self.installing, &self.said) {
            (true, _) => Some(self.install_line()),
            (false, Some(said)) => Some(SharedString::from(said.clone())),
            (false, None) => None,
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(step(6.))
            .bg(rgb(GROUND))
            .text_color(rgb(INK))
            .child(img(mark).size(px(104.)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(step(1.5))
                    .child(
                        div()
                            .text_size(px(21.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t("welcome.greeting")),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(rgb(DIM))
                            .child(t("welcome.tagline")),
                    ),
            )
            .child(
                Button::new("welcome-install")
                    .primary()
                    .label(match self.installing {
                        true => t("welcome.installing"),
                        false => t("welcome.install_now"),
                    })
                    .disabled(self.installing)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.install(cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("welcome-by-hand")
                    .xsmall()
                    .ghost()
                    .label(t("welcome.by_hand"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            surface::INSTALL_LINE.to_owned(),
                        ));
                        this.say(t("welcome.copied"), Tone::Done);
                        cx.notify();
                    })),
            )
            // The loader's own words, often a path or a parse error naming
            // a line and column. Drawn through `said` like every other
            // complaint, so it can be selected and pasted rather than
            // retyped off the screen.
            .children(note.map(|note| said("welcome-note", note.as_ref(), FAINT, 11.)))
            .into_any_element()
    }

    /// The last thing the installer said, for the one line the splash
    /// has room for.
    fn install_line(&self) -> SharedString {
        std::fs::read_to_string(surface::install_log(&self.state_root))
            .ok()
            .and_then(|text| {
                text.lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .map(|line| SharedString::from(line.trim().to_owned()))
            })
            .unwrap_or_else(|| SharedString::from(t("welcome.starting")))
    }

    /// Runs the embedded installer off the main thread. The splash stays
    /// up until it is done, because until it is done there is still
    /// nothing else to show.
    fn install(&mut self, cx: &mut Context<Self>) {
        if self.installing {
            return;
        }
        self.installing = true;
        let state_root = self.state_root.clone();
        let config = self.config.clone();
        cx.spawn(async move |this, cx| {
            // The window's own poll loop asks for a frame while this
            // runs, which is what keeps the line under the button up to
            // date without a second timer here.
            let done = cx
                .background_executor()
                .spawn(async move {
                    // The command, and then the service that runs it:
                    // the button says Install Service.
                    surface::install(&state_root).and_then(|command| {
                        surface::serve_after_install(&command, config.as_deref(), &state_root)
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                this.installing = false;
                this.ready = surface::installed();
                this.command_build = surface::command_build();
                this.say_result(done);
                // Everything the other panes show came back empty while
                // there was nothing to ask; ask again now.
                this.pane = Pane::Groups;
                this.read_at = None;
                this.log = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
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
            Err(complaint) => self.say(complaint, Tone::Trouble),
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
            self.say(said, Tone::Trouble);
        }
    }

    fn save(&mut self) {
        let Some(sheet) = &mut self.sheet else { return };
        if let Some(outcome) = sheet.save() {
            self.say_result(outcome);
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
        // While the loader is behind the typing its last word is about a
        // document nobody is looking at any more, so nothing is drawn
        // from it: the line beside Save says it is checking instead.
        let checking = sheet.checking();
        let refused = match checking {
            true => None,
            false => sheet.refused().map(str::to_owned),
        };
        let faults = refused.as_deref().map(surface::faults).unwrap_or_default();
        let blamed = refused.as_deref().and_then(surface::blamed);
        // Every fault that names a key goes under that key. What is
        // left belongs to the file rather than to a field, and that is
        // what the line beside Save is for.
        // Several complaints can be about one key — two bad filenames in
        // one list are two — so a key keeps all of its own.
        self.at_fields.clear();
        let mut homeless = 0;
        for fault in &faults {
            match surface::fault_at(fault) {
                Some(at) => self
                    .at_fields
                    .entry((at.section.clone(), at.key.clone()))
                    .or_default()
                    .push(at),
                None => homeless += 1,
            }
        }
        // A warning is not a refusal — the loader would take the file —
        // so it is amber where a fault is red, and it never stops Save.
        // It lands the same way, at the key it is about.
        self.at_warned.clear();
        if !checking {
            for warning in sheet.warned() {
                if let Some(at) = surface::fault_at(warning) {
                    self.at_warned
                        .entry((at.section.clone(), at.key.clone()))
                        .or_default()
                        .push(at);
                }
            }
        }
        let showing = self.showing_faults && refused.is_some();
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
                                        // Asking to save is asking why
                                        // not, so a refusal unfolds and
                                        // the top of the form — where it
                                        // is drawn — comes into view.
                                        let refused = this
                                            .sheet
                                            .as_ref()
                                            .is_some_and(|sheet| sheet.refused().is_some());
                                        if refused {
                                            this.showing_faults = true;
                                            this.form.set_offset(point(px(0.), px(0.)));
                                        }
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
                                .when(room > Room::Tight, |line| line.pb(step(1.)))
                                .text_size(px(10.5))
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
                                .flex_shrink_0()
                                .when(room > Room::Tight, |line| line.pb(step(2.5)))
                                .text_size(px(10.5))
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
                                .mx(step(2.))
                                .px(step(0.5))
                                .flex_shrink_0()
                                .when(room > Room::Tight, |line| line.mb(step(2.5)))
                                .flex()
                                .items_baseline()
                                .gap(step(1.5))
                                .cursor_pointer()
                                .child(div().text_size(px(9.)).text_color(rgb(FAINT)).child(
                                    match showing {
                                        true => "\u{25be}",
                                        false => "\u{25b8}",
                                    },
                                ))
                                .child(
                                    div().text_size(px(11.)).text_color(rgb(RED)).child(counted(
                                        "config.faults",
                                        homeless,
                                        &[],
                                    )),
                                )
                                .child(
                                    div()
                                        .font_family(self.mono.clone())
                                        .text_size(px(10.))
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
                    .children(sections.clone().into_iter().map(|section| {
                        let chosen = section == open;
                        let label = section.title();
                        let group = matches!(section, Section::Group(_));
                        // A group's own two actions ride on its row and
                        // show on hover, which is where a person looks
                        // for them — not in a pair of buttons at the
                        // foot of the list, where they belonged to
                        // whichever row happened to be open.
                        let name = match &section {
                            Section::Group(name) => Some(name.clone()),
                            _ => None,
                        };
                        let crew = SharedString::from(format!("row-{label}"));
                        div()
                            .id(SharedString::from(format!("section-{label}")))
                            .group(crew.clone())
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
                            .child(div().min_w(px(0.)).truncate().child(label.clone()))
                            .child(div().flex_1())
                            .when(blamed.as_ref() == Some(&section), |row| row.child(dot(RED)))
                            .when_some(name.clone(), |row, name| {
                                let renaming = name.clone();
                                let dropping = name.clone();
                                row.child(
                                    div()
                                        .id(SharedString::from(format!("edit-{renaming}")))
                                        .text_color(rgba(0x00000000))
                                        .group_hover(crew.clone(), |glyph| {
                                            glyph.text_color(rgb(FAINT))
                                        })
                                        .hover(|glyph| glyph.text_color(rgb(INK)))
                                        .text_size(px(11.))
                                        .child("\u{270e}")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            // The row beneath selects a section;
                                            // this does not.
                                            cx.stop_propagation();
                                            this.ask_rename(&renaming, window, cx);
                                        })),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(format!("drop-{dropping}")))
                                        .text_color(rgba(0x00000000))
                                        .group_hover(crew.clone(), |glyph| {
                                            glyph.text_color(rgb(FAINT))
                                        })
                                        .hover(|glyph| glyph.text_color(rgb(RED)))
                                        .text_size(px(11.))
                                        .child("\u{2715}")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.ask_drop(&dropping, window, cx);
                                        })),
                                )
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.section = section.clone();
                                cx.notify();
                            }))
                    }))
                    // Making a group: a name, and the two keys it cannot
                    // load without, left empty for the form to ask for.
                    .child(self.naming(window, cx)),
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
                                        .text_size(px(11.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(rgb(RED))
                                        .child(t("config.refused")),
                                )
                                .children(
                                    faults
                                        .iter()
                                        .filter(|fault| surface::fault_at(fault).is_none())
                                        .enumerate()
                                        .map(|(index, fault)| {
                                            div().min_w(px(0.)).child(said(
                                                format!("homeless-{index}"),
                                                &crate::text::display_block(fault),
                                                DIM,
                                                11.,
                                            ))
                                        }),
                                ),
                        )
                    })
                    .children(self.form(window, cx)),
            )
            .into_any_element()
    }

    /// The one row under the list: a plus that opens the naming dialog.
    ///
    /// Built like a section row rather than as a button, because a
    /// button brings its own padding and the words then start somewhere
    /// the group names above do not. The plus sits in a box the width
    /// of a group's dot, so every label in the column lines up.
    fn naming(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("new-group")
            .mt(step(1.))
            .px(step(2.5))
            .py(step(1.5))
            .rounded(px(6.))
            .cursor_pointer()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(step(1.5))
            .text_size(px(12.5))
            .text_color(rgb(FAINT))
            .hover(|row| row.bg(rgb(PANEL)).text_color(rgb(INK)))
            .child(
                div()
                    .w(px(7.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child("+"),
            )
            .child(t("group.add"))
            .on_click(cx.listener(|this, _, window, cx| {
                this.ask_name(None, window, cx);
            }))
            .into_any_element()
    }

    /// Asks for a name — for a group being made, or one being renamed.
    ///
    /// A dialog rather than a field under the list: naming a group is a
    /// thing a person starts and finishes, and a window that keeps the
    /// rest of itself live underneath invites them to wander off and
    /// leave a half-typed name sitting in a corner.
    fn ask_name(&mut self, from: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let held = from.map(str::to_owned);
        let field = cx.new(|cx| {
            let field = TextareaState::new(window, cx).placeholder(t("group.name_it"));
            match &held {
                Some(name) => field.default_value(name.clone()),
                None => field,
            }
        });
        field.update(cx, |field, cx| field.focus(window, cx));
        self.naming = Some(field.clone());
        self.renaming = held.clone();
        let dash = cx.entity().downgrade();
        let title = match held.is_some() {
            true => t("group.rename_title"),
            false => t("group.new_title"),
        };
        window.open_dialog(cx, move |dialog: Dialog, _, _| {
            let dash = dash.clone();
            dialog
                .title(title)
                .child(Textarea::new(&field).bordered(true))
                .button_props(
                    DialogButtonProps::default()
                        .show_cancel(true)
                        .ok_text(t("group.make")),
                )
                .on_ok(move |_, _, cx| {
                    if let Some(dash) = dash.upgrade() {
                        dash.update(cx, |dash, cx| {
                            dash.make(cx);
                            cx.notify();
                        });
                    }
                    true
                })
        });
        cx.notify();
    }

    /// The same, for the pencil on a row.
    fn ask_rename(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_name(Some(name), window, cx);
    }

    /// Asks before a group leaves the file, and says what it cannot do.
    fn ask_drop(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let name = name.to_owned();
        let dash = cx.entity().downgrade();
        let question = fill("group.removing", &[("name", &name)]);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let dash = dash.clone();
            let name = name.clone();
            alert
                .title(t("group.remove_title"))
                .description(question.clone())
                .show_cancel(true)
                .on_ok(move |_, _, cx| {
                    if let Some(dash) = dash.upgrade() {
                        dash.update(cx, |dash, cx| {
                            dash.drop(&name);
                            cx.notify();
                        });
                    }
                    true
                })
        });
        cx.notify();
    }

    /// Makes the group the field names, or renames the open one to it.
    fn make(&mut self, cx: &mut Context<Self>) {
        let Some(field) = self.naming.clone() else {
            return;
        };
        let name = field.read(cx).value().to_string();
        let name = name.trim().to_owned();
        let from = self.renaming.clone();
        let Some(sheet) = &mut self.sheet else { return };
        let complaint = match &from {
            Some(from) => sheet.rename_group(from, &name),
            None => sheet.make_group(&name),
        };
        match complaint {
            Some(said) => self.say(said, Tone::Trouble),
            None => {
                self.say(
                    fill(
                        match from.is_some() {
                            true => "group.renamed",
                            false => "group.made",
                        },
                        &[("name", &name)],
                    ),
                    Tone::Done,
                );
                // The form follows the group it just made or renamed,
                // and every cached field belongs to the old name.
                self.section = Section::Group(name);
                self.fields.clear();
                self.choices.clear();
                self.lists.clear();
                self.naming = None;
                self.renaming = None;
            }
        }
    }

    /// Takes the group out, and says what is left to do about its state.
    fn drop(&mut self, name: &str) {
        let Some(sheet) = &mut self.sheet else { return };
        match sheet.drop_group(name) {
            Some(said) => self.say(said, Tone::Trouble),
            None => {
                self.say(fill("group.removed", &[("name", name)]), Tone::Done);
                self.section = Section::Settings;
                self.fields.clear();
                self.choices.clear();
                self.lists.clear();
            }
        }
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
    fn run(
        &mut self,
        part: &Section,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let properties = match part {
            Section::Settings => self.shape.get("properties").cloned(),
            Section::Defaults => self.shape["$defs"]["Defaults"].get("properties").cloned(),
            Section::Advanced => self.shape["$defs"]["Advanced"].get("properties").cloned(),
            Section::Alerts => self.shape["$defs"]["AlertsAdvanced"]
                .get("properties")
                .cloned(),
            Section::P2P => self.shape["$defs"]["P2pAdvanced"]
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
        let at = (part.clone(), key.to_owned());

        let said_here: Vec<(String, Vec<String>)> = self
            .at_fields
            .get(&at)
            .or_else(|| self.at_warned.get(&at))
            .map(|every| {
                every
                    .iter()
                    .map(|at| (at.said.clone(), at.instead.clone()))
                    .collect()
            })
            .unwrap_or_default();
        // Red is "something here is wrong", whether the loader refuses
        // the file over it or merely remarks on it — a hook that is not
        // there is a broken alerter either way. Amber is kept for the
        // one thing it can mean on its own: edited, not written yet.
        // The two used to share it, and a dot could not be read.
        let ink = RED;
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
            .when(self.room == Room::Tight, |field| {
                field.flex_col().gap(step(1.5))
            })
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
                                    // says so where the key is read, not
                                    // only in a count at the top — and a
                                    // key the loader is refusing says so
                                    // louder than one merely edited.
                                    .when(touched || !said_here.is_empty(), |name| {
                                        name.text_color(rgb(match said_here.is_empty() {
                                            false => ink,
                                            true => AMBER,
                                        }))
                                    })
                                    .child(key.to_owned()),
                            )
                            .when(touched || !said_here.is_empty(), |row| {
                                row.child(dot(match said_here.is_empty() {
                                    false => ink,
                                    true => AMBER,
                                }))
                            }),
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
                    // The loader's complaint about this value, under the
                    // value: the mistake and the fix in one place.
                    .children(said_here.iter().enumerate().map(|(nth, (words, instead))| {
                        let at = (part.clone(), key.to_owned());
                        div()
                            .flex()
                            .flex_col()
                            .gap(step(1.))
                            .child(
                                div()
                                    .max_w(px(620.))
                                    .flex()
                                    .gap(step(1.5))
                                    .text_size(px(11.))
                                    .text_color(rgb(ink))
                                    .child(div().flex_shrink_0().child("\u{26a0}"))
                                    // A flex child will not wrap until it
                                    // is allowed to be narrower than its
                                    // text, which is what this says.
                                    .child(div().min_w(px(0.)).child(said(
                                        format!("said-{key}-{nth}"),
                                        words,
                                        ink,
                                        11.,
                                    ))),
                            )
                            .when(!instead.is_empty(), |column| {
                                column.child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .gap(step(1.5))
                                        .children(instead.iter().map(|word| {
                                            let taken = word.clone();
                                            let at = at.clone();
                                            Button::new(SharedString::from(format!(
                                                "instead-{key}-{nth}-{word}"
                                            )))
                                            .xsmall()
                                            .outline()
                                            .label(word.clone())
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.offer(&at, &taken, window, cx);
                                                cx.notify();
                                            }))
                                        })),
                                )
                            })
                    }))
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

    // One parameter per thing the schema knows about the field, plus the
    // two GPUI asks of every render.
    #[allow(clippy::too_many_arguments)]
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
            // A value the file holds that is not one of these words has
            // no row to select, and a control that then says "not in the
            // file" about a line that is in the file is lying about it.
            let unknown = set && !words.iter().any(|(word, _)| *word == now);
            let default = match (unknown, field["default"].as_str()) {
                (true, _) => now.clone(),
                (false, Some(default)) => default.to_owned(),
                (false, None) => fallback.to_owned(),
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
                                Select::new(&list).menu_width(px(580.)).placeholder(
                                    match (unknown, default.is_empty()) {
                                        (true, _) => default.clone(),
                                        (false, true) => t("config.absent").to_owned(),
                                        (false, false) => {
                                            fill("config.default_is", &[("default", &default)])
                                        }
                                    },
                                ),
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
                let at = (part.clone(), key.to_owned());
                let named = key.to_owned();
                let key = key.to_owned();
                let section = section.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(step(2.5))
                    // A key the file does not hold is a third state, and
                    // a switch has two positions. Drawn faint, it reads
                    // as a value nobody chose — which is what it is —
                    // and the one beside it that somebody did chose
                    // reads as deliberate without having to be read.
                    .child(
                        div().when(!set, |held| held.opacity(0.45)).child(
                            Switch::new(SharedString::from(format!("switch-{named}")))
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
                        ),
                    )
                    .when(!set, |row| {
                        row.child(div().text_size(px(11.)).text_color(rgb(FAINT)).child(
                            match default.as_bool() {
                                Some(true) => t("config.absent_on").to_owned(),
                                Some(false) => t("config.absent_off").to_owned(),
                                None => t("config.absent").to_owned(),
                            },
                        ))
                    })
                    // The way back to the third state, which a two-state
                    // control cannot otherwise reach. The same Unset a
                    // dropdown offers, so a field that is set offers one
                    // way to stop being set, whatever kind of field.
                    .when(set, |row| {
                        row.child(
                            Button::new(SharedString::from(format!("unset-switch-{named}")))
                                .small()
                                .ghost()
                                .label(t("config.unset"))
                                .tooltip(t("tip.unset"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.unset(&at);
                                    cx.notify();
                                })),
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

    /// Put one of the words the loader offered into the field.
    ///
    /// A list field is a block of lines, and the bad line is the one
    /// being complained about — so the offered name replaces it rather
    /// than being appended under it.
    fn offer(
        &mut self,
        at: &(Section, String),
        word: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A field that takes one of a fixed set of words has no text
        // block behind it — the offered word is simply the value.
        let Some(block) = self.fields.get(at).cloned() else {
            self.put(
                &Spot {
                    section: at.0.clone(),
                    key: at.1.clone(),
                    item: None,
                },
                Some(toml_edit::value(word.to_owned())),
            );
            self.choices.remove(at);
            return;
        };
        let wrong = self
            .at_fields
            .get(at)
            .and_then(|every| every.first())
            .and_then(|fault| {
                // The name it complained about is the one in quotes.
                let (_, rest) = fault.said.split_once('"')?;
                let (named, _) = rest.split_once('"')?;
                Some(named.to_owned())
            })
            .unwrap_or_default();
        let held = block.read(cx).value().to_string();
        let written: Vec<String> = match wrong.is_empty() {
            true => vec![word.to_owned()],
            false => held
                .lines()
                .map(|line| match line.trim() == wrong {
                    true => word.to_owned(),
                    false => line.to_owned(),
                })
                .collect(),
        };
        block.update(cx, |block, cx| {
            block.set_value(written.join("\n"), window, cx);
        });
        self.take_one(at, cx);
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
    #[allow(clippy::too_many_arguments)] // as `widget` above
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
        let list =
            self.lists.contains(at) || held.as_ref().is_some_and(|item| item.as_array().is_some());
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
        // A field being emptied is the one edit worth waiting for: it
        // is how a person takes back a value the loader complained
        // about, and holding the complaint on screen for another half
        // second says the deletion did not work.
        let cleared = value.is_none();
        if let Some(sheet) = &mut self.sheet {
            let said = match cleared {
                true => sheet.put(&at, value),
                false => sheet.later(&at, value),
            };
            if let Some(said) = said {
                self.say(said, Tone::Trouble);
            }
        }
        self.typed_at = match cleared {
            true => None,
            false => Some(Instant::now()),
        };
    }

    // ── what the window does to the fleet ────────────────────────────

    fn resolve(&mut self, item: &Conflict, keep: &str) {
        let mut command = std::process::Command::new(surface::exe());
        // This row's destination, not the whole group: the conflict is
        // one session's, and the command would otherwise reach for every
        // destination in the group, including one that is away.
        command
            .arg("resolve")
            .arg(&item.group)
            .arg(&item.path)
            .arg("--keep")
            .arg(keep)
            .arg("--host")
            .arg(&item.host)
            .arg("--yes")
            .arg("--state-root")
            .arg(&self.state_root);
        if let Some(config) = &self.config {
            command.arg("--config").arg(config);
        }
        match command.output() {
            Ok(output) if output.status.success() => {
                self.conflict = None;
                self.sides = None;
                self.diff = None;
                self.say(
                    fill(
                        "status.kept",
                        &[
                            ("keep", keep),
                            ("path", &crate::text::display_safe(&item.path)),
                        ],
                    ),
                    Tone::Done,
                );
            }
            Ok(output) => self.say(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                Tone::Trouble,
            ),
            Err(error) => self.say(
                fill("status.resolve_failed", &[("error", &error.to_string())]),
                Tone::Trouble,
            ),
        }
        self.read_at = None;
    }

    fn read_diff(&mut self, item: &Conflict) {
        let mut command = std::process::Command::new(surface::exe());
        // This row's destination: without it the command compares every
        // destination in turn, and a file that is identical on the first
        // one read as identical, with the real difference further down.
        command
            .arg("diff")
            .arg(&item.group)
            .arg(&item.path)
            .arg("--host")
            .arg(&item.host)
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
        self.say_result(surface::reveal(file));
    }
}

/// What kind of thing the footer says: the colour of its message.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    /// Something to know.
    Plain,
    /// Something that worked.
    Done,
    /// Something that did not.
    Trouble,
}

/// A group's doctor report, open under its card.
#[derive(Clone)]
struct Doctored {
    group: String,
    report: String,
    /// Opened by Reset: the report is what to read before merging, and
    /// the Merge button sits with it.
    offer_reset: bool,
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

/// A line of the loader's own words, which a person can select and copy.
///
/// Every other label in this window is text the window wrote and a
/// reader can retype. A complaint is not: it names a path, a value,
/// a filename, and the thing anybody wants to do with it is paste it
/// somewhere. So complaints alone are drawn through the kit's text
/// view, which selects, rather than a plain label, which cannot.
fn said(id: impl Into<SharedString>, words: &str, colour: u32, size: f32) -> impl IntoElement {
    let id: SharedString = id.into();
    TextView::markdown(id, surface::as_written(words))
        .selectable(true)
        // A selection carries what was read, not the escaping that got
        // it there.
        .selection_format(SelectionFormat::Plain)
        .text_size(px(size))
        .text_color(rgb(colour))
}

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
