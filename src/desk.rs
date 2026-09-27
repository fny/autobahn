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

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui;

use crate::supervisor::{status_report, GroupReport, SessionReport, StatusReport};

/// How often the fleet is re-read when nothing is working, and when
/// something is. A session mid-scan reports progress that is worth
/// watching; a quiet fleet is not, and a window that repaints twice a
/// second on battery is the thing MAC-7 complains about.
const POLL_AT_REST: Duration = Duration::from_secs(2);
const POLL_WHILE_WORKING: Duration = Duration::from_millis(500);

/// The panes, in the order the sidebar lists them.
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
    log_filter: LogFilter,
    /// The last thing an action said, kept until the next one.
    said: Option<String>,
    /// Photographing itself: where the files go, and which pane is next.
    /// The window has no screen-recording permission to borrow and does
    /// not need one — egui can hand back the frame it just drew.
    shots: Option<Shots>,
}

/// A run that walks the panes, saves each one, and quits.
struct Shots {
    directory: PathBuf,
    remaining: Vec<(Pane, &'static str)>,
    /// Frames to let the pane settle before the shutter: the first frame
    /// of a pane has no report, no diff and no measured column widths.
    settle: u32,
    taken: Vec<PathBuf>,
}

#[derive(Clone, PartialEq, Eq)]
struct Conflict {
    group: String,
    host: String,
    path: String,
    blocked: bool,
}

#[derive(Default, PartialEq, Eq)]
struct LogFilter {
    errors_only: bool,
    session: Option<String>,
}

/// Runs the window until it is closed.
pub fn run(config: Option<PathBuf>, state_root: PathBuf) -> Result<()> {
    run_with(config, state_root, None)
}

/// The same window, told to photograph itself into `directory` and quit.
/// Personal tooling for a personal app: it is how the design document's
/// screenshots are made, and it needs no permission from the system.
pub fn shoot(config: Option<PathBuf>, state_root: PathBuf, directory: PathBuf) -> Result<()> {
    run_with(config, state_root, Some(directory))
}

fn run_with(
    config: Option<PathBuf>,
    state_root: PathBuf,
    shots: Option<PathBuf>,
) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([760.0, 480.0])
            .with_title("Autobahn Desk"),
        ..Default::default()
    };
    let shots = shots.map(|directory| Shots {
        directory,
        remaining: vec![
            (Pane::Fleet, "fleet"),
            (Pane::Conflicts, "conflicts"),
            (Pane::Log, "log"),
            (Pane::Hosts, "hosts"),
        ],
        settle: 0,
        taken: Vec::new(),
    });
    let desk = Desk {
        config,
        state_root,
        pane: Pane::Fleet,
        report: None,
        read_at: None,
        selected: None,
        conflict: None,
        diff: None,
        log: Vec::new(),
        log_filter: LogFilter::default(),
        said: None,
        shots,
    };
    eframe::run_native("autobahn desk", options, Box::new(|_| Ok(Box::new(desk))))
        .map_err(|error| anyhow::anyhow!("unable to open the window: {error}"))
}

impl eframe::App for Desk {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.refresh_if_due();
        let working = self.working();
        ctx.request_repaint_after(match working {
            true => POLL_WHILE_WORKING,
            false => POLL_AT_REST,
        });

        egui::TopBottomPanel::top("bar").show(ctx, |ui| self.bar(ui));
        egui::TopBottomPanel::bottom("said").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                match &self.said {
                    Some(said) => ui.label(egui::RichText::new(said).monospace().size(11.0)),
                    None => ui.label(
                        egui::RichText::new(self.state_root.display().to_string())
                            .monospace()
                            .size(11.0)
                            .weak(),
                    ),
                };
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| match self.pane {
            Pane::Fleet => self.fleet(ui),
            Pane::Conflicts => self.conflicts(ui),
            Pane::Log => self.log_pane(ui),
            Pane::Hosts => self.hosts(ui),
        });
        self.photograph(ctx);
    }
}

