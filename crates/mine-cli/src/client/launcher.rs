//! The start screen (launcher). A menu drawn in the middle of the terminal pane when no session is attached or `⌃a c` is pressed, for starting a session with one key.
//! It handles only pure functions, such as key interpretation and row hit-testing, and drawing; state and sending `New` are `App`'s job.
//! Choosing a new name and the session spec for the chosen row are in `mine_core::launch`.

use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use mine_core::accounts::Labels;
use mine_core::launch::{Choice, tilde};
use mine_core::view::folder_name;

use crate::client::ui::{HEAD, MUTED, chip};

/// Key hints on the right of the status line while the launcher is open. Account rows use the name from [`Choice::label`].
pub fn keys(labels: &Labels) -> String {
    let [a, b, x] = [Choice::ClaudeA, Choice::ClaudeB, Choice::Codex].map(|c| c.label(labels));
    format!("↵ {a} · b {b} · x {x} · s shell")
}

/// Menu rows: (choice, key shown). The row name is [`Choice::label`].
const ROWS: [(Choice, &str); 5] = [
    (Choice::ClaudeA, "↵"),
    (Choice::ClaudeB, "b"),
    (Choice::Codex, "x"),
    (Choice::Shell, "s"),
    (Choice::Typed, "n"),
];

/// The body that follows the top two rows (the project header and a blank row): `New session`, a blank row, the menu rows, a blank row, the hint.
/// The menu rows start at the fourth row from the top (0-based).
const OPTIONS_AT: u16 = 4;
const HEIGHT: u16 = OPTIONS_AT + ROWS.len() as u16 + 2;
/// Default width of the name column. Chips line up in the same column after it.
const LABEL_W: usize = 10;

/// The row a key picks. Counted only when the bytes read at once are a single key. Sequences starting with Esc (arrows, encoded keys) and
/// other characters are ignored.
///
/// ponytail: Hangul (`ㄴ` `ㅌ` `ㅠ` `ㅜ` etc.) isn't mapped to English keys. The macOS input method holds a jamo until the next key, so mapping it
/// would fire late or not at all. Instead `App` tells the user to switch to English (`input::HANGUL_HINT`).
pub fn choice_of_key(bytes: &[u8]) -> Option<Choice> {
    Some(match bytes {
        b"\r" | b"\n" => Choice::ClaudeA,
        b"b" => Choice::ClaudeB,
        b"x" => Choice::Codex,
        b"s" => Choice::Shell,
        b"n" => Choice::Typed,
        _ => return None,
    })
}

/// Width of the name column. This is the default; if a name on a row with a chip is longer, the chips are aligned in the same column to match.
fn label_w(labels: &Labels) -> usize {
    ROWS.iter()
        .filter(|(choice, _)| chip(choice.word()).is_some())
        .map(|(choice, _)| Span::raw(choice.label(labels)).width())
        .fold(LABEL_W, usize::max)
}

/// The menu body rows. Independent of the project; only the account row names follow `labels`.
fn body(labels: &Labels) -> Vec<Line<'static>> {
    let key = Style::new().add_modifier(Modifier::BOLD);
    let label_w = label_w(labels);
    let mut out = vec![Line::styled("New session", HEAD), Line::default()];
    out.extend(ROWS.iter().map(|&(choice, k)| {
        let label = choice.label(labels);
        let pad = label_w.saturating_sub(Span::raw(label).width());
        let mut spans = vec![
            Span::styled(k, key),
            Span::raw(format!("  {label}{}", " ".repeat(pad))),
        ];
        // The chip is the same as in the list. Rows without an account (shell, custom input) have none.
        if let Some(chip) = chip(choice.word()) {
            spans.push(Span::raw("  "));
            spans.push(chip);
        }
        Line::from(spans)
    }));
    out.push(Line::default());
    out.push(Line::styled("⌃a d detach", MUTED));
    out
}

/// Menu width. Measured without the project header (the path can be long), so the menu doesn't shift with path length.
fn menu_width(labels: &Labels) -> u16 {
    body(labels).iter().map(Line::width).max().unwrap_or(0) as u16
}

