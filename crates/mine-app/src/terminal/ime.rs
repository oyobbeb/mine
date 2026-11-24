//! The input method's composition (a Hangul syllable being typed). Only committed text goes to the session.

use std::ops::Range;

/// The text the input method is composing. It is drawn at the cursor and sent only once committed.
#[derive(Debug, Default)]
pub struct Ime {
    marked: Option<String>,
}

impl Ime {
    /// The composition changed (`ㅎ` → `하` → `한`). Empty text ends it.
    pub fn mark(&mut self, text: &str) {
        self.marked = (!text.is_empty()).then(|| text.to_string());
    }

    /// The input method committed `text`. Ends the composition and returns the bytes to send.
    pub fn commit(&mut self, text: &str) -> Vec<u8> {
        self.marked = None;
        text.as_bytes().to_vec()
    }

    /// The composition was dropped without committing. Nothing is sent.
    pub fn unmark(&mut self) {
        self.marked = None;
    }

    pub fn marked(&self) -> Option<&str> {
        self.marked.as_deref()
    }

    /// The composing text within the UTF-16 `range`, clamped to it, and the range actually covered.
    pub fn text_utf16(&self, range: Range<usize>) -> Option<(String, Range<usize>)> {
        let units: Vec<u16> = self.marked.as_deref()?.encode_utf16().collect();
        let end = range.end.min(units.len());
        let start = range.start.min(end);
        Some((String::from_utf16_lossy(&units[start..end]), start..end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ime_commits_once() {
        let mut ime = Ime::default();
        for text in ["ㅎ", "하", "한"] {
            ime.mark(text);
            assert_eq!(ime.marked(), Some(text));
        }
        assert_eq!(ime.commit("한"), "한".as_bytes());
        assert_eq!(ime.marked(), None);
    }

    #[test]
    fn ime_unmark_sends_nothing() {
        let mut ime = Ime::default();
        ime.mark("한");
        ime.unmark();
        assert_eq!(ime.marked(), None);
        // The dropped syllable doesn't ride along with the next commit.
        assert_eq!(ime.commit("a"), b"a");
        // An emptied composition (the last jamo erased) ends it too.
        ime.mark("ㅎ");
        ime.mark("");
        assert_eq!(ime.marked(), None);
    }

    #[test]
    fn ime_reads_back_the_composition_in_utf16() {
        let mut ime = Ime::default();
        assert_eq!(ime.text_utf16(0..1), None);
        ime.mark("한a");
        assert_eq!(ime.text_utf16(0..1), Some(("한".into(), 0..1)));
        // A range past the end is clamped to the composition.
        assert_eq!(ime.text_utf16(1..9), Some(("a".into(), 1..2)));
        assert_eq!(ime.text_utf16(5..9), Some((String::new(), 2..2)));
    }
}
