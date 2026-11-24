//! 시작 화면(런처). 붙은 세션이 없거나 `⌃a c`를 눌렀을 때 터미널 칸 가운데에 그리는, 키 하나로 세션을 시작하는 메뉴.
//! 키 해석·줄 위치 판정 같은 순수 함수와 그리기만 맡고, 상태와 `New` 전송은 `App`이 한다.
//! 새 이름 정하기와 고른 줄의 세션 명세는 `mine_core::launch`에 있다.

use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use mine_core::accounts::Labels;
use mine_core::launch::{Choice, tilde};
use mine_core::view::folder_name;

use crate::client::ui::{HEAD, MUTED, chip};

/// 시작 화면이 열려 있을 때 상태줄 오른쪽 키 안내. 계정 줄은 [`Choice::label`]의 이름이다.
pub fn keys(labels: &Labels) -> String {
    let [a, b, x] = [Choice::ClaudeA, Choice::ClaudeB, Choice::Codex].map(|c| c.label(labels));
    format!("↵ {a} · b {b} · x {x} · s shell")
}

/// 메뉴 줄: (선택, 키 표시). 줄 이름은 [`Choice::label`]이다.
const ROWS: [(Choice, &str); 5] = [
    (Choice::ClaudeA, "↵"),
    (Choice::ClaudeB, "b"),
    (Choice::Codex, "x"),
    (Choice::Shell, "s"),
    (Choice::Typed, "n"),
];

/// 맨 위 두 줄(프로젝트 머리와 빈 줄) 다음에 오는 본문: `새 세션`, 빈 줄, 메뉴 줄들, 빈 줄, 안내.
/// 메뉴 줄은 위에서 네 번째 줄(0부터)에서 시작한다.
const OPTIONS_AT: u16 = 4;
const HEIGHT: u16 = OPTIONS_AT + ROWS.len() as u16 + 2;
/// 이름 칸의 기본 폭. 칩이 이 뒤에 같은 열로 맞는다.
const LABEL_W: usize = 10;

/// 한 키가 고르는 줄. 한 번에 읽힌 바이트가 키 하나일 때만 센다. Esc로 시작하는 시퀀스(화살표, 인코딩된 키)와
/// 그 밖의 글자는 무시한다.
///
/// ponytail: 한글(`ㄴ` `ㅌ` `ㅠ` `ㅜ` 등)은 영문 키로 옮기지 않는다. macOS 입력기는 자모를 다음 키까지 쥐고 있어 옮기면
/// 늦게 터지거나 안 터진다. 대신 `App`이 영문으로 바꾸라고 알린다(`input::HANGUL_HINT`).
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

/// 이름 칸 폭. 기본 폭이고, 칩이 붙는 줄의 이름이 더 길면 그에 맞춰 칩이 같은 열에 서게 한다.
fn label_w(labels: &Labels) -> usize {
    ROWS.iter()
        .filter(|(choice, _)| chip(choice.word()).is_some())
        .map(|(choice, _)| Span::raw(choice.label(labels)).width())
        .fold(LABEL_W, usize::max)
}

/// 메뉴 본문 줄들. 프로젝트와 무관하고, 계정 줄의 이름만 `labels`를 따른다.
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
        // 칩은 목록과 같은 것이다. 계정이 없는 줄(shell, 직접 입력)에는 없다.
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

/// 메뉴 폭. 프로젝트 머리(경로가 길 수 있다)는 빼고 잰다. 그래야 경로 길이에 메뉴 자리가 흔들리지 않는다.
fn menu_width(labels: &Labels) -> u16 {
    body(labels).iter().map(Line::width).max().unwrap_or(0) as u16
}

/// 터미널 칸 `term` 가운데에 놓은 메뉴의 왼쪽 위. 칸이 작으면 칸의 왼쪽·위에 붙는다.
fn origin(term: Rect, labels: &Labels) -> (u16, u16) {
    (
        term.x + term.width.saturating_sub(menu_width(labels)) / 2,
        term.y + term.height.saturating_sub(HEIGHT) / 2,
    )
}

/// `(col, row)`(바깥 화면 좌표)에 있는 메뉴 줄. 줄이 그려진 폭 안만 센다.
pub fn hit(term: Rect, col: u16, row: u16, labels: &Labels) -> Option<Choice> {
    let (x, y) = origin(term, labels);
    let i = row.checked_sub(y + OPTIONS_AT)?;
    let (choice, ..) = ROWS.get(usize::from(i))?;
    (col >= x && col - x < menu_width(labels)).then_some(*choice)
}

