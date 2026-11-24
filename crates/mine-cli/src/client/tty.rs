//! Handling the outer terminal: raw mode, alternate screen, mouse and paste settings and restoring them, the one-time
//! color query at startup, and the stdin reader thread.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use alacritty_terminal::vte::ansi::Rgb;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use tokio::sync::mpsc;

/// Enables the alternate screen, SGR mouse (press 1000, drag 1002, SGR format 1006), focus reports (1004, for presence) and
/// bracketed paste (2004). 1003, which reports motion only, is not enabled because it floods us with events.
/// Finally pushes one kitty keyboard level (`CSI > 0 u`). Flags set per session go on this level only.
const ENTER: &[u8] = b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[>0u";
/// Resets the per-session key encoding modes (DECCKM, numeric keypad, modifyOtherKeys) to their defaults, pops the pushed kitty level
/// to return to the outer terminal's original one, then turns `ENTER` off in reverse and shows the cursor again.
/// The kitty level is per screen, so it is popped before leaving the alternate screen.
const LEAVE: &[u8] = b"\x1b[?1l\x1b>\x1b[>4;0m\x1b[<u\
    \x1b[?2004l\x1b[?1004l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?1049l\x1b[?25h";
/// Appends DA1 after the default foreground and background color queries. The terminal answers in order and always answers DA1,
/// so once the DA1 reply arrives, the earlier replies (if any) have all arrived.
const QUERY: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c";
/// Maximum time to wait for query replies.
const PROBE_WAIT: Duration = Duration::from_millis(200);

/// Whether the outer terminal is currently configured. Ensures it is restored only once even if both the panic hook and drop try to restore it.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// A guard that lives while the outer terminal is configured. When dropped (including when unwinding from a panic) it restores the original state.
pub struct Tty;

impl Tty {
    pub fn enter() -> io::Result<Tty> {
        enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        let tty = Tty;
        // Restore before the default hook writes, so the panic message isn't erased along with the alternate screen.
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

/// Asks the outer terminal once for its default foreground and background colors. Call it after switching to raw mode and before starting the stdin reader thread.
///
/// Reads on this thread only up to `PROBE_WAIT` and returns, so no reader thread is left behind (the first key isn't swallowed).
/// If neither is answered, the colors are `None`. The bytes returned alongside are input that arrived in the meantime and isn't a reply; feed them to the input parser first.
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
        // SAFETY: chunk is a buffer that can hold chunk.len() bytes. Nobody has read stdin yet,
        // so skipping `io::stdin()`'s buffer loses no bytes.
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

/// True if stdin becomes readable within `wait`. macOS's poll(2) can't be trusted on a tty, so select(2) is used.
fn readable(wait: Duration) -> bool {
    // SAFETY: Put only stdin (0, less than FD_SETSIZE) into a zero-filled fd_set and pass it to select.
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

/// Starts a thread that reads stdin and forwards it. It ends when input ends or errors, and the receiving side gets `None`.
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

/// The result of splitting the bytes read during the query.
#[derive(Debug, Default)]
struct Probe {
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    /// The DA1 reply has also arrived.
    done: bool,
    /// Bytes that aren't replies (user input). Order is preserved.
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

/// For `ESC ] 10|11 ; <color> (BEL | ESC \)`: the length, the number (10 foreground, 11 background), and the color (`None` if the format is wrong).
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

/// For `ESC [ ? <digits and ;> c`: its length.
fn da1_reply(b: &[u8]) -> Option<usize> {
    let body = b.strip_prefix(b"\x1b[?")?;
    let n = body
        .iter()
        .position(|c| !c.is_ascii_digit() && *c != b';')?;
    (body[n] == b'c').then_some(3 + n + 1)
}

/// Reduces `rgb:R/G/B` (1–4 hex digits per component) to 8-bit components.
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
        // Bytes that aren't replies (keys the user pressed in the meantime) are kept in order.
        assert_eq!(p.rest, b"x\x1b[Ay");
    }

    #[test]
    fn probe_without_answers_keeps_everything() {
        // A reply that didn't finish in time is kept as input.
        let p = parse_probe(b"ab\x1b]11;rgb:1");
        assert_eq!((p.fg, p.bg, p.done), (None, None, false));
        assert_eq!(p.rest, b"ab\x1b]11;rgb:1");
        // If only DA1 arrives, there are no colors.
        let p = parse_probe(b"\x1b[?1;2c");
        assert_eq!((p.fg, p.bg, p.done), (None, None, true));
        assert!(p.rest.is_empty());
        // A color reply with a wrong format isn't counted as a color and is discarded.
        let p = parse_probe(b"\x1b]10;rgb:zz/00/00\x07\x1b]11;rgb:1/2/3\x07");
        assert_eq!((p.fg, p.bg), (None, Some(rgb(0x11, 0x22, 0x33))));
        assert!(p.rest.is_empty());
    }
}
