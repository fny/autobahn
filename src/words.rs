//! What the surfaces say, in one place.
//!
//! Every line a person reads in the window or the menu bar comes from
//! `assets/words/en.toml`, looked up by a key. Nothing here translates
//! anything yet — English is the only catalogue, and it is compiled in,
//! so the app has no file to lose. But the strings are no longer spread
//! through the layout, which is the part that makes a second language
//! possible rather than a rewrite.
//!
//! A key that is not in the catalogue reads as itself. A missing line is
//! a visible mistake, not a panic and not an empty label.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The catalogue as shipped.
const ENGLISH: &str = include_str!("../assets/words/en.toml");

fn catalogue() -> &'static HashMap<String, String> {
    static WORDS: OnceLock<HashMap<String, String>> = OnceLock::new();
    WORDS.get_or_init(|| {
        let mut words = HashMap::new();
        let Ok(document) = ENGLISH.parse::<toml::Table>() else {
            return words;
        };
        // Two levels: a table per surface, a key per line. Flat enough
        // to read as a file, grouped enough to find anything in it.
        for (surface, table) in document {
            let Some(table) = table.as_table() else { continue };
            for (key, value) in table {
                if let Some(text) = value.as_str() {
                    words.insert(format!("{surface}.{key}"), text.to_owned());
                }
            }
        }
        words
    })
}

/// The line for a key.
pub(crate) fn t(key: &str) -> &'static str {
    match catalogue().get(key) {
        Some(text) => text.as_str(),
        // Leaked deliberately: a missing key is a mistake to see in the
        // window, and there are only ever a handful of them.
        None => Box::leak(key.to_owned().into_boxed_str()),
    }
}

/// The line for a key, with `{name}` replaced by what is given.
pub(crate) fn fill(key: &str, values: &[(&str, &str)]) -> String {
    let mut text = t(key).to_owned();
    for (name, value) in values {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key the surfaces ask for is a key the catalogue holds.
    ///
    /// The lookup is by string, so nothing but this would notice a key
    /// renamed in one place and not the other — and what a person would
    /// see is the key itself, in the middle of the window.
    #[test]
    fn every_key_the_window_asks_for_is_in_the_catalogue() {
        let sources = [
            include_str!("desk/mod.rs"),
            include_str!("desk/area.rs"),
            include_str!("menubar.rs"),
        ];
        let mut asked = Vec::new();
        for source in sources {
            let mut rest = source;
            while let Some(at) = rest.find("t(\"") {
                rest = &rest[at + 3..];
                // Only a call: `t("…")`, never `format!("…")`.
                let before = source.len() - rest.len() - 3;
                let previous = source[..before].chars().last().unwrap_or(' ');
                if previous.is_alphanumeric() || previous == '_' {
                    continue;
                }
                if let Some(end) = rest.find('"') {
                    asked.push(rest[..end].to_owned());
                }
            }
        }
        assert!(asked.len() > 50, "the surfaces ask for {} keys", asked.len());
        let missing: Vec<&String> = asked
            .iter()
            .filter(|key| !catalogue().contains_key(*key))
            .collect();
        assert!(missing.is_empty(), "not in assets/words/en.toml: {missing:?}");
    }

    /// A key nobody asks for is a line nobody reads.
    #[test]
    fn the_catalogue_says_only_what_is_asked_for() {
        let sources = concat!(
            include_str!("desk/mod.rs"),
            include_str!("desk/area.rs"),
            include_str!("menubar.rs"),
        );
        let unused: Vec<&String> = catalogue()
            .keys()
            .filter(|key| !sources.contains(&format!("\"{key}\"")))
            .collect();
        assert!(unused.is_empty(), "nothing asks for: {unused:?}");
    }

    /// What is put into a line lands where the line says it should.
    #[test]
    fn a_line_takes_what_is_put_into_it() {
        assert_eq!(
            fill("status.copied", &[("text", "one line")]),
            "copied: one line"
        );
        assert_eq!(t("not.a.key"), "not.a.key");
    }
}
