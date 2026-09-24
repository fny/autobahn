//! Text that came from elsewhere, made safe for a shell or a terminal.
//!
//! File names, remote error text and relayed agent output are chosen by
//! the other side, and a POSIX name may hold any byte but NUL and `/`.
//! Before such text becomes part of a command, it goes through
//! [`shell_quote`]; before it reaches a terminal or a log line, it goes
//! through [`display_safe`], and [`cap_line`] bounds how long it can be.

use std::borrow::Cow;
use std::fmt::Write as _;

/// Quote `s` as one POSIX shell word: wrap it in single quotes, inside
/// which nothing is special, and write each `'` as `'\''` (close the
/// quote, an escaped quote, reopen it). The result is always quoted, even
/// when `s` needs none, so `""` becomes `''` and is still one word.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Quote each word with [`shell_quote`] and join them with spaces, so the
/// shell splits the result back into exactly `words`.
pub fn shell_join(words: &[&str]) -> String {
    let mut out = String::new();
    for (i, word) in words.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&shell_quote(word));
    }
    out
}

/// Replace every control character in `s` (C0, DEL and C1, which covers
/// ESC and BEL) with a visible escape, so text from elsewhere cannot move
/// the cursor, set the clipboard, or split one log line into several.
/// `\n`, `\r` and `\t` read as themselves; other C0 characters and DEL
/// become `\xNN`, and C1 characters `\u{NN}`. Borrows when nothing needs
/// replacing, which is nearly always.
pub fn display_safe(s: &str) -> Cow<'_, str> {
    let Some(first) = s.find(char::is_control) else {
        return Cow::Borrowed(s);
    };
    let mut out = String::with_capacity(s.len() + 8);
    out.push_str(&s[..first]);
    for c in s[first..].chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x80 && c.is_control() => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Like [`display_safe`], but for a message meant to be read as a block:
/// a parse error with a caret line under it, say. Newlines survive so the
/// shape is kept; every other control character is escaped as before, and
/// each line after the first is indented, so text from elsewhere cannot
/// forge a line that looks like autobahn's own.
pub fn display_block(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for (index, line) in s.split('\n').enumerate() {
        if index > 0 {
            out.push_str("\n  ");
        }
        out.push_str(&display_safe(line));
    }
    out
}

/// The marker [`cap_line`] puts where it cut.
const ELLIPSIS: &str = "…";

