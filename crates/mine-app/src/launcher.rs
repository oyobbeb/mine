//! 실행 화면: 붙은 세션이 없거나 ⌘N·＋·새 작업을 눌렀을 때 가운데에 그리는, 키 하나로 세션을 시작하는 메뉴.
//! 1단계 TUI의 시작 화면(`mine-cli` `client::launcher`)과 같은 내용이다. 상태와 `New` 전송은 `Model`이 한다.

use gpui::prelude::FluentBuilder;
use gpui::{
    Context, FontWeight, InteractiveElement, IntoElement, Keystroke, ParentElement,
    StatefulInteractiveElement, Styled, div, px, rgb,
};
use gpui_component::input::Input;
use gpui_component::{h_flex, v_flex};
use mine_core::launch::{Choice, tilde};
use mine_core::view::folder_name;

use crate::sidebar::account_chip;
use crate::theme::*;
use crate::workspace::Workspace;

/// 메뉴 줄: (선택, 키 표시). 줄 이름은 [`label`]이다.
const ROWS: [(Choice, &str); 5] = [
    (Choice::ClaudeA, "↵"),
    (Choice::ClaudeB, "b"),
    (Choice::Codex, "x"),
    (Choice::Shell, "s"),
    (Choice::Typed, "n"),
];

/// 메뉴 폭과 이름 칸 폭. 칩이 이름 칸 뒤에 같은 자리로 맞는다.
const MENU_W: f32 = 340.;
const LABEL_W: f32 = 150.;

/// 메뉴 줄 이름. 앞의 넷은 종류 말(`Choice::word`)이고, shell 말고는 그 말이 칩의 계정 이름이기도 하다.
fn label(choice: Choice) -> &'static str {
    choice.word().unwrap_or("이름·모델 직접 입력")
}

/// 한 키가 고르는 줄. 꾸밈키(⇧ 포함)가 붙은 키와 그 밖의 키는 무시한다.
///
/// GPUI는 입력기를 거치지 않는 키(포커스가 글 입력 칸이 아닐 때)를 배열의 영문 키로 알린다. 그래서 TUI와 달리
/// 한/영이 한글이어도 `s`가 `s`로 온다.
pub fn choice_of_key(k: &Keystroke) -> Option<Choice> {
    if k.modifiers.modified() {
        return None;
    }
    Some(match k.key.as_str() {
        "enter" => Choice::ClaudeA,
        "b" => Choice::ClaudeB,
        "x" => Choice::Codex,
        "s" => Choice::Shell,
        "n" => Choice::Typed,
        _ => return None,
    })
}

/// 꾸밈키 없는 Esc.
pub fn is_escape(k: &Keystroke) -> bool {
    k.key == "escape" && !k.modifiers.modified()
}

/// 가운데를 채우는 실행 화면. 머리(프로젝트 이름과 `~` 경로), "새 세션", 메뉴 줄, 입력 상자, 안내 순서다.
pub fn render(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let m = &ws.model;
    let project = m.project();
    let home = std::env::home_dir();
    let head = h_flex()
        .gap_3()
        .items_baseline()
        .min_w_0()
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(15.))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(TITLE))
                .child(folder_name(project)),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(13.))
                .text_color(rgb(FAINT))
                .child(tilde(project, home.as_deref())),
        );
    let rows = ROWS
        .iter()
        .enumerate()
        .map(|(k, &(choice, key))| row(k, choice, key, m.typing && choice == Choice::Typed, cx))
        .collect::<Vec<_>>();
    let typed = ws.typed_input().filter(|_| m.typing).map(|input| {
        // 입력 상자는 부모 글 크기를 따른다. 뿌리(16px)가 아니라 메뉴와 같은 13px로 둔다.
        v_flex()
            .mt_2()
            .gap_1()
            .text_size(px(13.))
            .child(Input::new(input))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(FAINT))
                    .child("Enter 만들기 · Esc 돌아가기"),
            )
    });
    div()
        .id("launcher")
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgb(WINDOW_BG))
        .child(
            v_flex()
                .w(px(MENU_W))
                .child(head)
                .child(
                    div()
                        .mt_5()
                        .mb_2()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TITLE))
                        .child("새 세션"),
                )
                .children(rows)
                .children(typed)
                .child(
                    div()
                        .mt_5()
                        .text_size(px(12.))
                        .text_color(rgb(FAINT))
                        .child("⌘W 닫기"),
                ),
        )
}

/// 메뉴 한 줄: `↵  claude-a   [A]`. 누르면 그 줄을 고른다.
fn row(
    k: usize,
    choice: Choice,
    key: &'static str,
    selected: bool,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    h_flex()
        .id(("launch", k))
        .h(px(30.))
        .px_2()
        .gap_3()
        .rounded(px(6.))
        .cursor_pointer()
        .text_size(px(13.))
        .when(selected, |this| this.bg(rgb(SELECTED_BG)))
        .hover(|s| s.bg(rgb(SELECTED_BG)))
        .child(
            div()
                .w(px(16.))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(TITLE))
                .child(key),
        )
        .child(
            div()
                .w(px(LABEL_W))
                .text_color(rgb(TEXT))
                .child(label(choice)),
        )
        .children(account_chip(choice.word()))
        .on_click(cx.listener(move |ws, _, window, cx| ws.launch(choice, window, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(source: &str) -> Keystroke {
        Keystroke::parse(source).unwrap()
    }

    #[test]
    fn keys_choose_one_row_and_everything_else_is_ignored() {
        use Choice::*;
        let k = |s: &str| choice_of_key(&key(s));
        assert_eq!(k("enter"), Some(ClaudeA));
        assert_eq!(k("b"), Some(ClaudeB));
        assert_eq!(k("x"), Some(Codex));
        assert_eq!(k("s"), Some(Shell));
        assert_eq!(k("n"), Some(Typed));
        for ignored in [
            "a",
            "shift-b",
            "shift-enter",
            "cmd-n",
            "cmd-s",
            "ctrl-x",
            "alt-s",
            "escape",
            "space",
            "tab",
            "up",
        ] {
            assert_eq!(k(ignored), None, "{ignored}");
        }
        // 줄마다 보이는 키가 그 줄을 고른다(Enter는 `↵`).
        for (choice, shown) in ROWS {
            let source = if shown == "↵" { "enter" } else { shown };
            assert_eq!(k(source), Some(choice), "{shown}");
        }
        assert!(is_escape(&key("escape")));
        assert!(!is_escape(&key("cmd-escape")) && !is_escape(&key("n")));
    }

    #[test]
    fn rows_name_the_kinds_like_the_tui() {
        let labels: Vec<_> = ROWS.iter().map(|&(c, _)| label(c)).collect();
        assert_eq!(
            labels,
            [
                "claude-a",
                "claude-b",
                "codex",
                "shell",
                "이름·모델 직접 입력"
            ]
        );
        // 칩은 claude-a·claude-b·codex 줄에만 붙는다.
        let chips: Vec<_> = ROWS
            .iter()
            .map(|&(c, _)| chip(c.word()).map(|(letter, _)| letter))
            .collect();
        assert_eq!(chips, [Some("A"), Some("B"), Some("C"), None, None]);
    }
}
