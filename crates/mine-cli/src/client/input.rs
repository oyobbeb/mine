//! Interpretation of the client's raw stdin. Turns the prefix key (`⌃a`), SGR mouse and paste into `Action`s.
//! A pure functional state machine that does no I/O.
//!
//! The outer terminal sends keys in the key encoding the attached session has enabled (kitty keyboard, modifyOtherKeys). So `⌃a` and
//! the command key after it can arrive as 0x01 and a letter byte, or as encoded keys like `CSI 97;5u` or `CSI 27;5;97~`.

use mine_core::select::{MouseEv, MouseKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    NextSession,
    PrevSession,
    NextProject,
    PrevProject,
    AllProjects,
    NewSession,
    Approvals,
    KillSession,
    ToggleList,
    CopyMode,
    Detach,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Bytes to pass to the active session as they are.
    Pass(Vec<u8>),
    Cmd(Cmd),
    Mouse(MouseEv),
    /// Pasted content (markers excluded).
    Paste(Vec<u8>),
    /// Hangul arrived instead of a command key after the prefix. It is discarded (not passed to the session) and the user is told to switch to English.
    Hangul,
    /// The outer terminal's focus report (`CSI I` in, `CSI O` out; mode 1004, which `tty::ENTER` turns on).
    Focus(bool),
}

/// The hint shown in the status line when Hangul arrives where a command key is expected.
pub const HANGUL_HINT: &str = "Switch your keyboard to English and press again";

/// Whether it's a Hangul jamo (U+1100–11FF, U+3130–318F) or syllable (U+AC00–D7A3).
pub fn is_hangul(c: char) -> bool {
    matches!(c, '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}' | '\u{AC00}'..='\u{D7A3}')
}

/// Whether `bytes` starts with one Hangul character. If the character hasn't fully arrived, it's not.
pub fn hangul_at(bytes: &[u8]) -> bool {
    let Some(&lead) = bytes.first() else {
        return false;
    };
    bytes
        .get(..1 + usize::from(utf8_tail(lead)))
        .and_then(|c| std::str::from_utf8(c).ok())
        .and_then(|s| s.chars().next())
        .is_some_and(is_hangul)
}

const PREFIX: u8 = 0x01;
const ESC: u8 = 0x1b;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// Maximum length for which an unfinished CSI candidate (SGR mouse, encoded key) is held in the buffer.
const CSI_MAX: usize = 32;

#[derive(Debug, Default)]
pub struct InputParser {
    /// `⌃a` was received and the next byte is the command.
    prefix: bool,
    /// A paste start marker was received. The content up to the end marker accumulates in `paste`.
    in_paste: bool,
    paste: Vec<u8>,
    /// An incomplete marker or mouse sequence to continue on the next `feed`.
    pending: Vec<u8>,
    /// Remaining continuation bytes of a UTF-8 character discarded after the prefix. Carries over across read boundaries.
    skip: u8,
}

/// The result of interpreting a byte sequence that starts with `ESC`.
enum Esc {
    PasteStart,
    /// The consumed length and the mouse event. `None` is a report that can't be expressed as `MouseKind` and is swallowed.
    Mouse(usize, Option<MouseEv>),
    /// The consumed length and the encoded key.
    Key(usize, Key),
    /// A focus report: `CSI I` (in) or `CSI O` (out).
    Focus(bool),
    /// The beginning of a marker, mouse or encoded key sequence. More must be received.
    Partial,
    /// An unknown sequence. ESC is emitted as an ordinary byte and the rest is read on.
    Other,
}

fn classify(rest: &[u8]) -> Esc {
    if rest.starts_with(PASTE_START) {
        return Esc::PasteStart;
    }
    // A length of 1 is a lone Esc key. Pass it on immediately without waiting.
    if rest.len() > 1 && PASTE_START.starts_with(rest) {
        return Esc::Partial;
    }
    let Some(body) = rest.strip_prefix(b"\x1b[") else {
        return Esc::Other;
    };
    match body.first() {
        Some(b'I') => return Esc::Focus(true),
        Some(b'O') => return Esc::Focus(false),
        _ => {}
    }
    // SGR mouse is `CSI < b ; x ; y M|m`; an encoded key is `CSI digits[:;]… u|~`. Read through the final byte and tell them apart.
    let (mouse, params) = match body.strip_prefix(b"<") {
        Some(params) => (true, params),
        None => (false, body),
    };
    let param = |b: &u8| b.is_ascii_digit() || *b == b';' || (!mouse && *b == b':');
    let Some(n) = params.iter().position(|b| !param(b)) else {
        return if rest.len() <= CSI_MAX {
            Esc::Partial
        } else {
            Esc::Other
        };
    };
    let (len, fin) = (rest.len() - params.len() + n + 1, params[n]);
    if !mouse {
        return Key::parse(&params[..n], fin).map_or(Esc::Other, |key| Esc::Key(len, key));
    }
    match sgr_params(&params[..n]) {
        Some((b, x, y)) if matches!(fin, b'M' | b'm') => {
            Esc::Mouse(len, mouse_event(b, x, y, fin == b'M'))
        }
        _ => Esc::Other,
    }
}

