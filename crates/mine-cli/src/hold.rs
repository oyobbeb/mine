//! Holder process (`mine hold`). It owns one session's PTY and child process and relays them to
//! the daemon over a Unix socket.
//!
//! The PTY's owner lives outside the daemon so sessions survive daemon restarts and upgrades. That is why this
//! process is deliberately kept small, so it almost never needs upgrading (std threads only, no tokio). It does just four things:
//!
//! 1. Open a PTY (80x24) and start the child. `Resize` changes the size.
//! 2. Accumulate PTY output in a ring buffer and, if a daemon is connected, stream it live as `Output`.
//! 3. Accept socket connections. There is only one connection at a time, so a new connection drops the previous one. A new
//!    connection first receives `Hello` and one `Output` holding the whole ring (sent even if empty), then the live output.
//! 4. On `Kill`, or when the child exits, close the listener so no new connections are accepted. When the child exits, write
//!    the exit code to the exit file, remove the socket file, send `Exited` and finish. If a holder started afresh for the same
//!    session has taken over the socket path, the exit file and socket file are its own, so leave them alone.
//!
//! The exit file holds the exit code as decimal text (`3`, no newline). If the child died from a signal it holds `signal`
//! and `Exited`'s `code` is `None`. The file is written to a temp file and then renamed, so once it is visible its contents are complete.
//!
//! The holder does not daemonize itself (whoever spawns it does `setsid`). Rust ignores SIGPIPE, so writing to a dead
//! daemon connection only returns an error, and just that connection is dropped.

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

/// Maximum number of bytes of recent output to hand back to a reconnecting daemon.
pub const RING_CAP: usize = 2 * 1024 * 1024;

/// How long to wait, after receiving `Kill` and sending SIGHUP, before moving on to SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(2);
/// Maximum time to wait to finish reading the output left in the PTY after the child exits. Keeps us from hanging even if a grandchild holds the slave open.
pub const DRAIN_GRACE: Duration = Duration::from_millis(500);
/// If a write to the daemon connection stays blocked this long, that connection is dropped. Keeps a stalled daemon from blocking new connections and PTY reads.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A ring buffer that keeps only the last `cap` bytes.
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

    /// Appends bytes, discarding the oldest bytes first once the limit is exceeded.
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

    /// Returns the accumulated bytes in order, oldest first.
    pub fn bytes(&self) -> Vec<u8> {
        let (head, tail) = self.buf.as_slices();
        [head, tail].concat()
    }
}

/// The ring buffer and the current daemon connection. They share one lock so there are no missing or overlapping bytes between
/// the snapshot a new connection receives and the live output that follows.
struct Hub {
    ring: Ring,
    conn: Option<Conn>,
    next_conn_id: u64,
    /// The `Exited` to send to a connection that attaches late, after the child has ended.
    done: Option<HoldOut>,
}

struct Conn {
    id: u64,
    stream: UnixStream,
}

impl Hub {
    /// Appends output to the ring and sends it to the current connection. If the write fails, that connection is dropped.
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

    /// Drops the current connection. The thread reading that connection also wakes up on EOF.
    fn drop_conn(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = conn.stream.shutdown(Shutdown::Both);
        }
    }
}

/// State shared by all threads.
struct Ctx {
    hub: Mutex<Hub>,
    /// Channel to the thread that writes PTY input. Kept separate so `Kill` and `Resize` handling doesn't stall when input blocks.
    input: Sender<Vec<u8>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child_pid: u32,
    /// Set after the child has been reaped. Checked so we don't send signals to a pid that no longer exists.
    exited: AtomicBool,
    killing: AtomicBool,
    /// Dropping this end wakes the accept loop, which then closes the listener.
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

    /// Stops accepting new connections. Keeps a daemon that has just started a new holder for the same session from attaching to this one, which is about to end.
    fn stop_accepting(&self) {
        drop(lock(&self.stop_accept).take());
    }

    /// Sends SIGHUP to the child's process group and, if it is still alive after `KILL_GRACE`, SIGKILL.
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

/// The child is the leader of a new session via `setsid`, so its pid is also the process group id. The signal reaches grandchildren in one go.
fn signal_group(pid: u32, sig: libc::c_int) {
    // SAFETY: killpg takes only integer arguments and touches no memory. The pid is greater than 0 (checked in run).
    unsafe { libc::killpg(pid as libc::pid_t, sig) };
}

/// A value that identifies a file (device, inode, and the seconds and nanoseconds of the modification time).
type FileId = (u64, u64, i64, i64);

fn file_id(path: &Path) -> Option<FileId> {
    let m = fs::symlink_metadata(path).ok()?;
    Some((m.dev(), m.ino(), m.ctime(), m.ctime_nsec()))
}

/// The socket file this holder created. On exit (including error paths) it is removed only if that file is still at the path.
// ponytail: a holder started afresh for the same session removes the same path and recreates the socket. So "my file is
// still at the path" doubles as the sign that "no newer holder was started", and no separate nonce is passed. Since the
// listener is closed first, and with filesystems like Linux ext4 that reuse inode numbers immediately in mind, the
// modification time (nanoseconds) is compared too. A very short gap remains between the check and the remove or write. The
// daemon blocks a `New` with the same id until this connection ends after `Kill`, so a new holder is normally not started in that gap.
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

/// Runs the holder process. Returns once the child has ended and cleanup is done.
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
    // portable-pty silently replaces a missing directory with home, so check first.
    if !cwd.is_dir() {
        bail!("Not a working directory: {}", cwd.display());
    }