impl Desk {
    /// The top bar: the panes, and what the fleet adds up to.
    fn bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading(egui::RichText::new("autobahn").strong());
            ui.add_space(14.0);
            for pane in [Pane::Fleet, Pane::Conflicts, Pane::Log, Pane::Hosts] {
                let label = match (pane, self.waiting()) {
                    (Pane::Conflicts, n) if n > 0 => format!("{} {n}", pane.title()),
                    _ => pane.title().to_owned(),
                };
                if ui.selectable_label(self.pane == pane, label).clicked() {
                    self.pane = pane;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (text, colour) = self.headline();
                ui.label(egui::RichText::new(text).color(colour).monospace());
            });
        });
        ui.add_space(6.0);
    }

    /// One line for the whole fleet: what needs a person, then what is
    /// away, then the count that is fine.
    fn headline(&self) -> (String, egui::Color32) {
        let Some(report) = &self.report else {
            return ("reading…".to_owned(), GREY);
        };
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
        let service = match report.supervisor_running {
            true => "supervisor running",
            false => "no supervisor",
        };
        let colour = match (needs, away) {
            (0, 0) => GREEN,
            (0, _) => RED,
            _ => AMBER,
        };
        (
            format!("{needs} need you · {away} away · {fine} synchronized · {service}"),
            colour,
        )
    }

    fn fleet(&mut self, ui: &mut egui::Ui) {
        let Some(report) = self.report.clone() else {
            ui.label("reading the fleet…");
            return;
        };
        egui::ScrollArea::vertical().show(ui, |ui| {
            for group in &report.groups {
                self.band(ui, group);
            }
        });
    }

    /// One group: its name and root, what its sessions add up to, and a
    /// row each. A group with nothing to say stays one line.
    fn band(&mut self, ui: &mut egui::Ui, group: &GroupReport) {
        let quiet = group
            .sessions
            .iter()
            .all(|session| matches!(severity(&session.state), Severity::Fine));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&group.name).strong().size(17.0));
            ui.label(egui::RichText::new(&group.alpha).monospace().weak().size(12.0));
            if !group.role.is_empty() {
                ui.label(
                    egui::RichText::new(format!("{} · term {}", group.role, group.term))
                        .color(BLUE)
                        .monospace()
                        .size(12.0),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let summary = summarize(&group.sessions);
                ui.label(egui::RichText::new(summary.0).color(summary.1).monospace().size(12.0));
            });
        });
        ui.separator();
        if quiet && group.sessions.len() > 1 {
            return;
        }
        for session in &group.sessions {
            self.session_row(ui, group, session);
        }
    }

    fn session_row(&mut self, ui: &mut egui::Ui, group: &GroupReport, session: &SessionReport) {
        let key = (group.name.clone(), session.session.clone());
        let open = self.selected.as_ref() == Some(&key);
        let response = ui.horizontal(|ui| {
            ui.add_space(8.0);
            dot(ui, severity(&session.state));
            ui.label(egui::RichText::new(&session.beta).monospace().size(12.5));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(match session.age_seconds {
                        Some(age) => format_age(age),
                        None => "never".to_owned(),
                    })
                    .monospace()
                    .weak()
                    .size(11.5),
                );
                ui.label(
                    egui::RichText::new(format!("{} cycles", session.cycles))
                        .monospace()
                        .weak()
                        .size(11.5),
                );
                ui.label(
                    egui::RichText::new(&session.mode)
                        .monospace()
                        .weak()
                        .size(11.0),
                );
                ui.label(
                    egui::RichText::new(state_words(session))
                        .color(colour_of(severity(&session.state)))
                        .monospace()
                        .size(12.0),
                );
            });
        });
        if response.response.interact(egui::Sense::click()).clicked() {
            self.selected = match open {
                true => None,
                false => Some(key),
            };
        }
        // A session that is working says what it is doing, because that is
        // the one thing a recorded status cannot tell you.
        if let Some(progress) = &session.progress {
            if progress.phase.is_working() {
                ui.horizontal(|ui| {
                    ui.add_space(26.0);
                    ui.label(
                        egui::RichText::new(describe(progress))
                            .monospace()
                            .size(11.5)
                            .color(BLUE),
                    );
                });
            }
        }
        if let Some(error) = &session.error {
            ui.horizontal(|ui| {
                ui.add_space(26.0);
                ui.label(
                    egui::RichText::new(crate::text::display_safe(error).to_string())
                        .monospace()
                        .size(11.5)
                        .color(RED),
                );
            });
        }
        if open {
            self.detail(ui, group, session);
        }
    }

    /// The open session: both roots, what it last did, what is stuck, and
    /// the four things that can be asked of it.
    fn detail(&mut self, ui: &mut egui::Ui, group: &GroupReport, session: &SessionReport) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                ui.vertical(|ui| {
                    pair(ui, "alpha", &group.alpha);
                    pair(ui, "beta", &session.beta);
                    pair(ui, "mode", &session.mode);
                    pair(ui, "cycles", &session.cycles.to_string());
                    pair(ui, "state", &session.state);
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Flush").clicked() {
                            self.control(group, session, Verb::Flush);
                        }
                        if ui.button("Verify").clicked() {
                            self.control(group, session, Verb::Verify);
                        }
                        if ui.button("Pause").clicked() {
                            self.control(group, session, Verb::Pause);
                        }
                        if ui.button("Resume").clicked() {
                            self.control(group, session, Verb::Resume);
                        }
                    });
                    ui.label(
                        egui::RichText::new(
                            "Reset is not here on purpose: it resurrects deletions, and wants \
                             a sentence of its own before it is offered.",
                        )
                        .size(11.0)
                        .weak(),
                    );
                });
                ui.separator();
                ui.vertical(|ui| {
                    if session.blocked.is_empty() && session.conflicts.is_empty() {
                        ui.label(egui::RichText::new("nothing is waiting").weak().size(12.0));
                    }
                    for blocked in &session.blocked {
                        ui.label(
                            egui::RichText::new(crate::text::display_safe(blocked).to_string())
                                .monospace()
                                .size(11.0)
                                .color(AMBER),
                        );
                    }
                    for conflict in &session.conflicts {
                        ui.label(
                            egui::RichText::new(
                                crate::text::display_safe(&conflict.path).to_string(),
                            )
                            .monospace()
                            .size(11.0)
                            .color(AMBER),
                        );
                    }
                });
            });
        });
    }

    /// Every conflict and blocked path in the fleet, and the diff for the
    /// one that is open.
    fn conflicts(&mut self, ui: &mut egui::Ui) {
        let waiting = self.waiting_list();
        if waiting.is_empty() {
            ui.add_space(20.0);
            ui.label(egui::RichText::new("nothing needs you").size(16.0));
            return;
        }
        let open = self.conflict.clone();
        egui::SidePanel::left("queue")
            .resizable(true)
            .default_width(330.0)
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for item in &waiting {
                        let selected = open.as_ref() == Some(item);
                        let label = format!(
                            "{}  {}",
                            item.group,
                            crate::text::display_safe(&item.path)
                        );
                        let text = match item.blocked {
                            true => egui::RichText::new(label).monospace().size(11.5).color(AMBER),
                            false => egui::RichText::new(label).monospace().size(11.5),
                        };
                        if ui.selectable_label(selected, text).clicked() {
                            self.conflict = Some(item.clone());
                            self.diff = None;
                        }
                    }
                });
            });
        let Some(item) = self.conflict.clone() else {
            ui.label("pick one");
            return;
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(crate::text::display_safe(&item.path).to_string())
                    .monospace()
                    .size(12.5),
            );
        });
        ui.separator();
        if item.blocked {
            ui.label(
                egui::RichText::new(
                    "A blocked path is a filesystem to fix, not a version to choose.",
                )
                .size(12.0)
                .weak(),
            );
            return;
        }
        ui.horizontal(|ui| {
            if ui.button("Keep alpha").clicked() {
                self.resolve(&item, "alpha");
            }
            if ui.button(format!("Keep {}", item.host)).clicked() {
                let host = item.host.clone();
                self.resolve(&item, &host);
            }
            if ui.button("Keep both").clicked() {
                self.resolve(&item, "both");
            }
            if ui.button("Show the difference").clicked() {
                self.read_diff(&item);
            }
        });
        ui.add_space(8.0);
        if let Some(diff) = &self.diff {
            egui::ScrollArea::both().show(ui, |ui| {
                for line in diff.lines() {
                    let colour = match line.chars().next() {
                        Some('+') => GREEN,
                        Some('-') => RED,
                        Some('@') => BLUE,
                        _ => GREY,
                    };
                    ui.label(
                        egui::RichText::new(crate::text::display_safe(line).to_string())
                            .monospace()
                            .size(11.5)
                            .color(colour),
                    );
                }
            });
        }
    }

    /// The supervisor's own account, filtered — the file is megabytes and
    /// the line anyone wants is one of hundreds of thousands.
    fn log_pane(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.log_filter.errors_only, "errors only");
            if ui.button("Re-read").clicked() {
                self.read_log();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("{} lines", self.log.len()))
                        .monospace()
                        .weak()
                        .size(11.0),
                );
            });
        });
        ui.separator();
        if self.log.is_empty() {
            self.read_log();
        }
        let errors_only = self.log_filter.errors_only;
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.log {
                    let error = line.contains(" error:") || line.contains("refused");
                    if errors_only && !error {
                        continue;
                    }
                    let colour = match (error, line.contains("debug:")) {
                        (true, _) => RED,
                        (_, true) => GREY,
                        _ => egui::Color32::PLACEHOLDER,
                    };
                    ui.label(
                        egui::RichText::new(crate::text::display_safe(line).to_string())
                            .monospace()
                            .size(11.0)
                            .color(colour),
                    );
                }
            });
    }

    /// Every host the fleet talks to, what it is carrying, and whether
    /// this build can talk to it.
    fn hosts(&mut self, ui: &mut egui::Ui) {
        let Some(report) = &self.report else { return };
        let mut hosts: Vec<(String, usize, Severity, Option<String>)> = Vec::new();
        for group in &report.groups {
            for session in &group.sessions {
                let entry = hosts.iter_mut().find(|(host, ..)| host == &session.host);
                let severity = severity(&session.state);
                match entry {
                    Some((_, count, worst, error)) => {
                        *count += 1;
                        if severity > *worst {
                            *worst = severity;
                            *error = session.error.clone();
                        }
                    }
                    None => hosts.push((
                        session.host.clone(),
                        1,
                        severity,
                        session.error.clone(),
                    )),
                }
            }
        }
        hosts.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        ui.add_space(6.0);
        for (host, count, worst, error) in hosts {
            ui.horizontal(|ui| {
                dot(ui, worst);
                ui.label(egui::RichText::new(&host).monospace().size(13.0));
                ui.label(
                    egui::RichText::new(format!("{count} sessions"))
                        .monospace()
                        .weak()
                        .size(11.5),
                );
                if let Some(error) = error.filter(|_| worst != Severity::Fine) {
                    ui.label(
                        egui::RichText::new(
                            crate::text::display_safe(error.rsplit(": ").next().unwrap_or(&error))
                                .to_string(),
                        )
                        .monospace()
                        .size(11.0)
                        .color(colour_of(worst)),
                    );
                }
            });
        }
        ui.add_space(12.0);
        ui.separator();
        ui.label(egui::RichText::new("the agent bundle").strong().size(13.0));
        let agents = self.state_root.join("agents");
        match std::fs::read_to_string(agents.join("MANIFEST")) {
            Ok(manifest) => {
                for line in manifest.lines() {
                    ui.label(egui::RichText::new(line).monospace().size(11.5));
                }
            }
            Err(_) => {
                ui.label(
                    egui::RichText::new(format!(
                        "{} carries no manifest, so a stale bundle is caught by the handshake \
                         rather than before it is sent",
                        agents.display()
                    ))
                    .size(11.5)
                    .weak(),
                );
            }
        }
        ui.label(
            egui::RichText::new(format!("this build is {}", crate::protocol::version()))
                .monospace()
                .size(11.5)
                .weak(),
        );
    }

    // ── the seam ─────────────────────────────────────────────────────

    fn refresh_if_due(&mut self) {
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
            return;
        }
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
        self.sessions()
            .iter()
            .any(|session| session.progress.as_ref().is_some_and(|p| p.phase.is_working()))
    }

    fn sessions(&self) -> Vec<&SessionReport> {
        self.report
            .iter()
            .flat_map(|report| report.groups.iter())
            .flat_map(|group| group.sessions.iter())
            .collect()
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
    fn control(&mut self, group: &GroupReport, session: &SessionReport, verb: Verb) {
        let selector = crate::supervisor::control::Selector {
            group: Some(group.name.clone()),
            host: Some(session.beta.clone()),
            session: Some(session.session.clone()),
        };
        let request = match verb {
            Verb::Flush => crate::supervisor::control::ControlRequest::Flush(selector),
            Verb::Verify => crate::supervisor::control::ControlRequest::Verify(selector),
            Verb::Pause => crate::supervisor::control::ControlRequest::Pause(selector),
            Verb::Resume => crate::supervisor::control::ControlRequest::Resume(selector),
        };
        self.said = Some(match crate::supervisor::control::send(&self.state_root, &request) {
            Ok(_) => format!("{} {}", verb.done(), session.beta),
            Err(error) => format!("{error:#}"),
        });
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

    /// Walks the panes, saving each, then quits. Nothing here runs
    /// unless `shoot` asked for it.
    fn photograph(&mut self, ctx: &egui::Context) {
        let Some(shots) = &mut self.shots else { return };
        let Some((pane, name)) = shots.remaining.first().copied() else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        };
        if self.pane != pane {
            self.pane = pane;
            shots.settle = 0;
            // The conflicts pane is worth photographing with something
            // open in it, as a reader would find it.
            if pane == Pane::Conflicts {
                if let Some(first) = self.waiting_list().into_iter().find(|item| !item.blocked) {
                    self.conflict = Some(first.clone());
                    self.read_diff(&first);
                }
            }
            ctx.request_repaint();
            return;
        }
        let Some(shots) = &mut self.shots else { return };
        shots.settle += 1;
        if shots.settle == 8 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            ctx.request_repaint();
            return;
        }
        if shots.settle < 8 {
            ctx.request_repaint();
            return;
        }
        let image = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else {
            ctx.request_repaint();
            return;
        };
        let Some(shots) = &mut self.shots else { return };
        let path = shots.directory.join(format!("desk-{name}.png"));
        let width = image.size[0] as u32;
        let height = image.size[1] as u32;
        let pixels: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|pixel| pixel.to_array())
            .collect();
        match image::RgbaImage::from_raw(width, height, pixels) {
            Some(buffer) => match buffer.save(&path) {
                Ok(()) => shots.taken.push(path),
                Err(error) => eprintln!("unable to save {}: {error}", path.display()),
            },
            None => eprintln!("the frame did not fit its own dimensions"),
        }
        shots.remaining.remove(0);
        shots.settle = 0;
        if shots.remaining.is_empty() {
            for taken in &shots.taken {
                println!("{}", taken.display());
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }

    fn read_log(&mut self) {
        let path = self.state_root.join("service.log");
        let Ok(text) = std::fs::read_to_string(&path) else {
            self.log = vec![format!("{} cannot be read", path.display())];
            return;
        };
        // The tail only: the file runs to megabytes, and a window is not
        // where anyone reads the first line of it.
        self.log = text.lines().rev().take(400).map(str::to_owned).collect();
        self.log.reverse();
    }
}

