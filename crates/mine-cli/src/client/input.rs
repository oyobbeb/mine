//! 클라이언트 raw stdin 해석. 접두 키(`⌃a`)·SGR 마우스·붙여넣기를 `Action`으로 바꾼다.
//! 순수 함수형 상태 기계이며 I/O를 하지 않는다.
//!
//! 바깥 터미널은 붙은 세션이 켠 키 인코딩(kitty 키보드, modifyOtherKeys)으로 키를 보낸다. 그래서 `⌃a`와
//! 그 뒤 명령 키는 0x01·글자 바이트로도, `CSI 97;5u`·`CSI 27;5;97~` 같은 인코딩된 키로도 온다.

use mine_core::select::{MouseEv, MouseKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    NextSession,
    PrevSession,
    NextProject,
    PrevProject,
    AllProjects,
    NewSession,
    KillSession,
    ToggleList,
    CopyMode,
    Detach,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 그대로 활성 세션에 넘길 바이트.
    Pass(Vec<u8>),
    Cmd(Cmd),
    Mouse(MouseEv),
    /// 붙여넣기 내용(마커 제외).
    Paste(Vec<u8>),
    /// 접두 뒤에 명령 키 대신 한글이 왔다. 버리고(세션에 넘기지 않는다) 영문으로 바꾸라고 알린다.
    Hangul,
}

/// 한글이 명령 키 자리에 왔을 때 상태줄에 띄우는 안내.
pub const HANGUL_HINT: &str = "한/영을 영문으로 바꾸고 다시 누르세요";

/// 한글 자모(U+1100–11FF, U+3130–318F)나 음절(U+AC00–D7A3)인가.
pub fn is_hangul(c: char) -> bool {
    matches!(c, '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}' | '\u{AC00}'..='\u{D7A3}')
}

/// `bytes`가 한글 한 글자로 시작하는가. 글자가 덜 왔으면 아니다.
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
/// 끝나지 않는 CSI 후보(SGR 마우스, 인코딩된 키)를 버퍼에 붙들어 두는 최대 길이.
const CSI_MAX: usize = 32;

#[derive(Debug, Default)]
pub struct InputParser {
    /// `⌃a`를 받았고 다음 바이트가 명령이다.
    prefix: bool,
    /// 붙여넣기 시작 마커를 받았다. 끝 마커까지의 내용이 `paste`에 쌓인다.
    in_paste: bool,
    paste: Vec<u8>,
    /// 다음 `feed`에서 이어 받을 미완성 마커/마우스 시퀀스.
    pending: Vec<u8>,
    /// 접두 뒤에 버린 UTF-8 문자의 남은 연속 바이트 수. 읽기 경계를 넘어 이어진다.
    skip: u8,
}

/// `ESC`로 시작하는 바이트열의 해석 결과.
enum Esc {
    PasteStart,
    /// 소비한 길이와 마우스 이벤트. `None`은 `MouseKind`로 표현하지 못해 삼키는 보고다.
    Mouse(usize, Option<MouseEv>),
    /// 소비한 길이와 인코딩된 키.
    Key(usize, Key),
    /// 마커, 마우스, 인코딩된 키 시퀀스의 앞부분이다. 더 받아야 한다.
    Partial,
    /// 모르는 시퀀스. ESC를 일반 바이트로 내보내고 나머지는 이어서 읽는다.
    Other,
}

