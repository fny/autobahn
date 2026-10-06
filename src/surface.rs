//! What a window over the fleet is, apart from its drawing.
//!
//! `crate::app` draws it and owns none of it. The sections of the
//! configuration file and how a value is written back, what a
//! conflict's two sides hold, what a session is waiting on and how it
//! is grouped, the words for a size or an age — all of it is here,
//! where it can be tested without opening a window.

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
    pub(crate) primary_root: String,
    pub(crate) replica_root: String,
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
    /// `[experimental.p2p-dangerously-experimental]`: the lease timing.
    P2P,
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
            Section::P2P => t("config.advanced_p2p").to_owned(),
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
    /// `primary` or `replica`, the words every other surface uses.
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
    /// Whether any execute bit is set, which is the scanner's own rule
    /// (`scan::MODE_EXECUTABLE_MASK`). Two files the engine calls equal
    /// must match here as well as in their digest, so a card that
    /// compares only digests can say two sides agree when the thing
    /// they disagree about is this.
    pub(crate) executable: bool,
    /// Why there is nothing else to say: another machine, or gone.
    pub(crate) trouble: Option<String>,
}

/// What two sides that hash the same are actually disagreeing about.
///
/// Identical content is not a conflict: reconciliation compares digest
/// *and* executability, and two entries equal in both never reach the
/// conflicts pane at all — the ancestor simply advances. So a card
/// showing two matching digests is looking at one of two other things,
/// and saying "either choice keeps it" is wrong about the first.
pub(crate) enum Agreement {
    /// Same bytes, different mode. The choice decides whether the file
    /// stays executable, which is exactly what is being conflicted over.
    OnlyTheMode { executable: &'static str },
    /// Same bytes, same mode. The conflict was recorded by an earlier
    /// cycle and something has settled it since — a copy by hand, most
    /// likely. Nothing needs choosing; the next cycle clears it.
    Settled,
}

/// The verdict, when both sides hashed and hashed the same.
pub(crate) fn agreement(primary: &Side, replica: &Side) -> Option<Agreement> {
    if primary.digest.is_none() || primary.digest != replica.digest {
        return None;
    }
    match (primary.executable, replica.executable) {
        (true, false) => Some(Agreement::OnlyTheMode {
            executable: primary.name,
        }),
        (false, true) => Some(Agreement::OnlyTheMode {
            executable: replica.name,
        }),
        _ => Some(Agreement::Settled),
    }
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
            executable: false,
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
        executable: false,
        trouble: None,
    };
    match std::fs::symlink_metadata(&file) {
        Ok(metadata) => {
            side.size = Some(metadata.len());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                side.executable = metadata.permissions().mode() & 0o111 != 0;
            }
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
            side.trouble = Some(fill(
                "conflicts.unreadable",
                &[("reason", &error.to_string())],
            ))
        }
    }
    side
}

