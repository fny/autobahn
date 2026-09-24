//! Whether output may be styled, decided in one place.
//!
//! Reports are written with autobahn's own escape sequences in them —
//! colour for the state words, bold for the folder, dim for the aside —
//! and filtered here on their way out. Colour is only for a person at a
//! terminal who has not asked for none: a pipe, a file, `TERM=dumb` and
//! `NO_COLOR` (<https://no-color.org>) all get the same words without it.
//! A script reading `status` sees text, not escape codes.
//!
//! `NO_COLOR` asks for no *colour*, and the convention leaves bold, dim and
//! reverse video alone. So a terminal with it set keeps those, which is
//! what lets the shop and the live display still show where the cursor is.

use std::borrow::Cow;
use std::io::IsTerminal;
use std::sync::OnceLock;

/// How much styling output may carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// None at all: plain text.
    Plain,
    /// Bold, dim, reverse video and the like, but no colour.
    Emphasis,
    /// Everything.
    Colour,
}

/// The level for a report written to standard output: colour only when
/// stdout is a terminal, `NO_COLOR` is unset or empty, and `TERM` is not
/// `dumb`. Decided once, since none of it changes while the command runs.
pub fn stdout_level() -> Level {
    static LEVEL: OnceLock<Level> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        decide(
            std::io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR"),
            std::env::var_os("TERM"),
        )
    })
}

/// The level for a full-screen display — the shop and the live status —
/// which runs on a terminal by definition, so only `NO_COLOR` matters.
pub fn screen_level() -> Level {
    static LEVEL: OnceLock<Level> = OnceLock::new();
    *LEVEL.get_or_init(|| decide(true, std::env::var_os("NO_COLOR"), None))
}

/// The rule itself, apart from where its inputs come from.
pub fn decide(
    terminal: bool,
    no_color: Option<std::ffi::OsString>,
    term: Option<std::ffi::OsString>,
) -> Level {
    if !terminal || term.as_deref() == Some(std::ffi::OsStr::new("dumb")) {
        return Level::Plain;
    }
    match no_color {
        Some(value) if !value.is_empty() => Level::Emphasis,
        _ => Level::Colour,
    }
}

/// `text` with its styling reduced to what `level` allows. The words are
/// never touched: only `ESC [ … m` sequences are removed, or have their
/// colour parameters taken out.
pub fn apply(text: &str, level: Level) -> Cow<'_, str> {
    if level == Level::Colour || !text.contains("\x1b[") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("\x1b[") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let parameters = tail
            .find(|c: char| !(c.is_ascii_digit() || c == ';'))
            .unwrap_or(tail.len());
        if !tail[parameters..].starts_with('m') {
            // Not a style sequence; not ours to judge.
            out.push_str("\x1b[");
            rest = tail;
            continue;
        }
        if level == Level::Emphasis {
            let kept = emphasis_only(&tail[..parameters]);
            if !kept.is_empty() || tail[..parameters].is_empty() {
                out.push_str("\x1b[");
                out.push_str(&kept);
                out.push('m');
            }
        }
        rest = &tail[parameters + 1..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The parameters of one style sequence without its colours: foreground
/// and background, basic, bright, 256-colour and true-colour.
fn emphasis_only(parameters: &str) -> String {
    let codes: Vec<&str> = parameters.split(';').collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < codes.len() {
        let code: u32 = codes[index].parse().unwrap_or(0);
        match code {
            30..=37 | 39 | 40..=47 | 49 | 90..=97 | 100..=107 => {}
            // Extended colours carry their arguments with them.
            38 | 48 => {
                index += match codes.get(index + 1).copied() {
                    Some("5") => 2,
                    Some("2") => 4,
                    _ => 0,
                };
            }
            _ => kept.push(codes[index]),
        }
        index += 1;
    }
    kept.join(";")
}

/// Writes `text` to standard output with the styling it may carry there.
pub fn emit(text: &str) {
    print!("{}", apply(text, stdout_level()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_needs_a_terminal_and_no_objection() {
        let some = |value: &str| Some(std::ffi::OsString::from(value));
        assert_eq!(decide(true, None, some("xterm")), Level::Colour);
        assert_eq!(decide(false, None, some("xterm")), Level::Plain);
        assert_eq!(decide(true, None, some("dumb")), Level::Plain);
        assert_eq!(decide(true, some("1"), some("xterm")), Level::Emphasis);
        // An empty NO_COLOR is unset, as the convention says.
        assert_eq!(decide(true, some(""), None), Level::Colour);
    }

    #[test]
    fn styling_is_reduced_and_the_words_are_kept() {
        let text = "\x1b[1m/w\x1b[0m \x1b[2mg\x1b[0m\n  status: \x1b[33m1 conflicts\x1b[0m, \
                    \x1b[1;31mhalted\x1b[0m \x1b[38;5;208mo\x1b[0m \x1b[38;2;1;2;3mt\x1b[0m";
        assert_eq!(
            apply(text, Level::Plain),
            "/w g\n  status: 1 conflicts, halted o t"
        );
        assert_eq!(
            apply(text, Level::Emphasis),
            "\x1b[1m/w\x1b[0m \x1b[2mg\x1b[0m\n  status: 1 conflicts\x1b[0m, \
             \x1b[1mhalted\x1b[0m o\x1b[0m t\x1b[0m"
        );
        assert_eq!(apply(text, Level::Colour), text);
        // A sequence that is not a style passes as it is.
        assert_eq!(apply("a\x1b[2Kb", Level::Plain), "a\x1b[2Kb");
    }
}
