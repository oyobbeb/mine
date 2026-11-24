//! 보관 프로세스(`mine hold`). 세션 하나의 PTY와 자식 프로세스를 쥐고 유닉스 소켓으로 데몬에 중계한다.
//!
//! 데몬이 재시작·업그레이드돼도 세션이 죽지 않도록 PTY의 주인을 데몬 밖으로 뺐다. 그래서 업그레이드할
//! 일이 거의 없게 일부러 작게 만든다(std 스레드만 쓰고 tokio는 쓰지 않는다). 하는 일은 넷뿐이다.
//!
//! 1. PTY(80x24)를 열어 자식을 띄운다. 크기는 `Resize`로 바뀐다.
//! 2. PTY 출력을 링 버퍼에 쌓고, 연결된 데몬이 있으면 `Output`으로 실시간 전송한다.
//! 3. 소켓 연결을 받는다. 연결은 한 번에 하나라서 새 연결이 오면 이전 연결을 끊는다. 새 연결에는 `Hello`와
//!    링 전체를 담은 `Output` 하나(비어 있어도 보낸다)를 먼저 보내고, 그 뒤로 실시간 출력을 이어 보낸다.
//! 4. `Kill`을 받거나 자식이 끝나면 리스너를 닫아 새 연결을 그만 받는다. 자식이 끝나면 종료 코드를 exit 파일에
//!    쓰고, 소켓 파일을 지우고, `Exited`를 보내고 끝난다. 같은 세션을 새로 띄운 보관 프로세스가 소켓 경로를
//!    차지했으면 exit 파일과 소켓 파일은 그쪽 것이므로 건드리지 않는다.
//!
//! exit 파일에는 종료 코드를 십진수 글자로 쓴다(`3`, 줄바꿈 없음). 시그널로 죽었으면 `signal`을 쓰고
//! `Exited`의 `code`는 `None`이다. 파일은 임시 파일에 쓴 뒤 이름을 바꿔 놓아서, 파일이 보이면 내용이 완성돼 있다.
//!
//! 보관 프로세스는 스스로 데몬화하지 않는다(`setsid`는 띄우는 쪽이 한다). Rust는 SIGPIPE를 무시하므로 죽은
//! 데몬 연결에 쓰면 오류가 날 뿐이고, 그 연결만 버린다.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use mine_core::proto::{HOLD_VERSION, HoldIn, HoldOut, read_msg, write_msg};

/// 재연결한 데몬에 되돌려 줄 최근 출력의 최대 바이트 수.
pub const RING_CAP: usize = 2 * 1024 * 1024;

/// `Kill`을 받고 SIGHUP을 보낸 뒤 SIGKILL로 넘어가기까지 기다리는 시간.
pub const KILL_GRACE: Duration = Duration::from_secs(2);
/// 자식이 끝난 뒤 PTY에 남은 출력을 마저 읽도록 기다리는 최대 시간. 손자가 슬레이브를 붙들고 있어도 멈추지 않게 한다.
pub const DRAIN_GRACE: Duration = Duration::from_millis(500);
/// 데몬 연결에 쓰다가 이만큼 막히면 그 연결을 버린다. 멈춘 데몬이 새 연결과 PTY 읽기를 막지 않게 한다.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// 마지막 `cap` 바이트만 남기는 링 버퍼.
pub struct Ring {
    cap: usize,
    buf: VecDeque<u8>,
}

impl Ring {
    pub fn new(cap: usize) -> Ring {
        Ring {
            cap,
            buf: VecDeque::new(),
        }
    }

    /// 바이트를 덧붙이고, 한도를 넘으면 가장 오래된 바이트부터 버린다.
    pub fn push(&mut self, data: &[u8]) {
        if data.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&data[data.len() - self.cap..]);
            return;
        }
        let over = (self.buf.len() + data.len()).saturating_sub(self.cap);
        self.buf.drain(..over);
        self.buf.extend(data);
    }

    /// 쌓인 바이트를 오래된 것부터 순서대로 돌려준다.
    pub fn bytes(&self) -> Vec<u8> {
        let (head, tail) = self.buf.as_slices();
        [head, tail].concat()
    }
}

