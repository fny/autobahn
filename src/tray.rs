//! The menu bar app: `autobahn tray`.
//!
//! A thin view over the status report. The icon is the summary — green
//! when every session is synchronized, yellow when any is in conflict, red
//! when any is halted or unreachable, grey when nothing is running — and
//! the menu is the detail: each group, each destination with its state,
//! and for each conflict the three ways to settle it, which run the same
//! `resolve` command a terminal would. Nothing here touches state
//! directly; everything goes through the same functions the CLI uses, so
//! the app cannot disagree with `status` or get a resolution wrong.
//!
//! The report is polled every few seconds. It reads a handful of small
//! files, so polling costs nothing, and it avoids inventing a push
//! protocol for the one client. A *transition* — a session entering
//! conflict, halting, or going unreachable, and a return to synchronized —
//! raises a desktop notification; the icon colour is the steady-state
//! signal.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use muda::{IsMenuItem, Menu, MenuEvent, MenuItem, MenuItemKind, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::event_loop::{ControlFlow, EventLoop, EventLoopProxy};

/// What wakes the event loop: a poll timer, or a menu choice.
#[derive(Debug)]
enum Wake {
    Tick,
    Menu(muda::MenuId),
}

use crate::config::SessionPlan;
use crate::supervisor::{status_report, StatusReport};

/// How often the report is refreshed.
const POLL: Duration = Duration::from_secs(3);

/// What a menu item does when chosen.
#[derive(Clone, Debug)]
enum Action {
    Resolve {
        group: String,
        path: String,
        keep: String,
    },
    Diff {
        group: String,
        path: String,
        host: String,
    },
    OpenLog,
    ServiceStart,
    ServiceStop,
    ServiceRestart,
    Refresh,
    Quit,
}

/// The menu, kept as live handles so it can be edited in place.
///
/// The menu is never replaced once built. Replacing it dismisses it if it
/// is open — a menu that vanishes under the pointer at the moment
/// something changes is the worst possible time — whereas AppKit updates
/// an open menu's items live. So the summary and every session line are
/// handles whose text is set, conflict submenus are added and removed
/// within their session's submenu, and the service items are fixed
/// entries that are enabled or disabled. Only a change to the *shape* of
/// the configuration — a group or destination added or removed — rebuilds
/// it, and that is rare and never mid-glance.
struct MenuModel {
    menu: Menu,
    summary: MenuItem,
    /// Present only while there is an error to show; a blank item would
    /// be an empty row.
    error: Option<MenuItem>,
    groups: Vec<GroupItems>,
    service_start: MenuItem,
    service_stop: MenuItem,
    service_restart: MenuItem,
    /// The shape the model was built for: group names and their
    /// destinations. A report with a different shape needs a rebuild.
    shape: Vec<(String, Vec<String>)>,
}

struct GroupItems {
    submenu: Submenu,
    sessions: Vec<SessionItems>,
}

struct SessionItems {
    line: MenuItem,
    /// The session's error, present only while it has one.
    detail: Option<MenuItem>,
    /// Conflict submenus by path, in menu order.
    conflicts: Vec<(String, Submenu)>,
}

/// The icon's overall colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Health {
    Idle,
    Good,
    Attention,
    Bad,
}

/// Runs the tray until quit.
pub fn run(config: Option<PathBuf>, state_root: PathBuf) -> Result<()> {
    let mut builder = EventLoop::<Wake>::with_user_event();
    // A menu bar app has no dock icon and no windows.
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        builder.with_activation_policy(ActivationPolicy::Accessory);
    }
    let event_loop = builder.build().context("unable to create the event loop")?;

    // A windowless loop does not wake itself on a timer reliably, so it
    // is woken explicitly: a thread ticks it for polls, and menu choices
    // are forwarded as they happen rather than found on the next tick.
    let ticker: EventLoopProxy<Wake> = event_loop.create_proxy();
    std::thread::spawn(move || loop {
        std::thread::sleep(POLL);
        if let Err(error) = ticker.send_event(Wake::Tick) {
            eprintln!("tick: {error}");
            return;
        }
    });
    let clicks: EventLoopProxy<Wake> = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = clicks.send_event(Wake::Menu(event.id));
    }));

    run_loop(event_loop, config, state_root)
}

