//! The launcher: the menu, drawn in the center, that starts a session with a single key. It shows when no session is attached or when ⌘N, ＋ or New task is pressed.
//! Same content as the TUI's launcher (`mine-cli` `client::launcher`): one row per profile, then shell and the typed form. `Model` handles the state and sending `New`.

use gpui::prelude::FluentBuilder;
use gpui::{
    Context, FontWeight, InteractiveElement, IntoElement, Keystroke, ParentElement,
    StatefulInteractiveElement, Styled, div, px,
};
use gpui_component::input::Input;
use gpui_component::{h_flex, v_flex};
use mine_core::launch::{self, Choice, enter_choice, tilde};
use mine_core::profiles::ProfileInfo;
use mine_core::view::folder_name;

use crate::sidebar::chip;
use crate::theme::{self, Theme};
use crate::workspace::Workspace;

/// Minimum menu width, and the name column's minimum width. The chip lines up after the name column, and a longer row widens the menu.
///
/// ponytail: the name column isn't measured, so a label wider than `LABEL_W` pushes its own chip right. Measure with the window's
/// text system if labels that long become common.
const MENU_W: f32 = 340.;
const LABEL_W: f32 = 150.;

/// A menu row as drawn: what it picks, its key (blank past the ninth profile), its name, and for a profile its chip (letter, color)
/// and, if it isn't logged in, the login note. Drawing and tests use the same values.
struct Row {
    choice: Choice,
    key: String,
    label: String,
    chip: Option<(String, u32)>,
    hint: Option<String>,
}

fn row_texts(profiles: &[ProfileInfo]) -> Vec<Row> {
    launch::rows(profiles)
        .into_iter()
        .map(|(choice, key)| {
            let info = match &choice {
                Choice::Profile(id) => launch::profile(profiles, id),
                _ => None,
            };
            Row {
                key: key.map(String::from).unwrap_or_default(),
                label: choice.label(profiles).to_string(),
                chip: info.map(|p| (p.chip.clone(), p.rgb())),
                hint: info.and_then(launch::login_hint),
                choice,
            }
        })
        .collect()
}

/// The hint under the menu: what Enter starts, and ⌘W.
fn footer(profiles: &[ProfileInfo]) -> String {
    match enter_choice(profiles) {
        Some(choice) => format!("↵ {} · ⌘W close", choice.label(profiles)),
        None => "⌘W close".into(),
    }
}

/// The row that one key picks: Enter, `1`–`9`, `s` or `n`. Keys with modifiers (including ⇧) and any other key are ignored.
///
/// GPUI reports keys that bypass the input method (when focus isn't in a text input) as the layout's English key. So,
/// unlike in the TUI, `s` arrives as `s` even when the keyboard is switched to Hangul.
pub fn choice_of_key(k: &Keystroke, profiles: &[ProfileInfo]) -> Option<Choice> {
    if k.modifiers.modified() {
        return None;
    }
    let mut chars = k.key.chars();
    match (k.key.as_str(), chars.next(), chars.next()) {
        ("enter", ..) => enter_choice(profiles),
        (_, Some(key), None) => launch::choice_of_key(key, profiles),
        _ => None,
    }
}

/// Esc without modifiers.
pub fn is_escape(k: &Keystroke) -> bool {
    k.key == "escape" && !k.modifiers.modified()
}

/// The launcher that fills the center. Order: header (project name and `~` path), "New session", menu rows, input box, hint.
pub fn render(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let m = &ws.model;
    let t = theme::current(cx).clone();
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
                .text_color(t.text.hsla())
                .child(folder_name(project)),
        )
        // Zero width plus grow: a long path fills what the rows leave and truncates, instead of widening the menu.
        .child(
            div()
                .w_0()
                .flex_1()
                .truncate()
                .text_size(px(13.))
                .text_color(t.text_tertiary.hsla())
                .child(tilde(project, home.as_deref())),
        );
    let rows = row_texts(&m.profiles)
        .into_iter()
        .enumerate()
        .map(|(k, r)| {
            let selected = m.typing && r.choice == Choice::Typed;
            row(k, r, selected, &t, cx)
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
                    .text_color(t.text_tertiary.hsla())
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
        .child(
            v_flex()
                .min_w(px(MENU_W))
                .child(head)
                .child(
                    div()
                        .mt_5()
                        .mb_2()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text.hsla())
                        .child("New session"),
                )
                .children(rows)
                .children(typed)
                .child(
                    div()
                        .mt_5()
                        .text_size(px(12.))
                        .text_color(t.text_tertiary.hsla())
                        .child(footer(&m.profiles)),
                ),
        )
}

