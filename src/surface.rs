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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Section {
    /// The keys at the top of the file.
    Settings,
    /// `[defaults]`, inherited by every group.
    Defaults,
    /// `[experimental]`, whose defaults are the right answer.
    Advanced,
    /// `[experimental.alerts]`: how long a condition must hold.
    Alerts,
    /// `[experimental.peering-dangerously-experimental]`: the lease timing.
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
    /// The file as it was read, parsed. Every "has this changed" question
    /// is answered against this, never against a count of how many times
    /// somebody touched a control: setting a value back to what it was is
    /// not a change, and a form that says it is asks to be ignored.
    ///
    /// An edit is held here, not written as it is made: the supervisor
    /// re-reads the file two seconds after it changes, and a form that
    /// wrote every keystroke would hand it half-finished configurations
    /// to start sessions from.
    pub(crate) was: toml_edit::DocumentMut,
    /// Whether the loader has been asked about the document as it now
    /// stands, and whether the next edit should skip asking.
    stale: bool,
    quiet: bool,
    /// What the loader would take, and still say something about.
    pub(crate) warned: Vec<String>,
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
    "experimental",
    "disabled",
    "alerts",
    "peering-experimental",
];

/// The same, for `[experimental]`: its two timing tables are sections of
/// their own, and `peering-experimental` is the spelling that was
/// renamed.
pub(crate) const SILENT_IN_ADVANCED: &[&str] = &["alerts", "peering-dangerously-experimental", "peering-experimental"];

/// Session keys nobody should meet before they have gone looking.
///
/// They are not dangerous, they are a different kind of question. Two
/// are about ownership and only mean anything to an agent running as
/// root (`root::check_agent`). The rest are about how the machinery
/// works rather than what it should do — where the journal is flushed,
/// where staging lives, which binary the other end runs, and a promise
/// that the credentials under a root were meant.
///
/// They are drawn at the foot of the section they belong to, under the
/// same heading as `[experimental]`, and only for a window that has
/// been let in. See `Desk::unlocked` in either window.
pub(crate) const EXPERIMENTAL: &[&str] = &[
    "power_saver_experimental",
    "interval",
    "durability",
    "staging",
    "agent_command",
    "acknowledge_secrets",
    "file_mode",
    "directory_mode",
    "default_owner",
    "default_group",
];

/// The tables one listed section draws, in the order it draws them.
///
/// Every section but `[experimental]` draws itself alone. It held a
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
        Section::Advanced => experimental(document),
        Section::Alerts => experimental(document)?
            .entry("alerts")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut(),
        Section::Peering => experimental(document)?
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

/// The `[experimental]` table, or the `[advanced]` one a file written
/// before the rename already has.
///
/// Whichever is there is the one written to. Making the new one beside
/// the old would leave a file saying the same thing twice, which the
/// parser refuses as a duplicate — so an old file stays an old file
/// until somebody renames the header themselves.
fn experimental<'a>(
    document: &'a mut toml_edit::DocumentMut,
) -> Option<&'a mut toml_edit::Table> {
    let name = match document.contains_key("experimental") {
        true => "experimental",
        false => match document.contains_key("advanced") {
            true => "advanced",
            false => "experimental",
        },
    };
    document
        .entry(name)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
}

/// What one document holds for a key of a section.
fn in_document(
    document: &toml_edit::DocumentMut,
    section: &Section,
    key: &str,
) -> Option<toml_edit::Item> {
    // Read from whichever spelling the file has, as the parser does.
    let tuning = || {
        document
            .get("experimental")
            .or_else(|| document.get("advanced"))
    };
    let table: &toml_edit::Item = match section {
        Section::Settings => document.as_item(),
        Section::Defaults => document.get("defaults")?,
        Section::Advanced => tuning()?,
        Section::Alerts => tuning()?.get("alerts")?,
        Section::Peering => tuning()?.get("peering-dangerously-experimental")?,
        Section::Group(name) => document.get("groups")?.get(name)?,
    };
    table.get(key).cloned()
}

