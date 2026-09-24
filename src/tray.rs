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
    /// A queued action finished, so the menu and the icon are stale.
    Done,
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
    /// The health its dot was last drawn for. Redrawing an unchanged dot
    /// every poll would rebuild an image a few times a second for nothing.
    health: Option<Health>,
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
    // The alerter's plan comes from the configuration, so the tray holds
    // exactly the timing the hook does. A configuration that cannot be
    // read yet — the tray may start before one exists — gets the built-in
    // plan, which is the same thing minus the hook.
    let plan = {
        let path = match &config {
            Some(path) => path.clone(),
            None => crate::paths::default_config_path()?,
        };
        crate::config::Config::load(&path)
            .and_then(|config| config.alert_plan())
            .or_else(|_| crate::config::Config::default().alert_plan())?
    };
    let hook_configured = plan.on_alert.is_some();
    // Actions run on a worker thread, one after another. Before this
    // they ran on the event loop, so a second menu choice was lost while
    // the first was still running and a slow resolve froze the menu bar
    // for as long as it took.
    let (queue, jobs) = std::sync::mpsc::channel::<Action>();
    let queued = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let failure: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    {
        let waker: EventLoopProxy<Wake> = event_loop.create_proxy();
        let queued = queued.clone();
        let failure = failure.clone();
        let config = config.clone();
        let state_root = state_root.clone();
        std::thread::spawn(move || {
            for action in jobs {
                let outcome = run_action(action, config.as_ref(), &state_root);
                queued.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                if let Err(error) = outcome {
                    // Kept for the loop to raise: a notification belongs
                    // on the main thread.
                    *failure.lock().unwrap_or_else(|error| error.into_inner()) =
                        Some(format!("{error:#}"));
                }
                let _ = waker.send_event(Wake::Done);
            }
        });
    }

    let mut app = App {
        config,
        state_root,
        queue,
        queued,
        failure,
        tray: None,
        actions: HashMap::new(),
        alerter: crate::alerts::Alerter::new(plan),
        hook_configured,
        ink: menu_bar_ink(None),
        health: Health::Idle,
        report: None,
        last_error: None,
        last_notice: None,
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
    /// Decides when a notification is due, from what the report shows.
    /// The same policy the supervisor's hook uses — confirmation, only
    /// growth is news, a cascade gathered into one, trouble that comes and
    /// goes reported once — so the tray cannot say something the hook
    /// would not. Before this it announced every transition, recoveries
    /// included, with no hold time and no coalescing, and was a second
    /// source of exactly the storm the hook's rules exist to prevent.
    alerter: crate::alerts::Alerter,
    /// Whether `on_alert` is configured. When it is, the hook is the one
    /// place notifications come from and the tray stays quiet: two
    /// sources with identical rules still means everything twice.
    hook_configured: bool,
    health: Health,
    /// The menu bar's ink at the last poll. The sign is drawn in it, so a
    /// switch between light and dark redraws the icon on the next poll.
    ink: Ink,
    report: Option<StatusReport>,
    /// Chosen actions, handed to the worker thread in the order they
    /// were chosen.
    queue: std::sync::mpsc::Sender<Action>,
    /// How many are waiting or running, for the menu to report.
    queued: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// What the worker's last failure was, for the loop to raise.
    failure: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// The last action's failure, shown at the top of the menu until an
    /// action succeeds — a notification can be missed.
    last_error: Option<String>,
    /// The refused configuration edit last announced, so each is
    /// announced once.
    last_notice: Option<crate::supervisor::reload::Notice>,
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
            Wake::Done => {
                match self
                    .failure
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                {
                    Some(message) => {
                        notify_with("autobahn", &message, crate::icon::ensure(&self.state_root));
                        self.last_error = Some(message);
                    }
                    None => self.last_error = None,
                }
                self.refresh();
            }
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
                .with_icon(icon(Health::Idle, menu_bar_ink(None)))
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
        self.notify(&report);
        let health = health_of(&report);
        let ink = menu_bar_ink(self.tray.as_ref());
        if health != self.health || ink != self.ink {
            if let Some(tray) = &self.tray {
                let _ = tray.set_icon(Some(icon(health, ink)));
            }
            self.health = health;
            self.ink = ink;
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
            let submenu = Submenu::new(group_label(group), true);
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
            groups.push(GroupItems {
                submenu,
                sessions,
                health: None,
            });
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
        match self.queued.load(std::sync::atomic::Ordering::SeqCst) {
            0 => {}
            one => parts.push(format!("{one} queued")),
        }
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
        if report.config_notice.is_some() {
            parts.push("configuration refused".to_owned());
        }
        if report.supervisor_mismatch.is_some() {
            parts.push("restart needed".to_owned());
        }
        parts.push(format!("{} synchronized", count("synchronized")));
        for (state, word) in [
            ("conflicts", "in conflict"),
            ("halted", "halted"),
            ("unreachable", "unreachable"),
            ("errored", "failing"),
            ("blocked", "blocked"),
        ] {
            let n = count(state);
            if n > 0 {
                parts.push(format!("{n} {word}"));
            }
        }
        let summary = parts.join(", ");
        model.summary.set_text(&summary);
        // The refused edit takes the same line, under an action's failure
        // when there is one: both are things the person did.
        let error_text = self
            .last_error
            .as_deref()
            .map(|e| format!("⚠ {e}"))
            .or_else(|| {
                report
                    .config_notice
                    .as_ref()
                    .map(|notice| format!("⚠ configuration refused: {}", notice.message))
            })
            .or_else(|| {
                report
                    .supervisor_mismatch
                    .as_ref()
                    .map(|mismatch| format!("⚠ {mismatch}"))
            });
        set_optional(&model.menu, &model.summary, &mut model.error, error_text);
        if let Some(tray) = &self.tray {
            let _ = tray.set_tooltip(Some(format!("autobahn — {summary}")));
            // The menu bar carries the name of whatever needs attention.
            // One group is named outright; several are the worst one and
            // a count, because the menu bar is not a place for a list.
            let troubled = troubled_groups(report);
            let title = match troubled.split_first() {
                None => None,
                Some((first, [])) => Some(first.clone()),
                Some((first, rest)) => Some(format!("{first} +{}", rest.len())),
            };
            tray.set_title(title.as_deref());
        }

        for (group, items) in report.groups.iter().zip(model.groups.iter_mut()) {
            // The row carries its own state, so the group that needs a
            // person is visible in the menu without opening its submenu.
            items.submenu.set_text(group_label(group));
            let health = group_health(group, report.supervisor_running);
            if items.health != Some(health) {
                items.submenu.set_icon(Some(status_dot(health)));
                items.health = Some(health);
            }
            for (session, entry) in group.sessions.iter().zip(items.sessions.iter_mut()) {
                // A session that is working says so; one that is not is
                // described by how its last cycle ended. The same rule the
                // command line follows, for the same reason: an age that
                // only grows over a stale state reads as stuck.
                let line = match session.progress.as_ref().filter(|progress| {
                    progress.phase.is_working() && progress.working_seconds >= SLOW_PHASE_SECONDS
                }) {
                    Some(progress) => {
                        let mut line = format!(
                            "{}  —  {}, {}",
                            session.host,
                            progress.phase.label(),
                            format_elapsed(progress.seconds)
                        );
                        if let Some(remaining) = progress.remaining_seconds.filter(|left| *left > 0)
                        {
                            use std::fmt::Write;
                            let _ = write!(line, ", about {} left", format_elapsed(remaining));
                        }
                        line
                    }
                    None => {
                        let age = session
                            .age_seconds
                            .map(format_age)
                            .unwrap_or_else(|| "never run".to_owned());
                        format!("{}  —  {}, {}", session.host, session.state, age)
                    }
                };
                entry.line.set_text(line);
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

    /// Shows the alerter what every session is in, and raises a desktop
    /// notification if it says one is due. The report already carries
    /// each session's alerting conditions, decided by the supervisor's
    /// own rule, so nothing here reinterprets a state word.
    fn notify(&mut self, report: &StatusReport) {
        if self.hook_configured {
            return;
        }
        // A refused edit is one event, announced once; the line in the
        // menu stays until the file loads again.
        if report.config_notice != self.last_notice {
            if let Some(notice) = &report.config_notice {
                notify_with(
                    "autobahn",
                    &format!("configuration refused: {}", notice.message),
                    crate::icon::ensure(&self.state_root),
                );
            }
            self.last_notice = report.config_notice.clone();
        }
        let sessions: Vec<crate::alerts::SessionAlerts> = report
            .groups
            .iter()
            .flat_map(|group| {
                group
                    .sessions
                    .iter()
                    .map(move |session| crate::alerts::SessionAlerts {
                        group: group.name.clone(),
                        host: session.host.clone(),
                        alerts: session.alerts.clone(),
                        summary: session.alert_summary.clone(),
                        after: session.alert_after,
                    })
            })
            .collect();
        if let Some(crate::alerts::Fire::Alert {
            summary, detail, ..
        }) = self.alerter.observe(&sessions, std::time::Instant::now())
        {
            notify_with(&summary, &detail, crate::icon::ensure(&self.state_root));
        }
    }

    /// Queues a chosen action. Several choices stack up and run in the
    /// order they were made, so the menu answers at once however long the
    /// work takes.
    fn perform(&mut self, action: Action) {
        // Nothing to run: the refresh that follows is the whole effect.
        if matches!(action, Action::Refresh | Action::Quit) {
            return;
        }
        self.queued
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.queue.send(action).is_err() {
            self.queued
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            self.last_error = Some("the worker thread has stopped".to_owned());
        }
    }
}

/// Runs one action by invoking the CLI — the same command a terminal
/// would, so the app cannot resolve differently than the user could.
fn run_action(
    action: Action,
    config: Option<&PathBuf>,
    state_root: &std::path::Path,
) -> Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("autobahn"));
    // --config and --state-root belong to the subcommand, so they go
    // after it.
    let mut command = std::process::Command::new(&exe);
    let mut common: Vec<std::ffi::OsString> = Vec::new();
    if let Some(config) = config {
        common.push("--config".into());
        common.push(config.clone().into());
    }
    common.push("--state-root".into());
    common.push(state_root.to_path_buf().into());
    match action {
        Action::Resolve { group, path, keep } => {
            // The menu item is the confirmation, and nothing here could
            // answer a prompt.
            command.args(["resolve", &group, &path, "--keep", &keep, "--yes"]);
            command.args(&common);
            run_quiet(command)
        }
        Action::Diff { group, path, host } => {
            // The diff is written to a file and opened with whatever the
            // desktop opens text with — a menu cannot show one.
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
/// Raises a notification, carrying autobahn's own icon where the platform
/// allows one.
///
/// On macOS `osascript` shows Script Editor's icon and offers no way to
/// change it, so a notifier that does is preferred when one is installed
/// and the plain script is the fallback. That is the whole reason the
/// binary carries an image at all.
fn notify_with(title: &str, body: &str, icon: Option<PathBuf>) {
    if std::env::var_os("AUTOBAHN_TRAY_DEBUG").is_some() {
        eprintln!("notify: {title} — {body}");
    }
    let title = title.to_owned();
    let body = body.to_owned();
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        {
            // Inside the app bundle the notification is autobahn's own:
            // macOS takes the icon from the bundle that sent it, which is
            // the whole reason the bundle exists. Outside it, there is no
            // identity to claim and the fallbacks below are the best that
            // an unsigned command line can do.
            if let Some(bundle) = bundle_identifier() {
                if notify_rust::set_application(&bundle).is_ok() {
                    let mut notification = notify_rust::Notification::new();
                    notification.summary(&title).body(&body);
                    let _ = notification.show();
                    return;
                }
            }
            let escape = |text: &str| text.replace('\\', "\\\\").replace('"', "\\\"");
            let script = format!(
                "display notification \"{}\" with title \"{}\"",
                escape(&body),
                escape(&title)
            );
            if let Some(icon) = icon.as_ref().filter(|path| path.exists()) {
                if let Ok(notifier) = which_notifier() {
                    let _ = std::process::Command::new(notifier)
                        .args([
                            "-title", &title, "-message", &body, "-group", "autobahn", "-appIcon",
                        ])
                        .arg(icon)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    return;
                }
            }
            let _ = std::process::Command::new("osascript")
                .args(["-e", &script])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut notification = notify_rust::Notification::new();
            notification.summary(&title).body(&body).appname("autobahn");
            if let Some(icon) = icon.as_ref().filter(|path| path.exists()) {
                notification.icon(&icon.display().to_string());
            }
            let _ = notification.show();
        }
    });
}

/// This process's bundle identifier, when it is running inside one.
///
/// Read from the bundle's own `Info.plist` rather than hardcoded, so a
/// copy someone renamed or re-signed still announces what it actually is.
#[cfg(target_os = "macos")]
fn bundle_identifier() -> Option<String> {
    let executable = std::env::current_exe().ok()?;
    let contents = executable.parent()?.parent()?;
    if !contents.ends_with("Contents") {
        return None;
    }
    let plist = std::fs::read_to_string(contents.join("Info.plist")).ok()?;
    // The identifier follows its key, and the file is small enough that
    // finding it this way beats taking a plist parser as a dependency.
    let after = plist.split("<key>CFBundleIdentifier</key>").nth(1)?;
    let value = after.split("<string>").nth(1)?.split("</string>").next()?;
    Some(value.trim().to_owned())
}

/// The first notifier on `PATH` that takes an icon. Looked up rather than
/// configured: someone who has one has it for everything, and someone who
/// does not gets the plain notification without being asked to install
/// anything.
#[cfg(target_os = "macos")]
fn which_notifier() -> Result<PathBuf, ()> {
    for directory in std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        // A login service inherits a sparse PATH, and Homebrew is where
        // this comes from on the machines that have it.
        .chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from))
    {
        let candidate = directory.join("terminal-notifier");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(())
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

/// The groups that need a person, worst first.
///
/// The coloured dot says *that* something is wrong; this says *which*,
/// beside the icon, without opening the menu. It mirrors the states
/// `health_of` treats as trouble, so the name and the colour can never
/// disagree about whether there is a problem.
fn troubled_groups(report: &StatusReport) -> Vec<String> {
    if !report.supervisor_running {
        return Vec::new();
    }
    // Lower ranks are worse, so the group named first is the one to look
    // at first.
    let rank = |state: &str| match state {
        "halted" | "unreachable" | "errored" => Some(0u8),
        "conflicts" | "blocked" => Some(1u8),
        _ => None,
    };
    let mut troubled: Vec<(u8, String)> = report
        .groups
        .iter()
        .filter_map(|group| {
            group
                .sessions
                .iter()
                .filter_map(|session| rank(session.state.as_str()))
                .min()
                .map(|worst| (worst, group.name.clone()))
        })
        .collect();
    troubled.sort_by_key(|(worst, _)| *worst);
    troubled.into_iter().map(|(_, name)| name).collect()
}

/// One group's state: its unhappiest session decides.
///
/// The same states `health_of` treats as trouble, so a group's dot and the
/// icon's colour can never disagree.
/// How a group is named in the menu: its root, its name, and — when it is
/// peering — which side is doing the work. A group that is not peering
/// says nothing about roles, which is every group until someone asks for
/// one.
fn group_label(group: &crate::supervisor::GroupReport) -> String {
    let role = match group.role.as_str() {
        "leader" => ", leading",
        "follower" => ", following",
        _ => "",
    };
    format!("{}  ({}{role})", group.alpha, group.name)
}

fn group_health(group: &crate::supervisor::GroupReport, supervisor_running: bool) -> Health {
    if !supervisor_running {
        return Health::Idle;
    }
    // Worst wins, and a session that has never run is not yet good news.
    let rank = |health: Health| match health {
        Health::Bad => 3u8,
        Health::Attention => 2,
        Health::Idle => 1,
        Health::Good => 0,
    };
    let mut worst = Health::Good;
    for session in &group.sessions {
        let health = match session.state.as_str() {
            "halted" | "unreachable" | "errored" => Health::Bad,
            "conflicts" | "blocked" => Health::Attention,
            "never-run" => Health::Idle,
            _ => Health::Good,
        };
        if rank(health) > rank(worst) {
            worst = health;
        }
    }
    worst
}

/// A group's state as one dot, in the colours the icon already uses.
///
/// Menu images are pinned to 18 points, so this is drawn at 36 pixels —
/// exactly twice, as the status icon is — and a retina menu gets it pixel
/// for pixel. The dot is deliberately smaller than its box: it marks a row
/// of text rather than standing as an icon of its own.
fn status_dot(health: Health) -> muda::Icon {
    const SIZE: u32 = 36;
    const SS: u32 = 4;
    const RADIUS: f32 = 7.5;
    let (r, g, b) = match health {
        Health::Idle => (142, 142, 147),
        Health::Good => (52, 199, 89),
        Health::Attention => (255, 204, 0),
        Health::Bad => (255, 69, 58),
    };
    let centre = SIZE as f32 / 2.0;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for py in 0..SIZE {
        for px in 0..SIZE {
            // Coverage of this pixel by the circle, in subsamples, so the
            // edge is smooth at this size.
            let mut covered = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                    if ((x - centre).powi(2) + (y - centre).powi(2)).sqrt() <= RADIUS {
                        covered += 1;
                    }
                }
            }
            let alpha = (covered * 255 / (SS * SS)) as u8;
            rgba.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    muda::Icon::from_rgba(rgba, SIZE, SIZE).expect("a valid dot")
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
            "halted" | "unreachable" | "errored" => return Health::Bad,
            "conflicts" | "blocked" => health = Health::Attention,
            _ => {}
        }
    }
    health
}