/// One menu row: `1  personal   [P]`, and the dim login note for a profile that isn't logged in. Clicking picks that row.
fn row(
    k: usize,
    r: Row,
    selected: bool,
    t: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let choice = r.choice;
    h_flex()
        .id(("launch", k))
        .h(px(30.))
        .px_2()
        .gap_3()
        .rounded(px(6.))
        .cursor_pointer()
        .text_size(px(13.))
        .when(selected, |this| this.bg(t.panel_strong.hsla()))
        .hover(|s| s.bg(t.panel_strong.hsla()))
        .child(
            div()
                .w(px(16.))
                .flex_shrink_0()
                .font_weight(FontWeight::BOLD)
                .text_color(t.text.hsla())
                .child(r.key),
        )
        .child(
            div()
                .min_w(px(LABEL_W))
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_color(t.text.hsla())
                .child(r.label),
        )
        .children(r.chip.map(|(letter, color)| chip(letter, color)))
        .children(r.hint.map(|hint| {
            div()
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_color(t.text_tertiary.hsla())
                .child(hint)
        }))
        .on_click(cx.listener(move |ws, _, window, cx| ws.launch(choice.clone(), window, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Model, builtin, profile};
    use mine_core::link::LinkEvent;
    use mine_core::profiles::ProfileKind;
    use mine_core::proto::FromDaemon;
    use std::path::PathBuf;

    fn key(source: &str) -> Keystroke {
        Keystroke::parse(source).unwrap()
    }

    #[test]
    fn keys_choose_one_row_and_everything_else_is_ignored() {
        let list = builtin();
        let k = |s: &str| choice_of_key(&key(s), &list);
        let p = |id: &str| Some(Choice::Profile(id.into()));
        assert_eq!(k("enter"), p("claude-a"));
        assert_eq!(k("1"), p("claude-a"));
        assert_eq!(k("2"), p("claude-b"));
        assert_eq!(k("3"), p("codex"));
        assert_eq!(k("s"), Some(Choice::Shell));
        assert_eq!(k("n"), Some(Choice::Typed));
        for ignored in [
            "a",
            "b",
            "x",
            "0",
            "4",
            "shift-2",
            "shift-enter",
            "cmd-n",
            "cmd-1",
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
        // Before the list arrives, Enter and digits do nothing.
        assert_eq!(choice_of_key(&key("enter"), &[]), None);
        assert_eq!(choice_of_key(&key("1"), &[]), None);
        assert_eq!(choice_of_key(&key("s"), &[]), Some(Choice::Shell));
        assert!(is_escape(&key("escape")));
        assert!(!is_escape(&key("cmd-escape")) && !is_escape(&key("n")));
    }

    #[test]
    fn app_launcher_rows_follow_profiles() {
        let mut m = Model::new(PathBuf::from("/p"));
        // Before the daemon sends the list, only shell and the typed form.
        let keys = |m: &Model| -> Vec<String> {
            row_texts(&m.profiles)
                .into_iter()
                .map(|row| row.key)
                .collect()
        };
        assert_eq!(keys(&m), ["s", "n"]);
        let mut list = builtin();
        let mut team = profile("team", ProfileKind::Claude, "team", "#98c379");
        team.logged_in = false;
        list.push(team);
        m.on_link(LinkEvent::Msg(FromDaemon::Profiles { profiles: list }));
        let rows = row_texts(&m.profiles);
        let shown: Vec<_> = rows
            .iter()
            .map(|row| (row.key.as_str(), row.label.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("1", "personal"),
                ("2", "work"),
                ("3", "Codex"),
                ("4", "team"),
                ("s", "shell"),
                ("n", "Custom name and model"),
            ]
        );
        // The chip is the profile's letter and color; shell and the typed form have none.
        let chips: Vec<_> = rows.iter().map(|row| row.chip.clone()).collect();
        assert_eq!(
            chips,
            [
                Some(("P".to_string(), 0xe5c07b)),
                Some(("W".to_string(), 0xc678dd)),
                Some(("C".to_string(), 0x56b6c2)),
                Some(("T".to_string(), 0x98c379)),
                None,
                None,
            ]
        );
        // Only the profile that isn't logged in carries the login note.
        let hints: Vec<_> = rows.iter().map(|row| row.hint.as_deref()).collect();
        assert_eq!(
            hints,
            [
                None,
                None,
                None,
                Some("Not logged in · mine profile login team"),
                None,
                None
            ]
        );
        // The value that picks a row is the profile id, regardless of its label.
        assert_eq!(rows[1].choice, Choice::Profile("claude-b".into()));
        // The footer names the Enter row.
        assert_eq!(footer(&m.profiles), "↵ personal · ⌘W close");
        assert_eq!(footer(&[]), "⌘W close");
    }
}
