//! The pure part of the terminal pane's mouse handling: a position to a cell, drag selection, and which presses, drags,
//! releases and wheel turns go to the session as SGR reports (`mine_core::select`). The GPUI wiring is in
//! `terminal/mod.rs`. The state lives in `Model::mouse`, so the model can drop the selection when the cells under it
//! change (scrolling, resizing, another session).

use mine_core::screen::Screen;
use mine_core::select::{
    MouseEv, MouseKind, Selection, encode_sgr, selected_text, session_takes, wants_mouse,
};

/// The cell (column, row) under (`x`, `y`), in pixels from the top left of cell (0, 0). Clamped into the pane, so a drag
/// or release outside the pane lands on the nearest edge cell.
pub fn cell_at(x: f32, y: f32, cell_w: f32, line_h: f32, cols: u16, rows: u16) -> (u16, u16) {
    // `as` turns NaN and negatives into 0, and saturates the infinity a zero cell size gives.
    let fit = |at: f32, cell: f32, n: u16| ((at / cell) as u16).min(n.saturating_sub(1));
    (fit(x, cell_w, cols), fit(y, line_h, rows))
}

/// What the pane does after a mouse event.
#[derive(Debug, PartialEq)]
pub enum Act {
    None,
    /// The selection changed: redraw.
    Redraw,
    /// SGR mouse reports for the session.
    Send(Vec<u8>),
    /// A left click on this cell with no drag: opens the link there, if any.
    Click((u16, u16)),
    /// Scrolls the local copy of the screen by this many lines (positive is toward the past).
    Scroll(i32),
}

/// Mouse state of the terminal pane.
#[derive(Debug, Default)]
pub struct Mouse {
    /// Where the left button went down for a local selection, and whether the drag has left that cell since. Held
    /// until the release.
    anchor: Option<((u16, u16), bool)>,
    /// The selection shown. It is screen cells, so it stays through new output and goes on a press, a scroll, or a
    /// resize.
    sel: Option<Selection>,
    /// The button of a press sent to the session. Its drags and its release go to the session too, even from outside
    /// the pane (R26), so the session never sees a button stuck down.
    held: Option<u8>,
}

impl Mouse {
    /// A press, drag or release over cell (`ev.col`, `ev.row`), already clamped into the pane. While the session has
    /// mouse reporting on and Shift is not held, a press goes to the session, and so do its drags and release.
    /// Otherwise the left button selects.
    pub fn event(&mut self, screen: Option<&Screen>, ev: MouseEv, shift: bool) -> Act {
        let Some(screen) = screen else {
            return Act::None;
        };
        let at = (ev.col, ev.row);
        match ev.kind {
            MouseKind::Down => {
                // A click clears the selection, whoever gets the press.
                let cleared = self.clear();
                if wants_mouse(screen) && !shift {
                    self.held = Some(ev.button);
                    return Act::Send(encode_sgr(&ev, ev.col, ev.row));
                }
                if ev.button == 0 {
                    self.anchor = Some((at, false));
                }
                if cleared { Act::Redraw } else { Act::None }
            }
            MouseKind::Drag => {
                if let Some(button) = self.held {
                    return self.report_held(screen, MouseEv { button, ..ev });
                }
                let Some((start, moved)) = &mut self.anchor else {
                    return Act::None;
                };
                *moved |= *start != at;
                let sel = (*start != at).then_some(Selection {
                    start: *start,
                    end: at,
                });
                if sel == self.sel {
                    return Act::None;
                }
                self.sel = sel;
                Act::Redraw
            }
            MouseKind::Up => {
                if self.held == Some(ev.button) {
                    return self.report_held(screen, ev);
                }
                let Some((start, moved)) = self.anchor.take().filter(|_| ev.button == 0) else {
                    return Act::None;
                };
                self.sel = (start != at).then_some(Selection { start, end: at });
                match self.sel {
                    Some(_) => Act::Redraw,
                    None if !moved => Act::Click(at),
                    None => Act::None,
                }
            }
            MouseKind::WheelUp | MouseKind::WheelDown => Act::None,
        }
    }

    /// A drag or the release of the press the session holds. If the session turned reporting off meanwhile, the
    /// press is over and nothing goes out.
    fn report_held(&mut self, screen: &Screen, ev: MouseEv) -> Act {
        let on = wants_mouse(screen);
        if ev.kind == MouseKind::Up || !on {
            self.held = None;
        }
        if on && session_takes(screen, &ev) {
            Act::Send(encode_sgr(&ev, ev.col, ev.row))
        } else {
            Act::None
        }
    }

