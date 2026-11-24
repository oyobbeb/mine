//! The launcher: the menu, drawn in the center, that starts a session with a single key. It shows when no session is attached or when ⌘N, ＋ or New task is pressed.
//! Same content as the stage 1 TUI's launcher (`mine-cli` `client::launcher`). `Model` handles the state and sending `New`.

use gpui::prelude::FluentBuilder;
use gpui::{
    Context, FontWeight, InteractiveElement, IntoElement, Keystroke, ParentElement,
    StatefulInteractiveElement, Styled, div, px, rgb,
};
use gpui_component::input::Input;
use gpui_component::{h_flex, v_flex};
use mine_core::accounts::Labels;
use mine_core::launch::{Choice, tilde};
use mine_core::view::folder_name;

use crate::sidebar::account_chip;
use crate::theme::*;
use crate::workspace::Workspace;

/// A menu row: (choice, key label). The row name is [`label`].
const ROWS: [(Choice, &str); 5] = [
    (Choice::ClaudeA, "↵"),
    (Choice::ClaudeB, "b"),
    (Choice::Codex, "x"),
    (Choice::Shell, "s"),
    (Choice::Typed, "n"),
];

/// Menu width and name-column width. The chip lines up at the same position after the name column.
const MENU_W: f32 = 340.;
const LABEL_W: f32 = 150.;

/// The text of a menu row as drawn on screen: (choice, key label, row name). The row name is [`Choice::label`], so only
/// the account rows carry the names from `labels`. Drawing and tests use the same text.
fn row_texts(labels: &Labels) -> Vec<(Choice, &'static str, String)> {
    ROWS.iter()
        .map(|&(choice, key)| (choice, key, choice.label(labels).to_string()))
        .collect()
}

/// The row that one key picks. Keys with modifiers (including ⇧) and any other key are ignored.
///
/// GPUI reports keys that bypass the input method (when focus isn't in a text input) as the layout's English key. So,
/// unlike in the TUI, `s` arrives as `s` even when the keyboard is switched to Hangul.
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

/// Esc without modifiers.
pub fn is_escape(k: &Keystroke) -> bool {
    k.key == "escape" && !k.modifiers.modified()
}

/// The launcher that fills the center. Order: header (project name and `~` path), "New session", menu rows, input box, hint.
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
    let rows = row_texts(&m.labels)
        .into_iter()
        .enumerate()
        .map(|(k, (choice, key, name))| {
            row(
                k,
                choice,
                key,
                name,
                m.typing && choice == Choice::Typed,
                cx,
            )
        })
        .collect::<Vec<_>>();
    let typed = ws.typed_input().filter(|_| m.typing).map(|input| {
        // The input box follows the parent's text size. Set it to 13px like the menu, not the root's 16px.
        v_flex()
            .mt_2()
            .gap_1()
            .text_size(px(13.))
            .child(Input::new(input))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(FAINT))
                    .child("Enter create · Esc back"),
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
                        .child("New session"),
                )
                .children(rows)
                .children(typed)
                .child(
                    div()
                        .mt_5()
                        .text_size(px(12.))
                        .text_color(rgb(FAINT))
                        .child("⌘W close"),
                ),
        )
}

/// One menu row: `↵  claude-a   [A]`. Clicking picks that row. The account name goes in the name slot.
fn row(
    k: usize,
    choice: Choice,
    key: &'static str,
    name: String,
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
        .child(div().w(px(LABEL_W)).text_color(rgb(TEXT)).child(name))
        .children(account_chip(choice.word()))
        .on_click(cx.listener(move |ws, _, window, cx| ws.launch(choice, window, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::labels_from;

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
        // The key shown on each row picks that row (Enter is `↵`).
        for (choice, shown) in ROWS {
            let source = if shown == "↵" { "enter" } else { shown };
            assert_eq!(k(source), Some(choice), "{shown}");
        }
        assert!(is_escape(&key("escape")));
        assert!(!is_escape(&key("cmd-escape")) && !is_escape(&key("n")));
    }

    #[test]
    fn rows_name_the_kinds_like_the_tui() {
        // The default before the name is read is the kind word.
        let rows = row_texts(&Labels::default());
        let names: Vec<_> = rows.iter().map(|(_, _, name)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "claude-a",
                "claude-b",
                "codex",
                "shell",
                "Custom name and model"
            ]
        );
        // Chips go only on the claude-a, claude-b and codex rows.
        let chips: Vec<_> = rows
            .iter()
            .map(|(choice, ..)| chip(choice.word()).map(|(letter, _)| letter))
            .collect();
        assert_eq!(chips, [Some("A"), Some("B"), Some("C"), None, None]);
    }

    #[test]
    fn rows_show_account_labels_and_keep_keys_and_choices() {
        // claude-a has a label, codex has an automatic name. claude-b has nothing set, so it shows the id. shell's label is not used.
        let config =
            "[accounts.claude-a]\nlabel = \"personal\"\n[accounts.shell]\nlabel = \"sh\"\n";
        let rows = row_texts(&labels_from(config));
        let shown: Vec<_> = rows
            .iter()
            .map(|(_, key, name)| (*key, name.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("↵", "personal"),
                ("b", "claude-b"),
                ("x", "Codex"),
                ("s", "shell"),
                ("n", "Custom name and model"),
            ]
        );
        // The value that picks a row is the kind word as is, regardless of the name (it becomes the session's account id).
        let words: Vec<_> = rows.iter().map(|(choice, ..)| choice.word()).collect();
        assert_eq!(
            words,
            [
                Some("claude-a"),
                Some("claude-b"),
                Some("codex"),
                Some("shell"),
                None
            ]
        );
    }
}
