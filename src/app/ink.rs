//! Colour for the supervisor's log, inside the kit's own editor.
//!
//! The editor colours code through a highlighter, and the kit ships one
//! per bundled language — none of which is a log. The seam it hands the
//! parser is small enough to answer directly: a line of our log has a
//! fixed shape, so the runs can be read off it without a grammar.
//!
//!     2026-09-24 09:55:57 debug: [dev@halle.steinbach.de] blocked: …
//!     └── when ──────────┘ └ kind ┘ └── who ────────────┘ └ what ┘
//!
//! Colouring here rather than drawing our own rows keeps everything the
//! editor already gives the log: selection, ⌃F search, line numbers.

use std::ops::Range;

use gpui_kit::component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter, Rope,
};
use gpui_kit::*;

use super::{AMBER, BLUE, DIM, FAINT, GREEN, INK, RED};

/// A line's own colour, chosen from the word that opens its message.
fn ink_for(word: &str) -> u32 {
    match word {
        "error" | "halted" | "unreachable" | "errored" | "failed" => RED,
        "blocked" | "conflicts" | "refused" | "warning" | "paused" => AMBER,
        "synchronized" | "started" | "connected" | "cleared" => GREEN,
        _ => DIM,
    }
}

fn paint(colour: u32) -> HighlightStyle {
    HighlightStyle {
        color: Some(rgb(colour).into()),
        ..Default::default()
    }
}

/// The runs one line contributes, at absolute offsets from `at`.
fn runs_for(line: &str, at: usize, out: &mut Vec<(Range<usize>, HighlightStyle)>) {
    let mut cut = 0usize;
    let push = |out: &mut Vec<(Range<usize>, HighlightStyle)>, span: Range<usize>, style| {
        if span.start < span.end {
            out.push((at + span.start..at + span.end, style));
        }
    };

    // The stamp: always the same nineteen characters, when it is there.
    if line.len() >= 19 && line.as_bytes()[0].is_ascii_digit() && line.as_bytes()[4] == b'-' {
        push(out, 0..19, paint(FAINT));
        cut = 19;
    }

    // `debug:` marks the whole line as chatter, and dims what follows.
    let chatter = line[cut..].starts_with(" debug:");
    if chatter {
        push(out, cut..cut + 7, paint(FAINT));
        cut += 7;
    }

    // The session, in brackets: one colour, so the eye can group by it.
    if let Some(open) = line[cut..].find('[') {
        if let Some(close) = line[cut + open..].find(']') {
            push(
                out,
                cut + open..cut + open + close + 1,
                paint(if chatter { FAINT } else { BLUE }),
            );
            cut += open + close + 1;
        }
    }

    // The message. Its first word, when it ends in a colon, is the news.
    let rest = &line[cut..];
    let start = rest.len() - rest.trim_start().len();
    let word = rest[start..].split([':', ' ']).next().unwrap_or("");
    let labelled = !word.is_empty() && rest[start + word.len()..].starts_with(':');
    if labelled {
        push(
            out,
            cut + start..cut + start + word.len() + 1,
            paint(ink_for(word)),
        );
        cut += start + word.len() + 1;
    }
    // Everything left is prose: bright, unless the line was chatter.
    push(
        out,
        cut..line.len(),
        paint(if chatter { FAINT } else { INK }),
    );
}

/// The log's highlighter: no grammar, no incremental state worth keeping.
pub(super) struct LogInk {
    runs: Vec<(Range<usize>, HighlightStyle)>,
}

impl LogInk {
    pub(super) fn new() -> Self {
        LogInk { runs: Vec::new() }
    }

    /// The runs for a whole document, which is how this one is read.
    fn read(text: &str) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut runs = Vec::new();
        let mut at = 0usize;
        for line in text.split_inclusive('\n') {
            let bare = line.trim_end_matches(['\n', '\r']);
            runs_for(bare, at, &mut runs);
            at += line.len();
        }
        runs
    }
}

impl InputHighlighter for LogInk {
    fn language(&self) -> SharedString {
        "log".into()
    }

