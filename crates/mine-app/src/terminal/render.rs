//! Material for drawing the terminal pane. Without GPUI, turns the cells of the client `Screen` into text runs, and
//! turns the pane size and wheel scrolling into cell units.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use mine_core::screen::Screen;

use crate::theme::{ANSI, TERM_BG, TERM_CURSOR, TERM_FG, indexed};

/// Cells that continue in the same style along one row. Occupies `cells` columns starting at `col`. Colors are `0xRRGGBB`.
///
/// A wide character (2 cells) and a narrow character are never mixed in one run, and a cell with a combining character
/// on it forms a run by itself. So the number of characters in a run tells the width of one character (the drawing
/// side places each character to fit its cell).
#[derive(Clone, Debug, PartialEq)]
pub struct CellRun {
    pub col: u16,
    pub cells: u16,
    pub text: String,
    pub fg: u32,
    /// `None` for the default background (nothing is painted).
    pub bg: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub dim: bool,
    /// OSC 8 link address.
    pub link: Option<String>,
}

impl CellRun {
    /// A run made of one `cell` (`cells` columns from `col`).
    fn of(cell: &Cell, col: usize, cells: u16) -> CellRun {
        let flags = cell.flags;
        let mut fg = color(cell.fg);
        let mut bg = (cell.bg != Color::Named(NamedColor::Background)).then(|| color(cell.bg));
        if flags.contains(Flags::INVERSE) {
            let back = bg.unwrap_or(TERM_BG);
            bg = Some(fg);
            fg = back;
        }
        if flags.contains(Flags::HIDDEN) {
            fg = bg.unwrap_or(TERM_BG);
        }
        CellRun {
            col: col as u16,
            cells,
            text: symbol(cell),
            fg,
            bg,
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.intersects(Flags::ALL_UNDERLINES),
            strike: flags.contains(Flags::STRIKEOUT),
            dim: flags.contains(Flags::DIM),
            link: cell.hyperlink().map(|link| link.uri().to_string()),
        }
    }

    /// Whether the style (color, attributes, link) is the same, ignoring the text and position.
    fn same_look(&self, other: &CellRun) -> bool {
        (
            self.fg,
            self.bg,
            self.bold,
            self.italic,
            self.underline,
            self.strike,
            self.dim,
            &self.link,
        ) == (
            other.fg,
            other.bg,
            other.bold,
            other.italic,
            other.underline,
            other.strike,
            other.dim,
            &other.link,
        )
    }
}

/// The text runs of screen `row` (0-based, following the scrolled position). Blanks at the end of the line are dropped.
pub fn row_runs(screen: &Screen, row: usize) -> Vec<CellRun> {
    let grid = screen.term().grid();
    if row >= grid.screen_lines() {
        return Vec::new();
    }
    let line = &grid[Line(row as i32 - grid.display_offset() as i32)];
    let end = (0..grid.columns())
        .rev()
        .find(|&x| !blank(&line[Column(x)]))
        .map_or(0, |x| x + 1);
    let mut runs: Vec<CellRun> = Vec::new();
    // Can the next cell be appended to the last run? It must have no combining character and the same character width (1 or 2 cells).
    let mut open: Option<u16> = None;
    for x in 0..end {
        let cell = &line[Column(x)];
        // The trailing cell of a wide character is covered by the preceding character.
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        let run = CellRun::of(cell, x, width);
        let plain = !combining(cell);
        match runs.last_mut() {
            Some(last) if plain && open == Some(width) && last.same_look(&run) => {
                last.text.push_str(&run.text);
                last.cells += width;
            }
            _ => runs.push(run),
        }
        open = plain.then_some(width);
    }
    runs
}

/// A cell that doesn't need to be drawn: a space on the default background with no underline, strikethrough, inverse or link.
fn blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell
            .flags
            .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
        && cell.hyperlink().is_none()
        && !combining(cell)
}

/// The cell has a combining character on it. A cell with a link also has the extra part (`extra`), so `zerowidth()` returns an empty slice.
fn combining(cell: &Cell) -> bool {
    cell.zerowidth().is_some_and(|chars| !chars.is_empty())
}

/// The cell's character and the combining characters on it. A control-character cell such as a tab is a space.
fn symbol(cell: &Cell) -> String {
    let mut text = String::from(if cell.c.is_control() { ' ' } else { cell.c });
    text.extend(cell.zerowidth().unwrap_or_default());
    text
}

