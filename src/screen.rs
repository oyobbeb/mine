//! 세션 화면 상태. PTY 출력을 먹여 칸을 유지하고, 같은 크기의 새 `Screen`이
//! 똑같은 화면을 되살릴 수 있는 바이트열(스냅숏)을 만든다.
//!
//! 안쪽 앱의 터미널 질의(DA1, 커서 위치 DSR, OSC 10/11 색, kitty 키보드 모드)에 대한 답은
//! `Replies`가 모아 두고 `take_replies`로 꺼낸다. 데몬만 꺼낸 답을 보관 프로세스에 입력으로 보낸다.
//! 클라이언트의 `Screen`은 꺼내서 버린다(같은 질의에 두 번 답하지 않도록).

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid, GridCell, Row, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::term::{Config, MIN_COLUMNS, MIN_SCREEN_LINES, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

/// 화면 위로 밀려난 줄을 몇 줄까지 간직하는가.
pub const SCROLLBACK: usize = 10_000;

/// `Term`에 크기를 알려 주는 구조체. 최소 크기 아래로는 내려가지 않는다.
#[derive(Clone, Copy)]
struct Size {
    cols: usize,
    rows: usize,
}

impl Size {
    fn new(cols: u16, rows: u16) -> Size {
        Size {
            cols: usize::from(cols).max(MIN_COLUMNS),
            rows: usize::from(rows).max(MIN_SCREEN_LINES),
        }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// 색 질의(OSC 10/11/12)에 답하는 처음 기본 전경색과 배경색. 클라이언트가 바깥 터미널의 실제 색을 알려 주면
/// `set_default_colors`로 바꾼다. 앱이 OSC로 바꾼 색이 있으면 그 색이 먼저다.
const DEFAULT_FG: Rgb = Rgb {
    r: 0xff,
    g: 0xff,
    b: 0xff,
};
const DEFAULT_BG: Rgb = Rgb {
    r: 0x28,
    g: 0x2c,
    b: 0x34,
};

/// 꺼내지 않고 쌓아 둘 답의 최대 개수. 꺼내지 않는 `Screen`(테스트 등)에서도 무한히 늘지 않게 막는다.
const MAX_PENDING_REPLIES: usize = 256;

/// 질의에 대한 답 하나. 색 답은 꺼낼 때 그 시점 색으로 채운다.
enum Reply {
    Text(String),
    Color(usize, Arc<dyn Fn(Rgb) -> String + Send + Sync>),
}

/// `Term`이 보내는 사건 중 PTY로 돌려보낼 답만 모으는 수신자.
#[derive(Clone, Default)]
pub struct Replies(Arc<Mutex<Vec<Reply>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        let reply = match event {
            Event::PtyWrite(text) => Reply::Text(text),
            Event::ColorRequest(index, format) => Reply::Color(index, format),
            _ => return,
        };
        let mut pending = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.len() < MAX_PENDING_REPLIES {
            pending.push(reply);
        }
    }
}

/// 세션 하나의 화면. 데몬이 PTY 출력을 `feed`로 먹이고, 붙는 클라이언트에는 `snapshot`을 보낸다.
pub struct Screen {
    term: Term<Replies>,
    parser: Processor,
    replies: Replies,
    /// 대체 화면에 있는 동안 가려진 주 화면(스크롤백 포함). `Term`은 비활성 화면을 내주지 않으므로
    /// 진입한 순간에 복사해 두고, 스냅숏이 대체 화면 뒤의 주 화면을 되살릴 때 쓴다.
    primary: Option<Grid<Cell>>,
    /// 색 질의에 답하는 기본 전경색(커서 색도 같다)과 배경색.
    default_fg: Rgb,
    default_bg: Rgb,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Screen {
        let config = Config {
            scrolling_history: SCROLLBACK,
            // 안쪽 앱이 켠 kitty 키보드 모드를 mode()로 읽고 스냅숏으로 되살리기 위해 켠다.
            kitty_keyboard: true,
            ..Config::default()
        };
        let replies = Replies::default();
        Screen {
            term: Term::new(config, &Size::new(cols, rows), replies.clone()),
            parser: Processor::new(),
            replies,
            primary: None,
            default_fg: DEFAULT_FG,
            default_bg: DEFAULT_BG,
        }
    }

    /// 색 질의에 답하는 기본 전경색과 배경색을 바꾼다. 커서 색은 전경색을 따른다.
    pub fn set_default_colors(&mut self, fg: Rgb, bg: Rgb) {
        self.default_fg = fg;
        self.default_bg = bg;
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        // 대체 화면 진입(`CSI ? 1049 h` 등)은 `h`로, 나감은 `l`로 끝난다. 그 바이트마다 끊어,
        // 진입 직후 주 화면이 아직 그대로일 때 붙잡고, 한 번에 나갔다 다시 들어와도 놓치지 않는다.
        for chunk in bytes.split_inclusive(|&b| b == b'h' || b == b'l') {
            self.parser.advance(&mut self.term, chunk);
            self.track_alt();
        }
    }

    /// 지금 화면이 대체 화면인지에 맞춰 가려진 주 화면 복사본을 만들거나 버린다.
    fn track_alt(&mut self) {
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        match (alt, self.primary.is_some()) {
            (true, false) => self.primary = Some(self.copy_primary()),
            (false, true) => self.primary = None,
            _ => {}
        }
    }

    /// 지금까지 먹인 출력의 터미널 질의에 대한 답(PTY에 쓸 바이트). 꺼낸 답은 비운다.
    pub fn take_replies(&mut self) -> Vec<u8> {
        let pending = std::mem::take(
            &mut *self
                .replies
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let mut out = String::new();
        for reply in pending {
            match reply {
                Reply::Text(text) => out.push_str(&text),
                Reply::Color(index, format) => {
                    if let Some(color) = self.color(index) {
                        out.push_str(&format(color));
                    }
                }
            }
        }
        out.into_bytes()
    }

    /// 색 질의에 답할 색. 앱이 정한 색이 먼저고, 없으면 기본 전경·배경색이다. 그 밖의 색 번호는 답하지 않는다.
    fn color(&self, index: usize) -> Option<Rgb> {
        let fallback = match index {
            i if i == NamedColor::Background as usize => self.default_bg,
            i if i == NamedColor::Foreground as usize || i == NamedColor::Cursor as usize => {
                self.default_fg
            }
            _ => return self.term.colors()[index],
        };
        Some(self.term.colors()[index].unwrap_or(fallback))
    }

    /// 동기화 갱신(`CSI ? 2026 h`) 중이면 그 갱신이 끝나야 할 시각. 이때까지 끝나지 않으면 `end_sync`를 부른다.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// 끝나지 않은 동기화 갱신을 끝내고 모아 둔 출력을 화면에 반영한다. 세션이 멈춘 화면에 갇히지 않게 한다.
    pub fn end_sync(&mut self) {
        self.parser.stop_sync(&mut self.term);
        self.track_alt();
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let size = Size::new(cols, rows);
        self.term.resize(size);
        if let Some(primary) = &mut self.primary {
            // `Term`이 비활성 주 화면에 하는 것과 같이 줄 바꿈을 다시 맞춘다.
            primary.resize(true, size.rows, size.cols);
        }
    }

    /// 스크롤백 + 보이는 칸(SGR 포함) + 모드 + 커서. 같은 크기의 새 `Screen`에 먹이면 화면이 되살아난다.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut w = Writer::new();
        // `2J`는 비어 있는 주 화면에서도 빈 줄 하나를 스크롤백에 밀어 넣으므로 `3J`로 스크롤백을 비운다.
        w.out.push_str("\x1b[0m\x1b[H\x1b[2J\x1b[3J");
        let grid = self.term.grid();
        let mode = *self.term.mode();
        if mode.contains(TermMode::ALT_SCREEN) {
            if let Some(primary) = &self.primary {
                w.grid(primary);
                w.cursor(primary);
                w.pen(&primary.cursor.template);
            }
            w.out.push_str("\x1b[?1049h");
            // 진입할 때 비워지는 대체 화면의 바탕색은 그때 펜에 달렸다. 기본색으로 다시 비우고 그린다.
            w.plain();
            w.out.push_str("\x1b[2J");
        }
        w.grid(grid);
        w.modes(mode);
        w.cursor(grid);
        w.pen(&grid.cursor.template);
        w.out.into_bytes()
    }

    /// 보이는 줄. 오른쪽 공백은 뗀다. 스크롤해 둔 위치와 무관하게 실제 화면이다.
    pub fn lines(&self) -> Vec<String> {
        let grid = self.term.grid();
        (0..grid.screen_lines())
            .map(|line| row_text(&grid[Line(line as i32)], grid.columns()))
            .collect()
    }

    /// 스크롤백 + 보이는 줄, 오래된 것부터.
    pub fn history_text(&self) -> Vec<String> {
        let grid = self.term.grid();
        (grid.topmost_line().0..=grid.bottommost_line().0)
            .map(|line| row_text(&grid[Line(line)], grid.columns()))
            .collect()
    }

    /// `(col, row)`, 0부터.
    pub fn cursor(&self) -> (u16, u16) {
        let point = self.term.grid().cursor.point;
        (point.column.0 as u16, point.line.0 as u16)
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// 양수 = 과거로.
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    pub fn term(&self) -> &Term<Replies> {
        &self.term
    }

    /// 대체 화면이 막 열린 지금, 가려진 주 화면을 복사한다.
    ///
    /// `swap_alt`로 주 화면을 잠시 꺼냈다가 되돌리면 대체 화면이 비워지므로,
    /// 대체 화면은 미리 복사해 두었다가 되돌려 놓는다.
    fn copy_primary(&mut self) -> Grid<Cell> {
        let alt = self.term.grid().clone();
        self.term.swap_alt();
        let primary = self.term.grid().clone();
        self.term.swap_alt();
        *self.term.grid_mut() = alt;
        primary
    }
}

/// SGR로 나타내는 칸 속성. 기본값이면 `0`만 내보낸다.
#[derive(Clone, Copy, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
}

/// 펜이 되살리는 플래그. 넓은 글자나 줄 이음 같은 칸 구조 플래그는 뺀다.
const PEN_FLAGS: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT)
    .union(Flags::ALL_UNDERLINES);

const SGR_FLAGS: [(Flags, &str); 11] = [
    (Flags::BOLD, "1"),
    (Flags::DIM, "2"),
    (Flags::ITALIC, "3"),
    (Flags::UNDERLINE, "4"),
    (Flags::DOUBLE_UNDERLINE, "4:2"),
    (Flags::UNDERCURL, "4:3"),
    (Flags::DOTTED_UNDERLINE, "4:4"),
    (Flags::DASHED_UNDERLINE, "4:5"),
    (Flags::INVERSE, "7"),
    (Flags::HIDDEN, "8"),
    (Flags::STRIKEOUT, "9"),
];

impl Pen {
    const DEFAULT: Pen = Pen {
        fg: Color::Named(NamedColor::Foreground),
        bg: Color::Named(NamedColor::Background),
        flags: Flags::empty(),
        underline: None,
    };

    fn of(cell: &Cell) -> Pen {
        Pen {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & PEN_FLAGS,
            underline: cell.underline_color(),
        }
    }

    /// 이 펜을 통째로 정하는 SGR. 앞의 속성은 `0`으로 지운다.
    fn write_sgr(&self, out: &mut String) {
        out.push_str("\x1b[0");
        push_color(out, self.fg, 38);
        push_color(out, self.bg, 48);
        if let Some(color) = self.underline {
            push_color(out, color, 58);
        }
        for (flag, param) in SGR_FLAGS {
            if self.flags.contains(flag) {
                let _ = write!(out, ";{param}");
            }
        }
        out.push('m');
    }
}

/// 이름 있는 색의 0~15번 번호. 기본색 계열은 `None`.
fn named_index(name: NamedColor) -> Option<u8> {
    use NamedColor::*;
    Some(match name {
        Black | DimBlack => 0,
        Red | DimRed => 1,
        Green | DimGreen => 2,
        Yellow | DimYellow => 3,
        Blue | DimBlue => 4,
        Magenta | DimMagenta => 5,
        Cyan | DimCyan => 6,
        White | DimWhite => 7,
        BrightBlack => 8,
        BrightRed => 9,
        BrightGreen => 10,
        BrightYellow => 11,
        BrightBlue => 12,
        BrightMagenta => 13,
        BrightCyan => 14,
        BrightWhite => 15,
        Foreground | Background | Cursor | BrightForeground | DimForeground => return None,
    })
}

/// 색 하나를 SGR 매개변수(앞에 `;`)로 쓴다. `kind`는 38(전경), 48(배경), 58(밑줄). 기본색은 쓰지 않는다.
fn push_color(out: &mut String, color: Color, kind: u8) {
    let _ = match color {
        Color::Named(name) => {
            let Some(i) = named_index(name) else { return };
            match kind {
                38 => write!(out, ";{}", if i < 8 { 30 + i } else { 82 + i }),
                48 => write!(out, ";{}", if i < 8 { 40 + i } else { 92 + i }),
                _ => write!(out, ";{kind};5;{i}"),
            }
        }
        Color::Indexed(i) => write!(out, ";{kind};5;{i}"),
        Color::Spec(Rgb { r, g, b }) => write!(out, ";{kind};2;{r};{g};{b}"),
    };
}

/// 스냅숏 바이트를 쌓으면서, 터미널에 이미 켜 둔 펜과 하이퍼링크를 기억해 바뀔 때만 내보낸다.
struct Writer {
    out: String,
    pen: Pen,
    link: Option<Hyperlink>,
}

impl Writer {
    fn new() -> Writer {
        Writer {
            out: String::new(),
            pen: Pen::DEFAULT,
            link: None,
        }
    }

    /// 칸의 속성과 하이퍼링크로 상태를 맞춘다. 글자는 쓰지 않는다.
    fn pen(&mut self, cell: &Cell) {
        let pen = Pen::of(cell);
        if pen != self.pen {
            pen.write_sgr(&mut self.out);
            self.pen = pen;
        }
        let link = cell.hyperlink().filter(link_is_plain);
        if link != self.link {
            if self.link.is_some() {
                self.out.push_str("\x1b]8;;\x1b\\");
            }
            if let Some(link) = &link {
                // 자동으로 붙은 id는 원래 없던 값이므로 내보내지 않는다.
                let id = link.id();
                let params = if id.ends_with("_alacritty") {
                    String::new()
                } else {
                    format!("id={id}")
                };
                let _ = write!(self.out, "\x1b]8;{params};{}\x1b\\", link.uri());
            }
            self.link = link;
        }
    }

    /// 펜만 기본으로 돌린다. 하이퍼링크는 줄을 넘어도 이어지므로 그대로 둔다.
    fn reset_pen(&mut self) {
        if self.pen != Pen::DEFAULT {
            self.out.push_str("\x1b[0m");
            self.pen = Pen::DEFAULT;
        }
    }

    /// 펜과 하이퍼링크를 모두 기본으로 돌린다.
    fn plain(&mut self) {
        self.pen(&Cell::default());
    }

    fn cell(&mut self, cell: &Cell) {
        // 넓은 글자의 뒤 칸, 그리고 넓은 글자가 다음 줄로 넘어가며 남긴 칸은 앞 글자가 만든다.
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            return;
        }
        self.pen(cell);
        self.out.push(printable(cell.c));
        for &c in cell.zerowidth().unwrap_or_default() {
            self.out.push(c);
        }
    }

    /// 스크롤백과 보이는 줄을 위에서부터 그린다. 마지막 줄이 맨 아래에 놓이면서 앞줄이 스크롤백으로 밀린다.
    fn grid(&mut self, grid: &Grid<Cell>) {
        let cols = grid.columns();
        let bottom = grid.bottommost_line().0;
        self.out.push_str("\x1b[H");
        let mut after_wrap = false;
        for line in grid.topmost_line().0..=bottom {
            let row = &grid[Line(line)];
            // 줄 바꿈으로 이어진 줄은 끝까지 그려 자동 줄 바꿈이 이어 받게 한다(줄 이음 표시를 보존한다).
            let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            let mut len = if wrapped {
                cols
            } else {
                content_len(row, cols)
            };
            if after_wrap {
                // 앞줄의 줄 바꿈 대기는 글자를 하나 써야 풀린다. 이어진 줄이 비었어도 첫 칸은 쓴다.
                len = len.max(1);
            }
            for col in 0..len {
                self.cell(&row[Column(col)]);
            }
            if after_wrap && len < cols {
                // 자동 줄 바꿈은 새 줄을 첫 글자의 펜 바탕색으로 채운다. 그리지 않은 칸은 기본으로 되돌린다.
                self.reset_pen();
                self.out.push_str("\x1b[K");
            }
            if line < bottom && !wrapped {
                // 새 줄의 바탕색이 펜을 따라가므로 줄바꿈 전에 펜을 지운다.
                self.reset_pen();
                self.out.push_str("\r\n");
            }
            after_wrap = wrapped;
        }
    }

    /// 주어진 화면의 커서를 되살린다.
    fn cursor(&mut self, grid: &Grid<Cell>) {
        let point = grid.cursor.point;
        let (row, mut col) = (point.line.0 + 1, point.column.0 + 1);
        if grid.cursor.input_needs_wrap {
            // 마지막 칸을 쓴 직후라 다음 글자가 줄을 넘긴다. 그 칸을 다시 써서 같은 대기 상태를 만든다.
            let line = &grid[point.line];
            if line[point.column].flags.contains(Flags::WIDE_CHAR_SPACER) {
                col -= 1;
            }
            let _ = write!(self.out, "\x1b[{row};{col}H");
            self.cell(&line[Column(col - 1)]);
        } else {
            let _ = write!(self.out, "\x1b[{row};{col}H");
        }
    }

    fn modes(&mut self, mode: TermMode) {
        for (flag, code) in MODES {
            let set = if mode.contains(flag) { 'h' } else { 'l' };
            let _ = write!(self.out, "\x1b[?{code}{set}");
        }
        self.out.push_str(if mode.contains(TermMode::APP_KEYPAD) {
            "\x1b="
        } else {
            "\x1b>"
        });
        let kitty: u8 = KITTY_FLAGS
            .iter()
            .filter(|(flag, _)| mode.contains(*flag))
            .map(|(_, bit)| bit)
            .sum();
        let _ = write!(self.out, "\x1b[={kitty};1u");
    }
}

