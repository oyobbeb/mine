//! tmux-style copy mode. Moves around the lines frozen at start (scrollback + visible lines) with the keyboard to select and search.
//! A pure state machine, so it does no I/O. Only syncing with the frozen screen (`reveal`, `highlight`) reads the client screen.
//!
//! Coordinates are a line number (index into `lines`, 0 being the oldest line) and a character position (a `char` index, neither a byte nor a cell).

use alacritty_terminal::grid::{Dimensions, Grid, Row};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};

use super::input::HANGUL_HINT;
use mine_core::screen::Screen;
use mine_core::search::find;
use mine_core::select::Selection;

/// The result of consuming one key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyOut {
    Stay,
    /// Ends copy mode.
    Exit,
    /// Copies this text and ends.
    Copy(String),
}

#[derive(Clone, Debug)]
pub struct CopyMode {
    pub line: usize,
    pub col: usize,
    /// The start point of a selection begun with `v`. The selection runs up to the cursor.
    pub anchor: Option<(usize, usize)>,
    /// The last confirmed search query. `n`, `N` and Enter on empty input repeat it.
    pub query: String,
    /// The direction of the last confirmed search. `/` is forward (down), `?` is backward (up).
    pub forward: bool,
    /// A search query is being typed after `/` or `?`.
    pub typing: bool,
    /// Hangul arrived where a command key goes and nothing was done. Until the next key or wheel, the status line tells the user to switch to English.
    pub hangul: bool,
    /// The query being typed. `query` isn't touched until Enter confirms it, so Esc leaves the previous search as it was.
    pub input: String,
    /// The direction of the search being typed. It becomes `forward` once Enter confirms it.
    typing_forward: bool,
    /// Lines per page (the number of lines in the terminal pane). `freeze` sets it from the screen's line count.
    /// Page keys (PgUp, PgDn, ⌃b, ⌃f) move the cursor by this much, half-page keys (⌃u, ⌃d) by half of it.
    pub page: usize,
    /// The number of lines the cursor has moved by page keys or the wheel that hasn't been reflected on screen yet (positive is down). `reveal` moves the screen by the same number of lines and clears it.
    drift: i32,
    /// The search just made found no match. Cleared on the next key.
    pub miss: bool,
    /// The leading bytes of a UTF-8 character that was only partly entered into the query.
    pending: Vec<u8>,
    /// For a line that continues onto the next line by wrapping, the number of right-hand spaces cut off from its line count. `None` for a line that doesn't continue.
    /// When copying, such a line gets its spaces back and is joined to the next line without a `\n`.
    wraps: Vec<Option<usize>>,
}

/// The number of characters in this line.
fn len_at(lines: &[String], line: usize) -> usize {
    lines.get(line).map_or(0, |l| l.chars().count())
}

/// From the `from`-th character of a line through the `to`-th (inclusive), or to the end of the line if none. Counted in character positions, not bytes.
fn slice(line: &str, from: usize, to: Option<usize>) -> String {
    let rest = line.chars().skip(from);
    match to {
        Some(to) => rest.take(to + 1 - from).collect(),
        None => rest.collect(),
    }
}

impl CopyMode {
    /// Starts at the beginning of the last line that has any characters. Starting on a blank line at the bottom of the screen would leave the cursor floating in the air.
    /// If all lines are empty, it's the bottom line.
    pub fn new(lines: &[String]) -> Self {
        let line = lines
            .iter()
            .rposition(|l| !l.is_empty())
            .unwrap_or(lines.len().saturating_sub(1));
        CopyMode {
            line,
            col: 0,
            anchor: None,
            query: String::new(),
            forward: true,
            typing: false,
            hangul: false,
            input: String::new(),
            typing_forward: true,
            page: 1,
            drift: 0,
            miss: false,
            pending: Vec::new(),
            wraps: Vec::new(),
        }
    }

    pub fn key(&mut self, lines: &[String], b: u8) -> CopyOut {
        self.miss = false;
        self.hangul = false;
        if self.typing {
            self.type_key(lines, b);
            return CopyOut::Stay;
        }
        match b {
            b'q' | 0x1b => return CopyOut::Exit,
            b'h' => self.col = self.col.saturating_sub(1),
            b'l' => self.col = (self.col + 1).min(len_at(lines, self.line).saturating_sub(1)),
            b'j' => self.go(lines, self.line + 1),
            b'k' => self.go(lines, self.line.saturating_sub(1)),
            b'0' => self.col = 0,
            b'$' => self.col = len_at(lines, self.line).saturating_sub(1),
            b'g' => (self.line, self.col) = (0, 0),
            b'G' => (self.line, self.col) = (lines.len().saturating_sub(1), 0),
            // A page key (PgUp and PgDn come in as ⌃b and ⌃f via `split`). A half page moves at least one line.
            0x02 => self.move_lines(lines, -(self.page as isize)),
            0x06 => self.move_lines(lines, self.page as isize),
            0x15 => self.move_lines(lines, -((self.page / 2).max(1) as isize)),
            0x04 => self.move_lines(lines, (self.page / 2).max(1) as isize),
            b'v' => self.anchor = self.anchor.is_none().then_some((self.line, self.col)),
            b'/' | b'?' => {
                self.typing = true;
                self.typing_forward = b == b'/';
                self.input.clear();
                self.pending.clear();
            }
            b'n' => self.step(lines, self.forward),
            b'N' => self.step(lines, !self.forward),
            b'y' | b'\r' | b'\n' => return self.yank(lines),
            _ => {}
        }
        CopyOut::Stay
    }

    /// Arrow keys (`A`–`D`). Does nothing while a search query is being typed.
    pub fn arrow(&mut self, lines: &[String], dir: u8) -> CopyOut {
        if self.typing {
            return CopyOut::Stay;
        }
        let vi = match dir {
            b'A' => b'k',
            b'B' => b'j',
            b'C' => b'l',
            _ => b'h',
        };
        self.key(lines, vi)
    }

    /// Moves to `line` (the last line if it's past it) and clamps `col` into that line.
    fn go(&mut self, lines: &[String], line: usize) {
        self.line = line.min(lines.len().saturating_sub(1));
        self.col = self.col.min(len_at(lines, self.line).saturating_sub(1));
    }

    /// Moves the cursor by `delta` lines (positive is down). Stops at the start and end, and clamps `col` into the new line.
    /// The screen follows by the number of lines `reveal` moved, so the cursor stays on the same line of the screen.
    /// Also clears the notice here so "No matches" goes away even on a wheel scroll that doesn't pass through a key.
    pub fn move_lines(&mut self, lines: &[String], delta: isize) {
        self.miss = false;
        self.hangul = false;
        let from = self.line;
        self.go(lines, self.line.saturating_add_signed(delta));
        self.drift += self.line as i32 - from as i32;
    }