/// kitty modifier bits (the parameter minus 1). Lock keys (Caps Lock 64, Num Lock 128) are excluded because they don't affect the command.
const SHIFT: u32 = 1;
const CTRL: u32 = 4;
const LOCKS: u32 = 64 | 128;
/// The code kitty uses to report pressing a modifier key itself (left Shift through ISO Level 5 Shift) (flag 8).
const MODIFIER_KEYS: std::ops::RangeInclusive<u32> = 57441..=57454;

/// One encoded key. Reads kitty `CSI code[:shifted code[:base layout code]] ; modifiers[:event] ; text u` and
/// modifyOtherKeys `CSI 27 ; modifiers ; code ~` in the same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    /// The code point of the pressed key. kitty sends the character as it is without Shift pressed.
    code: u32,
    /// The character with Shift pressed (kitty flag 4).
    shifted: Option<u32>,
    /// The key at the same position on the US layout (kitty flag 4). It arrives when the layout differs from US. The prefix is recognized with this
    /// only when `code` isn't ASCII (Hangul layouts, etc.). For ASCII letter keys, like ⌃q on AZERTY, that letter takes precedence.
    base: Option<u32>,
    /// The character the key produced (kitty flag 16).
    text: Option<u32>,
    /// Modifier bits. Lock key bits are excluded.
    mods: u32,
    /// 1 press, 2 repeat, 3 release.
    event: u32,
}

impl Key {
    /// Reads a key from the parameters after `ESC [` and the final byte. `None` if it isn't a key encoding.
    fn parse(params: &[u8], fin: u8) -> Option<Key> {
        let fields: Vec<Vec<Option<u32>>> = std::str::from_utf8(params)
            .ok()?
            .split(';')
            .map(|f| f.split(':').map(|v| v.parse().ok()).collect())
            .collect();
        let get = |i: usize, j: usize| fields.get(i)?.get(j).copied().flatten();
        let key = match fin {
            b'u' => Key {
                code: get(0, 0)?,
                shifted: get(0, 1),
                base: get(0, 2),
                text: get(2, 0),
                mods: get(1, 0).unwrap_or(1),
                event: get(1, 1).unwrap_or(1),
            },
            b'~' if fields.len() == 3 && get(0, 0) == Some(27) => Key {
                code: get(2, 0)?,
                shifted: None,
                base: None,
                text: None,
                mods: get(1, 0)?,
                event: 1,
            },
            _ => return None,
        };
        Some(Key {
            mods: key.mods.saturating_sub(1) & !LOCKS,
            ..key
        })
    }

    /// A press. If event types aren't reported (kitty flag 2 absent), everything is a press.
    fn press(&self) -> bool {
        self.event == 1
    }

    /// A `⌃a` press.
    fn is_prefix(&self) -> bool {
        let a = u32::from(b'a');
        let key = if self.code > 0x7f {
            self.base
        } else {
            Some(self.code)
        };
        self.press() && self.mods == CTRL && key == Some(a)
    }

    /// The character to look up in the post-prefix command table. If a modifier other than Shift is attached, it isn't a command.
    fn command_byte(&self) -> Option<u8> {
        if self.mods & !SHIFT != 0 {
            return None;
        }
        let c = match self.text {
            Some(text) => text,
            None if self.mods & SHIFT != 0 => self.shifted.unwrap_or_else(|| shift_us(self.code)),
            None => self.code,
        };
        u8::try_from(c).ok()
    }
}