/// 입력 해석에 영향을 주는 DEC 비공개 모드. 마우스 모드끼리, 1005와 1006은 서로 배타라
/// 켜진 것 하나만 `h`가 나가므로 순서와 무관하게 같은 상태가 된다.
const MODES: [(TermMode, u16); 11] = [
    (TermMode::APP_CURSOR, 1),
    (TermMode::LINE_WRAP, 7),
    (TermMode::SHOW_CURSOR, 25),
    (TermMode::MOUSE_REPORT_CLICK, 1000),
    (TermMode::MOUSE_DRAG, 1002),
    (TermMode::MOUSE_MOTION, 1003),
    (TermMode::FOCUS_IN_OUT, 1004),
    (TermMode::UTF8_MOUSE, 1005),
    (TermMode::SGR_MOUSE, 1006),
    (TermMode::ALTERNATE_SCROLL, 1007),
    (TermMode::BRACKETED_PASTE, 2004),
];

/// kitty 키보드 프로토콜 플래그 비트.
const KITTY_FLAGS: [(TermMode, u8); 5] = [
    (TermMode::DISAMBIGUATE_ESC_CODES, 1),
    (TermMode::REPORT_EVENT_TYPES, 2),
    (TermMode::REPORT_ALTERNATE_KEYS, 4),
    (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
    (TermMode::REPORT_ASSOCIATED_TEXT, 16),
];

/// OSC 8에 그대로 실어도 안전한 링크인가(제어 문자가 끼면 시퀀스를 깰 수 있다).
fn link_is_plain(link: &Hyperlink) -> bool {
    !link.uri().chars().any(char::is_control) && !link.id().chars().any(char::is_control)
}

/// 칸의 글자를 그대로 내보내도 되는 글자로 바꾼다. 탭 칸(`\t`)은 커서를 움직이므로 공백으로 쓴다.
fn printable(c: char) -> char {
    if c.is_control() { ' ' } else { c }
}

/// 오른쪽 끝의 아무 속성 없는 빈칸을 뺀 줄 길이.
fn content_len(row: &Row<Cell>, cols: usize) -> usize {
    (0..cols)
        .rev()
        .find(|&col| {
            let cell = &row[Column(col)];
            !cell.is_empty() || cell.hyperlink().is_some()
        })
        .map_or(0, |col| col + 1)
}

fn row_text(row: &Row<Cell>, cols: usize) -> String {
    let mut text = String::new();
    for col in 0..cols {
        let cell = &row[Column(col)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        text.push(printable(cell.c));
        text.extend(cell.zerowidth().unwrap_or_default());
    }
    text.truncate(text.trim_end_matches(' ').len());
    text
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    use super::*;

    fn rebuilt(s: &Screen) -> Screen {
        let mut f = Screen::new(40, 10);
        f.feed(&s.snapshot());
        f
    }

    /// 스크롤백을 포함한 모든 칸(속성, 줄 이음 표시까지)이 같은지 본다. 다르면 첫 불일치 칸만 알린다.
    fn assert_same_cells(f: &Screen, s: &Screen) {
        let (gf, gs) = (f.term().grid(), s.term().grid());
        assert_eq!(
            (gf.topmost_line(), gf.columns(), gf.screen_lines()),
            (gs.topmost_line(), gs.columns(), gs.screen_lines()),
            "크기가 다르다"
        );
        for line in gs.topmost_line().0..=gs.bottommost_line().0 {
            for col in 0..gs.columns() {
                let (cf, cs) = (&gf[Line(line)][Column(col)], &gs[Line(line)][Column(col)]);
                assert_eq!(cf, cs, "줄 {line}, 칸 {col}");
            }
        }
    }

    #[test]
    fn snapshot_rebuilds_same_screen() {
        let mut s = Screen::new(40, 10);
        s.feed("\x1b[31mred\x1b[0m\r\n한글🙂 wide\r\n\x1b[5;10Hx".as_bytes());
        let f = rebuilt(&s);
        assert_eq!(f.lines(), s.lines());
        assert_eq!(f.cursor(), s.cursor());
        assert_eq!(
            f.term().grid()[Line(0)][Column(0)].fg,
            Color::Named(NamedColor::Red)
        );
    }

    #[test]
    fn snapshot_keeps_scrollback() {
        let mut s = Screen::new(40, 10);
        for i in 0..50 {
            s.feed(format!("line {i}\r\n").as_bytes());
        }
        let h = rebuilt(&s).history_text();
        assert!(h.len() >= 50);
        assert_eq!(h[0], "line 0");
    }

    #[test]
    fn snapshot_restores_modes() {
        let mut s = Screen::new(40, 10);
        s.feed(b"\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1049h");
        let m = rebuilt(&s).mode();
        for want in [
            TermMode::BRACKETED_PASTE,
            TermMode::MOUSE_REPORT_CLICK,
            TermMode::SGR_MOUSE,
            TermMode::ALT_SCREEN,
        ] {
            assert!(m.contains(want), "{want:?}");
        }
    }

    #[test]
    fn scrollback_capped() {
        let mut s = Screen::new(10, 5);
        for i in 0..(SCROLLBACK + 100) {
            s.feed(format!("{i}\r\n").as_bytes());
        }
        assert!(s.history_text().len() <= SCROLLBACK + 5);
    }

    #[test]
    fn snapshot_keeps_hyperlinks() {
        let mut s = Screen::new(40, 10);
        s.feed(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ plain");
        let f = rebuilt(&s);
        let row = &f.term().grid()[Line(0)];
        let link = row[Column(0)].hyperlink().expect("link cell");
        assert_eq!(link.uri(), "https://example.com");
        assert_eq!(row[Column(3)].hyperlink(), row[Column(0)].hyperlink());
        assert!(row[Column(4)].hyperlink().is_none());
        assert_eq!(row[Column(5)].c, 'p');
        assert!(row[Column(5)].hyperlink().is_none());
    }

    #[test]
    fn snapshot_keeps_hyperlink_ids_and_separates_links() {
        let mut s = Screen::new(40, 10);
        s.feed(b"\x1b]8;id=a1;https://a.example\x1b\\aa\x1b]8;;\x1b\\");
        s.feed(b"\x1b]8;;https://b.example\x1b\\bb\x1b]8;;https://b.example\x1b\\cc\x1b]8;;\x1b\\");
        let f = rebuilt(&s);
        let row = &f.term().grid()[Line(0)];
        let a = row[Column(0)].hyperlink().expect("a");
        assert_eq!((a.id(), a.uri()), ("a1", "https://a.example"));
        // 같은 주소라도 따로 연 링크는 다른 링크로 남는다.
        let (b, c) = (row[Column(2)].hyperlink(), row[Column(4)].hyperlink());
        assert_eq!(b.as_ref().map(|l| l.uri()), Some("https://b.example"));
        assert_ne!(b, c);
    }

    #[test]
    fn snapshot_keeps_attributes() {
        let mut s = Screen::new(40, 10);
        s.feed(b"\x1b[1;3;4;7;9mA\x1b[0m\x1b[2;8mB\x1b[0m");
        s.feed(b"\x1b[4:2mC\x1b[4:3;58;2;1;2;3mD\x1b[4:4mE\x1b[4:5;58;5;9mF\x1b[0m");
        s.feed(b"\x1b[38;5;200;48;2;10;20;30mG\x1b[91;102mH\x1b[90;41mI\x1b[0m\r\n");
        s.feed("\x1b[32m한\x1b[44m글\x1b[0m e\u{301}".as_bytes());
        s.feed(b"\x1b[43m\x1b[K\r\n\x1b[0mtail");
        let f = rebuilt(&s);
        assert_same_cells(&f, &s);
        let (a, g) = (&f.term().grid()[Line(0)][Column(0)], 6);
        assert!(
            a.flags
                .contains(Flags::BOLD | Flags::ITALIC | Flags::INVERSE)
        );
        assert_eq!(f.term().grid()[Line(0)][Column(g)].fg, Color::Indexed(200));
    }

    #[test]
    fn snapshot_keeps_soft_wraps() {
        let mut s = Screen::new(40, 10);
        // 100칸 한 줄, 그리고 마지막 칸에 안 들어가는 넓은 글자로 넘어가는 줄.
        s.feed("x".repeat(100).as_bytes());
        s.feed(format!("\r\n{}한글\r\nend", "a".repeat(39)).as_bytes());
        let f = rebuilt(&s);
        assert_same_cells(&f, &s);
        assert_eq!(f.lines(), s.lines());
    }

    #[test]
    fn snapshot_keeps_pending_wrap() {
        let mut s = Screen::new(40, 10);
        s.feed("a".repeat(40).as_bytes());
        let mut f = rebuilt(&s);
        assert_eq!(f.cursor(), s.cursor());
        assert!(f.term().grid().cursor.input_needs_wrap);
        s.feed(b"b");
        f.feed(b"b");
        assert_eq!(f.lines(), s.lines());
        assert_eq!(f.cursor(), (1, 1));
    }

    #[test]
    fn snapshot_keeps_pending_wrap_after_wide_char() {
        let mut s = Screen::new(40, 10);
        s.feed("a".repeat(38).as_bytes());
        s.feed("한".as_bytes());
        let mut f = rebuilt(&s);
        assert_eq!(f.cursor(), s.cursor());
        assert!(f.term().grid().cursor.input_needs_wrap);
        s.feed(b"b");
        f.feed(b"b");
        assert_same_cells(&f, &s);
    }

    #[test]
    fn snapshot_keeps_empty_row_after_wrapped_row() {
        // 줄이 40칸을 채워 다음 줄로 넘어간 뒤, 넘어간 글자를 지워 이어진 줄이 비는 경우.
        let mut s = Screen::new(40, 10);
        s.feed(format!("{}b\x08 \x08\r\nnext", "a".repeat(40)).as_bytes());
        let f = rebuilt(&s);
        assert_eq!(f.lines(), s.lines());
        assert_eq!(f.lines()[1], "");
        assert_same_cells(&f, &s);

        // 스크롤백이 있어도 보이는 줄이 밀리지 않는다.
        let mut s = Screen::new(40, 10);
        for i in 0..20 {
            s.feed(format!("l{i}\r\n").as_bytes());
        }
        s.feed(format!("{}b\r\x1b[K\r\nnext", "a".repeat(40)).as_bytes());
        let f = rebuilt(&s);
        assert_eq!(f.lines(), s.lines());
        assert_same_cells(&f, &s);

        // 이어진 빈 줄이 맨 아래 줄이어도 줄 이음 표시가 남는다.
        let mut s = Screen::new(40, 10);
        s.feed(format!("\x1b[9;1H{}b\x08 \x08", "a".repeat(40)).as_bytes());
        let f = rebuilt(&s);
        assert_same_cells(&f, &s);
        assert_eq!(f.cursor(), s.cursor());
    }

    #[test]
    fn snapshot_wrap_does_not_leak_background() {
        // 이어진 줄의 첫 칸 펜 바탕색이 자동 줄 바꿈으로 새 줄 전체에 번지면 안 된다.
        let mut s = Screen::new(40, 10);
        for i in 0..20 {
            s.feed(format!("l{i}\r\n").as_bytes());
        }
        s.feed(format!("\x1b[H\x1b[41m{}\x1b[0m\x1b[K", "x".repeat(41)).as_bytes());
        let f = rebuilt(&s);
        let bg = |s: &Screen| s.term().grid()[Line(1)][Column(5)].bg;
        assert_eq!(bg(&s), Color::Named(NamedColor::Background));
        assert_eq!(bg(&f), bg(&s));
        assert_same_cells(&f, &s);
    }

    #[test]
    fn snapshot_survives_tab_cells() {
        let mut s = Screen::new(40, 10);
        s.feed(b"a\tb\r\n\tc");
        let f = rebuilt(&s);
        assert_eq!(f.lines(), s.lines());
        assert_eq!(f.cursor(), s.cursor());
    }

    #[test]
    fn snapshot_keeps_pen_and_cursor_state() {
        let mut s = Screen::new(40, 10);
        s.feed(b"x\x1b[1;34m\x1b]8;id=z;https://e.example\x1b\\\x1b[?25l\x1b[3;7H");
        let mut f = rebuilt(&s);
        assert_eq!(f.cursor(), (6, 2));
        assert!(!f.mode().contains(TermMode::SHOW_CURSOR));
        // 이어지는 출력은 같은 펜과 링크로 나온다.
        s.feed(b"y");
        f.feed(b"y");
        assert_same_cells(&f, &s);
    }

    #[test]
    fn snapshot_restores_input_modes() {
        let mut s = Screen::new(40, 10);
        s.feed(b"\x1b[?1h\x1b[?1004h\x1b[?1007l\x1b[?7l\x1b[?1003h\x1b=\x1b[>9u");
        let m = rebuilt(&s).mode();
        for want in [
            TermMode::APP_CURSOR,
            TermMode::FOCUS_IN_OUT,
            TermMode::MOUSE_MOTION,
            TermMode::APP_KEYPAD,
            TermMode::DISAMBIGUATE_ESC_CODES,
            TermMode::REPORT_ALL_KEYS_AS_ESC,
        ] {
            assert!(m.contains(want), "{want:?}");
        }
        for not in [
            TermMode::ALTERNATE_SCROLL,
            TermMode::LINE_WRAP,
            TermMode::MOUSE_REPORT_CLICK,
            TermMode::MOUSE_DRAG,
            TermMode::SGR_MOUSE,
            TermMode::BRACKETED_PASTE,
        ] {
            assert!(!m.contains(not), "{not:?}");
        }
        assert!(m.contains(TermMode::SHOW_CURSOR));
    }

    #[test]
    fn snapshot_in_alt_screen_keeps_primary() {
        let mut s = Screen::new(40, 10);
        for i in 0..30 {
            s.feed(format!("before {i}\r\n").as_bytes());
        }
        s.feed(b"\x1b[31mprompt$ \x1b[0m\x1b[?1049h\x1b[2;3Halt screen\x1b[7mbar");
        // 복사하느라 대체 화면을 건드리지 않았다.
        assert_eq!(s.lines()[1], "  alt screenbar");
        let mut f = rebuilt(&s);
        assert!(f.mode().contains(TermMode::ALT_SCREEN));
        assert_same_cells(&f, &s);
        assert_eq!(f.cursor(), s.cursor());
        // 대체 화면을 나서면 가려져 있던 주 화면과 스크롤백이 돌아온다.
        s.feed(b"\x1b[?1049l");
        f.feed(b"\x1b[?1049l");
        assert!(!f.mode().contains(TermMode::ALT_SCREEN));
        assert_same_cells(&f, &s);
        assert_eq!(f.history_text(), s.history_text());
        assert_eq!(f.cursor(), s.cursor());
        assert!(s.lines().iter().any(|l| l == "prompt$"));
    }

    #[test]
    fn snapshot_in_alt_screen_after_exit_and_reentry_in_one_feed() {
        // 한 번에 들어온 바이트 안에서 나갔다 다시 들어가면, 그 사이에 주 화면에 쓴 글이 복사본에 있어야 한다.
        let mut s = Screen::new(40, 10);
        s.feed(b"p0\r\n");
        s.feed(b"\x1b[?1049h");
        s.feed(b"\x1b[?1049lOUT X1\r\n\x1b[?1049h");
        let mut f = rebuilt(&s);
        s.feed(b"\x1b[?1049l");
        f.feed(b"\x1b[?1049l");
        assert_eq!(s.lines()[..3], ["p0", "OUT X1", ""]);
        assert_eq!(f.lines(), s.lines());
        assert_same_cells(&f, &s);
    }

    #[test]
    fn snapshot_in_alt_screen_after_resize_keeps_primary() {
        let mut s = Screen::new(40, 10);
        for i in 0..20 {
            s.feed(format!("row {i}\r\n").as_bytes());
        }
        s.feed(b"\x1b[?1049h\x1b[Halt");
        s.resize(30, 8);
        let mut f = Screen::new(30, 8);
        f.feed(&s.snapshot());
        s.feed(b"\x1b[?1049l");
        f.feed(b"\x1b[?1049l");
        assert_eq!(f.lines(), s.lines());
        assert_eq!(f.history_text(), s.history_text());
    }

    #[test]
    fn snapshot_ignores_scroll_offset() {
        let mut s = Screen::new(40, 10);
        for i in 0..50 {
            s.feed(format!("line {i}\r\n").as_bytes());
        }
        let before = s.snapshot();
        s.scroll(5);
        assert_eq!(s.term().grid().display_offset(), 5);
        assert_eq!(s.snapshot(), before);
        s.scroll(-5);
        assert_eq!(s.term().grid().display_offset(), 0);
    }

    #[test]
    fn answers_cursor_position_query() {
        let mut s = Screen::new(40, 10);
        s.feed(b"\x1b[3;5H\x1b[6n");
        assert_eq!(s.take_replies(), b"\x1b[3;5R");
        assert!(s.take_replies().is_empty());
    }

    #[test]
    fn answers_device_colour_and_keyboard_queries() {
        let mut s = Screen::new(40, 10);
        s.feed(b"\x1b[c\x1b]11;?\x07\x1b]10;?\x1b\\\x1b[>1u\x1b[?u");
        let want = "\x1b[?6c\x1b]11;rgb:2828/2c2c/3434\x07\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b[?1u";
        assert_eq!(String::from_utf8(s.take_replies()).unwrap(), want);
        // 앱이 OSC로 바꾼 색으로 답한다.
        s.feed(b"\x1b]11;rgb:01/02/03\x07\x1b]11;?\x07");
        assert_eq!(s.take_replies(), b"\x1b]11;rgb:0101/0202/0303\x07");
    }

    #[test]
    fn colour_queries_use_set_default_colours() {
        let mut s = Screen::new(40, 10);
        let fg = Rgb {
            r: 0x0a,
            g: 0x0b,
            b: 0x0c,
        };
        let bg = Rgb {
            r: 0x01,
            g: 0x02,
            b: 0x03,
        };
        s.set_default_colors(fg, bg);
        s.feed(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07");
        let want = "\x1b]10;rgb:0a0a/0b0b/0c0c\x07\x1b]11;rgb:0101/0202/0303\x07\x1b]12;rgb:0a0a/0b0b/0c0c\x07";
        assert_eq!(String::from_utf8(s.take_replies()).unwrap(), want);
    }

    #[test]
    fn unread_replies_are_capped() {
        let mut s = Screen::new(40, 10);
        s.feed(&b"\x1b[c".repeat(MAX_PENDING_REPLIES + 10));
        assert_eq!(
            s.take_replies().len(),
            MAX_PENDING_REPLIES * b"\x1b[?6c".len()
        );
    }

    #[test]
    fn unfinished_sync_update_can_be_ended() {
        let mut s = Screen::new(40, 10);
        s.feed(b"\x1b[?2026hframe");
        assert!(s.sync_deadline().is_some());
        assert_eq!(s.lines()[0], "");
        s.end_sync();
        assert!(s.sync_deadline().is_none());
        assert_eq!(s.lines()[0], "frame");
        // 끝난 동기화 갱신은 데드라인이 없다.
        s.feed(b"\x1b[?2026h!\x1b[?2026l");
        assert!(s.sync_deadline().is_none());
        assert_eq!(s.lines()[0], "frame!");
    }

    #[test]
    fn resize_changes_grid_and_clamps() {
        let mut s = Screen::new(40, 10);
        s.resize(20, 5);
        assert_eq!((s.term().columns(), s.term().screen_lines()), (20, 5));
        s.resize(0, 0);
        assert_eq!(
            (s.term().columns(), s.term().screen_lines()),
            (MIN_COLUMNS, MIN_SCREEN_LINES)
        );
    }
}
