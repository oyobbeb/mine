//! The start screen (launcher). A menu drawn in the middle of the terminal pane when no session is attached or `⌃a c` is pressed, for starting a session with one key.
//! It handles only pure functions, such as key interpretation and row hit-testing, and drawing; state and sending `New` are `App`'s job.
//! The rows, their keys, choosing a new name and the session spec for the chosen row are in `mine_core::launch`.

use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use mine_core::launch::{self, Choice, enter_choice, tilde};
use mine_core::profiles::ProfileInfo;
use mine_core::view::folder_name;

use crate::client::ui::{HEAD, MUTED, chip};

/// Key hints on the right of the status line while the launcher is open: what Enter starts, the digit range, and shell.
pub fn keys(profiles: &[ProfileInfo]) -> String {
    let enter = enter_choice(profiles).map(|choice| format!("↵ {}", choice.label(profiles)));
    let digits = match profiles.len().min(9) {
        0 => None,
        1 => Some("1 profile".to_string()),
        n => Some(format!("1–{n} profiles")),
    };
    enter
        .into_iter()
        .chain(digits)
        .chain(["s shell".to_string()])
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The body that follows the top two rows (the project header and a blank row): `New session`, a blank row, the menu rows, a blank row, the hint.
/// The menu rows start at the fourth row from the top (0-based).
const OPTIONS_AT: u16 = 4;
/// Default width of the name column. Chips line up in the same column after it.
const LABEL_W: usize = 10;

/// Menu height: everything from the project header to the hint.
fn height(profiles: &[ProfileInfo]) -> u16 {
    OPTIONS_AT + launch::rows(profiles).len() as u16 + 2
}

/// The row a key picks. Counted only when the bytes read at once are a single key: Enter, `1`–`9`, `s` or `n`. Sequences starting
/// with Esc (arrows, encoded keys) and other characters are ignored.
///
/// ponytail: Hangul (`ㄴ` `ㅜ` etc.) isn't mapped to English keys. The macOS input method holds a jamo until the next key, so mapping it
/// would fire late or not at all. Instead `App` tells the user to switch to English (`input::HANGUL_HINT`).
pub fn choice_of_key(bytes: &[u8], profiles: &[ProfileInfo]) -> Option<Choice> {
    match bytes {
        b"\r" | b"\n" => enter_choice(profiles),
        &[key] => launch::choice_of_key(char::from(key), profiles),
        _ => None,
    }
}

/// Width of the name column. This is the default; if a profile's name is longer, the chips are aligned in the same column to match.
fn label_w(profiles: &[ProfileInfo]) -> usize {
    profiles
        .iter()
        .map(|p| Span::raw(p.label.as_str()).width())
        .fold(LABEL_W, usize::max)
}

/// The menu body rows. Independent of the project. A profile row is its key, label and chip, and a dim login note if it isn't logged in.
fn body(profiles: &[ProfileInfo]) -> Vec<Line<'static>> {
    let key_style = Style::new().add_modifier(Modifier::BOLD);
    let label_w = label_w(profiles);
    let mut out = vec![Line::styled("New session", HEAD), Line::default()];
    out.extend(launch::rows(profiles).into_iter().map(|(choice, key)| {
        let label = choice.label(profiles);
        let pad = label_w.saturating_sub(Span::raw(label).width());
        let mut spans = vec![
            Span::styled(key.unwrap_or(' ').to_string(), key_style),
            Span::raw(format!("  {label}{}", " ".repeat(pad))),
        ];
        // The chip is the same as in the list. Rows without a profile (shell, custom input) have none.
        if let Choice::Profile(id) = &choice
            && let Some(info) = launch::profile(profiles, id)
        {
            spans.push(Span::raw("  "));
            spans.push(chip(info));
            if let Some(hint) = launch::login_hint(info) {
                spans.push(Span::styled(format!("  {hint}"), MUTED));
            }
        }
        Line::from(spans)
    }));
    out.push(Line::default());
    out.push(Line::styled("⌃a d detach", MUTED));
    out
}

/// Menu width: the longest row. Measured without the project header (the path can be long), so the menu doesn't shift with path length.
fn menu_width(profiles: &[ProfileInfo]) -> u16 {
    body(profiles).iter().map(Line::width).max().unwrap_or(0) as u16
}