/// 링 버퍼와 현재 데몬 연결. 둘을 한 자물쇠로 묶어, 새 연결이 받는 스냅샷과 이어지는 실시간 출력 사이에
/// 빠지거나 겹치는 바이트가 없게 한다.
struct Hub {
    ring: Ring,
    conn: Option<Conn>,
    next_conn_id: u64,
    /// 자식이 끝난 뒤 늦게 붙은 연결에 보낼 `Exited`.
    done: Option<HoldOut>,
}

struct Conn {
    id: u64,
    stream: UnixStream,
}

impl Hub {
    /// 출력을 링에 쌓고 현재 연결로 보낸다. 쓰기가 실패하면 그 연결을 버린다.
    fn publish(&mut self, data: &[u8]) {
        self.ring.push(data);
        let failed = match &self.conn {
            Some(conn) => write_msg(
                &mut &conn.stream,
                &HoldOut::Output {
                    data: data.to_vec(),
                },
            )
            .is_err(),
            None => false,
        };
        if failed {
            self.drop_conn();
        }
    }

    /// 현재 연결을 끊는다. 그 연결을 읽던 스레드도 EOF로 깨어난다.
    fn drop_conn(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = conn.stream.shutdown(Shutdown::Both);
        }
    }
}

/// 모든 스레드가 함께 쓰는 상태.
struct Ctx {
    hub: Mutex<Hub>,
    /// PTY 입력 쓰기를 맡은 스레드로 가는 통로. 입력이 막혀도 `Kill`·`Resize` 처리가 멈추지 않게 분리했다.
    input: Sender<Vec<u8>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child_pid: u32,
    /// 자식을 거둔 뒤 켠다. 이미 사라진 pid에 시그널을 보내지 않으려고 본다.
    exited: AtomicBool,
    killing: AtomicBool,
    /// 이 끝을 버리면 accept 루프가 깨어나 리스너를 닫는다.
    stop_accept: Mutex<Option<UnixStream>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Ctx {
    fn handle(self: &Arc<Self>, msg: HoldIn) {
        match msg {
            HoldIn::Input { data } => {
                let _ = self.input.send(data);
            }
            HoldIn::Resize { cols, rows } => {
                let _ = lock(&self.master).resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
            HoldIn::Kill => self.kill(),
        }
    }

    /// 새 연결을 그만 받는다. 끝나 가는 이 보관 프로세스에, 같은 세션을 새로 띄운 데몬이 잘못 붙지 않게 한다.
    fn stop_accepting(&self) {
        drop(lock(&self.stop_accept).take());
    }

    /// 자식의 프로세스 그룹에 SIGHUP을 보내고, 그래도 살아 있으면 `KILL_GRACE` 뒤 SIGKILL을 보낸다.
    fn kill(self: &Arc<Self>) {
        if self.killing.swap(true, Ordering::SeqCst) {
            return;
        }
        self.stop_accepting();
        signal_group(self.child_pid, libc::SIGHUP);
        let ctx = Arc::clone(self);
        thread::spawn(move || {
            thread::sleep(KILL_GRACE);
            if !ctx.exited.load(Ordering::SeqCst) {
                signal_group(ctx.child_pid, libc::SIGKILL);
            }
        });
    }
}

/// 자식은 `setsid`로 새 세션의 리더가 돼 pid가 곧 프로세스 그룹 id다. 손자까지 한 번에 보낸다.
fn signal_group(pid: u32, sig: libc::c_int) {
    // SAFETY: killpg는 정수 인자만 받고 메모리를 건드리지 않는다. pid는 0보다 크다(run에서 확인한다).
    unsafe { libc::killpg(pid as libc::pid_t, sig) };
}

/// 파일을 알아보는 값(장치, 아이노드, 변경 시각의 초와 나노초).
type FileId = (u64, u64, i64, i64);

fn file_id(path: &Path) -> Option<FileId> {
    let m = fs::symlink_metadata(path).ok()?;
    Some((m.dev(), m.ino(), m.ctime(), m.ctime_nsec()))
}

/// 이 보관 프로세스가 만든 소켓 파일. 종료 시(오류 경로 포함) 경로에 아직 그 파일이 있을 때만 지운다.
// ponytail: 같은 세션을 새로 띄운 보관 프로세스는 같은 경로를 지우고 소켓을 다시 만든다. 그래서 "경로에 아직 내가
// 만든 파일이 있다"를 "더 새로 띄운 보관 프로세스가 없다"는 표시로 쓰고, 따로 nonce를 넘기지 않는다. 리스너를 먼저
// 닫으므로 Linux ext4처럼 아이노드 번호를 곧바로 다시 쓰는 파일 시스템을 생각해 변경 시각(나노초)까지 비교한다.
// 확인한 뒤 지우거나 쓰기까지의 아주 짧은 틈은 남는다. 데몬이 Kill 뒤 이 연결이 끝날 때까지 같은 id의 New를 막아서
// 보통은 그 틈에 새로 띄울 일이 없다.
struct SockGuard {
    path: PathBuf,
    file: Option<FileId>,
}

impl SockGuard {
    fn still_mine(&self) -> bool {
        self.file.is_some() && file_id(&self.path) == self.file
    }
}

impl Drop for SockGuard {
    fn drop(&mut self) {
        if self.still_mine() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// 보관 프로세스를 실행한다. 자식이 끝나고 정리를 마치면 돌아온다.
pub fn run(
    sock: &Path,
    exit_file: &Path,
    cwd: &Path,
    env: Vec<(String, String)>,
    argv: Vec<String>,
) -> Result<()> {
    let Some((program, args)) = argv.split_first() else {
        bail!("There is no command to run");
    };
    // portable-pty는 없는 폴더를 조용히 홈으로 바꿔 버리므로 먼저 확인한다.
    if !cwd.is_dir() {
        bail!("Not a working directory: {}", cwd.display());
    }

    // 소켓을 먼저 만들어 경로가 잘못됐으면 자식을 띄우기 전에 실패한다. 연결은 accept 루프가 돌 때까지 대기열에 쌓인다.
    let _ = fs::remove_file(sock);
    let listener = UnixListener::bind(sock)
        .with_context(|| format!("Couldn't create the socket: {}", sock.display()))?;
    let mut sock_guard = SockGuard {
        path: sock.to_path_buf(),
        file: file_id(sock),
    };
    fs::set_permissions(sock, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Couldn't change the socket permissions: {}", sock.display()))?;
    // 권한을 바꾸면 변경 시각이 바뀐다.
    sock_guard.file = file_id(sock);
    // 같은 세션의 지난 보관 프로세스가 늦게 남긴 exit 파일을 지운다. 그쪽은 이제 소켓이 바뀐 것을 보고 쓰지 않는다.
    let _ = fs::remove_file(exit_file);
    // poll이 준비됐다고 한 연결이 그새 끊겨도 accept에서 멈추지 않게 논블로킹으로 둔다.
    listener
        .set_nonblocking(true)
        .context("Couldn't make the socket non-blocking")?;
    let (stop_tx, stop_rx) =
        UnixStream::pair().context("Couldn't create the socket that wakes the accept loop")?;

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("Couldn't open the PTY")?;
    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    cmd.cwd(cwd);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("Couldn't start the command: {program}"))?;
    // 슬레이브를 이쪽에 남겨 두면 자식이 끝나도 마스터 읽기가 끝나지 않는다.
    drop(pair.slave);
    let child_pid = child
        .process_id()
        .filter(|&pid| pid > 0)
        .context("Couldn't determine the child pid")?;
    let reader = pair
        .master
        .try_clone_reader()
        .context("Couldn't read from the PTY")?;
    let mut writer = pair
        .master
        .take_writer()
        .context("Couldn't write to the PTY")?;

    let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
    let ctx = Arc::new(Ctx {
        hub: Mutex::new(Hub {
            ring: Ring::new(RING_CAP),
            conn: None,
            next_conn_id: 0,
            done: None,
        }),
        input: input_tx,
        master: Mutex::new(pair.master),
        child_pid,
        exited: AtomicBool::new(false),
        killing: AtomicBool::new(false),
        stop_accept: Mutex::new(Some(stop_tx)),
    });

    // PTY 입력은 한 스레드가 순서대로 쓴다. 자식이 읽지 않아 막혀도 다른 처리는 멈추지 않는다.
    thread::spawn(move || {
        for data in input_rx {
            if writer
                .write_all(&data)
                .and_then(|()| writer.flush())
                .is_err()
            {
                break;
            }
        }
    });

    let (drained_tx, drained_rx) = mpsc::channel::<()>();
    {
        let ctx = Arc::clone(&ctx);
        thread::spawn(move || {
            pump_output(reader, &ctx);
            let _ = drained_tx.send(());
        });
    }
    let accept = {
        let ctx = Arc::clone(&ctx);
        thread::spawn(move || accept_loop(listener, stop_rx, &ctx))
    };

    let status = child
        .wait()
        .context("Couldn't wait for the child to exit")?;
    ctx.exited.store(true, Ordering::SeqCst);
    // Exited를 알리기 전에 리스너가 닫혔음을 확인한다. 그 뒤로 이 경로에 붙는 데몬은 새로 띄운 보관 프로세스를 찾는다.
    ctx.stop_accepting();
    let _ = accept.join();
    // 마지막 출력이 Exited보다 먼저 가도록 PTY가 비워지길 잠깐 기다린다.
    let _ = drained_rx.recv_timeout(DRAIN_GRACE);

    let code = match status.signal() {
        Some(_) => None,
        None => Some(status.exit_code() as i32),
    };
    // 소켓 경로를 새로 띄운 보관 프로세스가 차지했으면 exit 파일도 그쪽 것이다.
    // 파일 쓰기가 실패해도 데몬에는 알린 뒤 오류로 끝낸다.
    let written = if sock_guard.still_mine() {
        write_exit_file(exit_file, code)
    } else {
        Ok(())
    };
    // 소켓을 Exited보다 먼저 지운다. Exited를 받은 데몬이 곧바로 같은 세션을 새로 띄워도 새 소켓은 남는다.
    drop(sock_guard);
    announce_exit(&ctx, code);
    written
}

/// PTY 출력을 읽어 링에 쌓고 연결로 보낸다. PTY가 닫히면 돌아온다.
fn pump_output(mut pty: Box<dyn Read + Send>, ctx: &Ctx) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => lock(&ctx.hub).publish(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // Linux는 슬레이브가 모두 닫히면 EIO를 돌려준다.
            Err(_) => break,
        }
    }
}

/// 연결을 받는 대로 데몬으로 붙이고 입력을 읽는 스레드를 띄운다. `stop`의 맞은편이 닫히면 돌아와 리스너를 닫는다.
///
/// 다른 스레드에서 막힌 accept를 이식성 있게 깨울 방법이 없어서, 리스너와 `stop`을 함께 poll한다.
fn accept_loop(listener: UnixListener, stop: UnixStream, ctx: &Arc<Ctx>) {
    let mut fds = [listener.as_raw_fd(), stop.as_raw_fd()].map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    });
    loop {
        // SAFETY: 두 fd는 이 함수가 끝까지 쥐는 리스너와 소켓의 것이고, 배열 길이를 함께 넘긴다.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                thread::sleep(Duration::from_millis(100));
            }
            continue;
        }
        if fds[1].revents != 0 {
            return;
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => {
                // EMFILE 같은 일시 오류에서 바쁜 반복을 피한다.
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        // macOS는 받은 연결에 리스너의 논블로킹 설정을 물려준다.
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        if let Some(id) = attach(ctx, &stream) {
            let ctx = Arc::clone(ctx);
            thread::spawn(move || read_input(&ctx, stream, id));
        }
    }
}