    /// A key while a search query is being typed. Esc only cancels the input and stays in copy mode.
    fn type_key(&mut self, lines: &[String], b: u8) {
        match b {
            // Only fold up the input. The previous query and direction are unchanged, so `n` and `N` continue from them.
            0x1b => {
                self.typing = false;
                self.input.clear();
                self.pending.clear();
            }
            b'\r' | b'\n' => {
                self.typing = false;
                self.pending.clear();
                self.search(lines);
            }
            // If there is a partly arrived character, discard only that fragment.
            0x7f | 0x08 => {
                if self.pending.is_empty() {
                    self.input.pop();
                } else {
                    self.pending.clear();
                }
            }
            0x00..=0x1f => {}
            _ => {
                self.pending.push(b);
                match std::str::from_utf8(&self.pending) {
                    Ok(text) => {
                        self.input.push_str(text);
                        self.pending.clear();
                    }
                    // The character hasn't fully arrived yet. Wait for the next byte.
                    Err(e) if e.error_len().is_none() => {}
                    Err(_) => self.pending.clear(),
                }
            }
        }
    }

    /// Enter: confirms the typed query and goes to the first match at or before/after the cursor position, depending on direction.
    /// If there is none to the end, it wraps around to the opposite end and searches there. If the input is empty, it repeats the previous query in the new direction,
    /// and this time it goes to the next match after the cursor like `n` (if the cursor position counted as a match, it would stay in place).
    fn search(&mut self, lines: &[String]) {
        let forward = self.typing_forward;
        let input = std::mem::take(&mut self.input);
        self.forward = forward;
        if input.is_empty() {
            return self.step(lines, forward);
        }
        self.query = input;
        let hits = find(lines, &self.query);
        let here = (self.line, self.col);
        let hit = if forward {
            hits.iter().find(|&&p| p >= here).or(hits.first())
        } else {
            hits.iter().rfind(|&&p| p <= here).or(hits.last())
        };
        self.jump(hit.copied());
    }

    /// `n` (forward) and `N` (backward): go to the next or previous match after the cursor. At the end, wrap around to the opposite end.
    fn step(&mut self, lines: &[String], forward: bool) {
        let hits = find(lines, &self.query);
        let here = (self.line, self.col);
        let hit = if forward {
            hits.iter().find(|&&p| p > here).or(hits.first())
        } else {
            hits.iter().rfind(|&&p| p < here).or(hits.last())
        };
        self.jump(hit.copied());
    }

    /// Moves to a match. If there is none, leaves the cursor where it is and notifies (if the query is empty, there's nothing to notify).
    fn jump(&mut self, hit: Option<(usize, usize)>) {
        match hit {
            Some((line, col)) => (self.line, self.col) = (line, col),
            None => self.miss = !self.query.is_empty(),
        }
    }

    /// `y` and Enter: copies the selection if there is one, otherwise ends without copying, like tmux.
    fn yank(&self, lines: &[String]) -> CopyOut {
        let Some(anchor) = self.anchor else {
            return CopyOut::Exit;
        };
        let (from, to) = (
            anchor.min((self.line, self.col)),
            anchor.max((self.line, self.col)),
        );
        let mut out = String::new();
        for i in from.0..=to.0 {
            let text = lines.get(i).map_or("", String::as_str);
            let start = if i == from.0 { from.1 } else { 0 };
            out.push_str(&slice(text, start, (i == to.0).then_some(to.1)));
            if i < to.0 {
                match self.wraps.get(i).copied().flatten() {
                    // A line continued by wrapping: the trailing spaces are characters too, and join straight onto the next line.
                    Some(pad) => out.extend(std::iter::repeat_n(' ', pad)),
                    None => out.push('\n'),
                }
            }
        }
        CopyOut::Copy(out)
    }

    /// The text to show in the status line. While a search query is being typed, it's that input.
    pub fn status(&self, total: usize) -> String {
        if self.typing {
            let prompt = if self.typing_forward { '/' } else { '?' };
            return format!("{prompt}{}", self.input);
        }
        let hint = if self.hangul {
            HANGUL_HINT
        } else if self.miss {
            "No matches"
        } else {
            "v select · y copy · / ? search · q quit"
        };
        format!("Copy mode │ {}/{total} │ {hint}", self.line + 1)
    }

    /// Scrolls the client screen so the cursor line is visible. For lines the cursor moved by page keys or the wheel, the screen follows by the same number of lines;
    /// otherwise it leaves things as they are if already visible, and moves only as far as the edge if off screen.
    pub fn reveal(&mut self, screen: &mut Screen) {
        // Screen scrolling is positive upward, so its sign is opposite to the cursor's. If blocked at the end of the scrollback, adjust so it's visible from below.
        screen.scroll(-std::mem::take(&mut self.drift));
        let (row, rows) = {
            let grid = screen.term().grid();
            (view_row(grid, self.line), grid.screen_lines() as i64)
        };
        let delta = if row < 0 {
            -row
        } else if row >= rows {
            rows - 1 - row
        } else {
            0
        };
        screen.scroll(delta as i32);
    }

    /// The area to paint inverted over the currently visible screen. If there is no selection, it's the one cursor cell.
    /// If the selection runs off screen, it's clipped at the first and last visible cells; if it's entirely off screen, `None`.
    pub fn highlight(&self, screen: &Screen) -> Option<Selection> {
        let grid = screen.term().grid();
        let (rows, cols) = (grid.screen_lines() as i64, grid.columns());
        let cursor = (self.line, self.col);
        let (from, to) = match self.anchor {
            Some(anchor) => (anchor.min(cursor), anchor.max(cursor)),
            None => (cursor, cursor),
        };
        let (top, bottom) = (view_row(grid, from.0), view_row(grid, to.0));
        if bottom < 0 || top >= rows {
            return None;
        }
        let cell = |(line, col): (usize, usize), row: i64| {
            let x = grid_col(&grid[Line(line_of(grid, line))], cols, col);
            (x as u16, row as u16)
        };
        let start = if top < 0 { (0, 0) } else { cell(from, top) };
        let end = if bottom >= rows {
            ((cols - 1) as u16, (rows - 1) as u16)
        } else {
            cell(to, bottom)
        };
        Some(Selection { start, end })
    }
}

/// The grid line number for a frozen line number. Scrollback is negative.
fn line_of(grid: &Grid<Cell>, line: usize) -> i32 {
    line as i32 - grid.history_size() as i32
}

/// Which line of the currently visible screen a frozen line number is at the current scroll position. Off screen it is negative or at least the screen's line count.
fn view_row(grid: &Grid<Cell>, line: usize) -> i64 {
    i64::from(line_of(grid, line)) + grid.display_offset() as i64
}

