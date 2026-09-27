//! The text being edited, and everything that can be done to it.
//!
//! No window, no framework: a string, a caret, a selection and the
//! moves a person expects of a text field on this machine. Keeping it
//! apart from the view is what lets the awkward parts — where a word
//! ends, what option-backspace takes out, what undo puts back — be
//! tested without opening a window, and it is how the editor in GPUI
//! Kit is arranged too.
//!
//! Offsets are byte indices into the text and always fall on character
//! boundaries.

use std::ops::Range;

/// How far a move or a deletion reaches.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum By {
    /// One character.
    Character,
    /// To the start or end of the word.
    Word,
    /// To the start or end of the line.
    Line,
    /// To the start or end of everything.
    All,
}

/// What an edit was, so that a run of typing undoes as one thing rather
/// than one character at a time.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Typing,
    Deleting,
    Other,
}

/// The text, the caret, and what it takes to put a change back.
#[derive(Clone)]
struct Moment {
    text: String,
    head: usize,
    tail: usize,
}

/// A block of text being edited.
pub struct Buffer {
    text: String,
    /// The caret.
    head: usize,
    /// The other end of the selection; equal to `head` when there is
    /// none.
    tail: usize,
    /// The column a run of up and down keeps aiming for, so passing a
    /// short line does not lose the place.
    column: Option<usize>,
    done: Vec<Moment>,
    undone: Vec<Moment>,
    last: Kind,
}

impl Buffer {
    pub fn new(text: String) -> Buffer {
        let end = text.len();
        Buffer {
            text,
            head: end,
            tail: end,
            column: None,
            done: Vec::new(),
            undone: Vec::new(),
            last: Kind::Other,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn head(&self) -> usize {
        self.head
    }

    /// The selected range, in order.
    pub fn selection(&self) -> Range<usize> {
        match self.head <= self.tail {
            true => self.head..self.tail,
            false => self.tail..self.head,
        }
    }

    pub fn selected(&self) -> &str {
        &self.text[self.selection()]
    }

    /// How many lines the text has.
    pub fn lines(&self) -> usize {
        self.text.split('\n').count()
    }

    /// Where each line starts.
    pub fn starts(&self) -> Vec<usize> {
        let mut starts = vec![0];
        for (at, byte) in self.text.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(at + 1);
            }
        }
        starts
    }

    /// Which line an offset is on.
    pub fn line_of(&self, offset: usize) -> usize {
        self.starts()
            .iter()
            .rposition(|start| *start <= offset)
            .unwrap_or(0)
    }

    /// The line's range, without its newline.
    pub fn line_at(&self, offset: usize) -> Range<usize> {
        let line = self.line_of(offset);
        let starts = self.starts();
        let start = starts[line];
        let end = match starts.get(line + 1) {
            Some(next) => next - 1,
            None => self.text.len(),
        };
        start..end
    }

    /// The word around an offset: letters, digits and underscores, as a
    /// double click selects elsewhere on this machine.
    pub fn word_at(&self, offset: usize) -> Range<usize> {
        let offset = self.clamp(offset);
        if !self.wordish(offset) && offset > 0 && self.wordish(self.back(offset)) {
            // Just past the end of a word is still that word.
            return self.word_at(self.back(offset));
        }
        let mut start = offset;
        while start > 0 && self.wordish(self.back(start)) {
            start = self.back(start);
        }
        let mut end = offset;
        while end < self.text.len() && self.wordish(end) {
            end = self.forward(end);
        }
        start..end
    }