fn run_loop(
    event_loop: EventLoop<Wake>,
    config: Option<PathBuf>,
    state_root: PathBuf,
) -> Result<()> {
    let mut app = App {
        config,
        state_root,
        tray: None,
        actions: HashMap::new(),
        last_states: HashMap::new(),
        health: Health::Idle,
        report: None,
        last_error: None,
        model: None,
    };
    event_loop
        .run_app(&mut app)
        .context("the event loop failed")?;
    Ok(())
}

struct App {
    config: Option<PathBuf>,
    state_root: PathBuf,
    tray: Option<TrayIcon>,
    /// Menu item ids to what choosing them does.
    actions: HashMap<muda::MenuId, Action>,
    /// Each session's state at the last poll, keyed by group@host, for
    /// noticing transitions.
    last_states: HashMap<String, String>,
    health: Health,
    report: Option<StatusReport>,
    /// The last action's failure, shown at the top of the menu until an
    /// action succeeds — a notification can be missed.
    last_error: Option<String>,
    /// The live menu.
    model: Option<MenuModel>,
}

impl winit::application::ApplicationHandler<Wake> for App {
    fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, wake: Wake) {
        if std::env::var_os("AUTOBAHN_TRAY_DEBUG").is_some() {
            eprintln!("wake: {wake:?}");
        }
        match wake {
            Wake::Tick => self.refresh(),
            Wake::Menu(id) => {
                if let Some(action) = self.actions.get(&id).cloned() {
                    match action {
                        Action::Quit => event_loop.exit(),
                        other => {
                            self.perform(other);
                            self.refresh();
                        }
                    }
                }
            }
        }
    }

    fn resumed(&mut self, _: &winit::event_loop::ActiveEventLoop) {
        // The tray must be created once the event loop runs (a macOS
        // requirement), and only once.
        if self.tray.is_none() {
            let menu = Menu::new();
            let tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_icon(icon(Health::Idle))
                .with_tooltip("autobahn")
                .build()
                .expect("unable to create the tray icon");
            self.tray = Some(tray);
            self.refresh();
        }
    }

    fn window_event(
        &mut self,
        _: &winit::event_loop::ActiveEventLoop,
        _: winit::window::WindowId,
        _: winit::event::WindowEvent,
    ) {
    }

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        // Everything arrives as a user event; between them, sleep.
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}

impl App {
    /// Rebuilds the report, the menu, and the icon; raises notifications
    /// for transitions.
    fn refresh(&mut self) {
        let report = match self.build_report() {
            Ok(report) => report,
            Err(error) => {
                self.set_menu_error(&format!("{error:#}"));
                return;
            }
        };
        self.notify_transitions(&report);
        let health = health_of(&report);
        if health != self.health {
            if let Some(tray) = &self.tray {
                let _ = tray.set_icon(Some(icon(health)));
            }
            self.health = health;
        }
        let shape = shape_of(&report);
        let rebuild = match &self.model {
            Some(model) => model.shape != shape,
            None => true,
        };
        if rebuild {
            self.build_menu(&report, shape);
        }
        self.update_menu(&report);
        self.report = Some(report);
    }

    fn build_report(&self) -> Result<StatusReport> {
        let path = match &self.config {
            Some(path) => path.clone(),
            None => crate::paths::default_config_path()?,
        };
        let plans = crate::config::Config::load(&path)?.plans()?;
        let selected: Vec<&SessionPlan> = plans.iter().collect();
        Ok(status_report(&selected, &self.state_root))
    }

