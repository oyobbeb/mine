//! Daemon connection. Starts the daemon if it isn't running. Headless commands (`mine ls`, `new`, `kill`) use the synchronous `Conn`,
//! and the client screen uses the asynchronous `AsyncConn`.

use std::fs::OpenOptions;
use std::io::{self, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;

use crate::paths::{State, check_sock_path, ensure_dir};
use crate::proto::{FromDaemon, ToDaemon, VERSION, read_msg, recv, send, write_msg};
use crate::session::shell_quote;

/// Maximum time to wait to reattach to a daemon that was just started.
const SPAWN_WAIT: Duration = Duration::from_secs(2);
/// Maximum time to wait for one response. Keeps a command from hanging forever if the daemon stalls.
const REPLY_WAIT: Duration = Duration::from_secs(15);

/// Synchronous connection to the daemon (for headless commands).
pub struct Conn {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    unknowns: Unknowns,
}

/// Asynchronous connection to the daemon (for the client screen). Messages sent on `tx` are written in order by a single write task,
/// and a single read task passes received messages to `rx`. When the connection drops, `rx` returns `None`.
///
/// `proto::recv` is not cancel-safe, so only `rx.recv()` (which is cancel-safe) goes into `select!`.
pub struct AsyncConn {
    pub tx: mpsc::UnboundedSender<ToDaemon>,
    pub rx: mpsc::UnboundedReceiver<FromDaemon>,
}

impl Conn {
    /// Attaches to the daemon. If it can't, starts `current_exe() daemon` and reattaches within `SPAWN_WAIT`.
    ///
    /// A leftover `daemon.sock` file is removed and recreated by the daemon after it takes the pid file lock. If the client removed it,
    /// another client started at the same time could delete the socket that was just created, so this is left to the daemon.
    pub fn connect_or_spawn(state: &State) -> Result<Conn> {
        let sock = state.daemon_sock();
        check_sock_path(&sock)?;
        let stream = match UnixStream::connect(&sock) {
            Ok(stream) => stream,
            Err(_) => {
                spawn_daemon(state, &current_exe()?, None)?;
                wait_connect(&sock)?
            }
        };
        Conn::handshake(stream, state)
    }

    /// Async version of [`Conn::connect_or_spawn`]. Starts and waits for the daemon the same way.
    pub async fn connect_or_spawn_async(state: &State) -> Result<AsyncConn> {
        connect_or_spawn_async_with(state, &current_exe()?, None).await
    }

    /// Exchanges `Hello` and checks the version.
    fn handshake(stream: UnixStream, state: &State) -> Result<Conn> {
        stream.set_read_timeout(Some(REPLY_WAIT))?;
        let mut conn = Conn {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
            unknowns: Unknowns::new(state),
        };
        write_msg(&mut conn.writer, &ToDaemon::Hello { version: VERSION })?;
        let hello = conn.recv()?;
        check_hello(Some(hello), state)?;
        Ok(conn)
    }

    /// Sends a request and waits for the response. Session output (`Output`), the profile list and unknown messages are skipped, and
    /// `Error` is returned as an error.
    pub fn request(&mut self, msg: &ToDaemon) -> Result<FromDaemon> {
        write_msg(&mut self.writer, msg).context("Couldn't send the request to the daemon")?;
        loop {
            match self.recv()? {
                FromDaemon::Output { .. } | FromDaemon::Profiles { .. } => {}
                FromDaemon::Unknown => self.unknowns.note(),
                FromDaemon::Error { message } => bail!("{message}"),
                reply => return Ok(reply),
            }
        }
    }

    fn recv(&mut self) -> Result<FromDaemon> {
        match read_msg(&mut self.reader) {
            Ok(Some(msg)) => Ok(msg),
            Ok(None) => bail!("Disconnected from the daemon"),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Err(no_reply())
            }
            Err(e) => Err(e).context("Couldn't read the daemon's response"),
        }
    }
}