/// A US-layout character with Shift pressed. kitty sends `⇧0` as the unpressed character, like `CSI 48;2u`.
///
/// ponytail: Only the US layout is known. Other layouts match through the characters kitty flags 4 and 16 provide (`shifted`, `text`).
fn shift_us(code: u32) -> u32 {
    const PLAIN: &[u8] = b"`1234567890-=[]\\;',./";
    const SHIFTED: &[u8] = b"~!@#$%^&*()_+{}|:\"<>?";
    match u8::try_from(code) {
        Ok(c) if c.is_ascii_lowercase() => u32::from(c.to_ascii_uppercase()),
        Ok(c) => PLAIN
            .iter()
            .position(|&p| p == c)
            .map_or(code, |i| u32::from(SHIFTED[i])),
        Err(_) => code,
    }
}

/// Reads the `b;x;y` parameters. `None` if the format is wrong.
fn sgr_params(params: &[u8]) -> Option<(u16, u16, u16)> {
    let mut it = std::str::from_utf8(params)
        .ok()?
        .split(';')
        .map(|v| v.parse::<u16>().ok());
    let (b, x, y) = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some((b, x, y))
}

/// A press is `M`, a release is `m`. `None` if it can't be represented, such as horizontal wheel (66/67) or codes of 128 and above.
fn mouse_event(b: u16, x: u16, y: u16, press: bool) -> Option<MouseEv> {
    let b = b & !(4 | 8 | 16); // Ignore the modifier key bits.
    let kind = match b {
        64 => MouseKind::WheelUp,
        65 => MouseKind::WheelDown,
        _ if b >= 64 => return None,
        _ if !press => MouseKind::Up,
        _ if b & 32 != 0 => MouseKind::Drag,
        _ => MouseKind::Down,
    };
    Some(MouseEv {
        kind,
        button: (b & 3) as u8,
        col: x.saturating_sub(1),
        row: y.saturating_sub(1),
    })
}

fn command(key: u8) -> Option<Cmd> {
    Some(match key {
        b'n' => Cmd::NextSession,
        b'p' => Cmd::PrevSession,
        b')' => Cmd::NextProject,
        b'(' => Cmd::PrevProject,
        b'0' => Cmd::AllProjects,
        b'c' => Cmd::NewSession,
        b'a' => Cmd::Approvals,
        b'x' => Cmd::KillSession,
        b'b' => Cmd::ToggleList,
        b'[' => Cmd::CopyMode,
        b'd' => Cmd::Detach,
        _ => return None,
    })
}

/// The number of continuation bytes following a UTF-8 lead byte. 0 for ASCII and the like.
fn utf8_tail(lead: u8) -> u8 {
    match lead {
        0xC0..=0xDF => 1,
        0xE0..=0xEF => 2,
        0xF0..=0xF7 => 3,
        _ => 0,
    }
}

/// Emits the accumulated ordinary bytes as `Pass`.
fn flush(out: &mut Vec<Action>, plain: &mut Vec<u8>) {
    if !plain.is_empty() {
        out.push(Action::Pass(std::mem::take(plain)));
    }
}

