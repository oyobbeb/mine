//! The parts of the terminal pane's mouse handling that are tied to the screen: drag selection and copy (OSC 52), and drawing the selection inverted.
//! `mine_core::select` rebuilds SGR and reads the selected text. All coordinates are (column, row) within the terminal pane, 0-based. `ui::hit` has already converted outer screen coordinates.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use mine_core::select::Selection;

/// Maximum bytes copied at once.
///
/// ponytail: Terminals often reject OSC 52 over 100KB (it grows further with base64). So the text is cut at a character
/// boundary, copied, and the user is told. Splitting a large selection or sending it another way can wait until it's needed.
const MAX_COPY: usize = 100_000;

/// OSC 52 that writes to the clipboard. The outer terminal receives it and puts it on the system clipboard.
pub fn osc52(text: &str) -> Vec<u8> {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text)).into_bytes()
}

/// Clamps an outer screen coordinate into the terminal pane and converts it to a pane-relative coordinate. Keeps the selection going even when dragged outside the pane.
pub fn local(term: Rect, col: u16, row: u16) -> (u16, u16) {
    (
        col.saturating_sub(term.x).min(term.width.saturating_sub(1)),
        row.saturating_sub(term.y)
            .min(term.height.saturating_sub(1)),
    )
}

/// The OSC 52 bytes to copy and the status line notice. `None` if there is no text to copy.
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

/// Draws the selection inverted. Overlaid on the buffer drawn by `ui::draw`.
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

/// Progress of a drag selection. Pressing starts it, dragging extends it, and releasing finalizes it.
#[derive(Debug, Default)]
pub struct Select {
    /// The pressed cell. Held until release.
    anchor: Option<(u16, u16)>,
    /// Set only after dragging to a cell different from the pressed one. A click in place is not a selection.
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

    /// Releases. Returns the selection to copy, if any. The selection isn't cleared, so the inversion stays.
    pub fn up(&mut self, at: (u16, u16)) -> Option<Selection> {
        self.drag(at);
        self.anchor.take()?;
        self.sel
    }

    /// Whether the button is being held down.
    pub fn dragging(&self) -> bool {
        self.anchor.is_some()
    }

    pub fn selection(&self) -> Option<&Selection> {
        self.sel.as_ref()
    }

    /// Discards everything, including what is in progress. Returns `true` if an inversion had been drawn.
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
        // Extend to the release position too.
        assert_eq!(s.up((6, 3)), Some(sel((3, 2), (6, 3))));
        assert!(!s.dragging());
        // The inversion remains after release, and clearing it reports that there was one.
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
        // Drag and release are ignored if there was no press.
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
        // 가 is 3 bytes, so the 100_000-byte boundary (100_000 = 3 * 33333 + 1) falls in the middle of a character.
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
        // The upper row runs from (1,1) to the end of the line, and the lower row from the start of the line to (4,2).
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