fn classify(rest: &[u8]) -> Esc {
    if rest.starts_with(PASTE_START) {
        return Esc::PasteStart;
    }
    // 길이 1은 Esc 키 단독이다. 기다리지 않고 바로 전달한다.
    if rest.len() > 1 && PASTE_START.starts_with(rest) {
        return Esc::Partial;
    }
    let Some(body) = rest.strip_prefix(b"\x1b[") else {
        return Esc::Other;
    };
    // SGR 마우스는 `CSI < b ; x ; y M|m`, 인코딩된 키는 `CSI 숫자[:;]… u|~`다. 끝 바이트까지 받아 가른다.
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

/// kitty 수정 키 비트(매개변수에서 1을 뺀 값). 잠금 키(Caps Lock 64, Num Lock 128)는 명령을 가리지 않으므로 뺀다.
const SHIFT: u32 = 1;
const CTRL: u32 = 4;
const LOCKS: u32 = 64 | 128;
/// kitty가 수정 키 자체(왼쪽 Shift ~ ISO Level 5 Shift)를 누른 보고에 쓰는 코드(플래그 8).
const MODIFIER_KEYS: std::ops::RangeInclusive<u32> = 57441..=57454;

/// 인코딩된 키 하나. kitty `CSI 코드[:시프트한 코드[:기본 배열 코드]] ; 수정[:이벤트] ; 글자 u`와
/// modifyOtherKeys `CSI 27 ; 수정 ; 코드 ~`를 같은 꼴로 읽는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    /// 누른 키의 코드포인트. kitty는 Shift를 누르지 않은 글자로 보낸다.
    code: u32,
    /// Shift를 누른 글자(kitty 플래그 4).
    shifted: Option<u32>,
    /// US 배열에서 같은 자리 키(kitty 플래그 4). 배열이 US와 다르면 온다. 접두는 `code`가 ASCII가 아닐 때
    /// (한글 배열 등)만 이것으로 알아본다. AZERTY의 ⌃q처럼 ASCII 글자 키는 그 글자가 먼저다.
    base: Option<u32>,
    /// 키가 만든 글자(kitty 플래그 16).
    text: Option<u32>,
    /// 수정 키 비트. 잠금 키 비트는 뺐다.
    mods: u32,
    /// 1 누름, 2 반복, 3 뗌.
    event: u32,
}

impl Key {
    /// `ESC [` 뒤 매개변수와 끝 바이트로 키를 읽는다. 키 인코딩이 아니면 `None`이다.
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

    /// 누름이다. 이벤트 종류를 보고하지 않으면(kitty 플래그 2가 없으면) 모두 누름이다.
    fn press(&self) -> bool {
        self.event == 1
    }

    /// `⌃a` 누름이다.
    fn is_prefix(&self) -> bool {
        let a = u32::from(b'a');
        let key = if self.code > 0x7f {
            self.base
        } else {
            Some(self.code)
        };
        self.press() && self.mods == CTRL && key == Some(a)
    }

    /// 접두 뒤 명령표에 넣을 글자. Shift 말고 다른 수정 키가 붙었으면 명령이 아니다.
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

/// Shift를 누른 US 배열 글자. kitty는 `⇧0`을 `CSI 48;2u`처럼 누르지 않은 글자로 보낸다.
///
/// ponytail: US 배열만 안다. 다른 배열은 kitty 플래그 4·16이 알려 주는 글자(`shifted`, `text`)로 맞는다.
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

/// `b;x;y` 파라미터를 읽는다. 형식이 틀리면 `None`이다.
fn sgr_params(params: &[u8]) -> Option<(u16, u16, u16)> {
    let mut it = std::str::from_utf8(params)
        .ok()?
        .split(';')
        .map(|v| v.parse::<u16>().ok());
    let (b, x, y) = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some((b, x, y))
}

/// 눌림은 `M`, 뗌은 `m`이다. 가로 휠(66/67)이나 128 이상 코드처럼 표현하지 못하면 `None`이다.
fn mouse_event(b: u16, x: u16, y: u16, press: bool) -> Option<MouseEv> {
    let b = b & !(4 | 8 | 16); // 수정 키 비트는 무시한다.
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
        b'x' => Cmd::KillSession,
        b'b' => Cmd::ToggleList,
        b'[' => Cmd::CopyMode,
        b'd' => Cmd::Detach,
        _ => return None,
    })
}

/// UTF-8 선두 바이트 뒤에 따라오는 연속 바이트 수. ASCII 등은 0이다.
fn utf8_tail(lead: u8) -> u8 {
    match lead {
        0xC0..=0xDF => 1,
        0xE0..=0xEF => 2,
        0xF0..=0xF7 => 3,
        _ => 0,
    }
}

