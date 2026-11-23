//! 사용자가 승인한 목업 색과 터미널 색. 값은 GPUI `rgb()`에 넘기는 `0xrrggbb`다.

use gpui::{App, Hsla, rgb};
use gpui_component::{Theme, ThemeMode};

/// 창 바탕.
pub const WINDOW_BG: u32 = 0x1e2128;
/// 왼쪽 목록 바탕.
pub const LIST_BG: u32 = 0x23252b;
/// 제목줄과 띠.
pub const BAR_BG: u32 = 0x1b1d23;
/// 선택한 줄, 누를 수 있는 줄에 마우스를 올렸을 때.
pub const SELECTED_BG: u32 = 0x2f323a;
pub const BORDER: u32 = 0x2c2f36;
pub const TEXT: u32 = 0xd0d3da;
pub const TITLE: u32 = 0xe6e6e6;
/// 흐린 글(묶음 이름, 둘째 줄).
pub const MUTED: u32 = 0x8a8f99;
/// 더 흐린 글(개수, 단축키, 안내).
pub const FAINT: u32 = 0x6b707a;
/// 승인 대기 세션의 대기 문구.
pub const WAITING: u32 = 0xc9a85c;
/// 승인 대기 기호 ◆.
pub const APPROVAL: u32 = 0xe5c07b;
/// 작업 중 기호 ◐와 끝남 기호 ✓. TUI 목록과 같은 색이다.
pub const WORKING: u32 = 0x56b6c2;
pub const DONE: u32 = 0x98c379;
/// 계정 칩: claude-a, claude-b, codex.
pub const CHIP_A: u32 = 0xe5c07b;
pub const CHIP_B: u32 = 0xc678dd;
pub const CHIP_C: u32 = 0x56b6c2;
/// 칩 글자색.
pub const CHIP_FG: u32 = 0x1e2128;

/// 터미널 기본 글과 바탕. 앱은 붙을 때 이 두 색을 `ToDaemon::Colors`로 데몬에 보낸다.
pub const TERM_FG: u32 = 0xd0d3da;
pub const TERM_BG: u32 = 0x1e2128;
#[allow(dead_code, reason = "Task 4의 터미널 칸이 쓴다")]
pub const TERM_CURSOR: u32 = 0xd0d3da;

/// ANSI 0–15.
#[allow(dead_code, reason = "Task 4의 터미널 칸이 쓴다")]
pub const ANSI: [u32; 16] = [
    0x1e2128, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xd0d3da, //
    0x5c6370, 0xef8a92, 0xb5d99c, 0xf0d197, 0x8cc5f5, 0xd898e6, 0x7fcbd5, 0xffffff,
];

/// 256색 팔레트의 `i`번 색. 0–15는 [`ANSI`], 16–231은 xterm 6×6×6 큐브, 232–255는 회색 단계다.
#[allow(dead_code, reason = "Task 4의 터미널 칸이 쓴다")]
pub fn indexed(i: u8) -> u32 {
    const LEVELS: [u32; 6] = [0, 95, 135, 175, 215, 255];
    match i {
        0..=15 => ANSI[usize::from(i)],
        16..=231 => {
            let n = u32::from(i - 16);
            let (r, g, b) = (
                LEVELS[(n / 36) as usize],
                LEVELS[(n / 6 % 6) as usize],
                LEVELS[(n % 6) as usize],
            );
            r << 16 | g << 8 | b
        }
        232..=255 => {
            let v = 8 + 10 * u32::from(i - 232);
            v << 16 | v << 8 | v
        }
    }
}

/// `#rrggbb` 문자열.
pub fn hex(color: u32) -> String {
    format!("#{color:06x}")
}

/// 계정 칩의 글자와 바탕색. claude-a `A`, claude-b `B`, codex `C`이고 그 밖(shell)은 칩이 없다.
pub fn chip(account: Option<&str>) -> Option<(&'static str, u32)> {
    match account? {
        "claude-a" => Some(("A", CHIP_A)),
        "claude-b" => Some(("B", CHIP_B)),
        "codex" => Some(("C", CHIP_C)),
        _ => None,
    }
}

fn hsla(color: u32) -> Hsla {
    rgb(color).into()
}

/// `gpui-component` 테마를 어두운 판으로 두고 목업 색을 입힌다. 시스템이 밝은 모양이어도 같은 색이다.
pub fn install(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let t = Theme::global_mut(cx);
    t.background = hsla(WINDOW_BG);
    t.foreground = hsla(TEXT);
    t.border = hsla(BORDER);
    t.muted_foreground = hsla(MUTED);
    t.title_bar = hsla(BAR_BG);
    t.title_bar_border = hsla(BORDER);
    t.sidebar = hsla(LIST_BG);
    t.sidebar_border = hsla(BORDER);
    t.sidebar_foreground = hsla(TEXT);
    t.sidebar_accent = hsla(SELECTED_BG);
    t.sidebar_accent_foreground = hsla(TITLE);
    t.scrollbar = hsla(LIST_BG);
    t.scrollbar_thumb = hsla(SELECTED_BG);
    t.scrollbar_thumb_hover = hsla(BORDER);
    // 입력 상자(실행 화면의 `n`): 테두리는 경계색, 포커스 테두리는 흐린 글색, 커서는 글색이다.
    t.input = hsla(BORDER);
    t.ring = hsla(MUTED);
    t.caret = hsla(TEXT);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_matches_xterm() {
        assert_eq!(indexed(1), 0xe06c75);
        assert_eq!(indexed(15), 0xffffff);
        assert_eq!(indexed(16), 0x000000);
        assert_eq!(indexed(21), 0x0000ff);
        assert_eq!(indexed(196), 0xff0000);
        assert_eq!(indexed(231), 0xffffff);
        assert_eq!(indexed(232), 0x080808);
        assert_eq!(indexed(255), 0xeeeeee);
        assert_eq!(hex(TERM_BG), "#1e2128");
    }

    #[test]
    fn chips_follow_accounts() {
        assert_eq!(chip(Some("claude-a")), Some(("A", CHIP_A)));
        assert_eq!(chip(Some("claude-b")), Some(("B", CHIP_B)));
        assert_eq!(chip(Some("codex")), Some(("C", CHIP_C)));
        assert_eq!(chip(None), None);
        assert_eq!(chip(Some("shell")), None);
    }
}
