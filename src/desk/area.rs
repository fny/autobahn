//! A block of text a person can type into.
//!
//! gpui draws text; it does not hand out a text field, and the one in
//! its examples is a single line. This is that example's machinery with
//! the line count taken out of it and the keys filled in: many lines, a
//! caret the mouse can place, selection by character, word or line, the
//! clipboard, undo, and the input handler that makes accents and other
//! scripts work.
//!
//! The text itself lives in [`super::buffer::Buffer`], which knows
//! nothing about windows. Here is only what a window must do: shape the
//! lines, paint them, turn a click into an offset, and say which keys
//! mean what.
//!
//! The key map is the one every other field on this machine has —
//! command for the line, option for the word, control for the readline
//! set — which is also, gratifyingly, the one GPUI Kit's editor binds.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    div, fill, point, px, relative, size, App, Bounds, ClipboardItem, Context, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PaintQuad, Pixels, Point, ShapedLine, SharedString, Style, TextRun, UTF16Selection,
    UnderlineStyle, Window,
};

use super::buffer::{Buffer, By, Kind2};

/// What the block tells the window it is in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Said {
    /// The text is finished with: keep it.
    Keep,
    /// Leave the value as it was.
    Leave,
}

impl EventEmitter<Said> for Area {}

/// The colours a block is drawn in, from the window's palette.
#[derive(Clone, Copy)]
pub struct Ink {
    pub text: u32,
    pub caret: u32,
    pub selection: u32,
}

/// A block of editable text.
pub struct Area {
    text: Buffer,
    /// Whether a newline is a line or the end of the edit.
    many: bool,
    focus: FocusHandle,
    font: SharedString,
    size: Pixels,
    ink: Ink,
    /// How many lines are shown at once; the rest scroll.
    rows: usize,
    /// The first line shown.
    first: usize,
    /// What the system is composing, while it composes it.
    marked: Option<Range<usize>>,
    /// Where the lines were last drawn. A click is only an offset with
    /// these in hand.
    laid: Option<Laid>,
    dragging: bool,
    /// What a drag takes at a time: a double click takes words, a
    /// triple click takes lines.
    grain: Grain,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Grain {
    Character,
    Word,
    Line,
}

struct Laid {
    lines: Vec<ShapedLine>,
    bounds: Bounds<Pixels>,
    height: Pixels,
    /// Which line the first shaped one is.
    first: usize,
}

impl Area {
    pub fn new(
        text: String,
        many: bool,
        rows: usize,
        font: SharedString,
        size: Pixels,
        ink: Ink,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut area = Area {
            text: Buffer::new(text),
            many,
            focus: cx.focus_handle(),
            font,
            size,
            ink,
            rows: rows.max(1),
            first: 0,
            marked: None,
            laid: None,
            dragging: false,
            grain: Grain::Character,
        };
        area.follow();
        area
    }

    /// What has been typed.
    pub fn text(&self) -> &str {
        self.text.text()
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
    }

    /// Scrolls so the caret is on screen, which every move ends with.
    fn follow(&mut self) {
        let line = self.text.line_of(self.text.head());
        if line < self.first {
            self.first = line;
        }
        let last = self.first + self.rows.saturating_sub(1);
        if line > last {
            self.first = line + 1 - self.rows;
        }
        self.first = self.first.min(self.text.lines().saturating_sub(self.rows));
    }

    /// Which offset a point in the window falls on.
    fn offset_at(&self, at: Point<Pixels>) -> Option<usize> {
        let laid = self.laid.as_ref()?;
        let down = (at.y - laid.bounds.top()).max(px(0.));
        let shown = ((down / laid.height) as usize).min(laid.lines.len().saturating_sub(1));
        let shaped = laid.lines.get(shown)?;
        let across = at.x - laid.bounds.left();
        let within = shaped
            .index_for_x(across)
            .unwrap_or_else(|| match across <= px(0.) {
                true => 0,
                false => shaped.len,
            });
        let line = laid.first + shown;
        Some(self.text.starts().get(line).copied().unwrap_or(0) + within)
    }

    // ── the mouse ────────────────────────────────────────────────────

    fn down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let Some(at) = self.offset_at(event.position) else {
            return;
        };
        self.dragging = true;
        self.grain = match event.click_count {
            0 | 1 => Grain::Character,
            2 => Grain::Word,
            _ => Grain::Line,
        };
        match self.grain {
            Grain::Character => self.text.move_to(at, event.modifiers.shift),
            Grain::Word => self.text.select_word(at),
            Grain::Line => self.text.select_line(at),
        }
        self.follow();
        cx.notify();
    }

    fn moved(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        let Some(at) = self.offset_at(event.position) else {
            return;
        };
        // A drag that began on a word keeps taking whole words, and one
        // that began on a line keeps taking whole lines.
        match self.grain {
            Grain::Character => self.text.move_to(at, true),
            Grain::Word => {
                let word = self.text.word_at(at);
                let to = match word.start < self.text.selection().start {
                    true => word.start,
                    false => word.end,
                };
                self.text.move_to(to, true);
            }
            Grain::Line => {
                let line = self.text.line_at(at);
                let to = match line.start < self.text.selection().start {
                    true => line.start,
                    false => line.end,
                };
                self.text.move_to(to, true);
            }
        }
        self.follow();
        cx.notify();
    }