/// Reads the file once: says whether it looks binary, and hashes it when
/// it is small enough to be worth hashing.
pub(crate) fn read_through(
    file: &std::path::Path,
    size: u64,
) -> std::io::Result<(bool, Option<String>)> {
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
    Ok((binary, hash.then(|| hasher.finalize().to_hex().to_string())))
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

/// Keys the form does not show at the top of the file, because each has
/// a place of its own in the sidebar.
pub(crate) const SILENT_AT_THE_TOP: &[&str] = &["groups", "defaults", "experimental"];

/// The same, for `[experimental]`: its two timing tables are sections of
/// their own.
pub(crate) const SILENT_IN_ADVANCED: &[&str] = &["alerts", "p2p-dangerously-experimental"];

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
/// been let in. See `AutobahnApp::unlocked`.
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
        Section::Advanced => vec![Section::Advanced, Section::Alerts, Section::P2P],
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
        Section::P2P => experimental(document)?
            .entry("p2p-dangerously-experimental")
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

/// The `[experimental]` table, made if the file has none.
fn experimental(document: &mut toml_edit::DocumentMut) -> Option<&mut toml_edit::Table> {
    document
        .entry("experimental")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
}

/// What one document holds for a key of a section.
fn in_document(
    document: &toml_edit::DocumentMut,
    section: &Section,
    key: &str,
) -> Option<toml_edit::Item> {
    let tuning = || document.get("experimental");
    let table: &toml_edit::Item = match section {
        Section::Settings => document.as_item(),
        Section::Defaults => document.get("defaults")?,
        Section::Advanced => tuning()?,
        Section::Alerts => tuning()?.get("alerts")?,
        Section::P2P => tuning()?.get("p2p-dangerously-experimental")?,
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
    let heading = match refusal.lines().next() {
        Some("invalid configuration:") => 1,
        _ => 0,
    };
    for line in refusal.lines().skip(heading) {
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
/// ignores: …", "group 'aws': max_file_size: …" — which is what
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
                    "experimental" => Section::Advanced,
                    "experimental.alerts" => Section::Alerts,
                    table if table.contains("p2p") => Section::P2P,
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
            '\\' | '`'
                | '*'
                | '_'
                | '{'
                | '}'
                | '['
                | ']'
                | '('
                | ')'
                | '#'
                | '+'
                | '-'
                | '.'
                | '!'
                | '|'
                | '<'
                | '>'
                | '~'
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

/// Whether a log line is the supervisor complaining.
pub(crate) fn is_complaint(line: &str) -> bool {
    line.contains(" error:") || line.contains("refused")
}

/// Digits a person can read at a glance.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// What went wrong, out of a session's error, for a column with room for
/// one thing. An unreachable host's error is wrapped twice: "unable to
/// reach X: " in front and ": unable to synchronize with X" behind, and
/// the row already names X. What is left is the reason: the ssh message.
pub(crate) fn cause(error: &str) -> String {
    let error = error.trim();
    let without_tail = match error.rfind(": unable to synchronize with ") {
        Some(at) => &error[..at],
        None => error,
    };
    let without_head = match without_tail.strip_prefix("unable to reach ") {
        Some(rest) => rest
            .split_once(": ")
            .map(|(_, rest)| rest)
            .unwrap_or(without_tail),
        None => without_tail,
    };
    match without_head.trim() {
        "" => error.to_owned(),
        cause => cause.to_owned(),
    }
}

/// A message as it goes to the clipboard: every line of it, with the
/// line breaks kept and any other control character written out, so
/// what is pasted into a terminal is text and nothing else.
pub(crate) fn for_the_clipboard(said: &str) -> String {
    said.lines()
        .map(|line| crate::text::display_safe(line).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
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

/// The short name of a replica: the host it is on, or the last part of the
/// path when it is a directory on this machine. Long enough to tell two
/// apart, short enough to sit on a button.
pub(crate) fn short_name(replica: &str) -> String {
    match replica.split_once(':') {
        Some((host, _)) if !host.starts_with('/') && !host.starts_with('~') => host.to_owned(),
        _ => replica.rsplit('/').next().unwrap_or(replica).to_owned(),
    }
}

/// What can be asked of the login service, from a window.
///
/// The service is this user's own — the same launchd job or systemd unit
/// `autobahn install` registers — so the window calls the library
/// straight rather than shelling out to itself. Installing carries the
/// config and state root the window was opened with: a service pointed
/// at a different file than the form is editing is the one failure here
/// that looks like nothing at all.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    Start,
    Stop,
    Restart,
    Install,
    Uninstall,
}

impl Order {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Order::Start => t("service.start"),
            Order::Stop => t("service.stop"),
            Order::Restart => t("service.restart"),
            Order::Install => t("service.install"),
            Order::Uninstall => t("service.uninstall"),
        }
    }

    pub(crate) fn about(self) -> &'static str {
        match self {
            Order::Start => t("tip.service_start"),
            Order::Stop => t("tip.service_stop"),
            Order::Restart => t("tip.service_restart"),
            Order::Install => t("tip.service_install"),
            Order::Uninstall => t("tip.service_uninstall"),
        }
    }

    /// The word for what just happened, for the status line.
    fn done(self) -> &'static str {
        match self {
            Order::Start => t("service.started"),
            Order::Stop => t("service.stopped"),
            Order::Restart => t("service.restarted"),
            Order::Install => t("service.installed"),
            Order::Uninstall => t("service.uninstalled"),
        }
    }
}

/// What the service is doing, or why that cannot be said.
pub(crate) fn service_state() -> Option<crate::service::ServiceState> {
    crate::service::state().ok()
}

/// The orders that make sense in a given state. An install that is
/// already installed, or a stop of something that is not running, are
/// not offered rather than offered and refused.
pub(crate) fn orders(state: crate::service::ServiceState) -> Vec<Order> {
    use crate::service::ServiceState::*;
    match state {
        NotInstalled => vec![Order::Install],
        Stopped => vec![Order::Start, Order::Uninstall],
        Running => vec![Order::Stop, Order::Restart, Order::Uninstall],
    }
}

/// Carries one out, and says what happened either way.
pub(crate) fn ask(
    order: Order,
    config: Option<&std::path::Path>,
    state_root: &std::path::Path,
) -> Result<String, String> {
    let done = match order {
        Order::Start => crate::service::start(),
        Order::Stop => crate::service::stop(),
        Order::Restart => crate::service::restart(),
        // The window's own config and state root, not the defaults: a
        // service watching a different file than this form edits would
        // look like the form doing nothing.
        // The command's path, not this program's: the service runs
        // `autobahn watch`, and this program is autobahn-app.
        Order::Install => match found() {
            Some(command) => crate::service::install(&command, config, Some(state_root)),
            None => Err(anyhow::anyhow!("the autobahn command is not installed")),
        },
        Order::Uninstall => crate::service::uninstall(),
    };
    match done {
        Ok(()) => Ok(order.done().to_owned()),
        Err(error) => Err(fill(
            "service.refused",
            &[
                ("order", order.label()),
                ("error", &first_line(&format!("{error:#}"))),
            ],
        )),
    }
}

/// Downloads the latest release over this one, and says what happened.
///
/// The same work `autobahn update` does, through the same library: the
/// command and the agent bundle the controller streams to hosts, with
/// the login service pointed at the new binary and restarted. It takes
/// as long as a download takes, so a window calls it off the main
/// thread and shows what came back.
pub(crate) fn update() -> Result<String, String> {
    match crate::update::run(crate::update::Options {
        version: None,
        bin_dir: None,
        no_agents: false,
        dry_run: false,
        // A service registered against the old path keeps working only
        // if it is pointed at the new one.
        retarget: true,
    }) {
        Ok(()) => Ok(t("service.updated").to_owned()),
        Err(error) => Err(fill(
            "service.refused",
            &[
                ("order", t("service.update")),
                ("error", &first_line(&format!("{error:#}"))),
            ],
        )),
    }
}

/// What the button that opens a file manager should be called, which is
/// not the same word on every desktop.
pub(crate) fn reveal_label() -> &'static str {
    match cfg!(target_os = "macos") {
        true => t("conflicts.reveal_finder"),
        false => t("conflicts.reveal_folder"),
    }
}

