//! 데몬 연결. 데몬이 없으면 띄운다. 헤드리스 명령(`mine ls`, `new`, `kill`)은 동기 `Conn`을,
//! 클라이언트 화면은 비동기 `AsyncConn`을 쓴다.

use std::fs::OpenOptions;
use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;

use crate::paths::{State, check_sock_path, ensure_dir};
use crate::proto::{FromDaemon, ToDaemon, VERSION, read_msg, recv, send, write_msg};

/// 띄운 데몬에 다시 붙기까지 기다리는 최대 시간.
const SPAWN_WAIT: Duration = Duration::from_secs(2);
/// 응답 하나를 기다리는 최대 시간. 데몬이 멈춰도 명령이 끝없이 매달리지 않게 한다.
const REPLY_WAIT: Duration = Duration::from_secs(15);

/// 데몬과의 동기 연결(헤드리스 명령용).
pub struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

/// 데몬과의 비동기 연결(클라이언트 화면용). `tx`로 보낸 메시지는 쓰기 작업 하나가 차례로 쓰고,
/// 읽기 작업 하나가 받은 메시지를 `rx`로 넘긴다. 연결이 끊기면 `rx`가 `None`을 돌려준다.
///
/// `proto::recv`는 취소에 안전하지 않으므로 `select!`에는 `rx.recv()`(취소에 안전하다)만 넣는다.
pub struct AsyncConn {
    pub tx: mpsc::UnboundedSender<ToDaemon>,
    pub rx: mpsc::UnboundedReceiver<FromDaemon>,
}

impl Conn {
    /// 데몬에 붙는다. 붙지 못하면 `current_exe() daemon`을 띄우고 `SPAWN_WAIT` 안에 다시 붙는다.
    ///
    /// 남은 `daemon.sock` 파일은 데몬이 pid 파일 잠금을 잡은 뒤 지우고 새로 만든다. 클라이언트가 지우면
    /// 동시에 뜬 다른 클라이언트가 막 만들어진 소켓을 지울 수 있어서 데몬에 맡긴다.
    pub fn connect_or_spawn(state: &State) -> Result<Conn> {
        let sock = state.daemon_sock();
        check_sock_path(&sock)?;
        let stream = match UnixStream::connect(&sock) {
            Ok(stream) => stream,
            Err(_) => {
                spawn_daemon(state)?;
                wait_connect(&sock)?
            }
        };
        Conn::handshake(stream)
    }

    /// [`Conn::connect_or_spawn`]의 비동기 판. 데몬을 띄우고 기다리는 방법은 같다.
    pub async fn connect_or_spawn_async(state: &State) -> Result<AsyncConn> {
        let sock = state.daemon_sock();
        check_sock_path(&sock)?;
        let stream = match tokio::net::UnixStream::connect(&sock).await {
            Ok(stream) => stream,
            Err(_) => {
                spawn_daemon(state)?;
                let stream = tokio::task::spawn_blocking(move || wait_connect(&sock)).await??;
                stream.set_nonblocking(true)?;
                tokio::net::UnixStream::from_std(stream)?
            }
        };
        let (read, mut write) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(read);
        send(&mut write, &ToDaemon::Hello { version: VERSION }).await?;
        let hello = tokio::time::timeout(REPLY_WAIT, recv(&mut reader))
            .await
            .map_err(|_| no_reply())?
            .context("데몬 응답을 읽지 못했다")?;
        check_hello(hello)?;
        let (tx, mut outgoing) = mpsc::unbounded_channel::<ToDaemon>();
        tokio::spawn(async move {
            while let Some(msg) = outgoing.recv().await {
                if send(&mut write, &msg).await.is_err() {
                    break;
                }
            }
        });
        // ponytail: 받는 쪽은 무한 큐다. 클라이언트도 데몬과 같은 양의 출력을 화면에 먹이므로 따라간다고 보고,
        // 처지면 데몬이 이 연결을 끊기 전에 클라이언트 메모리가 는다. 문제가 되면 바이트로 센 상한을 둔다.
        let (incoming, rx) = mpsc::unbounded_channel::<FromDaemon>();
        tokio::spawn(async move {
            while let Ok(Some(msg)) = recv(&mut reader).await {
                if incoming.send(msg).is_err() {
                    break;
                }
            }
        });
        Ok(AsyncConn { tx, rx })
    }