impl InputParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes the bytes read from stdin and returns the actions in byte order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        let mut out = Vec::new();
        let mut plain = Vec::new();
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);
        let mut i = 0;
        while i < buf.len() {
            if self.in_paste {
                // The end marker may straddle a read boundary, so search again from the tail end before appending the new bytes.
                let from = self.paste.len().saturating_sub(PASTE_END.len() - 1);
                self.paste.extend_from_slice(&buf[i..]);
                let Some(at) = self.paste[from..]
                    .windows(PASTE_END.len())
                    .position(|w| w == PASTE_END)
                    .map(|n| n + from)
                else {
                    break;
                };
                buf = self.paste.split_off(at + PASTE_END.len());
                self.paste.truncate(at);
                out.push(Action::Paste(std::mem::take(&mut self.paste)));
                self.in_paste = false;
                i = 0;
                continue;
            }
            let b = buf[i];
            if self.skip > 0 {
                if b & 0xC0 == 0x80 {
                    self.skip -= 1;
                    i += 1;
                    continue;
                }
                self.skip = 0; // A truncated character. Read this byte as usual.
            }
            if b == ESC {
                match classify(&buf[i..]) {
                    // Leave the prefix as is. The sequence still to come may be a command key.
                    Esc::Partial => {
                        self.pending = buf[i..].to_vec();
                        break;
                    }
                    Esc::Key(len, key) => {
                        self.key(key, &buf[i..i + len], &mut out, &mut plain);
                        i += len;
                    }
                    // Not typed by the user, so a prefix waiting for its command key keeps waiting.
                    Esc::Focus(focused) => {
                        flush(&mut out, &mut plain);
                        out.push(Action::Focus(focused));
                        i += 3;
                    }
                    // An ESC sequence that isn't a key releases the prefix and is interpreted as usual (paste, mouse, CSI).
                    Esc::PasteStart => {
                        self.prefix = false;
                        flush(&mut out, &mut plain);
                        self.in_paste = true;
                        i += PASTE_START.len();
                    }
                    Esc::Mouse(len, ev) => {
                        self.prefix = false;
                        if let Some(ev) = ev {
                            flush(&mut out, &mut plain);
                            out.push(Action::Mouse(ev));
                        }
                        i += len;
                    }
                    Esc::Other => {
                        self.prefix = false;
                        plain.push(ESC);
                        i += 1;
                    }
                }
                continue;
            }
            i += 1;
            if self.prefix {
                let tail = utf8_tail(b);
                // A multi-byte character must be read to the end to tell whether it's Hangul. If it straddles the read boundary, defer to the next read.
                if buf.len() < i + usize::from(tail) {
                    self.pending = buf[i - 1..].to_vec();
                    break;
                }
                self.prefix = false;
                match (b, command(b)) {
                    (PREFIX, _) => plain.push(PREFIX),
                    (_, Some(cmd)) => {
                        flush(&mut out, &mut plain);
                        out.push(Action::Cmd(cmd));
                    }
                    // An unknown key is discarded (not passed to the session). If it's Hangul, notify. A multi-byte character is discarded to the end.
                    _ => {
                        if hangul_at(&buf[i - 1..]) {
                            flush(&mut out, &mut plain);
                            out.push(Action::Hangul);
                        }
                        self.skip = tail;
                    }
                }
                continue;
            }
            match b {
                PREFIX => self.prefix = true,
                _ => plain.push(b),
            }
        }
        flush(&mut out, &mut plain);
        out
    }

    /// One encoded key (`seq` is its bytes). Only the prefix key and the command key after the prefix are read; everything else is passed through as received.
    fn key(&mut self, key: Key, seq: &[u8], out: &mut Vec<Action>, plain: &mut Vec<u8>) {
        if !self.prefix {
            if key.is_prefix() {
                self.prefix = true;
            } else {
                plain.extend_from_slice(seq);
            }
            return;
        }
        // A modifier key press (`⇧`) that makes up the prefix combination, or a non-press event, is swallowed and the next key is awaited.
        if MODIFIER_KEYS.contains(&key.code) || !key.press() {
            return;
        }
        self.prefix = false;
        if key.is_prefix() {
            // `⌃a ⌃a`: the second `⌃a` is passed through in the encoding the session chose.
            plain.extend_from_slice(seq);
        } else if let Some(cmd) = key.command_byte().and_then(command) {
            flush(out, plain);
            out.push(Action::Cmd(cmd));
        }
        // An unknown key is discarded.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(bytes: &[u8]) -> Vec<Action> {
        InputParser::new().feed(bytes)
    }

    #[test]
    fn plain_bytes_pass() {
        assert_eq!(p(b"ls\r"), vec![Action::Pass(b"ls\r".to_vec())]);
    }

    #[test]
    fn prefix_commands() {
        assert_eq!(p(b"\x01n"), vec![Action::Cmd(Cmd::NextSession)]);
    }

    #[test]
    fn double_prefix_is_literal() {
        assert_eq!(p(b"\x01\x01"), vec![Action::Pass(vec![1])]);
    }

    #[test]
    fn prefix_split_across_reads() {
        let mut ip = InputParser::new();
        assert!(ip.feed(b"\x01").is_empty());
        assert_eq!(ip.feed(b"d"), vec![Action::Cmd(Cmd::Detach)]);
    }

    #[test]
    fn prefix_inside_paste_passes_through() {
        assert_eq!(
            p(b"\x1b[200~a\x01nb\x1b[201~"),
            vec![Action::Paste(b"a\x01nb".to_vec())]
        );
    }

    #[test]
    fn sgr_mouse_parsed_zero_based() {
        assert_eq!(
            p(b"\x1b[<0;5;3M"),
            vec![Action::Mouse(MouseEv {
                kind: MouseKind::Down,
                button: 0,
                col: 4,
                row: 2
            })]
        );
        assert!(matches!(
            p(b"\x1b[<64;1;1M")[0],
            Action::Mouse(MouseEv {
                kind: MouseKind::WheelUp,
                ..
            })
        ));
    }

    #[test]
    fn escape_alone_passes() {
        // A lone Esc is passed on immediately.
        assert_eq!(p(b"\x1b"), vec![Action::Pass(vec![0x1b])]);
    }

    #[test]
    fn paste_split_across_three_feeds() {
        let mut ip = InputParser::new();
        assert!(ip.feed(b"\x1b[200~ab\x01\x1b[2").is_empty());
        assert!(ip.feed(b"01").is_empty());
        assert_eq!(
            ip.feed(b"~x"),
            vec![
                Action::Paste(b"ab\x01".to_vec()),
                Action::Pass(b"x".to_vec())
            ]
        );
    }

    #[test]
    fn focus_reports_parse_as_focus_events() {
        assert_eq!(
            p(b"a\x1b[Ib\x1b[O"),
            vec![
                Action::Pass(b"a".to_vec()),
                Action::Focus(true),
                Action::Pass(b"b".to_vec()),
                Action::Focus(false)
            ]
        );
        // Split across reads, and arriving between the prefix and its command key, which still works.
        let mut ip = InputParser::new();
        assert!(ip.feed(b"\x01\x1b[").is_empty());
        assert_eq!(ip.feed(b"O"), vec![Action::Focus(false)]);
        assert_eq!(ip.feed(b"n"), vec![Action::Cmd(Cmd::NextSession)]);
    }

    #[test]
    fn unknown_csi_passes_through() {
        assert_eq!(p(b"\x1b[A"), vec![Action::Pass(b"\x1b[A".to_vec())]);
    }

    #[test]
    fn incomplete_sgr_mouse_is_buffered_then_bounded() {
        let mut ip = InputParser::new();
        assert!(ip.feed(b"\x1b[<0;5").is_empty());
        assert_eq!(
            ip.feed(b";3m"),
            vec![Action::Mouse(MouseEv {
                kind: MouseKind::Up,
                button: 0,
                col: 4,
                row: 2
            })]
        );
        // A candidate that hasn't ended is emitted as is once it exceeds 32 bytes.
        let long = [b"\x1b[<".as_slice(), &[b'1'; 40]].concat();
        assert_eq!(p(&long), vec![Action::Pass(long.clone())]);
    }

    #[test]
    fn prefix_then_paste_is_still_a_paste() {
        let paste = b"\x1b[200~a\x01nb\x1b[201~";
        let want = vec![Action::Paste(b"a\x01nb".to_vec())];
        assert_eq!(p(&[b"\x01".as_slice(), paste].concat()), want);
        let mut ip = InputParser::new();
        assert!(ip.feed(b"\x01").is_empty());
        assert_eq!(ip.feed(paste), want);
    }

    #[test]
    fn prefix_then_csi_passes_through() {
        assert_eq!(p(b"\x01\x1b[A"), vec![Action::Pass(b"\x1b[A".to_vec())]);
    }

    #[test]
    fn prefix_then_multibyte_char_is_dropped_whole() {
        // A multi-byte character that isn't Hangul (2-, 3- or 4-byte). Discarded without notice.
        for ch in ["é", "あ", "😀"] {
            let all = [b"\x01".as_slice(), ch.as_bytes()].concat();
            // Continuation bytes don't leak wherever the read boundary falls.
            for cut in 0..=all.len() {
                let mut ip = InputParser::new();
                assert!(ip.feed(&all[..cut]).is_empty(), "{ch} cut {cut}");
                assert!(ip.feed(&all[cut..]).is_empty(), "{ch} cut {cut}");
            }
            // Bytes after the discarded character are passed through as usual.
            assert_eq!(
                p(&[all.as_slice(), b"ls"].concat()),
                vec![Action::Pass(b"ls".to_vec())],
                "{ch}"
            );
        }
    }

    #[test]
    fn hangul_ranges() {
        for c in [
            '\u{1100}', '\u{11FF}', '\u{3130}', 'ㄴ', '\u{318F}', '\u{AC00}', '한', '\u{D7A3}',
        ] {
            assert!(is_hangul(c), "{c:?}");
        }
        for c in [
            'a', '\u{10FF}', '\u{1200}', '\u{312F}', '\u{3190}', 'あ', '\u{ABFF}', '\u{D7A4}',
        ] {
            assert!(!is_hangul(c), "{c:?}");
        }
        assert!(hangul_at("한글".as_bytes()) && !hangul_at(b"a") && !hangul_at(b""));
        // A partially received character doesn't count as Hangul.
        assert!(!hangul_at(&"한".as_bytes()[..2]));
    }

    #[test]
    fn prefix_then_hangul_is_reported_not_passed_to_the_session() {
        // Jamo (ㅊ is `c` in Hangul input), the old jamo block, and syllables. Notified once wherever the read boundary falls.
        for ch in ["ㅊ", "\u{1100}", "각"] {
            let all = [b"\x01".as_slice(), ch.as_bytes()].concat();
            for cut in 0..=all.len() {
                let mut ip = InputParser::new();
                let mut got = ip.feed(&all[..cut]);
                got.extend(ip.feed(&all[cut..]));
                assert_eq!(got, vec![Action::Hangul], "{ch} cut {cut}");
            }
            // Bytes after the notification are passed through as usual. The prefix was released.
            assert_eq!(
                p(&[all.as_slice(), b"ns"].concat()),
                vec![Action::Hangul, Action::Pass(b"ns".to_vec())],
                "{ch}"
            );
        }
        // The same after an encoded prefix.
        assert_eq!(p(&cat(&[KITTY_A, "한".as_bytes()])), [Action::Hangul]);
        assert_eq!(p(&cat(&[MOK_A, "ㅊ".as_bytes()])), [Action::Hangul]);
        // Order with the character that came before is preserved.
        assert_eq!(
            p(&cat(&[b"ab\x01", "ㅊ".as_bytes(), b"c"])),
            [
                Action::Pass(b"ab".to_vec()),
                Action::Hangul,
                Action::Pass(b"c".to_vec())
            ]
        );
    }

    #[test]
    fn hangul_without_the_prefix_is_ordinary_input() {
        assert_eq!(
            p("한글 ㅊ".as_bytes()),
            vec![Action::Pass("한글 ㅊ".as_bytes().to_vec())]
        );
    }

    #[test]
    fn unrepresentable_sgr_mouse_is_swallowed() {
        assert!(p(b"\x1b[<66;1;1M").is_empty());
    }

    /// `⌃a` encoded with kitty and modifyOtherKeys.
    const KITTY_A: &[u8] = b"\x1b[97;5u";
    const MOK_A: &[u8] = b"\x1b[27;5;97~";

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn encoded_prefix_then_legacy_key_is_a_command() {
        assert_eq!(p(&cat(&[KITTY_A, b"n"])), [Action::Cmd(Cmd::NextSession)]);
        assert_eq!(p(&cat(&[MOK_A, b"d"])), [Action::Cmd(Cmd::Detach)]);
        // The press form with an event type attached is `⌃a` too.
        assert_eq!(
            p(&cat(&[b"\x1b[97;5:1u", b"p"])),
            [Action::Cmd(Cmd::PrevSession)]
        );
        // Lock key bits (Caps Lock 64, Num Lock 128) don't matter.
        assert_eq!(
            p(&cat(&[b"\x1b[97;69u", b"n"])),
            [Action::Cmd(Cmd::NextSession)]
        );
    }

    #[test]
    fn encoded_prefix_then_encoded_key_is_a_command() {
        assert_eq!(
            p(&cat(&[KITTY_A, b"\x1b[110u"])),
            [Action::Cmd(Cmd::NextSession)]
        );
        // A Shift character arrives as the unpressed key (`⇧0` is 48), as the shifted character (41), or as the accompanying text (flag 16).
        for shifted in [
            b"\x1b[48;2u".as_slice(),
            b"\x1b[41;2u",
            b"\x1b[48:41;2u",
            b"\x1b[48;2;41u",
            b"\x1b[27;2;41~",
        ] {
            assert_eq!(
                p(&cat(&[KITTY_A, shifted])),
                [Action::Cmd(Cmd::NextProject)],
                "{shifted:?}"
            );
        }
        assert_eq!(
            p(&cat(&[KITTY_A, b"\x1b[57;2u"])),
            [Action::Cmd(Cmd::PrevProject)]
        );
        // `⇧n` is `N`, so it isn't a command, and a key with Ctrl isn't either. Both are discarded along with the prefix.
        assert!(p(&cat(&[KITTY_A, b"\x1b[110;2u"])).is_empty());
        assert!(p(&cat(&[KITTY_A, b"\x1b[110;5u"])).is_empty());
        // Flag 8 also sends modifier key presses. They're part of the prefix combination, so they don't release the prefix.
        assert_eq!(
            p(&cat(&[
                b"\x1b[57442;5u",
                KITTY_A,
                b"\x1b[57441;2u\x1b[48;2u"
            ])),
            [
                Action::Pass(b"\x1b[57442;5u".to_vec()),
                Action::Cmd(Cmd::NextProject)
            ]
        );
    }

    #[test]
    fn base_layout_key_counts_only_for_non_ascii_keys() {
        // ⌃q on AZERTY is ⌃q even if its US-layout position is a (flag 4). The app doesn't lose ⌃q.
        assert_eq!(
            p(b"\x1b[113::97;5un"),
            [Action::Pass(b"\x1b[113::97;5un".to_vec())]
        );
        // When the key character isn't ASCII, as on a Hangul layout, `⌃a` is recognized by its US-layout position a.
        assert_eq!(p(b"\x1b[12596::97;5un"), [Action::Cmd(Cmd::NextSession)]);
    }

    #[test]
    fn double_encoded_prefix_passes_the_second_as_it_came() {
        assert_eq!(
            p(&cat(&[KITTY_A, KITTY_A])),
            [Action::Pass(KITTY_A.to_vec())]
        );
        assert_eq!(p(&cat(&[MOK_A, MOK_A])), [Action::Pass(MOK_A.to_vec())]);
        // Even mixed in, the second one is passed through as it arrived.
        assert_eq!(p(&cat(&[b"\x01", MOK_A])), [Action::Pass(MOK_A.to_vec())]);
        assert_eq!(p(&cat(&[KITTY_A, b"\x01"])), [Action::Pass(vec![1])]);
    }

    #[test]
    fn other_encoded_keys_pass_unchanged() {
        // Shift+Enter, Ctrl+c, Esc, modifyOtherKeys Shift+Enter, release event.
        for key in [
            b"\x1b[13;2u".as_slice(),
            b"\x1b[99;5u",
            b"\x1b[27u",
            b"\x1b[27;2;13~",
            b"\x1b[97;5:3u",
        ] {
            assert_eq!(p(key), [Action::Pass(key.to_vec())], "{key:?}");
        }
        assert_eq!(p(b"a\x1b[13;2ub"), [Action::Pass(b"a\x1b[13;2ub".to_vec())]);
    }

    #[test]
    fn encoded_forms_split_across_feeds() {
        let cases: [(&[u8], Vec<Action>); 4] = [
            (
                &cat(&[KITTY_A, b"\x1b[110u"]),
                vec![Action::Cmd(Cmd::NextSession)],
            ),
            (&cat(&[MOK_A, b"d"]), vec![Action::Cmd(Cmd::Detach)]),
            (
                &cat(&[KITTY_A, KITTY_A]),
                vec![Action::Pass(KITTY_A.to_vec())],
            ),
            (b"\x1b[13;2u", vec![Action::Pass(b"\x1b[13;2u".to_vec())]),
        ];
        for (all, want) in cases {
            // A lone ESC at the end of a read isn't waited for and is passed as the Esc key (`escape_alone_passes`). That spot is excluded when cutting.
            for cut in (0..=all.len()).filter(|&c| c == 0 || all[c - 1] != ESC) {
                let mut ip = InputParser::new();
                let mut got = ip.feed(&all[..cut]);
                got.extend(ip.feed(&all[cut..]));
                assert_eq!(got, want, "{all:?} cut {cut}");
            }
        }
    }

    #[test]
    fn encoded_prefix_keeps_r20_for_paste_mouse_and_other_csi() {
        let paste = b"\x1b[200~a\x01nb\x1b[201~";
        assert_eq!(
            p(&cat(&[KITTY_A, paste])),
            [Action::Paste(b"a\x01nb".to_vec())]
        );
        assert!(matches!(
            p(&cat(&[KITTY_A, b"\x1b[<0;5;3M"]))[..],
            [Action::Mouse(MouseEv {
                kind: MouseKind::Down,
                ..
            })]
        ));
        assert_eq!(
            p(&cat(&[MOK_A, b"\x1b[A"])),
            [Action::Pass(b"\x1b[A".to_vec())]
        );
    }
}
