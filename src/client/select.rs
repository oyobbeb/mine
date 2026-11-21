//! 터미널 칸의 마우스: 세션에 넘길 SGR 다시 만들기, 드래그 선택과 복사(OSC 52), 선택 반전 그리기.
//! 좌표는 모두 터미널 칸 안 (칸, 줄) 0부터다. 바깥 화면 좌표는 `ui::hit`이 이미 바꿔 준다.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use crate::client::input::{MouseEv, MouseKind};
use crate::screen::Screen;

/// 한 번에 복사하는 글자의 최대 바이트.
///
/// ponytail: 100KB가 넘는 OSC 52는 터미널이 거부하는 일이 많다(base64로 늘면 더 크다). 그래서 글자 경계에서
/// 잘라 복사하고 알린다. 큰 선택을 나눠 보내거나 다른 경로로 내보내는 일은 필요해질 때 한다.
const MAX_COPY: usize = 100_000;

/// 지금 보이는 화면 위의 선택. 두 끝 칸을 모두 포함하고, 어느 쪽이 먼저여도 된다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub start: (u16, u16),
    pub end: (u16, u16),
}

impl Selection {
    /// 위쪽(같은 줄이면 왼쪽) 끝과 아래쪽 끝.
    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let key = |&(col, row): &(u16, u16)| (row, col);
        if key(&self.start) <= key(&self.end) {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }
}

/// 선택 영역의 글자. 스크롤해 둔 화면이면 그때 보이는 줄을 읽는다.
/// 줄 오른쪽 공백은 떼고 줄은 `\n`으로 잇되, 자동 줄바꿈으로 이어진 줄 사이에는 `\n`을 넣지 않는다.
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
        // 넓은 글자의 뒤 칸에서 시작해도 그 글자를 담는다.
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
        // 다음 줄로 이어지는 줄은 끝의 공백도 글자다.
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

/// 클립보드를 쓰는 OSC 52. 바깥 터미널이 받아 시스템 클립보드에 넣는다.
pub fn osc52(text: &str) -> Vec<u8> {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text)).into_bytes()
}

/// 세션이 마우스 보고(누름·끌기·움직임 중 하나)를 켰다.
pub fn wants_mouse(screen: &Screen) -> bool {
    screen.mode().intersects(TermMode::MOUSE_MODE)
}

/// 세션이 이 이벤트를 받겠다고 했는가. 끌기는 끌기(1002)나 움직임(1003)을 켠 세션만 받는다.
/// 바깥 터미널에는 1002를 켜 두므로, 누름(1000)만 켠 세션에는 끌기를 걸러 줘야 한다.
pub fn session_takes(screen: &Screen, ev: &MouseEv) -> bool {
    ev.kind != MouseKind::Drag
        || screen
            .mode()
            .intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
}

/// 터미널 칸 기준 좌표(`col`, `row`, 0부터)로 SGR 마우스 보고를 다시 만든다.
/// 눌림·끌기·휠은 `M`, 뗌은 `m`이고, 끌기는 단추 번호에 32를 더한다. 휠은 64(위)·65(아래)다.
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

/// 바깥 화면 좌표를 터미널 칸 안으로 눌러 칸 기준 좌표로 바꾼다. 칸 밖으로 끌어도 선택이 이어지게 한다.
pub fn local(term: Rect, col: u16, row: u16) -> (u16, u16) {
    (
        col.saturating_sub(term.x).min(term.width.saturating_sub(1)),
        row.saturating_sub(term.y)
            .min(term.height.saturating_sub(1)),
    )
}

/// 복사할 OSC 52 바이트와 상태줄 알림. 복사할 글자가 없으면 `None`이다.
pub fn copy(text: &str) -> Option<(Vec<u8>, String)> {
    if text.is_empty() {
        return None;
    }
    let mut end = text.len().min(MAX_COPY);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &text[..end];
    let note = if end < text.len() { " (잘림)" } else { "" };
    let message = format!("복사됨 {}자{note}", cut.chars().count());
    Some((osc52(cut), message))
}