/// 쌓인 일반 바이트를 `Pass`로 내보낸다.
fn flush(out: &mut Vec<Action>, plain: &mut Vec<u8>) {
    if !plain.is_empty() {
        out.push(Action::Pass(std::mem::take(plain)));
    }
}

impl InputParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// stdin에서 읽은 바이트를 먹고, 바이트 순서대로 동작을 돌려준다.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Action> {
        let mut out = Vec::new();
        let mut plain = Vec::new();
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);
        let mut i = 0;
        while i < buf.len() {
            if self.in_paste {
                // 끝 마커가 읽기 경계에 걸칠 수 있어, 새로 붙이기 전 끝부분부터 다시 찾는다.
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
                self.skip = 0; // 잘린 문자다. 이 바이트는 평소처럼 읽는다.
            }
            if b == ESC {
                match classify(&buf[i..]) {
                    // 접두는 그대로 둔다. 이어 받을 시퀀스가 명령 키일 수 있다.
                    Esc::Partial => {
                        self.pending = buf[i..].to_vec();
                        break;
                    }
                    Esc::Key(len, key) => {
                        self.key(key, &buf[i..i + len], &mut out, &mut plain);
                        i += len;
                    }
                    // 키가 아닌 ESC 시퀀스는 접두를 풀고 평소처럼 해석한다(붙여넣기·마우스·CSI).
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
                // 여러 바이트 문자는 끝까지 읽어야 한글인지 가른다. 읽기 경계에 걸쳤으면 다음 읽기로 미룬다.
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
                    // 모르는 키는 버린다(세션에 넘기지 않는다). 한글이면 알린다. 여러 바이트 문자는 끝까지 버린다.
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

    /// 인코딩된 키 하나(`seq`는 그 바이트). 접두 키와 접두 뒤 명령 키만 읽고, 나머지는 온 그대로 넘긴다.
    fn key(&mut self, key: Key, seq: &[u8], out: &mut Vec<Action>, plain: &mut Vec<u8>) {
        if !self.prefix {
            if key.is_prefix() {
                self.prefix = true;
            } else {
                plain.extend_from_slice(seq);
            }
            return;
        }
        // 접두 조합을 이루는 수정 키 누름(`⇧`)이나 누름이 아닌 이벤트는 삼키고 다음 키를 기다린다.
        if MODIFIER_KEYS.contains(&key.code) || !key.press() {
            return;
        }
        self.prefix = false;
        if key.is_prefix() {
            // `⌃a ⌃a`: 두 번째 `⌃a`는 세션이 고른 인코딩 그대로 넘긴다.
            plain.extend_from_slice(seq);
        } else if let Some(cmd) = key.command_byte().and_then(command) {
            flush(out, plain);
            out.push(Action::Cmd(cmd));
        }
        // 모르는 키는 버린다.
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
        // Esc 단독은 바로 전달한다.
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
        // 끝나지 않는 후보는 32바이트를 넘으면 그대로 내보낸다.
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
        // 한글이 아닌 여러 바이트 문자(2·3·4바이트)다. 알림도 없이 버린다.
        for ch in ["é", "あ", "😀"] {
            let all = [b"\x01".as_slice(), ch.as_bytes()].concat();
            // 읽기 경계가 어디에 걸려도 연속 바이트가 새지 않는다.
            for cut in 0..=all.len() {
                let mut ip = InputParser::new();
                assert!(ip.feed(&all[..cut]).is_empty(), "{ch} cut {cut}");
                assert!(ip.feed(&all[cut..]).is_empty(), "{ch} cut {cut}");
            }
            // 버린 문자 뒤의 바이트는 평소처럼 전달한다.
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
        // 덜 온 글자는 한글로 치지 않는다.
        assert!(!hangul_at(&"한".as_bytes()[..2]));
    }

    #[test]
    fn prefix_then_hangul_is_reported_not_passed_to_the_session() {
        // 자모(ㅊ은 한글 입력의 `c`), 옛 자모 영역, 음절. 읽기 경계가 어디에 걸려도 한 번 알린다.
        for ch in ["ㅊ", "\u{1100}", "각"] {
            let all = [b"\x01".as_slice(), ch.as_bytes()].concat();
            for cut in 0..=all.len() {
                let mut ip = InputParser::new();
                let mut got = ip.feed(&all[..cut]);
                got.extend(ip.feed(&all[cut..]));
                assert_eq!(got, vec![Action::Hangul], "{ch} cut {cut}");
            }
            // 알린 뒤의 바이트는 평소처럼 전달한다. 접두는 풀렸다.
            assert_eq!(
                p(&[all.as_slice(), b"ns"].concat()),
                vec![Action::Hangul, Action::Pass(b"ns".to_vec())],
                "{ch}"
            );
        }
        // 인코딩된 접두 뒤도 같다.
        assert_eq!(p(&cat(&[KITTY_A, "한".as_bytes()])), [Action::Hangul]);
        assert_eq!(p(&cat(&[MOK_A, "ㅊ".as_bytes()])), [Action::Hangul]);
        // 앞서 온 글자와 순서가 유지된다.
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

    /// kitty와 modifyOtherKeys로 인코딩된 `⌃a`.
    const KITTY_A: &[u8] = b"\x1b[97;5u";
    const MOK_A: &[u8] = b"\x1b[27;5;97~";

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn encoded_prefix_then_legacy_key_is_a_command() {
        assert_eq!(p(&cat(&[KITTY_A, b"n"])), [Action::Cmd(Cmd::NextSession)]);
        assert_eq!(p(&cat(&[MOK_A, b"d"])), [Action::Cmd(Cmd::Detach)]);
        // 이벤트 종류를 붙인 누름 형태도 `⌃a`다.
        assert_eq!(
            p(&cat(&[b"\x1b[97;5:1u", b"p"])),
            [Action::Cmd(Cmd::PrevSession)]
        );
        // 잠금 키(Caps Lock 64, Num Lock 128) 비트는 가리지 않는다.
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
        // Shift 글자는 누르지 않은 키(`⇧0`은 48)로도, 시프트한 글자(41)로도, 함께 온 글자(플래그 16)로도 온다.
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
        // `⇧n`은 `N`이라 명령이 아니고, Ctrl이 붙은 키도 명령이 아니다. 둘 다 접두와 함께 버린다.
        assert!(p(&cat(&[KITTY_A, b"\x1b[110;2u"])).is_empty());
        assert!(p(&cat(&[KITTY_A, b"\x1b[110;5u"])).is_empty());
        // 플래그 8은 수정 키 누름도 보낸다. 접두 조합의 일부라 접두를 풀지 않는다.
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
        // AZERTY의 ⌃q는 US 배열 자리가 a(플래그 4)라도 ⌃q다. 앱이 ⌃q를 잃지 않는다.
        assert_eq!(
            p(b"\x1b[113::97;5un"),
            [Action::Pass(b"\x1b[113::97;5un".to_vec())]
        );
        // 한글 배열처럼 키 글자가 ASCII가 아니면 US 배열 자리 a로 `⌃a`를 알아본다.
        assert_eq!(p(b"\x1b[12596::97;5un"), [Action::Cmd(Cmd::NextSession)]);
    }

    #[test]
    fn double_encoded_prefix_passes_the_second_as_it_came() {
        assert_eq!(
            p(&cat(&[KITTY_A, KITTY_A])),
            [Action::Pass(KITTY_A.to_vec())]
        );
        assert_eq!(p(&cat(&[MOK_A, MOK_A])), [Action::Pass(MOK_A.to_vec())]);
        // 섞여 와도 두 번째 것을 온 그대로 넘긴다.
        assert_eq!(p(&cat(&[b"\x01", MOK_A])), [Action::Pass(MOK_A.to_vec())]);
        assert_eq!(p(&cat(&[KITTY_A, b"\x01"])), [Action::Pass(vec![1])]);
    }

    #[test]
    fn other_encoded_keys_pass_unchanged() {
        // Shift+Enter, Ctrl+c, Esc, modifyOtherKeys Shift+Enter, 뗌 이벤트.
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
            // 읽기 끝의 단독 ESC는 기다리지 않고 Esc 키로 넘긴다(`escape_alone_passes`). 그 자리는 빼고 자른다.
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
