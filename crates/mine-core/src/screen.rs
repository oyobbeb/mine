//! Session screen state. Feeds in PTY output to keep the cells, and builds a byte sequence (snapshot) from which a new `Screen` of the same size
//! can restore the exact same screen.
//!
//! Answers to the inner app's terminal queries (DA1, cursor position DSR, OSC 10/11 colors, kitty keyboard mode)
//! are collected by `Replies` and taken out with `take_replies`. Only the daemon sends the taken answers to the holder process as input.
//! A client's `Screen` takes them out and throws them away (so the same query isn't answered twice).

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Grid, GridCell, Row, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::term::{Config, MIN_COLUMNS, MIN_SCREEN_LINES, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

/// How many lines pushed off the top of the screen are kept.
pub const SCROLLBACK: usize = 10_000;

/// Struct that tells `Term` the size. It never goes below the minimum size.
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

/// Initial default foreground and background colors that answer color queries (OSC 10/11/12). When the client reports the outer terminal's actual colors,
/// they are replaced with `set_default_colors`. A color the app changed with OSC takes precedence.
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

/// Maximum length of the start of a modifyOtherKeys sequence held back when it straddles a read boundary.
const MOK_MAX: usize = 16;

/// Maximum number of answers kept without being taken out. Keeps a `Screen` that never takes them out (tests, etc.) from growing without bound.
const MAX_PENDING_REPLIES: usize = 256;

/// One answer to a query. Color answers are filled in with the color at the time they are taken out.
enum Reply {
    Text(String),
    Color(usize, Arc<dyn Fn(Rgb) -> String + Send + Sync>),
}

/// Receiver that collects only those events from `Term` that need an answer sent back to the PTY.
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

/// The screen of one session. The daemon feeds PTY output with `feed` and sends `snapshot` to clients that attach.
pub struct Screen {
    term: Term<Replies>,
    parser: Processor,
    replies: Replies,
    /// The main screen hidden while on the alternate screen (including scrollback). `Term` doesn't hand out the inactive screen,
    /// so it is copied at the moment of entry and used when a snapshot restores the main screen behind the alternate screen.
    primary: Option<Grid<Cell>>,
    /// Default foreground (the cursor color is the same) and background that answer color queries.
    default_fg: Rgb,
    default_bg: Rgb,
    /// The xterm modifyOtherKeys level the inner app set (`CSI > 4 ; N m`). vte reads it but the alacritty `Term` doesn't remember it,
    /// so it is looked for separately in the output.
    modify_other_keys: u8,
    /// The start of that sequence that straddled a read boundary. Continued on the next `feed`.
    mok_tail: Vec<u8>,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Screen {
        let config = Config {
            scrolling_history: SCROLLBACK,
            // Turned on so the kitty keyboard mode the inner app set can be read with mode() and restored from the snapshot.
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
            modify_other_keys: 0,
            mok_tail: Vec::new(),
        }
    }

    /// Changes the default foreground and background colors that answer color queries. The cursor color follows the foreground.
    pub fn set_default_colors(&mut self, fg: Rgb, bg: Rgb) {
        self.default_fg = fg;
        self.default_bg = bg;
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.track_modify_other_keys(bytes);
        // Entering the alternate screen (`CSI ? 1049 h`, etc.) ends in `h` and leaving ends in `l`. Split at each of those bytes
        // so the main screen is captured right after entry while it is still unchanged, and nothing is missed even if it leaves and re-enters in one go.
        for chunk in bytes.split_inclusive(|&b| b == b'h' || b == b'l') {
            self.parser.advance(&mut self.term, chunk);
            self.track_alt();
        }
    }

    /// Finds and remembers the sequence in the output that sets the modifyOtherKeys level. A full reset (`ESC c`) returns it to 0.
    ///
    /// ponytail: doesn't follow the vte state machine, only scans for ESC. A level changed inside a synchronized update (`?2026`) is applied
    /// before the screen, and the same bytes inside a DCS are counted too. Neither harms key encoding.
    fn track_modify_other_keys(&mut self, bytes: &[u8]) {
        let mut i = 0;
        if !self.mok_tail.is_empty() {
            // The straddling start is joined only with the first `MOK_MAX` bytes of the next chunk. The whole chunk is not copied.
            // The start has no ESC other than the first, so if it isn't a sequence, the chunk can just be scanned from the beginning.
            let tail = std::mem::take(&mut self.mok_tail);
            let joined = [tail.as_slice(), &bytes[..bytes.len().min(MOK_MAX)]].concat();
            match mok_seq(&joined) {
                Mok::Set(level, len) => {
                    self.modify_other_keys = level;
                    i = len - tail.len();
                }
                // Only when it is shorter than `MOK_MAX` is it a start, so the chunk has been joined in full.
                Mok::Partial => {
                    self.mok_tail = joined;
                    return;
                }
                Mok::No => {}
            }
        }
        while let Some(at) = bytes[i..].iter().position(|&b| b == 0x1b) {
            let at = i + at;
            match mok_seq(&bytes[at..]) {
                Mok::Set(level, len) => {
                    self.modify_other_keys = level;
                    i = at + len;
                }
                Mok::Partial => {
                    self.mok_tail = bytes[at..].to_vec();
                    return;
                }
                Mok::No => i = at + 1,
            }
        }
    }

    /// Makes or drops the copy of the hidden main screen to match whether the screen is now the alternate screen.
    fn track_alt(&mut self) {
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        match (alt, self.primary.is_some()) {
            (true, false) => self.primary = Some(self.copy_primary()),
            (false, true) => self.primary = None,
            _ => {}
        }
    }

    /// Answers to the terminal queries in the output fed so far (bytes to write to the PTY). The answers taken out are cleared.
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

    /// The color to answer a color query with. A color the app set comes first, otherwise the default foreground or background. Other color numbers are not answered.
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

    /// If a synchronized update (`CSI ? 2026 h`) is in progress, the time that update must end by. If it hasn't ended by then, call `end_sync`.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Ends an unfinished synchronized update and applies the output collected to the screen. Keeps the session from getting stuck on a frozen screen.
    pub fn end_sync(&mut self) {
        self.parser.stop_sync(&mut self.term);
        self.track_alt();
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let size = Size::new(cols, rows);
        self.term.resize(size);
        if let Some(primary) = &mut self.primary {
            // Re-fit the line wrapping the same way `Term` does for the inactive main screen.
            primary.resize(true, size.rows, size.cols);
        }
    }

    /// Scrollback + visible cells (with SGR) + modes + cursor. Feeding it to a new `Screen` of the same size brings the screen back.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut w = Writer::new();
        // `2J` pushes one blank line into scrollback even on an empty main screen, so `3J` clears the scrollback.
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
            // The background color of the alternate screen, which is cleared on entry, was tied to the pen at that time. Clear it again with the default color and draw.
            w.plain();
            w.out.push_str("\x1b[2J");
        }
        w.grid(grid);
        w.modes(mode, self.modify_other_keys);
        w.cursor(grid);
        w.pen(&grid.cursor.template);
        w.out.into_bytes()
    }

    /// Visible lines. Trailing whitespace is trimmed. It is the real screen regardless of the scrolled position.
    pub fn lines(&self) -> Vec<String> {
        let grid = self.term.grid();
        (0..grid.screen_lines())
            .map(|line| row_text(&grid[Line(line as i32)], grid.columns()))
            .collect()
    }

    /// Scrollback + visible lines, oldest first.
    pub fn history_text(&self) -> Vec<String> {
        let grid = self.term.grid();
        (grid.topmost_line().0..=grid.bottommost_line().0)
            .map(|line| row_text(&grid[Line(line)], grid.columns()))
            .collect()
    }

    /// `(col, row)`, 0-based.
    pub fn cursor(&self) -> (u16, u16) {
        let point = self.term.grid().cursor.point;
        (point.column.0 as u16, point.line.0 as u16)
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// The xterm modifyOtherKeys level the inner app last set (0-2).
    pub fn modify_other_keys(&self) -> u8 {
        self.modify_other_keys
    }

    /// The kitty keyboard flags that are on (1 disambiguate keys, 2 event types, 4 alternate keys, 8 all keys as escapes, 16 associated text).
    pub fn kitty_flags(&self) -> u8 {
        kitty_bits(self.mode())
    }

    /// Positive = toward the past.
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    pub fn term(&self) -> &Term<Replies> {
        &self.term
    }

    /// Now that the alternate screen has just opened, copies the hidden main screen.
    ///
    /// Swapping the main screen out briefly with `swap_alt` and back clears the alternate screen,
    /// so the alternate screen is copied beforehand and put back.
    fn copy_primary(&mut self) -> Grid<Cell> {
        let alt = self.term.grid().clone();
        self.term.swap_alt();
        let primary = self.term.grid().clone();
        self.term.swap_alt();
        *self.term.grid_mut() = alt;
        primary
    }
}