    fn set_menu_error(&mut self, message: &str) {
        // With no report there is no model; a bare menu carries the
        // message and a way out.
        self.model = None;
        let menu = Menu::new();
        let _ = menu.append(&MenuItem::new(format!("autobahn: {message}"), false, None));
        let _ = menu.append(&PredefinedMenuItem::separator());
        self.actions.clear();
        let quit = MenuItem::new("Quit", true, None);
        self.actions.insert(quit.id().clone(), Action::Quit);
        let _ = menu.append(&quit);
        if let Some(tray) = &self.tray {
            tray.set_menu(Some(Box::new(menu)));
        }
    }

    /// Builds the menu for a configuration shape. Called once, and again
    /// only when the shape changes.
    fn build_menu(&mut self, report: &StatusReport, shape: Vec<(String, Vec<String>)>) {
        self.actions.clear();
        let menu = Menu::new();
        let summary = MenuItem::new("", false, None);
        let _ = menu.append(&summary);
        let _ = menu.append(&PredefinedMenuItem::separator());

        let mut groups = Vec::new();
        for group in &report.groups {
            let submenu = Submenu::new(format!("{}  ({})", group.alpha, group.name), true);
            let mut sessions = Vec::new();
            for _ in &group.sessions {
                let line = MenuItem::new("", false, None);
                let _ = submenu.append(&line);
                sessions.push(SessionItems {
                    line,
                    detail: None,
                    conflicts: Vec::new(),
                });
            }
            let _ = menu.append(&submenu);
            groups.push(GroupItems { submenu, sessions });
        }
        let _ = menu.append(&PredefinedMenuItem::separator());

        let mut fixed = |label: &str, action: Action| -> MenuItem {
            let item = MenuItem::new(label, true, None);
            self.actions.insert(item.id().clone(), action);
            let _ = menu.append(&item);
            item
        };
        let service_start = fixed("Start service", Action::ServiceStart);
        let service_stop = fixed("Stop service", Action::ServiceStop);
        let service_restart = fixed("Restart service", Action::ServiceRestart);
        fixed("Open log", Action::OpenLog);
        fixed("Refresh", Action::Refresh);
        let _ = menu.append(&PredefinedMenuItem::separator());
        fixed("Quit", Action::Quit);

        if let Some(tray) = &self.tray {
            tray.set_menu(Some(Box::new(menu.clone())));
        }
        self.model = Some(MenuModel {
            menu,
            summary,
            error: None,
            groups,
            service_start,
            service_stop,
            service_restart,
            shape,
        });
    }

