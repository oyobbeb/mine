//! 바깥 터미널 다루기. raw 모드·대체 화면·마우스·붙여넣기 설정과 되돌리기, 시작할 때 한 번 하는 색 질의,
//! 표준 입력 읽기 스레드.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::Rgb;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tokio::sync::mpsc;

/// 대체 화면, SGR 마우스(누름 1000·끌기 1002·SGR 형식 1006), bracketed paste(2004)를 켠다.
/// 움직임만 보고하는 1003은 이벤트가 쏟아지므로 켜지 않는다.
/// 끝으로 kitty 키보드 단을 하나 쌓는다(`CSI > 0 u`). 세션을 따라 거는 플래그는 이 단에만 쓴다.
const ENTER: &[u8] = b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[>0u";
/// 세션을 따라 건 키 인코딩 모드(DECCKM, 숫자판, modifyOtherKeys)를 기본값으로 돌리고, 쌓은 kitty 단을 꺼내
/// 바깥 터미널의 원래 단으로 돌아간다. 그다음 `ENTER`를 거꾸로 끄고 커서를 다시 보인다.
/// kitty 단은 화면마다 따로라 대체 화면을 나가기 전에 꺼낸다.
const LEAVE: &[u8] = b"\x1b[?1l\x1b>\x1b[>4;0m\x1b[<u\
    \x1b[?2004l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?1049l\x1b[?25h";
/// 기본 전경색·배경색 질의 뒤에 DA1을 붙인다. 터미널은 차례로 답하고 DA1에는 모두 답하므로,
/// DA1 답이 오면 앞의 답도 (있다면) 다 왔다.
const QUERY: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c";
/// 질의 답을 기다리는 최대 시간.
const PROBE_WAIT: Duration = Duration::from_millis(200);

/// 바깥 터미널이 설정된 상태인가. 패닉 훅과 drop이 둘 다 되돌리려 해도 한 번만 되돌린다.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// 바깥 터미널을 설정한 동안 살아 있는 가드. 버려지면(패닉으로 풀릴 때도) 원래대로 되돌린다.
pub struct Tty;

impl Tty {
    pub fn enter() -> io::Result<Tty> {
        enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        let tty = Tty;
        // 패닉 메시지가 대체 화면과 함께 지워지지 않게, 기본 훅이 쓰기 전에 되돌린다.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            hook(info);
        }));
        let mut out = io::stdout();
        out.write_all(ENTER)?;
        out.flush()?;
        Ok(tty)
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let mut out = io::stdout();
        let _ = out.write_all(LEAVE);
        let _ = out.flush();
        let _ = disable_raw_mode();
    }
}

/// 바깥 터미널에 기본 전경·배경색을 한 번 묻는다. raw 모드로 바꾼 뒤, 표준 입력 읽기 스레드를 띄우기 전에 부른다.
///
/// 이 스레드에서 `PROBE_WAIT`까지만 읽고 돌아오므로 남는 읽기 스레드가 없다(첫 키를 먹지 않는다).
/// 둘 다 답하지 않으면 색은 `None`이다. 함께 돌려주는 바이트는 그사이 들어온 답 아닌 입력이고, 입력 해석기에 먼저 먹인다.
pub fn probe_colors() -> (Option<(Rgb, Rgb)>, Vec<u8>) {
    let mut out = io::stdout();
    if out.write_all(QUERY).and_then(|()| out.flush()).is_err() {
        return (None, Vec::new());
    }
    let deadline = Instant::now() + PROBE_WAIT;
    let mut buf = Vec::new();
    while !parse_probe(&buf).done {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !readable(left) {
            break;
        }
        let mut chunk = [0u8; 1024];
        // SAFETY: chunk는 chunk.len()바이트를 쓸 수 있는 버퍼다. 표준 입력은 아직 아무도 읽지 않아
        // `io::stdin()`의 버퍼를 건너뛰어도 잃는 바이트가 없다.
        let n = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
        let Ok(n) = usize::try_from(n) else { break };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let p = parse_probe(&buf);
    (p.fg.zip(p.bg), p.rest)
}

/// 표준 입력이 `wait` 안에 읽을 수 있게 되면 참이다. macOS의 poll(2)은 tty에서 믿을 수 없어 select(2)를 쓴다.
fn readable(wait: Duration) -> bool {
    // SAFETY: 0으로 채운 fd_set에 표준 입력(0, FD_SETSIZE보다 작다) 하나만 넣고 select에 넘긴다.
    unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut set);
        libc::FD_SET(0, &mut set);
        let mut tv = libc::timeval {
            tv_sec: wait.as_secs() as libc::time_t,
            tv_usec: wait.subsec_micros() as libc::suseconds_t,
        };
        libc::select(
            1,
            &mut set,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        ) > 0
    }
}

