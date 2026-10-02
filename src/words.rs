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
            // Every key would read as itself and the whole window would
            // fill with `fleet.cycles_one`. It is a mistake somebody
            // makes editing the file by hand, and it should be a failing
            // test rather than a puzzle in front of them.
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

/// One of the hints, chosen at random.
///
/// The bottom of the window is a line that is empty most of the time,
/// and a line that is empty most of the time is a place to teach
/// somebody something they would not otherwise find — the TUI, the
/// JSON, the door on the wordmark. Every hint is in the catalogue
/// under `[hints]`, so adding one is adding a line to a file.
///
/// Random rather than in order: a person who opens this window twice
/// a day should not read the same sentence every time.
pub(crate) fn hint() -> &'static str {
    hint_besides("")
}

/// The same, but never the one already showing.
///
/// A hint that is meant to change and does not looks like a window
/// that failed to notice the click.
pub(crate) fn hint_besides(showing: &str) -> &'static str {
    static EVERY: OnceLock<Vec<&'static str>> = OnceLock::new();
    let every = EVERY.get_or_init(|| {
        let mut said: Vec<&'static str> = catalogue()
            .iter()
            .filter(|(key, text)| key.starts_with("hints.") && !text.trim().is_empty())
            .map(|(_, text)| text.as_str())
            .collect();
        // The map's order is the hash's; sorted, the choice below is the
        // only thing deciding which one anybody sees.
        said.sort();
        said
    });
    let fresh: Vec<&&'static str> = every
        .iter()
        .filter(|said| **said != showing)
        .collect();
    let choosing: &[&&'static str] = match fresh.is_empty() {
        true => return every.first().copied().unwrap_or(""),
        false => &fresh,
    };
    // No dependency for this: the clock is random enough to pick one of
    // half a dozen sentences, and nothing here needs to be unguessable.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.subsec_nanos() as usize)
        .unwrap_or(0);
    *choosing[now % choosing.len()]
}

