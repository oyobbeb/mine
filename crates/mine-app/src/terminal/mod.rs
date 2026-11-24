//! The terminal pane: draws the attached session's screen (`Model::screen`) and wires pane size, the mouse (selection,
//! ⌘C, links, reports to the session, the wheel), keys, the input method, paste, and the ⌘F find box to the model.
//!
//! The structure is the same as Zed's terminal: inside the `div` that [`render`] builds (focus, key context [`CONTEXT`],
//! mouse and key handling) there is a single `canvas`. The find box floats over its top right corner.
//! - prepaint: derives rows and columns from the pane size and reports them to the model (`Attach` the first time,
//!   `Resize` when it changes). Then, for each visible row, builds text runs with [`render::row_runs`] and shapes them.
//! - paint: registers the input method handler and the window-wide drag and release listeners, then draws background
//!   rectangles, the selection and find matches, text, underlines, the cursor, and the text being composed, in that
//!   order.
//!
//! Keys take two paths, so none is sent twice. A key with an encoding of its own (Enter, arrows, Ctrl+C) is sent from
//! the key-down listener, which stops it there. A key that types a character is left alone, and its text arrives
//! through the input method (`EntityInputHandler`). While a syllable is being composed, macOS hands every key to the
//! input method first, so the syllable is committed before the key's own bytes: 한 then Enter sends "한" then "\r".

pub mod find;
pub mod ime;
pub mod keys;
pub mod mouse;
pub mod render;

use std::ops::Range;
use std::time::Instant;

use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::CursorShape;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Bounds, ClipboardItem, ContentMask, Context, DispatchPhase,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, FontStyle, FontWeight, Hsla,
    InputHandler, InteractiveElement, IntoElement, KeyDownEvent, Keystroke, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, ScrollWheelEvent,
    ShapedLine, Styled, Subscription, TextRun, TouchPhase, UTF16Selection, Window,
    WindowTextSystem, canvas, div, fill, font, outline, point, px, rgb, size,
};
use gpui_component::Sizable;
use gpui_component::input::{Input, InputEvent, InputState};
use mine_core::keys::KeyModes;
use mine_core::select::{MouseEv, MouseKind};

use crate::actions::{self, CloseFind, Find, FindPrev, Paste};
use crate::theme::{FAINT, TERM_BG, TERM_CURSOR, TERM_FG, TERM_SELECTION};
use crate::workspace::Workspace;
use find::FindState;
use ime::Ime;
use keys::KeyIn;
use mouse::Act;
use render::{CellRun, CursorCell};

/// The terminal pane's key context. It is inside the `Workspace` context, so ⌘ shortcuts still arrive even when the pane has focus.
pub const CONTEXT: &str = "Terminal";
/// The find box's key context, for Shift+Enter and Esc. The box is beside the pane, not in it, so keys typed into it
/// don't reach the pane's key-down listener (and the session).
pub const FIND_CONTEXT: &str = "TerminalFind";
const FONT_FAMILY: &str = "Menlo";
const FONT_SIZE: Pixels = px(13.);
/// Margin around the pane's edge.
const PADDING: Pixels = px(6.);
/// Opacity of dim text (SGR 2).
const DIM: f32 = 0.6;
/// Thickness of the bar and underline cursors.
const CARET: Pixels = px(2.);
/// Thickness of underlines and strikethroughs.
const RULE: Pixels = px(1.);

/// The size of one cell: the width of 'M' in Menlo 13px and the line height (ascent + descent, rounded up), and where
/// the underline and strikethrough sit from the top of the cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub cell_w: Pixels,
    pub line_h: Pixels,
    pub underline: Pixels,
    pub strike: Pixels,
}

impl Metrics {
    pub fn measure(window: &Window) -> Metrics {
        let text = window.text_system();
        let id = text.resolve_font(&font(FONT_FAMILY));
        let cell_w = text
            .advance(id, FONT_SIZE, 'M')
            .map_or(FONT_SIZE * 0.6, |advance| advance.width);
        // The font's descent is negative.
        let ascent = text.ascent(id, FONT_SIZE);
        let descent = text.descent(id, FONT_SIZE).abs();
        let line_h = (ascent + descent).ceil();
        // The same places GPUI's own text decoration uses (`ShapedLine::paint`).
        let baseline = (line_h - ascent - descent) / 2. + ascent;
        Metrics {
            cell_w,
            line_h,
            underline: baseline + descent * 0.618,
            strike: (ascent * 0.5 + baseline) * 0.5,
        }
    }
}