/// One value, written the same way however it was spaced in the file.
///
/// Comparing two values by their text compares their whitespace with
/// them: `["a", "b"]` and `["a","b"]` say the same thing, and a form
/// that rebuilt a list from a text block would otherwise report every
/// list it touched as changed.
fn plain(item: &toml_edit::Item) -> String {
    fn written(held: &toml_edit::Value) -> String {
        match held {
            toml_edit::Value::Array(array) => {
                let inside: Vec<String> = array.iter().map(written).collect();
                format!("[{}]", inside.join(", "))
            }
            toml_edit::Value::InlineTable(table) => {
                let mut inside: Vec<String> = table
                    .iter()
                    .map(|(key, held)| format!("{key} = {}", written(held)))
                    .collect();
                inside.sort();
                format!("{{{}}}", inside.join(", "))
            }
            leaf => leaf.to_string().trim().to_owned(),
        }
    }
    match item {
        toml_edit::Item::Value(held) => written(held),
        // A table is never compared as a leaf: `differ` walks into it.
        other => other.to_string(),
    }
}

/// Counts the leaf values that differ between two documents.
fn differ(was: &toml_edit::Table, now: &toml_edit::Table, changes: &mut usize) {
    for (key, held) in was.iter() {
        match now.get(key) {
            None => *changes += 1,
            Some(mine) => match (held.as_table(), mine.as_table()) {
                (Some(was), Some(now)) => differ(was, now, changes),
                _ => {
                    if plain(held) != plain(mine) {
                        *changes += 1;
                    }
                }
            },
        }
    }
    for (key, _) in now.iter() {
        if was.get(key).is_none() {
            *changes += 1;
        }
    }
}

/// The faults in a refusal, each said once.
///
/// The loader plans every group, so one bad value in `[defaults]` comes
/// back once per group that inherits it — eight lines that differ only
/// in a name. They are one fault, and a window that says "8 problems"
/// about one wrong word has repeated the loader's mistake rather than
/// reported it.
pub(crate) fn faults(refusal: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    // `plans` gathers its complaints under a heading; everything else
    // the loader refuses a file for — a log level it does not know, a
    // hook timing it cannot read — arrives as a sentence on its own.
    let body = match refusal.lines().next() {
        Some("invalid configuration:") => refusal.lines().skip(1),
        _ => refusal.lines().skip(0),
    };
    for line in body {
        // Whole lines, kept whole. The loader blames the table a value
        // was written in, so the copies an inherited value used to make
        // are already identical — and the `group 'x': ` in front of a
        // group's own fault is not noise to cut off, it is how
        // `fault_at` knows whose field to put it under.
        let line = line.trim().to_owned();
        if !line.is_empty() && !seen.contains(&line) {
            seen.push(line);
        }
    }
    seen
}

/// One fault, matched to the field it is about.
///
/// The loader names the key it was reading — "the defaults'
/// ignore_files: …", "group 'aws': max_file_size: …" — which is what
/// lets a form put the complaint under the control that caused it
/// rather than in a paragraph at the top of the pane.
///
/// Returns where it belongs, what to say there, and the words it
/// offered instead, when it offered any.
pub(crate) struct At {
    pub(crate) section: Section,
    pub(crate) key: String,
    pub(crate) said: String,
    pub(crate) instead: Vec<String>,
}