    /// `lines` lines of wheel movement over cell `at` (positive is toward the past). The session gets one report per
    /// line while it has mouse reporting on and Shift is not held; otherwise the local copy scrolls.
    pub fn wheel(&self, screen: Option<&Screen>, lines: i32, at: (u16, u16), shift: bool) -> Act {
        let Some(screen) = screen.filter(|_| lines != 0) else {
            return Act::None;
        };
        if shift || !wants_mouse(screen) {
            return Act::Scroll(lines);
        }
        let kind = if lines > 0 {
            MouseKind::WheelUp
        } else {
            MouseKind::WheelDown
        };
        let ev = MouseEv {
            kind,
            button: 0,
            col: at.0,
            row: at.1,
        };
        Act::Send(encode_sgr(&ev, at.0, at.1).repeat(lines.unsigned_abs() as usize))
    }

    pub fn selection(&self) -> Option<Selection> {
        self.sel
    }

    /// The selected text for ⌘C. `None` if nothing (or only blanks) is selected.
    pub fn copy(&self, screen: Option<&Screen>) -> Option<String> {
        Some(selected_text(screen?, self.sel.as_ref()?)).filter(|text| !text.is_empty())
    }

    /// Drops the selection and a selection drag in progress. Returns whether a selection was shown. A press held by
    /// the session is kept, so its release still goes out.
    pub fn clear(&mut self) -> bool {
        self.anchor = None;
        self.sel.take().is_some()
    }
}

