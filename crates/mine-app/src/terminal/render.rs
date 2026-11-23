//! 터미널 칸을 그릴 재료. GPUI 없이 클라이언트 `Screen`의 셀을 글자 묶음으로 바꾸고, 칸 크기와 휠 스크롤을
//! 셀 단위로 바꾼다.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use mine_core::screen::Screen;

use crate::theme::{ANSI, TERM_BG, TERM_CURSOR, TERM_FG, indexed};

/// 한 줄에서 같은 모양으로 이어진 셀들. `col`부터 `cells`칸을 차지한다. 색은 `0xRRGGBB`다.
///
/// 넓은 글자(2칸)와 좁은 글자는 한 묶음에 섞지 않고, 결합 문자가 얹힌 셀은 혼자 묶인다. 그래서 묶음의 글자
/// 수로 글자 하나의 폭을 알 수 있다(그리는 쪽이 글자를 셀에 맞춰 놓는다).
#[derive(Clone, Debug, PartialEq)]
pub struct CellRun {
    pub col: u16,
    pub cells: u16,
    pub text: String,
    pub fg: u32,
    /// 기본 바탕이면 `None`이다(칠하지 않는다).
    pub bg: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub dim: bool,
    /// OSC 8 링크 주소.
    pub link: Option<String>,
}

impl CellRun {
    /// `cell` 하나로 된 묶음(`col`부터 `cells`칸).
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

    /// 글자와 자리 말고 모양(색, 속성, 링크)이 같은가.
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

/// 화면에 보이는 `row`(0부터, 스크롤해 둔 위치를 따른다)의 글자 묶음. 줄 끝의 빈칸은 뺀다.
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
    // 마지막 묶음에 다음 셀을 이어 붙일 수 있는가: 결합 문자가 없고, 그 글자 폭(1 또는 2칸)이 같아야 한다.
    let mut open: Option<u16> = None;
    for x in 0..end {
        let cell = &line[Column(x)];
        // 넓은 글자의 뒤 칸은 앞 글자가 덮는다.
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

/// 그리지 않아도 되는 셀: 기본 바탕의 공백이고 밑줄·취소선·반전·링크가 없다.
fn blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell
            .flags
            .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
        && cell.hyperlink().is_none()
        && !combining(cell)
}

/// 셀에 결합 문자가 얹혀 있다. 링크가 있는 셀도 덧붙은 칸(`extra`)이 있어 `zerowidth()`가 빈 조각을 준다.
fn combining(cell: &Cell) -> bool {
    cell.zerowidth().is_some_and(|chars| !chars.is_empty())
}

/// 칸의 글자와 그 위에 얹힌 결합 문자. 탭 같은 제어 문자 칸은 공백이다.
fn symbol(cell: &Cell) -> String {
    let mut text = String::from(if cell.c.is_control() { ' ' } else { cell.c });
    text.extend(cell.zerowidth().unwrap_or_default());
    text
}

/// alacritty 색을 `0xRRGGBB`로. 0–15는 목업 팔레트, 16–255는 xterm 큐브와 회색 단계, 기본 글·바탕·커서는 터미널
/// 색이다. 세션이 OSC 4/10/11로 바꾼 색은 따르지 않는다(스냅숏이 되살리지 않아 다시 붙으면 달라진다).
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

/// 칸 크기(픽셀)에 들어가는 (열, 행). 최소 `(1, 1)`이다.
pub fn grid_size(pane_w: f32, pane_h: f32, cell_w: f32, line_h: f32) -> (u16, u16) {
    let fit = |len: f32, cell: f32| {
        if cell > 0.0 {
            // 셀 폭의 정확한 배수가 부동소수 오차로 한 칸 모자라지 않게 조금 더한다. `as`는 NaN과 음수를 0으로,
            // 너무 큰 값을 `u16::MAX`로 자른다.
            ((len / cell + 1e-3).floor() as u16).max(1)
        } else {
            1
        }
    };
    (fit(pane_w, cell_w), fit(pane_h, line_h))
}

/// 휠·트랙패드 픽셀 이동을 줄 수로 바꾼다. 한 줄이 안 되는 나머지는 `acc`에 모아 다음 이동에 더한다.
/// 양수가 과거 쪽이다(`Screen::scroll`과 같다).
pub fn scroll_lines(acc: &mut f32, delta_px: f32, line_h: f32) -> i32 {
    if line_h <= 0.0 || !delta_px.is_finite() {
        return 0;
    }
    *acc += delta_px;
    let lines = (*acc / line_h).trunc();
    *acc -= lines * line_h;
    lines as i32
}

/// 그릴 커서. 넓은 글자 위면 2칸이고, `text`는 그 셀의 글자다(블록 커서 위에 바탕색으로 다시 쓴다).
#[derive(Clone, Debug, PartialEq)]
pub struct CursorCell {
    pub col: u16,
    pub row: u16,
    pub cells: u16,
    pub shape: CursorShape,
    pub text: String,
}

/// 세션 커서. 세션이 숨겼거나 스크롤해 과거를 보는 동안은 없다. 모양은 세션이 정한 것(DECSCUSR)이고, 정하지
/// 않았으면 블록이다.
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

/// 보이는 (`col`, `row`) 셀의 OSC 8 링크 주소.
pub fn link_at(screen: &Screen, col: u16, row: u16) -> Option<String> {
    let grid = screen.term().grid();
    let (col, row) = (usize::from(col), usize::from(row));
    if col >= grid.columns() || row >= grid.screen_lines() {
        return None;
    }
    let mut point = Point::new(Line(row as i32 - grid.display_offset() as i32), Column(col));
    // 넓은 글자의 뒤 칸을 누르면 그 글자다.
    if col > 0 && grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) {
        point.column -= 1;
    }
    grid[point].hyperlink().map(|link| link.uri().to_string())
}