    fn wordish(&self, at: usize) -> bool {
        self.text[at..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    }

    fn clamp(&self, at: usize) -> usize {
        let mut at = at.min(self.text.len());
        while !self.text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    fn back(&self, from: usize) -> usize {
        let mut at = from.saturating_sub(1);
        while at > 0 && !self.text.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    fn forward(&self, from: usize) -> usize {
        let mut at = (from + 1).min(self.text.len());
        while !self.text.is_char_boundary(at) {
            at += 1;
        }
        at
    }

    /// Where a move of this size ends up, going back or forward.
    pub fn reach(&self, by: By, forward: bool) -> usize {
        match (by, forward) {
            (By::Character, true) => self.forward(self.head),
            (By::Character, false) => self.back(self.head),
            (By::Word, true) => {
                let mut at = self.head;
                while at < self.text.len() && !self.wordish(at) {
                    at = self.forward(at);
                }
                while at < self.text.len() && self.wordish(at) {
                    at = self.forward(at);
                }
                at
            }
            (By::Word, false) => {
                let mut at = self.head;
                while at > 0 && !self.wordish(self.back(at)) {
                    at = self.back(at);
                }
                while at > 0 && self.wordish(self.back(at)) {
                    at = self.back(at);
                }
                at
            }
            (By::Line, true) => self.line_at(self.head).end,
            (By::Line, false) => self.line_at(self.head).start,
            (By::All, true) => self.text.len(),
            (By::All, false) => 0,
        }
    }

    /// Moves the caret, taking the text in between when `select`.
    pub fn move_to(&mut self, offset: usize, select: bool) {
        self.head = self.clamp(offset);
        if !select {
            self.tail = self.head;
        }
        self.column = None;
        self.last = Kind::Other;
    }

    /// Moves by a whole word, line or character.
    pub fn step(&mut self, by: By, forward: bool, select: bool) {
        // An unselecting sideways move from a selection lands on its
        // edge rather than a character further on, as it does anywhere
        // else.
        if !select && by == By::Character && !self.selection().is_empty() {
            let range = self.selection();
            let to = match forward {
                true => range.end,
                false => range.start,
            };
            self.move_to(to, false);
            return;
        }
        let to = self.reach(by, forward);
        self.move_to(to, select);
    }

    /// Up or down a line, keeping the column a run of them started in.
    pub fn vertical(&mut self, down: bool, select: bool) {
        let starts = self.starts();
        let line = self.line_of(self.head);
        let column = self.column.unwrap_or(self.head - starts[line]);
        let wanted = match (down, line) {
            (true, line) => line + 1,
            (false, 0) => {
                self.move_to(0, select);
                self.column = Some(column);
                return;
            }
            (false, line) => line - 1,
        };
        let Some(start) = starts.get(wanted).copied() else {
            let end = self.text.len();
            self.move_to(end, select);
            self.column = Some(column);
            return;
        };
        let end = self.line_at(start).end;
        let to = self.clamp((start + column).min(end));
        self.move_to(to, select);
        self.column = Some(column);
    }

    /// Selects everything.
    pub fn select_all(&mut self) {
        self.tail = 0;
        self.head = self.text.len();
        self.last = Kind::Other;
    }

    /// Selects the word around an offset, as a double click does.
    pub fn select_word(&mut self, offset: usize) {
        let word = self.word_at(offset);
        self.tail = word.start;
        self.head = word.end;
        self.last = Kind::Other;
    }

    /// Selects the line around an offset, as a triple click does.
    pub fn select_line(&mut self, offset: usize) {
        let line = self.line_at(offset);
        self.tail = line.start;
        self.head = line.end;
        self.last = Kind::Other;
    }

    /// Puts `what` in place of `range`.
    pub fn replace(&mut self, range: Range<usize>, what: &str, kind: Kind2) {
        let kind = match kind {
            Kind2::Typing => Kind::Typing,
            Kind2::Deleting => Kind::Deleting,
            Kind2::Other => Kind::Other,
        };
        // A run of typing, or a run of deleting, undoes as one thing.
        if kind != self.last || kind == Kind::Other {
            self.remember();
        }
        self.last = kind;
        self.undone.clear();
        let range = self.clamp(range.start)..self.clamp(range.end);
        self.text.replace_range(range.clone(), what);
        self.head = range.start + what.len();
        self.tail = self.head;
        self.column = None;
    }

    /// Types text in, over the selection if there is one.
    pub fn insert(&mut self, what: &str) {
        self.replace(self.selection(), what, Kind2::Typing);
    }

    /// Takes out the selection, or as far as `by` reaches when there is
    /// none.
    pub fn delete(&mut self, by: By, forward: bool) {
        let range = match self.selection().is_empty() {
            false => self.selection(),
            true => {
                let to = self.reach(by, forward);
                match forward {
                    true => self.head..to,
                    false => to..self.head,
                }
            }
        };
        if range.is_empty() {
            return;
        }
        self.replace(range, "", Kind2::Deleting);
    }

    /// Control-K: what is left of the line, or the line break itself
    /// when the caret is already at the end.
    pub fn kill_line(&mut self) {
        let line = self.line_at(self.head);
        let to = match line.end == self.head {
            true => self.forward(self.head),
            false => line.end,
        };
        if to == self.head {
            return;
        }
        self.replace(self.head..to, "", Kind2::Other);
    }

    fn remember(&mut self) {
        self.done.push(Moment {
            text: self.text.clone(),
            head: self.head,
            tail: self.tail,
        });
        // A personal tool does not need a week of history.
        if self.done.len() > 200 {
            self.done.remove(0);
        }
    }

    /// Puts back what the last edit changed.
    pub fn undo(&mut self) -> bool {
        let Some(moment) = self.done.pop() else {
            return false;
        };
        self.undone.push(Moment {
            text: self.text.clone(),
            head: self.head,
            tail: self.tail,
        });
        self.text = moment.text;
        self.head = moment.head;
        self.tail = moment.tail;
        self.last = Kind::Other;
        true
    }

    /// Takes back an undo.
    pub fn redo(&mut self) -> bool {
        let Some(moment) = self.undone.pop() else {
            return false;
        };
        self.done.push(Moment {
            text: self.text.clone(),
            head: self.head,
            tail: self.tail,
        });
        self.text = moment.text;
        self.head = moment.head;
        self.tail = moment.tail;
        self.last = Kind::Other;
        true
    }
}

/// What an edit is, for the undo history. A public spelling of the
/// private [`Kind`], so a caller can say what it is doing without the
/// history's shape being anyone else's business.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind2 {
    Typing,
    Deleting,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, head: usize) -> Buffer {
        let mut buffer = Buffer::new(text.to_owned());
        buffer.move_to(head, false);
        buffer
    }

    /// A double click takes the word under the pointer, including the
    /// underscores a file name is full of, and a click just past a word
    /// still means that word.
    #[test]
    fn a_word_is_what_a_double_click_would_take() {
        let buffer = Buffer::new("__pycache__ and .DS_Store".to_owned());
        assert_eq!(buffer.selected_range(buffer.word_at(3)), "__pycache__");
        assert_eq!(buffer.selected_range(buffer.word_at(11)), "__pycache__");
        assert_eq!(buffer.selected_range(buffer.word_at(17)), "DS_Store");
        assert_eq!(buffer.selected_range(buffer.word_at(12)), "and");
    }

    /// A triple click takes the line, and no more than the line.
    #[test]
    fn a_line_is_the_line_without_its_break() {
        let buffer = Buffer::new("one\ntwo\nthree".to_owned());
        assert_eq!(buffer.selected_range(buffer.line_at(5)), "two");
        assert_eq!(buffer.selected_range(buffer.line_at(0)), "one");
        assert_eq!(buffer.selected_range(buffer.line_at(13)), "three");
    }

    /// Option-left and option-right walk words the way they do in every
    /// other field on this machine.
    #[test]
    fn option_walks_by_word() {
        let mut buffer = at("target/demo/media", 17);
        buffer.step(By::Word, false, false);
        assert_eq!(buffer.head(), 12);
        buffer.step(By::Word, false, false);
        assert_eq!(buffer.head(), 7);
        buffer.step(By::Word, true, false);
        assert_eq!(buffer.head(), 11);
    }

    /// Command-left and command-right are the line, not the document.
    #[test]
    fn command_walks_by_line() {
        let mut buffer = at("one\ntwo three\nfour", 9);
        buffer.step(By::Line, false, false);
        assert_eq!(buffer.head(), 4);
        buffer.step(By::Line, true, false);
        assert_eq!(buffer.head(), 13);
        buffer.step(By::All, false, false);
        assert_eq!(buffer.head(), 0);
    }

    /// Up and down keep the column they started in, even across a line
    /// too short to hold it.
    #[test]
    fn up_and_down_keep_their_column() {
        let mut buffer = at("longest line\nx\nanother line", 8);
        buffer.vertical(true, false);
        assert_eq!(buffer.head(), 14, "the short line ends early");
        buffer.vertical(true, false);
        assert_eq!(buffer.head(), 23, "and the column comes back");
    }

    /// Option-backspace takes the word, command-backspace takes the
    /// line up to the caret, and control-K takes what is after it.
    #[test]
    fn the_deletions_take_what_they_say() {
        let mut buffer = at("~/Workspace/Voltai", 18);
        buffer.delete(By::Word, false);
        assert_eq!(buffer.text(), "~/Workspace/");

        let mut buffer = at("one\ntwo three", 13);
        buffer.delete(By::Line, false);
        assert_eq!(buffer.text(), "one\n");

        let mut buffer = at("one\ntwo three", 7);
        buffer.kill_line();
        assert_eq!(buffer.text(), "one\ntwo");
        buffer.kill_line();
        assert_eq!(buffer.text(), "one\ntwo", "nothing left to kill");
    }

    /// A run of typing undoes as one thing, and a run of deleting as
    /// another; redo puts each back.
    #[test]
    fn undo_takes_back_a_run_rather_than_a_character() {
        let mut buffer = Buffer::new(String::new());
        for letter in "node_modules".chars() {
            buffer.insert(&letter.to_string());
        }
        assert_eq!(buffer.text(), "node_modules");
        buffer.delete(By::Word, false);
        assert_eq!(buffer.text(), "");

        assert!(buffer.undo());
        assert_eq!(buffer.text(), "node_modules", "the deletion comes back");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "", "and the typing goes");
        assert!(buffer.redo());
        assert_eq!(buffer.text(), "node_modules");
    }

    /// A sideways key with something selected lands on the edge of it.
    #[test]
    fn leaving_a_selection_lands_on_its_edge() {
        let mut buffer = Buffer::new("abcdef".to_owned());
        buffer.move_to(2, false);
        buffer.step(By::Character, true, true);
        buffer.step(By::Character, true, true);
        assert_eq!(buffer.selected(), "cd");
        buffer.step(By::Character, false, false);
        assert_eq!(buffer.head(), 2);
        assert!(buffer.selection().is_empty());
    }

    impl Buffer {
        /// The text of a range, for the tests to read.
        fn selected_range(&self, range: Range<usize>) -> &str {
            &self.text[range]
        }
    }
}