/// The line for a key, with `{name}` replaced by what is given.
pub(crate) fn fill(key: &str, values: &[(&str, &str)]) -> String {
    let mut text = t(key).to_owned();
    for (name, value) in values {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

/// The line for a key, in the form that suits a number.
///
/// English has two: one thing and several. A key is written as a pair —
/// `cycles_one` and `cycles_many` — and this picks between them, so a
/// session never reports "1 cycles". `{count}` is filled in for both,
/// since the singular usually spells the number out and sometimes does
/// not. A language with more forms than two would need more of them
/// here, which is the point of the pair being in the file rather than
/// an `s` glued on in the layout.
pub(crate) fn count(key: &str, n: usize, values: &[(&str, &str)]) -> String {
    let key = match n {
        1 => format!("{key}_one"),
        _ => format!("{key}_many"),
    };
    // What the caller gives wins: a count is often written for a
    // person — "6,764" — rather than as the number itself.
    let number = n.to_string();
    let mut all: Vec<(&str, &str)> = values.to_vec();
    if !all.iter().any(|(name, _)| *name == "count") {
        all.push(("count", &number));
    }
    fill(&key, &all)
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
        // Every surface that speaks: the two windows, what they share,
        // and the menu bar.
        let sources = [
            include_str!("dash/mod.rs"),
            include_str!("surface.rs"),
            include_str!("menubar.rs"),
        ];
        let mut asked = Vec::new();
        for source in sources {
            for (opening, plural) in [("t(\"", false), ("fill(\"", false), ("count(\"", true)] {
                let mut rest = source;
                while let Some(at) = rest.find(opening) {
                    rest = &rest[at + opening.len()..];
                    // A call, not the tail of a longer name.
                    let before = source.len() - rest.len() - opening.len();
                    let previous = source[..before].chars().last().unwrap_or(' ');
                    if previous.is_alphanumeric() || previous == '_' || previous == ':' {
                        continue;
                    }
                    let Some(end) = rest.find('"') else { continue };
                    let key = &rest[..end];
                    match plural {
                        // A counted line is a pair, and both halves must
                        // be there or one number in ten reads wrongly.
                        true => {
                            asked.push(format!("{key}_one"));
                            asked.push(format!("{key}_many"));
                        }
                        false => asked.push(key.to_owned()),
                    }
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
            include_str!("dash/mod.rs"),
            include_str!("surface.rs"),
            include_str!("menubar.rs"),
        );
        let unused: Vec<&String> = catalogue()
            .keys()
            // `hint()` reads every key under `[hints]` at once, so no
            // source names one of them and none of them is unused.
            .filter(|key| !key.starts_with("hints."))
            .filter(|key| {
                let counted = key
                    .strip_suffix("_one")
                    .or_else(|| key.strip_suffix("_many"));
                !sources.contains(&format!("\"{key}\""))
                    && !counted.is_some_and(|stem| sources.contains(&format!("\"{stem}\"")))
            })
            .collect();
        assert!(unused.is_empty(), "nothing asks for: {unused:?}");
    }

    /// The catalogue parses. Without this the loader returns an empty
    /// map, every key reads as itself, and the window fills with
    /// `fleet.cycles_one` — which is a stray quote somewhere in a file
    /// anybody may edit, and should not take an afternoon to find.
    #[test]
    fn the_catalogue_is_a_file_that_parses() {
        let document = ENGLISH
            .parse::<toml::Table>()
            .expect("assets/words/en.toml does not parse");
        assert!(!document.is_empty());
        assert!(!catalogue().is_empty(), "the catalogue loaded nothing");
        assert_eq!(t("app.name"), "autobahn", "a known key reads as itself");
    }

    /// Every hint is offered, and an empty one is not a hint.
    #[test]
    fn a_hint_is_one_of_the_ones_written_down() {
        let every: Vec<&str> = catalogue()
            .iter()
            .filter(|(key, text)| key.starts_with("hints.") && !text.trim().is_empty())
            .map(|(_, text)| text.as_str())
            .collect();
        assert!(!every.is_empty(), "there are no hints to show");
        for _ in 0..50 {
            let said = hint();
            assert!(every.contains(&said), "{said:?} is not one of the hints");
        }
        // Asked for one besides the one showing, it never gives that
        // one back: a hint that is meant to change and does not looks
        // like a window that did not notice the click.
        for showing in &every {
            for _ in 0..50 {
                let next = hint_besides(showing);
                assert_ne!(&next, showing);
                assert!(every.contains(&next), "{next:?} is not one of the hints");
            }
        }
    }

    /// What is put into a line lands where the line says it should.
    #[test]
    fn a_line_takes_what_is_put_into_it() {
        assert_eq!(
            fill("status.ran", &[("command", "clean --yes")]),
            "clean --yes finished"
        );
        assert_eq!(t("not.a.key"), "not.a.key");
    }

    /// One of a thing reads as one of a thing.
    #[test]
    fn a_count_of_one_is_not_a_plural() {
        assert_eq!(count("fleet.cycles", 1, &[]), "1 cycle");
        assert_eq!(count("fleet.cycles", 2, &[]), "2 cycles");
        assert_eq!(count("hosts.session", 1, &[]), "1 session");
        assert_eq!(count("hosts.session", 9, &[]), "9 sessions");
        assert_eq!(count("pane.group", 1, &[]), "1 group");
        assert_eq!(
            count("fleet.cycles", 6764, &[("count", "6,764")]),
            "6,764 cycles",
            "a count written for a person is the one that is used"
        );
        // Every pair in the catalogue has both halves.
        for key in catalogue().keys() {
            if let Some(stem) = key.strip_suffix("_one") {
                assert!(
                    catalogue().contains_key(&format!("{stem}_many")),
                    "{stem} has no plural"
                );
            }
            if let Some(stem) = key.strip_suffix("_many") {
                assert!(
                    catalogue().contains_key(&format!("{stem}_one")),
                    "{stem} has no singular"
                );
            }
        }
    }
}