/// 새 연결에 `Hello`와 링 전체를 보내고 현재 연결로 삼는다(이전 연결은 끊는다). 붙이지 못하면 `None`이다.
fn attach(ctx: &Ctx, stream: &UnixStream) -> Option<u64> {
    stream.set_write_timeout(Some(WRITE_TIMEOUT)).ok()?;
    let mine = stream.try_clone().ok()?;
    let mut hub = lock(&ctx.hub);
    let mut w = &mine;
    write_msg(
        &mut w,
        &HoldOut::Hello {
            version: HOLD_VERSION,
            child_pid: ctx.child_pid,
        },
    )
    .ok()?;
    write_msg(
        &mut w,
        &HoldOut::Output {
            data: hub.ring.bytes(),
        },
    )
    .ok()?;
    if let Some(done) = &hub.done {
        // 자식이 이미 끝났다. 알려 주고 연결은 붙이지 않는다.
        let _ = write_msg(&mut w, done);
        return None;
    }
    hub.drop_conn();
    hub.next_conn_id += 1;
    let id = hub.next_conn_id;
    hub.conn = Some(Conn { id, stream: mine });
    Some(id)
}

/// 연결에서 `HoldIn`을 읽어 처리한다. 연결이 닫히거나 깨진 메시지가 오면 이 연결을 버린다.
fn read_input(ctx: &Arc<Ctx>, stream: UnixStream, id: u64) {
    let mut reader = BufReader::new(stream);
    while let Ok(Some(msg)) = read_msg::<_, HoldIn>(&mut reader) {
        ctx.handle(msg);
    }
    let mut hub = lock(&ctx.hub);
    // 그새 새 연결로 바뀌었으면 건드리지 않는다.
    if hub.conn.as_ref().is_some_and(|conn| conn.id == id) {
        hub.drop_conn();
    }
}