/// The colour the menu bar draws its own glyphs in.
type Ink = (u8, u8, u8);

/// Reads the menu bar's ink: white on a dark bar, black on a light one.
///
/// A template image would let macOS pick this itself, but a template is
/// recoloured whole, and the state dot has to keep its colour — so the
/// app asks which appearance is in effect and draws the sign to match.
///
/// It asks the status item's own button, not the application. macOS 26
/// chooses the menu bar's ink from the wallpaper behind it, and the
/// application's appearance does not follow: a light system over a dark
/// wallpaper reports Aqua to the application while the menu bar draws
/// in white, and a sign drawn from the application's answer came out
/// black among white neighbours. The button lives in the menu bar's own
/// window, so its appearance is the one the bar actually draws with.
/// Before the status item exists there is no button, and the
/// application's appearance stands in until the first poll.
#[cfg(target_os = "macos")]
fn menu_bar_ink(tray: Option<&TrayIcon>) -> Ink {
    // The trait carries `effectiveAppearance` for both the button and
    // the application.
    use objc2_app_kit::{NSAppearanceCustomization, NSApplication};
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return (0, 0, 0);
    };
    let appearance = tray
        .and_then(|tray| tray.ns_status_item())
        .and_then(|item| item.button(mtm))
        .map(|button| button.effectiveAppearance())
        .unwrap_or_else(|| NSApplication::sharedApplication(mtm).effectiveAppearance());
    // Every dark appearance — DarkAqua, VibrantDark, the high-contrast
    // variants — carries the word; matching on it covers them all.
    if appearance.name().to_string().contains("Dark") {
        (255, 255, 255)
    } else {
        (0, 0, 0)
    }
}

