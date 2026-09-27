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
    Groups,
    Conflicts,
    Config,
    Log,
    Hosts,
}

impl Pane {
    fn title(self) -> &'static str {
        match self {
            Pane::Groups => "Groups",
            Pane::Conflicts => "Conflicts",
            Pane::Config => "Config",
            Pane::Log => "Log",
            Pane::Hosts => "Hosts",
        }
    }

    /// The line under the title: what this pane is for.
    fn about(self) -> &'static str {
        match self {
            Pane::Groups => "every group, every session, and what each one last did",
            Pane::Conflicts => "the paths waiting on a person",
            Pane::Config => "the file, as the parser reads it",
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
    /// Both sides of the open conflict, read when it was opened.
    sides: Option<(Side, Side)>,
    diff: Option<String>,
    /// The configuration file as the editor holds it, and which part of
    /// it the form is showing.
    sheet: Option<Sheet>,
    section: Section,
    /// The field being typed into, if any.
    typing: Option<Typing>,
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

#[derive(Clone, PartialEq, Eq)]
struct Conflict {
    group: String,
    host: String,
    path: String,
    blocked: bool,
    /// Where each side of this path lives. A conflict over a file that
    /// is not text has no diff to read, so the two files themselves are
    /// what the window has to show — and it needs to know where they are.
    alpha_root: String,
    beta_root: String,
}

/// Which part of the file the form is showing.
#[derive(Clone, PartialEq, Eq)]
enum Section {
    /// The keys at the top of the file.
    Settings,
    /// `[defaults]`, inherited by every group.
    Defaults,
    /// One `[groups.x]`.
    Group(String),
}

impl Section {
    fn title(&self) -> String {
        match self {
            Section::Settings => "settings".to_owned(),
            Section::Defaults => "defaults".to_owned(),
            Section::Group(name) => name.clone(),
        }
    }
}

/// The configuration file, as the editor holds it between saves.
struct Sheet {
    path: PathBuf,
    /// The file exactly as it was read, so an edit made elsewhere since
    /// then is noticed rather than overwritten.
    text: String,
    /// The file itself, comments and order kept: every edit goes through
    /// `toml_edit`, so saving a form does not rewrite a hand-written
    /// file into something its author would not recognise.
    document: toml_edit::DocumentMut,
    /// What the parser said when the last save was refused. Nothing is
    /// written while this is set — the parser is the referee, not the
    /// form.
    refused: Option<String>,
}

/// A field being typed into.
struct Typing {
    at: Spot,
    text: String,
    /// Where the next character goes, as a byte index into `text`.
    cursor: usize,
}

/// Where one value lives in the file.
#[derive(Clone, PartialEq, Eq)]
struct Spot {
    section: Section,
    key: String,
    /// Which entry, when the value is a list.
    item: Option<usize>,
}

/// One side of a conflict, as the filesystem has it. Read once, when the
/// path is opened, and never again on the way to a frame: a window that
/// hashes a file every sixtieth of a second is a window that stops.
#[derive(Clone, PartialEq, Eq)]
struct Side {
    /// `alpha` or `beta`, the words every other surface uses.
    name: &'static str,
    /// The directory this side of the pair lives in.
    root: String,
    /// The whole path, as a person would type it.
    place: String,
    /// The file itself, when it is on this machine.
    file: Option<PathBuf>,
    size: Option<u64>,
    /// Seconds since the epoch, for the same stamp the log uses.
    modified: Option<i64>,
    /// The blake3 of the contents — the digest the scanner records, so
    /// two sides that agree here are the same file to the engine too.
    digest: Option<String>,
    /// Whether the first few kilobytes hold a NUL, which is how `diff`
    /// decides it will not print the file either.
    binary: bool,
    /// Why there is nothing else to say: another machine, or gone.
    trouble: Option<String>,
}

/// Files larger than this are measured and dated but not hashed. Reading
/// a gigabyte to fill in one line is not worth freezing the window for,
/// and the size and the time already answer "which one is mine".
const HASH_LIMIT: u64 = 512 * 1024 * 1024;

/// Everything about one side, in one pass over the file.
fn inspect(name: &'static str, root: &str, path: &str) -> Side {
    let place = format!("{}/{path}", root.trim_end_matches('/'));
    let Some(directory) = on_this_machine(root) else {
        return Side {
            name,
            root: root.to_owned(),
            place,
            file: None,
            size: None,
            modified: None,
            digest: None,
            binary: false,
            trouble: Some("on another machine".to_owned()),
        };
    };
    let file = directory.join(path);
    let mut side = Side {
        name,
        root: root.to_owned(),
        place,
        file: Some(file.clone()),
        size: None,
        modified: None,
        digest: None,
        binary: false,
        trouble: None,
    };
    match std::fs::symlink_metadata(&file) {
        Ok(metadata) => {
            side.size = Some(metadata.len());
            side.modified = metadata
                .modified()
                .ok()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| since.as_secs() as i64);
            if !metadata.is_file() {
                side.trouble = Some("not a plain file".to_owned());
                return side;
            }
        }
        Err(error) => {
            side.trouble = Some(format!("cannot be read: {}", error.kind()));
            return side;
        }
    }
    match read_through(&file, side.size.unwrap_or(0)) {
        Ok((binary, digest)) => {
            side.binary = binary;
            side.digest = digest;
        }
        Err(error) => side.trouble = Some(format!("cannot be read: {error}")),
    }
    side
}