/// 선택 영역을 반전으로 그린다. `ui::draw`가 그린 버퍼에 덧입힌다.
pub fn paint_selection(buf: &mut Buffer, term: Rect, sel: &Selection) {
    if term.is_empty() {
        return;
    }
    let ((c0, r0), (c1, r1)) = sel.ordered();
    let last = term.width.saturating_sub(1);
    for y in r0..=r1.min(term.height.saturating_sub(1)) {
        let from = if y == r0 { c0 } else { 0 };
        let to = if y == r1 { c1 } else { last }.min(last);
        for x in from..=to {
            buf[(term.x + x, term.y + y)]
                .modifier
                .toggle(Modifier::REVERSED);
        }
    }
}

/// 드래그 선택의 진행 상태. 누르면 시작하고, 끌면 늘어나며, 떼면 선택이 확정된다.
#[derive(Debug, Default)]
pub struct Select {
    /// 누른 칸. 뗄 때까지 있다.
    anchor: Option<(u16, u16)>,
    /// 누른 칸과 다른 칸까지 끌었을 때만 있다. 제자리 클릭은 선택이 아니다.
    sel: Option<Selection>,
}

impl Select {
    pub fn down(&mut self, at: (u16, u16)) {
        self.anchor = Some(at);
        self.sel = None;
    }

    pub fn drag(&mut self, at: (u16, u16)) {
        if let Some(start) = self.anchor {
            self.sel = (start != at).then_some(Selection { start, end: at });
        }
    }

    /// 뗀다. 복사할 선택이 있으면 돌려준다. 선택은 지우지 않아 반전이 남는다.
    pub fn up(&mut self, at: (u16, u16)) -> Option<Selection> {
        self.drag(at);
        self.anchor.take()?;
        self.sel
    }

    /// 누르고 있는 중인가.
    pub fn dragging(&self) -> bool {
        self.anchor.is_some()
    }

    pub fn selection(&self) -> Option<&Selection> {
        self.sel.as_ref()
    }