/// 표준 입력을 읽어 넘기는 스레드를 띄운다. 입력이 끝나거나 오류가 나면 끝나고, 받는 쪽은 `None`을 받는다.
pub fn spawn_stdin_reader() -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    });
    rx
}

/// 질의하는 동안 읽은 바이트를 나눈 결과.
#[derive(Debug, Default)]
struct Probe {
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    /// DA1 답까지 왔다.
    done: bool,
    /// 답이 아닌 바이트(사용자 입력). 순서를 지킨다.
    rest: Vec<u8>,
}

fn parse_probe(bytes: &[u8]) -> Probe {
    let mut p = Probe::default();
    let mut i = 0;
    while i < bytes.len() {
        let at = &bytes[i..];
        if let Some((len, which, color)) = colour_reply(at) {
            match which {
                10 => p.fg = color,
                _ => p.bg = color,
            }
            i += len;
        } else if let Some(len) = da1_reply(at) {
            p.done = true;
            i += len;
        } else {
            p.rest.push(bytes[i]);
            i += 1;
        }
    }
    p
}

/// `ESC ] 10|11 ; <색> (BEL | ESC \)`이면 길이, 번호(10 전경, 11 배경), 색(형식이 틀리면 `None`).
fn colour_reply(b: &[u8]) -> Option<(usize, u8, Option<Rgb>)> {
    let which = match b.get(..5)? {
        b"\x1b]10;" => 10,
        b"\x1b]11;" => 11,
        _ => return None,
    };
    let body = &b[5..];
    let end = body.iter().position(|&c| c == 0x07 || c == 0x1b)?;
    let term_len = match body[end] {
        0x07 => 1,
        _ if body.get(end + 1) == Some(&b'\\') => 2,
        _ => return None,
    };
    Some((5 + end + term_len, which, parse_rgb(&body[..end])))
}

/// `ESC [ ? <숫자와 ;> c`이면 그 길이.
fn da1_reply(b: &[u8]) -> Option<usize> {
    let body = b.strip_prefix(b"\x1b[?")?;
    let n = body
        .iter()
        .position(|c| !c.is_ascii_digit() && *c != b';')?;
    (body[n] == b'c').then_some(3 + n + 1)
}

/// `rgb:R/G/B`(성분마다 16진 1~4자리)를 8비트 성분으로 줄인다.
fn parse_rgb(s: &[u8]) -> Option<Rgb> {
    let s = std::str::from_utf8(s).ok()?.strip_prefix("rgb:")?;
    let mut parts = s.split('/').map(|v| {
        if v.is_empty() || v.len() > 4 || !v.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let max = (1u32 << (4 * v.len())) - 1;
        Some((u32::from_str_radix(v, 16).ok()? * 255 / max) as u8)
    });
    let (r, g, b) = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(Rgb { r, g, b })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }

    #[test]
    fn probe_reply_parses_colours_and_keeps_user_bytes() {
        let p = parse_probe(
            b"x\x1b]10;rgb:ffff/fefe/0000\x1b\\\x1b[A\x1b]11;rgb:28/2c/34\x07y\x1b[?62;22c",
        );
        assert_eq!(p.fg, Some(rgb(0xff, 0xfe, 0x00)));
        assert_eq!(p.bg, Some(rgb(0x28, 0x2c, 0x34)));
        assert!(p.done);
        // 답이 아닌 바이트(사용자가 그새 누른 키)는 순서대로 남긴다.
        assert_eq!(p.rest, b"x\x1b[Ay");
    }

    #[test]
    fn probe_without_answers_keeps_everything() {
        // 시간 안에 끝나지 않은 답은 입력으로 남긴다.
        let p = parse_probe(b"ab\x1b]11;rgb:1");
        assert_eq!((p.fg, p.bg, p.done), (None, None, false));
        assert_eq!(p.rest, b"ab\x1b]11;rgb:1");
        // DA1만 오면 색은 없다.
        let p = parse_probe(b"\x1b[?1;2c");
        assert_eq!((p.fg, p.bg, p.done), (None, None, true));
        assert!(p.rest.is_empty());
        // 형식이 틀린 색 답은 색으로 치지 않고 버린다.
        let p = parse_probe(b"\x1b]10;rgb:zz/00/00\x07\x1b]11;rgb:1/2/3\x07");
        assert_eq!((p.fg, p.bg), (None, Some(rgb(0x11, 0x22, 0x33))));
        assert!(p.rest.is_empty());
    }
}