/// Reads the file once: says whether it looks binary, and hashes it when
/// it is small enough to be worth hashing.
fn read_through(file: &std::path::Path, size: u64) -> std::io::Result<(bool, Option<String>)> {
    use std::io::Read;

    let mut handle = std::fs::File::open(file)?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut hasher = blake3::Hasher::new();
    let hash = size <= HASH_LIMIT;
    let mut read = 0u64;
    let mut binary = false;
    loop {
        let count = handle.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        // `diff` calls a file binary on a NUL near its start, and so
        // does this, so the two never disagree about what can be shown.
        if read < 8192 && buffer[..count].contains(&0) {
            binary = true;
        }
        if hash {
            hasher.update(&buffer[..count]);
        } else if binary {
            break;
        }
        read += count as u64;
    }
    Ok((
        binary,
        hash.then(|| hasher.finalize().to_hex().to_string()),
    ))
}

/// The directory a root names on this machine, or `None` when the root
/// is `host:path` and belongs to another one.
fn on_this_machine(root: &str) -> Option<PathBuf> {
    if let Some((before, _)) = root.split_once(':') {
        if !before.starts_with('/') && !before.starts_with('~') && !before.contains('/') {
            return None;
        }
    }
    let Some(rest) = root.strip_prefix('~') else {
        return Some(PathBuf::from(root));
    };
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(rest.trim_start_matches('/')))
}