/// Top left of the menu centered in terminal pane `term`. If the pane is small, it sticks to the pane's left and top.
fn origin(term: Rect, labels: &Labels) -> (u16, u16) {
    (
        term.x + term.width.saturating_sub(menu_width(labels)) / 2,
        term.y + term.height.saturating_sub(HEIGHT) / 2,
    )
}

/// The menu row at `(col, row)` (outer screen coordinates). Only counts within the drawn width.
pub fn hit(term: Rect, col: u16, row: u16, labels: &Labels) -> Option<Choice> {
    let (x, y) = origin(term, labels);
    let i = row.checked_sub(y + OPTIONS_AT)?;
    let (choice, ..) = ROWS.get(usize::from(i))?;
    (col >= x && col - x < menu_width(labels)).then_some(*choice)
}

/// Draws the menu centered in the terminal pane. Draws only text over the default background. Anything beyond the pane is clipped.
///
/// ponytail: If the project path is long, the right side is cut off. Shortening the front with `…` can wait until it's needed.
pub fn draw(buf: &mut Buffer, term: Rect, project: &Path, labels: &Labels) {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let head = Line::from(vec![
        Span::styled(
            folder_name(project),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {}", tilde(project, home.as_deref())), MUTED),
    ]);
    let (x, top) = origin(term, labels);
    let lines = [head, Line::default()].into_iter().chain(body(labels));
    for (y, line) in (top..term.bottom()).zip(lines) {
        if x < term.right() {
            buf.set_line(x, y, &line, term.right() - x);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ui::labels_from;
    use ratatui::style::Color;

    #[test]
    fn keys_choose_one_row_and_everything_else_is_ignored() {
        use Choice::*;
        let k = choice_of_key;
        assert_eq!(k(b"\r"), Some(ClaudeA));
        assert_eq!(k(b"\n"), Some(ClaudeA));
        assert_eq!(k(b"b"), Some(ClaudeB));
        assert_eq!(k(b"x"), Some(Codex));
        assert_eq!(k(b"s"), Some(Shell));
        assert_eq!(k(b"n"), Some(Typed));
        for ignored in [
            &b"a"[..],
            b"B",
            b"S",
            b" ",
            b"\x7f",
            b"\x1b",
            b"\x1b[B",
            b"\x1b[115u",
            b"sb",
            b"",
            "ㄴ".as_bytes(),
        ] {
            assert_eq!(k(ignored), None, "{ignored:?}");
        }
        // The key shown on each row picks that row (Enter is `↵`).
        for (choice, key) in ROWS {
            let bytes: &[u8] = if key == "↵" { b"\r" } else { key.as_bytes() };
            assert_eq!(k(bytes), Some(choice), "{key}");
        }
    }

    /// The terminal pane. It must be centered even if the position isn't (34, 0).
    const TERM: Rect = Rect::new(34, 0, 86, 39);

    fn drawn(term: Rect, project: &str) -> Buffer {
        drawn_with(term, project, &Labels::default())
    }

    fn drawn_with(term: Rect, project: &str, labels: &Labels) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 120, 40));
        draw(&mut buf, term, Path::new(project), labels);
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
                "↵  claude-a    A",
                "b  claude-b    B",
                "x  codex       C",
                "s  shell",
                "n  Custom name and model",
                "⌃a d detach",
            ]
        );
        // The block of eleven rows (blank rows included) is vertically centered, and the block of menu width is horizontally centered.
        let top = rows[0].0;
        let bottom = rows.last().unwrap().0;
        assert_eq!(
            (bottom - top + 1, top),
            (HEIGHT, (TERM.height - HEIGHT) / 2)
        );
        let width = menu_width(&Labels::default());
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
    fn chips_are_drawn_like_the_list_chips_and_the_rest_is_plain() {
        let buf = drawn(TERM, "/p/kyoto");
        let at = |text: &str| {
            let (y, x, _) = rows(&buf)
                .into_iter()
                .find(|r| r.2.starts_with(text))
                .unwrap();
            (x, y)
        };
        let (x, y) = at("↵");
        assert!(buf[(x, y)].modifier.contains(Modifier::BOLD));
        // The chip is a single character, same as the list's. Its color is the account color.
        let chip = |text: &str| {
            let (x, y) = at(text);
            let c = &buf[(x + 15, y)];
            (c.symbol().to_string(), c.bg)
        };
        assert_eq!(chip("↵"), ("A".into(), Color::Rgb(0xe5, 0xc0, 0x7b)));
        assert_eq!(chip("b"), ("B".into(), Color::Rgb(0xc6, 0x78, 0xdd)));
        assert_eq!(chip("x"), ("C".into(), Color::Rgb(0x56, 0xb6, 0xc2)));
        // The shell row has no chip, and its background isn't painted (default background).
        let (x, y) = at("s");
        assert_eq!(buf[(x + 15, y)].symbol(), " ");
        assert_eq!(buf[(x + 15, y)].bg, Color::Reset);
        let (hx, hy) = at("kyoto");
        assert_eq!(buf[(hx + 8, hy)].fg, Color::Rgb(0x6b, 0x70, 0x7a));
    }

    #[test]
    fn rows_show_account_labels_and_the_chips_stay_in_one_column() {
        // claude-a has a label, claude-b has a label longer than the column, and codex has an automatic name. shell's label isn't used.
        let labels = labels_from(
            "[accounts.claude-a]\nlabel = \"개인\"\n\
             [accounts.claude-b]\nlabel = \"company-work\"\n\
             [accounts.shell]\nlabel = \"쉘\"\n",
        );
        let buf = drawn_with(TERM, "/p/kyoto", &labels);
        let rows = rows(&buf);
        let texts: Vec<&str> = rows.iter().map(|(_, _, t)| t.as_str()).collect();
        // The name column fits the longest name (12 columns), and all chips are in the same column after it.
        assert_eq!(
            texts[2..7],
            [
                "↵  개인          A",
                "b  company-work  B",
                "x  Codex         C",
                "s  shell",
                "n  Custom name and model",
            ]
        );
        for line in ["claude-a", "claude-b", "codex"] {
            assert!(texts.iter().all(|t| !t.contains(line)), "{line}");
        }
        // The hit test on pressing a row matches the drawn rows.
        let y = rows[3].0;
        let left = rows[3].1;
        assert_eq!(hit(TERM, left, y, &labels), Some(Choice::ClaudeB));
        assert_eq!(hit(TERM, left + 17, y, &labels), Some(Choice::ClaudeB));
        // The status line hint uses the same names.
        assert_eq!(keys(&labels), "↵ 개인 · b company-work · x Codex · s shell");
        assert_eq!(
            keys(&Labels::default()),
            "↵ claude-a · b claude-b · x codex · s shell"
        );
    }

    #[test]
    fn hit_maps_option_rows_and_nothing_else() {
        let buf = drawn(TERM, "/p/kyoto");
        let rows = rows(&buf);
        let y_of = |text: &str| rows.iter().find(|r| r.2.starts_with(text)).unwrap().0;
        let ids = Labels::default();
        let left = TERM.x + (TERM.width - menu_width(&ids)) / 2;
        let pick = |y, x| hit(TERM, x, y, &ids);
        assert_eq!(pick(y_of("↵"), left), Some(Choice::ClaudeA));
        assert_eq!(pick(y_of("b"), left + 5), Some(Choice::ClaudeB));
        assert_eq!(pick(y_of("x"), left + 21), Some(Choice::Codex));
        assert_eq!(pick(y_of("s"), left + 3), Some(Choice::Shell));
        assert_eq!(pick(y_of("n"), left + 10), Some(Choice::Typed));
        // Beside the rows (outside the menu width) and the title, header and hint rows are not rows.
        assert_eq!(pick(y_of("↵"), left - 1), None);
        assert_eq!(pick(y_of("↵"), left + menu_width(&ids)), None);
        for text in ["kyoto", "New session", "⌃a d"] {
            assert_eq!(pick(y_of(text), left + 1), None, "{text}");
        }
        assert_eq!(pick(y_of("↵") - 1, left), None);
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
                    let _ = hit(term, x, y, &Labels::default());
                }
            }
        }
    }
}
