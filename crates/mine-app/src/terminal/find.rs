//! The ⌘F find box's state: the matches of the query over the scrollback and visible lines (`Screen::history_text`),
//! which one is current, the scroll that brings it on screen, and the cells of the matches on the visible screen. The
//! GPUI wiring (the input box, Enter, Shift+Enter, Esc) is in `terminal/mod.rs`.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use mine_core::screen::Screen;
use mine_core::search::find;

/// The query, its matches as (line, char position) over `history_text` (oldest line first), and the current one.
#[derive(Clone, Debug, Default)]
pub struct FindState {
    pub query: String,
    pub hits: Vec<(usize, usize)>,
    pub current: usize,
    /// The session the box was opened for (`Model::attached`). Its matches and count are that session's.
    session: Option<String>,
}

impl FindState {
    pub fn new(session: Option<String>) -> FindState {
        FindState {
            session,
            ..FindState::default()
        }
    }

    /// Whether this is the find box of `attached`. When another session is attached, the box closes rather than
    /// showing the old session's count.
    pub fn searches(&self, attached: Option<&str>) -> bool {
        self.session.as_deref() == attached
    }

    /// Finds the query in `lines`, ignoring case. The current match is the newest one, nearest the live screen.
    pub fn search(&mut self, lines: &[String]) {
        self.hits = find(lines, &self.query);
        self.current = self.hits.len().saturating_sub(1);
    }

    /// The next match down, wrapping from the newest to the oldest.
    pub fn next(&mut self) -> Option<(usize, usize)> {
        self.advance(1)
    }

    /// The previous match up, wrapping from the oldest to the newest.
    pub fn prev(&mut self) -> Option<(usize, usize)> {
        self.advance(self.hits.len().saturating_sub(1))
    }

    /// Enter (`forward`) and Shift+Enter. The history may have changed since the last search (new output, a resize,
    /// another session), so it searches `lines` again, then goes to the first match after the current one's place, or
    /// the last one before it, wrapping around, like `n`/`N` in the TUI's copy mode. With no current match it lands
    /// on the newest.
    ///
    /// ponytail: the place is a line number. Once the scrollback is full, output drops old lines and shifts the
    /// numbers, so a step can skip or repeat one match; it still lands on a real match with a right count.
    pub fn step(&mut self, lines: &[String], forward: bool) -> Option<(usize, usize)> {
        let here = self.hits.get(self.current).copied();
        self.search(lines);
        let Some(here) = here else {
            return self.hits.get(self.current).copied();
        };
        // Put `current` where `next`/`prev` step onto the first match after `here` / the last one before it.
        let last = self.hits.len().saturating_sub(1);
        self.current = if forward {
            self.hits.iter().rposition(|&p| p <= here).unwrap_or(last)
        } else {
            self.hits.iter().position(|&p| p >= here).unwrap_or(0)
        };
        if forward { self.next() } else { self.prev() }
    }

    /// Moves `by` matches down, wrapping around.
    fn advance(&mut self, by: usize) -> Option<(usize, usize)> {
        let n = self.hits.len();
        if n == 0 {
            return None;
        }
        self.current = (self.current + by) % n;
        Some(self.hits[self.current])
    }

    /// The count shown in the find box: "2/5", or "No matches". `None` while the query is empty.
    pub fn status(&self) -> Option<String> {
        if self.query.is_empty() {
            None
        } else if self.hits.is_empty() {
            Some("No matches".into())
        } else {
            Some(format!("{}/{}", self.current + 1, self.hits.len()))
        }
    }
}

/// The lines to scroll (positive is toward the past) so `line`, an index into `history_text`, is on screen. 0 if it
/// already is.
pub fn reveal(screen: &Screen, line: usize) -> i32 {
    let grid = screen.term().grid();
    let row = line as i64 - grid.history_size() as i64 + grid.display_offset() as i64;
    let rows = grid.screen_lines() as i64;
    let lines = if row < 0 {
        -row
    } else if row >= rows {
        rows - 1 - row
    } else {
        0
    };
    lines as i32
}