    fn update(
        &mut self,
        _edit: Option<InputEdit>,
        text: &Rope,
        _folding: bool,
        _window: &mut Window,
        _cx: &mut Context<EditorState>,
    ) {
        self.runs = LogInk::read(&text.to_string());
    }

    fn styles(
        &self,
        range: &Range<usize>,
        _resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        // The contract is full cover: every byte of `range` named once,
        // in order. Ours name only the coloured parts, so the gaps are
        // filled here rather than stored.
        let mut out = Vec::new();
        let mut at = range.start;
        for (span, style) in &self.runs {
            if span.end <= at {
                continue;
            }
            if span.start >= range.end {
                break;
            }
            let start = span.start.max(at);
            let end = span.end.min(range.end);
            if at < start {
                out.push((at..start, HighlightStyle::default()));
            }
            if start < end {
                out.push((start..end, *style));
            }
            at = end;
        }
        if at < range.end {
            out.push((at..range.end, HighlightStyle::default()));
        }
        out
    }

    fn fold_ranges(&self, _text: &Rope) -> Vec<FoldRange> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    // No glob from the parent here: `use gpui_kit::*` brings the kit's own
    // `test` macro with it, and the attribute below would then expand into
    // itself for ever.
    use super::{LogInk, AMBER, BLUE, FAINT, INK, RED};
    use gpui_kit::component::input::{HighlightStyleResolver, InputHighlighter};
    use gpui_kit::{HighlightStyle, Rgba};

    fn colour_of(line: &str, needle: &str) -> Option<u32> {
        let runs = LogInk::read(line);
        let at = line.find(needle).expect("needle is in the line");
        runs.iter()
            .find(|(span, _)| span.contains(&at))
            .and_then(|(_, style)| style.color)
            .map(|colour| {
                let rgba = Rgba::from(colour);
                ((rgba.r * 255.).round() as u32) << 16
                    | ((rgba.g * 255.).round() as u32) << 8
                    | (rgba.b * 255.).round() as u32
            })
    }

    const LINE: &str = "2026-09-24 09:55:57 [dev@halle.steinbach.de] blocked: replica path: why";
    const CHATTER: &str =
        "2026-09-24 09:55:57 debug: [dev@halle.steinbach.de] cycle finished in 0.10s";

    #[test]
    fn the_news_of_a_line_takes_the_colour_of_its_state() {
        assert_eq!(colour_of(LINE, "blocked:"), Some(AMBER));
        assert_eq!(
            colour_of("2026-09-24 09:55:57 [a] error: no", "error:"),
            Some(RED)
        );
        assert_eq!(colour_of(LINE, "2026-09-24"), Some(FAINT));
        assert_eq!(colour_of(LINE, "[dev@halle"), Some(BLUE));
        assert_eq!(colour_of(LINE, "replica path"), Some(INK));
    }

    #[test]
    fn chatter_is_dim_from_end_to_end() {
        for needle in ["2026-09-24", "debug:", "[dev@halle", "cycle"] {
            assert_eq!(colour_of(CHATTER, needle), Some(FAINT), "{needle}");
        }
    }

    #[test]
    fn a_line_with_no_shape_at_all_is_still_covered() {
        let runs = LogInk::read("something happened\n");
        assert!(!runs.is_empty());
        // And the offsets of the second line start after the first.
        let two = LogInk::read("one\ntwo:\n");
        let second = two
            .iter()
            .find(|(span, _)| span.start >= 4)
            .expect("the second line has a run");
        assert!(second.0.start >= 4);
    }

    #[test]
    fn what_is_asked_for_comes_back_whole_and_in_order() {
        struct Nothing;
        impl HighlightStyleResolver for Nothing {
            fn style(&self, _: &str) -> Option<HighlightStyle> {
                None
            }
        }
        let mut ink = LogInk::new();
        ink.runs = LogInk::read(LINE);
        let asked = 5..40;
        let back = ink.styles(&asked, &Nothing);
        assert_eq!(back.first().unwrap().0.start, asked.start);
        assert_eq!(back.last().unwrap().0.end, asked.end);
        for pair in back.windows(2) {
            assert_eq!(pair[0].0.end, pair[1].0.start);
        }
    }
}
