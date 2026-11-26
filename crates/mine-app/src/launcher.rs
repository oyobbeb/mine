//! The launcher: the sheet, centered in the empty terminal pane, that starts a session with a single key. It shows when no session is
//! attached or when ⌘N, ＋ or New task is pressed. Same content as the TUI's launcher (`mine-cli` `client::launcher`): one row per
//! profile, then shell and the typed form. `Model` handles the state and sending `New`.
//!
//! Sizes follow the mockup's Launcher artboard as Chrome renders it: the sheet is 504 wide (460 plus 2 × 22 of content-box padding),
//! and each text sets the line height Chrome gives it.

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

use crate::glass;
use crate::sidebar::{chip, tracked};
use crate::terminal::FONT_FAMILY;
use crate::theme::{self, Theme};
use crate::workspace::Workspace;

/// The sheet's width, and the gap between its blocks and between its rows.
const SHEET_W: f32 = 504.;
const BLOCK_GAP: f32 = 14.;
const ROW_GAP: f32 = 4.;
/// A row's key capsule and chip, and the gap between a row's parts.
const KEY: f32 = 26.;
const CHIP: f32 = 20.;
const PART_GAP: f32 = 12.;

/// A menu row as drawn: what it picks, its key (blank past the ninth profile), its name, for a profile its chip (letter, color) and,
/// if it isn't logged in, the login note, and whether Enter picks it. Drawing and tests use the same values.
struct Row {
    choice: Choice,
    key: String,
    label: String,
    chip: Option<(String, u32)>,
    hint: Option<String>,
    enter: bool,
}

fn row_texts(profiles: &[ProfileInfo]) -> Vec<Row> {
    let enter = enter_choice(profiles);
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
                enter: enter.as_ref() == Some(&choice),
                choice,
            }
        })
        .collect()
}

/// The row Enter picks is lit: the typed form's while it is open, otherwise the default profile's.
fn lit(row: &Row, typing: bool) -> bool {
    if typing {
        row.choice == Choice::Typed
    } else {
        row.enter
    }
}

/// The hints under the menu, one part each: what Enter starts, or in the typed form what Enter and Esc do, then ⌘W.
fn footer(profiles: &[ProfileInfo], typing: bool) -> Vec<String> {
    let mut parts: Vec<String> = if typing {
        vec!["Enter create".into(), "Esc back".into()]
    } else {
        enter_choice(profiles)
            .map(|choice| format!("↵ {}", choice.label(profiles)))
            .into_iter()
            .collect()
    };
    parts.push("⌘W close".into());
    parts
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

/// The launcher that fills the center: a sheet centered in the pane. Order: the project name and `~` path, "New session", the menu
/// rows, the typed form's input, the hints.
pub fn render(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let m = &ws.model;
    let t = theme::current(cx).clone();
    let project = m.project();
    let home = std::env::home_dir();
    let head = v_flex()
        .px(px(8.))
        .gap(px(2.))
        .child(
            div()
                .truncate()
                .text_size(px(20.))
                .line_height(px(23.))
                .font_weight(FontWeight::BOLD)
                .text_color(t.text.hsla())
                .child(folder_name(project)),
        )
        .child(
            div()
                .truncate()
                .font_family(FONT_FAMILY)
                .text_size(px(12.))
                .line_height(px(14.))
                .text_color(t.text_secondary.hsla())
                .child(tilde(project, home.as_deref())),
        );
    let label = h_flex()
        .px(px(8.))
        .text_size(px(11.))
        .line_height(px(13.))
        .text_color(t.text_secondary.hsla())
        .children(tracked(&"New session".to_uppercase(), cx));
    let mut rows = v_flex().gap(px(ROW_GAP));
    for (k, r) in row_texts(&m.profiles).into_iter().enumerate() {
        // A line between the profiles and shell, 4 × 12 inside the rows' gaps.
        if k > 0 && r.choice == Choice::Shell {
            rows = rows.child(div().h(px(1.)).mx(px(12.)).my(px(4.)).bg(t.edge.hsla()));
        }
        let lit = lit(&r, m.typing);
        rows = rows.child(row(k, r, lit, &t, cx));
    }
    // The input is clear like the rows; it follows the parent's text size, 14 px like a row name.
    let typed = ws.typed_input().filter(|_| m.typing).map(|input| {
        glass::clear_capsule(&t)
            .px(glass::pad(6.))
            .py(glass::pad(4.))
            .text_size(px(14.))
            .child(Input::new(input).appearance(false))
    });
    let hints = h_flex()
        .justify_center()
        .gap(px(18.))
        .pt(px(4.))
        .text_size(px(12.))
        .line_height(px(15.))
        .text_color(t.text_secondary.hsla())
        .children(footer(&m.profiles, m.typing));
    div()
        .id("launcher")
        .flex_1()
        .min_h_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            glass::sheet(&t)
                .w(px(SHEET_W))
                .flex()
                .flex_col()
                .gap(px(BLOCK_GAP))
                .pt(glass::pad(26.))
                .px(glass::pad(22.))
                .pb(glass::pad(18.))
                .child(head)
                .child(label)
                .child(rows)
                .children(typed)
                .child(hints),
        )
}