/// Where `query` matches on the visible rows, as (row, first column, cell count). Characters are counted the way
/// `history_text` counts them (a wide character is one character over two cells, a combining character is one more
/// character on its cell, trailing blanks are dropped), so this agrees with `find`.
pub fn visible(screen: &Screen, query: &str) -> Vec<(u16, u16, u16)> {
    let len = query.chars().count();
    if len == 0 {
        return Vec::new();
    }
    let grid = screen.term().grid();
    let offset = grid.display_offset() as i32;
    let mut out = Vec::new();
    for y in 0..grid.screen_lines() {
        let row = &grid[Line(y as i32 - offset)];
        let mut text = String::new();
        // The (column, cells) each character of `text` sits on.
        let mut at: Vec<(u16, u16)> = Vec::new();
        for x in 0..grid.columns() {
            let cell = &row[Column(x)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let cells = if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            let marks = cell.zerowidth().unwrap_or_default();
            text.push(if cell.c.is_control() { ' ' } else { cell.c });
            text.extend(marks);
            at.extend(std::iter::repeat_n((x as u16, cells), 1 + marks.len()));
        }
        text.truncate(text.trim_end_matches(' ').len());
        for (_, i) in find(std::slice::from_ref(&text), query) {
            let (col, _) = at[i];
            let (last, cells) = at[i + len - 1];
            out.push((y as u16, col, last + cells - col));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn screen(cols: u16, rows: u16, bytes: &[u8]) -> Screen {
        let mut s = Screen::new(cols, rows);
        s.feed(bytes);
        s
    }

    #[test]
    fn find_box_belongs_to_the_session_it_was_opened_for() {
        let f = FindState::new(Some("a".into()));
        assert!(f.searches(Some("a")));
        // Another session attached, or none (the session vanished): the box closes.
        assert!(!f.searches(Some("b")));
        assert!(!f.searches(None));
    }

    #[test]
    fn step_searches_again_when_the_history_changed() {
        let mut f = FindState {
            query: "err".into(),
            ..FindState::default()
        };
        // Session A: five matches, on the newest ("5/5").
        f.search(&lines(&["err 1", "err 2", "x", "err 3", "err 4", "err 5"]));
        assert_eq!(f.status().as_deref(), Some("5/5"));
        // Session B (or a reflowed, trimmed history): Enter lands on one of B's own matches, counted over B.
        let b = lines(&["ok", "err b1", "ok", "ok", "ERR b2", "ok", "ok"]);
        assert_eq!(f.step(&b, true), Some((1, 0)));
        assert_eq!(f.status().as_deref(), Some("1/2"));
        assert_eq!(f.step(&b, true), Some((4, 0)));
        assert_eq!(f.step(&b, false), Some((1, 0)));
        // Output added a match: the step goes on from the current one to the next, in the new list.
        let more = lines(&["ok", "err b1", "ok", "err new", "ERR b2"]);
        assert_eq!(f.step(&more, true), Some((3, 0)));
        assert_eq!(f.status().as_deref(), Some("2/3"));
        // The current match is gone (between two others): forward and backward land on its neighbours.
        let gone = lines(&["err b1", "x", "x", "x", "err c"]);
        assert_eq!(f.clone().step(&gone, true), Some((4, 0)));
        assert_eq!(f.step(&gone, false), Some((0, 0)));
        // No match left: nothing, and the count says so. A match coming back starts on the newest.
        assert_eq!(f.step(&lines(&["ok"]), true), None);
        assert_eq!(f.status().as_deref(), Some("No matches"));
        assert_eq!(f.step(&lines(&["err", "err"]), true), Some((1, 0)));
    }

    #[test]
    fn find_wraps_both_ways() {
        let mut f = FindState {
            query: "ERROR".into(),
            ..FindState::default()
        };
        f.search(&lines(&["error a", "ok", "x Error", "ERROR"]));
        // Case-insensitive. It starts on the newest match.
        assert_eq!(f.hits, [(0, 0), (2, 2), (3, 0)]);
        assert_eq!(f.current, 2);
        assert_eq!(f.status().as_deref(), Some("3/3"));
        // Next from the newest wraps to the oldest, and previous from the oldest wraps back.
        assert_eq!(f.next(), Some((0, 0)));
        assert_eq!(f.status().as_deref(), Some("1/3"));
        assert_eq!(f.prev(), Some((3, 0)));
        assert_eq!(f.prev(), Some((2, 2)));
        assert_eq!(f.next(), Some((3, 0)));
        // No match: nothing to step to.
        f.query = "zzz".into();
        f.search(&lines(&["error"]));
        assert_eq!((f.next(), f.prev()), (None, None));
        assert_eq!(f.status().as_deref(), Some("No matches"));
        f.query.clear();
        f.search(&lines(&["error"]));
        assert_eq!(f.status(), None);
    }

    #[test]
    fn reveal_scrolls_the_match_into_view() {
        // `seq 1 500` on 24 rows: "250" is far up in the scrollback.
        let seq: String = (1..=500).map(|i| format!("{i}\r\n")).collect();
        let mut s = screen(80, 24, seq.as_bytes());
        let mut f = FindState {
            query: "250".into(),
            ..FindState::default()
        };
        f.search(&s.history_text());
        assert_eq!(f.hits, [(249, 0)]);
        let lines = reveal(&s, 249);
        assert!(lines > 0, "{lines}");
        s.scroll(lines);
        assert_eq!(visible(&s, "250"), [(0, 0, 3)]);
        // Already on screen: no scroll.
        assert_eq!(reveal(&s, 249), 0);
        assert_eq!(reveal(&s, 249 + 23), 0);
        // Below the view: scroll toward the present just far enough to show it on the last row.
        assert_eq!(reveal(&s, 249 + 24), -1);
    }

    #[test]
    fn visible_matches_map_chars_to_cells() {
        // A wide character takes two cells; a combining character adds a character on its cell.
        let s = screen(20, 3, "한글 ab\r\nxe\u{301}ab AB".as_bytes());
        assert_eq!(visible(&s, "글 a"), [(0, 2, 4)]);
        assert_eq!(visible(&s, "ab"), [(0, 5, 2), (1, 2, 2), (1, 5, 2)]);
        assert_eq!(visible(&s, "e\u{301}a"), [(1, 1, 2)]);
        // Trailing blanks don't match, as in `history_text`.
        assert_eq!(visible(&s, "AB  "), []);
        assert_eq!(visible(&s, ""), []);
    }
}