/// The position and cell size of the pane as last drawn. Used to turn a mouse position into a cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    /// Top left of the first cell (0, 0), in window coordinates.
    pub origin: Point<Pixels>,
    pub metrics: Metrics,
    pub cols: u16,
    pub rows: u16,
}

impl Geometry {
    /// The cell (column, row) under window coordinates `pos`. None if it is outside the pane.
    pub fn cell_at(&self, pos: Point<Pixels>) -> Option<(u16, u16)> {
        let x = f32::from(pos.x - self.origin.x) / f32::from(self.metrics.cell_w);
        let y = f32::from(pos.y - self.origin.y) / f32::from(self.metrics.line_h);
        if !(x >= 0.0 && y >= 0.0) {
            return None;
        }
        let (col, row) = (x as u16, y as u16);
        (col < self.cols && row < self.rows).then_some((col, row))
    }

    /// The cell under window coordinates `pos`, clamped into the pane (for drags and releases outside it).
    pub fn clamped(&self, pos: Point<Pixels>) -> (u16, u16) {
        mouse::cell_at(
            (pos.x - self.origin.x).into(),
            (pos.y - self.origin.y).into(),
            self.metrics.cell_w.into(),
            self.metrics.line_h.into(),
            self.cols,
            self.rows,
        )
    }

    /// The rectangle of `cells` cells starting at (`col`, `row`), in window coordinates.
    pub fn cell_bounds(&self, col: u16, row: u16, cells: u16) -> Bounds<Pixels> {
        let Metrics { cell_w, line_h, .. } = self.metrics;
        Bounds::new(
            point(
                self.origin.x + cell_w * f32::from(col),
                self.origin.y + line_h * f32::from(row),
            ),
            size(cell_w * f32::from(cells), line_h),
        )
    }
}

/// View state of the terminal pane. The screen, attaching and size live in `Model`; only GPUI-side work is here.
pub struct Pane {
    /// While a session is attached and the launcher is closed, focus is always here. Keys and the input method arrive here.
    pub focus: FocusHandle,
    /// The text the input method is composing. Drawn at the cursor, sent once committed.
    ime: Ime,
    /// The pane as last drawn. None before the first draw.
    pub geometry: Option<Geometry>,
    /// The remainder (pixels) of wheel movement that doesn't make a full line.
    scroll_acc: f32,
    /// The deadline of the timer set for a synchronized update that hasn't ended. It is not set twice for the same deadline.
    sync_timer: Option<Instant>,
    /// The ⌘F find box, while open.
    find: Option<FindBox>,
}

/// The open find box: its input, the subscription to its edits and Enter, and the matches.
struct FindBox {
    input: Entity<InputState>,
    state: FindState,
    _events: Subscription,
}

impl Pane {
    pub fn new(cx: &mut App) -> Pane {
        Pane {
            focus: cx.focus_handle(),
            ime: Ime::default(),
            geometry: None,
            scroll_acc: 0.0,
            sync_timer: None,
            find: None,
        }
    }
}

impl Workspace {
    /// Trackpad/wheel: goes to the session while it has mouse reporting on and Shift isn't held; otherwise scrolls the
    /// local copy of the screen.
    fn term_scroll(&mut self, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let Some(geometry) = self.term.geometry else {
            return;
        };
        if matches!(ev.touch_phase, TouchPhase::Started) {
            self.term.scroll_acc = 0.0;
        }
        let line_h = geometry.metrics.line_h;
        let mut delta = ev.delta.pixel_delta(line_h);
        // macOS turns a mouse wheel turned with Shift held into sideways scrolling.
        if ev.modifiers.shift && delta.y == px(0.) {
            delta.y = delta.x;
        }
        let lines = render::scroll_lines(&mut self.term.scroll_acc, delta.y.into(), line_h.into());
        let at = geometry.clamped(ev.position);
        let shift = ev.modifiers.shift;
        let act = self
            .model
            .mouse
            .wheel(self.model.screen.as_ref(), lines, at, shift);
        self.term_act(act, ev.position, cx);
    }

    fn term_link(&self, (col, row): (u16, u16)) -> Option<String> {
        render::link_at(self.model.screen.as_ref()?, col, row)
    }

    fn term_cell(&self, pos: Point<Pixels>) -> Option<(u16, u16)> {
        self.term.geometry?.cell_at(pos)
    }

