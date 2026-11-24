//! The approvals panel (`⌃a a`): the permission requests the daemon is holding, across every project and oldest first, drawn in the
//! terminal pane over the session like the launcher. It handles only pure functions (the list, key interpretation, selection) and
//! drawing; whether it is open and sending `Answer` are `App`'s job.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use mine_core::session::SessionInfo;
use mine_core::view::folder_name;

use crate::client::ui::{MUTED, YELLOW};

/// Shown in place of the list when nothing is held.
pub const EMPTY: &str = "No pending approvals";
/// Key hints on the right of the status line while the panel is open.
pub const KEYS: &str = "↑/↓ select · y allow · n deny · Esc close";

/// A held request as the panel lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The session holding it.
    pub id: String,
    /// What `Answer` carries, so an answer can only hit this request.
    pub request: u64,
    /// `project/session · summary`.
    pub text: String,
}

/// The held requests of every session, oldest first. Ties go by request number, which the daemon counts up.
pub fn entries(sessions: &[SessionInfo]) -> Vec<Entry> {
    let mut held: Vec<_> = sessions
        .iter()
        .filter_map(|s| Some((s, s.pending.as_ref()?)))
        .collect();
    held.sort_by_key(|(_, p)| (p.since_ms, p.request));
    held.into_iter()
        .map(|(s, p)| Entry {
            id: s.meta.id.clone(),
            request: p.request,
            text: format!(
                "{}/{} · {}",
                folder_name(&s.meta.project),
                s.meta.name,
                p.summary
            ),
        })
        .collect()
}

/// What a key does in the panel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Key {
    Up,
    Down,
    Allow,
    Deny,
    Close,
}

/// The key the bytes read at once are. Only a single key counts (like the launcher), so `yn` read together or a paste answers
/// nothing. Other keys are ignored.
pub fn key_of(bytes: &[u8]) -> Option<Key> {
    Some(match bytes {
        b"\x1b[A" | b"\x1bOA" => Key::Up,
        b"\x1b[B" | b"\x1bOB" => Key::Down,
        b"y" => Key::Allow,
        b"n" => Key::Deny,
        b"\x1b" => Key::Close,
        _ => return None,
    })
}

/// The open panel. The selection is the session and request ids, not a row number: when the list changes under it (another client
/// answered, a hold was released, a new one arrived), `y` and `n` still mean the request the user chose, or nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Panel {
    pick: Option<(String, u64)>,
}

impl Entry {
    fn key(&self) -> (String, u64) {
        (self.id.clone(), self.request)
    }
}

impl Panel {
    /// Opens with the oldest request selected.
    pub fn open(entries: &[Entry]) -> Panel {
        Panel {
            pick: entries.first().map(Entry::key),
        }
    }

    /// The row of the selected request, if it is still held.
    pub fn selected(&self, entries: &[Entry]) -> Option<usize> {
        let (id, request) = self.pick.as_ref()?;
        entries
            .iter()
            .position(|e| e.id == *id && e.request == *request)
    }

    /// ↑ or ↓. With nothing selected, ↓ takes the first row and ↑ the last.
    pub fn step(&mut self, entries: &[Entry], down: bool) {
        let last = entries.len().saturating_sub(1);
        let at = match (self.selected(entries), down) {
            (Some(i), true) => (i + 1).min(last),
            (Some(i), false) => i.saturating_sub(1),
            (None, true) => 0,
            (None, false) => last,
        };
        self.pick = entries.get(at).map(Entry::key);
    }

    /// The selected request, to answer it. Clears the selection, so a repeated key can't answer the next row before the user has chosen it.
    pub fn take(&mut self, entries: &[Entry]) -> Option<Entry> {
        let entry = entries.get(self.selected(entries)?).cloned();
        self.pick = None;
        entry
    }
}