/// An alacritty color as `0xRRGGBB`. 0–15 are the mockup palette, 16–255 are the xterm cube and gray ramp, and the
/// default text, background and cursor are the terminal colors. Colors the session changes with OSC 4/10/11 are not
/// followed (a snapshot doesn't restore them, so they would differ after reattaching).
fn color(color: Color) -> u32 {
    use NamedColor::*;
    let i = match color {
        Color::Spec(Rgb { r, g, b }) => {
            return u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b);
        }
        Color::Indexed(i) => return indexed(i),
        Color::Named(Foreground | BrightForeground | DimForeground) => return TERM_FG,
        Color::Named(Background) => return TERM_BG,
        Color::Named(Cursor) => return TERM_CURSOR,
        Color::Named(Black | DimBlack) => 0,
        Color::Named(Red | DimRed) => 1,
        Color::Named(Green | DimGreen) => 2,
        Color::Named(Yellow | DimYellow) => 3,
        Color::Named(Blue | DimBlue) => 4,
        Color::Named(Magenta | DimMagenta) => 5,
        Color::Named(Cyan | DimCyan) => 6,
        Color::Named(White | DimWhite) => 7,
        Color::Named(BrightBlack) => 8,
        Color::Named(BrightRed) => 9,
        Color::Named(BrightGreen) => 10,
        Color::Named(BrightYellow) => 11,
        Color::Named(BrightBlue) => 12,
        Color::Named(BrightMagenta) => 13,
        Color::Named(BrightCyan) => 14,
        Color::Named(BrightWhite) => 15,
    };
    ANSI[i]
}

/// The (columns, rows) that fit in a pane size (pixels). At least `(1, 1)`.
pub fn grid_size(pane_w: f32, pane_h: f32, cell_w: f32, line_h: f32) -> (u16, u16) {
    let fit = |len: f32, cell: f32| {
        if cell > 0.0 {
            // Add a little so an exact multiple of the cell width doesn't come up one cell short from floating-point
            // error. `as` turns NaN and negatives into 0 and saturates values that are too large to `u16::MAX`.
            ((len / cell + 1e-3).floor() as u16).max(1)
        } else {
            1
        }
    };
    (fit(pane_w, cell_w), fit(pane_h, line_h))
}

/// Converts wheel/trackpad pixel movement into a line count. The remainder that doesn't make a full line is gathered
/// in `acc` and added to the next movement. Positive is toward the past (same as `Screen::scroll`).
pub fn scroll_lines(acc: &mut f32, delta_px: f32, line_h: f32) -> i32 {
    if line_h <= 0.0 || !delta_px.is_finite() {
        return 0;
    }
    *acc += delta_px;
    let lines = (*acc / line_h).trunc();
    *acc -= lines * line_h;
    lines as i32
}

/// The cursor to draw. On a wide character it is 2 cells, and `text` is that cell's character (rewritten in the background color on top of a block cursor).
#[derive(Clone, Debug, PartialEq)]
pub struct CursorCell {
    pub col: u16,
    pub row: u16,
    pub cells: u16,
    pub shape: CursorShape,
    pub text: String,
}

/// The session cursor. None while the session has hidden it or while scrolled back into the past. The shape is the one
/// the session chose (DECSCUSR), or a block if it chose none.
pub fn cursor(screen: &Screen) -> Option<CursorCell> {
    let term = screen.term();
    let grid = term.grid();
    let shape = term.cursor_style().shape;
    if grid.display_offset() != 0
        || !term.mode().contains(TermMode::SHOW_CURSOR)
        || shape == CursorShape::Hidden
    {
        return None;
    }
    let mut point = grid.cursor.point;
    if point.column.0 > 0 && grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) {
        point.column -= 1;
    }
    let cell = &grid[point];
    Some(CursorCell {
        col: point.column.0 as u16,
        row: point.line.0 as u16,
        cells: if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        },
        shape,
        text: symbol(cell),
    })
}

/// The OSC 8 link address of the visible (`col`, `row`) cell.
pub fn link_at(screen: &Screen, col: u16, row: u16) -> Option<String> {
    let grid = screen.term().grid();
    let (col, row) = (usize::from(col), usize::from(row));
    if col >= grid.columns() || row >= grid.screen_lines() {
        return None;
    }
    let mut point = Point::new(Line(row as i32 - grid.display_offset() as i32), Column(col));
    // Pressing the trailing cell of a wide character means that character.
    if col > 0 && grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) {
        point.column -= 1;
    }
    grid[point].hyperlink().map(|link| link.uri().to_string())
}