    /// A press, drag or release at window position `pos`. Drags and releases arrive from the whole window
    /// ([`track_mouse`]); the model only acts on them for a press that started in the pane.
    fn term_mouse(
        &mut self,
        kind: MouseKind,
        button: MouseButton,
        pos: Point<Pixels>,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        let button = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::Navigate(_) => return,
        };
        let Some(geometry) = self.term.geometry else {
            return;
        };
        let (col, row) = geometry.clamped(pos);
        let ev = MouseEv {
            kind,
            button,
            col,
            row,
        };
        let act = self
            .model
            .mouse
            .event(self.model.screen.as_ref(), ev, shift);
        self.term_act(act, pos, cx);
    }

    fn term_act(&mut self, act: Act, pos: Point<Pixels>, cx: &mut Context<Self>) {
        match act {
            Act::None => {}
            Act::Redraw => cx.notify(),
            Act::Send(bytes) => self.term_send(bytes, cx),
            Act::Scroll(lines) => {
                if self.model.scroll(lines) {
                    cx.notify();
                }
            }
            // Only a release on the pressed cell itself opens the link, not one outside the pane that clamps onto it.
            Act::Click(at) => {
                if self.term_cell(pos) == Some(at)
                    && let Some(url) = self.term_link(at).filter(|url| render::openable(url))
                {
                    cx.open_url(&url);
                }
            }
        }
    }

    /// ⌘C: the selected text to the clipboard. Without a selection nothing happens.
    fn term_copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.model.mouse.copy(self.model.screen.as_ref()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// ⌘F: opens the find box, or gives focus back to the open one.
    fn find_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = match &self.term.find {
            Some(find) => find.input.clone(),
            None => {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find"));
                let events = cx.subscribe(&input, |ws, input, ev: &InputEvent, cx| match ev {
                    InputEvent::Change => {
                        let query = input.read(cx).value().to_string();
                        ws.find_search(query, cx);
                    }
                    InputEvent::PressEnter { .. } => ws.find_step(true, cx),
                    InputEvent::Focus | InputEvent::Blur => {}
                });
                self.term.find = Some(FindBox {
                    input: input.clone(),
                    state: FindState::default(),
                    _events: events,
                });
                input
            }
        };
        input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// Typing in the find box searches the scrollback and visible lines and shows the newest match.
    fn find_search(&mut self, query: String, cx: &mut Context<Self>) {
        let lines = self.history();
        let Some(find) = &mut self.term.find else {
            return;
        };
        find.state.query = query;
        find.state.search(&lines);
        let hit = find.state.hits.get(find.state.current).copied();
        self.find_reveal(hit, cx);
    }

    /// Enter (next match) and Shift+Enter (previous match). Searches again first: since the last search the session may
    /// have printed more, been resized, or been switched for another.
    fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let lines = self.history();
        let Some(find) = &mut self.term.find else {
            return;
        };
        let hit = find.state.step(&lines, forward);
        self.find_reveal(hit, cx);
    }

    /// The attached session's scrollback and visible lines, oldest first.
    fn history(&self) -> Vec<String> {
        self.model
            .screen
            .as_ref()
            .map_or_else(Vec::new, |s| s.history_text())
    }

    /// Scrolls so the match's line is visible.
    fn find_reveal(&mut self, hit: Option<(usize, usize)>, cx: &mut Context<Self>) {
        if let (Some((line, _)), Some(screen)) = (hit, &self.model.screen) {
            let lines = find::reveal(screen, line);
            self.model.scroll(lines);
        }
        cx.notify();
    }

    /// Esc in the find box: closes it and gives focus back to the pane.
    fn find_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.term.find = None;
        window.focus(&self.term.focus);
        cx.notify();
    }

    /// A key with an encoding of its own goes to the session in the session's key modes, and stops here so the input
    /// method doesn't send it again. Keys that type a character pass on to the input method.
    fn term_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let modes = self.model.screen.as_ref().map(KeyModes::of);
        let Some(bytes) = keys::encode_key(&key_in(&ev.keystroke), &modes.unwrap_or_default())
        else {
            return;
        };
        self.term_send(bytes, cx);
        cx.stop_propagation();
    }

    /// ⌘V: the clipboard text, bracketed if the session turned on bracketed paste.
    fn term_paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self
            .model
            .screen
            .as_ref()
            .is_some_and(|s| s.mode().contains(TermMode::BRACKETED_PASTE));
        self.term_send(keys::paste_bytes(&text, bracketed), cx);
    }

    /// Sends input to the session. The model returns a scrolled-back view to the bottom, so it redraws.
    fn term_send(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if !bytes.is_empty() {
            let effects = self.model.input(bytes);
            self.apply(effects);
        }
        cx.notify();
    }

    /// The visible cell of the session cursor, where the input method composes. It follows the cursor even while the
    /// session hides it (Claude Code draws its own).
    fn ime_cell(&self) -> Option<(u16, u16)> {
        let screen = self.model.screen.as_ref()?;
        let (col, row) = screen.cursor();
        let offset = screen.term().grid().display_offset();
        Some((col, row.saturating_add(offset as u16)))
    }

    /// Sets a GPUI timer for the attached session's unfinished synchronized update (`?2026`). When the deadline passes, ends it and redraws.
    pub(crate) fn watch_sync(&mut self, cx: &mut Context<Self>) {
        let Some(deadline) = self.model.sync_deadline() else {
            return;
        };
        if self.term.sync_timer == Some(deadline) {
            return;
        }
        self.term.sync_timer = Some(deadline);
        cx.spawn(async move |this, cx| {
            let wait = deadline.saturating_duration_since(Instant::now());
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |ws, cx| {
                // Only this timer's own mark: if a newer update already set a timer, clearing its mark here would
                // let the call below set a second one for the same deadline.
                ws.term.sync_timer.take_if(|set| *set == deadline);
                if ws.model.end_stale_sync(Instant::now()) {
                    cx.notify();
                }
                // If a new update opened in the meantime, set the timer again for that deadline.
                ws.watch_sync(cx);
            });
        })
        .detach();
    }
}