/// Variant of [`Conn::connect_or_spawn_async`] that picks which daemon to start. If there is no daemon, it starts `daemon_exe daemon`.
///
/// The desktop app's own executable (`mine-app`) has no `daemon` subcommand, so it passes the path of `mine`.
/// If `spawn_env` is given, the daemon does not inherit this process's environment and receives only that environment plus `MINE_STATE_DIR`.
pub async fn connect_or_spawn_async_with(
    state: &State,
    daemon_exe: &Path,
    spawn_env: Option<&[(String, String)]>,
) -> Result<AsyncConn> {
    let sock = state.daemon_sock();
    check_sock_path(&sock)?;
    let stream = match tokio::net::UnixStream::connect(&sock).await {
        Ok(stream) => stream,
        Err(_) => {
            spawn_daemon(state, daemon_exe, spawn_env)?;
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
        .context("Couldn't read the daemon's response")?;
    check_hello(hello, state)?;
    let (tx, mut outgoing) = mpsc::unbounded_channel::<ToDaemon>();
    tokio::spawn(async move {
        while let Some(msg) = outgoing.recv().await {
            if send(&mut write, &msg).await.is_err() {
                break;
            }
        }
    });
    // ponytail: the receiving side is an unbounded queue. The client feeds its screen the same amount of output as the daemon, so we assume it keeps up;
    // if it falls behind, client memory grows before the daemon drops this connection. If that becomes a problem, add a cap counted in bytes.
    let (incoming, rx) = mpsc::unbounded_channel::<FromDaemon>();
    let mut unknowns = Unknowns::new(state);
    tokio::spawn(async move {
        while let Ok(Some(msg)) = recv(&mut reader).await {
            if matches!(msg, FromDaemon::Unknown) {
                unknowns.note();
                continue;
            }
            if incoming.send(msg).is_err() {
                break;
            }
        }
    });
    Ok(AsyncConn { tx, rx })
}

/// Notes, once per connection, that the daemon sent a message type this client doesn't know. The message itself is skipped. The note goes
/// to `logs/client.log`, not stderr, which is the TUI's screen.
struct Unknowns {
    log: PathBuf,
    noted: bool,
}

impl Unknowns {
    fn new(state: &State) -> Unknowns {
        Unknowns {
            log: state.log_dir().join("client.log"),
            noted: false,
        }
    }

    fn note(&mut self) {
        if std::mem::replace(&mut self.noted, true) {
            return;
        }
        let line = format!(
            "pid {}: the daemon sent a message type this client doesn't know; ignoring it and any more like it\n",
            std::process::id()
        );
        // Best effort: without a log directory only the note is lost.
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .and_then(|mut f| f.write_all(line.as_bytes()));
    }
}

fn current_exe() -> Result<std::path::PathBuf> {
    std::env::current_exe().context("Couldn't determine the executable path")
}

fn no_reply() -> anyhow::Error {
    anyhow::anyhow!(
        "The daemon didn't respond within {} seconds",
        REPLY_WAIT.as_secs()
    )
}

/// How to stop the running daemon and start it again. Attached to the version-mismatch error. Sessions are held by the holder processes,
/// so they survive the daemon exiting, and the new daemon started by the next `mine` reattaches to them.
pub fn restart_hint(state: &State) -> String {
    format!(
        "Stop the daemon with the command below, then run mine again (your sessions stay alive): kill \"$(cat {})\"",
        shell_quote(&state.daemon_pid().to_string_lossy())
    )
}

/// Checks that the daemon's first message is a `Hello` of the same version.
fn check_hello(msg: Option<FromDaemon>, state: &State) -> Result<()> {
    match msg {
        Some(FromDaemon::Hello { version, .. }) if version == VERSION => Ok(()),
        Some(FromDaemon::Hello { version, .. }) => {
            bail!(
                "Daemon protocol version mismatch (daemon {version}, this command {VERSION}). {}",
                restart_hint(state)
            )
        }
        Some(FromDaemon::Error { message }) => {
            bail!("The daemon refused the connection: {message}")
        }
        Some(other) => bail!("The daemon sent a different message instead of Hello: {other:?}"),
        None => bail!("Disconnected from the daemon"),
    }
}

/// Starts `exe daemon` as a new session leader. Its stdout and stderr go to `logs/daemon.log`.
/// If `env` is given, it is passed instead of this process's environment. `MINE_STATE_DIR` is `state` either way.
///
/// So that a long-running client screen doesn't leave a finished daemon as a zombie, one thread waits for it and reaps it. If this process
/// exits first, the daemon keeps running and init reaps it.
fn spawn_daemon(state: &State, exe: &Path, env: Option<&[(String, String)]>) -> Result<()> {
    ensure_dir(&state.root)?;
    ensure_dir(&state.log_dir())?;
    let log_path = state.log_dir().join("daemon.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Couldn't open the log file: {}", log_path.display()))?;
    let mut cmd = Command::new(exe);
    if let Some(env) = env {
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
    cmd.arg("daemon")
        .env("MINE_STATE_DIR", &state.root)
        // Keep the daemon from holding on to the directory the command was run in.
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: after fork and before exec, only setsid, which is async-signal-safe, is called.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Couldn't start the daemon: {}", exe.display()))?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Retries for `SPAWN_WAIT` until the socket accepts a connection.
fn wait_connect(sock: &Path) -> Result<UnixStream> {
    let deadline = Instant::now() + SPAWN_WAIT;
    loop {
        match UnixStream::connect(sock) {
            Ok(stream) => return Ok(stream),
            Err(e) if Instant::now() >= deadline => {
                return Err(e).with_context(|| {
                    format!(
                        "Couldn't connect to the new daemon within {} seconds: {}",
                        SPAWN_WAIT.as_secs(),
                        sock.display()
                    )
                });
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_mismatch_tells_how_to_restart_the_daemon() {
        let state = State {
            root: "/s s".into(),
        };
        let hello = FromDaemon::Hello {
            version: VERSION + 1,
            features: Vec::new(),
            daemon: None,
        };
        let e = check_hello(Some(hello), &state).unwrap_err().to_string();
        assert!(e.contains("kill \"$(cat '/s s/daemon.pid')\""), "{e}");
    }

    fn temp_state() -> (tempfile::TempDir, State) {
        let dir = tempfile::Builder::new()
            .prefix("m")
            .tempdir_in("/tmp")
            .unwrap();
        let state = State {
            root: dir.path().to_path_buf(),
        };
        ensure_dir(&state.log_dir()).unwrap();
        (dir, state)
    }

    fn client_log(state: &State) -> String {
        std::fs::read_to_string(state.log_dir().join("client.log")).unwrap_or_default()
    }

    /// A newer daemon on `state`'s socket. It answers one connection's Hello, sends two message types this client doesn't know,
    /// the profile list and an empty session list, then reads one more message (or the hang-up) and ends.
    fn newer_daemon(state: &State) -> std::thread::JoinHandle<()> {
        use std::io::Write;
        let listener = std::os::unix::net::UnixListener::bind(state.daemon_sock()).unwrap();
        std::thread::spawn(move || {
            let (mut writer, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(writer.try_clone().unwrap());
            let _: Option<ToDaemon> = read_msg(&mut reader).unwrap();
            let hello = FromDaemon::Hello {
                version: VERSION,
                features: Vec::new(),
                daemon: None,
            };
            write_msg(&mut writer, &hello).unwrap();
            for _ in 0..2 {
                writer
                    .write_all(b"{\"t\":\"FromTheFuture\",\"x\":1}\n")
                    .unwrap();
            }
            write_msg(&mut writer, &FromDaemon::Profiles { profiles: vec![] }).unwrap();
            write_msg(&mut writer, &FromDaemon::Sessions { sessions: vec![] }).unwrap();
            let _ = read_msg::<_, ToDaemon>(&mut reader);
        })
    }

    #[test]
    fn conn_skips_unknown_messages_and_the_profile_list() {
        let (_dir, state) = temp_state();
        let daemon = newer_daemon(&state);
        let mut conn = Conn::connect_or_spawn(&state).unwrap();
        let reply = conn.request(&ToDaemon::List).unwrap();
        assert_eq!(reply, FromDaemon::Sessions { sessions: vec![] });
        drop(conn);
        daemon.join().unwrap();
        // Logged once per connection.
        assert_eq!(
            client_log(&state).lines().count(),
            1,
            "{}",
            client_log(&state)
        );
    }

    #[tokio::test]
    async fn async_conn_skips_unknown_messages() {
        let (_dir, state) = temp_state();
        let _daemon = newer_daemon(&state);
        let mut conn = connect_or_spawn_async_with(&state, Path::new("/nonexistent"), None)
            .await
            .unwrap();
        let profiles = FromDaemon::Profiles { profiles: vec![] };
        assert_eq!(conn.rx.recv().await, Some(profiles));
        let sessions = FromDaemon::Sessions { sessions: vec![] };
        assert_eq!(conn.rx.recv().await, Some(sessions));
        assert_eq!(
            client_log(&state).lines().count(),
            1,
            "{}",
            client_log(&state)
        );
    }
}