    /// `Hello`를 주고받고 버전을 확인한다.
    fn handshake(stream: UnixStream) -> Result<Conn> {
        stream.set_read_timeout(Some(REPLY_WAIT))?;
        let mut conn = Conn {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
        };
        write_msg(&mut conn.writer, &ToDaemon::Hello { version: VERSION })?;
        let hello = conn.recv()?;
        check_hello(Some(hello))?;
        Ok(conn)
    }

    /// 요청을 보내고 응답을 기다린다. 세션 출력(`Output`)은 건너뛰고, `Error`는 오류로 돌려준다.
    pub fn request(&mut self, msg: &ToDaemon) -> Result<FromDaemon> {
        write_msg(&mut self.writer, msg).context("데몬에 요청을 보내지 못했다")?;
        loop {
            match self.recv()? {
                FromDaemon::Output { .. } => {}
                FromDaemon::Error { message } => bail!("{message}"),
                reply => return Ok(reply),
            }
        }
    }

    fn recv(&mut self) -> Result<FromDaemon> {
        match read_msg(&mut self.reader) {
            Ok(Some(msg)) => Ok(msg),
            Ok(None) => bail!("데몬 연결이 끊겼다"),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Err(no_reply())
            }
            Err(e) => Err(e).context("데몬 응답을 읽지 못했다"),
        }
    }
}

fn no_reply() -> anyhow::Error {
    anyhow::anyhow!("데몬이 {}초 안에 답하지 않았다", REPLY_WAIT.as_secs())
}

/// 데몬의 첫 메시지가 같은 버전의 `Hello`인지 확인한다.
fn check_hello(msg: Option<FromDaemon>) -> Result<()> {
    match msg {
        Some(FromDaemon::Hello { version }) if version == VERSION => Ok(()),
        Some(FromDaemon::Hello { version }) => {
            bail!(
                "데몬 프로토콜 버전이 다르다(데몬 {version}, 이 명령 {VERSION}). 데몬을 다시 띄운다"
            )
        }
        Some(FromDaemon::Error { message }) => bail!("데몬이 연결을 거절했다: {message}"),
        Some(other) => bail!("데몬이 Hello 대신 다른 메시지를 보냈다: {other:?}"),
        None => bail!("데몬 연결이 끊겼다"),
    }
}

/// 데몬을 새 세션 리더로 띄운다. 표준 출력과 오류는 `logs/daemon.log`에 붙인다.
///
/// 오래 도는 클라이언트 화면이 끝난 데몬을 좀비로 남기지 않게 스레드 하나가 기다려 거둔다. 이 프로세스가
/// 먼저 끝나면 데몬은 계속 돌고 init이 거둔다.
fn spawn_daemon(state: &State) -> Result<()> {
    ensure_dir(&state.root)?;
    ensure_dir(&state.log_dir())?;
    let log_path = state.log_dir().join("daemon.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("로그 파일을 열 수 없다: {}", log_path.display()))?;
    let exe = std::env::current_exe().context("실행 파일 경로를 알 수 없다")?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .env("MINE_STATE_DIR", &state.root)
        // 데몬이 명령을 실행한 폴더를 붙잡고 있지 않게 한다.
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: fork 뒤 exec 전에 비동기 시그널 안전 함수인 setsid만 부른다.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("데몬을 띄우지 못했다")?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// 소켓이 연결을 받을 때까지 `SPAWN_WAIT` 동안 다시 시도한다.
fn wait_connect(sock: &Path) -> Result<UnixStream> {
    let deadline = Instant::now() + SPAWN_WAIT;
    loop {
        match UnixStream::connect(sock) {
            Ok(stream) => return Ok(stream),
            Err(e) if Instant::now() >= deadline => {
                return Err(e).with_context(|| {
                    format!(
                        "띄운 데몬에 {}초 안에 붙지 못했다: {}",
                        SPAWN_WAIT.as_secs(),
                        sock.display()
                    )
                });
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}
