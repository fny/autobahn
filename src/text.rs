//! Text that came from elsewhere, made safe for a shell or a terminal.
//!
//! File names, remote error text and relayed agent output are chosen by
//! the other side, and a POSIX name may hold any byte but NUL and `/`.
//! Before such text becomes part of a command, it goes through
//! [`shell_quote`]; before it reaches a terminal or a log line, it goes
//! through [`display_safe`], and [`cap_line`] bounds how long it can be.

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
    }
}