pub(crate) fn fault_at(fault: &str) -> Option<At> {
    /// A name a key could have: nothing with a space or a quote in it.
    fn keyish(word: &str) -> bool {
        !word.is_empty()
            && word
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }

    let (section, rest) = match fault.strip_prefix("the defaults' ") {
        Some(rest) => (Section::Defaults, rest),
        None => match fault.strip_prefix("group '") {
            Some(rest) => {
                let (name, rest) = rest.split_once("': ")?;
                (Section::Group(name.to_owned()), rest)
            }
            // No prefix at all: the head is either a bare key at the top
            // of the file, or a dotted path into one of the experimental
            // tables, whose last segment is the key.
            None => {
                let (head, said) = fault.split_once(": ")?;
                let (table, key) = match head.rsplit_once('.') {
                    Some((table, key)) => (table, key),
                    None => ("", head),
                };
                if !keyish(key) {
                    return None;
                }
                let section = match table {
                    "" => Section::Settings,
                    "experimental" | "advanced" => Section::Advanced,
                    "experimental.alerts" | "advanced.alerts" => Section::Alerts,
                    table if table.contains("peering") => Section::Peering,
                    _ => return None,
                };
                return Some(offered(section, key.to_owned(), said.to_owned()));
            }
        },
    };
    // "key: what went wrong". A colon inside the complaint itself is
    // common, so only the first one counts, and only when what is in
    // front of it looks like a key rather than a sentence.
    let (key, said) = rest.split_once(": ")?;
    if !keyish(key) {
        return None;
    }
    Some(offered(section, key.to_owned(), said.to_owned()))
}

/// The choices a complaint offered, taken out of the sentence it was
/// hiding in: "(available: a, b, c)" is a list of words to pick from.
fn offered(section: Section, key: String, said: String) -> At {
    let mut said = said;
    let mut instead = Vec::new();
    for opener in ["(available: ", "(expected one of: "] {
        let Some(open) = said.find(opener) else {
            continue;
        };
        let Some(close) = said[open..].find(')') else {
            continue;
        };
        instead = said[open + opener.len()..open + close]
            .split(", ")
            .map(str::trim)
            .filter(|word| !word.is_empty())
            .map(str::to_owned)
            .collect();
        said.replace_range(open..open + close + 1, "");
        break;
    }
    At {
        section,
        key,
        said: said.trim().trim_end_matches(&[' ', ','][..]).to_owned(),
        instead,
    }
}

