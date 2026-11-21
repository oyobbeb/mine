//! tmux식 복사 모드. 시작할 때 얼려 둔 줄(스크롤백 + 보이는 줄)을 키보드로 돌아다니며 고르고 검색한다.
//! 순수 상태 기계라 I/O를 하지 않는다. 얼려 둔 화면과 맞추는 일(`reveal`, `highlight`)만 클라이언트 화면을 읽는다.
//!
//! 좌표는 줄 번호(`lines` 인덱스, 0이 가장 오래된 줄)와 글자 위치(`char` 인덱스, 바이트도 칸도 아니다)다.

use alacritty_terminal::grid::{Dimensions, Grid, Row};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};

use super::select::Selection;
use crate::screen::Screen;

/// 키 하나를 먹은 결과.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyOut {
    Stay,
    /// 복사 모드를 끝낸다.
    Exit,
    /// 이 글자를 복사하고 끝낸다.
    Copy(String),
}

#[derive(Clone, Debug)]
pub struct CopyMode {
    pub line: usize,
    pub col: usize,
    /// `v`로 시작한 선택의 시작점. 커서까지가 선택이다.
    pub anchor: Option<(usize, usize)>,
    pub query: String,
    /// `/` 뒤 검색어를 입력하는 중이다.
    pub typing: bool,
    /// 방금 한 검색이 맞는 곳을 찾지 못했다. 다음 키에 지워진다.
    pub miss: bool,
    /// 검색어에 넣다 만 UTF-8 글자의 앞 바이트.
    pending: Vec<u8>,
    /// 줄 바꿈으로 다음 줄에 이어지는 줄의, 줄 수에서 잘려 나간 오른쪽 공백 수. 이어지지 않는 줄은 `None`이다.
    /// 복사할 때 이런 줄은 공백을 되살려 `\n` 없이 다음 줄에 붙인다.
    wraps: Vec<Option<usize>>,
}

/// 두 글자가 대소문자를 빼면 같은가. 소문자가 여러 글자로 풀리는 글자(`İ`)도 글자 단위로 견준다.
fn same(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// `query`가 시작하는 곳을 (줄, 글자 위치)로 모두 모은다. 대소문자는 가리지 않고, 겹쳐도 센다.
pub fn find(lines: &[String], query: &str) -> Vec<(usize, usize)> {
    let query: Vec<char> = query.chars().collect();
    if query.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        for (at, window) in chars.windows(query.len()).enumerate() {
            if query.iter().zip(window).all(|(&q, &c)| same(q, c)) {
                hits.push((i, at));
            }
        }
    }
    hits
}

/// 이 줄의 글자 수.
fn len_at(lines: &[String], line: usize) -> usize {
    lines.get(line).map_or(0, |l| l.chars().count())
}

/// 한 줄의 `from`번째 글자부터 `to`번째(포함)까지, 없으면 줄 끝까지. 바이트가 아니라 글자 위치로 센다.
fn slice(line: &str, from: usize, to: Option<usize>) -> String {
    let rest = line.chars().skip(from);
    match to {
        Some(to) => rest.take(to + 1 - from).collect(),
        None => rest.collect(),
    }
}