/// Elsewhere the bar's colour is not knowable, so the sign is drawn in a
/// grey that reads on either.
#[cfg(not(target_os = "macos"))]
fn menu_bar_ink(_tray: Option<&TrayIcon>) -> Ink {
    (142, 142, 147)
}

/// The Autobahn sign — two lanes to the horizon under a bridge — in the
/// menu bar's ink, with the health in a dot at the corner, drawn in code
/// so there is no asset to ship or lose.
///
/// Idle is the sign struck through, the mark a wifi icon uses for *off*.
/// Every other health draws the sign plain and adds the dot in that
/// health's colour, ringed by a transparent gap
/// so it sits *on* the sign rather than merging with it — the same gap
/// runs under the bridge, which is what makes it a bridge rather than a
/// stripe. Rendered at 36 pixels, exactly twice the 18 points macOS shows
/// a status image at, so a Retina bar gets it pixel for pixel.
fn icon(health: Health, ink: Ink) -> Icon {
    Icon::from_rgba(icon_rgba(health, ink), 36, 36).expect("a valid icon")
}

/// The icon's pixels, straight RGBA, row-major, 36 by 36.
fn icon_rgba(health: Health, ink: Ink) -> Vec<u8> {
    const SIZE: u32 = 36;
    // Geometry is expressed in a 22-unit square, the size it was designed
    // at, and scaled here.
    const UNIT: f32 = SIZE as f32 / 22.0;
    // Samples per pixel edge: sixteen per pixel is enough for the slanted
    // lane edges to be smooth at this size.
    const SS: u32 = 4;

    let dot: Option<(u8, u8, u8)> = match health {
        Health::Idle => None,
        Health::Good => Some((52, 199, 89)),
        Health::Attention => Some((255, 204, 0)),
        Health::Bad => Some((255, 69, 58)),
    };
    // Idle strikes the sign through rather than fading it. A faded sign
    // says "nothing is running" too quietly: at a glance it reads as a
    // dim sign, not as a state. The slash is the mark every wifi and
    // bell icon already uses for *off*, so it needs no explaining.
    let slash = matches!(health, Health::Idle);
    // Bottom left to top right, the direction SF Symbols draws it. Half
    // the stroke's width, then the transparent gap that keeps it from
    // merging with the lanes it crosses — the same trick as the dot's
    // ring, and as the gap under the bridge.
    let (ax, ay, bx, by) = (3.4f32, 19.2f32, 18.6f32, 2.8f32);
    let (slash_half, slash_gap) = (1.1f32, 1.1f32);
    // Distance from a point to that segment.
    let off_slash = |x: f32, y: f32| -> f32 {
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        ((x - (ax + t * dx)).powi(2) + (y - (ay + t * dy)).powi(2)).sqrt()
    };

    // The two lanes, as quadrilaterals, and the bridge with its gap.
    let left = [(2.5, 19.5), (8.0, 19.5), (10.2, 2.5), (8.9, 2.5)];
    let right = [(14.0, 19.5), (19.5, 19.5), (13.1, 2.5), (11.8, 2.5)];
    let inside = |polygon: &[(f32, f32); 4], x: f32, y: f32| -> bool {
        // Even-odd crossing test.
        let mut hit = false;
        let mut j = 3;
        for i in 0..4 {
            let (xi, yi) = polygon[i];
            let (xj, yj) = polygon[j];
            if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                hit = !hit;
            }
            j = i;
        }
        hit
    };
    let in_sign = |x: f32, y: f32| -> bool {
        let bridge = (1.5..=20.5).contains(&x) && (9.4..=11.6).contains(&y);
        let gap = (1.5..=20.5).contains(&x) && (11.6..12.7).contains(&y);
        if gap {
            return false;
        }
        bridge || inside(&left, x, y) || inside(&right, x, y)
    };
    let (dot_x, dot_y, dot_r, ring_r) = (17.0f32, 16.7f32, 3.0f32, 4.3f32);

    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for py in 0..SIZE {
        for px in 0..SIZE {
            // Coverage of this pixel by the sign and by the dot, each in
            // [0, 1]; the ring keeps them from ever sharing a pixel.
            let mut sign = 0u32;
            let mut dotted = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / UNIT;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / UNIT;
                    let d = ((x - dot_x).powi(2) + (y - dot_y).powi(2)).sqrt();
                    let across = if slash { off_slash(x, y) } else { f32::MAX };
                    if dot.is_some() && d <= dot_r {
                        dotted += 1;
                    } else if dot.is_some() && d <= ring_r {
                        // Transparent: the gap around the dot.
                    } else if across <= slash_half {
                        sign += 1;
                    } else if across <= slash_half + slash_gap {
                        // Transparent: the gap beside the slash.
                    } else if in_sign(x, y) {
                        sign += 1;
                    }
                }
            }
            let samples = (SS * SS) as f32;
            let (r, g, b, a) = if dotted > 0 {
                let (r, g, b) = dot.unwrap_or(ink);
                (r, g, b, dotted as f32 / samples)
            } else {
                (ink.0, ink.1, ink.2, sign as f32 / samples)
            };
            rgba.extend_from_slice(&[r, g, b, (a * 255.0).round() as u8]);
        }
    }
    rgba
}