/// The section a refusal is about, when it names one.
///
/// A fault inherited from `[defaults]` is reported against every group
/// that inherits it, and the group is not where anybody would go to fix
/// it — the defaults are. That is why this looks for the defaults first.
pub(crate) fn blamed(refusal: &str) -> Option<Section> {
    if refusal.contains("the defaults'") || refusal.contains("defaults.") {
        return Some(Section::Defaults);
    }
    for line in refusal.lines() {
        if let Some(rest) = line.trim().strip_prefix("group '") {
            if let Some((name, _)) = rest.split_once('\'') {
                return Some(Section::Group(name.to_owned()));
            }
        }
    }
    if refusal.contains("advanced.") || refusal.contains("experimental.") {
        return Some(Section::Advanced);
    }
    None
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
/// What the loader makes of a document: what it would refuse it for, or
/// what it would load it and still say about it.
pub(crate) fn refusal(path: &std::path::Path, text: &str) -> Result<Vec<String>, String> {
    match crate::supervisor::reload::load_bytes(path, text.as_bytes()) {
        Ok(loaded) => Ok(loaded.warnings),
        Err(error) => Err(format!("{error:#}")),
    }
}

/// A plain sentence, written so a Markdown reader leaves it alone.
///
/// The kit draws selectable text through a Markdown view, and our
/// messages are full of characters Markdown means something by: the
/// asterisk in `*.safetensors`, the underscores in `max_file_size`,
/// the backticks in a field's description. Escaped, they read as
/// themselves — and a selection copied out of the view carries the
/// rendered text, not this.
pub(crate) fn as_written(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for letter in text.chars() {
        if matches!(
            letter,
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')'
                | '#' | '+' | '-' | '.' | '!' | '|' | '<' | '>' | '~'
        ) {
            out.push('\\');
        }
        out.push(letter);
    }
    out
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
        let mut sheet = Sheet {
            path,
            text,
            was: document.clone(),
            document,
            refused: None,
            warned: Vec::new(),
            stale: false,
            quiet: false,
        };
        // A file that already does not load says so when it is opened,
        // not only once somebody edits it. This is the one slow call in
        // reading a sheet, and it happens once.
        sheet.ask();
        Ok(sheet)
    }

    /// What the file holds for a key of a section.
    pub(crate) fn held(&self, section: &Section, key: &str) -> Option<toml_edit::Item> {
        in_document(&self.document, section, key)
    }

    /// The sections of the file, in the order they are written.
    pub(crate) fn sections(&self) -> Vec<Section> {
        // Alerts and peering are not listed: they are drawn inside
        // `[experimental]`, the only place anyone looks for them.
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
        self.document = document;
        match self.quiet {
            true => self.stale = true,
            false => self.ask(),
        }
    }

    /// Ask the loader what it makes of the document as it stands.
    ///
    /// This is the real thing — `reload::load_bytes`, the very call the
    /// supervisor makes — and it plans every session in the file, which
    /// on a fleet this size is tens of milliseconds. Fine for a click.
    /// Not fine for a keystroke, which is what [`Sheet::later`] is for.
    fn ask(&mut self) {
        match refusal(&self.path, &self.document.to_string()) {
            Ok(warnings) => {
                self.refused = None;
                self.warned = warnings;
            }
            Err(refused) => {
                self.refused = Some(refused);
                // A file that does not load was never planned, so
                // whatever it might also be warned about is unknown.
                self.warned.clear();
            }
        }
        self.stale = false;
    }

    /// Take a changed document without asking the loader yet.
    ///
    /// Typing is a stream of changes and the loader is too slow to run
    /// on each one. The document is updated — so the count of what is
    /// pending stays honest as you type — and the verdict is marked
    /// stale for [`Sheet::settle`] to catch up on once the typing stops.
    pub(crate) fn later(&mut self, at: &Spot, value: Option<toml_edit::Item>) -> Option<String> {
        let quietly = std::mem::replace(&mut self.quiet, true);
        let said = self.put(at, value);
        self.quiet = quietly;
        said
    }

    /// Catch up on a verdict that typing left behind. True if it moved.
    pub(crate) fn settle(&mut self) -> bool {
        if !self.stale {
            return false;
        }
        self.ask();
        true
    }

    /// How many values differ from the file on disk.
    ///
    /// Counted, not tallied: a value put back to what it was stops
    /// counting, which is the whole reason this walks two documents
    /// rather than adding one per edit.
    pub(crate) fn pending(&self) -> usize {
        let mut changes = 0;
        differ(self.was.as_table(), self.document.as_table(), &mut changes);
        changes
    }

    /// Whether one value differs from the file on disk.
    pub(crate) fn changed(&self, section: &Section, key: &str) -> bool {
        let was = in_document(&self.was, section, key);
        let now = in_document(&self.document, section, key);
        match (was, now) {
            (None, None) => false,
            (Some(was), Some(now)) => plain(&was) != plain(&now),
            _ => true,
        }
    }

    /// What the loader says about the document as it stands.
    pub(crate) fn refused(&self) -> Option<&str> {
        self.refused.as_deref()
    }

    /// Whether the loader has not caught up with the typing yet.
    ///
    /// While this is true the last verdict is about a document nobody
    /// is looking at any more, so a window shows nothing rather than
    /// something that was true two letters ago.
    pub(crate) fn checking(&self) -> bool {
        self.stale
    }

    /// What the loader would take the file but still say about it.
    pub(crate) fn warned(&self) -> &[String] {
        &self.warned
    }

    /// Writes the edited document, if the loader takes it and nobody
    /// else has touched the file since it was read.
    pub(crate) fn save(&mut self) -> Option<String> {
        let sheet = self;
        if sheet.pending() == 0 {
            return None;
        }
        // Typing may have outrun the loader. Ask before writing: the
        // gate is the whole point, and a stale yes is not one.
        sheet.settle();
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
                let edits = sheet.pending();
                sheet.text = text;
                sheet.was = sheet.document.clone();
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

    /// An edit must not turn one section into two. A file written before
    /// the rename says `[advanced]`, the parser still reads it, and a
    /// window that wrote `[experimental]` beside it would leave a file
    /// that names the same table twice — which the parser refuses, on
    /// the next save, for a change the person did not make.
    #[test]
    fn a_file_that_says_advanced_goes_on_saying_advanced() {
        let path = std::path::Path::new("config.toml");
        let text = "[advanced]\nallow_root = false\n\n\
                    [groups.notes]\nalpha = \"/tmp/a\"\nbetas = [\"/tmp/b\"]\n";
        let mut document: toml_edit::DocumentMut = text.parse().expect("the file parses");

        table_for(&mut document, &Section::Alerts)
            .expect("the table is there")
            .insert("coalesce_after", toml_edit::value("5s"));
        let written = document.to_string();
        assert!(written.contains("[advanced.alerts]"), "{written}");
        assert!(!written.contains("[experimental"), "{written}");
        crate::config::Config::parse(path, &written).expect("the parser takes it");

        // A file with neither gets the name the section has now.
        let mut fresh: toml_edit::DocumentMut = "".parse().expect("an empty file parses");
        table_for(&mut fresh, &Section::Advanced)
            .expect("the table is made")
            .insert("allow_root", toml_edit::value(true));
        assert!(fresh.to_string().contains("[experimental]"), "{fresh}");
    }

    /// Not every refusal is a list under a heading. A file the loader
    /// turns down for one thing says so in one sentence, and that
    /// sentence is the fault — dropping it left the window with a red
    /// dot and nothing to show for it.
    #[test]
    fn a_refusal_that_is_one_sentence_is_one_fault() {
        let alone = "log: unknown log level \"loud\" (available: quiet, normal, debug)";
        assert_eq!(faults(alone), vec![alone.to_owned()]);
        assert_eq!(fault_at(alone).map(|at| at.key), Some("log".to_owned()));
    }

    /// Eight lines that differ only in a group name are one fault, and
    /// the defaults are where somebody would go to fix it.
    #[test]
    fn one_bad_value_inherited_everywhere_is_one_fault() {
        let refusal = format!(
            "invalid configuration:\n{}",
            ["  the defaults' ignore_files: no ignore file named \"asdfasdf\" in /x"; 3]
                .join("\n")
        );
        let one = faults(&refusal);
        assert_eq!(one.len(), 1, "{one:?}");
        assert!(one[0].starts_with("the defaults' ignore_files"), "{one:?}");
        assert_eq!(blamed(&refusal), Some(Section::Defaults));

        // Two different faults stay two, and a group's own fault keeps
        // the group in front of it — which is how it finds its field.
        let its_own = "invalid configuration:\n  group 'aws': max_file_size: invalid size \
                       'asdf'\n  group 'fny': mode: unknown mode 'sideways'";
        let both = faults(its_own);
        assert_eq!(both.len(), 2, "{both:?}");
        let at = fault_at(&both[0]).expect("a group's own key");
        assert_eq!(at.section, Section::Group("aws".to_owned()));
        assert_eq!(at.key, "max_file_size");
        assert_eq!(blamed(its_own), Some(Section::Group("aws".to_owned())));
    }

    /// Every character Markdown reads as punctuation comes back as
    /// itself, and nothing else is touched.
    #[test]
    fn a_sentence_survives_being_drawn_as_markdown() {
        let said = "no ignore file named \"a_b*c\" in ~/.autobahn/ignores (available: x.y)";
        let written = as_written(said);
        assert!(written.contains("a\\_b\\*c"), "{written}");
        assert!(written.contains("\\(available"), "{written}");
        // And unescaping it gives back exactly what went in, so nothing
        // was dropped or doubled on the way.
        assert_eq!(written.replace('\\', ""), said.replace('\\', ""));
    }

    /// A complaint that names its key belongs under that key, and the
    /// filenames it offered are a choice rather than a sentence.
    #[test]
    fn a_fault_that_names_its_key_lands_on_that_field() {
        let at = fault_at(
            "the defaults' ignore_files: no ignore file named \"asdfasdf\" in \
             /Users/x/.autobahn/ignores (available: Node.gitignore, Rust.gitignore)",
        )
        .expect("it names a key");
        assert_eq!(at.section, Section::Defaults);
        assert_eq!(at.key, "ignore_files");
        assert_eq!(at.instead, vec!["Node.gitignore", "Rust.gitignore"]);
        assert!(!at.said.contains("available"), "{}", at.said);
        assert!(at.said.starts_with("no ignore file named"), "{}", at.said);

        let mine = fault_at("group 'aws': max_file_size: invalid size 'asdf'")
            .expect("a group's own key");
        assert_eq!(mine.section, Section::Group("aws".to_owned()));
        assert_eq!(mine.key, "max_file_size");
        assert!(mine.instead.is_empty());

        // A complaint that is a sentence, not a key, belongs nowhere in
        // particular and must not be forced under a field.
        assert!(fault_at("group 'aws': a peering mode needs a local alpha").is_none());
        assert!(fault_at("sessions 'a' and 'b': endpoint nested").is_none());
    }

    /// The rest of the file: a bare key is the top of it, and a dotted
    /// one names the table it is in.
    #[test]
    fn a_fault_anywhere_in_the_file_finds_its_field() {
        let top = fault_at("log: unknown log level \"loud\" (available: quiet, normal, debug)")
            .expect("a key at the top of the file");
        assert_eq!(top.section, Section::Settings);
        assert_eq!(top.key, "log");
        assert_eq!(top.instead, vec!["quiet", "normal", "debug"]);

        let timing = fault_at("experimental.alerts.settle_after: invalid duration 'soon'")
            .expect("a key in the alert timing");
        assert_eq!(timing.section, Section::Alerts);
        assert_eq!(timing.key, "settle_after");

        let lease = fault_at(
            "experimental.peering-dangerously-experimental.ttl: invalid duration 'soon'",
        )
        .expect("a key in the lease timing");
        assert_eq!(lease.section, Section::Peering);
        assert_eq!(lease.key, "ttl");

        let root = fault_at("experimental.allow_root: not a boolean").expect("a key in the table");
        assert_eq!(root.section, Section::Advanced);

        // And a sentence with a colon in it is still not a key.
        assert!(fault_at("unable to read configuration /x: no such file").is_none());
    }

    /// A hidden key is still a key: if one is renamed in the structs the
    /// parser reads, the list must not go on hiding a name nothing has —
    /// the field would come back to the top of the form, in the middle
    /// of the ordinary settings, and nobody would be told.
    #[test]
    fn every_hidden_key_is_a_key_the_file_really_has() {
        let shape = crate::config::schema();
        // A key belongs to the top of the file, to a group, or to both
        // — but it must belong somewhere, or the list is hiding a name
        // nothing has and the field would quietly come back.
        let places = [
            shape["properties"].as_object(),
            shape["$defs"]["Group"]["properties"].as_object(),
        ];
        for key in EXPERIMENTAL {
            assert!(
                places
                    .iter()
                    .flatten()
                    .any(|properties| properties.contains_key(*key)),
                "nothing in the file has a '{key}' to hide"
            );
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
        assert_eq!(refusal(path, text), Ok(Vec::new()));

        let mut document: toml_edit::DocumentMut = text.parse().unwrap();
        let table = table_for(&mut document, &Section::Group("a".to_owned())).unwrap();
        table.insert(
            "ignores",
            toml_edit::value(toml_edit::Array::from_iter(["["])),
        );
        let broken = document.to_string();
        crate::config::Config::parse(path, &broken)
            .expect("serde takes it: a list of strings is a list of strings");
        let complaint = refusal(path, &broken).expect_err("the loader does not");
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