/// The pane of the attached session, with the find box over its top right corner while open. Clicking the pane gives
/// it focus.
pub fn render(ws: &Workspace, window: &Window, cx: &mut Context<Workspace>) -> impl IntoElement {
    let entity = cx.entity();
    let input = (ws.term.focus.clone(), entity.clone());
    let focused = ws.term.focus.is_focused(window);
    let pane = div()
        .size_full()
        .p(PADDING)
        .bg(rgb(TERM_BG))
        .track_focus(&ws.term.focus)
        .key_context(CONTEXT)
        .on_key_down(cx.listener(|ws, ev: &KeyDownEvent, _, cx| ws.term_key(ev, cx)))
        .on_action(cx.listener(|ws, _: &Paste, _, cx| ws.term_paste(cx)))
        .on_action(cx.listener(|ws, _: &actions::Copy, _, cx| ws.term_copy(cx)))
        .on_action(cx.listener(|ws, _: &Find, window, cx| ws.find_open(window, cx)))
        .on_scroll_wheel(cx.listener(|ws, ev: &ScrollWheelEvent, _, cx| ws.term_scroll(ev, cx)))
        .on_any_mouse_down(cx.listener(|ws, ev: &MouseDownEvent, _, cx| {
            let shift = ev.modifiers.shift;
            ws.term_mouse(MouseKind::Down, ev.button, ev.position, shift, cx)
        }))
        .child(
            canvas(
                move |bounds, window, cx| layout(&entity, focused, bounds, window, cx),
                move |bounds, frame, window, cx| {
                    let (focus, entity) = input;
                    track_mouse(&entity, window);
                    let handler = TermInput(ElementInputHandler::new(bounds, entity));
                    window.handle_input(&focus, handler, cx);
                    frame.paint(window, cx)
                },
            )
            .size_full(),
        );
    div()
        .relative()
        .flex_1()
        .min_h_0()
        .child(pane)
        .when_some(ws.term.find.as_ref(), |this, find| {
            this.child(find_box(find, cx))
        })
}

/// Drags and releases are heard on the whole window, not only over the pane: a press that started in the pane keeps
/// extending its selection, or reporting to the session, outside the pane, and still gets its release there (R26).
/// The capture phase runs before any element can stop the event.
fn track_mouse(ws: &Entity<Workspace>, window: &mut Window) {
    let moved = ws.clone();
    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
        if phase == DispatchPhase::Capture
            && let Some(button) = ev.pressed_button
        {
            moved.update(cx, |ws, cx| {
                ws.term_mouse(MouseKind::Drag, button, ev.position, ev.modifiers.shift, cx)
            });
        }
    });
    let released = ws.clone();
    window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
        if phase == DispatchPhase::Capture {
            released.update(cx, |ws, cx| {
                ws.term_mouse(
                    MouseKind::Up,
                    ev.button,
                    ev.position,
                    ev.modifiers.shift,
                    cx,
                )
            });
        }
    });
}