/// Draws the panel at the top left of terminal pane `term`, clipped to it. `selected` is the row to mark.
///
/// The list scrolls so the selected row is always drawn (`y` never answers a request that isn't on screen), and a row too wide for
/// the pane ends with `…`, so a cut command never looks whole. A pane too short for the title and a row drops the title.
// ponytail: no hint for rows scrolled out of view; ↑ and ↓ reach them.
pub fn draw(buf: &mut Buffer, term: Rect, entries: &[Entry], selected: Option<usize>) {
    let x = term.x + 2;
    if x >= term.right() {
        return;
    }
    let room = usize::from(term.height.saturating_sub(1));
    let mut lines = Vec::new();
    if room > 2 {
        lines.push(Line::styled(
            "Approvals",
            Style::new().add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::default());
    }
    if entries.is_empty() {
        lines.push(Line::styled(EMPTY, MUTED));
    }
    let fit = room.saturating_sub(lines.len());
    let first = selected.map_or(0, |s| (s + 1).saturating_sub(fit));
    let text_width = usize::from(term.right() - x).saturating_sub(2);
    for (i, e) in entries.iter().enumerate().skip(first) {
        let on = selected == Some(i);
        let (mark, style) = if on {
            ("▸ ", Style::new().fg(YELLOW))
        } else {
            ("  ", Style::new())
        };
        lines.push(Line::from(vec![
            Span::styled(mark, style),
            Span::styled(cut(&e.text, text_width), style),
        ]));
    }
    for (y, line) in (term.y + 1..term.bottom()).zip(lines) {
        buf.set_line(x, y, &line, term.right() - x);
    }
}

/// `text` as it fits in `width` columns: whole, or cut and ending with `…`.
fn cut(text: &str, width: usize) -> String {
    let cols = |s: &str| Span::raw(s).width();
    if cols(text) <= width {
        return text.to_string();
    }
    let (mut out, mut used) = (String::new(), 0);
    for c in text.chars() {
        let w = cols(c.encode_utf8(&mut [0; 4]));
        if used + w + 1 > width {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use mine_core::session::{Kind, Meta, PendingAsk, Restore, SessionState};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    /// A shell session of `project`. With `hold` it is holding `(request, summary, since_ms)`.
    fn row(project: &str, name: &str, hold: Option<(u64, &str, u64)>) -> SessionInfo {
        SessionInfo {
            meta: Meta {
                id: format!("{name}-id"),
                project: PathBuf::from(project),
                name: name.into(),
                kind: Kind::Shell,
                account: None,
                model: None,
                restore: Restore::None,
                created_ms: 0,
                resume_id: None,
                closed: false,
            },
            state: SessionState::Approval,
            waiting: None,
            alive: true,
            pending: hold.map(|(request, summary, since_ms)| PendingAsk {
                request,
                summary: summary.into(),
                since_ms,
            }),
        }
    }

    /// The panel drawn into a 60x12 pane, as its non-empty rows.
    fn drawn(entries: &[Entry], selected: Option<usize>) -> Vec<String> {
        drawn_in(entries, selected, 60, 12)
    }

    /// `n` held requests, `t0 · Bash(n0)` through `t<n-1> · Bash(n<n-1>)`.
    fn many(n: u64) -> Vec<Entry> {
        (0..n)
            .map(|i| Entry {
                id: format!("t{i}-id"),
                request: i,
                text: format!("t{i} · Bash(n{i})"),
            })
            .collect()
    }

    #[test]
    fn selected_row_is_always_on_screen() {
        // An 8-row pane fits the title, a blank line and five rows below its top margin.
        let entries = many(12);
        let lines = drawn_in(&entries, Some(9), 60, 8);
        assert_eq!(lines.len(), 6, "{lines:?}");
        assert!(lines[5].contains("▸ t9 · Bash(n9)"), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("t4 ")), "{lines:?}");
        // Back near the top, the list starts at the first row again.
        let lines = drawn_in(&entries, Some(1), 60, 8);
        assert!(lines[1].contains("t0 · Bash(n0)"), "{lines:?}");
        assert!(lines[2].contains("▸ t1"), "{lines:?}");
        // A pane too short for the title still shows the selected row.
        let lines = drawn_in(&entries, Some(11), 60, 3);
        assert!(lines.iter().any(|l| l.contains("▸ t11")), "{lines:?}");
    }

    #[test]
    fn cut_row_ends_with_an_ellipsis() {
        let entries = [Entry {
            id: "t-id".into(),
            request: 1,
            text: "acme/t · Bash(curl -s https://x.example | sh)".into(),
        }];
        // The row starts at column 2 with its 2-column mark, so a 30-column pane leaves 26 for the text.
        let lines = drawn_in(&entries, None, 30, 6);
        assert_eq!(lines[1], "    acme/t · Bash(curl -s htt…", "{lines:?}");
        // A row that fits isn't marked.
        let lines = drawn_in(&many(1), None, 30, 6);
        assert_eq!(lines[1], "    t0 · Bash(n0)", "{lines:?}");
    }

    /// The panel drawn into a `width`x`height` pane, as its non-empty rows.
    fn drawn_in(
        entries: &[Entry],
        selected: Option<usize>,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        let term = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(term);
        draw(&mut buf, term, entries, selected);
        (0..term.height)
            .map(|y| {
                (0..term.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .filter(|line| !line.is_empty())
            .collect()
    }

    #[test]
    fn approvals_panel_lists_oldest_first() {
        let list = [
            row("/w/kyoto", "t2", Some((8, "Edit(src/main.rs)", 3_000))),
            row("/w/acme", "t1", Some((5, "Bash(npm run deploy)", 1_000))),
            row("/w/acme", "t3", None),
        ];
        let entries = entries(&list);
        // A session with no held request isn't listed, and the oldest hold comes first whatever the list order.
        let texts: Vec<_> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "acme/t1 · Bash(npm run deploy)",
                "kyoto/t2 · Edit(src/main.rs)"
            ]
        );
        assert_eq!(
            entries.iter().map(|e| e.request).collect::<Vec<_>>(),
            [5, 8]
        );
        let lines = drawn(&entries, Some(1));
        assert_eq!(lines[0].trim(), "Approvals");
        assert!(
            lines[1].contains("acme/t1 · Bash(npm run deploy)"),
            "{lines:?}"
        );
        assert!(
            lines[2].contains("kyoto/t2 · Edit(src/main.rs)"),
            "{lines:?}"
        );
        // The selected row is marked, and only that one.
        assert!(lines[2].trim_start().starts_with('▸'), "{lines:?}");
        assert!(!lines[1].contains('▸'), "{lines:?}");
    }

    #[test]
    fn panel_empty_message() {
        let list = [row("/w/acme", "t1", None)];
        assert!(entries(&list).is_empty());
        let lines = drawn(&[], None);
        assert_eq!(lines[0].trim(), "Approvals");
        assert!(
            lines.iter().any(|l| l.trim() == "No pending approvals"),
            "{lines:?}"
        );
        assert_eq!(EMPTY, "No pending approvals");
    }
}