/// A size a person can hold in their head, and the exact one beside it.
fn human_size(size: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if size < 1024 {
        return format!("{size} bytes");
    }
    let mut value = size as f64 / 1024.0;
    let mut unit = UNITS[0];
    for next in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!("{value:.1} {unit} · {} bytes", thousands(size))
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
            // A window over the fleet is the whole app: when it closes,
            // the app has nothing left to be.
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            open_window(config.clone(), state_root.clone(), None, cx).ok();
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
                    let frame = camera::grab(
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
            title: Some("Autobahn Desk".into()),
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
            typing: None,
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
                    .child(self.nav(Pane::Groups, None, cx))
                    .child(self.nav(Pane::Conflicts, Some(waiting), cx))
                    .child(self.nav(Pane::Config, None, cx))
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

    fn groups(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(report) = self.report.clone() else {
            return empty("reading the fleet…");
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
                // Wide enough for a path: the two roots are the point of
                // this panel, and a clipped root names nothing.
                div()
                    .w(px(560.))
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
                                        this.open_conflict(item.clone());
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
                                .when(!binary, |row| {
                                    row.child(
                                        button("show-diff", "Show the difference").on_click(
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
            .child(label("neither side is text"))
            .child(
                div()
                    .max_w(px(680.))
                    .text_size(px(T_META))
                    .text_color(rgb(DIM))
                    .child(
                        "There is nothing to merge, so the only question is which file \
                         survives. Keep both settles it by keeping the other one beside \
                         it under a suffixed name.",
                    ),
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
                                .child(format!(
                                    "Both sides hash the same: {suffix} is one file in \
                                     two places, and either choice keeps it."
                                )),
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
                    .when(newer, |head| head.child(pill("written last", BLUE)))
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
                "size",
                side.size.map(human_size).unwrap_or_else(|| "—".to_owned()),
            ))
            .child(self.pair(
                "written",
                side.modified
                    .map(|at| crate::logging::stamp(at as libc::time_t))
                    .unwrap_or_else(|| "—".to_owned()),
            ))
            .child(self.pair(
                "blake3",
                match (&side.digest, side.size) {
                    (Some(digest), _) => digest.chars().take(16).collect::<String>(),
                    (None, Some(size)) if size > HASH_LIMIT => "too large to hash here".to_owned(),
                    _ => "—".to_owned(),
                },
            ))
            .when_some(file, |card, file| {
                card.child(
                    div().pt(step(1.5)).flex().child(
                        button(
                            format!("reveal-{}", side.name),
                            "Reveal in Finder",
                        )
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
            return empty("no configuration file to read");
        };
        let path = sheet.path.clone();
        let refused = sheet.refused.clone();
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
                            .child(
                                "Every change is written at once. Nothing is written that the \
                                 parser would not load.",
                            ),
                    )
                    .child(
                        div().px(step(2.)).pb(step(2.5)).flex().child(
                            button("re-read-config", "Re-read the file").on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.typing = None;
                                    this.read_sheet();
                                    cx.notify();
                                },
                            )),
                        ),
                    )
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
                                this.typing = None;
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
                                        .text_size(px(T_META))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(rgb(AMBER))
                                        .child("not saved — the file would not load"),
                                )
                                .child(
                                    div()
                                        .font_family(self.mono.clone())
                                        .text_size(px(T_META))
                                        .text_color(rgb(DIM))
                                        .child(crate::text::display_block(&refused)),
                                ),
                        )
                    })
                    .children(self.form(cx)),
            )
            .into_any_element()
    }

    /// The sections of the open file, in the order they are written.
    fn sections(&self) -> Vec<Section> {
        let mut sections = vec![Section::Settings, Section::Defaults];
        if let Some(sheet) = &self.sheet {
            if let Some(groups) = sheet.document.get("groups").and_then(|item| item.as_table()) {
                for (name, _) in groups.iter() {
                    sections.push(Section::Group(name.to_owned()));
                }
            }
        }
        sections
    }

    /// The fields of the open section, from the schema.
    fn form(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let properties = match &self.section {
            Section::Settings => self.shape.get("properties").cloned(),
            Section::Defaults => self.shape["$defs"]["Defaults"].get("properties").cloned(),
            Section::Group(_) => self.shape["$defs"]["Group"].get("properties").cloned(),
        };
        let Some(serde_json::Value::Object(properties)) = properties else {
            return vec![empty("the schema says nothing about this section")];
        };
        properties
            .iter()
            .filter(|(key, _)| !SILENT.contains(&key.as_str()))
            .map(|(key, field)| self.field(key, field, cx))
            .collect()
    }

    /// One field: its name, what it holds now, and the control for it.
    fn field(&self, key: &str, field: &serde_json::Value, cx: &mut Context<Self>) -> AnyElement {
        let about = field["description"].as_str().unwrap_or_default().to_owned();
        let words: Vec<(String, String)> = field["x-words"]
            .as_array()
            .map(|words| {
                words
                    .iter()
                    .map(|word| {
                        (
                            word["word"].as_str().unwrap_or_default().to_owned(),
                            word["about"].as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let widget = field["x-widget"].as_str().unwrap_or_default().to_owned();
        let held = self.held(key);
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
                            .font_family(self.mono.clone())
                            .text_size(px(T_ROW))
                            .text_color(rgb(INK))
                            .child(key.to_owned()),
                    )
                    .when(!widget.is_empty(), |column| {
                        column.child(
                            div()
                                .text_size(px(T_PILL))
                                .text_color(rgb(FAINT))
                                .child(widget.clone()),
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
                    .child(self.widget(key, field, &words, held, cx))
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
        key: &str,
        field: &serde_json::Value,
        words: &[(String, String)],
        held: Option<toml_edit::Item>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let section = self.section.clone();
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
                .children(words.iter().map(|(word, _)| {
                    let chosen = *word == now;
                    let word = word.clone();
                    let writing = word.clone();
                    let key = key.to_owned();
                    let section = section.clone();
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
                            .child("not in the file · inherited"),
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
                    .child(self.switch(key, now, cx))
                    .when(!set, |row| {
                        row.child(
                            div()
                                .text_size(px(T_META))
                                .text_color(rgb(FAINT))
                                .child(match default.as_bool() {
                                    Some(true) => "not in the file · on unless said otherwise",
                                    Some(false) => "not in the file · off unless said otherwise",
                                    None => "not in the file",
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
                self.list(key, entries, cx)
            }
            Holds::Line => {
                let text = held.as_ref().map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| item.to_string().trim().to_owned())
                });
                self.line(key, None, text, cx)
            }
        }
    }

    fn switch(&self, key: &str, on: bool, cx: &mut Context<Self>) -> AnyElement {
        let section = self.section.clone();
        let key = key.to_owned();
        toggle_switch(SharedString::from(format!("switch-{key}")), on)
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

    /// A list of strings: each entry editable, removable, and one more
    /// can be started.
    fn list(&self, key: &str, entries: Vec<String>, cx: &mut Context<Self>) -> AnyElement {
        let section = self.section.clone();
        div()
            .flex()
            .flex_col()
            .gap(step(1.))
            .children(entries.iter().enumerate().map(|(index, entry)| {
                let key = key.to_owned();
                let section = section.clone();
                div()
                    .flex()
                    .items_center()
                    .gap(step(1.5))
                    .child(self.line(&key, Some(index), Some(entry.clone()), cx))
                    .child(
                        button(format!("drop-{key}-{index}"), "×").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.drop_item(
                                    &Spot {
                                        section: section.clone(),
                                        key: key.clone(),
                                        item: Some(index),
                                    },
                                );
                                cx.notify();
                            },
                        )),
                    )
            }))
            .child({
                let key = key.to_owned();
                let section = section.clone();
                let next = entries.len();
                div().flex().child(
                    button(format!("add-{key}"), "Add").on_click(cx.listener(
                        move |this, _, window, cx| {
                            this.start_typing(
                                Spot {
                                    section: section.clone(),
                                    key: key.clone(),
                                    item: Some(next),
                                },
                                String::new(),
                                window,
                            );
                            cx.notify();
                        },
                    )),
                )
            })
            .into_any_element()
    }

    /// One line of text: what it holds, or a caret where it is being
    /// typed into.
    fn line(
        &self,
        key: &str,
        item: Option<usize>,
        held: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let spot = Spot {
            section: self.section.clone(),
            key: key.to_owned(),
            item,
        };
        let typing = self
            .typing
            .as_ref()
            .filter(|typing| typing.at == spot);
        let id = SharedString::from(format!(
            "line-{}-{key}-{}",
            self.section.title(),
            item.map(|index| index.to_string()).unwrap_or_default()
        ));
        let frame = div()
            .id(id)
            .h(step(7.))
            .px(step(2.))
            .min_w(px(220.))
            .max_w(px(620.))
            .rounded(px(6.))
            .border_1()
            .flex()
            .items_center()
            .cursor_pointer()
            .font_family(self.mono.clone())
            .text_size(px(T_META));
        match typing {
            Some(typing) => {
                let (before, after) = typing.text.split_at(typing.cursor);
                frame
                    .bg(rgb(SUNK))
                    .border_color(tint(BLUE, 0x80))
                    .text_color(rgb(INK))
                    .child(before.to_owned())
                    .child(div().w(px(1.5)).h(px(15.)).bg(rgb(BLUE)))
                    .child(after.to_owned())
                    .into_any_element()
            }
            None => {
                let start = held.clone().unwrap_or_default();
                frame
                    .bg(rgb(RAISED))
                    .border_color(rgb(LINE))
                    .hover(|line| line.border_color(rgb(0x39424e)))
                    .text_color(match held.is_some() {
                        true => rgb(INK),
                        false => rgb(FAINT),
                    })
                    .child(match &held {
                        Some(text) if text.is_empty() => "(empty)".to_owned(),
                        Some(text) => text.clone(),
                        None => "not set".to_owned(),
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_typing(spot.clone(), start.clone(), window);
                        cx.notify();
                    }))
                    .into_any_element()
            }
        }
    }

    // ── what the editor does to the file ─────────────────────────────

    fn config_path(&self) -> Option<PathBuf> {
        match &self.config {
            Some(path) => Some(path.clone()),
            None => crate::paths::default_config_path().ok(),
        }
    }

    fn read_sheet(&mut self) {
        let Some(path) = self.config_path() else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match text.parse::<toml_edit::DocumentMut>() {
                Ok(document) => {
                    self.sheet = Some(Sheet {
                        path,
                        text,
                        document,
                        refused: None,
                    })
                }
                Err(error) => {
                    self.said = Some(format!("{} does not parse: {error}", path.display()))
                }
            },
            Err(error) => {
                self.said = Some(format!("unable to read {}: {error}", path.display()))
            }
        }
    }

    /// What the file holds for a key of the open section.
    fn held(&self, key: &str) -> Option<toml_edit::Item> {
        let sheet = self.sheet.as_ref()?;
        let table: &toml_edit::Item = match &self.section {
            Section::Settings => sheet.document.as_item(),
            Section::Defaults => sheet.document.get("defaults")?,
            Section::Group(name) => sheet.document.get("groups")?.get(name)?,
        };
        table.get(key).cloned()
    }

    fn start_typing(&mut self, at: Spot, text: String, window: &mut Window) {
        let cursor = text.len();
        self.typing = Some(Typing { at, text, cursor });
        window.focus(&self.focus);
    }

    /// Keys, while a field is being typed into. Nothing else in the
    /// window reads them.
    fn typed(&mut self, event: &gpui::KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(typing) = &mut self.typing else {
            return;
        };
        let key = event.keystroke.key.as_str();
        let command = event.keystroke.modifiers.platform;
        match key {
            "escape" => self.typing = None,
            "enter" => {
                let typing = self.typing.take().expect("a field is being typed into");
                let text = typing.text.clone();
                self.put(&typing.at, Some(toml_edit::value(text)));
            }
            "backspace" => {
                if typing.cursor > 0 {
                    let mut at = typing.cursor - 1;
                    while !typing.text.is_char_boundary(at) {
                        at -= 1;
                    }
                    typing.text.replace_range(at..typing.cursor, "");
                    typing.cursor = at;
                }
            }
            "left" => {
                let mut at = typing.cursor;
                while at > 0 {
                    at -= 1;
                    if typing.text.is_char_boundary(at) {
                        break;
                    }
                }
                typing.cursor = at;
            }
            "right" => {
                let mut at = typing.cursor;
                while at < typing.text.len() {
                    at += 1;
                    if typing.text.is_char_boundary(at) {
                        break;
                    }
                }
                typing.cursor = at;
            }
            "v" if command => {
                if let Some(pasted) = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .filter(|text| !text.is_empty())
                {
                    let pasted: String = pasted.lines().next().unwrap_or_default().to_owned();
                    let at = typing.cursor;
                    typing.text.insert_str(at, &pasted);
                    typing.cursor = at + pasted.len();
                }
            }
            _ => {
                if command {
                    return;
                }
                if let Some(typed) = event.keystroke.key_char.as_ref() {
                    let at = typing.cursor;
                    typing.text.insert_str(at, typed);
                    typing.cursor = at + typed.len();
                }
            }
        }
        cx.notify();
    }

    /// Writes one value into the file — or does not, and says why.
    ///
    /// The form never decides whether an edit is allowed: the candidate
    /// document goes through `Config::parse`, the very function the
    /// supervisor loads the file with, and only a document that parses
    /// is written to disk.
    fn put(&mut self, at: &Spot, value: Option<toml_edit::Item>) {
        let Some(sheet) = &self.sheet else { return };
        let mut document = sheet.document.clone();
        {
            let table = match table_for(&mut document, &at.section) {
                Some(table) => table,
                None => {
                    self.said = Some(format!("{} is not in the file", at.section.title()));
                    return;
                }
            };
            match (at.item, value) {
                (None, Some(value)) => {
                    table.insert(&at.key, value);
                }
                (None, None) => {
                    table.remove(&at.key);
                }
                (Some(index), value) => {
                    let mut array = table
                        .get(&at.key)
                        .and_then(|item| item.as_array())
                        .cloned()
                        .unwrap_or_default();
                    match value {
                        Some(value) => {
                            let text = value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string());
                            match index < array.len() {
                                true => {
                                    array.replace(index, text);
                                }
                                false => array.push(text),
                            }
                        }
                        None => {
                            if index < array.len() {
                                array.remove(index);
                            }
                        }
                    }
                    table.insert(&at.key, toml_edit::value(array));
                }
            }
        }
        self.commit(document);
    }

    fn drop_item(&mut self, at: &Spot) {
        self.put(at, None);
    }

    /// Checks a candidate document with the parser, and writes it only
    /// if the parser takes it.
    fn commit(&mut self, document: toml_edit::DocumentMut) {
        let Some(sheet) = &mut self.sheet else { return };
        // Somebody may have been editing the same file in an editor
        // since it was read. Their work is not this window's to
        // overwrite.
        if let Ok(now) = std::fs::read_to_string(&sheet.path) {
            if now != sheet.text {
                sheet.refused = Some(
                    "the file changed on disk since this form read it. Re-read it, then make \
                     the change again."
                        .to_owned(),
                );
                self.said = Some("not saved: the file changed on disk".to_owned());
                return;
            }
        }
        let text = document.to_string();
        match crate::config::Config::parse(&sheet.path, &text) {
            Ok(_) => match std::fs::write(&sheet.path, &text) {
                Ok(()) => {
                    sheet.document = document;
                    sheet.text = text;
                    sheet.refused = None;
                    let path = sheet.path.clone();
                    self.said = Some(format!("saved {}", tilde(&path.display().to_string())));
                }
                Err(error) => {
                    self.said = Some(format!("unable to write: {error}"));
                }
            },
            Err(error) => {
                sheet.refused = Some(format!("{error:#}"));
                self.said = Some("not saved: the file would not load".to_owned());
            }
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
                format!("revealed {}", tilde(&file.display().to_string()))
            }
            Ok(status) => format!("the Finder refused: {status}"),
            Err(error) => format!("unable to ask the Finder: {error}"),
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
                self.sides = None;
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

    /// Opens a pane the way a reader would find it: with something in
    /// it. Used when a window opens straight onto one pane.
    fn settle(&mut self, pane: Pane) {
        match pane {
            Pane::Groups => {
                if self.selected.is_none() {
                    self.selected = self.report.as_ref().and_then(|report| {
                        report.groups.iter().find_map(|group| {
                            group
                                .sessions
                                .first()
                                .map(|session| (group.name.clone(), session.session.clone()))
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

/// Asking the window server for the rectangle this window occupies. A
/// process may photograph its own windows without the screen-recording
/// permission a capture of the whole display would need, which is what
/// makes the screenshots in the design document possible at all.
#[cfg(target_os = "macos")]
mod camera {
    use std::ffi::c_void;

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

    pub fn grab(x: f64, y: f64, width: f64, height: f64) -> Result<image::RgbaImage> {
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
            image::RgbaImage::from_raw(width as u32, height as u32, pixels)
                .ok_or_else(|| anyhow!("the frame did not fit its own dimensions"))
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod camera {
    use anyhow::{anyhow, Result};

    pub fn grab(_x: f64, _y: f64, _w: f64, _h: f64) -> Result<image::RgbaImage> {
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

/// What a field holds, as the schema says.
enum Holds {
    Switch,
    List,
    Line,
}

/// Reads the type out of a schema field, through the `["string","null"]`
/// spelling an optional field gets.
fn holds(field: &serde_json::Value) -> Holds {
    let named = |name: &str| match &field["type"] {
        serde_json::Value::String(only) => only == name,
        serde_json::Value::Array(any) => any.iter().any(|kind| kind == name),
        _ => false,
    };
    if named("boolean") {
        return Holds::Switch;
    }
    if named("array") {
        return Holds::List;
    }
    Holds::Line
}

/// Keys the form does not show: the retired spellings kept only so a
/// file that uses them gets an answer, and the sections that have a
/// place of their own in the sidebar.
const SILENT: &[&str] = &[
    "groups",
    "defaults",
    "advanced",
    "disabled",
    "alerts",
    "peering-experimental",
];

/// The table one section lives in, made if the file has not got it yet.
fn table_for<'a>(
    document: &'a mut toml_edit::DocumentMut,
    section: &Section,
) -> Option<&'a mut toml_edit::Table> {
    match section {
        Section::Settings => Some(document.as_table_mut()),
        Section::Defaults => document
            .entry("defaults")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut(),
        Section::Group(name) => document
            .entry("groups")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()?
            .entry(name)
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut(),
    }
}

/// The first sentence of a doc comment: enough to say what a field is
/// for, without turning a form into a manual.
fn first_sentence(about: &str) -> String {
    let about = about.replace('\n', " ");
    match about.split_once(". ") {
        Some((first, _)) => format!("{first}."),
        None => about,
    }
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

/// The end of a path: the last `keep` parts, with a mark where the rest
/// was dropped. A window cannot show a long path and a person does not
/// need the whole of one twice on the same screen.
fn tail(path: &str, keep: usize) -> String {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() <= keep {
        return path.to_owned();
    }
    format!("…/{}", parts[parts.len() - keep..].join("/"))
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

    /// A root either names a directory this process can open or a
    /// machine it can only talk to, and the window says different things
    /// about the two.
    #[test]
    fn a_root_is_read_as_a_path_or_as_a_machine() {
        assert_eq!(on_this_machine("fny:~/code"), None);
        assert_eq!(on_this_machine("/tmp/a"), Some(PathBuf::from("/tmp/a")));
        // A colon inside a local path is a character, not a host.
        assert_eq!(on_this_machine("/tmp/a:b"), Some(PathBuf::from("/tmp/a:b")));
        let home = std::env::var("HOME").expect("a home directory");
        assert_eq!(on_this_machine("~/x"), Some(PathBuf::from(home).join("x")));
    }

    /// An edit through the form is an edit to the file the person wrote:
    /// their comments and their order survive it, and the parser — the
    /// same function the supervisor loads the file with — is what says
    /// whether it may be written at all.
    #[test]
    fn an_edit_keeps_the_file_a_person_wrote_and_the_parser_has_the_last_word() {
        let path = std::path::Path::new("config.toml");
        let text = "# the fleet\n\
                    [defaults]\n\
                    mode = \"two-way-conflict\"  # both ways\n\
                    \n\
                    [groups.notes]\n\
                    alpha = \"/tmp/a\"\n\
                    betas = [\"/tmp/b\"]\n";
        let mut document: toml_edit::DocumentMut = text.parse().expect("the file parses");

        let table = table_for(&mut document, &Section::Group("notes".to_owned()))
            .expect("the group is in the file");
        table.insert("mode", toml_edit::value("one-way-alpha"));
        let written = document.to_string();
        assert!(written.contains("# the fleet"), "{written}");
        assert!(written.contains("# both ways"), "{written}");
        assert!(written.contains("mode = \"one-way-alpha\""), "{written}");
        crate::config::Config::parse(path, &written).expect("the parser takes it");

        // A key the parser does not know is refused, and the form is not
        // the thing that decided so.
        let table = table_for(&mut document, &Section::Group("notes".to_owned())).unwrap();
        table.insert("mdoe", toml_edit::value("two-way-conflict"));
        let complaint = crate::config::Config::parse(path, &document.to_string())
            .expect_err("the parser refuses it");
        assert!(format!("{complaint:#}").contains("mdoe"), "{complaint:#}");
    }

    /// A section the file has not got yet is made when something is
    /// written into it, and not before.
    #[test]
    fn a_missing_section_is_made_only_when_it_is_written_to() {
        let mut document: toml_edit::DocumentMut =
            "[groups.a]\nalpha = \"/tmp/a\"\n".parse().unwrap();
        assert!(!document.to_string().contains("[defaults]"));
        let table = table_for(&mut document, &Section::Defaults).expect("a table is made");
        table.insert("interval", toml_edit::value(30));
        assert!(document.to_string().contains("[defaults]"));
        assert!(document.to_string().contains("interval = 30"));
    }

    /// A path that will not fit keeps its end, which is the part that
    /// tells two sides of a conflict apart.
    #[test]
    fn a_long_path_keeps_the_end_that_matters() {
        assert_eq!(tail("/a/b/c/d/e", 3), "…/c/d/e");
        assert_eq!(tail("/a/b", 3), "/a/b");
        assert_eq!(tail("fny:~/code/src", 3), "fny:~/code/src");
    }

    /// Both answers to "how big": the one a person compares at a glance
    /// and the one they would see in a terminal.
    #[test]
    fn a_size_is_readable_and_exact() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(2_048), "2.0 KiB · 2,048 bytes");
        assert_eq!(human_size(5_242_880), "5.0 MiB · 5,242,880 bytes");
    }

    /// One pass over a file answers both questions, and the digest is
    /// the one the scanner records — so a side that matches here matches
    /// for the engine too.
    #[test]
    fn one_read_says_whether_it_is_text_and_what_it_hashes_to() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("a.txt");
        std::fs::write(&text, b"hello\n").unwrap();
        let (binary, digest) = read_through(&text, 6).unwrap();
        assert!(!binary);
        assert_eq!(
            digest.unwrap(),
            blake3::hash(b"hello\n").to_hex().to_string()
        );

        let blob = dir.path().join("a.bin");
        std::fs::write(&blob, b"\x7fELF\0\0\0").unwrap();
        let (binary, digest) = read_through(&blob, 7).unwrap();
        assert!(binary);
        assert!(digest.is_some(), "a small binary is still hashed");
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