/// 누른 링크를 열어도 되는가. 웹·메일·파일 주소만 연다. 주소를 미리 보여 주지 않으니, 세션 출력이 심은 다른
/// 앱 주소(`x-…://`)로 무언가를 실행하지 않게 막는다.
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
        // 줄 끝의 빈칸은 묶음이 되지 않는다.
        assert_eq!(texts(&runs), [(0, 2, "ab"), (2, 2, "cd")]);
        assert_eq!((runs[0].fg, runs[0].bg), (TERM_FG, None));
        assert_eq!(runs[1].fg, 0xe06c75);
        // 가운데 빈칸은 같은 모양이면 이어 묶는다.
        let runs = row_runs(&screen(20, 3, b"a  b"), 0);
        assert_eq!(texts(&runs), [(0, 4, "a  b")]);
        // 빈 줄은 묶음이 없다.
        assert!(row_runs(&screen(20, 3, b"a"), 1).is_empty());
    }

    #[test]
    fn row_runs_wide_char_counts_two_cells() {
        let runs = row_runs(&screen(20, 3, "한a".as_bytes()), 0);
        assert_eq!(texts(&runs), [(0, 2, "한"), (2, 1, "a")]);
        // 넓은 글자끼리는 묶는다. 좁은 글자와는 섞지 않는다.
        let runs = row_runs(&screen(20, 3, "x한글 y".as_bytes()), 0);
        assert_eq!(texts(&runs), [(0, 1, "x"), (1, 4, "한글"), (5, 2, " y")]);
    }

    #[test]
    fn row_runs_keep_combining_cells_apart() {
        // 결합 문자가 얹힌 셀은 혼자 묶인다(글자 수와 칸 수가 맞지 않는다).
        let runs = row_runs(&screen(20, 3, "ae\u{301}b".as_bytes()), 0);
        assert_eq!(texts(&runs), [(0, 1, "a"), (1, 1, "e\u{301}"), (2, 1, "b")]);
    }

    #[test]
    fn row_runs_inverse_swaps_colors() {
        let runs = row_runs(&screen(20, 3, b"\x1b[7mx\x1b[0m\x1b[31;44;7my"), 0);
        assert_eq!((runs[0].fg, runs[0].bg), (TERM_BG, Some(TERM_FG)));
        assert_eq!((runs[1].fg, runs[1].bg), (0x61afef, Some(0xe06c75)));
        // 반전한 빈칸은 바탕이 있으니 줄 끝이어도 남는다.
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
        // 숨긴 글자는 바탕색으로 쓴다.
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
        // 화면 밖의 줄은 없다.
        assert!(row_runs(&s, 3).is_empty());
    }

    #[test]
    fn grid_size_floors_and_clamps() {
        assert_eq!(grid_size(800.0, 600.0, 8.0, 17.0), (100, 35));
        assert_eq!(grid_size(0.0, 0.0, 8.0, 17.0), (1, 1));
        assert_eq!(grid_size(807.9, 33.9, 8.0, 17.0), (100, 1));
        // 셀 폭의 배수에서 부동소수 오차로 한 칸을 잃지 않는다.
        let cell = 7.825_195_3;
        assert_eq!(grid_size(cell * 113.0, 160.0, cell, 16.0).0, 113);
        // 셀 크기를 모르면(0) 최소 크기다.
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
        // 반대로 돌리면 남은 것부터 깎는다.
        assert_eq!(scroll_lines(&mut acc, -24.0, 17.0), -1);
        assert_eq!(scroll_lines(&mut acc, -34.0, 17.0), -2);
        // 줄 높이를 모르면 움직이지 않는다.
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
        // 숨기면 없다.
        s.feed(b"\x1b[?25l");
        assert_eq!(cursor(&s), None);
        // 넓은 글자의 뒤 칸에 놓인 커서는 그 글자로 물러나 2칸이 되고, 그 글자를 안다.
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