/// The cell where the `char_idx`-th character (combining characters included) of a line sits. Counted the same way as `history_text`: cells after a wide character are skipped,
/// and combining characters stacked on one cell are counted one character each.
fn grid_col(row: &Row<Cell>, cols: usize, char_idx: usize) -> usize {
    let mut seen = 0;
    for x in 0..cols {
        let cell = &row[Column(x)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        seen += 1 + cell.zerowidth().map_or(0, <[char]>::len);
        if char_idx < seen {
            return x;
        }
    }
    cols - 1
}

/// The lines to freeze when starting copy mode (scrollback + visible lines) and the `CopyMode` that starts on that line.
/// The lines are exactly `Screen::history_text`'s, one screen line per line. Lines continued by wrapping are recorded separately in `wraps`.
/// A page (`page`) is the screen's line count, i.e. the number of lines in the terminal pane. It's set here so forgetting to set it separately doesn't make it page one line at a time.
pub fn freeze(screen: &Screen) -> (Vec<String>, CopyMode) {
    let lines = screen.history_text();
    let mut mode = CopyMode::new(&lines);
    mode.page = screen.term().grid().screen_lines();
    mode.wraps = wrap_pads(screen, &lines);
    (lines, mode)
}

/// For each line of `lines` (`history_text`), if it continued onto the next line by auto-wrap, returns the number of right-hand spaces `history_text` stripped.
/// Whether it continued is judged by `WRAPLINE` on the last cell of the line, the same as `mine_core::select::selected_text`.
fn wrap_pads(screen: &Screen, lines: &[String]) -> Vec<Option<usize>> {
    let grid = screen.term().grid();
    let cols = grid.columns();
    (grid.topmost_line().0..=grid.bottommost_line().0)
        .zip(lines)
        .map(|(line, text)| {
            let row = &grid[Line(line)];
            if !row[Column(cols - 1)].flags.contains(Flags::WRAPLINE) {
                return None;
            }
            // Subtract the remaining character count from the character count, counted the same way as `history_text`.
            let full: usize = (0..cols)
                .map(|x| &row[Column(x)])
                .filter(|cell| {
                    !cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                })
                .map(|cell| 1 + cell.zerowidth().map_or(0, <[char]>::len))
                .sum();
            Some(full.saturating_sub(text.chars().count()))
        })
        .collect()
}

/// A key extracted from a chunk of input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Byte(u8),
    /// The final byte of an arrow, `A` (up), `B` (down), `C` (right), `D` (left).
    Arrow(u8),
    /// `ESC [ 5 ~`.
    PageUp,
    /// `ESC [ 6 ~`.
    PageDown,
}

/// Splits an input chunk that was about to go to the session into keys. Returns, with each key, the byte position where that key ends (including sequences discarded before it).
/// Used to pass `bytes[end..]` after the key that ended copy mode to the session.
///
/// An arrow (`ESC [ A`, `ESC O A`, etc.) is `Arrow`, PgUp and PgDn are `PageUp` and `PageDown`, a lone Esc is `Byte(0x1b)`,
/// and any other escape sequence (other CSI and SS3, Alt keys) is discarded whole. If Esc comes twice in a row, that's two Esc keys.
/// Characters are one byte each.
pub fn split(bytes: &[u8]) -> Vec<(Key, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        if b != 0x1b {
            out.push((Key::Byte(b), i));
            continue;
        }
        let Some(&lead) = bytes.get(i) else {
            out.push((Key::Byte(0x1b), i));
            break;
        };
        // If Esc follows Esc, the earlier one is a lone Esc key. The later one is read in the next round.
        if lead == 0x1b {
            out.push((Key::Byte(0x1b), i));
            continue;
        }
        i += 1;
        // CSI (`[`) ends with a byte in 0x40..=0x7e and SS3 (`O`) with one byte. Anything else is an Alt key, so just one is discarded.
        let end = match lead {
            b'[' => bytes[i..]
                .iter()
                .position(|c| (0x40..=0x7e).contains(c))
                .map(|n| i + n),
            b'O' => (i < bytes.len()).then_some(i),
            _ => continue,
        };
        let Some(end) = end else { break };
        // `ESC [ 5 ; 2 ~` with a modifier is a page key too. Only the first parameter is looked at.
        let first = bytes[i..end].split(|&c| c == b';').next();
        let key = match (bytes[end], first) {
            (b'A'..=b'D', _) => Some(Key::Arrow(bytes[end])),
            (b'~', Some(b"5")) if lead == b'[' => Some(Key::PageUp),
            (b'~', Some(b"6")) if lead == b'[' => Some(Key::PageDown),
            _ => None,
        };
        i = end + 1;
        if let Some(key) = key {
            out.push((key, i));
        }
    }
    out
}

/// The three forms of an Esc press without modifiers that arrive via the kitty keyboard protocol (`CSI 27 u`, `CSI 27;1 u`, `CSI 27;1:1 u`).
/// If the session has that protocol on, Esc arrives like this right after leaving copy mode (modifyOtherKeys sends Esc as is).
const KITTY_ESC: [&[u8]; 3] = [b"\x1b[27u", b"\x1b[27;1u", b"\x1b[27;1:1u"];

