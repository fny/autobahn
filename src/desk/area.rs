//! A block of text a person can type into.
//!
//! gpui draws text; it does not hand out a text field, and the one in
//! its examples is a single line. A configuration is mostly lines —
//! twenty ignore patterns, three betas — so this is the same machinery
//! with the line count taken out of it: many lines, a caret that can be
//! put anywhere with the mouse, a selection that spans lines, and the
//! clipboard.
//!
//! It holds text and nothing else. What the text means, when it is
//! written and what refuses it are the window's business.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    div, fill, point, px, relative, size, App, Bounds, ClipboardItem, Context, Element, ElementId,
    Entity, EventEmitter, FocusHandle, Focusable, GlobalElementId, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, SharedString,
    ShapedLine, Style, TextRun, Window,
};

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
    text: String,
    /// Byte offsets into `text`: the caret is `head`, and a selection
    /// runs from `tail` to it.
    head: usize,
    tail: usize,
    /// Whether a newline is a line or the end of the edit.
    lines: bool,
    focus: FocusHandle,
    font: SharedString,
    size: Pixels,
    ink: Ink,
    /// Where the lines were last drawn. A click is only an offset with
    /// these in hand.
    laid: Option<Laid>,
    dragging: bool,
}

struct Laid {
    lines: Vec<ShapedLine>,
    bounds: Bounds<Pixels>,
    height: Pixels,
}

impl Area {
    pub fn new(
        text: String,
        lines: bool,
        font: SharedString,
        size: Pixels,
        ink: Ink,
        cx: &mut Context<Self>,
    ) -> Self {
        let end = text.len();
        Area {
            text,
            head: end,
            tail: end,
            lines,
            focus: cx.focus_handle(),
            font,
            size,
            ink,
            laid: None,
            dragging: false,
        }
    }

    /// What has been typed.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
    }

    fn selection(&self) -> Range<usize> {
        match self.head <= self.tail {
            true => self.head..self.tail,
            false => self.tail..self.head,
        }
    }

    fn put(&mut self, what: &str) {
        let range = self.selection();
        self.text.replace_range(range.clone(), what);
        self.head = range.start + what.len();
        self.tail = self.head;
    }

    /// The byte offsets each line starts at, including the one after a
    /// trailing newline.
    fn starts(&self) -> Vec<usize> {
        let mut starts = vec![0];
        for (at, byte) in self.text.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(at + 1);
            }
        }
        starts
    }

    fn line_of(&self, offset: usize) -> usize {
        self.starts()
            .iter()
            .rposition(|start| *start <= offset)
            .unwrap_or(0)
    }

    fn line_end(&self, line: usize) -> usize {
        let starts = self.starts();
        match starts.get(line + 1) {
            Some(next) => next - 1,
            None => self.text.len(),
        }
    }

    fn step(&self, from: usize, forward: bool) -> usize {
        match forward {
            true => {
                let mut at = (from + 1).min(self.text.len());
                while !self.text.is_char_boundary(at) {
                    at += 1;
                }
                at
            }
            false => {
                let mut at = from.saturating_sub(1);
                while at > 0 && !self.text.is_char_boundary(at) {
                    at -= 1;
                }
                at
            }
        }
    }

    fn go(&mut self, to: usize, extend: bool) {
        self.head = to;
        if !extend {
            self.tail = to;
        }
    }

    /// The same column on the line above or below, as far as that line
    /// reaches.
    fn vertical(&mut self, down: bool, extend: bool) {
        let starts = self.starts();
        let line = self.line_of(self.head);
        let column = self.head - starts[line];
        let wanted = match down {
            true => line + 1,
            false => match line {
                0 => return,
                line => line - 1,
            },
        };
        let Some(start) = starts.get(wanted) else {
            return;
        };
        let end = self.line_end(wanted);
        let mut at = (start + column).min(end);
        while !self.text.is_char_boundary(at) {
            at -= 1;
        }
        self.go(at, extend);
    }

    /// Which offset a point in the window falls on.
    fn offset_at(&self, at: Point<Pixels>) -> Option<usize> {
        let laid = self.laid.as_ref()?;
        let down = (at.y - laid.bounds.top()).max(px(0.));
        let line = ((down / laid.height) as usize).min(laid.lines.len().saturating_sub(1));
        let shaped = laid.lines.get(line)?;
        let across = at.x - laid.bounds.left();
        let within = shaped
            .index_for_x(across)
            .unwrap_or_else(|| match across <= px(0.) {
                true => 0,
                false => shaped.len,
            });
        Some(self.starts().get(line).copied().unwrap_or(0) + within)
    }

    // ── what the mouse does ──────────────────────────────────────────

    fn down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        if let Some(at) = self.offset_at(event.position) {
            self.dragging = true;
            self.go(at, event.modifiers.shift);
            cx.notify();
        }
    }

    fn moved(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.dragging {
            return;
        }
        if let Some(at) = self.offset_at(event.position) {
            self.go(at, true);
            cx.notify();
        }
    }

    fn up(&mut self, _event: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.dragging = false;
    }

    // ── what the keys do ─────────────────────────────────────────────

    fn key(&mut self, event: &gpui::KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let command = event.keystroke.modifiers.platform;
        let shift = event.keystroke.modifiers.shift;
        match (key, command) {
            ("escape", _) => {
                cx.emit(Said::Leave);
                return;
            }
            ("enter", true) => {
                cx.emit(Said::Keep);
                return;
            }
            ("enter", false) => match self.lines {
                true => self.put("\n"),
                false => {
                    cx.emit(Said::Keep);
                    return;
                }
            },
            ("a", true) => {
                self.tail = 0;
                self.head = self.text.len();
            }
            ("c", true) | ("x", true) => {
                let taken = self.text[self.selection()].to_owned();
                if !taken.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(taken));
                    if key == "x" {
                        self.put("");
                    }
                }
            }
            ("v", true) => {
                if let Some(pasted) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let pasted = match self.lines {
                        true => pasted,
                        false => pasted.lines().next().unwrap_or_default().to_owned(),
                    };
                    self.put(&pasted);
                }
            }
            ("backspace", _) => match self.selection().is_empty() {
                true => {
                    let to = self.step(self.head, false);
                    self.tail = to;
                    self.put("");
                }
                false => self.put(""),
            },
            ("delete", _) => match self.selection().is_empty() {
                true => {
                    let to = self.step(self.head, true);
                    self.tail = to;
                    self.put("");
                }
                false => self.put(""),
            },
            ("left", _) => {
                let to = self.step(self.head, false);
                self.go(to, shift);
            }
            ("right", _) => {
                let to = self.step(self.head, true);
                self.go(to, shift);
            }
            ("up", _) => self.vertical(false, shift),
            ("down", _) => self.vertical(true, shift),
            ("home", _) => {
                let line = self.line_of(self.head);
                let to = self.starts()[line];
                self.go(to, shift);
            }
            ("end", _) => {
                let to = self.line_end(self.line_of(self.head));
                self.go(to, shift);
            }
            _ => {
                if command {
                    return;
                }
                let Some(typed) = event.keystroke.key_char.as_ref() else {
                    return;
                };
                self.put(typed);
            }
        }
        cx.notify();
    }
}