/// Cell attributes expressed in SGR. If they are the default, only `0` is emitted.
#[derive(Clone, Copy, PartialEq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
}

/// Flags the pen restores. Cell structure flags such as wide chars and line joins are left out.
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

    /// The SGR that sets this pen entirely. Earlier attributes are cleared with `0`.
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

/// The index 0-15 of a named color. `None` for the default-color family.
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

/// Writes one color as an SGR parameter (preceded by `;`). `kind` is 38 (foreground), 48 (background) or 58 (underline). The default color is not written.
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

/// Accumulates snapshot bytes, remembering the pen and hyperlink already set on the terminal so it emits only what changes.
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

    /// Matches the state to the cell's attributes and hyperlink. Does not write the character.
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
                // An automatically assigned id was not there originally, so it isn't emitted.
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

    /// Resets only the pen to default. A hyperlink continues across lines, so it is left as is.
    fn reset_pen(&mut self) {
        if self.pen != Pen::DEFAULT {
            self.out.push_str("\x1b[0m");
            self.pen = Pen::DEFAULT;
        }
    }

    /// Resets both the pen and the hyperlink to default.
    fn plain(&mut self) {
        self.pen(&Cell::default());
    }

    fn cell(&mut self, cell: &Cell) {
        // The trailing cell of a wide char, and the cell left behind when a wide char wraps to the next line, are made by the preceding char.
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

    /// Draws the scrollback and visible lines from the top. The last line ends up at the bottom, pushing earlier lines into scrollback.
    fn grid(&mut self, grid: &Grid<Cell>) {
        let cols = grid.columns();
        let bottom = grid.bottommost_line().0;
        self.out.push_str("\x1b[H");
        let mut after_wrap = false;
        for line in grid.topmost_line().0..=bottom {
            let row = &grid[Line(line)];
            // A line continued by a wrap is drawn to the end so auto-wrap carries on from it (this preserves the line-join flag).
            let wrapped = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            let mut len = if wrapped {
                cols
            } else {
                content_len(row, cols)
            };
            if after_wrap {
                // The pending wrap of the previous line is released only by writing a char. Even if the continued line is empty, the first cell is written.
                len = len.max(1);
            }
            // The empty slot at the end of a line is skipped only when the next line starts with that wide char (if the char is written, the client wraps it itself).
            // If the continued line was written again, fill with spaces to create the pending wrap. Otherwise the next line's first cell would be written at the end of this line.
            // A restored cell loses its empty-slot marker. This is a value where copying a continued line gets at most one extra space.
            let next_wide = line < bottom
                && grid[Line(line + 1)][Column(0)]
                    .flags
                    .contains(Flags::WIDE_CHAR);
            for col in 0..len {
                let cell = &row[Column(col)];
                if !next_wide && cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                    self.pen(cell);
                    self.out.push(' ');
                } else {
                    self.cell(cell);
                }
            }
            if after_wrap && len < cols {
                // Auto-wrap fills the new line with the background color of the first char's pen. Cells that weren't drawn are reset to default.
                self.reset_pen();
                self.out.push_str("\x1b[K");
            }
            if line < bottom && !wrapped {
                // The new line's background color follows the pen, so clear the pen before the line break.
                self.reset_pen();
                self.out.push_str("\r\n");
            }
            after_wrap = wrapped;
        }
    }

    /// Restores the cursor of the given screen.
    fn cursor(&mut self, grid: &Grid<Cell>) {
        let point = grid.cursor.point;
        let (row, mut col) = (point.line.0 + 1, point.column.0 + 1);
        if grid.cursor.input_needs_wrap {
            // The last cell was just written, so the next char wraps the line. Rewrite that cell to create the same pending state.
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

    fn modes(&mut self, mode: TermMode, modify_other_keys: u8) {
        for (flag, code) in MODES {
            let set = if mode.contains(flag) { 'h' } else { 'l' };
            let _ = write!(self.out, "\x1b[?{code}{set}");
        }
        self.out.push_str(if mode.contains(TermMode::APP_KEYPAD) {
            "\x1b="
        } else {
            "\x1b>"
        });
        let _ = write!(
            self.out,
            "\x1b[={};1u\x1b[>4;{modify_other_keys}m",
            kitty_bits(mode)
        );
    }
}

/// Whether output starting with `ESC` is a sequence that sets modifyOtherKeys.
enum Mok {
    /// Level and sequence length.
    Set(u8, usize),
    /// Only the start arrived.
    Partial,
    No,
}

/// Reads `CSI > 4 [; N] m` the way vte does. Empty and 0 are the default (1 for the first, 0 for the second), and from the third on is ignored.
/// A level outside 0-2 and a resource other than 4 (modifyCursorKeys, etc.) are ignored, like vte.
fn mok_seq(buf: &[u8]) -> Mok {
    const HEAD: &[u8] = b"\x1b[>";
    if buf.starts_with(b"\x1bc") {
        return Mok::Set(0, 2);
    }
    if buf.len() < HEAD.len() {
        return if HEAD.starts_with(buf) {
            Mok::Partial
        } else {
            Mok::No
        };
    }
    let Some(body) = buf.strip_prefix(HEAD) else {
        return Mok::No;
    };
    let Some(n) = body.iter().position(|c| !c.is_ascii_digit() && *c != b';') else {
        return if buf.len() < MOK_MAX {
            Mok::Partial
        } else {
            Mok::No
        };
    };
    if body[n] != b'm' {
        return Mok::No;
    }
    let mut params = body[..n].split(|&c| c == b';').map(|p| {
        std::str::from_utf8(p)
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|&v| v != 0)
    });
    let mut next = |default| params.next().flatten().unwrap_or(default);
    match (next(1), next(0)) {
        (4, level @ 0..=2) => Mok::Set(level as u8, HEAD.len() + n + 1),
        _ => Mok::No,
    }
}