/// Shows a file where it lives, and says what happened.
///
/// macOS has one answer for this and Linux has none: there is no
/// portable "open the folder and select this". The file managers that
/// can do it are asked first, by name, and the fallback opens the
/// folder and leaves the finding to the person — which is still better
/// than the `open -R` this used to run everywhere, which on Linux is
/// either missing or a program for opening virtual consoles.
pub(crate) fn reveal(file: &std::path::Path) -> Result<String, String> {
    #[cfg_attr(target_os = "macos", allow(unused_variables))]
    let folder = file.parent().unwrap_or(file);
    let shown = tilde(&file.display().to_string());
    #[cfg(target_os = "macos")]
    let tried: Vec<(&str, Vec<&std::ffi::OsStr>)> =
        vec![("open", vec!["-R".as_ref(), file.as_os_str()])];
    #[cfg(not(target_os = "macos"))]
    let tried: Vec<(&str, Vec<&std::ffi::OsStr>)> = vec![
        ("nautilus", vec!["--select".as_ref(), file.as_os_str()]),
        ("dolphin", vec!["--select".as_ref(), file.as_os_str()]),
        ("nemo", vec![file.as_os_str()]),
        ("thunar", vec![folder.as_os_str()]),
        ("xdg-open", vec![folder.as_os_str()]),
    ];
    let mut last = None;
    for (program, arguments) in tried {
        match std::process::Command::new(program).args(arguments).status() {
            Ok(status) if status.success() => {
                return Ok(fill("status.revealed", &[("path", &shown)]));
            }
            // A file manager that is not installed is not a failure to
            // report; it is the next one's turn.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(status) => last = Some(status.to_string()),
            Err(error) => last = Some(error.to_string()),
        }
    }
    match last {
        Some(why) => Err(fill("status.finder_refused", &[("status", &why)])),
        None => Err(fill("status.no_file_manager", &[("path", &shown)])),
    }
}

/// Every directory worth asking, in the order it is worth asking it.
///
/// A window opened from the Finder inherits almost no PATH — not the
/// shell's, and in particular not `~/.local/bin`, which is where the
/// installer puts the command. So these are asked for by name. PATH is
/// asked as well, in `found`, for a window started from a terminal; it
/// is not listed here because a developer's PATH is thirty entries of
/// noise and the welcome pane has to be readable.
pub(crate) fn looked_in() -> Vec<PathBuf> {
    let mut places = Vec::new();
    let mut add = |folder: PathBuf| {
        if !places.contains(&folder) {
            places.push(folder);
        }
    };
    if let Some(beside) = std::env::current_exe()
        .ok()
        .and_then(|here| here.parent().map(std::path::Path::to_path_buf))
    {
        add(beside);
    }
    if let Some(home) = std::env::var_os("HOME") {
        add(PathBuf::from(&home).join(".local").join("bin"));
    }
    add(PathBuf::from("/usr/local/bin"));
    add(PathBuf::from("/opt/homebrew/bin"));
    places
}

/// Whether a file is there and can be run. A directory of the right
/// name, or a file with no execute bit, is not the command.
fn runnable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|about| about.is_file() && about.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    path.is_file()
}

/// Where to look for the command instead of looking.
///
/// `scripts/views.sh` sets this: at a shim that answers like `autobahn`,
/// or at a path that is not there, which is the only way to see the
/// welcome pane on a machine that has the real command installed —
/// `looked_in` names absolute folders, so no amount of PATH will hide
/// one that is sitting in `/usr/local/bin`.
pub(crate) const TOLD_WHERE: &str = "AUTOBAHN_BIN";

/// The `autobahn` command, if this machine has one.
///
/// Not `current_exe`, which is what this used to be: the kit window is
/// its own binary, so every `resolve` and `diff` it ran was handed to
/// `autobahn-app`, which answered "unknown argument resolve" and
/// looked like a button that did nothing.
pub(crate) fn found() -> Option<PathBuf> {
    // Being told beats looking, in both directions: a path that is not
    // runnable means there is no command, rather than meaning carry on
    // searching.
    if let Some(told) = std::env::var_os(TOLD_WHERE) {
        let told = PathBuf::from(told);
        return runnable(&told).then_some(told);
    }
    if let Ok(here) = std::env::current_exe() {
        if here.file_name().is_some_and(|name| name == "autobahn") {
            return Some(here);
        }
    }
    let on_path = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<PathBuf>>())
        .unwrap_or_default();
    looked_in()
        .into_iter()
        .chain(on_path)
        .map(|folder| folder.join("autobahn"))
        .find(|candidate| runnable(candidate))
}

/// The command, or its bare name — which fails the same way every other
/// missing command does, and is what the welcome pane exists to avoid.
pub(crate) fn exe() -> PathBuf {
    found().unwrap_or_else(|| PathBuf::from("autobahn"))
}

/// Whether there is anything to shell out to at all. Every button in
/// the window that is not the welcome pane depends on this.
pub(crate) fn installed() -> bool {
    found().is_some()
}

/// The installer, as it was when this window was built.
///
/// Embedded rather than fetched: running a shell script downloaded at
/// the moment of use would put the weakest link at the end of the
/// chain. The release it downloads is still checked — the script tests
/// every file against the release's SHA256SUMS, and against the
/// signing key when minisign is installed — but the script doing the
/// checking is the one that shipped here.
pub(crate) const INSTALLER: &str = include_str!("../scripts/install.sh");

/// The same thing by hand, for somebody who would rather read it first.
pub(crate) const INSTALL_LINE: &str =
    "curl -fsSL https://github.com/fny/autobahn/releases/latest/download/install.sh | sh";

/// Where the installer's account of itself is kept. The log pane tails
/// this like any other log, which is how the install is watched.
pub(crate) fn install_log(state_root: &std::path::Path) -> PathBuf {
    state_root.join("install.log")
}