    /// Brings the menu's text and conflict entries up to date, in place.
    fn update_menu(&mut self, report: &StatusReport) {
        let Some(model) = self.model.as_mut() else {
            return;
        };

        let sessions: Vec<_> = report
            .groups
            .iter()
            .flat_map(|group| group.sessions.iter())
            .collect();
        let count = |state: &str| sessions.iter().filter(|s| s.state == state).count();
        let mut parts = Vec::new();
        if !report.supervisor_running {
            parts.push(
                match report.service.as_str() {
                    "stopped" => "Not running (service stopped)",
                    "not-installed" => "Not running (no service installed)",
                    _ => "Not running",
                }
                .to_owned(),
            );
        }
        parts.push(format!("{} synchronized", count("synchronized")));
        for (state, word) in [
            ("conflicts", "in conflict"),
            ("halted", "halted"),
            ("unreachable", "unreachable"),
            ("error", "failing"),
            ("problems", "with problems"),
        ] {
            let n = count(state);
            if n > 0 {
                parts.push(format!("{n} {word}"));
            }
        }
        let summary = parts.join(", ");
        model.summary.set_text(&summary);
        let error_text = self.last_error.as_deref().map(|e| format!("⚠ {e}"));
        set_optional(&model.menu, &model.summary, &mut model.error, error_text);
        if let Some(tray) = &self.tray {
            let _ = tray.set_tooltip(Some(format!("autobahn — {summary}")));
        }

        for (group, items) in report.groups.iter().zip(model.groups.iter_mut()) {
            for (session, entry) in group.sessions.iter().zip(items.sessions.iter_mut()) {
                let age = session
                    .age_seconds
                    .map(format_age)
                    .unwrap_or_else(|| "never run".to_owned());
                entry
                    .line
                    .set_text(format!("{}  —  {}, {}", session.host, session.state, age));
                let detail_text = session
                    .error
                    .as_deref()
                    .map(|error| format!("      {}", error.rsplit(": ").next().unwrap_or(error)));
                set_optional(&items.submenu, &entry.line, &mut entry.detail, detail_text);

                // Conflicts: remove the ones that are gone, add the ones
                // that are new, leave the rest untouched.
                let wanted: Vec<&str> = session.conflicts.iter().map(|c| c.path.as_str()).collect();
                entry.conflicts.retain(|(path, submenu)| {
                    if wanted.contains(&path.as_str()) {
                        true
                    } else {
                        let _ = items.submenu.remove(submenu);
                        false
                    }
                });
                for conflict in &session.conflicts {
                    if entry
                        .conflicts
                        .iter()
                        .any(|(path, _)| path == &conflict.path)
                    {
                        continue;
                    }
                    let item = Submenu::new(format!("      ⚠ {}", conflict.path), true);
                    let mut add = |label: String, action: Action| {
                        let choice = MenuItem::new(label, true, None);
                        self.actions.insert(choice.id().clone(), action);
                        let _ = item.append(&choice);
                    };
                    add(
                        "Show diff".into(),
                        Action::Diff {
                            group: group.name.clone(),
                            path: conflict.path.clone(),
                            host: session.host.clone(),
                        },
                    );
                    let _ = item.append(&PredefinedMenuItem::separator());
                    for (label, keep) in [
                        ("Keep alpha's version".to_owned(), "alpha".to_owned()),
                        (
                            format!("Keep {}'s version", session.host),
                            session.host.clone(),
                        ),
                        ("Keep both".to_owned(), "both".to_owned()),
                    ] {
                        add(
                            label,
                            Action::Resolve {
                                group: group.name.clone(),
                                path: conflict.path.clone(),
                                keep,
                            },
                        );
                    }
                    // Placed after this session's line, its detail if
                    // any, and the conflicts it already has — before the
                    // next session.
                    let position = items
                        .submenu
                        .items()
                        .iter()
                        .position(|k| k.id() == entry.line.id())
                        .map(|p| {
                            p + 1 + usize::from(entry.detail.is_some()) + entry.conflicts.len()
                        })
                        .unwrap_or(0);
                    let _ = items.submenu.insert(&item, position);
                    entry.conflicts.push((conflict.path.clone(), item));
                }
            }
        }

        let (start, stop, restart) = match report.service.as_str() {
            "running" => (false, true, true),
            "stopped" => (true, false, false),
            _ => (false, false, false),
        };
        model.service_start.set_enabled(start);
        model.service_stop.set_enabled(stop);
        model.service_restart.set_enabled(restart);
    }

    /// Raises a desktop notification for each session whose state changed
    /// to or from something that needs attention.
    fn notify_transitions(&mut self, report: &StatusReport) {
        let first = self.last_states.is_empty();
        for group in &report.groups {
            for session in &group.sessions {
                let key = format!("{}@{}", group.name, session.host);
                let previous = self.last_states.insert(key, session.state.clone());
                if first {
                    continue; // the initial poll establishes a baseline, silently
                }
                let Some(previous) = previous else { continue };
                if previous == session.state {
                    continue;
                }
                let attention =
                    |state: &str| matches!(state, "conflicts" | "halted" | "unreachable" | "error");
                let (title, body) = if attention(&session.state) {
                    (
                        format!("{} — {}", group.alpha, session.state),
                        match session.state.as_str() {
                            "conflicts" => format!(
                                "{}: {} conflict(s), first {}",
                                session.host,
                                session.conflicts.len(),
                                session
                                    .conflicts
                                    .first()
                                    .map(|c| c.path.as_str())
                                    .unwrap_or("")
                            ),
                            _ => format!(
                                "{}: {}",
                                session.host,
                                session
                                    .error
                                    .as_deref()
                                    .map(|e| e.rsplit(": ").next().unwrap_or(e))
                                    .unwrap_or(&session.state)
                            ),
                        },
                    )
                } else if attention(&previous) {
                    (
                        format!("{} — synchronized again", group.alpha),
                        format!("{} recovered", session.host),
                    )
                } else {
                    continue;
                };
                notify(&title, &body);
            }
        }
    }

