//! 터미널 칸의 마우스 중 화면에 묶인 부분: 드래그 선택과 복사(OSC 52), 선택 반전 그리기.
//! SGR 다시 만들기와 선택 글자 읽기는 `mine_core::select`가 한다. 좌표는 모두 터미널 칸 안 (칸, 줄) 0부터다. 바깥 화면 좌표는 `ui::hit`이 이미 바꿔 준다.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use mine_core::select::Selection;

/// 한 번에 복사하는 글자의 최대 바이트.
///
/// ponytail: 100KB가 넘는 OSC 52는 터미널이 거부하는 일이 많다(base64로 늘면 더 크다). 그래서 글자 경계에서
/// 잘라 복사하고 알린다. 큰 선택을 나눠 보내거나 다른 경로로 내보내는 일은 필요해질 때 한다.
const MAX_COPY: usize = 100_000;

/// 클립보드를 쓰는 OSC 52. 바깥 터미널이 받아 시스템 클립보드에 넣는다.
pub fn osc52(text: &str) -> Vec<u8> {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text)).into_bytes()
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
    let note = if end < text.len() { " (truncated)" } else { "" };
    let message = format!("Copied {} chars{note}", cut.chars().count());
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

    fn sel(start: (u16, u16), end: (u16, u16)) -> Selection {
        Selection { start, end }
    }

    #[test]
    fn osc52_format() {
        assert_eq!(osc52("hi"), b"\x1b]52;c;aGk=\x07".to_vec());
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
        assert_eq!(msg, "Copied 4 chars");
    }

    #[test]
    fn copy_truncates_big_selections_at_a_char_boundary() {
        // 가는 3바이트라 100_000바이트 경계(100_000 = 3 * 33333 + 1)가 글자 한가운데다.
        let text = "가".repeat(40_000);
        let (bytes, msg) = copy(&text).unwrap();
        assert_eq!(msg, "Copied 33333 chars (truncated)");
        assert_eq!(bytes, osc52(&"가".repeat(33_333)));
        let (_, msg) = copy(&"a".repeat(100_000)).unwrap();
        assert_eq!(msg, "Copied 100000 chars");
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