/// Top left of the menu centered in terminal pane `term`. If the pane is small, it sticks to the pane's left and top.
fn origin(term: Rect, profiles: &[ProfileInfo]) -> (u16, u16) {
    (
        term.x + term.width.saturating_sub(menu_width(profiles)) / 2,
        term.y + term.height.saturating_sub(height(profiles)) / 2,
    )
}

/// The menu row at `(col, row)` (outer screen coordinates). Only counts within the drawn width.
pub fn hit(term: Rect, col: u16, row: u16, profiles: &[ProfileInfo]) -> Option<Choice> {
    let (x, y) = origin(term, profiles);
    let i = row.checked_sub(y + OPTIONS_AT)?;
    let (choice, _) = launch::rows(profiles).into_iter().nth(usize::from(i))?;
    (col >= x && col - x < menu_width(profiles)).then_some(choice)
}

/// Draws the menu centered in the terminal pane. Draws only text over the default background. Anything beyond the pane is clipped.
///
/// ponytail: If the project path is long, the right side is cut off. Shortening the front with `…` can wait until it's needed.
pub fn draw(buf: &mut Buffer, term: Rect, project: &Path, profiles: &[ProfileInfo]) {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let head = Line::from(vec![
        Span::styled(
            folder_name(project),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {}", tilde(project, home.as_deref())), MUTED),
    ]);
    let (x, top) = origin(term, profiles);
    let lines = [head, Line::default()].into_iter().chain(body(profiles));
    for (y, line) in (top..term.bottom()).zip(lines) {
        if x < term.right() {
            buf.set_line(x, y, &line, term.right() - x);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ui::{builtin, profile};
    use mine_core::profiles::ProfileKind;
    use ratatui::style::Color;

    #[test]
    fn keys_choose_one_row_and_everything_else_is_ignored() {
        let list = builtin();
        let k = |bytes: &[u8]| choice_of_key(bytes, &list);
        let p = |id: &str| Some(Choice::Profile(id.into()));
        // Enter is the default Claude profile; digits are the rows in order.
        assert_eq!(k(b"\r"), p("claude-a"));
        assert_eq!(k(b"\n"), p("claude-a"));
        assert_eq!(k(b"1"), p("claude-a"));
        assert_eq!(k(b"2"), p("claude-b"));
        assert_eq!(k(b"3"), p("codex"));
        assert_eq!(k(b"s"), Some(Choice::Shell));
        assert_eq!(k(b"n"), Some(Choice::Typed));
        for ignored in [
            &b"a"[..],
            b"b",
            b"x",
            b"0",
            b"4",
            b"S",
            b" ",
            b"\x7f",
            b"\x1b",
            b"\x1b[B",
            b"\x1b[115u",
            b"sb",
            b"12",
            b"",
            "ㄴ".as_bytes(),
        ] {
            assert_eq!(k(ignored), None, "{ignored:?}");
        }
        // Before the list arrives, Enter and digits do nothing.
        assert_eq!(choice_of_key(b"\r", &[]), None);
        assert_eq!(choice_of_key(b"1", &[]), None);
        assert_eq!(choice_of_key(b"s", &[]), Some(Choice::Shell));
    }

    /// The terminal pane. It must be centered even if the position isn't (34, 0).
    const TERM: Rect = Rect::new(34, 0, 86, 39);

    fn drawn(term: Rect, project: &str) -> Buffer {
        drawn_with(term, project, &builtin())
    }

    fn drawn_with(term: Rect, project: &str, profiles: &[ProfileInfo]) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 120, 40));
        draw(&mut buf, term, Path::new(project), profiles);
        buf
    }

    /// The characters of one row (cells after a wide character are skipped) and the column of the first character.
    fn row(buf: &Buffer, y: u16) -> Option<(u16, String)> {
        let (mut text, mut first, mut x) = (String::new(), None, 0);
        while x < buf.area.width {
            let sym = buf[(x, y)].symbol();
            if sym != " " {
                first.get_or_insert(x);
            }
            text.push_str(sym);
            x += Span::raw(sym).width().max(1) as u16;
        }
        first.map(|x| (x, text.trim().to_string()))
    }

    fn rows(buf: &Buffer) -> Vec<(u16, u16, String)> {
        (0..buf.area.height)
            .filter_map(|y| row(buf, y).map(|(x, t)| (y, x, t)))
            .collect()
    }

    #[test]
    fn draws_the_menu_centred_in_the_term_pane() {
        let buf = drawn(TERM, "/p/kyoto");
        let rows = rows(&buf);
        let texts: Vec<&str> = rows.iter().map(|(_, _, t)| t.as_str()).collect();
        assert_eq!(
            texts,
            [
                "kyoto  /p/kyoto",
                "New session",
                "1  personal    P",
                "2  work        W",
                "3  Codex       C",
                "s  shell",
                "n  Custom name and model",
                "⌃a d detach",
            ]
        );
        // The block of eleven rows (blank rows included) is vertically centered, and the block of menu width is horizontally centered.
        let top = rows[0].0;
        let bottom = rows.last().unwrap().0;
        let height = height(&builtin());
        assert_eq!((bottom - top + 1, top), (11, (TERM.height - height) / 2));
        let width = menu_width(&builtin());
        assert_eq!(width, 24);
        let left = TERM.x + (TERM.width - width) / 2;
        for (_, x, _) in &rows {
            assert_eq!(*x, left);
        }
        // Leave everything outside the terminal pane untouched.
        for y in 0..40 {
            for x in (0..TERM.x).chain(TERM.right()..120) {
                assert_eq!(buf[(x, y)].symbol(), " ", "({x},{y})");
            }
        }
    }

    #[test]
    fn before_the_profile_list_only_shell_and_the_typed_form_show() {
        let texts: Vec<String> = rows(&drawn_with(TERM, "/p/kyoto", &[]))
            .into_iter()
            .map(|(_, _, t)| t)
            .collect();
        assert_eq!(
            texts[2..],
            ["s  shell", "n  Custom name and model", "⌃a d detach"]
        );
        assert_eq!(keys(&[]), "s shell");
    }

    #[test]
    fn chips_are_drawn_like_the_list_chips_and_the_rest_is_plain() {
        let buf = drawn(TERM, "/p/kyoto");
        let at = |text: &str| {
            let (y, x, _) = rows(&buf)
                .into_iter()
                .find(|r| r.2.starts_with(text))
                .unwrap();
            (x, y)
        };
        let (x, y) = at("1");
        assert!(buf[(x, y)].modifier.contains(Modifier::BOLD));
        // The chip is the profile's letter on the profile's color, the same as the list's.
        let chip = |text: &str| {
            let (x, y) = at(text);
            let c = &buf[(x + 15, y)];
            (c.symbol().to_string(), c.bg)
        };
        assert_eq!(chip("1"), ("P".into(), Color::Rgb(0xe5, 0xc0, 0x7b)));
        assert_eq!(chip("2"), ("W".into(), Color::Rgb(0xc6, 0x78, 0xdd)));
        assert_eq!(chip("3"), ("C".into(), Color::Rgb(0x56, 0xb6, 0xc2)));
        // The shell row has no chip, and its background isn't painted (default background).
        let (x, y) = at("s");
        assert_eq!(buf[(x + 15, y)].symbol(), " ");
        assert_eq!(buf[(x + 15, y)].bg, Color::Reset);
        let (hx, hy) = at("kyoto");
        assert_eq!(buf[(hx + 8, hy)].fg, Color::Rgb(0x6b, 0x70, 0x7a));
    }

    #[test]
    fn rows_show_profile_labels_and_the_chips_stay_in_one_column() {
        // A wide label, one longer than the column, and a ninth-plus profile without a key.
        let mut list = vec![
            profile("claude-a", ProfileKind::Claude, "개인", "#e5c07b"),
            profile("claude-b", ProfileKind::Claude, "company-work", "#c678dd"),
        ];
        list.extend((3..=10).map(|n| {
            profile(
                &format!("p{n}"),
                ProfileKind::Codex,
                &format!("p{n}"),
                "#56b6c2",
            )
        }));
        let buf = drawn_with(TERM, "/p/kyoto", &list);
        let rows = rows(&buf);
        let texts: Vec<&str> = rows.iter().map(|(_, _, t)| t.as_str()).collect();
        // The name column fits the longest name (12 columns), and all chips are in the same column after it.
        assert_eq!(texts[2..4], ["1  개인          개", "2  company-work  C"]);
        assert_eq!(texts[10], "9  p9            P");
        // The tenth profile has no key; its row starts with the blank key column.
        assert_eq!(texts[11], "p10           P");
        assert_eq!(rows[11].1, rows[10].1 + 3);
        // The hit test on pressing a row matches the drawn rows, keyless rows included.
        let (y, left) = (rows[3].0, rows[3].1);
        let b = Some(Choice::Profile("claude-b".into()));
        assert_eq!(hit(TERM, left, y, &list), b);
        assert_eq!(hit(TERM, left + 17, y, &list), b);
        assert_eq!(
            hit(TERM, left, rows[11].0, &list),
            Some(Choice::Profile("p10".into()))
        );
        // The status line hint names the Enter row and the digit range.
        assert_eq!(keys(&list), "↵ 개인 · 1–9 profiles · s shell");
        assert_eq!(keys(&list[..1]), "↵ 개인 · 1 profile · s shell");
    }

    #[test]
    fn not_logged_in_rows_show_the_login_command_dim_and_widen_the_menu() {
        let mut list = builtin();
        list[1].logged_in = false;
        let buf = drawn_with(TERM, "/p/kyoto", &list);
        let rows = rows(&buf);
        let (y, x, text) = &rows[3];
        assert_eq!(
            text,
            "2  work        W  Not logged in · mine profile login claude-b"
        );
        assert_eq!(buf[(*x + 18, *y)].fg, Color::Rgb(0x6b, 0x70, 0x7a));
        // The menu is as wide as its longest row, and still centered.
        let width = menu_width(&list);
        assert_eq!(width as usize, Span::raw(text.as_str()).width());
        assert_eq!(*x, TERM.x + (TERM.width - width) / 2);
    }

    #[test]
    fn hit_maps_option_rows_and_nothing_else() {
        let buf = drawn(TERM, "/p/kyoto");
        let rows = rows(&buf);
        let y_of = |text: &str| rows.iter().find(|r| r.2.starts_with(text)).unwrap().0;
        let list = builtin();
        let left = TERM.x + (TERM.width - menu_width(&list)) / 2;
        let pick = |y, x| hit(TERM, x, y, &list);
        let p = |id: &str| Some(Choice::Profile(id.into()));
        assert_eq!(pick(y_of("1"), left), p("claude-a"));
        assert_eq!(pick(y_of("2"), left + 5), p("claude-b"));
        assert_eq!(pick(y_of("3"), left + 21), p("codex"));
        assert_eq!(pick(y_of("s"), left + 3), Some(Choice::Shell));
        assert_eq!(pick(y_of("n"), left + 10), Some(Choice::Typed));
        // Beside the rows (outside the menu width) and the title, header and hint rows are not rows.
        assert_eq!(pick(y_of("1"), left - 1), None);
        assert_eq!(pick(y_of("1"), left + menu_width(&list)), None);
        for text in ["kyoto", "New session", "⌃a d"] {
            assert_eq!(pick(y_of(text), left + 1), None, "{text}");
        }
        assert_eq!(pick(y_of("1") - 1, left), None);
        assert_eq!(pick(y_of("n") + 1, left), None);
    }

    #[test]
    fn a_pane_too_small_for_the_menu_is_clipped_not_panicked() {
        for term in [
            Rect::new(34, 0, 10, 5),
            Rect::new(34, 0, 86, 3),
            Rect::new(34, 0, 1, 1),
        ] {
            let buf = drawn(
                term,
                "/some/very/long/project/path/that/does/not/fit/in/the/pane",
            );
            for y in 0..40 {
                for x in (0..term.x).chain(term.right()..120) {
                    assert_eq!(buf[(x, y)].symbol(), " ");
                }
                if y >= term.bottom() {
                    assert!(row(&buf, y).is_none());
                }
            }
            // Hit-testing finishes wherever in the pane you click.
            for y in term.y..term.bottom() {
                for x in term.x..term.right() {
                    let _ = hit(term, x, y, &builtin());
                }
            }
        }
    }
}