    // Create the socket first so a bad path fails before the child is started. Connections queue until the accept loop runs.
    let _ = fs::remove_file(sock);
    let listener = UnixListener::bind(sock)
        .with_context(|| format!("Couldn't create the socket: {}", sock.display()))?;
    let mut sock_guard = SockGuard {
        path: sock.to_path_buf(),
        file: file_id(sock),
    };
    fs::set_permissions(sock, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Couldn't change the socket permissions: {}", sock.display()))?;
    // Changing the permissions changes the modification time.
    sock_guard.file = file_id(sock);
    // Remove the exit file that the previous holder for the same session left late. That one sees the socket has changed and no longer writes it.
    let _ = fs::remove_file(exit_file);
    // Make it non-blocking so a connection that poll said was ready but has since dropped can't stall accept.
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
    // If the slave stays open on this side, master reads won't end when the child exits.
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

    // One thread writes PTY input, in order. Other handling doesn't stall even if the child isn't reading and the write blocks.
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
    // Confirm the listener is closed before announcing Exited. After that, a daemon attaching to this path finds the newly started holder.
    ctx.stop_accepting();
    let _ = accept.join();
    // Briefly wait for the PTY to drain so the last output goes out before Exited.
    let _ = drained_rx.recv_timeout(DRAIN_GRACE);

    let code = match status.signal() {
        Some(_) => None,
        None => Some(status.exit_code() as i32),
    };
    // If a newly started holder has taken the socket path, the exit file is theirs too.
    // Even if the file write fails, tell the daemon and then finish with an error.
    let written = if sock_guard.still_mine() {
        write_exit_file(exit_file, code)
    } else {
        Ok(())
    };
    // Remove the socket before Exited. Even if the daemon that receives Exited immediately starts the same session again, the new socket survives.
    drop(sock_guard);
    announce_exit(&ctx, code);
    written
}

/// Reads PTY output, accumulates it in the ring and sends it to the connection. Returns when the PTY closes.
fn pump_output(mut pty: Box<dyn Read + Send>, ctx: &Ctx) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => lock(&ctx.hub).publish(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // On Linux, EIO is returned once all slaves are closed.
            Err(_) => break,
        }
    }
}

/// Attaches each accepted connection to the daemon and starts a thread to read its input. Returns and closes the listener when the opposite end of `stop` closes.
///
/// There is no portable way to wake an accept blocked in another thread, so the listener and `stop` are polled together.
fn accept_loop(listener: UnixListener, stop: UnixStream, ctx: &Arc<Ctx>) {
    let mut fds = [listener.as_raw_fd(), stop.as_raw_fd()].map(|fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    });
    loop {
        // SAFETY: The two fds belong to the listener and socket this function holds to the end, and the array length is passed along.
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
                // Avoid a busy loop on transient errors such as EMFILE.
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        // macOS gives accepted connections the listener's non-blocking setting.
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        if let Some(id) = attach(ctx, &stream) {
            let ctx = Arc::clone(ctx);
            thread::spawn(move || read_input(&ctx, stream, id));
        }
    }
}

/// Sends `Hello` and the whole ring to a new connection and makes it the current one (the previous connection is dropped). Returns `None` if it can't be attached.
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
        // The child has already ended. Tell it so and don't attach the connection.
        let _ = write_msg(&mut w, done);
        return None;
    }
    hub.drop_conn();
    hub.next_conn_id += 1;
    let id = hub.next_conn_id;
    hub.conn = Some(Conn { id, stream: mine });
    Some(id)
}

/// Reads and handles `HoldIn` from the connection. Drops this connection when it closes or a broken message arrives.
fn read_input(ctx: &Arc<Ctx>, stream: UnixStream, id: u64) {
    let mut reader = BufReader::new(stream);
    while let Ok(Some(msg)) = read_msg::<_, HoldIn>(&mut reader) {
        ctx.handle(msg);
    }
    let mut hub = lock(&ctx.hub);
    // If it has meanwhile been replaced by a new connection, leave it alone.
    if hub.conn.as_ref().is_some_and(|conn| conn.id == id) {
        hub.drop_conn();
    }
}

/// Sends `Exited` to the current connection and remembers it for connections that attach afterwards.
fn announce_exit(ctx: &Ctx, code: Option<i32>) {
    let msg = HoldOut::Exited { code };
    let mut hub = lock(&ctx.hub);
    if let Some(conn) = &hub.conn {
        let _ = write_msg(&mut &conn.stream, &msg);
        let _ = conn.stream.shutdown(Shutdown::Write);
    }
    hub.done = Some(msg);
}

/// Writes the exit code to a temp file and renames it so the reader never sees a half-written file.
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