/// 현재 연결에 `Exited`를 보내고, 이후 붙는 연결을 위해 기억해 둔다.
fn announce_exit(ctx: &Ctx, code: Option<i32>) {
    let msg = HoldOut::Exited { code };
    let mut hub = lock(&ctx.hub);
    if let Some(conn) = &hub.conn {
        let _ = write_msg(&mut &conn.stream, &msg);
        let _ = conn.stream.shutdown(Shutdown::Write);
    }
    hub.done = Some(msg);
}

/// 종료 코드를 임시 파일에 쓰고 이름을 바꿔, 읽는 쪽이 쓰다 만 내용을 보지 않게 한다.
fn write_exit_file(path: &Path, code: Option<i32>) -> Result<()> {
    let text = match code {
        Some(code) => code.to_string(),
        None => "signal".to_string(),
    };
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, text)
        .with_context(|| format!("Couldn't write the exit file: {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("Couldn't move the exit file: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_last_bytes() {
        let mut r = Ring::new(RING_CAP);
        for i in 0..(5 * 1024) {
            r.push(&[(i % 251) as u8; 1024]);
        }
        let b = r.bytes();
        assert_eq!(b.len(), RING_CAP);
        assert_eq!(*b.last().unwrap(), ((5 * 1024 - 1) % 251) as u8);
    }

    #[test]
    fn ring_below_cap_keeps_everything_in_order() {
        let mut r = Ring::new(8);
        r.push(b"abc");
        r.push(b"de");
        assert_eq!(r.bytes(), b"abcde");
    }

    #[test]
    fn ring_drops_oldest_bytes_when_over_cap() {
        let mut r = Ring::new(5);
        r.push(b"abc");
        r.push(b"defg");
        assert_eq!(r.bytes(), b"cdefg");
    }

    #[test]
    fn ring_single_push_larger_than_cap_keeps_tail() {
        let mut r = Ring::new(4);
        r.push(b"xy");
        r.push(b"0123456789");
        assert_eq!(r.bytes(), b"6789");
    }
}