/// May the pressed link be opened? Only web, mail and file addresses are opened. The address isn't shown beforehand, so
/// this keeps session output from launching anything through an address of another app (`x-…://`) that it planted.
pub fn openable(url: &str) -> bool {
    let scheme = url
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase());
    matches!(
        scheme.as_deref(),
        Some("http" | "https" | "mailto" | "file")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{TERM_BG, TERM_FG};

    fn screen(cols: u16, rows: u16, bytes: &[u8]) -> Screen {
        let mut s = Screen::new(cols, rows);
        s.feed(bytes);
        s
    }

    fn texts(runs: &[CellRun]) -> Vec<(u16, u16, &str)> {
        runs.iter()
            .map(|r| (r.col, r.cells, r.text.as_str()))
            .collect()
    }

    #[test]
    fn row_runs_merges_same_style() {
        let runs = row_runs(&screen(20, 3, b"ab\x1b[31mcd"), 0);
        // Blanks at the end of a line don't form a run.
        assert_eq!(texts(&runs), [(0, 2, "ab"), (2, 2, "cd")]);
        assert_eq!((runs[0].fg, runs[0].bg), (TERM_FG, None));
        assert_eq!(runs[1].fg, 0xe06c75);
        // A blank in the middle is merged into the run if the style is the same.
        let runs = row_runs(&screen(20, 3, b"a  b"), 0);
        assert_eq!(texts(&runs), [(0, 4, "a  b")]);
        // An empty line has no runs.
        assert!(row_runs(&screen(20, 3, b"a"), 1).is_empty());
    }

    #[test]
    fn row_runs_wide_char_counts_two_cells() {
        let runs = row_runs(&screen(20, 3, "한a".as_bytes()), 0);
        assert_eq!(texts(&runs), [(0, 2, "한"), (2, 1, "a")]);
        // Wide characters are merged with each other. They are not mixed with narrow ones.
        let runs = row_runs(&screen(20, 3, "x한글 y".as_bytes()), 0);
        assert_eq!(texts(&runs), [(0, 1, "x"), (1, 4, "한글"), (5, 2, " y")]);
    }

    #[test]
    fn row_runs_keep_combining_cells_apart() {
        // A cell with a combining character forms a run by itself (the character count doesn't match the cell count).
        let runs = row_runs(&screen(20, 3, "ae\u{301}b".as_bytes()), 0);
        assert_eq!(texts(&runs), [(0, 1, "a"), (1, 1, "e\u{301}"), (2, 1, "b")]);
    }

    #[test]
    fn row_runs_inverse_swaps_colors() {
        let runs = row_runs(&screen(20, 3, b"\x1b[7mx\x1b[0m\x1b[31;44;7my"), 0);
        assert_eq!((runs[0].fg, runs[0].bg), (TERM_BG, Some(TERM_FG)));
        assert_eq!((runs[1].fg, runs[1].bg), (0x61afef, Some(0xe06c75)));
        // An inverse blank has a background, so it stays even at the end of a line.
        let runs = row_runs(&screen(20, 3, b"a\x1b[7m  "), 0);
        assert_eq!(texts(&runs), [(0, 1, "a"), (1, 2, "  ")]);
    }

    #[test]
    fn row_runs_keeps_link() {
        let url = "https://example.com/a";
        let bytes = format!("a\x1b]8;;{url}\x1b\\link\x1b]8;;\x1b\\b");
        let s = screen(20, 3, bytes.as_bytes());
        let runs = row_runs(&s, 0);
        assert_eq!(texts(&runs), [(0, 1, "a"), (1, 4, "link"), (5, 1, "b")]);
        assert_eq!(runs[1].link.as_deref(), Some(url));
        assert_eq!((runs[0].link.as_ref(), runs[2].link.as_ref()), (None, None));
        assert_eq!(link_at(&s, 2, 0).as_deref(), Some(url));
        assert_eq!(link_at(&s, 0, 0), None);
        assert_eq!(link_at(&s, 19, 2), None);
        assert_eq!(link_at(&s, 99, 99), None);
    }

    #[test]
    fn row_runs_indexed_and_truecolor() {
        let runs = row_runs(
            &screen(
                20,
                3,
                b"\x1b[38;5;196mA\x1b[38;2;1;2;3mB\x1b[0;48;5;21mC\x1b[0;95mD",
            ),
            0,
        );
        let colors: Vec<_> = runs.iter().map(|r| (r.fg, r.bg)).collect();
        assert_eq!(
            colors,
            [
                (0xff0000, None),
                (0x010203, None),
                (TERM_FG, Some(0x0000ff)),
                (0xd898e6, None),
            ]
        );
    }

    #[test]
    fn row_runs_carry_attributes() {
        let runs = row_runs(&screen(20, 3, b"\x1b[1;3;4;9;2mz\x1b[0m\x1b[8mh"), 0);
        let z = &runs[0];
        assert!(z.bold && z.italic && z.underline && z.strike && z.dim);
        // Hidden text is drawn in the background color.
        assert_eq!((runs[1].fg, runs[1].bg), (TERM_BG, None));
        assert!(!runs[1].bold && !runs[1].underline);
    }

    #[test]
    fn row_runs_follow_the_scrolled_view() {
        let mut s = screen(20, 3, b"1\r\n2\r\n3\r\n4");
        let first = |s: &Screen| -> Vec<String> {
            (0..3)
                .map(|r| {
                    row_runs(s, r)
                        .first()
                        .map(|r| r.text.clone())
                        .unwrap_or_default()
                })
                .collect()
        };
        assert_eq!(first(&s), ["2", "3", "4"]);
        s.scroll(1);
        assert_eq!(first(&s), ["1", "2", "3"]);
        // A line outside the screen has none.
        assert!(row_runs(&s, 3).is_empty());
    }

    #[test]
    fn grid_size_floors_and_clamps() {
        assert_eq!(grid_size(800.0, 600.0, 8.0, 17.0), (100, 35));
        assert_eq!(grid_size(0.0, 0.0, 8.0, 17.0), (1, 1));
        assert_eq!(grid_size(807.9, 33.9, 8.0, 17.0), (100, 1));
        // A multiple of the cell width doesn't lose a cell to floating-point error.
        let cell = 7.825_195_3;
        assert_eq!(grid_size(cell * 113.0, 160.0, cell, 16.0).0, 113);
        // If the cell size is unknown (0), it is the minimum size.
        assert_eq!(grid_size(800.0, 600.0, 0.0, 0.0), (1, 1));
        assert_eq!(grid_size(f32::NAN, -5.0, 8.0, 17.0), (1, 1));
    }

    #[test]
    fn scroll_lines_accumulates() {
        let mut acc = 0.0;
        assert_eq!(scroll_lines(&mut acc, 6.0, 17.0), 0);
        assert_eq!(scroll_lines(&mut acc, 6.0, 17.0), 0);
        assert_eq!(scroll_lines(&mut acc, 6.0, 17.0), 1);
        assert!((acc - 1.0).abs() < 1e-4);
        assert_eq!(scroll_lines(&mut acc, 40.0, 17.0), 2);
        assert!((acc - 7.0).abs() < 1e-4);
        // Scrolling the other way eats into the remainder first.
        assert_eq!(scroll_lines(&mut acc, -24.0, 17.0), -1);
        assert_eq!(scroll_lines(&mut acc, -34.0, 17.0), -2);
        // If the line height is unknown, it doesn't move.
        assert_eq!(scroll_lines(&mut acc, 100.0, 0.0), 0);
    }

    #[test]
    fn cursor_follows_the_session_mode() {
        let mut s = screen(20, 3, b"ab");
        let c = cursor(&s).unwrap();
        assert_eq!(
            (c.col, c.row, c.cells, c.shape),
            (2, 0, 1, CursorShape::Block)
        );
        s.feed(b"\x1b[4 q");
        assert_eq!(cursor(&s).unwrap().shape, CursorShape::Underline);
        s.feed(b"\x1b[6 q");
        assert_eq!(cursor(&s).unwrap().shape, CursorShape::Beam);
        // Hidden means none.
        s.feed(b"\x1b[?25l");
        assert_eq!(cursor(&s), None);
        // A cursor on the trailing cell of a wide character backs up to that character, becomes 2 cells, and knows that character.
        let s = screen(20, 3, "한\x1b[D".as_bytes());
        let c = cursor(&s).unwrap();
        assert_eq!((c.col, c.cells, c.text.as_str()), (0, 2, "한"));
    }

    #[test]
    fn cursor_hides_while_scrolled_back() {
        let mut s = screen(20, 3, b"1\r\n2\r\n3\r\n4");
        assert!(cursor(&s).is_some());
        s.scroll(1);
        assert_eq!(cursor(&s), None);
        s.scroll(-1);
        assert!(cursor(&s).is_some());
    }

    #[test]
    fn only_web_mail_and_file_links_open() {
        assert!(openable("https://example.com"));
        assert!(openable("HTTP://example.com"));
        assert!(openable("mailto:a@b.c"));
        assert!(openable("file:///tmp/a.txt"));
        assert!(!openable("x-apple.systempreferences:com.apple"));
        assert!(!openable("javascript:alert(1)"));
        assert!(!openable(""));
    }
}