/// The cells a selection covers on a `cols` × `rows` pane, as (row, first column, cell count) per row.
pub fn spans(sel: &Selection, cols: u16, rows: u16) -> Vec<(u16, u16, u16)> {
    let ((c0, r0), (c1, r1)) = sel.ordered();
    let last = cols.saturating_sub(1);
    (r0..=r1.min(rows.saturating_sub(1)))
        .map(|row| {
            let from = if row == r0 { c0.min(last) } else { 0 };
            let to = if row == r1 { c1.min(last) } else { last };
            (row, from, to - from + 1)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;
    use crate::model::tests::{attached_t1, output, snapshot};

    /// A model attached to `k-t1` (80 × 24) whose screen has been fed `bytes`.
    fn model_with(bytes: &[u8]) -> Model {
        let mut m = attached_t1();
        m.on_link(snapshot("k-t1", Screen::new(80, 24).snapshot()));
        m.on_link(output("k-t1", bytes));
        m
    }

    fn ev(kind: MouseKind, (col, row): (u16, u16)) -> MouseEv {
        MouseEv {
            kind,
            button: 0,
            col,
            row,
        }
    }

    fn mouse(m: &mut Model, kind: MouseKind, at: (u16, u16), shift: bool) -> Act {
        m.mouse.event(m.screen.as_ref(), ev(kind, at), shift)
    }

    /// Presses at `from`, drags to `to` and releases there.
    fn drag(m: &mut Model, from: (u16, u16), to: (u16, u16), shift: bool) -> Vec<Act> {
        vec![
            mouse(m, MouseKind::Down, from, shift),
            mouse(m, MouseKind::Drag, to, shift),
            mouse(m, MouseKind::Up, to, shift),
        ]
    }

    #[test]
    fn cell_at_clamps() {
        assert_eq!(cell_at(0.0, 0.0, 8.0, 16.0, 10, 5), (0, 0));
        assert_eq!(cell_at(15.9, 31.9, 8.0, 16.0, 10, 5), (1, 1));
        // Left of or above the pane is the first cell; right of or below it is the last.
        assert_eq!(cell_at(-30.0, -1.0, 8.0, 16.0, 10, 5), (0, 0));
        assert_eq!(cell_at(500.0, 900.0, 8.0, 16.0, 10, 5), (9, 4));
        assert_eq!(cell_at(-3.0, 900.0, 8.0, 16.0, 10, 5), (0, 4));
        // An unknown cell size or a NaN position still gives a cell inside the pane.
        assert_eq!(cell_at(40.0, f32::NAN, 0.0, 16.0, 10, 5), (9, 0));
    }

    #[test]
    fn shift_drag_selects_even_when_session_wants_mouse() {
        let mut m = model_with(b"hello world\x1b[?1002h\x1b[?1006h");
        let acts = drag(&mut m, (0, 0), (4, 0), true);
        // Nothing goes to the session: the press, drag and release make a selection instead.
        assert!(!acts.iter().any(|a| matches!(a, Act::Send(_))), "{acts:?}");
        let sel = m.mouse.selection().expect("no selection");
        assert_eq!(selected_text(m.screen.as_ref().unwrap(), &sel), "hello");
        // The same drag without Shift goes to the session and selects nothing.
        let acts = drag(&mut m, (0, 0), (4, 0), false);
        assert!(acts.iter().all(|a| matches!(a, Act::Send(_))), "{acts:?}");
        assert_eq!(m.mouse.selection(), None);
    }

    #[test]
    fn mouse_forwarded_when_session_wants() {
        // Press only (1000): press and release go out, a drag doesn't. Coordinates are 1-based.
        let mut m = model_with(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            drag(&mut m, (2, 1), (5, 3), false),
            [
                Act::Send(b"\x1b[<0;3;2M".to_vec()),
                Act::None,
                Act::Send(b"\x1b[<0;6;4m".to_vec()),
            ]
        );
        // Drag too (1002), with the right button.
        m.on_link(output("k-t1", b"\x1b[?1002h"));
        let right = |kind, at: (u16, u16)| MouseEv {
            button: 2,
            ..ev(kind, at)
        };
        let screen = m.screen.as_ref();
        assert_eq!(
            m.mouse.event(screen, right(MouseKind::Down, (0, 0)), false),
            Act::Send(b"\x1b[<2;1;1M".to_vec())
        );
        assert_eq!(
            m.mouse.event(screen, right(MouseKind::Drag, (1, 0)), false),
            Act::Send(b"\x1b[<34;2;1M".to_vec())
        );
        assert_eq!(
            m.mouse.event(screen, right(MouseKind::Up, (1, 0)), false),
            Act::Send(b"\x1b[<2;2;1m".to_vec())
        );
        // The wheel: one report per line. Shift scrolls the local copy instead.
        assert_eq!(
            m.mouse.wheel(screen, 2, (3, 4), false),
            Act::Send(b"\x1b[<64;4;5M\x1b[<64;4;5M".to_vec())
        );
        assert_eq!(
            m.mouse.wheel(screen, -1, (3, 4), false),
            Act::Send(b"\x1b[<65;4;5M".to_vec())
        );
        assert_eq!(m.mouse.wheel(screen, 2, (3, 4), true), Act::Scroll(2));
        // Without mouse reporting the wheel scrolls and the left button selects.
        let mut m = model_with(b"abc");
        assert_eq!(
            m.mouse.wheel(m.screen.as_ref(), -3, (0, 0), false),
            Act::Scroll(-3)
        );
        assert_eq!(drag(&mut m, (0, 0), (1, 0), false)[2], Act::Redraw);
    }

    #[test]
    fn release_outside_the_pane_ends_the_press() {
        // A press sent to the session gets its release even when the button comes up outside the pane: the release
        // lands on the clamped edge cell. After that, moves don't go out as drags.
        let mut m = model_with(b"\x1b[?1002h\x1b[?1006h");
        mouse(&mut m, MouseKind::Down, (5, 5), false);
        let edge = cell_at(-40.0, 3000.0, 8.0, 16.0, 80, 24);
        assert_eq!(
            mouse(&mut m, MouseKind::Up, edge, false),
            Act::Send(b"\x1b[<0;1;24m".to_vec())
        );
        assert_eq!(mouse(&mut m, MouseKind::Drag, (6, 6), false), Act::None);
        // A selection drag released outside the pane ends at the edge and keeps its selection.
        let mut m = model_with(b"abc");
        mouse(&mut m, MouseKind::Down, (1, 0), false);
        mouse(&mut m, MouseKind::Drag, (2, 0), false);
        assert_eq!(mouse(&mut m, MouseKind::Up, (79, 0), false), Act::Redraw);
        assert_eq!(m.mouse.copy(m.screen.as_ref()).as_deref(), Some("bc"));
        assert_eq!(mouse(&mut m, MouseKind::Drag, (0, 3), false), Act::None);
        // If the session turns reporting off while its press is held, the press is over and nothing goes out.
        let mut m = model_with(b"\x1b[?1002h\x1b[?1006h");
        mouse(&mut m, MouseKind::Down, (0, 0), false);
        m.on_link(output("k-t1", b"\x1b[?1002l"));
        assert_eq!(mouse(&mut m, MouseKind::Up, (0, 0), false), Act::None);
    }

    #[test]
    fn copy_uses_selected_text() {
        // An auto-wrapped line (80 columns) and a short one.
        let long: String = (0..100)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let mut m = model_with(format!("{long}\r\nend").as_bytes());
        drag(&mut m, (0, 0), (2, 2), false);
        let sel = m.mouse.selection().expect("no selection");
        let text = m.mouse.copy(m.screen.as_ref()).expect("nothing to copy");
        assert_eq!(text, selected_text(m.screen.as_ref().unwrap(), &sel));
        // The wrapped rows join without a line break.
        assert_eq!(text, format!("{long}\nend"));
        // A click selects nothing, so there is nothing to copy.
        assert_eq!(mouse(&mut m, MouseKind::Down, (0, 0), false), Act::Redraw);
        assert_eq!(
            mouse(&mut m, MouseKind::Up, (0, 0), false),
            Act::Click((0, 0))
        );
        assert_eq!(m.mouse.copy(m.screen.as_ref()), None);
    }

    #[test]
    fn click_clears_the_selection_and_dragging_away_is_not_a_click() {
        let mut m = model_with(b"abc");
        drag(&mut m, (0, 0), (2, 0), false);
        assert!(m.mouse.selection().is_some());
        // A press clears it (and redraws), even one that goes to the session.
        m.on_link(output("k-t1", b"\x1b[?1000h\x1b[?1006h"));
        assert!(matches!(
            mouse(&mut m, MouseKind::Down, (1, 0), false),
            Act::Send(_)
        ));
        assert_eq!(m.mouse.selection(), None);
        // Dragging off the pressed cell and back is neither a selection nor a click (a link isn't opened).
        let mut m = model_with(b"abc");
        mouse(&mut m, MouseKind::Down, (1, 0), false);
        assert_eq!(mouse(&mut m, MouseKind::Drag, (2, 0), false), Act::Redraw);
        assert_eq!(mouse(&mut m, MouseKind::Drag, (1, 0), false), Act::Redraw);
        assert_eq!(mouse(&mut m, MouseKind::Up, (1, 0), false), Act::None);
        assert_eq!(m.mouse.selection(), None);
    }

    #[test]
    fn output_keeps_the_selection_and_scrolling_clears_it() {
        let many: String = (1..=40).map(|i| format!("{i}\r\n")).collect();
        let mut m = model_with(many.as_bytes());
        let select = |m: &mut Model| {
            drag(m, (0, 0), (1, 1), false);
            assert!(m.mouse.selection().is_some());
        };
        select(&mut m);
        m.on_link(output("k-t1", b"more\r\n"));
        assert!(m.mouse.selection().is_some(), "new output cleared it");
        // Scrolling the local copy clears it.
        assert!(m.scroll(3));
        assert_eq!(m.mouse.selection(), None);
        // So does input that brings a scrolled-back view to the bottom, but not input at the bottom.
        select(&mut m);
        m.input(b"x".to_vec());
        assert_eq!(m.mouse.selection(), None);
        select(&mut m);
        m.input(b"y".to_vec());
        assert!(m.mouse.selection().is_some());
        // A resize and a new snapshot change the cells under it.
        m.set_grid(70, 20);
        assert_eq!(m.mouse.selection(), None);
        select(&mut m);
        m.on_link(snapshot("k-t1", Screen::new(70, 20).snapshot()));
        assert_eq!(m.mouse.selection(), None);
    }

    #[test]
    fn spans_cover_the_selected_cells_per_row() {
        let sel = Selection {
            start: (6, 2),
            end: (3, 0),
        };
        assert_eq!(spans(&sel, 10, 5), [(0, 3, 7), (1, 0, 10), (2, 0, 7)]);
        let sel = Selection {
            start: (2, 1),
            end: (4, 1),
        };
        assert_eq!(spans(&sel, 10, 5), [(1, 2, 3)]);
        // Rows and columns past the pane are cut off.
        let sel = Selection {
            start: (0, 3),
            end: (99, 99),
        };
        assert_eq!(spans(&sel, 10, 5), [(3, 0, 10), (4, 0, 10)]);
    }
}