    /// 진행 중인 것까지 모두 버린다. 그려진 반전이 있었으면 `true`다.
    pub fn clear(&mut self) -> bool {
        let shown = self.sel.is_some();
        *self = Select::default();
        shown
    }
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
        // 넓은 글자의 뒤 칸에서 시작하거나 끝나도 그 글자를 한 번만 담는다.
        assert_eq!(selected_text(&sc, &sel((1, 0), (2, 0))), "한글");
        assert_eq!(selected_text(&sc, &sel((0, 0), (6, 0))), "한글 ab");
    }

    #[test]
    fn osc52_format() {
        assert_eq!(osc52("hi"), b"\x1b]52;c;aGk=\x07".to_vec());
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
        // 켜지 않은 SGR 형식(1006)만으로는 마우스를 쓰지 않는다.
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
        // 같은 줄에서 왼쪽으로 끈다.
        assert_eq!(selected_text(&sc, &sel((4, 0), (0, 0))), "hello");
        // 위쪽 줄의 오른쪽 칸으로 끈다.
        assert_eq!(selected_text(&sc, &sel((0, 1), (3, 0))), "lo\nw");
    }

    #[test]
    fn selection_reads_the_scrolled_view() {
        let mut sc = Screen::new(20, 3);
        for i in 0..10 {
            sc.feed(format!("line{i}\r\n").as_bytes());
        }
        // 맨 아래에는 line8, line9, 빈 줄이 보인다.
        assert_eq!(selected_text(&sc, &sel((0, 0), (4, 1))), "line8\nline9");
        sc.scroll(4);
        assert_eq!(
            selected_text(&sc, &sel((0, 0), (4, 2))),
            "line4\nline5\nline6"
        );
        // 보이는 줄을 읽어야 하므로 스크롤 위치마다 다르다.
        sc.scroll(-3);
        assert_eq!(selected_text(&sc, &sel((0, 0), (4, 0))), "line7");
    }

    #[test]
    fn soft_wrapped_line_joins_without_newline() {
        let sc = screen(5, 3, b"abcdefgh\r\nxy");
        assert_eq!(selected_text(&sc, &sel((0, 0), (4, 2))), "abcdefgh\nxy");
        // 줄바꿈 자리의 공백은 글자라서 떼지 않는다.
        let sc = screen(5, 3, b"abcd efgh");
        assert_eq!(selected_text(&sc, &sel((0, 0), (3, 1))), "abcd efgh");
        // 선택이 이어진 줄 한가운데서 끝나면 그 끝에서 자른다.
        assert_eq!(selected_text(&sc, &sel((0, 0), (4, 0))), "abcd");
    }

    #[test]
    fn selection_is_clamped_to_the_screen() {
        let sc = screen(10, 2, b"abc\r\ndef");
        assert_eq!(selected_text(&sc, &sel((0, 0), (99, 99))), "abc\ndef");
    }

    #[test]
    fn plain_click_is_not_a_selection() {
        let mut s = Select::default();
        s.down((3, 2));
        assert!(s.dragging());
        assert_eq!(s.up((3, 2)), None);
        assert!(!s.dragging());
        assert_eq!(s.selection(), None);
    }

    #[test]
    fn drag_then_up_yields_the_selection() {
        let mut s = Select::default();
        s.down((3, 2));
        s.drag((5, 2));
        assert_eq!(s.selection(), Some(&sel((3, 2), (5, 2))));
        // 뗄 때 위치까지 늘린다.
        assert_eq!(s.up((6, 3)), Some(sel((3, 2), (6, 3))));
        assert!(!s.dragging());
        // 뗀 뒤에도 반전은 남고, 지우면 있었다고 알린다.
        assert_eq!(s.selection(), Some(&sel((3, 2), (6, 3))));
        assert!(s.clear());
        assert!(!s.clear());
        assert_eq!(s.selection(), None);
    }

    #[test]
    fn dragging_back_to_the_start_cancels_the_selection() {
        let mut s = Select::default();
        s.down((3, 2));
        s.drag((6, 2));
        s.drag((3, 2));
        assert_eq!(s.up((3, 2)), None);
    }

    #[test]
    fn down_clears_the_previous_selection_and_stray_events_do_nothing() {
        let mut s = Select::default();
        s.down((0, 0));
        s.drag((4, 0));
        s.up((4, 0));
        s.down((1, 1));
        assert_eq!(s.selection(), None);
        assert_eq!(s.up((1, 1)), None);
        // 누르지 않았으면 끌기와 뗌은 무시한다.
        s.drag((2, 2));
        assert_eq!(s.selection(), None);
        assert_eq!(s.up((2, 2)), None);
    }

    #[test]
    fn copy_reports_char_count_and_skips_empty() {
        assert_eq!(copy(""), None);
        let (bytes, msg) = copy("한글hi").unwrap();
        assert_eq!(bytes, osc52("한글hi"));
        assert_eq!(msg, "복사됨 4자");
    }

    #[test]
    fn copy_truncates_big_selections_at_a_char_boundary() {
        // 가는 3바이트라 100_000바이트 경계(100_000 = 3 * 33333 + 1)가 글자 한가운데다.
        let text = "가".repeat(40_000);
        let (bytes, msg) = copy(&text).unwrap();
        assert_eq!(msg, "복사됨 33333자 (잘림)");
        assert_eq!(bytes, osc52(&"가".repeat(33_333)));
        let (_, msg) = copy(&"a".repeat(100_000)).unwrap();
        assert_eq!(msg, "복사됨 100000자");
    }

    #[test]
    fn local_clamps_into_the_term_area() {
        let term = Rect::new(34, 0, 20, 10);
        assert_eq!(local(term, 40, 5), (6, 5));
        assert_eq!(local(term, 10, 99), (0, 9));
        assert_eq!(local(term, 99, 0), (19, 0));
    }

    #[test]
    fn paint_inverts_selected_cells_only() {
        let term = Rect::new(2, 1, 6, 3);
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 5));
        paint_selection(&mut buf, term, &sel((4, 2), (1, 1)));
        let reversed = |x: u16, y: u16| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        // 위 줄은 (1,1)부터 줄 끝까지, 아래 줄은 줄 처음부터 (4,2)까지다.
        let on: Vec<(u16, u16)> = (0..10)
            .flat_map(|x| (0..5).map(move |y| (x, y)))
            .filter(|&(x, y)| reversed(x, y))
            .collect();
        let mut want: Vec<(u16, u16)> = (3..8).map(|x| (x, 2)).collect();
        want.extend((2..7).map(|x| (x, 3)));
        want.sort_unstable();
        assert_eq!(on, want);
    }
}