impl Focusable for Area {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
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
        let lines = area.text.split('\n').count().max(1);
        let height = window.line_height() * lines as f32;
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = height.into();
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
        let selection = area.selection();
        let height = window.line_height();
        let mut style = window.text_style();
        style.font_family = area.font.clone();
        style.font_size = area.size.into();
        let font = style.font();

        let mut lines = Vec::new();
        let mut quads = Vec::new();
        let mut caret = None;
        let mut at = 0usize;
        for (index, text) in area.text.split('\n').enumerate() {
            let run = TextRun {
                len: text.len(),
                font: font.clone(),
                color: gpui::rgb(area.ink.text).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(text.to_owned()),
                area.size,
                &[run],
                None,
            );
            let top = bounds.top() + height * index as f32;
            let line = at..at + text.len();
            // The part of this line the selection covers, if any.
            let start = selection.start.max(line.start);
            let end = selection.end.min(line.end);
            if start < end || (selection.start <= line.end && selection.end > line.end) {
                let from = shaped.x_for_index(start.saturating_sub(line.start));
                let to = match selection.end > line.end {
                    // The selection runs past this line, so it takes the
                    // newline with it and the highlight runs to the edge.
                    true => shaped.width + px(4.),
                    false => shaped.x_for_index(end - line.start),
                };
                if to > from {
                    quads.push(fill(
                        Bounds::from_corners(
                            point(bounds.left() + from, top),
                            point(bounds.left() + to, top + height),
                        ),
                        gpui::rgba(((area.ink.selection as u64) << 8 | 0x55) as u32),
                    ));
                }
            }
            if selection.is_empty() && (line.contains(&area.head) || area.head == line.end) {
                let x = shaped.x_for_index(area.head - line.start);
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
        for quad in drawn.quads.drain(..) {
            window.paint_quad(quad);
        }
        for (index, line) in drawn.lines.iter().enumerate() {
            let origin = point(bounds.left(), bounds.top() + drawn.height * index as f32);
            line.paint(origin, drawn.height, window, cx).ok();
        }
        let focused = self.area.read(cx).focus.is_focused(window);
        if let (true, Some(caret)) = (focused, drawn.caret.take()) {
            window.paint_quad(caret);
        }
        let lines = std::mem::take(&mut drawn.lines);
        let height = drawn.height;
        self.area.update(cx, |area, _| {
            area.laid = Some(Laid {
                lines,
                bounds,
                height,
            });
        });
    }
}
