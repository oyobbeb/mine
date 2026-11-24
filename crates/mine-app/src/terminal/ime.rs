//! The input method's composition (a Hangul syllable being typed). Only committed text goes to the session.

use std::ops::Range;

/// The text the input method is composing. It is drawn at the cursor and sent only once committed.
///
/// A composition belongs to the session it started in. Switching sessions by mouse mid-syllable leaves it with the
/// system input method, which keeps composing on it and commits it later. gpui has no call that makes the input method
/// drop it, so the composition stays readable (the input method reads it back), isn't drawn in the new session, and its
/// commit is dropped instead of going to the new session. A ⌘ key switch never gets here: macOS commits first.
#[derive(Debug, Default)]
pub struct Ime {
    marked: Option<String>,
    /// The session (`Model::attached`) the composition started in.
    owner: Option<String>,
}

impl Ime {
    /// The composition changed (`ㅎ` → `하` → `한`) while `session` is attached. Empty text ends it.
    pub fn mark(&mut self, text: &str, session: Option<&str>) {
        if self.marked.is_none() {
            self.owner = session.map(str::to_string);
        }
        self.marked = (!text.is_empty()).then(|| text.to_string());
    }

    /// The input method committed `text` while `session` is attached. Ends the composition and returns the bytes to
    /// send: none if the composition started in another session.
    pub fn commit(&mut self, text: &str, session: Option<&str>) -> Vec<u8> {
        let stale = self.marked.take().is_some() && self.owner.as_deref() != session;
        if stale {
            return Vec::new();
        }
        text.as_bytes().to_vec()
    }

    /// The composition to draw while `session` is attached: none if it started in another session.
    pub fn shown(&self, session: Option<&str>) -> Option<&str> {
        self.marked
            .as_deref()
            .filter(|_| self.owner.as_deref() == session)
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

    const S: Option<&str> = Some("s");

    #[test]
    fn ime_commits_once() {
        let mut ime = Ime::default();
        for text in ["ㅎ", "하", "한"] {
            ime.mark(text, S);
            assert_eq!(ime.marked(), Some(text));
        }
        assert_eq!(ime.commit("한", S), "한".as_bytes());
        assert_eq!(ime.marked(), None);
    }

    #[test]
    fn ime_unmark_sends_nothing() {
        let mut ime = Ime::default();
        ime.mark("한", S);
        ime.unmark();
        assert_eq!(ime.marked(), None);
        // The dropped syllable doesn't ride along with the next commit.
        assert_eq!(ime.commit("a", S), b"a");
        // An emptied composition (the last jamo erased) ends it too.
        ime.mark("ㅎ", S);
        ime.mark("", S);
        assert_eq!(ime.marked(), None);
    }

    #[test]
    fn ime_drops_a_composition_left_in_another_session() {
        let (a, b) = (Some("a"), Some("b"));
        let mut ime = Ime::default();
        ime.mark("한", a);
        assert_eq!(ime.shown(a), Some("한"));
        // Switched to b by mouse mid-syllable: it isn't drawn there, but the input method can still read it back.
        assert_eq!(ime.shown(b), None);
        assert_eq!(ime.marked(), Some("한"));
        // The input method keeps composing on the old syllable (한 + ㄱ = 학), then commits it: nothing goes to b.
        ime.mark("학", b);
        assert_eq!(ime.shown(b), None);
        assert_eq!(ime.commit("하", b), b"");
        // The next composition starts in b and is b's.
        ime.mark("가", b);
        assert_eq!(ime.shown(b), Some("가"));
        assert_eq!(ime.commit("가", b), "가".as_bytes());
        // So is a commit with no composition (a plain character).
        assert_eq!(ime.commit("x", b), b"x");
        // A composition left by detaching (the launcher shows) is dropped too.
        ime.mark("ㅎ", b);
        assert_eq!(ime.commit("ㅎ", None), b"");
        // Switching away and back before the commit keeps it where it was typed.
        ime.mark("한", a);
        assert_eq!(ime.commit("한", a), "한".as_bytes());
    }

    #[test]
    fn ime_reads_back_the_composition_in_utf16() {
        let mut ime = Ime::default();
        assert_eq!(ime.text_utf16(0..1), None);
        ime.mark("한a", S);
        assert_eq!(ime.text_utf16(0..1), Some(("한".into(), 0..1)));
        // A range past the end is clamped to the composition.
        assert_eq!(ime.text_utf16(1..9), Some(("a".into(), 1..2)));
        assert_eq!(ime.text_utf16(5..9), Some((String::new(), 2..2)));
    }
}