/// DEC private modes that affect input interpretation. The mouse modes are mutually exclusive, as are 1005 and 1006,
/// so only the one that is on emits `h`, and the state ends up the same regardless of order.
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

/// Bits of the kitty keyboard protocol flags.
const KITTY_FLAGS: [(TermMode, u8); 5] = [
    (TermMode::DISAMBIGUATE_ESC_CODES, 1),
    (TermMode::REPORT_EVENT_TYPES, 2),
    (TermMode::REPORT_ALTERNATE_KEYS, 4),
    (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
    (TermMode::REPORT_ASSOCIATED_TEXT, 16),
];

/// Gathers the kitty keyboard flags on in `mode` into protocol bits.
fn kitty_bits(mode: TermMode) -> u8 {
    KITTY_FLAGS
        .iter()
        .filter(|(flag, _)| mode.contains(*flag))
        .map(|(_, bit)| bit)
        .sum()
}

/// Whether a link is safe to carry in OSC 8 as is (control characters could break the sequence).
fn link_is_plain(link: &Hyperlink) -> bool {
    !link.uri().chars().any(char::is_control) && !link.id().chars().any(char::is_control)
}

/// Turns a cell's char into one that is fine to emit as is. A tab cell (`\t`) moves the cursor, so it is written as a space.
fn printable(c: char) -> char {
    if c.is_control() { ' ' } else { c }
}

/// The line length without trailing blank cells that have no attributes.
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

    /// Checks that all cells including scrollback (attributes, even the line-join flags) are the same. If they differ, reports only the first mismatching cell.
    fn assert_same_cells(f: &Screen, s: &Screen) {
        let (gf, gs) = (f.term().grid(), s.term().grid());
        assert_eq!(
            (gf.topmost_line(), gf.columns(), gf.screen_lines()),
            (gs.topmost_line(), gs.columns(), gs.screen_lines()),
            "sizes differ"
        );
        for line in gs.topmost_line().0..=gs.bottommost_line().0 {
            for col in 0..gs.columns() {
                let (cf, cs) = (&gf[Line(line)][Column(col)], &gs[Line(line)][Column(col)]);
                assert_eq!(cf, cs, "row {line}, col {col}");
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
        // A link opened separately stays a different link even with the same address.
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
        // A 100-cell line, and a line that wraps onto a wide char that doesn't fit in the last cell.
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
        // A line fills 40 cells and wraps to the next, then the wrapped char is erased so the continued line is empty.
        let mut s = Screen::new(40, 10);
        s.feed(format!("{}b\x08 \x08\r\nnext", "a".repeat(40)).as_bytes());
        let f = rebuilt(&s);
        assert_eq!(f.lines(), s.lines());
        assert_eq!(f.lines()[1], "");
        assert_same_cells(&f, &s);

        // Visible lines don't shift even with scrollback.
        let mut s = Screen::new(40, 10);
        for i in 0..20 {
            s.feed(format!("l{i}\r\n").as_bytes());
        }
        s.feed(format!("{}b\r\x1b[K\r\nnext", "a".repeat(40)).as_bytes());
        let f = rebuilt(&s);
        assert_eq!(f.lines(), s.lines());
        assert_same_cells(&f, &s);

        // The line-join flag remains even if the empty continued line is the bottom line.
        let mut s = Screen::new(40, 10);
        s.feed(format!("\x1b[9;1H{}b\x08 \x08", "a".repeat(40)).as_bytes());
        let f = rebuilt(&s);
        assert_same_cells(&f, &s);
        assert_eq!(f.cursor(), s.cursor());
    }

    /// Checks that the chars of every line and the line-join flag (`WRAPLINE` on the last cell) are the same.
    fn assert_same_rows_and_wraps(f: &Screen, s: &Screen) {
        assert_eq!(f.lines(), s.lines());
        let (gf, gs) = (f.term().grid(), s.term().grid());
        assert_eq!(
            gf.topmost_line(),
            gs.topmost_line(),
            "scrollback lengths differ"
        );
        let last = Column(gs.columns() - 1);
        for line in gs.topmost_line().0..=gs.bottommost_line().0 {
            assert_eq!(
                gf[Line(line)][last].flags.contains(Flags::WRAPLINE),
                gs[Line(line)][last].flags.contains(Flags::WRAPLINE),
                "line-join flag of row {line}"
            );
        }
    }

    #[test]
    fn snapshot_keeps_rows_after_leading_wide_spacer_when_next_row_was_rewritten() {
        // A wide char that didn't fit in the last cell wraps to the next line and leaves an empty slot at the end of the previous line, then
        // the continued line is erased and that char disappears. The line below must not shift up by one.
        let mut s = Screen::new(40, 10);
        s.feed(format!("{}한\r\x1b[K\r\nnext", "a".repeat(39)).as_bytes());
        assert_eq!(s.lines()[2], "next");
        let f = rebuilt(&s);
        assert_same_rows_and_wraps(&f, &s);
        assert_eq!(f.lines()[2], "next");
        assert_eq!(f.cursor(), s.cursor());

        // After erasing the continued line, even if another char is written, both the empty-slot marker and the line-join flag remain.
        // Checks the same branch as the first case when the continued line is not empty.
        let mut s = Screen::new(40, 10);
        s.feed(format!("{}한\r\x1b[K", "a".repeat(39)).as_bytes());
        s.feed(b"x\r\nnext");
        let flags = s.term().grid()[Line(0)][Column(39)].flags;
        assert!(flags.contains(Flags::LEADING_WIDE_CHAR_SPACER | Flags::WRAPLINE));
        let f = rebuilt(&s);
        assert_same_rows_and_wraps(&f, &s);

        // Overwriting a wide char without erasing it makes alacritty clear the empty-slot marker, leaving a space that has only the line-join flag.
        // In this case even the cells are restored identically.
        let mut s = Screen::new(40, 10);
        s.feed(format!("{}한\rx\r\nnext", "a".repeat(39)).as_bytes());
        let flags = s.term().grid()[Line(0)][Column(39)].flags;
        assert!(flags.contains(Flags::WRAPLINE));
        assert!(!flags.contains(Flags::LEADING_WIDE_CHAR_SPACER));
        let f = rebuilt(&s);
        assert_same_cells(&f, &s);
        assert_same_rows_and_wraps(&f, &s);

        // Visible lines don't shift even with scrollback.
        let mut s = Screen::new(40, 10);
        for i in 0..20 {
            s.feed(format!("l{i}\r\n").as_bytes());
        }
        s.feed(format!("{}한\r\x1b[K\r\nnext", "a".repeat(39)).as_bytes());
        let f = rebuilt(&s);
        assert_same_rows_and_wraps(&f, &s);

        // If the continued line still starts with that wide char, even the cells are restored identically as now.
        let mut s = Screen::new(40, 10);
        s.feed(format!("{}한글\r\nend", "a".repeat(39)).as_bytes());
        let f = rebuilt(&s);
        assert_same_cells(&f, &s);
        assert_same_rows_and_wraps(&f, &s);
    }

    #[test]
    fn snapshot_wrap_does_not_leak_background() {
        // The first cell's pen background color on a continued line must not spread over the whole new line through auto-wrap.
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
        // Output that follows comes out with the same pen and link.
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
    fn tracks_modify_other_keys_across_split_feeds() {
        let all = b"ab\x1b[>4;2mcd";
        for cut in 0..=all.len() {
            let mut s = Screen::new(40, 10);
            s.feed(&all[..cut]);
            s.feed(&all[cut..]);
            assert_eq!(s.modify_other_keys(), 2, "cut {cut}");
        }
        let mut s = Screen::new(40, 10);
        s.feed(b"\x1b[>4;1m");
        assert_eq!(s.modify_other_keys(), 1);
        // Without a level it is 0 (same as vte).
        s.feed(b"\x1b[>4m");
        assert_eq!(s.modify_other_keys(), 0);
        // Other `CSI > … m` (modifyCursorKeys, etc.) and SGR don't change the level.
        s.feed(b"\x1b[>4;2m\x1b[>1;0m\x1b[4;0m\x1b[>4;9m");
        assert_eq!(s.modify_other_keys(), 2);
        // A full reset (RIS) returns it to 0. This too can straddle a read boundary.
        s.feed(b"\x1b");
        s.feed(b"c");
        assert_eq!(s.modify_other_keys(), 0);
        // Even if a long chunk comes after the straddling start, it is scanned to the end.
        let long = "x".repeat(5000);
        s.feed(b"\x1b[>4;");
        s.feed(format!("2m{long}\x1b[>4;1m").as_bytes());
        assert_eq!(s.modify_other_keys(), 1);
        s.feed(b"\x1b[>");
        s.feed(format!("{long}\x1b[>4;2m{long}").as_bytes());
        assert_eq!(s.modify_other_keys(), 2);
    }

    #[test]
    fn snapshot_restores_keyboard_modes() {
        let mut s = Screen::new(40, 10);
        s.feed(b"\x1b[>4;2m\x1b[>13u");
        assert_eq!(s.kitty_flags(), 13);
        let f = rebuilt(&s);
        assert_eq!(f.modify_other_keys(), 2);
        assert_eq!(f.kitty_flags(), 13);
        // The off state is restored too.
        let mut s = Screen::new(40, 10);
        s.feed(b"\x1b[>4;2m\x1b[>4;0m");
        let mut f = Screen::new(40, 10);
        f.feed(b"\x1b[>4;1m");
        f.feed(&s.snapshot());
        assert_eq!(f.modify_other_keys(), 0);
    }

    #[test]
    fn snapshot_in_alt_screen_keeps_primary() {
        let mut s = Screen::new(40, 10);
        for i in 0..30 {
            s.feed(format!("before {i}\r\n").as_bytes());
        }
        s.feed(b"\x1b[31mprompt$ \x1b[0m\x1b[?1049h\x1b[2;3Halt screen\x1b[7mbar");
        // The alternate screen wasn't touched by the copying.
        assert_eq!(s.lines()[1], "  alt screenbar");
        let mut f = rebuilt(&s);
        assert!(f.mode().contains(TermMode::ALT_SCREEN));
        assert_same_cells(&f, &s);
        assert_eq!(f.cursor(), s.cursor());
        // Leaving the alternate screen brings back the hidden main screen and scrollback.
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
        // If it leaves and re-enters within the bytes that arrived at once, the text written on the main screen in between must be in the copy.
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
        // Answers with the color the app changed via OSC.
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
        // A finished synchronized update has no deadline.
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
