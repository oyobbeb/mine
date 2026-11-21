//! 클라이언트 raw stdin 해석. 접두 키(`⌃a`)·SGR 마우스·붙여넣기를 `Action`으로 바꾼다.
//! 순수 함수형 상태 기계이며 I/O를 하지 않는다.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Down,
    Up,
    Drag,
    WheelUp,
    WheelDown,
}

/// 마우스 이벤트. 좌표는 0부터 센다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEv {
    pub kind: MouseKind,
    pub button: u8,
    pub col: u16,
    pub row: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 그대로 활성 세션에 넘길 바이트.
    Pass(Vec<u8>),
    Cmd(Cmd),
    Mouse(MouseEv),
    /// 붙여넣기 내용(마커 제외).
    Paste(Vec<u8>),
}

const PREFIX: u8 = 0x01;
const ESC: u8 = 0x1b;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// 끝나지 않는 SGR 마우스 후보를 버퍼에 붙들어 두는 최대 길이.
const SGR_MAX: usize = 32;

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
    /// 마커나 마우스 시퀀스의 앞부분이다. 더 받아야 한다.
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
    let Some(body) = rest.strip_prefix(b"\x1b[<") else {
        return Esc::Other;
    };
    match body.iter().position(|b| !b.is_ascii_digit() && *b != b';') {
        None if rest.len() <= SGR_MAX => Esc::Partial,
        Some(n) if matches!(body[n], b'M' | b'm') => match sgr_params(&body[..n]) {
            Some((b, x, y)) => Esc::Mouse(3 + n + 1, mouse_event(b, x, y, body[n] == b'M')),
            None => Esc::Other,
        },
        _ => Esc::Other,
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
            if self.prefix {
                self.prefix = false;
                // ESC는 접두를 풀고 아래에서 평소처럼 해석한다(붙여넣기·마우스·CSI).
                if b != ESC {
                    i += 1;
                    match (b, command(b)) {
                        (PREFIX, _) => plain.push(PREFIX),
                        (_, Some(cmd)) => {
                            flush(&mut out, &mut plain);
                            out.push(Action::Cmd(cmd));
                        }
                        // 모르는 키는 버린다. 여러 바이트 문자는 끝까지 버린다.
                        _ => self.skip = utf8_tail(b),
                    }
                    continue;
                }
            }
            match b {
                PREFIX => {
                    self.prefix = true;
                    i += 1;
                }
                ESC => match classify(&buf[i..]) {
                    Esc::PasteStart => {
                        flush(&mut out, &mut plain);
                        self.in_paste = true;
                        i += PASTE_START.len();
                    }
                    Esc::Mouse(len, ev) => {
                        if let Some(ev) = ev {
                            flush(&mut out, &mut plain);
                            out.push(Action::Mouse(ev));
                        }
                        i += len;
                    }
                    Esc::Partial => {
                        self.pending = buf[i..].to_vec();
                        break;
                    }
                    Esc::Other => {
                        plain.push(ESC);
                        i += 1;
                    }
                },
                _ => {
                    plain.push(b);
                    i += 1;
                }
            }
        }
        flush(&mut out, &mut plain);
        out
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
        let all = [b"\x01".as_slice(), "한".as_bytes()].concat();
        // 읽기 경계가 어디에 걸려도 연속 바이트가 새지 않는다.
        for cut in 0..=all.len() {
            let mut ip = InputParser::new();
            assert!(ip.feed(&all[..cut]).is_empty(), "cut {cut}");
            assert!(ip.feed(&all[cut..]).is_empty(), "cut {cut}");
        }
        // 버린 문자 뒤의 바이트는 평소처럼 전달한다.
        assert_eq!(
            p(&[all.as_slice(), b"ls"].concat()),
            vec![Action::Pass(b"ls".to_vec())]
        );
    }

    #[test]
    fn unrepresentable_sgr_mouse_is_swallowed() {
        assert!(p(b"\x1b[<66;1;1M").is_empty());
    }
}