#[cfg(test)]
mod icon_tests {
    use super::*;

    fn pixels(health: Health, ink: Ink) -> Vec<[u8; 4]> {
        // `Icon` keeps its buffer private, so the test rebuilds it the
        // same way `icon` does and asks tray-icon only to accept it.
        let _ = icon(health, ink);
        render(health, ink)
    }

    /// The renderer, exposed to the test as the raw buffer.
    fn render(health: Health, ink: Ink) -> Vec<[u8; 4]> {
        let built = icon_rgba(health, ink);
        built.chunks(4).map(|c| [c[0], c[1], c[2], c[3]]).collect()
    }

    fn art(px: &[[u8; 4]]) -> String {
        let mut out = String::new();
        for y in 0..36 {
            for x in 0..36 {
                let [r, g, b, a] = px[y * 36 + x];
                out.push(match (a, (r, g, b)) {
                    (0, _) => '·',
                    (a, _) if a < 96 => '░',
                    (_, (52, 199, 89)) | (_, (255, 204, 0)) | (_, (255, 69, 58)) => '●',
                    (a, _) if a < 200 => '▒',
                    _ => '█',
                });
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn every_state_renders_and_the_shape_is_what_was_drawn() {
        for (ink, ink_name) in [((0, 0, 0), "black"), ((255, 255, 255), "white")] {
            for (health, name) in [
                (Health::Idle, "idle"),
                (Health::Good, "good"),
                (Health::Attention, "attention"),
                (Health::Bad, "bad"),
            ] {
                let px = pixels(health, ink);
                assert_eq!(px.len(), 36 * 36);
                eprintln!("--- {name} on {ink_name} ink ---\n{}", art(&px));

                let at = |x: usize, y: usize| px[y * 36 + x];
                // The bridge is solid ink across the middle, probed left
                // of centre, clear of where the idle slash crosses it.
                let bridge = at(5, 18);
                assert_eq!(
                    (bridge[0], bridge[1], bridge[2]),
                    ink,
                    "{name}: bridge is ink"
                );
                assert_eq!(bridge[3], 255, "{name}: the bridge is solid");
                // The gap under the bridge is transparent where a lane runs.
                let gap = at(9, 19);
                assert_eq!(
                    gap[3], 0,
                    "{name}: the gap under the bridge is clear, got {:?}",
                    gap
                );
                // Idle alone is struck through. Above the bridge and to
                // the right of both lanes nothing else is ever drawn, so
                // the region carries ink when idle and none otherwise.
                // A region, not a point: the slash's own edges move
                // whenever the geometry is nudged, and a fixed probe
                // then tests the nudge instead of the drawing.
                let struck = (0..15)
                    .flat_map(|y| (26..36).map(move |x| (x, y)))
                    .filter(|&(x, y)| at(x, y)[3] > 200)
                    .count();
                match health {
                    Health::Idle => {
                        assert!(struck > 15, "{name}: struck through, got {struck}")
                    }
                    _ => assert_eq!(struck, 0, "{name}: nothing above the lanes"),
                }
                // The dot: absent when idle, present and pure-coloured otherwise.
                let centre = at(28, 28);
                match health {
                    Health::Idle => assert_eq!(
                        (centre[0], centre[1], centre[2]),
                        ink,
                        "{name}: no dot when idle — the lane shows through"
                    ),
                    Health::Good => assert_eq!(centre, [52, 199, 89, 255]),
                    Health::Attention => assert_eq!(centre, [255, 204, 0, 255]),
                    Health::Bad => assert_eq!(centre, [255, 69, 58, 255]),
                }
                // Well outside everything: transparent.
                assert_eq!(at(1, 1)[3], 0, "{name}: corner is clear");
            }
        }
    }
}

/// How long a phase must have run before the menu reports it in place of
/// the last cycle's outcome. The menu is glanced at, not watched, and a
/// line that flickers into "scanning" between polls says less than the one
/// it replaces. Matches the command line's threshold.
const SLOW_PHASE_SECONDS: u64 = 5;

/// Formats an elapsed or remaining duration, at two significant units.
fn format_elapsed(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => match (seconds / 60, seconds % 60) {
            (minutes, 0) => format!("{minutes}m"),
            (minutes, rest) => format!("{minutes}m{rest:02}s"),
        },
        _ => match (seconds / 3600, (seconds % 3600) / 60) {
            (hours, 0) => format!("{hours}h"),
            (hours, minutes) => format!("{hours}h{minutes:02}m"),
        },
    }
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