    /// Performs a chosen action by running the CLI — the same command a
    /// terminal would, so the app cannot resolve differently than the
    /// user could.
    fn perform(&mut self, action: Action) {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("autobahn"));
        // --config and --state-root belong to the subcommand, so they go
        // after it.
        let mut command = std::process::Command::new(&exe);
        let mut common: Vec<std::ffi::OsString> = Vec::new();
        if let Some(config) = &self.config {
            common.push("--config".into());
            common.push(config.clone().into());
        }
        common.push("--state-root".into());
        common.push(self.state_root.clone().into());
        let outcome: Result<()> = match action {
            Action::Resolve { group, path, keep } => {
                command.args(["resolve", &group, &path, "--keep", &keep]);
                command.args(&common);
                run_quiet(command)
            }
            Action::Diff { group, path, host } => {
                // The diff is written to a file and opened with whatever
                // the desktop opens text with — a menu cannot show one.
                command.args(["diff", &group, &path, "--host", &host]);
                command.args(&common);
                match command.output() {
                    Ok(output) => {
                        let file = std::env::temp_dir()
                            .join(format!("autobahn-diff-{}.diff", path.replace('/', "_")));
                        let _ = std::fs::write(&file, &output.stdout);
                        open_path(&file)
                    }
                    Err(error) => Err(error.into()),
                }
            }
            Action::OpenLog => crate::service::log_path().and_then(|log| open_path(&log)),
            Action::ServiceStart => crate::service::start(),
            Action::ServiceStop => crate::service::stop(),
            Action::ServiceRestart => crate::service::restart(),
            Action::Refresh | Action::Quit => Ok(()),
        };
        match outcome {
            Ok(()) => self.last_error = None,
            Err(error) => {
                let message = format!("{error:#}");
                notify("autobahn", &message);
                self.last_error = Some(message);
            }
        }
    }
}

/// Raises a desktop notification, without ever blocking the event loop.
///
/// On macOS the notification is posted by a spawned `osascript`: the
/// native notification API needs a bundled application, and from a plain
/// binary the notify-rust call never returns — it hung the loop for good
/// when this was first tried. A separate process cannot do that. On Linux
/// notify-rust speaks D-Bus and returns; it is still detached, since the
/// bus can stall too.
fn notify(title: &str, body: &str) {
    if std::env::var_os("AUTOBAHN_TRAY_DEBUG").is_some() {
        eprintln!("notify: {title} — {body}");
    }
    let title = title.to_owned();
    let body = body.to_owned();
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        {
            let escape = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
            let script = format!(
                "display notification \"{}\" with title \"{}\"",
                escape(&body),
                escape(&title)
            );
            let _ = std::process::Command::new("osascript")
                .args(["-e", &script])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = notify_rust::Notification::new()
                .summary(&title)
                .body(&body)
                .appname("autobahn")
                .show();
        }
    });
}

fn run_quiet(mut command: std::process::Command) -> Result<()> {
    let output = command.output().context("unable to run autobahn")?;
    if output.status.success() {
        Ok(())
    } else {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim())
    }
}