    fn up(&mut self, _event: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.dragging = false;
    }

    /// The wheel, for a block taller than it is allowed to be.
    fn wheel(
        &mut self,
        event: &gpui::ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lines = self.text.lines();
        if lines <= self.rows {
            return;
        }
        let by = match event.delta {
            gpui::ScrollDelta::Lines(delta) => delta.y,
            gpui::ScrollDelta::Pixels(delta) => (delta.y / window.line_height()) as f32,
        };
        let most = (lines - self.rows) as f32;
        self.first = (self.first as f32 - by).clamp(0., most) as usize;
        cx.notify();
    }

    // ── the keys ─────────────────────────────────────────────────────

    fn key(&mut self, event: &gpui::KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        let key = stroke.key.as_str();
        let command = stroke.modifiers.platform;
        let option = stroke.modifiers.alt;
        let control = stroke.modifiers.control;
        let select = stroke.modifiers.shift;

        // How far a sideways key reaches: command to the line, option
        // to the word, neither to the character.
        let by = match (command, option) {
            (true, _) => By::Line,
            (_, true) => By::Word,
            _ => By::Character,
        };

        match (key, command, option, control) {
            ("escape", ..) => {
                cx.emit(Said::Leave);
                return;
            }
            ("enter", true, ..) => {
                cx.emit(Said::Keep);
                return;
            }
            ("enter", ..) => match self.many {
                true => self.text.insert("\n"),
                false => {
                    cx.emit(Said::Keep);
                    return;
                }
            },

            // The whole text.
            ("a", true, ..) => self.text.select_all(),
            ("c", true, ..) | ("x", true, ..) => {
                let taken = self.text.selected().to_owned();
                if !taken.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(taken));
                    if key == "x" {
                        self.text.delete(By::Character, true);
                    }
                }
            }
            ("v", true, ..) => {
                if let Some(pasted) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let pasted = match self.many {
                        true => pasted,
                        false => pasted.lines().next().unwrap_or_default().to_owned(),
                    };
                    self.text.insert(&pasted);
                }
            }
            ("z", true, ..) => {
                match select {
                    true => self.text.redo(),
                    false => self.text.undo(),
                };
            }

            // Where the caret goes.
            ("left", ..) => self.text.step(by, false, select),
            ("right", ..) => self.text.step(by, true, select),
            ("up", true, ..) => self.text.step(By::All, false, select),
            ("down", true, ..) => self.text.step(By::All, true, select),
            ("up", ..) => self.text.vertical(false, select),
            ("down", ..) => self.text.vertical(true, select),
            ("home", false, false, true) | ("end", false, false, true) => {
                self.text.step(By::All, key == "end", select)
            }
            ("home", ..) => self.text.step(By::Line, false, select),
            ("end", ..) => self.text.step(By::Line, true, select),
            ("pageup", ..) => {
                for _ in 0..self.rows {
                    self.text.vertical(false, select);
                }
            }
            ("pagedown", ..) => {
                for _ in 0..self.rows {
                    self.text.vertical(true, select);
                }
            }

            // The readline set, which this machine honours everywhere.
            ("a", false, false, true) => self.text.step(By::Line, false, select),
            ("e", false, false, true) => self.text.step(By::Line, true, select),
            ("b", false, false, true) => self.text.step(By::Character, false, select),
            ("f", false, false, true) => self.text.step(By::Character, true, select),
            ("p", false, false, true) => self.text.vertical(false, select),
            ("n", false, false, true) => self.text.vertical(true, select),
            ("k", false, false, true) => self.text.kill_line(),
            ("h", false, false, true) => self.text.delete(By::Character, false),
            ("d", false, false, true) => self.text.delete(By::Character, true),

            // What comes out.
            ("backspace", ..) => self.text.delete(by, false),
            ("delete", ..) => self.text.delete(by, true),

            _ => {
                // Everything else is the system's to turn into text, and
                // it arrives through the input handler below — which is
                // what makes an accent, or a script that composes, work
                // at all.
                return;
            }
        }
        self.marked = None;
        self.follow();
        cx.notify();
    }
}

impl Focusable for Area {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Typing, as the system hands it over: plain characters, and the
/// half-finished marks a composing keyboard makes on the way.
impl EntityInputHandler for Area {
    fn text_for_range(
        &mut self,
        wanted: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.text.text();
        let range = wanted.start.min(text.len())..wanted.end.min(text.len());
        if range != wanted {
            *adjusted = Some(range.clone());
        }
        Some(text[range].to_owned())
    }