/// Bound `s` to at most `max_bytes` bytes. A longer line is cut on a
/// character boundary and ends in `…`, which counts toward the bound;
/// when `max_bytes` is too small for even the marker, the line is cut
/// without one. Borrows when `s` already fits.
pub fn cap_line(s: &str, max_bytes: usize) -> Cow<'_, str> {
    if s.len() <= max_bytes {
        return Cow::Borrowed(s);
    }
    let (budget, marker) = match max_bytes.checked_sub(ELLIPSIS.len()) {
        Some(budget) => (budget, ELLIPSIS),
        None => (max_bytes, ""),
    };
    let mut cut = budget;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    Cow::Owned(format!("{}{marker}", &s[..cut]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// What `sh` prints for `printf %s <word>`: the value the shell sees.
    fn through_sh(word: &str) -> String {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {word}"))
            .output()
            .expect("sh runs");
        assert!(out.status.success(), "sh rejected {word:?}");
        String::from_utf8(out.stdout).expect("utf-8 back")
    }

    const AWKWARD: &[&str] = &[
        "",
        "plain",
        "two words",
        "it's",
        "''",
        "'\\''",
        "$(touch pwned)",
        "`touch pwned`",
        "a;b && c | d > e",
        "line\nbreak\n",
        "tab\there",
        "$HOME ${HOME} \"dq\" \\ back",
        "*?[a]~#!%",
        "-n",
        "ünï\u{1b}[31mcode",
    ];

    /// A block keeps its shape and loses its weapons: the newlines that
    /// make a parse error readable survive, the escape that would move a
    /// cursor does not, and a continuation line cannot pass for a first
    /// one.
    #[test]
    fn display_block_keeps_newlines_and_escapes_the_rest() {
        let shown = display_block("unable to parse\n  |\n1 | mdoe = \"x\"\n  | ^^^^");
        assert!(shown.contains('\n'), "{shown:?}");
        assert!(shown.contains("\n    |"), "continuation lines are indented: {shown:?}");
        assert!(!shown.contains("\\n"), "a newline is not escaped away: {shown:?}");

        let hostile = display_block("first\n\x1b]52;c;cHduZWQ=\x07second");
        assert!(!hostile.contains('\x1b'), "{hostile:?}");
        assert!(hostile.contains("\\x1b"), "{hostile:?}");
    }

    #[test]
    fn a_quoted_word_reaches_the_shell_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        for s in AWKWARD {
            let out = Command::new("sh")
                .current_dir(dir.path())
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote(s)))
                .output()
                .unwrap();
            assert!(out.status.success(), "sh rejected {s:?}");
            assert_eq!(String::from_utf8(out.stdout).unwrap(), *s);
        }
        assert!(!dir.path().join("pwned").exists());
    }

    #[test]
    fn a_joined_command_keeps_each_word_whole() {
        let joined = shell_join(AWKWARD);
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\0' {joined}"))
            .output()
            .unwrap();
        let words: Vec<&str> = std::str::from_utf8(&out.stdout)
            .unwrap()
            .split_terminator('\0')
            .collect();
        assert_eq!(words, AWKWARD);
        assert_eq!(shell_join(&[]), "");
    }

    #[test]
    fn a_quoted_command_survives_a_second_shell() {
        // The ssh shape: a command quoted once more to be one argument.
        let inner = format!("printf %s {}", shell_quote("it's $(x) `y`"));
        assert_eq!(
            through_sh(&format!("\"$(sh -c {})\"", shell_quote(&inner))),
            "it's $(x) `y`"
        );
    }

    proptest::proptest! {
        #[test]
        fn quoting_then_evaluating_returns_the_input(s in "[^\u{0}]{0,40}") {
            proptest::prop_assert_eq!(through_sh(&shell_quote(&s)), s);
        }

        #[test]
        fn display_safe_lets_no_control_character_through(s in proptest::string::string_regex("(.|[\u{0}-\u{1f}\u{7f}-\u{9f}]){0,40}").unwrap()) {
            let shown = display_safe(&s);
            proptest::prop_assert!(shown.bytes().all(|b| b >= 0x20 && b != 0x7f));
            let c1 = shown.chars().any(|c| ('\u{80}'..='\u{9f}').contains(&c));
            proptest::prop_assert!(!c1);
            if !s.chars().any(char::is_control) {
                proptest::prop_assert!(matches!(shown, Cow::Borrowed(_)));
            }
        }

        #[test]
        fn a_capped_line_is_a_prefix_within_the_cap(s in "\\PC{0,40}", max in 0usize..50) {
            let capped = cap_line(&s, max);
            proptest::prop_assert!(capped.len() <= max);
            if s.len() <= max {
                proptest::prop_assert!(matches!(capped, Cow::Borrowed(_)));
            } else {
                let body = capped.strip_suffix('…').unwrap_or(&capped);
                proptest::prop_assert!(s.starts_with(body));
            }
        }
    }

    #[test]
    fn control_characters_become_visible_escapes() {
        assert_eq!(display_safe("a\nb"), "a\\nb");
        assert_eq!(display_safe("\r\t"), "\\r\\t");
        assert_eq!(
            display_safe("\u{1b}]52;c;aGk=\u{7}"),
            "\\x1b]52;c;aGk=\\x07"
        );
        assert_eq!(display_safe("del\u{7f}"), "del\\x7f");
        assert_eq!(display_safe("csi\u{9b}2J"), "csi\\u{9b}2J");
        assert_eq!(display_safe("\u{0}"), "\\x00");
    }

    #[test]
    fn ordinary_text_is_borrowed() {
        for s in ["", "plain", "ünïcødé 名前", "back\\slash", "it's $(x)"] {
            assert!(matches!(display_safe(s), Cow::Borrowed(t) if t == s));
        }
    }

    #[test]
    fn a_long_line_is_cut_on_a_character_boundary() {
        assert_eq!(cap_line("short", 5), "short");
        assert!(matches!(cap_line("short", 5), Cow::Borrowed(_)));
        assert_eq!(cap_line("abcdefgh", 6), "abc…");
        // "é" is two bytes; a cut inside it backs off to before it.
        assert_eq!(cap_line("aébcdef", 5), "a…");
        assert_eq!(cap_line("abcdef", 2), "ab");
        assert_eq!(cap_line("éé", 1), "");
    }
}
