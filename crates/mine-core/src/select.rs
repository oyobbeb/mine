//! The pure part of terminal pane mouse handling that doesn't depend on the kind of screen: mouse events, rebuilding the SGR to pass to the session, and reading the text of a selection.
//! All coordinates are (column, row) inside the terminal pane, 0-based.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;

use crate::screen::Screen;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Down,
    Up,
    Drag,
    WheelUp,
    WheelDown,
}

/// A mouse event. Coordinates are 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEv {
    pub kind: MouseKind,
    pub button: u8,
    pub col: u16,
    pub row: u16,
}

/// A selection over the screen that is visible now. Includes both end cells, and either end may come first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub start: (u16, u16),
    pub end: (u16, u16),
}

impl Selection {
    /// The upper end (the left one if on the same row) and the lower end.
    pub fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let key = |&(col, row): &(u16, u16)| (row, col);
        if key(&self.start) <= key(&self.end) {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }
}

/// The text of a selection. If the screen is scrolled, reads the rows visible at that moment.
/// Trailing whitespace on a row is trimmed and rows are joined with `\n`, but no `\n` is inserted between rows joined by auto-wrap.
pub fn selected_text(screen: &Screen, sel: &Selection) -> String {
    let grid = screen.term().grid();
    let (cols, rows) = (grid.columns(), grid.screen_lines());
    let offset = grid.display_offset() as i32;
    let ((c0, r0), (c1, r1)) = sel.ordered();
    let (top, bottom) = (usize::from(r0), usize::from(r1).min(rows - 1));
    let mut out = String::new();
    for y in top..=bottom {
        let row = &grid[Line(y as i32 - offset)];
        let mut from = if y == top { usize::from(c0) } else { 0 }.min(cols - 1);
        let to = if y == bottom { usize::from(c1) } else { cols }.min(cols - 1);
        // Starting on the trailing cell of a wide char still includes that char.
        if from > 0 && row[Column(from)].flags.contains(Flags::WIDE_CHAR_SPACER) {
            from -= 1;
        }
        let mut line = String::new();
        for x in from..=to {
            let cell = &row[Column(x)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            line.push(if cell.c.is_control() { ' ' } else { cell.c });
            line.extend(cell.zerowidth().unwrap_or_default());
        }
        // A row that continues onto the next row keeps its trailing whitespace as text.
        let wraps = y < bottom && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        if !wraps {
            line.truncate(line.trim_end_matches(' ').len());
        }
        out.push_str(&line);
        if y < bottom && !wraps {
            out.push('\n');
        }
    }
    out
}

/// Whether the session turned on mouse reporting (any of press, drag, or motion).
pub fn wants_mouse(screen: &Screen) -> bool {
    screen.mode().intersects(TermMode::MOUSE_MODE)
}

/// Whether the session asked to receive this event. Drag is only for sessions that turned on drag (1002) or motion (1003).
/// 1002 stays on in the outer terminal, so drag has to be filtered out for sessions that turned on only press (1000).
pub fn session_takes(screen: &Screen, ev: &MouseEv) -> bool {
    ev.kind != MouseKind::Drag
        || screen
            .mode()
            .intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
}

/// Rebuilds an SGR mouse report with coordinates relative to the terminal pane (`col`, `row`, 0-based).
/// Press, drag and wheel are `M`, release is `m`, drag adds 32 to the button number. Wheel is 64 (up) and 65 (down).
pub fn encode_sgr(ev: &MouseEv, col: u16, row: u16) -> Vec<u8> {
    let (code, end) = match ev.kind {
        MouseKind::Down => (ev.button, 'M'),
        MouseKind::Up => (ev.button, 'm'),
        MouseKind::Drag => (ev.button + 32, 'M'),
        MouseKind::WheelUp => (64, 'M'),
        MouseKind::WheelDown => (65, 'M'),
    };
    format!("\x1b[<{code};{};{}{end}", col + 1, row + 1).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(cols: u16, rows: u16, bytes: &[u8]) -> Screen {
        let mut sc = Screen::new(cols, rows);
        sc.feed(bytes);
        sc
    }

    fn sel(start: (u16, u16), end: (u16, u16)) -> Selection {
        Selection { start, end }
    }

    fn ev(kind: MouseKind, button: u8) -> MouseEv {
        MouseEv {
            kind,
            button,
            col: 40,
            row: 5,
        }
    }

    #[test]
    fn selection_spans_lines_and_trims() {
        let sc = screen(20, 3, b"hello   \r\nworld");
        assert_eq!(selected_text(&sc, &sel((0, 0), (4, 1))), "hello\nworld");
    }

    #[test]
    fn selection_counts_wide_char_once() {
        let sc = screen(20, 3, "한글 ab".as_bytes());
        assert_eq!(selected_text(&sc, &sel((0, 0), (3, 0))), "한글");
        // Starting or ending on the trailing cell of a wide char still includes that char exactly once.
        assert_eq!(selected_text(&sc, &sel((1, 0), (2, 0))), "한글");
        assert_eq!(selected_text(&sc, &sel((0, 0), (6, 0))), "한글 ab");
    }

    #[test]
    fn mouse_forwarded_with_local_coords() {
        let down = ev(MouseKind::Down, 0);
        assert_eq!(encode_sgr(&down, 6, 5), b"\x1b[<0;7;6M".to_vec());
        assert_eq!(
            encode_sgr(&ev(MouseKind::Up, 0), 6, 5),
            b"\x1b[<0;7;6m".to_vec()
        );
        assert_eq!(
            encode_sgr(&ev(MouseKind::Drag, 0), 0, 0),
            b"\x1b[<32;1;1M".to_vec()
        );
        assert_eq!(
            encode_sgr(&ev(MouseKind::Down, 2), 1, 1),
            b"\x1b[<2;2;2M".to_vec()
        );
        assert_eq!(
            encode_sgr(&ev(MouseKind::WheelUp, 0), 3, 4),
            b"\x1b[<64;4;5M".to_vec()
        );
        assert_eq!(
            encode_sgr(&ev(MouseKind::WheelDown, 0), 3, 4),
            b"\x1b[<65;4;5M".to_vec()
        );
    }

    #[test]
    fn wants_mouse_follows_mode() {
        let mut sc = Screen::new(20, 3);
        assert!(!wants_mouse(&sc));
        sc.feed(b"\x1b[?1000h");
        assert!(wants_mouse(&sc));
        sc.feed(b"\x1b[?1000l");
        assert!(!wants_mouse(&sc));
        sc.feed(b"\x1b[?1002h");
        assert!(wants_mouse(&sc));
        sc.feed(b"\x1b[?1002l\x1b[?1003h");
        assert!(wants_mouse(&sc));
        // The SGR format (1006) alone, without it being turned on, doesn't use the mouse.
        sc.feed(b"\x1b[?1003l\x1b[?1006h");
        assert!(!wants_mouse(&sc));
    }

    #[test]
    fn drag_events_go_only_to_sessions_that_asked_for_them() {
        let mut sc = Screen::new(20, 3);
        sc.feed(b"\x1b[?1000h");
        assert!(session_takes(&sc, &ev(MouseKind::Down, 0)));
        assert!(session_takes(&sc, &ev(MouseKind::Up, 0)));
        assert!(session_takes(&sc, &ev(MouseKind::WheelUp, 0)));
        assert!(!session_takes(&sc, &ev(MouseKind::Drag, 0)));
        sc.feed(b"\x1b[?1002h");
        assert!(session_takes(&sc, &ev(MouseKind::Drag, 0)));
    }

    #[test]
    fn backward_drag_selects_the_same_text() {
        let sc = screen(20, 3, b"hello   \r\nworld");
        let forward = selected_text(&sc, &sel((1, 0), (2, 1)));
        assert_eq!(forward, "ello\nwor");
        assert_eq!(selected_text(&sc, &sel((2, 1), (1, 0))), forward);
        // Drag left on the same row.
        assert_eq!(selected_text(&sc, &sel((4, 0), (0, 0))), "hello");
        // Drag to the right cell of the row above.
        assert_eq!(selected_text(&sc, &sel((0, 1), (3, 0))), "lo\nw");
    }

    #[test]
    fn selection_reads_the_scrolled_view() {
        let mut sc = Screen::new(20, 3);
        for i in 0..10 {
            sc.feed(format!("line{i}\r\n").as_bytes());
        }
        // At the bottom, line8, line9 and a blank row are visible.
        assert_eq!(selected_text(&sc, &sel((0, 0), (4, 1))), "line8\nline9");
        sc.scroll(4);
        assert_eq!(
            selected_text(&sc, &sel((0, 0), (4, 2))),
            "line4\nline5\nline6"
        );
        // It reads the visible rows, so it differs for each scroll position.
        sc.scroll(-3);
        assert_eq!(selected_text(&sc, &sel((0, 0), (4, 0))), "line7");
    }

    #[test]
    fn soft_wrapped_line_joins_without_newline() {
        let sc = screen(5, 3, b"abcdefgh\r\nxy");
        assert_eq!(selected_text(&sc, &sel((0, 0), (4, 2))), "abcdefgh\nxy");
        // Whitespace at a line break is text, so it isn't trimmed.
        let sc = screen(5, 3, b"abcd efgh");
        assert_eq!(selected_text(&sc, &sel((0, 0), (3, 1))), "abcd efgh");
        // If the selection ends in the middle of a joined row, it is cut at that end.
        assert_eq!(selected_text(&sc, &sel((0, 0), (4, 0))), "abcd");
    }

    #[test]
    fn selection_is_clamped_to_the_screen() {
        let sc = screen(10, 2, b"abc\r\ndef");
        assert_eq!(selected_text(&sc, &sel((0, 0), (99, 99))), "abc\ndef");
    }
}