    /// Which character a point falls on, for the system's own
    /// hit-testing.
    fn character_index_for_point(
        &mut self,
        at: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.offset_at(at)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let range = self.text.selection();
        Some(UTF16Selection {
            range,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range.or(self.marked.clone()).unwrap_or(self.text.selection());
        self.text.replace(range, text, Kind2::Typing);
        self.marked = None;
        self.follow();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        marked: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range.or(self.marked.clone()).unwrap_or(self.text.selection());
        let start = range.start;
        self.text.replace(range, text, Kind2::Typing);
        self.marked = (!text.is_empty()).then(|| start..start + text.len());
        if let Some(marked) = marked {
            self.marked = Some(start + marked.start..start + marked.end);
        }
        self.follow();
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(bounds)
    }
}

impl Render for Area {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::down))
            .on_mouse_move(cx.listener(Self::moved))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::up))
            .on_scroll_wheel(cx.listener(Self::wheel))
            .cursor_text()
            .w_full()
            .child(Written {
                area: cx.entity(),
            })
    }
}

/// The lines themselves: shaped, painted, and measured so that the next
/// click knows where it landed.
struct Written {
    area: Entity<Area>,
}

struct Drawn {
    lines: Vec<ShapedLine>,
    quads: Vec<PaintQuad>,
    caret: Option<PaintQuad>,
    height: Pixels,
    first: usize,
}

impl IntoElement for Written {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Written {
    type RequestLayoutState = ();
    type PrepaintState = Drawn;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let area = self.area.read(cx);
        let lines = area.text.lines().clamp(1, area.rows);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (window.line_height() * lines as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let area = self.area.read(cx);
        let selection = area.text.selection();
        let head = area.text.head();
        let height = window.line_height();
        let mut style = window.text_style();
        style.font_family = area.font.clone();
        style.font_size = area.size.into();
        let font = style.font();
        let first = area.first;

        let mut lines = Vec::new();
        let mut quads = Vec::new();
        let mut caret = None;
        // Only the lines on screen are shaped: a hundred ignore
        // patterns are ten lines of work, not a hundred.
        let mut at = area.text.starts().get(first).copied().unwrap_or(0);
        for (index, text) in area
            .text
            .text()
            .split('\n')
            .skip(first)
            .take(area.rows)
            .enumerate()
        {
            let whole = TextRun {
                len: text.len(),
                font: font.clone(),
                color: gpui::rgb(area.ink.text).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = at..at + text.len();
            // What the system is composing is underlined, as it is in
            // every other field, so half-typed text looks half-typed.
            let runs = match area.marked.as_ref().filter(|marked| {
                marked.start >= line.start && marked.end <= line.end && marked.start < marked.end
            }) {
                Some(marked) => vec![
                    TextRun {
                        len: marked.start - line.start,
                        ..whole.clone()
                    },
                    TextRun {
                        len: marked.end - marked.start,
                        underline: Some(UnderlineStyle {
                            color: Some(gpui::rgb(area.ink.caret).into()),
                            thickness: px(1.),
                            wavy: false,
                        }),
                        ..whole.clone()
                    },
                    TextRun {
                        len: line.end - marked.end,
                        ..whole.clone()
                    },
                ]
                .into_iter()
                .filter(|run| run.len > 0)
                .collect(),
                None => vec![whole],
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(text.to_owned()),
                area.size,
                &runs,
                None,
            );
            let top = bounds.top() + height * index as f32;
            let start = selection.start.max(line.start);
            let end = selection.end.min(line.end);
            if start < end || (selection.start <= line.end && selection.end > line.end) {
                let from = shaped.x_for_index(start.saturating_sub(line.start));
                let to = match selection.end > line.end {
                    // The selection runs past this line, so it takes the
                    // newline with it and the highlight runs a little
                    // past the last character.
                    true => shaped.width + px(4.),
                    false => shaped.x_for_index(end - line.start),
                };
                if to > from {
                    quads.push(fill(
                        Bounds::from_corners(
                            point(bounds.left() + from, top),
                            point(bounds.left() + to, top + height),
                        ),
                        gpui::rgba((area.ink.selection << 8) | 0x55),
                    ));
                }
            }
            if selection.is_empty() && (line.contains(&head) || head == line.end) {
                let x = shaped.x_for_index(head - line.start);
                caret = Some(fill(
                    Bounds::new(point(bounds.left() + x, top), size(px(1.5), height)),
                    gpui::rgb(area.ink.caret),
                ));
            }
            lines.push(shaped);
            at = line.end + 1;
        }
        Drawn {
            lines,
            quads,
            caret,
            height,
            first,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut Self::RequestLayoutState,
        drawn: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.area.read(cx).focus.clone();
        // What makes typing arrive at all, and composing work.
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.area.clone()),
            cx,
        );
        for quad in drawn.quads.drain(..) {
            window.paint_quad(quad);
        }
        for (index, line) in drawn.lines.iter().enumerate() {
            let origin = point(bounds.left(), bounds.top() + drawn.height * index as f32);
            line.paint(origin, drawn.height, window, cx).ok();
        }
        if let (true, Some(caret)) = (focus.is_focused(window), drawn.caret.take()) {
            window.paint_quad(caret);
        }
        let lines = std::mem::take(&mut drawn.lines);
        let height = drawn.height;
        let first = drawn.first;
        self.area.update(cx, |area, _| {
            area.laid = Some(Laid {
                lines,
                bounds,
                height,
                first,
            });
        });
    }
}