impl CopyMode {
    /// 마지막으로 글자가 있는 줄의 처음에서 시작한다. 화면 아래 빈 줄에서 시작하면 커서가 허공에 뜨기 때문이다.
    /// 모든 줄이 비었으면 맨 아랫줄이다.
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
            typing: false,
            miss: false,
            pending: Vec::new(),
            wraps: Vec::new(),
        }
    }

    pub fn key(&mut self, lines: &[String], b: u8) -> CopyOut {
        self.miss = false;
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
            b'g' => (self.line, self.col) = (0, 0),
            b'G' => (self.line, self.col) = (lines.len().saturating_sub(1), 0),
            b'v' => self.anchor = self.anchor.is_none().then_some((self.line, self.col)),
            b'/' => {
                self.typing = true;
                self.query.clear();
                self.pending.clear();
            }
            b'n' => self.step(lines, true),
            b'N' => self.step(lines, false),
            b'y' | b'\r' | b'\n' => return self.yank(lines),
            _ => {}
        }
        CopyOut::Stay
    }

    /// 화살표(`A`~`D`). 검색어를 입력하는 중에는 아무것도 하지 않는다.
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

    /// `line`(마지막 줄을 넘으면 마지막 줄)으로 옮기고 `col`을 그 줄 안으로 줄인다.
    fn go(&mut self, lines: &[String], line: usize) {
        self.line = line.min(lines.len().saturating_sub(1));
        self.col = self.col.min(len_at(lines, self.line).saturating_sub(1));
    }

    /// 검색어를 입력하는 중의 키. Esc는 입력만 취소하고 복사 모드에 남는다.
    fn type_key(&mut self, lines: &[String], b: u8) {
        match b {
            0x1b => {
                self.typing = false;
                self.query.clear();
                self.pending.clear();
            }
            b'\r' | b'\n' => {
                self.typing = false;
                self.pending.clear();
                self.search(lines);
            }
            // 덜 온 글자가 있으면 그 조각만 버린다.
            0x7f | 0x08 => {
                if self.pending.is_empty() {
                    self.query.pop();
                } else {
                    self.pending.clear();
                }
            }
            0x00..=0x1f => {}
            _ => {
                self.pending.push(b);
                match std::str::from_utf8(&self.pending) {
                    Ok(text) => {
                        self.query.push_str(text);
                        self.pending.clear();
                    }
                    // 글자가 아직 덜 왔다. 다음 바이트를 기다린다.
                    Err(e) if e.error_len().is_none() => {}
                    Err(_) => self.pending.clear(),
                }
            }
        }
    }

    /// Enter: 커서 자리나 그 뒤의 첫 매치로 간다. 끝까지 없으면 처음으로 돌아가 찾는다.
    fn search(&mut self, lines: &[String]) {
        let hits = find(lines, &self.query);
        let here = (self.line, self.col);
        let hit = hits.iter().find(|&&p| p >= here).or(hits.first());
        self.jump(hit.copied());
    }

    /// `n`(앞으로)·`N`(뒤로): 커서 다음·이전 매치로 간다. 끝에서는 반대쪽 끝으로 돌아간다.
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

    /// 매치로 옮긴다. 없으면 제자리에 두고 알린다(검색어가 비었으면 알릴 것도 없다).
    fn jump(&mut self, hit: Option<(usize, usize)>) {
        match hit {
            Some((line, col)) => (self.line, self.col) = (line, col),
            None => self.miss = !self.query.is_empty(),
        }
    }

    /// `y`·Enter: 선택이 있으면 그 글자를 복사하고, 없으면 tmux처럼 복사 없이 끝낸다.
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
                    // 줄 바꿈으로 이어진 줄: 끝의 공백도 글자고, 다음 줄에 바로 붙는다.
                    Some(pad) => out.extend(std::iter::repeat_n(' ', pad)),
                    None => out.push('\n'),
                }
            }
        }
        CopyOut::Copy(out)
    }

    /// 상태줄에 띄울 글. 검색어를 입력하는 중이면 그 입력이다.
    pub fn status(&self, total: usize) -> String {
        if self.typing {
            return format!("/{}", self.query);
        }
        let hint = if self.miss {
            "검색 결과 없음"
        } else {
            "v 선택 · y 복사 · / 검색 · q 끝"
        };
        format!("복사 모드 │ {}/{total} │ {hint}", self.line + 1)
    }

    /// 클라이언트 화면을 커서 줄이 보이게 스크롤한다. 이미 보이면 그대로 두고, 벗어났으면 가장자리까지만 옮긴다.
    pub fn reveal(&self, screen: &mut Screen) {
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

    /// 지금 보이는 화면 위에 반전으로 칠할 곳. 선택이 없으면 커서 칸 하나다.
    /// 선택이 화면 밖으로 나가면 보이는 첫 칸·마지막 칸에서 자르고, 통째로 밖이면 `None`이다.
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

/// 얼려 둔 줄 번호의 그리드 줄 번호. 스크롤백은 음수다.
fn line_of(grid: &Grid<Cell>, line: usize) -> i32 {
    line as i32 - grid.history_size() as i32
}

/// 얼려 둔 줄 번호가 지금 스크롤 위치에서 보이는 화면의 몇 번째 줄인가. 화면 밖이면 음수이거나 화면 줄 수 이상이다.
fn view_row(grid: &Grid<Cell>, line: usize) -> i64 {
    i64::from(line_of(grid, line)) + grid.display_offset() as i64
}

/// 줄의 `char_idx`번째 글자(결합 문자 포함)가 놓인 칸. `history_text`와 같은 셈법이다: 넓은 글자의 뒤 칸은 건너뛰고
/// 칸 하나에 얹힌 결합 문자는 글자 하나씩으로 센다.
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

/// 복사 모드를 시작할 때 얼려 둘 줄(스크롤백 + 보이는 줄)과 그 줄에서 시작하는 `CopyMode`.
/// 줄은 `Screen::history_text` 그대로 화면 줄 하나가 한 줄이다. 줄 바꿈으로 이어진 줄은 `wraps`에 따로 적어 둔다.
pub fn freeze(screen: &Screen) -> (Vec<String>, CopyMode) {
    let lines = screen.history_text();
    let mut mode = CopyMode::new(&lines);
    mode.wraps = wrap_pads(screen, &lines);
    (lines, mode)
}

/// `lines`(`history_text`)의 줄마다, 자동 줄 바꿈으로 다음 줄에 이어졌으면 `history_text`가 뗀 오른쪽 공백 수를 돌려준다.
/// 이어졌는지는 `select::selected_text`와 같게 줄 끝 칸의 `WRAPLINE`으로 본다.
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
            // `history_text`와 같은 셈법으로 센 글자 수에서 남은 글자 수를 뺀다.
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

/// 입력 덩이에서 뽑은 키.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Byte(u8),
    /// 화살표의 끝 바이트 `A`(위) `B`(아래) `C`(오른쪽) `D`(왼쪽).
    Arrow(u8),
}