/// After copy mode is ended with Esc, strips the lone Esc at the very front of the remaining bytes. Many people press Esc twice quickly to get out,
/// and if the second Esc went to the session, in Claude Code it would interrupt the turn, and Esc Esc opens rewind.
/// A lone Esc is a bare Esc byte or the kitty-encoded Esc press without modifiers (`KITTY_ESC`).
/// Other sequences of CSI (`ESC [`) and SS3 (`ESC O`) are kept, as is an Esc that has a modifier or is a release or repeat event.
/// An Esc that isn't at the very front is left alone too.
pub fn drop_lone_esc(mut rest: &[u8]) -> &[u8] {
    loop {
        if let Some(tail) = KITTY_ESC.iter().find_map(|esc| rest.strip_prefix(*esc)) {
            rest = tail;
            continue;
        }
        match rest.split_first() {
            Some((&0x1b, tail)) if !matches!(tail.first(), Some(b'[' | b'O')) => rest = tail,
            _ => return rest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn typed(c: &mut CopyMode, ls: &[String], s: &str) {
        for b in s.bytes() {
            c.key(ls, b);
        }
    }

    /// `/query` Enter.
    fn search(c: &mut CopyMode, ls: &[String], query: &str) {
        c.key(ls, b'/');
        typed(c, ls, query);
        c.key(ls, b'\r');
    }

    fn at(c: &CopyMode) -> (usize, usize) {
        (c.line, c.col)
    }

    #[test]
    fn n_wraps_around() {
        let ls = l(&["foo", "bar", "foo x foo"]);
        let mut c = CopyMode::new(&ls);
        assert_eq!(at(&c), (2, 0));
        // The cursor position is a match too.
        search(&mut c, &ls, "foo");
        assert_eq!(at(&c), (2, 0));
        c.key(&ls, b'n');
        assert_eq!(at(&c), (2, 6));
        // From the last match, n wraps back to the first match.
        c.key(&ls, b'n');
        assert_eq!(at(&c), (0, 0));
        c.key(&ls, b'n');
        assert_eq!(at(&c), (2, 0));
    }

    #[test]
    fn capital_n_wraps_backwards() {
        let ls = l(&["foo", "bar", "foo x foo"]);
        let mut c = CopyMode::new(&ls);
        search(&mut c, &ls, "foo");
        c.key(&ls, b'N');
        assert_eq!(at(&c), (0, 0));
        // From the first match, N wraps back to the last match.
        c.key(&ls, b'N');
        assert_eq!(at(&c), (2, 6));
        c.key(&ls, b'N');
        assert_eq!(at(&c), (2, 0));
    }

    #[test]
    fn search_wraps_to_the_first_match_when_none_follow_the_cursor() {
        let ls = l(&["x", "foo", "tail"]);
        let mut c = CopyMode::new(&ls);
        search(&mut c, &ls, "FOO");
        assert_eq!(at(&c), (1, 0));
        assert!(!c.typing);
    }

    #[test]
    fn failed_search_stays_put_and_says_so_until_the_next_key() {
        let ls = l(&["alpha", "beta"]);
        let mut c = CopyMode::new(&ls);
        c.key(&ls, b'g');
        search(&mut c, &ls, "zzz");
        assert_eq!(at(&c), (0, 0));
        assert!(c.miss && !c.typing);
        assert!(c.status(2).contains("No matches"));
        // The same if n finds nothing either, and pressing another key clears the notice.
        c.key(&ls, b'n');
        assert!(c.miss);
        c.key(&ls, b'j');
        assert!(!c.miss);
        assert!(!c.status(2).contains("No matches"));
    }

    #[test]
    fn v_then_y_copies_range() {
        let ls = l(&["alpha", "beta"]);
        let mut c = CopyMode::new(&ls);
        for b in b"gv" {
            c.key(&ls, *b);
        }
        c.key(&ls, b'j');
        c.key(&ls, b'l');
        assert_eq!(c.key(&ls, b'y'), CopyOut::Copy("alpha\nbe".into()));
    }

    #[test]
    fn selection_is_normalised_whichever_end_comes_first() {
        let ls = l(&["alpha", "beta"]);
        let mut c = CopyMode::new(&ls);
        // The cursor is above and to the left of the selection start.
        typed(&mut c, &ls, "Gllvkh");
        assert_eq!((c.anchor, at(&c)), (Some((1, 2)), (0, 1)));
        assert_eq!(c.key(&ls, b'y'), CopyOut::Copy("lpha\nbet".into()));
        // A range within one line includes both end characters.
        let mut c = CopyMode::new(&ls);
        typed(&mut c, &ls, "glvll");
        assert_eq!(c.key(&ls, b'y'), CopyOut::Copy("lph".into()));
        // Middle lines are included whole.
        let ls = l(&["one", "two", "three"]);
        let mut c = CopyMode::new(&ls);
        typed(&mut c, &ls, "gvjjl");
        assert_eq!(c.key(&ls, b'\r'), CopyOut::Copy("one\ntwo\nth".into()));
    }

    #[test]
    fn v_toggles_the_anchor() {
        let ls = l(&["alpha"]);
        let mut c = CopyMode::new(&ls);
        c.key(&ls, b'v');
        assert_eq!(c.anchor, Some((0, 0)));
        c.key(&ls, b'v');
        assert_eq!(c.anchor, None);
    }

    #[test]
    fn y_or_enter_without_an_anchor_exits_without_copying() {
        let ls = l(&["alpha"]);
        let mut c = CopyMode::new(&ls);
        assert_eq!(c.key(&ls, b'y'), CopyOut::Exit);
        assert_eq!(c.key(&ls, b'\r'), CopyOut::Exit);
    }

    #[test]
    fn q_exits() {
        let ls = l(&["alpha"]);
        assert_eq!(CopyMode::new(&ls).key(&ls, b'q'), CopyOut::Exit);
        assert_eq!(CopyMode::new(&ls).key(&ls, 0x1b), CopyOut::Exit);
    }

    #[test]
    fn esc_while_typing_cancels_the_query_only() {
        let ls = l(&["alpha"]);
        let mut c = CopyMode::new(&ls);
        c.key(&ls, b'/');
        typed(&mut c, &ls, "ab");
        assert!(c.typing);
        assert_eq!(c.key(&ls, 0x1b), CopyOut::Stay);
        assert!(!c.typing && c.input.is_empty() && c.query.is_empty());
        // When not typing, Esc ends. While typing, q is a character too.
        c.key(&ls, b'/');
        assert_eq!(c.key(&ls, b'q'), CopyOut::Stay);
        assert_eq!(c.input, "q");
        c.key(&ls, 0x1b);
        assert_eq!(c.key(&ls, 0x1b), CopyOut::Exit);
    }

    #[test]
    fn korean_query_typed_byte_by_byte() {
        let ls = l(&["hello", "안녕 세계", "끝"]);
        let mut c = CopyMode::new(&ls);
        c.key(&ls, b'/');
        let bytes = "세계".as_bytes();
        // 세 is 3 bytes. It isn't added to the query until all of it has arrived.
        c.key(&ls, bytes[0]);
        c.key(&ls, bytes[1]);
        assert_eq!(c.input, "");
        for b in &bytes[2..] {
            c.key(&ls, *b);
        }
        assert_eq!(c.input, "세계");
        c.key(&ls, b'\r');
        assert_eq!((at(&c), c.query.as_str()), ((1, 3), "세계"));
        assert_eq!(
            c.status(3),
            "Copy mode │ 2/3 │ v select · y copy · / ? search · q quit"
        );
    }

    #[test]
    fn backspace_deletes_the_last_char_or_a_half_typed_one() {
        let ls = l(&["alpha"]);
        let mut c = CopyMode::new(&ls);
        c.key(&ls, b'/');
        typed(&mut c, &ls, "a한");
        assert_eq!(c.input, "a한");
        c.key(&ls, 0x7f);
        assert_eq!(c.input, "a");
        // If only the leading byte of 한 arrives and then it's erased, only that fragment is discarded.
        c.key(&ls, "한".as_bytes()[0]);
        c.key(&ls, 0x08);
        assert_eq!(c.input, "a");
        c.key(&ls, 0x7f);
        c.key(&ls, 0x7f);
        assert_eq!(c.input, "");
    }

    #[test]
    fn arrows_move_but_are_not_typed_into_the_query() {
        let ls = l(&["alpha", "beta"]);
        let mut c = CopyMode::new(&ls);
        c.arrow(&ls, b'D');
        c.arrow(&ls, b'C');
        c.arrow(&ls, b'A');
        assert_eq!(at(&c), (0, 1));
        c.arrow(&ls, b'B');
        assert_eq!(at(&c), (1, 1));
        c.key(&ls, b'/');
        c.arrow(&ls, b'A');
        c.arrow(&ls, b'B');
        assert_eq!(c.input, "");
        assert_eq!(at(&c), (1, 1));
    }

    #[test]
    fn hjkl_clamp_to_the_line() {
        let ls = l(&["abcdef", "ab", "", "x"]);
        let mut c = CopyMode::new(&ls);
        assert_eq!(at(&c), (3, 0));
        typed(&mut c, &ls, "g");
        typed(&mut c, &ls, "llllllllll");
        assert_eq!(at(&c), (0, 5));
        // j clamps col to the new line's length.
        typed(&mut c, &ls, "j");
        assert_eq!(at(&c), (1, 1));
        typed(&mut c, &ls, "j");
        assert_eq!(at(&c), (2, 0));
        typed(&mut c, &ls, "l");
        assert_eq!(at(&c), (2, 0));
        typed(&mut c, &ls, "jjj");
        assert_eq!(at(&c), (3, 0));
        typed(&mut c, &ls, "kkkkk");
        assert_eq!(at(&c), (0, 0));
        typed(&mut c, &ls, "h");
        assert_eq!(at(&c), (0, 0));
    }

    #[test]
    fn g_and_capital_g_jump_to_the_ends() {
        let ls = l(&["abc", "def", "ghi"]);
        let mut c = CopyMode::new(&ls);
        typed(&mut c, &ls, "gll");
        assert_eq!(at(&c), (0, 2));
        typed(&mut c, &ls, "G");
        assert_eq!(at(&c), (2, 0));
        typed(&mut c, &ls, "lg");
        assert_eq!(at(&c), (0, 0));
    }

    /// Thirty lines `l00`–`l29` with ten lines per page. The cursor starts on the last line.
    fn thirty() -> (Vec<String>, CopyMode) {
        let ls: Vec<String> = (0..30).map(|i| format!("l{i:02}")).collect();
        let mut c = CopyMode::new(&ls);
        c.page = 10;
        (ls, c)
    }

    #[test]
    fn page_keys_move_a_page_and_half_page_keys_move_half() {
        let (ls, mut c) = thirty();
        assert_eq!(c.line, 29);
        // ⌃b and ⌃f move one page (10 lines), ⌃u and ⌃d half a page (5 lines).
        c.key(&ls, 0x02);
        assert_eq!(c.line, 19);
        c.key(&ls, 0x15);
        assert_eq!(c.line, 14);
        c.key(&ls, 0x04);
        assert_eq!(c.line, 19);
        c.key(&ls, 0x06);
        assert_eq!(c.line, 29);
        // Even on a one-line page, a half page moves at least one line.
        c.page = 1;
        c.key(&ls, 0x15);
        assert_eq!(c.line, 28);
    }

    #[test]
    fn paging_clamps_at_both_ends_and_keeps_the_column_inside_the_line() {
        let (ls, mut c) = thirty();
        c.key(&ls, 0x06);
        c.key(&ls, 0x04);
        assert_eq!(c.line, 29);
        typed(&mut c, &ls, "g");
        // It doesn't go further up at the top. If fewer lines remain than a page, it moves only what remains.
        c.key(&ls, 0x02);
        c.key(&ls, 0x15);
        assert_eq!(c.line, 0);
        c.key(&ls, 0x06);
        c.key(&ls, 0x06);
        c.key(&ls, 0x06);
        c.key(&ls, 0x06);
        assert_eq!(c.line, 29);
        // The column is clamped into the new line.
        let ls = l(&["abcdef", "ab", "abcdef"]);
        let mut c = CopyMode::new(&ls);
        c.page = 1;
        typed(&mut c, &ls, "$");
        assert_eq!(at(&c), (2, 5));
        c.key(&ls, 0x02);
        assert_eq!(at(&c), (1, 1));
        c.key(&ls, 0x02);
        assert_eq!(at(&c), (0, 1));
        c.key(&ls, 0x06);
        c.key(&ls, 0x06);
        assert_eq!(at(&c), (2, 1));
    }

    #[test]
    fn paging_is_ignored_while_typing_a_query() {
        let (ls, mut c) = thirty();
        c.key(&ls, b'/');
        for b in [0x02, 0x06, 0x15, 0x04] {
            c.key(&ls, b);
        }
        assert_eq!((c.line, c.input.as_str()), (29, ""));
    }

    #[test]
    fn zero_and_dollar_go_to_the_first_and_last_char_of_the_line() {
        let ls = l(&["abcdef", "", "한글x"]);
        let mut c = CopyMode::new(&ls);
        assert_eq!(at(&c), (2, 0));
        // Counted by character position: the end of `한글x` is the 2nd character.
        typed(&mut c, &ls, "$");
        assert_eq!(at(&c), (2, 2));
        typed(&mut c, &ls, "0");
        assert_eq!(at(&c), (2, 0));
        // On an empty line it's 0.
        typed(&mut c, &ls, "k$");
        assert_eq!(at(&c), (1, 0));
        typed(&mut c, &ls, "k$");
        assert_eq!(at(&c), (0, 5));
        // Used together with a selection, it copies through to the end of the line.
        typed(&mut c, &ls, "0lv$");
        assert_eq!(c.key(&ls, b'y'), CopyOut::Copy("bcdef".into()));
        // While a search query is being typed, it's a character.
        c.key(&ls, b'/');
        typed(&mut c, &ls, "0$");
        assert_eq!((c.input.as_str(), at(&c)), ("0$", (0, 5)));
    }

    #[test]
    fn question_mark_searches_backwards_and_n_keeps_that_direction() {
        let ls = l(&["foo 1", "bar", "foo 2", "baz", "foo 3", "tail"]);
        let mut c = CopyMode::new(&ls);
        assert_eq!(at(&c), (5, 0));
        c.key(&ls, b'?');
        assert!(c.typing);
        assert_eq!(c.status(6), "?");
        typed(&mut c, &ls, "foo");
        assert_eq!(c.status(6), "?foo");
        c.key(&ls, b'\r');
        assert_eq!(at(&c), (4, 0));
        // n goes in the direction searched (backward), N the opposite (forward). At the end, they wrap to the opposite end.
        c.key(&ls, b'n');
        assert_eq!(at(&c), (2, 0));
        c.key(&ls, b'n');
        assert_eq!(at(&c), (0, 0));
        c.key(&ls, b'n');
        assert_eq!(at(&c), (4, 0));
        c.key(&ls, b'N');
        assert_eq!(at(&c), (0, 0));
        c.key(&ls, b'N');
        assert_eq!(at(&c), (2, 0));
        // Searching again with `/` changes the direction to forward.
        search(&mut c, &ls, "foo");
        assert_eq!(at(&c), (2, 0));
        c.key(&ls, b'n');
        assert_eq!(at(&c), (4, 0));
        c.key(&ls, b'N');
        assert_eq!(at(&c), (2, 0));
    }

    #[test]
    fn backward_search_counts_the_cursor_spot_and_wraps_to_the_last_match() {
        let ls = l(&["foo", "bar", "foo x foo"]);
        let mut c = CopyMode::new(&ls);
        typed(&mut c, &ls, "$");
        assert_eq!(at(&c), (2, 8));
        // The cursor position is a match too (same as `/`).
        c.key(&ls, b'?');
        typed(&mut c, &ls, "oo");
        c.key(&ls, b'\r');
        assert_eq!(at(&c), (2, 7));
        // If there's no match before it, it comes back to the last match.
        let ls = l(&["x", "foo", "bar foo"]);
        let mut c = CopyMode::new(&ls);
        typed(&mut c, &ls, "g");
        c.key(&ls, b'?');
        typed(&mut c, &ls, "foo");
        c.key(&ls, b'\r');
        assert_eq!(at(&c), (2, 4));
        c.key(&ls, b'?');
        typed(&mut c, &ls, "zzz");
        c.key(&ls, b'\r');
        assert!(c.miss);
    }

    #[test]
    fn a_new_search_starts_empty_and_esc_keeps_the_last_query_and_direction() {
        let ls = l(&["foo 1", "bar", "foo 2", "baz", "foo 3"]);
        let mut c = CopyMode::new(&ls);
        search(&mut c, &ls, "foo");
        assert_eq!(at(&c), (4, 0));
        // `?` starts with empty input, and the previous query isn't shown in the status line.
        c.key(&ls, b'?');
        assert_eq!((c.input.as_str(), c.status(5).as_str()), ("", "?"));
        typed(&mut c, &ls, "ba");
        c.key(&ls, 0x1b);
        // Esc only folds up the input; the previous query and direction (forward) stay.
        assert!(!c.typing);
        assert_eq!((c.query.as_str(), c.forward), ("foo", true));
        c.key(&ls, b'n');
        assert_eq!(at(&c), (0, 0));
        c.key(&ls, b'N');
        assert_eq!(at(&c), (4, 0));
        // The same even if the direction was backward.
        c.key(&ls, b'?');
        typed(&mut c, &ls, "foo");
        c.key(&ls, b'\r');
        c.key(&ls, b'/');
        typed(&mut c, &ls, "zzz");
        c.key(&ls, 0x1b);
        assert_eq!((c.query.as_str(), c.forward), ("foo", false));
        let before = at(&c);
        c.key(&ls, b'n');
        assert!(at(&c) < before);
    }

    #[test]
    fn enter_on_an_empty_input_repeats_the_last_query_in_the_new_direction() {
        let ls = l(&["foo 1", "bar", "foo 2", "baz", "foo 3", "tail"]);
        let mut c = CopyMode::new(&ls);
        search(&mut c, &ls, "foo");
        // There's no match after the cursor, so it wraps to the start, (0, 0).
        assert_eq!(at(&c), (0, 0));
        // Empty `/` Enter: forward with the same query. Like n, it goes to the next match after the cursor and doesn't stay in place.
        c.key(&ls, b'/');
        c.key(&ls, b'\r');
        assert_eq!((at(&c), c.query.as_str(), c.typing), ((2, 0), "foo", false));
        c.key(&ls, b'/');
        c.key(&ls, b'\r');
        assert_eq!(at(&c), (4, 0));
        // Empty `?` Enter: repeats in the new direction (backward), and now n goes backward too.
        c.key(&ls, b'?');
        c.key(&ls, b'\r');
        assert_eq!((at(&c), c.forward), ((2, 0), false));
        c.key(&ls, b'n');
        assert_eq!(at(&c), (0, 0));
        // If there's no previous query, nothing happens.
        let mut c = CopyMode::new(&ls);
        c.key(&ls, b'?');
        c.key(&ls, b'\r');
        assert_eq!((at(&c), c.miss, c.typing), ((5, 0), false, false));
    }

    #[test]
    fn paging_moves_the_view_by_as_many_lines_as_the_cursor() {
        // A 3-line screen holds 31 lines, and the scrollback is 28 lines. A page is 3 lines.
        let mut sc = screen(10, 3, &numbered(30));
        let ls = sc.history_text();
        let mut c = CopyMode::new(&ls);
        c.page = 3;
        c.reveal(&mut sc);
        assert_eq!((c.line, offset(&sc)), (29, 0));
        // The cursor is on the middle line of the screen.
        assert_eq!(c.highlight(&sc), Some(sel((0, 1), (0, 1))));
        // A page key moves the screen by the same number of lines, so the cursor stays on the same line. It doesn't scroll only up to the edge.
        c.key(&ls, 0x02);
        c.reveal(&mut sc);
        assert_eq!((c.line, offset(&sc)), (26, 3));
        assert_eq!(c.highlight(&sc), Some(sel((0, 1), (0, 1))));
        c.key(&ls, 0x06);
        c.reveal(&mut sc);
        assert_eq!((c.line, offset(&sc)), (29, 0));
        // At the top, the cursor stays visible even if the screen can't go up any further.
        for _ in 0..12 {
            c.key(&ls, 0x02);
        }
        c.reveal(&mut sc);
        assert_eq!((c.line, offset(&sc)), (0, 28));
        assert_eq!(c.highlight(&sc), Some(sel((0, 0), (0, 0))));
        // The number of lines pushed is used only once.
        c.reveal(&mut sc);
        assert_eq!(offset(&sc), 28);
    }

    #[test]
    fn new_starts_on_the_last_line_that_has_text() {
        assert_eq!(CopyMode::new(&l(&["a", "b", "", ""])).line, 1);
        assert_eq!(CopyMode::new(&l(&["", ""])).line, 1);
        assert_eq!(CopyMode::new(&[]).line, 0);
    }

    #[test]
    fn empty_buffer_takes_every_key_without_panicking() {
        let mut c = CopyMode::new(&[]);
        for b in b"hjklgGvnN/\r\x7fq y?\r0$\x02\x06\x15\x04" {
            c.key(&[], *b);
        }
        let mut c = CopyMode::new(&l(&[""]));
        typed(&mut c, &l(&[""]), "vlhjkG");
        assert_eq!(c.key(&l(&[""]), b'y'), CopyOut::Copy(String::new()));
    }

    #[test]
    fn status_shows_position_and_hints_or_the_query() {
        let ls = l(&["a", "b", "c"]);
        let mut c = CopyMode::new(&ls);
        assert_eq!(
            c.status(3),
            "Copy mode │ 3/3 │ v select · y copy · / ? search · q quit"
        );
        c.key(&ls, b'/');
        typed(&mut c, &ls, "ab");
        assert_eq!(c.status(3), "/ab");
        // A backward search shows its input with `?`.
        c.key(&ls, 0x1b);
        c.key(&ls, b'?');
        typed(&mut c, &ls, "ab");
        assert_eq!(c.status(3), "?ab");
    }

    #[test]
    fn split_maps_arrows_pages_and_a_lone_esc_and_drops_other_escapes() {
        use Key::{Arrow, Byte, PageDown, PageUp};
        let keys = |bytes: &[u8]| split(bytes).into_iter().map(|(k, _)| k).collect::<Vec<_>>();
        assert_eq!(
            keys(b"\x1b[A\x1b[B\x1b[C\x1b[D"),
            [Arrow(b'A'), Arrow(b'B'), Arrow(b'C'), Arrow(b'D')]
        );
        assert_eq!(keys(b"\x1bOA\x1bOD"), [Arrow(b'A'), Arrow(b'D')]);
        assert_eq!(keys(b"\x1b"), [Byte(0x1b)]);
        assert_eq!(keys(b"j\x1b"), [Byte(b'j'), Byte(0x1b)]);
        // PgUp (`5~`) and PgDn (`6~`) are page keys even with a modifier.
        assert_eq!(keys(b"\x1b[5~\x1b[6~\x1b[5;2~"), [PageUp, PageDown, PageUp]);
        // An arrow with a modifier is still an arrow, and other CSI, SS3 and Alt keys are discarded whole.
        assert_eq!(keys(b"\x1b[1;5Aq"), [Arrow(b'A'), Byte(b'q')]);
        assert_eq!(keys(b"\x1b[3~\x1b[15~\x1b[H\x1bOPj"), [Byte(b'j')]);
        assert_eq!(keys(b"\x1bxj"), [Byte(b'j')]);
        // An unfinished sequence is discarded. Characters go one byte at a time.
        assert_eq!(keys(b"j\x1b["), [Byte(b'j')]);
        assert_eq!(keys("한\r".as_bytes()).len(), 4);
        assert_eq!(keys(b"\r"), [Byte(b'\r')]);
    }

    #[test]
    fn split_reports_where_each_key_ends() {
        use Key::{Arrow, Byte, PageDown};
        // A discarded sequence counts toward the length passed over too.
        assert_eq!(
            split(b"q\x1b[Als"),
            [
                (Byte(b'q'), 1),
                (Arrow(b'A'), 4),
                (Byte(b'l'), 5),
                (Byte(b's'), 6)
            ]
        );
        assert_eq!(split(b"\x1b[3~j"), [(Byte(b'j'), 5)]);
        assert_eq!(split(b"\x1bxj"), [(Byte(b'j'), 3)]);
        assert_eq!(split(b"\x1bOBx"), [(Arrow(b'B'), 3), (Byte(b'x'), 4)]);
        assert_eq!(split(b"\x1b[6~"), [(PageDown, 4)]);
        // What follows the key that ended it is left as is.
        let bytes = b"qls\r";
        let (key, end) = split(bytes)[0];
        assert_eq!((key, &bytes[end..]), (Byte(b'q'), b"ls\r".as_slice()));
    }

    #[test]
    fn two_escapes_in_one_chunk_are_two_esc_keys() {
        use Key::{Arrow, Byte};
        assert_eq!(split(b"\x1b\x1b"), [(Byte(0x1b), 1), (Byte(0x1b), 2)]);
        assert_eq!(split(b"\x1b\x1b[A"), [(Byte(0x1b), 1), (Arrow(b'A'), 4)]);
        assert_eq!(
            split(b"j\x1b\x1b"),
            [(Byte(b'j'), 1), (Byte(0x1b), 2), (Byte(0x1b), 3)]
        );
    }

    #[test]
    fn drop_lone_esc_only_strips_leading_bare_escs() {
        assert_eq!(drop_lone_esc(b""), b"");
        assert_eq!(drop_lone_esc(b"\x1b"), b"");
        assert_eq!(drop_lone_esc(b"\x1b\x1b\x1b"), b"");
        assert_eq!(drop_lone_esc(b"\x1b\x1bls\r"), b"ls\r");
        // The start of a sequence is kept.
        assert_eq!(drop_lone_esc(b"\x1b[A"), b"\x1b[A");
        assert_eq!(drop_lone_esc(b"\x1b\x1bOA"), b"\x1bOA");
        assert_eq!(drop_lone_esc(b"\x1b\x1b\x1b[5~x"), b"\x1b[5~x");
        // An Esc press without modifiers in the kitty keyboard protocol is a lone Esc too. One with a modifier, or a release or repeat event, is kept.
        assert_eq!(drop_lone_esc(b"\x1b[27u"), b"");
        assert_eq!(drop_lone_esc(b"\x1b[27;1u\x1b[27;1:1u\x1bls"), b"ls");
        assert_eq!(drop_lone_esc(b"\x1b\x1b[27uls"), b"ls");
        assert_eq!(drop_lone_esc(b"\x1b[27;5u"), b"\x1b[27;5u");
        assert_eq!(drop_lone_esc(b"\x1b[27;2u"), b"\x1b[27;2u");
        assert_eq!(drop_lone_esc(b"\x1b[27;1:3u"), b"\x1b[27;1:3u");
        assert_eq!(drop_lone_esc(b"\x1b[270u"), b"\x1b[270u");
        // If it isn't at the very front, leave it alone.
        assert_eq!(drop_lone_esc(b"ls\x1b"), b"ls\x1b");
        assert_eq!(drop_lone_esc(b"x\x1b\x1b"), b"x\x1b\x1b");
    }

    #[test]
    fn freeze_sets_the_page_to_the_screen_lines() {
        // With a 3-line screen, a page is 3 lines. Even if not set separately, it doesn't page one line at a time.
        let sc = screen(10, 3, &numbered(30));
        let (ls, mut c) = freeze(&sc);
        assert_eq!((c.page, c.line), (3, 29));
        c.key(&ls, 0x02);
        assert_eq!(c.line, 26);
    }

    #[test]
    fn moving_lines_clears_the_no_match_notice() {
        let ls = l(&["a", "b", "c"]);
        let mut c = CopyMode::new(&ls);
        search(&mut c, &ls, "zzz");
        assert!(c.miss);
        c.move_lines(&ls, -1);
        assert!(!c.miss);
    }

    fn screen(cols: u16, rows: u16, bytes: &[u8]) -> Screen {
        let mut sc = Screen::new(cols, rows);
        sc.feed(bytes);
        sc
    }

    fn numbered(n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|i| format!("line{i}\r\n").into_bytes())
            .collect()
    }

    fn offset(sc: &Screen) -> usize {
        sc.term().grid().display_offset()
    }

    #[test]
    fn reveal_scrolls_just_enough_to_show_the_cursor_line() {
        // 10 lines written on a 3-line screen, so the bottom has line9 and a blank line, and the scrollback is 8 lines.
        let mut sc = screen(10, 3, &numbered(10));
        let ls = sc.history_text();
        assert_eq!(ls.len(), 11);
        let mut c = CopyMode::new(&ls);
        assert_eq!(c.line, 9);
        c.reveal(&mut sc);
        assert_eq!(offset(&sc), 0);
        // Going to the top scrolls all the way back.
        c.key(&ls, b'g');
        c.reveal(&mut sc);
        assert_eq!(offset(&sc), 8);
        // Within the screen it doesn't scroll, and the moment it goes off the bottom of the screen it comes down only one line at a time.
        c.key(&ls, b'j');
        c.reveal(&mut sc);
        assert_eq!(offset(&sc), 8);
        typed(&mut c, &ls, "jj");
        c.reveal(&mut sc);
        assert_eq!((c.line, offset(&sc)), (3, 7));
        // Going one line off the top moves up only one line.
        typed(&mut c, &ls, "kkk");
        c.reveal(&mut sc);
        assert_eq!((c.line, offset(&sc)), (0, 8));
        c.key(&ls, b'G');
        c.reveal(&mut sc);
        assert_eq!(offset(&sc), 0);
    }

    fn sel(start: (u16, u16), end: (u16, u16)) -> Selection {
        Selection { start, end }
    }

    #[test]
    fn highlight_marks_the_cursor_cell_or_the_anchor_range_in_view_coords() {
        let mut sc = screen(10, 3, &numbered(10));
        let ls = sc.history_text();
        let mut c = CopyMode::new(&ls);
        // The cursor is on the first cell of line9 (visible line 1).
        assert_eq!(c.highlight(&sc), Some(sel((0, 1), (0, 1))));
        typed(&mut c, &ls, "ll");
        assert_eq!(c.highlight(&sc), Some(sel((2, 1), (2, 1))));
        // The selection runs from the anchor to the cursor. The same even if the cursor is before it.
        typed(&mut c, &ls, "vk");
        assert_eq!(c.highlight(&sc), Some(sel((2, 0), (2, 1))));
        // An end that goes off the top is clamped to the first visible cell, and one that goes off the bottom to the last cell, and the paint continues.
        c.key(&ls, b'g');
        c.reveal(&mut sc);
        assert_eq!(c.highlight(&sc), Some(sel((0, 0), (9, 2))));
        // If the selection is entirely off screen, nothing is drawn.
        typed(&mut c, &ls, "G");
        assert_eq!(c.highlight(&sc), None);
    }

    #[test]
    fn highlight_maps_char_index_to_grid_column_over_wide_chars() {
        let sc = screen(10, 3, "한글 ab".as_bytes());
        let ls = sc.history_text();
        assert_eq!(ls[0], "한글 ab");
        let mut c = CopyMode::new(&ls);
        // 한 (cells 0–1) 글 (cells 2–3) space (4) a (5) b (6)
        let cols: Vec<u16> = (0..5)
            .map(|_| {
                let col = c.highlight(&sc).unwrap().start.0;
                c.key(&ls, b'l');
                col
            })
            .collect();
        assert_eq!(cols, [0, 2, 4, 5, 6]);
    }

    /// Writes 25 characters on a narrow screen (10 cells) without line breaks. The last cell of the first row is a space, so as a screen line it's cut to `abcdefghi`.
    const WRAPPED: &str = "abcdefghi jklmnopqrstuvwx";

    fn copy_range(sc: &Screen, keys: &str) -> CopyOut {
        let (ls, mut c) = freeze(sc);
        typed(&mut c, &ls, keys);
        c.key(&ls, b'y')
    }

    #[test]
    fn yank_joins_soft_wrapped_rows_without_newline_and_keeps_their_trailing_space() {
        let sc = screen(10, 5, WRAPPED.as_bytes());
        let (ls, _) = freeze(&sc);
        assert_eq!(&ls[..3], ["abcdefghi", "jklmnopqrs", "tuvwx"]);
        // From start to end: the space at the wrap point is a character so it remains, and there is no `\n`.
        assert_eq!(
            copy_range(&sc, "gvjjllll"),
            CopyOut::Copy(WRAPPED.to_string())
        );
        // The same when starting in the middle of a wrapped line and ending in the middle.
        assert_eq!(
            copy_range(&sc, "gllllllvjjhh"),
            CopyOut::Copy("ghi jklmnopqrstuv".to_string())
        );
        // Just the two wrapped lines: the space at the end of the first line remains.
        assert_eq!(
            copy_range(&sc, "gvjl"),
            CopyOut::Copy("abcdefghi jk".to_string())
        );
    }

    #[test]
    fn yank_puts_one_newline_only_at_the_real_line_end() {
        let sc = screen(10, 5, format!("{WRAPPED}\r\nnext\r\nlast").as_bytes());
        let (ls, c) = freeze(&sc);
        assert_eq!(
            &ls[..5],
            ["abcdefghi", "jklmnopqrs", "tuvwx", "next", "last"]
        );
        assert_eq!(c.line, 4);
        // From a wrapped line through a real line break into part of the next line: there is exactly one `\n`, between `tuvwx` and `next`.
        assert_eq!(
            copy_range(&sc, "gvjjjlll"),
            CopyOut::Copy(format!("{WRAPPED}\nnext"))
        );
        // Two real lines are joined with `\n` as they are, and the right-hand spaces are stripped.
        assert_eq!(
            copy_range(&sc, "gvjjjjll"),
            CopyOut::Copy(format!("{WRAPPED}\nnext\nlas"))
        );
    }

    #[test]
    fn rows_that_do_not_wrap_stay_trimmed_and_newline_joined() {
        // A line that isn't full is not a wrapped line.
        let sc = screen(10, 5, b"ab  \r\ncd");
        assert_eq!(copy_range(&sc, "gvjl"), CopyOut::Copy("ab\ncd".to_string()));
    }

    #[test]
    fn hangul_hint_replaces_the_status_hint_until_the_next_key_or_wheel() {
        let ls = l(&["abc", "def"]);
        let mut c = CopyMode::new(&ls);
        c.hangul = true;
        assert_eq!(
            c.status(2),
            "Copy mode │ 2/2 │ Switch your keyboard to English and press again"
        );
        c.move_lines(&ls, -1);
        assert!(
            !c.hangul
                && c.status(2)
                    .ends_with("v select · y copy · / ? search · q quit")
        );
        c.hangul = true;
        c.key(&ls, b'j');
        assert!(!c.hangul);
        // While a search query is being typed, the input is the status line.
        c.key(&ls, b'/');
        c.hangul = true;
        assert_eq!(c.status(2), "/");
    }
}