enum Verb {
    Flush,
    Verify,
    Pause,
    Resume,
}

impl Verb {
    fn done(&self) -> &'static str {
        match self {
            Verb::Flush => "flushed",
            Verb::Verify => "will verify",
            Verb::Pause => "paused",
            Verb::Resume => "resumed",
        }
    }
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

fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("autobahn"))
}

// ── how a state looks ────────────────────────────────────────────────

const GREEN: egui::Color32 = egui::Color32::from_rgb(0x3f, 0xb9, 0x7a);
const AMBER: egui::Color32 = egui::Color32::from_rgb(0xe0, 0xae, 0x42);
const RED: egui::Color32 = egui::Color32::from_rgb(0xe8, 0x79, 0x6a);
const BLUE: egui::Color32 = egui::Color32::from_rgb(0x6e, 0xa8, 0xf0);
const GREY: egui::Color32 = egui::Color32::from_rgb(0x8a, 0x94, 0xa0);

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

fn colour_of(severity: Severity) -> egui::Color32 {
    match severity {
        Severity::Fine => GREEN,
        Severity::Attention => AMBER,
        Severity::Bad => RED,
    }
}

fn dot(ui: &mut egui::Ui, severity: Severity) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), 4.0, colour_of(severity));
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

fn summarize(sessions: &[SessionReport]) -> (String, egui::Color32) {
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

fn pair(ui: &mut egui::Ui, name: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(name)
                .monospace()
                .size(11.0)
                .weak(),
        );
        ui.label(egui::RichText::new(value).monospace().size(12.0));
    });
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
}