/// The find box over the pane's top right corner, with the match count. Enter arrives as the input's own
/// `PressEnter`; Shift+Enter and Esc are bindings in [`FIND_CONTEXT`]. It occludes the pane, so clicks and drags in
/// it don't select terminal text.
fn find_box(find: &FindBox, cx: &mut Context<Workspace>) -> impl IntoElement {
    let status = find.state.status().map(|status| {
        div()
            .text_size(px(11.))
            .text_color(rgb(FAINT))
            .child(status)
    });
    div()
        .absolute()
        .top(PADDING)
        .right(px(16.))
        .w(px(260.))
        .key_context(FIND_CONTEXT)
        .occlude()
        .on_action(cx.listener(|ws, _: &FindPrev, _, cx| ws.find_step(false, cx)))
        .on_action(cx.listener(|ws, _: &CloseFind, window, cx| ws.find_close(window, cx)))
        .child(
            Input::new(&find.input)
                .small()
                .when_some(status, |input, status| input.suffix(status)),
        )
}

/// prepaint: reports the pane size to the model, and shapes the visible rows' text runs and the cursor.
fn layout(
    ws: &Entity<Workspace>,
    focused: bool,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> Frame {
    let metrics = Metrics::measure(window);
    let (cols, rows) = render::grid_size(
        bounds.size.width.into(),
        bounds.size.height.into(),
        metrics.cell_w.into(),
        metrics.line_h.into(),
    );
    let geometry = Geometry {
        origin: bounds.origin,
        metrics,
        cols,
        rows,
    };
    // The view's render is done, so the view can be modified. The screen is fitted to the pane size here, so this frame is drawn at the new size.
    let (runs, cursor, composing, marks) = ws.update(cx, |ws, _| {
        ws.term.geometry = Some(geometry);
        let effects = ws.model.set_grid(cols, rows);
        ws.apply(effects);
        let composing = ws.term.ime.marked().zip(ws.ime_cell()).map(composing_run);
        let Some(screen) = &ws.model.screen else {
            return (Vec::new(), None, None, Vec::new());
        };
        let runs: Vec<(u16, CellRun)> = (0..rows)
            .flat_map(|row| {
                render::row_runs(screen, usize::from(row))
                    .into_iter()
                    .map(move |run| (row, run))
            })
            .collect();
        // The selection and the find box's matches, both on the selection color.
        let mut marks = ws
            .model
            .mouse
            .selection()
            .map(|sel| mouse::spans(&sel, cols, rows))
            .unwrap_or_default();
        if let Some(find) = &ws.term.find {
            marks.extend(find::visible(screen, &find.state.query));
        }
        (runs, render::cursor(screen), composing, marks)
    });
    let marks = marks
        .into_iter()
        .map(|(row, col, cells)| geometry.cell_bounds(col, row, cells))
        .collect();
    let text = window.text_system().clone();
    let cells = runs
        .into_iter()
        .map(|(row, run)| Painted::shape(&text, &geometry, row, &run))
        .collect();
    let caret = cursor
        .filter(|c| c.col < cols && c.row < rows)
        .map(|c| Caret::shape(&text, &geometry, &c, focused));
    let composing = composing
        .filter(|(row, run)| run.col < cols && *row < rows)
        .map(|(row, run)| Painted::shape(&text, &geometry, row, &run));
    Frame {
        bounds,
        line_h: metrics.line_h,
        cells,
        marks,
        caret,
        composing,
    }
}

/// The text being composed as a run at (`col`, `row`): underlined, on the pane background so it covers the cells
/// under it.
///
/// ponytail: a non-ASCII character counts as two cells (input methods compose CJK). A narrow one (a dead-key accent)
/// gets a wide slot; it is only on screen until committed.
fn composing_run((text, (col, row)): (&str, (u16, u16))) -> (u16, CellRun) {
    let cells = text.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
    let run = CellRun {
        col,
        cells,
        text: text.to_string(),
        fg: TERM_FG,
        bg: Some(TERM_BG),
        bold: false,
        italic: false,
        underline: true,
        strike: false,
        dim: false,
        link: None,
    };
    (row, run)
}

/// What to draw in one frame.
struct Frame {
    bounds: Bounds<Pixels>,
    line_h: Pixels,
    cells: Vec<Painted>,
    /// The selected cells and the find box's matches. Painted over the cell backgrounds; the text stays as it is.
    marks: Vec<Bounds<Pixels>>,
    caret: Option<Caret>,
    /// The text being composed, drawn over the cursor.
    composing: Option<Painted>,
}

impl Frame {
    fn paint(self, window: &mut Window, cx: &mut App) {
        let mask = ContentMask {
            bounds: self.bounds,
        };
        window.with_content_mask(Some(mask), |window| {
            for cell in &self.cells {
                if let Some(bg) = cell.bg {
                    window.paint_quad(fill(cell.bounds, bg));
                }
            }
            for mark in &self.marks {
                window.paint_quad(fill(*mark, hsla(TERM_SELECTION)));
            }
            if let Some(caret) = &self.caret
                && caret.filled()
            {
                window.paint_quad(fill(caret.bounds, hsla(TERM_CURSOR)));
            }
            for cell in &self.cells {
                cell.paint(self.line_h, window, cx);
            }
            if let Some(caret) = &self.caret {
                caret.paint(self.line_h, window, cx);
            }
            if let Some(c) = &self.composing {
                window.paint_quad(fill(c.bounds, hsla(TERM_BG)));
                c.paint(self.line_h, window, cx);
            }
        });
    }
}

/// The input method of the terminal pane. Its document is only the text being composed: what was typed before belongs
/// to the session, so there is nothing else to read back or select.
impl EntityInputHandler for Workspace {
    /// The macOS Korean input method reads the composition back after every jamo. Given nothing, it commits each jamo
    /// on its own (ㅎㅏㄴ instead of 한).
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let (text, covered) = self.term.ime.text_utf16(range)?;
        *adjusted = Some(covered);
        Some(text)
    }

    /// The caret sits at the end of the composition.
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self
            .term
            .ime
            .marked()
            .map_or(0, |t| t.encode_utf16().count());
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.term.ime.marked().map(|t| 0..t.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.term.ime.unmark();
        cx.notify();
    }

    /// Committed text (a finished syllable, or a plain character) goes to the session.
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bytes = self.term.ime.commit(text);
        self.term_send(bytes, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.term.ime.mark(text);
        cx.notify();
    }

    /// The cursor cell, so the input method's candidate window opens next to it.
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (col, row) = self.ime_cell()?;
        Some(self.term.geometry?.cell_bounds(col, row, 1))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// The pane's input handler: the `EntityInputHandler` above with macOS press-and-hold turned off, so a held letter key
/// repeats as in a terminal instead of opening the accent menu (Zed's terminal does the same).
///
/// No unit test: building one needs a live `Entity<Workspace>`, and the only logic is the constant below.
struct TermInput(ElementInputHandler<Workspace>);

impl InputHandler for TermInput {
    fn selected_text_range(
        &mut self,
        ignore_disabled_input: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        self.0
            .selected_text_range(ignore_disabled_input, window, cx)
    }

    fn marked_text_range(&mut self, window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.0.marked_text_range(window, cx)
    }

    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        self.0.text_for_range(range, adjusted, window, cx)
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.0.replace_text_in_range(range, text, window, cx)
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.0
            .replace_and_mark_text_in_range(range, text, selected, window, cx)
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
        self.0.unmark_text(window, cx)
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        self.0.bounds_for_range(range, window, cx)
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<usize> {
        self.0.character_index_for_point(point, window, cx)
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

fn key_in(k: &Keystroke) -> KeyIn {
    KeyIn {
        key: k.key.clone(),
        key_char: k.key_char.clone(),
        ctrl: k.modifiers.control,
        alt: k.modifiers.alt,
        shift: k.modifiers.shift,
        cmd: k.modifiers.platform,
    }
}

/// One shaped text run and its position.
struct Painted {
    bounds: Bounds<Pixels>,
    bg: Option<Hsla>,
    line: ShapedLine,
    /// Where to place the text. A wide character is pushed to the middle of its two cells.
    text_at: Point<Pixels>,
    /// The text color, and the underline and strikethrough bars drawn in it.
    color: Hsla,
    rules: Vec<Bounds<Pixels>>,
}

impl Painted {
    fn paint(&self, line_h: Pixels, window: &mut Window, cx: &mut App) {
        let _ = self.line.paint(self.text_at, line_h, window, cx);
        for rule in &self.rules {
            window.paint_quad(fill(*rule, self.color));
        }
    }

    fn shape(text: &WindowTextSystem, geometry: &Geometry, row: u16, run: &CellRun) -> Painted {
        let mut color = hsla(run.fg);
        if run.dim {
            color = color.opacity(DIM);
        }
        let mut face = font(FONT_FAMILY);
        if run.bold {
            face.weight = FontWeight::BOLD;
        }
        if run.italic {
            face.style = FontStyle::Italic;
        }
        let style = TextRun {
            len: run.text.len(),
            font: face,
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let force = glyph_width(run, geometry.metrics.cell_w);
        let bounds = geometry.cell_bounds(run.col, row, run.cells);
        let line = text.shape_line(run.text.clone().into(), FONT_SIZE, &[style], force);
        // The fallback font of a wide character (such as Hangul) is narrower than two cells. Pinning it to the left
        // would leave only the space after it empty, so center it in the two cells. The line's width is the width the
        // font laid it out at before fitting to cells.
        let mut text_at = bounds.origin;
        let chars = run.text.chars().count();
        if usize::from(run.cells) == 2 * chars {
            let slot = geometry.metrics.cell_w * 2.;
            let natural = line.width / chars as f32;
            if natural < slot {
                text_at.x += (slot - natural) / 2.;
            }
        }
        Painted {
            bounds,
            bg: run.bg.map(hsla),
            line,
            text_at,
            color,
            rules: rules(geometry, row, run),
        }
    }
}

/// The underline and strikethrough of a run as bars across its cells. GPUI's own text decoration follows the shaped
/// glyphs, which falls short of the grid when a wide run is forced to two cells a character, so they are quads instead.
fn rules(geometry: &Geometry, row: u16, run: &CellRun) -> Vec<Bounds<Pixels>> {
    let cells = geometry.cell_bounds(run.col, row, run.cells);
    let Metrics {
        underline, strike, ..
    } = geometry.metrics;
    [(run.underline, underline), (run.strike, strike)]
        .into_iter()
        .filter(|&(on, _)| on)
        .map(|(_, y)| {
            let at = point(cells.origin.x, cells.origin.y + y);
            Bounds::new(at, size(cells.size.width, RULE))
        })
        .collect()
}

/// The width one character occupies. Each character must be placed to fit its cell (two cells for a wide character) so
/// the cells line up even when a fallback font (for Hangul etc.) is mixed in. A cell with a combining character on it
/// has a character count that doesn't match its cell count, so leave it as the font laid it out.
///
/// ponytail: a cell with one combining character on one wide character can't be told apart from two narrow
/// characters, so the combining character lands half a cell to the left. It's rare, so it stays.
fn glyph_width(run: &CellRun, cell_w: Pixels) -> Option<Pixels> {
    let chars = run.text.chars().count();
    match usize::from(run.cells) {
        cells if cells == chars => Some(cell_w),
        cells if cells == 2 * chars => Some(cell_w * 2.),
        _ => None,
    }
}

/// The cursor to draw. A block is filled when the pane has focus, and the character under it is redrawn in the background color. Without focus it is a hollow block.
struct Caret {
    bounds: Bounds<Pixels>,
    shape: CursorShape,
    focused: bool,
    glyph: Option<ShapedLine>,
}

impl Caret {
    fn shape(text: &WindowTextSystem, geometry: &Geometry, c: &CursorCell, focused: bool) -> Caret {
        let glyph =
            (focused && c.shape == CursorShape::Block && !c.text.trim().is_empty()).then(|| {
                let style = TextRun {
                    len: c.text.len(),
                    font: font(FONT_FAMILY),
                    color: hsla(TERM_BG),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                text.shape_line(c.text.clone().into(), FONT_SIZE, &[style], None)
            });
        Caret {
            bounds: geometry.cell_bounds(c.col, c.row, c.cells),
            shape: c.shape,
            focused,
            glyph,
        }
    }

    /// Is it a block painted under the text?
    fn filled(&self) -> bool {
        self.focused && self.shape == CursorShape::Block
    }

    fn paint(&self, line_h: Pixels, window: &mut Window, cx: &mut App) {
        let Bounds { origin, size: cell } = self.bounds;
        let color = hsla(TERM_CURSOR);
        match self.shape {
            CursorShape::Block if self.focused => {
                if let Some(glyph) = &self.glyph {
                    // Place it in the middle of the cell (the middle of two cells for a wide character).
                    let dx = ((cell.width - glyph.width) / 2.).max(px(0.));
                    let _ = glyph.paint(point(origin.x + dx, origin.y), line_h, window, cx);
                }
            }
            CursorShape::Block | CursorShape::HollowBlock => {
                window.paint_quad(outline(self.bounds, color, Default::default()));
            }
            CursorShape::Underline => {
                let at = point(origin.x, origin.y + cell.height - CARET);
                window.paint_quad(fill(Bounds::new(at, size(cell.width, CARET)), color));
            }
            CursorShape::Beam => {
                window.paint_quad(fill(Bounds::new(origin, size(CARET, cell.height)), color));
            }
            CursorShape::Hidden => {}
        }
    }
}

fn hsla(color: u32) -> Hsla {
    rgb(color).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> Geometry {
        Geometry {
            origin: point(px(100.), px(50.)),
            metrics: Metrics {
                cell_w: px(8.),
                line_h: px(16.),
                underline: px(13.),
                strike: px(9.),
            },
            cols: 10,
            rows: 5,
        }
    }

    #[test]
    fn cell_at_maps_window_points_to_cells() {
        let g = geometry();
        assert_eq!(g.cell_at(point(px(100.), px(50.))), Some((0, 0)));
        assert_eq!(g.cell_at(point(px(115.9), px(81.9))), Some((1, 1)));
        assert_eq!(g.cell_at(point(px(179.), px(129.))), Some((9, 4)));
        // Outside the pane (the top left margin, past the bottom right edge) there is no cell.
        assert_eq!(g.cell_at(point(px(99.), px(60.))), None);
        assert_eq!(g.cell_at(point(px(120.), px(49.))), None);
        assert_eq!(g.cell_at(point(px(180.), px(60.))), None);
        assert_eq!(g.cell_at(point(px(120.), px(130.))), None);
        assert_eq!(
            g.cell_bounds(2, 1, 2),
            Bounds::new(point(px(116.), px(66.)), size(px(16.), px(16.)))
        );
    }

    /// 한 then Enter as macOS delivers it while composing: the input method gets the Enter first, commits the syllable,
    /// then passes the key back. Each is sent once, the syllable first. The jamo typed to build it go to the input
    /// method only (the key path sends nothing for them).
    #[test]
    fn hangul_then_enter_sends_the_syllable_then_cr() {
        let modes = KeyModes::default();
        let key = |key: &str, ch: Option<&str>| KeyIn {
            key: key.into(),
            key_char: ch.map(str::to_string),
            ..KeyIn::default()
        };
        let mut ime = Ime::default();
        let mut sent = Vec::new();
        for (k, marked) in [("g", "ㅎ"), ("k", "하"), ("s", "한")] {
            sent.extend(keys::encode_key(&key(k, Some(k)), &modes));
            ime.mark(marked);
        }
        sent.push(ime.commit("한"));
        sent.extend(keys::encode_key(&key("enter", Some("\n")), &modes));
        assert_eq!(sent, ["한".as_bytes(), b"\r"]);
    }

    fn run(text: &str, cells: u16) -> CellRun {
        CellRun {
            col: 0,
            cells,
            text: text.into(),
            fg: 0,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            dim: false,
            link: None,
        }
    }

    #[test]
    fn glyphs_snap_to_cells_unless_combining() {
        let w = px(8.);
        assert_eq!(glyph_width(&run("ab", 2), w), Some(px(8.)));
        assert_eq!(glyph_width(&run("한글", 4), w), Some(px(16.)));
        assert_eq!(glyph_width(&run("e\u{301}", 1), w), None);
    }

    #[test]
    fn rules_span_the_run_cells_even_for_wide_chars() {
        let g = geometry();
        // "한글" from column 1 of row 2: four cells, so both bars are 32px wide from x = 108.
        let wide = CellRun {
            col: 1,
            underline: true,
            strike: true,
            ..run("한글", 4)
        };
        let top = px(50.) + px(16.) * 2.;
        assert_eq!(
            rules(&g, 2, &wide),
            [
                Bounds::new(point(px(108.), top + px(13.)), size(px(32.), RULE)),
                Bounds::new(point(px(108.), top + px(9.)), size(px(32.), RULE)),
            ]
        );
        assert!(rules(&g, 0, &run("ab", 2)).is_empty());
    }
}