/// 세션에 넘길 뻔한 입력 덩이를 키로 나눈다. 화살표(`ESC [ A`, `ESC O A` 등)는 `Arrow`, 단독 Esc는 `Byte(0x1b)`,
/// 그 밖의 이스케이프 시퀀스(다른 CSI·SS3, Alt 키)는 통째로 버린다. 글자는 바이트 하나씩이다.
pub fn split(bytes: &[u8]) -> Vec<Key> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        if b != 0x1b {
            out.push(Key::Byte(b));
            continue;
        }
        let Some(&lead) = bytes.get(i) else {
            out.push(Key::Byte(0x1b));
            break;
        };
        i += 1;
        // CSI(`[`)는 0x40..=0x7e 바이트로, SS3(`O`)는 바이트 하나로 끝난다. 그 밖에는 Alt 키라서 하나만 버린다.
        let end = match lead {
            b'[' => bytes[i..]
                .iter()
                .position(|c| (0x40..=0x7e).contains(c))
                .map(|n| i + n),
            b'O' => (i < bytes.len()).then_some(i),
            _ => continue,
        };
        let Some(end) = end else { break };
        if matches!(bytes[end], b'A'..=b'D') {
            out.push(Key::Arrow(bytes[end]));
        }
        i = end + 1;
    }
    out
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
    fn find_is_case_insensitive() {
        assert_eq!(
            find(&l(&["Error a", "ok", "x error"]), "error"),
            vec![(0, 0), (2, 2)]
        );
    }

    #[test]
    fn find_reports_every_match_by_char_index() {
        // 한 줄에 여럿(겹쳐도), 한글, 소문자로 바꾸면 바이트 길이가 달라지는 `İ`(2바이트 → 3바이트)가 앞에 있어도 글자 위치다.
        assert_eq!(find(&l(&["abab", "aaa"]), "ab"), vec![(0, 0), (0, 2)]);
        assert_eq!(find(&l(&["aaa"]), "aa"), vec![(0, 0), (0, 1)]);
        assert_eq!(find(&l(&["가나다 나", "İ Ab"]), "나"), vec![(0, 1), (0, 4)]);
        assert_eq!(find(&l(&["İ Ab"]), "ab"), vec![(0, 2)]);
        assert_eq!(find(&l(&["abc"]), ""), vec![]);
        assert_eq!(find(&l(&["ab"]), "abc"), vec![]);
    }

    #[test]
    fn n_wraps_around() {
        let ls = l(&["foo", "bar", "foo x foo"]);
        let mut c = CopyMode::new(&ls);
        assert_eq!(at(&c), (2, 0));
        // 커서 자리도 맞는 곳이다.
        search(&mut c, &ls, "foo");
        assert_eq!(at(&c), (2, 0));
        c.key(&ls, b'n');
        assert_eq!(at(&c), (2, 6));
        // 마지막 매치에서 n은 첫 매치로 돌아간다.
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
        // 첫 매치에서 N은 마지막 매치로 돌아간다.
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
        assert!(c.status(2).contains("검색 결과 없음"));
        // n도 못 찾으면 마찬가지고, 다른 키를 누르면 알림이 걷힌다.
        c.key(&ls, b'n');
        assert!(c.miss);
        c.key(&ls, b'j');
        assert!(!c.miss);
        assert!(!c.status(2).contains("검색 결과 없음"));
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
        // 커서가 선택 시작점의 위·왼쪽에 있다.
        typed(&mut c, &ls, "Gllvkh");
        assert_eq!((c.anchor, at(&c)), (Some((1, 2)), (0, 1)));
        assert_eq!(c.key(&ls, b'y'), CopyOut::Copy("lpha\nbet".into()));
        // 한 줄 안의 범위는 양 끝 글자를 포함한다.
        let mut c = CopyMode::new(&ls);
        typed(&mut c, &ls, "glvll");
        assert_eq!(c.key(&ls, b'y'), CopyOut::Copy("lph".into()));
        // 가운데 줄은 통째로 담는다.
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
        assert!(!c.typing && c.query.is_empty());
        // 입력 중이 아니면 Esc는 끝낸다. 입력 중에는 q도 글자다.
        c.key(&ls, b'/');
        assert_eq!(c.key(&ls, b'q'), CopyOut::Stay);
        assert_eq!(c.query, "q");
        c.key(&ls, 0x1b);
        assert_eq!(c.key(&ls, 0x1b), CopyOut::Exit);
    }

    #[test]
    fn korean_query_typed_byte_by_byte() {
        let ls = l(&["hello", "안녕 세계", "끝"]);
        let mut c = CopyMode::new(&ls);
        c.key(&ls, b'/');
        let bytes = "세계".as_bytes();
        // 세는 3바이트다. 다 오기 전에는 검색어에 넣지 않는다.
        c.key(&ls, bytes[0]);
        c.key(&ls, bytes[1]);
        assert_eq!(c.query, "");
        for b in &bytes[2..] {
            c.key(&ls, *b);
        }
        assert_eq!(c.query, "세계");
        c.key(&ls, b'\r');
        assert_eq!(at(&c), (1, 3));
        assert_eq!(
            c.status(3),
            "복사 모드 │ 2/3 │ v 선택 · y 복사 · / 검색 · q 끝"
        );
    }

    #[test]
    fn backspace_deletes_the_last_char_or_a_half_typed_one() {
        let ls = l(&["alpha"]);
        let mut c = CopyMode::new(&ls);
        c.key(&ls, b'/');
        typed(&mut c, &ls, "a한");
        assert_eq!(c.query, "a한");
        c.key(&ls, 0x7f);
        assert_eq!(c.query, "a");
        // 한의 앞 바이트만 오고 지우면 그 조각만 버린다.
        c.key(&ls, "한".as_bytes()[0]);
        c.key(&ls, 0x08);
        assert_eq!(c.query, "a");
        c.key(&ls, 0x7f);
        c.key(&ls, 0x7f);
        assert_eq!(c.query, "");
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
        assert_eq!(c.query, "");
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
        // j는 col을 새 줄 길이로 줄인다.
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

    #[test]
    fn new_starts_on_the_last_line_that_has_text() {
        assert_eq!(CopyMode::new(&l(&["a", "b", "", ""])).line, 1);
        assert_eq!(CopyMode::new(&l(&["", ""])).line, 1);
        assert_eq!(CopyMode::new(&[]).line, 0);
    }

    #[test]
    fn empty_buffer_takes_every_key_without_panicking() {
        let mut c = CopyMode::new(&[]);
        for b in b"hjklgGvnN/\r\x7fq y" {
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
            "복사 모드 │ 3/3 │ v 선택 · y 복사 · / 검색 · q 끝"
        );
        c.key(&ls, b'/');
        typed(&mut c, &ls, "ab");
        assert_eq!(c.status(3), "/ab");
    }

    #[test]
    fn split_maps_arrows_and_a_lone_esc_and_drops_other_escapes() {
        use Key::{Arrow, Byte};
        assert_eq!(
            split(b"\x1b[A\x1b[B\x1b[C\x1b[D"),
            [Arrow(b'A'), Arrow(b'B'), Arrow(b'C'), Arrow(b'D')]
        );
        assert_eq!(split(b"\x1bOA\x1bOD"), [Arrow(b'A'), Arrow(b'D')]);
        assert_eq!(split(b"\x1b"), [Byte(0x1b)]);
        assert_eq!(split(b"j\x1b"), [Byte(b'j'), Byte(0x1b)]);
        // 수정 키가 붙은 화살표도 화살표고, 다른 CSI·SS3와 Alt 키는 통째로 버린다.
        assert_eq!(split(b"\x1b[1;5Aq"), [Arrow(b'A'), Byte(b'q')]);
        assert_eq!(split(b"\x1b[5~\x1bOPj"), [Byte(b'j')]);
        assert_eq!(split(b"\x1bxj"), [Byte(b'j')]);
        // 끝나지 않은 시퀀스는 버린다. 글자는 바이트 하나씩 간다.
        assert_eq!(split(b"j\x1b["), [Byte(b'j')]);
        assert_eq!(split("한\r".as_bytes()).len(), 4);
        assert_eq!(split(b"\r"), [Byte(b'\r')]);
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
        // 3줄 화면에 10줄을 써서 맨 아래에는 line9와 빈 줄이 있고, 스크롤백은 8줄이다.
        let mut sc = screen(10, 3, &numbered(10));
        let ls = sc.history_text();
        assert_eq!(ls.len(), 11);
        let mut c = CopyMode::new(&ls);
        assert_eq!(c.line, 9);
        c.reveal(&mut sc);
        assert_eq!(offset(&sc), 0);
        // 맨 위로 가면 끝까지 거슬러 올라간다.
        c.key(&ls, b'g');
        c.reveal(&mut sc);
        assert_eq!(offset(&sc), 8);
        // 화면 안에서는 스크롤하지 않고, 화면 아래로 벗어나는 순간 한 줄씩만 내려온다.
        c.key(&ls, b'j');
        c.reveal(&mut sc);
        assert_eq!(offset(&sc), 8);
        typed(&mut c, &ls, "jj");
        c.reveal(&mut sc);
        assert_eq!((c.line, offset(&sc)), (3, 7));
        // 한 줄 위로 벗어나면 한 줄만 올라간다.
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
        // 커서는 line9(보이는 줄 1)의 첫 칸이다.
        assert_eq!(c.highlight(&sc), Some(sel((0, 1), (0, 1))));
        typed(&mut c, &ls, "ll");
        assert_eq!(c.highlight(&sc), Some(sel((2, 1), (2, 1))));
        // 선택은 anchor에서 커서까지다. 커서가 앞에 있어도 같다.
        typed(&mut c, &ls, "vk");
        assert_eq!(c.highlight(&sc), Some(sel((2, 0), (2, 1))));
        // 위로 벗어난 끝은 보이는 첫 칸으로, 아래로 벗어난 끝은 마지막 칸으로 눌러 이어 그린다.
        c.key(&ls, b'g');
        c.reveal(&mut sc);
        assert_eq!(c.highlight(&sc), Some(sel((0, 0), (9, 2))));
        // 선택이 통째로 화면 밖이면 그리지 않는다.
        typed(&mut c, &ls, "G");
        assert_eq!(c.highlight(&sc), None);
    }

    #[test]
    fn highlight_maps_char_index_to_grid_column_over_wide_chars() {
        let sc = screen(10, 3, "한글 ab".as_bytes());
        let ls = sc.history_text();
        assert_eq!(ls[0], "한글 ab");
        let mut c = CopyMode::new(&ls);
        // 한(칸 0~1) 글(칸 2~3) 공백(4) a(5) b(6)
        let cols: Vec<u16> = (0..5)
            .map(|_| {
                let col = c.highlight(&sc).unwrap().start.0;
                c.key(&ls, b'l');
                col
            })
            .collect();
        assert_eq!(cols, [0, 2, 4, 5, 6]);
    }

    /// 좁은 화면(10칸)에 줄바꿈 없이 25글자를 쓴다. 첫 줄 끝 칸이 공백이라 화면 줄로는 `abcdefghi`로 잘린다.
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
        // 처음부터 끝까지: 줄바꿈 자리의 공백은 글자라서 남고 `\n`은 없다.
        assert_eq!(
            copy_range(&sc, "gvjjllll"),
            CopyOut::Copy(WRAPPED.to_string())
        );
        // 이어진 줄 한가운데서 시작해 한가운데서 끝나도 같다.
        assert_eq!(
            copy_range(&sc, "gllllllvjjhh"),
            CopyOut::Copy("ghi jklmnopqrstuv".to_string())
        );
        // 이어진 두 줄만: 첫 줄 끝 공백이 남는다.
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
        // 이어진 줄에서 진짜 줄바꿈을 지나 다음 줄 일부까지: `\n`은 `tuvwx`와 `next` 사이 하나뿐이다.
        assert_eq!(
            copy_range(&sc, "gvjjjlll"),
            CopyOut::Copy(format!("{WRAPPED}\nnext"))
        );
        // 진짜 줄 둘은 그대로 `\n`으로 잇고 오른쪽 공백은 뗀다.
        assert_eq!(
            copy_range(&sc, "gvjjjjll"),
            CopyOut::Copy(format!("{WRAPPED}\nnext\nlas"))
        );
    }

    #[test]
    fn rows_that_do_not_wrap_stay_trimmed_and_newline_joined() {
        // 가득 찬 줄이 아니면 이어진 줄이 아니다.
        let sc = screen(10, 5, b"ab  \r\ncd");
        assert_eq!(copy_range(&sc, "gvjl"), CopyOut::Copy("ab\ncd".to_string()));
    }
}
