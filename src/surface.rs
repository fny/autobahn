//! What a window over the fleet is, apart from its drawing.
//!
//! Two windows draw this: `crate::desk` on GPUI, `crate::kit` on GPUI
//! Kit. Neither owns any of it. The sections of the configuration file
//! and how a value is written back, what a conflict's two sides hold,
//! what a session is waiting on and how it is grouped, the words for a
//! size or an age — all of it is here, where it can be tested without
//! opening a window and cannot drift between the two.

// Two windows draw this, and neither uses all of it: the one on GPUI
// has a log filter the kit's does not, the kit's has a text block that
// needs none of the first one's caret arithmetic. What one of them does
// not call is not dead — it is drawn by the other.
#![allow(dead_code)]

use std::path::PathBuf;

use crate::supervisor::SessionReport;
use crate::words::{count as counted, fill, t};

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Conflict {
    pub(crate) group: String,
    pub(crate) host: String,
    pub(crate) path: String,
    pub(crate) blocked: bool,
    /// Where each side of this path lives. A conflict over a file that
    /// is not text has no diff to read, so the two files themselves are
    /// what the window has to show — and it needs to know where they are.
    pub(crate) alpha_root: String,
    pub(crate) beta_root: String,
}

/// Which part of the file the form is showing.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum Section {
    /// The keys at the top of the file.
    Settings,
    /// `[defaults]`, inherited by every group.
    Defaults,
    /// `[advanced]`, whose defaults are the right answer.
    Advanced,
    /// `[advanced.alerts]`: how long a condition must hold.
    Alerts,
    /// `[advanced.peering-dangerously-experimental]`: the lease timing.
    Peering,
    /// One `[groups.x]`.
    Group(String),
}

impl Section {
    pub(crate) fn title(&self) -> String {
        match self {
            Section::Settings => t("config.settings").to_owned(),
            Section::Defaults => t("config.defaults").to_owned(),
            Section::Advanced => t("config.advanced").to_owned(),
            Section::Alerts => t("config.advanced_alerts").to_owned(),
            Section::Peering => t("config.advanced_peering").to_owned(),
            Section::Group(name) => name.clone(),
        }
    }
}

/// The configuration file, as the editor holds it between saves.
pub(crate) struct Sheet {
    pub(crate) path: PathBuf,
    /// The file exactly as it was read, so an edit made elsewhere since
    /// then is noticed rather than overwritten.
    pub(crate) text: String,
    /// The file itself, comments and order kept: every edit goes through
    /// `toml_edit`, so saving a form does not rewrite a hand-written
    /// file into something its author would not recognise.
    pub(crate) document: toml_edit::DocumentMut,
    /// What the loader said about the document as it now stands.
    /// Nothing is written while this is set — the loader is the referee,
    /// not the form.
    pub(crate) refused: Option<String>,
    /// How many changes have been made since the file was last read or
    /// written. An edit is held here, not written as it is made: the
    /// supervisor re-reads the file two seconds after it changes, and a
    /// form that wrote every click would hand it half-finished
    /// configurations to start sessions from.
    pub(crate) edits: usize,
}

/// Where one value lives in the file.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Spot {
    pub(crate) section: Section,
    pub(crate) key: String,
    /// Which entry, when the value is a list.
    pub(crate) item: Option<usize>,
}

/// One side of a conflict, as the filesystem has it. Read once, when the
/// path is opened, and never again on the way to a frame: a window that
/// hashes a file every sixtieth of a second is a window that stops.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Side {
    /// `alpha` or `beta`, the words every other surface uses.
    pub(crate) name: &'static str,
    /// The directory this side of the pair lives in.
    pub(crate) root: String,
    /// The whole path, as a person would type it.
    pub(crate) place: String,
    /// The file itself, when it is on this machine.
    pub(crate) file: Option<PathBuf>,
    pub(crate) size: Option<u64>,
    /// Seconds since the epoch, for the same stamp the log uses.
    pub(crate) modified: Option<i64>,
    /// The blake3 of the contents — the digest the scanner records, so
    /// two sides that agree here are the same file to the engine too.
    pub(crate) digest: Option<String>,
    /// Whether the first few kilobytes hold a NUL, which is how `diff`
    /// decides it will not print the file either.
    pub(crate) binary: bool,
    /// Why there is nothing else to say: another machine, or gone.
    pub(crate) trouble: Option<String>,
}