/// Runs the embedded installer, with everything it says going to
/// `install.log` as it is said rather than at the end.
///
/// The script's own default directory is `~/.local/bin`, so no
/// `--bin-dir` is passed: this is the documented one-liner, run from
/// here. It takes as long as a download takes, so it belongs on a
/// background thread.
pub(crate) fn install(state_root: &std::path::Path) -> Result<PathBuf, String> {
    use std::io::Write;
    let blame = |what: &str, error: std::io::Error| format!("{what}: {error}");
    std::fs::create_dir_all(state_root).map_err(|error| blame("the state root", error))?;

    // Written where it can be read afterwards, and removed when it is
    // done: a script that runs is a script somebody may want to see.
    let script = std::env::temp_dir().join(format!("autobahn-install-{}.sh", std::process::id()));
    std::fs::write(&script, INSTALLER).map_err(|error| blame("the installer", error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700));
    }

    let path = install_log(state_root);
    let log = std::fs::File::create(&path).map_err(|error| blame("the install log", error))?;
    let errors = log
        .try_clone()
        .map_err(|error| blame("the install log", error))?;

    // The child writes straight into the file, so the log grows while
    // the install runs rather than arriving in one piece at the end.
    let mut command = std::process::Command::new("sh");
    command.arg(&script);
    command.stdout(std::process::Stdio::from(log));
    command.stderr(std::process::Stdio::from(errors));
    // A window started from the Finder has almost no PATH, and the
    // script needs curl, tar and uname. The inherited PATH is kept
    // after them so a terminal's own answer still wins where it has one.
    let inherited = std::env::var("PATH").unwrap_or_default();
    command.env("PATH", format!("/usr/bin:/bin:/usr/sbin:/sbin:{inherited}"));

    let status = command.status().map_err(|error| blame("sh", error));
    let _ = std::fs::remove_file(&script);
    let status = status?;

    let mut note = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .map_err(|error| blame("the install log", error))?;
    match found() {
        Some(command) if status.success() => {
            let _ = writeln!(note, "installed: {}", command.display());
            Ok(command)
        }
        Some(command) => {
            let _ = writeln!(
                note,
                "the installer failed, but a command is here: {}",
                command.display()
            );
            Ok(command)
        }
        None => {
            let _ = writeln!(note, "the installer finished and no command is here");
            Err(fill(
                "welcome.install_failed",
                &[("status", &status.to_string())],
            ))
        }
    }
}

/// Which build the command is, in its own words, and where it is.
///
/// The app and the command are two programs, installed and updated
/// apart, so the app's own version says nothing about the command's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandBuild {
    /// What `autobahn --version` prints after the name.
    pub version: String,
    pub path: PathBuf,
}

/// Asks the command which build it is. It runs the command, so it is
/// asked once and again after an install or an update, not every frame.
pub(crate) fn command_build() -> Option<CommandBuild> {
    let path = found()?;
    let output = std::process::Command::new(&path)
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let said = String::from_utf8_lossy(&output.stdout);
    let said = said.trim();
    Some(CommandBuild {
        version: said.strip_prefix("autobahn ").unwrap_or(said).to_owned(),
        path,
    })
}

/// Which build the running supervisor is, as far as it can be asked.
///
/// A supervisor is the command as it was when the service last started
/// it: updating the command changes the file and not the process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SupervisorBuild {
    /// Running, and this app's own version; with its build, commit and
    /// all, when it could be asked.
    Same(Option<String>),
    /// Running another version: the one it named, or one from before
    /// supervisors could say.
    Other(Option<String>),
    /// Running, and not answering.
    Silent,
    /// Not running.
    Absent,
}

/// Asks the running supervisor which build it is.
pub(crate) fn supervisor_build(state_root: &std::path::Path) -> SupervisorBuild {
    use crate::supervisor::control::Probe;
    match crate::supervisor::control::probe(state_root) {
        Probe::Answered(_) => SupervisorBuild::Same(crate::supervisor::control::build(state_root)),
        Probe::Mismatch(version) => SupervisorBuild::Other(version),
        Probe::Unresponsive => SupervisorBuild::Silent,
        Probe::Absent => SupervisorBuild::Absent,
    }
}

impl SupervisorBuild {
    /// The line for it under "this build".
    pub(crate) fn said(&self) -> String {
        match self {
            SupervisorBuild::Same(build) => fill(
                "service.build_running",
                &[(
                    "version",
                    build.as_deref().unwrap_or(&crate::protocol::version()),
                )],
            ),
            SupervisorBuild::Other(Some(version)) => {
                fill("service.build_running", &[("version", version)])
            }
            SupervisorBuild::Other(None) => t("service.build_older").to_owned(),
            SupervisorBuild::Silent => t("service.build_silent").to_owned(),
            SupervisorBuild::Absent => t("service.build_absent").to_owned(),
        }
    }

    /// Its package version, when it is running and said one. An empty
    /// one for a supervisor too old to say: it matches nothing.
    fn package(&self) -> Option<String> {
        match self {
            // Its own words when it said them; this build's otherwise,
            // which is what "same" means.
            SupervisorBuild::Same(build) => {
                Some(package(build.as_deref().unwrap_or(&crate::protocol::version())).to_owned())
            }
            SupervisorBuild::Other(version) => {
                Some(package(version.as_deref().unwrap_or_default()).to_owned())
            }
            SupervisorBuild::Silent | SupervisorBuild::Absent => None,
        }
    }
}

/// What to do when the app, the command and the supervisor are not one
/// build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Advice {
    /// The command is older than the app: update it.
    UpdateCommand,
    /// The supervisor is not the command that is installed: restart it.
    Restart,
    /// The app is older than the command: get the newer app.
    NewerApp,
}

impl Advice {
    pub(crate) fn said(self) -> &'static str {
        match self {
            Advice::UpdateCommand => t("service.advice_update"),
            Advice::Restart => t("service.advice_restart"),
            Advice::NewerApp => t("service.advice_app"),
        }
    }
}

/// The package version inside a build's description: `1.0.0` out of
/// `1.0.0+e1 (1941064)` and out of `1.0.0 (1941064)`.
fn package(build: &str) -> &str {
    let end = build
        .find(|character: char| character == '+' || character.is_whitespace())
        .unwrap_or(build.len());
    &build[..end]
}

/// The commit inside a build's description: `a1b2c3d` out of
/// `1.0.1+e1 (a1b2c3d)`, `a1b2c3d, modified` out of a tree with changes,
/// and nothing out of a build that named none.
fn commit_of(build: &str) -> Option<&str> {
    let start = build.find('(')? + 1;
    let end = build[start..].find(')')? + start;
    Some(build[start..end].trim())
}