/// One menu row: the key capsule, the chip (or its empty slot), the name, and at the right the login note of a profile that isn't
/// logged in, whose row reads dim. Clicking picks that row.
///
/// The sheet's stack is at the 0.92 alpha cap, so nothing on it adds a translucent fill: the lit row shows by its edge and highlight,
/// hover lights a row's edge, and the key capsules are clear, except the lit row's, which is the solid accent.
fn row(
    k: usize,
    r: Row,
    lit: bool,
    t: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    let choice = r.choice;
    let ink = if r.hint.is_some() {
        t.text_tertiary
    } else {
        t.text
    };
    let key = if lit {
        div().bg(t.accent.hsla()).text_color(t.on_accent.hsla())
    } else {
        glass::clear_capsule(t)
    };
    let key = key
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .min_w(px(KEY))
        .h(px(KEY))
        .px(glass::pad(6.))
        .rounded_full()
        .text_size(px(12.))
        .line_height(px(15.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(r.key);
    // Shell and the typed form keep the chip's slot, so the names line up.
    let chip = match r.chip {
        Some((letter, color)) => chip(letter, color).size(px(CHIP)).line_height(px(CHIP)),
        None => div().flex_shrink_0().w(px(CHIP)),
    };
    let row = glass::clear_row(t, lit)
        .id(("launch", k))
        .flex()
        .items_center()
        .gap(px(PART_GAP))
        .py(glass::pad(10.))
        .px(glass::pad(12.))
        .text_color(ink.hsla())
        .child(key)
        .child(chip)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(14.))
                .line_height(px(17.))
                .when(lit, |name| name.font_weight(FontWeight::SEMIBOLD))
                .child(r.label),
        )
        .children(r.hint.map(|hint| {
            div()
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_size(px(12.))
                .line_height(px(15.))
                .child(hint)
        }));
    glass::hover_edge(row, if lit { t.highlight } else { t.edge })
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
        assert_eq!(footer(&m.profiles, false), ["↵ personal", "⌘W close"]);
        assert_eq!(footer(&[], false), ["⌘W close"]);
    }

    /// The Enter row is lit (spec 4.3): the default profile's, or the typed form's while it is open. The footer then says what Enter
    /// and Esc do in the form.
    #[test]
    fn the_enter_row_is_lit() {
        let list = builtin();
        let lits = |typing| -> Vec<bool> {
            row_texts(&list)
                .iter()
                .map(|row| lit(row, typing))
                .collect()
        };
        assert_eq!(lits(false), [true, false, false, false, false]);
        assert_eq!(lits(true), [false, false, false, false, true]);
        assert!(row_texts(&[]).iter().all(|row| !lit(row, false)));
        assert_eq!(
            footer(&list, true),
            ["Enter create", "Esc back", "⌘W close"]
        );
    }
}