/// 터미널 칸 가운데에 메뉴를 그린다. 기본 바탕 위에 글자만 얹는다. 칸보다 크면 칸 밖은 자른다.
///
/// ponytail: 프로젝트 경로가 길면 오른쪽이 잘린다. 앞을 `…`로 줄여 보이는 일은 필요해지면 한다.
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
        // 줄마다 보이는 키가 그 줄을 고른다(Enter는 `↵`).
        for (choice, key) in ROWS {
            let bytes: &[u8] = if key == "↵" { b"\r" } else { key.as_bytes() };
            assert_eq!(k(bytes), Some(choice), "{key}");
        }
    }

    /// 터미널 칸. 위치가 (34, 0)이 아니어도 가운데에 와야 한다.
    const TERM: Rect = Rect::new(34, 0, 86, 39);

    fn drawn(term: Rect, project: &str) -> Buffer {
        drawn_with(term, project, &Labels::default())
    }

    fn drawn_with(term: Rect, project: &str, labels: &Labels) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 120, 40));
        draw(&mut buf, term, Path::new(project), labels);
        buf
    }

    /// 한 줄의 글자(넓은 글자 뒤 칸은 건너뛴다)와 첫 글자의 열.
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
        // 열한 줄(빈 줄 포함) 덩이가 위아래 가운데이고, 메뉴 폭 덩이가 좌우 가운데다.
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
        // 터미널 칸 밖은 건드리지 않는다.
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
        // 칩은 목록과 같은 글자 하나다. 색은 계정 색이다.
        let chip = |text: &str| {
            let (x, y) = at(text);
            let c = &buf[(x + 15, y)];
            (c.symbol().to_string(), c.bg)
        };
        assert_eq!(chip("↵"), ("A".into(), Color::Rgb(0xe5, 0xc0, 0x7b)));
        assert_eq!(chip("b"), ("B".into(), Color::Rgb(0xc6, 0x78, 0xdd)));
        assert_eq!(chip("x"), ("C".into(), Color::Rgb(0x56, 0xb6, 0xc2)));
        // shell 줄에는 칩이 없고, 바탕은 칠하지 않는다(기본 바탕).
        let (x, y) = at("s");
        assert_eq!(buf[(x + 15, y)].symbol(), " ");
        assert_eq!(buf[(x + 15, y)].bg, Color::Reset);
        let (hx, hy) = at("kyoto");
        assert_eq!(buf[(hx + 8, hy)].fg, Color::Rgb(0x6b, 0x70, 0x7a));
    }

    #[test]
    fn rows_show_account_labels_and_the_chips_stay_in_one_column() {
        // claude-a는 별칭, claude-b는 칸보다 긴 별칭, codex는 자동 이름이다. shell의 별칭은 쓰지 않는다.
        let labels = labels_from(
            "[accounts.claude-a]\nlabel = \"개인\"\n\
             [accounts.claude-b]\nlabel = \"company-work\"\n\
             [accounts.shell]\nlabel = \"쉘\"\n",
        );
        let buf = drawn_with(TERM, "/p/kyoto", &labels);
        let rows = rows(&buf);
        let texts: Vec<&str> = rows.iter().map(|(_, _, t)| t.as_str()).collect();
        // 이름 칸은 가장 긴 이름(12칸)에 맞고, 칩은 모두 그 뒤 같은 열이다.
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
        // 줄을 누르는 판정은 그려진 줄과 맞는다.
        let y = rows[3].0;
        let left = rows[3].1;
        assert_eq!(hit(TERM, left, y, &labels), Some(Choice::ClaudeB));
        assert_eq!(hit(TERM, left + 17, y, &labels), Some(Choice::ClaudeB));
        // 상태줄 안내도 같은 이름이다.
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
        // 줄 옆(메뉴 폭 밖)과 제목·머리·안내 줄은 아니다.
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
            // 칸 안 어느 곳을 눌러도 판정은 끝난다.
            for y in term.y..term.bottom() {
                for x in term.x..term.right() {
                    let _ = hit(term, x, y, &Labels::default());
                }
            }
        }
    }
}