/// Files larger than this are measured and dated but not hashed. Reading
/// a gigabyte to fill in one line is not worth freezing the window for,
/// and the size and the time already answer "which one is mine".
pub(crate) const HASH_LIMIT: u64 = 512 * 1024 * 1024;

/// Everything about one side, in one pass over the file.
pub(crate) fn inspect(name: &'static str, root: &str, path: &str) -> Side {
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
            trouble: Some(t("conflicts.elsewhere").to_owned()),
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
                side.trouble = Some(t("conflicts.not_a_file").to_owned());
                return side;
            }
        }
        Err(error) => {
            side.trouble = Some(fill(
                "conflicts.unreadable",
                &[("reason", &error.kind().to_string())],
            ));
            return side;
        }
    }
    match read_through(&file, side.size.unwrap_or(0)) {
        Ok((binary, digest)) => {
            side.binary = binary;
            side.digest = digest;
        }
        Err(error) => {
            side.trouble =
                Some(fill("conflicts.unreadable", &[("reason", &error.to_string())]))
        }
    }
    side
}

/// Reads the file once: says whether it looks binary, and hashes it when
/// it is small enough to be worth hashing.
pub(crate) fn read_through(file: &std::path::Path, size: u64) -> std::io::Result<(bool, Option<String>)> {
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
pub(crate) fn on_this_machine(root: &str) -> Option<PathBuf> {
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
pub(crate) fn human_size(size: u64) -> String {
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

/// What a field holds, as the schema says.
pub(crate) enum Holds {
    Switch,
    List,
    Line,
}

/// Reads the type out of a schema field, through the `["string","null"]`
/// spelling an optional field gets.
pub(crate) fn holds(field: &serde_json::Value) -> Holds {
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

/// Keys the form does not show at the top of the file: the sections
/// that have a place of their own in the sidebar, and the retired
/// spellings kept only so that a file still using one gets an answer
/// rather than "unknown field".
///
/// Only at the top of the file. A group has a `disabled` of its own
/// that is a flag and not a retired anything, and hiding it everywhere
/// because the name is spoken for at the top is how it went missing.
pub(crate) const SILENT_AT_THE_TOP: &[&str] = &[
    "groups",
    "defaults",
    "advanced",
    "disabled",
    "alerts",
    "peering-experimental",
];

/// The same, for `[advanced]`: its two timing tables are sections of
/// their own, and `peering-experimental` is the spelling that was
/// renamed.
pub(crate) const SILENT_IN_ADVANCED: &[&str] = &["alerts", "peering-dangerously-experimental", "peering-experimental"];

/// Keys that belong to their section but almost nobody needs.
///
/// `default_owner` and `default_group` matter in one case: an agent
/// running as root, which refuses to start unless one of them is set
/// (`root::check_agent`). Every other fleet — your files, your user —
/// leaves them empty, so the form keeps them folded away rather than
/// asking a question about ownership between `mode` and `interval`.
pub(crate) const RARE: &[&str] = &["default_owner", "default_group"];

/// The tables one listed section draws, in the order it draws them.
///
/// Every section but `[advanced]` draws itself alone. Advanced held a
/// single key, and its two timing tables were listed beside it as
/// sections of their own — three places to look for one idea. They are
/// drawn together now; each field is still written to its own table,
/// which is what the `Section` beside it is for.
pub(crate) fn drawn_with(section: &Section) -> Vec<Section> {
    match section {
        Section::Advanced => vec![Section::Advanced, Section::Alerts, Section::Peering],
        alone => vec![alone.clone()],
    }
}

/// The table one section lives in, made if the file has not got it yet.
pub(crate) fn table_for<'a>(
    document: &'a mut toml_edit::DocumentMut,
    section: &Section,
) -> Option<&'a mut toml_edit::Table> {
    match section {
        Section::Settings => Some(document.as_table_mut()),
        Section::Defaults => document
            .entry("defaults")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut(),
        Section::Advanced => document
            .entry("advanced")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut(),
        Section::Alerts => document
            .entry("advanced")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()?
            .entry("alerts")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut(),
        Section::Peering => document
            .entry("advanced")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()?
            .entry("peering-dangerously-experimental")
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

/// A value written back into the file: a number where the text is one,
/// so `interval = 30` does not become `interval = "30"` and then fail
/// to load.
pub(crate) fn number_or_text(text: &str) -> toml_edit::Item {
    match text.parse::<i64>() {
        Ok(number) => toml_edit::value(number),
        Err(_) => toml_edit::value(text.to_owned()),
    }
}

/// What the supervisor would say about this document, or nothing if it
/// would take it.
///
/// Not `Config::parse`, which is types and unknown keys: this is
/// `reload::load_bytes`, the very function a running supervisor reloads
/// with, so the form cannot write a file that parses and is then
/// refused by the daemon two seconds later.
pub(crate) fn refusal(path: &std::path::Path, text: &str) -> Option<String> {
    crate::supervisor::reload::load_bytes(path, text.as_bytes())
        .err()
        .map(|error| format!("{error:#}"))
}

/// The first line of a message, for a status bar that has one line.
pub(crate) fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or_default().to_owned()
}

/// The first sentence of a doc comment: enough to say what a field is
/// for, without turning a form into a manual.
pub(crate) fn first_sentence(about: &str) -> String {
    let about = about.replace('\n', " ");
    match about.split_once(". ") {
        Some((first, _)) => format!("{first}."),
        None => about,
    }
}

/// What one session is waiting on, grouped the way `issues` groups it:
/// the reason first, the paths under it.
///
/// A cause is the innermost message, and twenty files stopped by one
/// thing are one heading with twenty paths, not twenty reasons.
pub(crate) fn waiting_groups(session: &SessionReport) -> Vec<(String, Vec<String>)> {
    const MOST: usize = 20;
    // Grouped by what stopped them: twenty files held up by one thing
    // are one heading with twenty paths, not twenty reasons.
    let mut causes: Vec<(&str, &str, Vec<String>)> = Vec::new();
    for entry in &session.blocked {
        let (side, path, cause) = crate::blocked::parts(entry);
        let path = crate::text::display_safe(path).to_string();
        match causes
            .iter_mut()
            .find(|(other_side, other, _)| *other_side == side && *other == cause)
        {
            Some((_, _, paths)) => paths.push(path),
            None => causes.push((side, cause, vec![path])),
        }
    }
    let mut groups: Vec<(String, Vec<String>)> = causes
        .into_iter()
        .map(|(side, cause, paths)| {
            let heading = counted(
                "waiting.blocked",
                paths.len(),
                &[("side", side), ("cause", cause)],
            );
            (heading, paths)
        })
        .collect();
    if !session.conflicts.is_empty() {
        groups.push((
            counted("waiting.conflict", session.conflicts.len(), &[]),
            session
                .conflicts
                .iter()
                .map(|conflict| crate::text::display_safe(&conflict.path).to_string())
                .collect(),
        ));
    }
    for (_, paths) in &mut groups {
        if paths.len() > MOST {
            let rest = paths.len() - MOST;
            paths.truncate(MOST);
            paths.push(counted("waiting.and_more", rest, &[]));
        }
    }
    groups
}

/// A line cut to fit a status bar.
pub(crate) fn cap(text: &str, most: usize) -> String {
    crate::text::cap_line(text, most).into_owned()
}

/// Whether a log line is the supervisor complaining.
pub(crate) fn is_complaint(line: &str) -> bool {
    line.contains(" error:") || line.contains("refused")
}

/// What a working session is doing, in one line: the phase, how long it
/// has been at it, and how far along when the numbers allow an honest
/// answer. The same rule `status` follows, so the two never disagree.
pub(crate) fn describe(progress: &crate::progress::ProgressSnapshot) -> String {
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
pub(crate) fn thousands(n: u64) -> String {
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
pub(crate) fn tilde(path: &str) -> String {
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
pub(crate) fn tail(path: &str, keep: usize) -> String {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.len() <= keep {
        return path.to_owned();
    }
    format!("…/{}", parts[parts.len() - keep..].join("/"))
}

/// The short name of a beta: the host it is on, or the last part of the
/// path when it is a directory on this machine. Long enough to tell two
/// apart, short enough to sit on a button.
pub(crate) fn short_name(beta: &str) -> String {
    match beta.split_once(':') {
        Some((host, _)) if !host.starts_with('/') && !host.starts_with('~') => host.to_owned(),
        _ => beta.rsplit('/').next().unwrap_or(beta).to_owned(),
    }
}

pub(crate) fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("autobahn"))
}

/// How much a state needs a person, in the order the fleet sorts by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Severity {
    Fine,
    Attention,
    Bad,
}

pub(crate) fn severity(state: &str) -> Severity {
    match state {
        "halted" | "unreachable" | "errored" => Severity::Bad,
        "conflicts" | "blocked" => Severity::Attention,
        _ => Severity::Fine,
    }
}

/// The state, with what it is waiting on when that is the point.
pub(crate) fn state_words(session: &SessionReport) -> String {
    match (session.conflicts.len(), session.blocked.len()) {
        (0, 0) => session.state.clone(),
        (c, 0) => format!("{} · {c}", session.state),
        (0, b) => format!("{} · {b}", session.state),
        (c, b) => format!("{} · {c} + {b}", session.state),
    }
}

pub(crate) fn format_age(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

impl Sheet {
    /// The file a window was pointed at, or the default one.
    pub(crate) fn path_for(config: Option<&std::path::Path>) -> Option<PathBuf> {
        match config {
            Some(path) => Some(path.to_path_buf()),
            None => crate::paths::default_config_path().ok(),
        }
    }

    /// Reads the file, or says why it could not be.
    pub(crate) fn read(config: Option<&std::path::Path>) -> Result<Sheet, String> {
        let Some(path) = Sheet::path_for(config) else {
            return Err(t("config.none").to_owned());
        };
        let text = std::fs::read_to_string(&path).map_err(|error| {
            fill(
                "status.unreadable",
                &[
                    ("path", &path.display().to_string()),
                    ("error", &error.to_string()),
                ],
            )
        })?;
        let document = text.parse::<toml_edit::DocumentMut>().map_err(|error| {
            fill(
                "config.unreadable",
                &[
                    ("path", &path.display().to_string()),
                    ("error", &error.to_string()),
                ],
            )
        })?;
        Ok(Sheet {
            path,
            text,
            document,
            refused: None,
            edits: 0,
        })
    }

    /// What the file holds for a key of a section.
    pub(crate) fn held(&self, section: &Section, key: &str) -> Option<toml_edit::Item> {
        let sheet = self;
        let table: &toml_edit::Item = match section {
            Section::Settings => sheet.document.as_item(),
            Section::Defaults => sheet.document.get("defaults")?,
            Section::Advanced => sheet.document.get("advanced")?,
            Section::Alerts => sheet.document.get("advanced")?.get("alerts")?,
            Section::Peering => sheet
                .document
                .get("advanced")?
                .get("peering-dangerously-experimental")?,
            Section::Group(name) => sheet.document.get("groups")?.get(name)?,
        };
        table.get(key).cloned()
    }

    /// The sections of the file, in the order they are written.
    pub(crate) fn sections(&self) -> Vec<Section> {
        // Alerts and peering are not listed: they are drawn inside
        // `[advanced]`, which is the only place anyone looks for them.
        let mut sections = vec![Section::Settings, Section::Defaults, Section::Advanced];
        if let Some(groups) = self.document.get("groups").and_then(|item| item.as_table()) {
            for (name, _) in groups.iter() {
                sections.push(Section::Group(name.to_owned()));
            }
        }
        sections
    }

    /// Writes one value into the file — or does not, and says why.
    ///
    /// The form never decides whether an edit is allowed: the candidate
    /// document goes through `Config::parse`, the very function the
    /// supervisor loads the file with, and only a document that parses
    /// is written to disk.
    pub(crate) fn put(&mut self, at: &Spot, value: Option<toml_edit::Item>) -> Option<String> {
        let mut document = self.document.clone();
        {
            let table = match table_for(&mut document, &at.section) {
                Some(table) => table,
                None => {
                    return Some(fill(
                        "config.missing_section",
                        &[("section", &at.section.title())],
                    ))
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
        self.hold(document);
        None
    }

    /// Takes a changed document as the one being edited, and asks the
    /// loader what it thinks of it. Nothing is written here.
    fn hold(&mut self, document: toml_edit::DocumentMut) {
        self.refused = refusal(&self.path, &document.to_string());
        self.document = document;
        self.edits += 1;
    }

    /// How many changes are waiting to be written.
    pub(crate) fn pending(&self) -> usize {
        self.edits
    }

    /// What the loader says about the document as it stands.
    pub(crate) fn refused(&self) -> Option<&str> {
        self.refused.as_deref()
    }

    /// Writes the edited document, if the loader takes it and nobody
    /// else has touched the file since it was read.
    pub(crate) fn save(&mut self) -> Option<String> {
        let sheet = self;
        if sheet.edits == 0 {
            return None;
        }
        if let Some(refused) = &sheet.refused {
            return Some(fill(
                "config.not_saved",
                &[("reason", &crate::surface::first_line(refused))],
            ));
        }
        // Somebody may have been editing the same file in an editor
        // since it was read. Their work is not this window's to
        // overwrite.
        if let Ok(now) = std::fs::read_to_string(&sheet.path) {
            if now != sheet.text {
                sheet.refused = Some(t("config.changed").to_owned());
                return Some(t("config.changed_short").to_owned());
            }
        }
        let text = sheet.document.to_string();
        match std::fs::write(&sheet.path, &text) {
            Ok(()) => {
                let edits = sheet.edits;
                sheet.text = text;
                sheet.edits = 0;
                sheet.refused = None;
                let path = sheet.path.clone();
                let path = tilde(&path.display().to_string());
                Some(counted("config.saved", edits, &[("path", &path)]))
            }
            Err(error) => Some(fill("config.unwritable", &[("error", &error.to_string())])),
        }
    }


}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folded key is still a key: if one is renamed in the structs the
    /// parser reads, the fold must not go on hiding a name nothing has.
    #[test]
    fn every_folded_key_is_a_key_the_file_really_has() {
        let shape = crate::config::schema();
        for section in ["Defaults", "Group"] {
            let properties = shape["$defs"][section]["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{section} has properties"));
            for key in RARE {
                assert!(
                    properties.contains_key(*key),
                    "{section} has no '{key}' to fold away"
                );
            }
        }
    }

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

    /// and the one they would see in a terminal.
    #[test]
    fn a_size_is_readable_and_exact() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(2_048), "2.0 KiB · 2,048 bytes");
        assert_eq!(human_size(5_242_880), "5.0 MiB · 5,242,880 bytes");
    }

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

    /// tells two sides of a conflict apart.
    #[test]
    fn a_long_path_keeps_the_end_that_matters() {
        assert_eq!(tail("/a/b/c/d/e", 3), "…/c/d/e");
        assert_eq!(tail("/a/b", 3), "/a/b");
        assert_eq!(tail("fny:~/code/src", 3), "fny:~/code/src");
    }

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

    /// changing and would refuse it there instead.
    #[test]
    fn the_gate_is_what_the_supervisor_would_load() {
        let path = std::path::Path::new("config.toml");
        let text = "[defaults]\nmode = \"two-way-conflict\"\n\n\
                    [groups.a]\nalpha = \"/tmp/a\"\nbetas = [\"/tmp/b\"]\n";
        assert_eq!(refusal(path, text), None);

        let mut document: toml_edit::DocumentMut = text.parse().unwrap();
        let table = table_for(&mut document, &Section::Group("a".to_owned())).unwrap();
        table.insert(
            "ignores",
            toml_edit::value(toml_edit::Array::from_iter(["["])),
        );
        let broken = document.to_string();
        crate::config::Config::parse(path, &broken)
            .expect("serde takes it: a list of strings is a list of strings");
        let complaint = refusal(path, &broken).expect("the loader does not");
        assert!(complaint.contains("ignore"), "{complaint}");
    }

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
}