/// Opens a file with the desktop's default handler.
fn open_path(path: &std::path::Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";
    std::process::Command::new(opener)
        .arg(path)
        .status()
        .with_context(|| format!("unable to open {}", path.display()))?;
    Ok(())
}

/// What `Menu` and `Submenu` have in common as containers of items —
/// muda gives them the same methods but no shared trait.
trait Container {
    fn items(&self) -> Vec<MenuItemKind>;
    fn insert(&self, item: &dyn IsMenuItem, position: usize) -> muda::Result<()>;
    fn remove(&self, item: &dyn IsMenuItem) -> muda::Result<()>;
}

impl Container for Menu {
    fn items(&self) -> Vec<MenuItemKind> {
        Menu::items(self)
    }
    fn insert(&self, item: &dyn IsMenuItem, position: usize) -> muda::Result<()> {
        Menu::insert(self, item, position)
    }
    fn remove(&self, item: &dyn IsMenuItem) -> muda::Result<()> {
        Menu::remove(self, item)
    }
}

impl Container for Submenu {
    fn items(&self) -> Vec<MenuItemKind> {
        Submenu::items(self)
    }
    fn insert(&self, item: &dyn IsMenuItem, position: usize) -> muda::Result<()> {
        Submenu::insert(self, item, position)
    }
    fn remove(&self, item: &dyn IsMenuItem) -> muda::Result<()> {
        Submenu::remove(self, item)
    }
}

/// Keeps an optional item — one that exists only while it has text —
/// in step with `text`, directly after `anchor`, editing the text in
/// place when the item already exists.
fn set_optional<M: Container>(
    parent: &M,
    anchor: &MenuItem,
    slot: &mut Option<MenuItem>,
    text: Option<String>,
) {
    match (slot.as_ref(), text) {
        (Some(item), Some(text)) => item.set_text(text),
        (Some(item), None) => {
            let _ = parent.remove(item);
            *slot = None;
        }
        (None, Some(text)) => {
            let item = MenuItem::new(text, false, None);
            let position = parent
                .items()
                .iter()
                .position(|k| k.id() == anchor.id())
                .map(|p| p + 1)
                .unwrap_or(0);
            let _ = parent.insert(&item, position);
            *slot = Some(item);
        }
        (None, None) => {}
    }
}

/// The configuration's shape: group names and their destinations.
fn shape_of(report: &StatusReport) -> Vec<(String, Vec<String>)> {
    report
        .groups
        .iter()
        .map(|group| {
            (
                group.name.clone(),
                group.sessions.iter().map(|s| s.host.clone()).collect(),
            )
        })
        .collect()
}

fn health_of(report: &StatusReport) -> Health {
    if !report.supervisor_running {
        return Health::Idle;
    }
    let states = report
        .groups
        .iter()
        .flat_map(|group| group.sessions.iter().map(|s| s.state.as_str()));
    let mut health = Health::Good;
    for state in states {
        match state {
            "halted" | "unreachable" | "error" => return Health::Bad,
            "conflicts" | "problems" => health = Health::Attention,
            _ => {}
        }
    }
    health
}

/// A filled circle in the health's colour, drawn in code so there is no
/// asset to ship or lose.
fn icon(health: Health) -> Icon {
    const SIZE: u32 = 22;
    let (r, g, b) = match health {
        Health::Idle => (150u8, 150u8, 150u8),
        Health::Good => (52, 199, 89),
        Health::Attention => (255, 204, 0),
        Health::Bad => (255, 69, 58),
    };
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let centre = (SIZE as f32 - 1.0) / 2.0;
    let radius = SIZE as f32 / 2.0 - 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - centre;
            let dy = y as f32 - centre;
            let distance = (dx * dx + dy * dy).sqrt();
            // A one-pixel soft edge, so the circle is not jagged.
            let alpha = ((radius - distance + 0.5).clamp(0.0, 1.0) * 255.0) as u8;
            rgba.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("a valid icon")
}

fn format_age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else {
        format!("{}h ago", seconds / 3600)
    }
}