/// A package version's numbers, for telling older from newer.
fn numbers(package: &str) -> Vec<u64> {
    package
        .split(|character: char| !character.is_ascii_digit())
        .take_while(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// What to do about the three builds, most useful first, or nothing.
///
/// An older command comes first: the update restarts the service
/// itself, so it settles the supervisor too. Then a supervisor that is
/// not the installed command, which a restart settles. Last an app
/// older than its command, which only a download settles. Two builds of
/// one version are told apart by their commits, when the supervisor
/// could say which it was built from.
pub(crate) fn advice(
    app: &str,
    command: Option<&CommandBuild>,
    supervisor: &SupervisorBuild,
) -> Option<Advice> {
    let command = command?;
    let installed = package(&command.version);
    let app = package(app);
    if numbers(installed) < numbers(app) {
        return Some(Advice::UpdateCommand);
    }
    if supervisor
        .package()
        .is_some_and(|running| running != installed)
    {
        return Some(Advice::Restart);
    }
    if let SupervisorBuild::Same(Some(running)) = supervisor {
        if commit_of(running) != commit_of(&command.version) {
            return Some(Advice::Restart);
        }
    }
    (numbers(installed) > numbers(app)).then_some(Advice::NewerApp)
}

/// Registers and starts the login service once the installer has put the
/// command at `command`, and says how the whole install went: one press
/// on the welcome pane ends with a supervisor running, rather than with a
/// command and a page that says there is no supervisor.
///
/// Only over a configuration the supervisor would start on. A fresh
/// install's describes no sessions, and a service registered over that
/// exits at once and is started again every ten seconds until someone
/// adds a group; `autobahn start` refuses the same file for the same
/// reason. What happened goes to `install.log` as well.
pub(crate) fn serve_after_install(
    command: &std::path::Path,
    config: Option<&std::path::Path>,
    state_root: &std::path::Path,
) -> Result<String, String> {
    use std::io::Write;
    let path = tilde(&command.display().to_string());
    let registered = (|| -> anyhow::Result<bool> {
        let file = match config {
            Some(file) => file.to_path_buf(),
            None => crate::paths::default_config_path()?,
        };
        let loaded = crate::supervisor::reload::load(&file)?;
        if loaded.plans.is_empty() {
            return Ok(false);
        }
        crate::config::OwnState::new(state_root, Some(&file)).check_plans(&loaded.plans)?;
        crate::service::install(command, config, Some(state_root))?;
        Ok(true)
    })();
    let (line, said) = match registered {
        Ok(true) => (
            "installed and started the login service".to_owned(),
            Ok(fill("welcome.serving", &[("path", &path)])),
        ),
        Ok(false) => (
            "the configuration describes no sessions, so no login service was installed".to_owned(),
            Ok(fill("welcome.no_sessions", &[("path", &path)])),
        ),
        Err(error) => {
            let error = first_line(&format!("{error:#}"));
            (
                format!("the login service was not installed: {error}"),
                Err(fill(
                    "welcome.not_serving",
                    &[("path", &path), ("error", &error)],
                )),
            )
        }
    };
    if let Ok(mut note) = std::fs::OpenOptions::new()
        .append(true)
        .open(install_log(state_root))
    {
        let _ = writeln!(note, "{line}");
    }
    said
}

/// Whether a login service registered to run `executable` cannot be
/// running the command: it names the app, which v1.0.0 registered in the
/// command's place, or a file that is not there any more.
///
/// Deliberately no wider than that. A command kept under another name,
/// or somewhere this window would not look, is somebody's own
/// arrangement and is left alone.
fn wrong_program(executable: &std::path::Path) -> bool {
    executable
        .file_name()
        .is_some_and(|name| name == "autobahn-app")
        || !runnable(executable)
}

/// Points a login service that runs the wrong program at the command,
/// starts it, and says so. `None` when there was nothing to mend.
///
/// Start at login in v1.0.0 registered `autobahn-app watch`. Whoever
/// switched it on there still has that registration after updating the
/// app: a service file names one path, and nothing rewrites it but an
/// install. So the app looks when it opens. The arguments and the state
/// root are kept; only the program changes.
pub(crate) fn mend_service() -> Option<Result<String, String>> {
    let registered = crate::service::registration().ok()??;
    if !wrong_program(&registered.executable) {
        return None;
    }
    let command = found()?;
    if wrong_program(&command) {
        return None;
    }
    let was = tilde(&registered.executable.display().to_string());
    // Reloading the definition starts the service on macOS, so a restart
    // there killed the supervisor two seconds into its first cycle and
    // started a second one; `start` leaves a running one alone. systemd
    // only reloads, so there it is restarted.
    let begin = match cfg!(target_os = "macos") {
        true => crate::service::start,
        false => crate::service::restart,
    };
    let mended = crate::service::retarget(&command).and_then(|()| begin());
    Some(match mended {
        Ok(()) => Ok(fill(
            "service.mended",
            &[
                ("was", was.as_str()),
                ("now", &tilde(&command.display().to_string())),
            ],
        )),
        Err(error) => Err(fill(
            "service.not_mended",
            &[
                ("was", was.as_str()),
                ("error", &first_line(&format!("{error:#}"))),
            ],
        )),
    })
}

/// Runs one of the command's subcommands and keeps everything it said,
/// for a report that is read whole: `doctor`'s. [`ran`] keeps one line.
pub(crate) fn report(
    arguments: &[&str],
    config: Option<&std::path::Path>,
    state_root: &std::path::Path,
) -> Result<String, String> {
    let mut command = std::process::Command::new(exe());
    command.args(arguments);
    if let Some(path) = config {
        command.arg("--config").arg(path);
    }
    command.arg("--state-root").arg(state_root);
    let done = match command.output() {
        Ok(done) => done,
        Err(error) => {
            return Err(fill(
                "status.unreachable_command",
                &[("error", &error.to_string())],
            ))
        }
    };
    let out = String::from_utf8_lossy(&done.stdout).trim().to_owned();
    let err = String::from_utf8_lossy(&done.stderr).trim().to_owned();
    let said = match (out.is_empty(), err.is_empty()) {
        (false, true) => out,
        (true, false) => err,
        (false, false) => format!("{out}\n{err}"),
        (true, true) => fill("status.ran", &[("command", &arguments.join(" "))]),
    };
    match done.status.success() {
        true => Ok(said),
        false => Err(said),
    }
}

/// Runs one of the command's own subcommands and says what it said.
///
/// The window does the small things itself, through the library; the
/// large ones — a clean that walks the state root, a resolve that moves
/// files — are the command's, and asking it is how they stay one
/// implementation rather than two.
pub(crate) fn ran(arguments: &[&str], config: Option<&std::path::Path>) -> Result<String, String> {
    let mut command = std::process::Command::new(exe());
    command.args(arguments);
    if let Some(path) = config {
        command.arg("--config").arg(path);
    }
    match command.output() {
        Ok(done) if done.status.success() => {
            let said = String::from_utf8_lossy(&done.stdout);
            Ok(match said.trim().lines().last() {
                Some(line) if !line.trim().is_empty() => line.trim().to_owned(),
                _ => fill("status.ran", &[("command", &arguments.join(" "))]),
            })
        }
        Ok(done) => {
            let complained = String::from_utf8_lossy(&done.stderr);
            Err(first_line(complained.trim()))
        }
        Err(error) => Err(fill(
            "status.unreachable_command",
            &[("error", &error.to_string())],
        )),
    }
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
        // Alerts and p2p are not listed: they are drawn inside
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

    /// Makes a group, with the two keys without which it cannot load.
    ///
    /// A group needs a primary and at least one replica, so it is written
    /// with both, empty. That is refused by the loader — which is the
    /// point: the form opens on a section whose two required fields are
    /// red and say what they want, rather than on a table that quietly
    /// does nothing.
    pub(crate) fn make_group(&mut self, name: &str) -> Option<String> {
        if name.trim().is_empty() {
            return Some(t("config.group_needs_a_name").to_owned());
        }
        let name = name.trim();
        // A name a command could not pass, and one already taken.
        if name.starts_with('-') {
            return Some(t("config.group_leading_dash").to_owned());
        }
        if self.groups().iter().any(|held| held == name) {
            return Some(fill("config.group_exists", &[("name", name)]));
        }
        let mut document = self.document.clone();
        let Some(table) = table_for(&mut document, &Section::Group(name.to_owned())) else {
            return Some(fill("config.missing_section", &[("section", name)]));
        };
        table.insert("primary", toml_edit::value(""));
        table.insert("replicas", toml_edit::value(toml_edit::Array::new()));
        self.hold(document);
        None
    }

    /// Takes a group out of the file. Its state is not this window's to
    /// remove — `autobahn clean` is what does that — so the caller says
    /// so rather than leaving ancestors and staged content behind in
    /// silence.
    pub(crate) fn drop_group(&mut self, name: &str) -> Option<String> {
        let mut document = self.document.clone();
        let Some(groups) = document
            .get_mut("groups")
            .and_then(|item| item.as_table_mut())
        else {
            return Some(fill("config.no_such_group", &[("name", name)]));
        };
        if groups.remove(name).is_none() {
            return Some(fill("config.no_such_group", &[("name", name)]));
        }
        self.hold(document);
        None
    }

    /// Renames a group, keeping everything in it and where it sits.
    pub(crate) fn rename_group(&mut self, from: &str, to: &str) -> Option<String> {
        let to = to.trim();
        if to.is_empty() {
            return Some(t("config.group_needs_a_name").to_owned());
        }
        if to == from {
            return None;
        }
        if to.starts_with('-') {
            return Some(t("config.group_leading_dash").to_owned());
        }
        if self.groups().iter().any(|held| held == to) {
            return Some(fill("config.group_exists", &[("name", to)]));
        }
        let mut document = self.document.clone();
        let Some(groups) = document
            .get_mut("groups")
            .and_then(|item| item.as_table_mut())
        else {
            return Some(fill("config.no_such_group", &[("name", from)]));
        };
        let Some(held) = groups.remove(from) else {
            return Some(fill("config.no_such_group", &[("name", from)]));
        };
        groups.insert(to, held);
        self.hold(document);
        None
    }

    /// Every group the file names, in the order it names them.
    pub(crate) fn groups(&self) -> Vec<String> {
        self.document
            .get("groups")
            .and_then(|item| item.as_table())
            .map(|groups| groups.iter().map(|(name, _)| name.to_owned()).collect())
            .unwrap_or_default()
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
    pub(crate) fn save(&mut self) -> Option<Result<String, String>> {
        let sheet = self;
        if sheet.pending() == 0 {
            return None;
        }
        // Typing may have outrun the loader. Ask before writing: the
        // gate is the whole point, and a stale yes is not one.
        sheet.settle();
        if let Some(refused) = &sheet.refused {
            return Some(Err(fill(
                "config.not_saved",
                &[("reason", &crate::surface::first_line(refused))],
            )));
        }
        // Somebody may have been editing the same file in an editor
        // since it was read. Their work is not this window's to
        // overwrite.
        if let Ok(now) = std::fs::read_to_string(&sheet.path) {
            if now != sheet.text {
                sheet.refused = Some(t("config.changed").to_owned());
                return Some(Err(t("config.changed_short").to_owned()));
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
                Some(Ok(counted("config.saved", edits, &[("path", &path)])))
            }
            Err(error) => Some(Err(fill(
                "config.unwritable",
                &[("error", &error.to_string())],
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrappers around an unreachable host's error go; the reason
    /// stays. Any other error is shown whole.
    #[test]
    fn a_cause_is_the_error_without_its_wrappers() {
        assert_eq!(
            cause(
                "unable to reach dev@halle: ssh: Could not resolve hostname halle: nodename \
                 nor servname provided, or not known: unable to synchronize with dev@halle"
            ),
            "ssh: Could not resolve hostname halle: nodename nor servname provided, or not known"
        );
        assert_eq!(
            cause("unable to synchronize with dev@halle"),
            "unable to synchronize with dev@halle"
        );
        assert_eq!(
            cause("halted: the primary folder is missing"),
            "halted: the primary folder is missing"
        );
    }

    /// All of a message is copied, line breaks and all; a control
    /// character in it arrives written out, not as itself.
    #[test]
    fn a_copied_message_keeps_its_lines_and_no_control_characters() {
        assert_eq!(
            for_the_clipboard("update failed:\n  werk \u{1b}[31m→ halle\n  lack"),
            "update failed:\n  werk \\x1b[31m→ halle\n  lack"
        );
        assert_eq!(for_the_clipboard("one line"), "one line");
    }

    /// The command older than the app is updated; a supervisor that is
    /// not the installed command is restarted; an app older than its
    /// command is told so; and three of a kind are left alone.
    #[test]
    fn three_builds_that_differ_say_what_to_do() {
        let command = |version: &str| CommandBuild {
            version: version.to_owned(),
            path: PathBuf::from("/usr/local/bin/autobahn"),
        };
        let other = |version: &str| SupervisorBuild::Other(Some(version.to_owned()));
        let app = "1.0.1+e1 (a1b2c3d)";
        let same = command("1.0.1 (a1b2c3d)");
        assert_eq!(advice(app, Some(&same), &other("1.0.1+e1")), None);
        assert_eq!(advice(app, Some(&same), &SupervisorBuild::Absent), None);
        assert_eq!(advice(app, None, &other("0.9.0+e1")), None);
        let old = command("1.0.0 (1941064)");
        assert_eq!(
            advice(app, Some(&old), &other("1.0.0+e1")),
            Some(Advice::UpdateCommand)
        );
        assert_eq!(
            advice(app, Some(&same), &other("1.0.0+e1")),
            Some(Advice::Restart)
        );
        assert_eq!(
            advice(app, Some(&same), &SupervisorBuild::Other(None)),
            Some(Advice::Restart)
        );
        // One version, two commits: the supervisor says which it is.
        let same_build = SupervisorBuild::Same(Some("1.0.1+e1 (a1b2c3d)".to_owned()));
        assert_eq!(advice(app, Some(&same), &same_build), None);
        let other_commit = SupervisorBuild::Same(Some("1.0.1+e1 (0000000)".to_owned()));
        assert_eq!(
            advice(app, Some(&same), &other_commit),
            Some(Advice::Restart)
        );
        let modified = SupervisorBuild::Same(Some("1.0.1+e1 (a1b2c3d, modified)".to_owned()));
        assert_eq!(advice(app, Some(&same), &modified), Some(Advice::Restart));
        // A supervisor that could not say its build is this build's
        // version, whatever that is today.
        let now = env!("CARGO_PKG_VERSION");
        let this_app = format!("{now}+e1 (a1b2c3d)");
        let this_command = command(&format!("{now} (a1b2c3d)"));
        assert_eq!(
            advice(&this_app, Some(&this_command), &SupervisorBuild::Same(None)),
            None
        );
        let new = command("1.10.0");
        assert_eq!(
            advice(app, Some(&new), &other("1.10.0+e2")),
            Some(Advice::NewerApp)
        );
        assert_eq!(
            advice(app, Some(&new), &other("1.0.1+e1")),
            Some(Advice::Restart)
        );
    }

    /// The app, or nothing at all, is not the command; the command is,
    /// wherever it is kept.
    #[test]
    fn a_service_that_runs_the_app_or_a_missing_file_is_wrong() {
        use std::os::unix::fs::PermissionsExt;
        let keep = tempfile::tempdir().expect("a temporary directory");
        let program = |name: &str| {
            let path = keep.path().join(name);
            std::fs::write(&path, "#!/bin/sh\n").expect("written");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("executable");
            path
        };
        assert!(!wrong_program(&program("autobahn")));
        assert!(wrong_program(&program("autobahn-app")));
        assert!(wrong_program(&keep.path().join("gone")));
    }

    /// A file with no experimental table gets one under the name the
    /// section has now.
    #[test]
    fn a_fresh_file_gets_the_name_the_section_has() {
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
        let alone = "log_level: unknown log level \"loud\" (available: quiet, normal, debug)";
        assert_eq!(faults(alone), vec![alone.to_owned()]);
        assert_eq!(
            fault_at(alone).map(|at| at.key),
            Some("log_level".to_owned())
        );
    }

    /// Eight lines that differ only in a group name are one fault, and
    /// the defaults are where somebody would go to fix it.
    #[test]
    fn one_bad_value_inherited_everywhere_is_one_fault() {
        let refusal = format!(
            "invalid configuration:\n{}",
            ["  the defaults' ignores: no ignore file named \"asdfasdf\" in /x"; 3].join("\n")
        );
        let one = faults(&refusal);
        assert_eq!(one.len(), 1, "{one:?}");
        assert!(one[0].starts_with("the defaults' ignores"), "{one:?}");
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

    /// A matching digest does not mean "one file in two places". The
    /// engine calls two entries equal only when the mode matches too,
    /// so the card has to say which of the two remaining cases it is —
    /// and for one of them "either choice keeps it" was wrong.
    #[test]
    fn two_sides_that_hash_the_same_still_say_what_differs() {
        let side = |name: &'static str, digest: Option<&str>, executable: bool| Side {
            name,
            root: "/tmp".to_owned(),
            place: format!("/tmp/{name}"),
            file: None,
            size: Some(1),
            modified: None,
            digest: digest.map(str::to_owned),
            binary: true,
            executable,
            trouble: None,
        };

        // Same bytes, same mode: recorded earlier, settled since.
        let both = agreement(
            &side("primary", Some("aa"), false),
            &side("replica", Some("aa"), false),
        );
        assert!(matches!(both, Some(Agreement::Settled)));

        // Same bytes, and the mode is the whole disagreement. The side
        // that is executable is named, because the choice decides it.
        let mode = agreement(
            &side("primary", Some("aa"), false),
            &side("replica", Some("aa"), true),
        );
        assert!(
            matches!(
                mode,
                Some(Agreement::OnlyTheMode {
                    executable: "replica"
                })
            ),
            "the executable side is the one named"
        );

        // Different bytes, or a file too large to hash, says nothing.
        assert!(agreement(
            &side("primary", Some("aa"), false),
            &side("replica", Some("bb"), false)
        )
        .is_none());
        assert!(agreement(&side("primary", None, false), &side("replica", None, false)).is_none());
    }

    /// A complaint that names its key belongs under that key, and the
    /// filenames it offered are a choice rather than a sentence.
    #[test]
    fn a_fault_that_names_its_key_lands_on_that_field() {
        let at = fault_at(
            "the defaults' ignores: no ignore file named \"asdfasdf\" in \
             /Users/x/.autobahn/ignores (available: Node.gitignore, Rust.gitignore)",
        )
        .expect("it names a key");
        assert_eq!(at.section, Section::Defaults);
        assert_eq!(at.key, "ignores");
        assert_eq!(at.instead, vec!["Node.gitignore", "Rust.gitignore"]);
        assert!(!at.said.contains("available"), "{}", at.said);
        assert!(at.said.starts_with("no ignore file named"), "{}", at.said);

        let mine =
            fault_at("group 'aws': max_file_size: invalid size 'asdf'").expect("a group's own key");
        assert_eq!(mine.section, Section::Group("aws".to_owned()));
        assert_eq!(mine.key, "max_file_size");
        assert!(mine.instead.is_empty());

        // A complaint that is a sentence, not a key, belongs nowhere in
        // particular and must not be forced under a field.
        assert!(fault_at("group 'aws': a p2p mode needs a local primary").is_none());
        assert!(fault_at("sessions 'a' and 'b': endpoint nested").is_none());
    }

    /// The rest of the file: a bare key is the top of it, and a dotted
    /// one names the table it is in.
    #[test]
    fn a_fault_anywhere_in_the_file_finds_its_field() {
        let top =
            fault_at("log_level: unknown log level \"loud\" (available: quiet, normal, debug)")
                .expect("a key at the top of the file");
        assert_eq!(top.section, Section::Settings);
        assert_eq!(top.key, "log_level");
        assert_eq!(top.instead, vec!["quiet", "normal", "debug"]);

        let timing = fault_at("experimental.alerts.settle_after: invalid duration 'soon'")
            .expect("a key in the alert timing");
        assert_eq!(timing.section, Section::Alerts);
        assert_eq!(timing.key, "settle_after");

        let lease =
            fault_at("experimental.p2p-dangerously-experimental.ttl: invalid duration 'soon'")
                .expect("a key in the lease timing");
        assert_eq!(lease.section, Section::P2P);
        assert_eq!(lease.key, "ttl");

        let root = fault_at("experimental.allow_root: not a boolean").expect("a key in the table");
        assert_eq!(root.section, Section::Advanced);

        // And a sentence with a colon in it is still not a key.
        assert!(fault_at("unable to read configuration /x: no such file").is_none());
    }

    /// A group can be made, renamed and taken out, and a name that
    /// would not work is refused before the file is touched.
    #[test]
    fn a_group_can_be_made_renamed_and_taken_out() {
        let text = "[defaults]\nmode = \"two-way-conflict\"\n\n\
                    [groups.notes]\nprimary = \"/tmp/a\"\nreplicas = [\"/tmp/b\"]\n";
        let mut sheet = Sheet {
            path: std::path::PathBuf::from("config.toml"),
            text: text.to_owned(),
            was: text.parse().unwrap(),
            document: text.parse().unwrap(),
            refused: None,
            warned: Vec::new(),
            stale: false,
            quiet: false,
        };

        // Made with the two keys it cannot load without, both empty —
        // so the loader refuses it and the form says which fields want
        // filling, rather than a table that quietly does nothing.
        assert_eq!(sheet.make_group("work"), None);
        assert!(sheet.groups().contains(&"work".to_owned()));
        assert!(sheet
            .held(&Section::Group("work".to_owned()), "primary")
            .is_some());
        assert!(sheet.refused().is_some(), "an empty primary is refused");

        // A name that is taken, or that a command would read as an
        // option, is refused before anything is written.
        assert!(sheet.make_group("work").is_some());
        assert!(sheet.make_group("-x").is_some());
        assert!(sheet.make_group("  ").is_some());
        assert_eq!(sheet.groups().len(), 2);

        // Renaming keeps what is in it.
        assert_eq!(sheet.rename_group("notes", "reading"), None);
        assert_eq!(
            sheet
                .held(&Section::Group("reading".to_owned()), "primary")
                .map(|held| held.to_string().trim().to_owned()),
            Some("\"/tmp/a\"".to_owned())
        );
        assert!(sheet.rename_group("reading", "work").is_some(), "taken");
        assert!(
            sheet.rename_group("nothing", "x").is_some(),
            "no such group"
        );

        // And taking one out leaves the rest alone.
        assert_eq!(sheet.drop_group("work"), None);
        assert_eq!(sheet.groups(), vec!["reading".to_owned()]);
        assert!(sheet.drop_group("work").is_some(), "already gone");
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
                    primary = \"/tmp/a\"\n\
                    replicas = [\"/tmp/b\"]\n";
        let mut document: toml_edit::DocumentMut = text.parse().expect("the file parses");

        let table = table_for(&mut document, &Section::Group("notes".to_owned()))
            .expect("the group is in the file");
        table.insert("mode", toml_edit::value("one-way-primary"));
        let written = document.to_string();
        assert!(written.contains("# the fleet"), "{written}");
        assert!(written.contains("# both ways"), "{written}");
        assert!(written.contains("mode = \"one-way-primary\""), "{written}");
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
                    [groups.a]\nprimary = \"/tmp/a\"\nreplicas = [\"/tmp/b\"]\n";
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
            "[groups.a]\nprimary = \"/tmp/a\"\n".parse().unwrap();
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
