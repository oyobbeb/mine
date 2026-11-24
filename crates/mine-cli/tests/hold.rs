//! Integration test for `mine hold`. It starts the real binary and connects over the socket.

use std::io::{BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mine_core::proto::{HoldIn, HoldOut, read_msg, write_msg};

/// Maximum time that socket reads and output searches wait.
const READ_WAIT: Duration = Duration::from_secs(3);

/// A started holder process. When dropped, it cleans up the child too.
struct Hold {
    sock: PathBuf,
    exit: PathBuf,
    child: Child,
    _dir: tempfile::TempDir,
}

/// Starts a holder process. Doesn't wait for the socket to appear (the child may exit right away).
fn spawn_hold(argv: &[&str]) -> Hold {
    spawn_hold_with_env(argv, &[])
}

/// Starts a holder process with `--env K=V` added.
fn spawn_hold_with_env(argv: &[&str], env: &[&str]) -> Hold {
    // Use a short path because of the Unix socket path limit (104 bytes on macOS).
    let dir = tempfile::Builder::new()
        .prefix("m")
        .tempdir_in("/tmp")
        .unwrap();
    let sock = dir.path().join("h.sock");
    let exit = dir.path().join("exit");
    let child = Command::new(env!("CARGO_BIN_EXE_mine"))
        .args(["hold", "--sock"])
        .arg(&sock)
        .arg("--exit")
        .arg(&exit)
        .arg("--cwd")
        .arg(dir.path())
        .args(env.iter().flat_map(|kv| ["--env", kv]))
        .arg("--")
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    Hold {
        sock,
        exit,
        child,
        _dir: dir,
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        // To avoid leaving the child process behind, first try sending Kill, and if it still doesn't end, kill the holder process.
        // A connected client is sent the replay, so read and discard until it ends so writes don't block.
        if let Ok(stream) = UnixStream::connect(&self.sock) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
            let mut w = &stream;
            let _ = write_msg(&mut w, &HoldIn::Kill);
            let mut r = &stream;
            let mut sink = [0u8; 64 * 1024];
            let mut eof = false;
            let deadline = Instant::now() + Duration::from_secs(4);
            while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
                match r.read(&mut sink) {
                    Ok(0) => eof = true,
                    Ok(_) => {}
                    Err(_) => {}
                }
                if eof {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

/// Connects to the socket. It may not exist yet or may be just being created, so it waits and retries.
fn connect(sock: &Path) -> Client {
    let deadline = Instant::now() + READ_WAIT;
    let stream = loop {
        match UnixStream::connect(sock) {
            Ok(stream) => break stream,
            Err(e) => assert!(
                Instant::now() < deadline,
                "couldn't connect to the socket: {} ({e})",
                sock.display()
            ),
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    stream.set_read_timeout(Some(READ_WAIT)).unwrap();
    Client {
        writer: stream.try_clone().unwrap(),
        reader: BufReader::new(stream),
    }
}

impl Client {
    /// Reads the next message. `None` if the connection has ended. Panics if none arrives in time.
    fn try_recv(&mut self) -> Option<HoldOut> {
        read_msg(&mut self.reader).expect("couldn't read a message in time")
    }

    fn recv(&mut self) -> HoldOut {
        self.try_recv().expect("the connection dropped")
    }

    fn send(&mut self, msg: HoldIn) {
        write_msg(&mut self.writer, &msg).unwrap();
    }

    /// Collects `Output` for up to 3 seconds and checks whether `needle` appears. Gives up if the connection ends or `Exited` arrives.
    fn read_until_contains(&mut self, needle: &str) -> bool {
        let deadline = Instant::now() + READ_WAIT;
        let mut seen = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            // macOS rejects setting a timeout on a socket the peer has already closed, so ignore a failure.
            let _ = self.reader.get_ref().set_read_timeout(Some(left));
            match read_msg::<_, HoldOut>(&mut self.reader) {
                Ok(Some(HoldOut::Output { data })) => {
                    // Look only at the new chunk and the boundary before it, so we don't rescan everything each time.
                    let from = seen.len().saturating_sub(needle.len());
                    seen.extend(data);
                    if String::from_utf8_lossy(&seen[from..]).contains(needle) {
                        return true;
                    }
                }
                Ok(Some(HoldOut::Hello { .. })) => {}
                Ok(Some(HoldOut::Exited { .. })) | Ok(None) | Err(_) => return false,
            }
        }
        false
    }
}

/// Waits up to `ms` milliseconds for the condition to become true. Panics on timeout.
fn wait_until(ms: u64, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while !cond() {
        assert!(
            Instant::now() < deadline,
            "the condition didn't become true within {ms}ms"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn replays_output_to_new_connection() {
    let h = spawn_hold(&["sh", "-c", "printf hi; cat"]);
    let mut c = connect(&h.sock);
    assert!(matches!(c.recv(), HoldOut::Hello { version: 1, .. }));
    assert!(c.read_until_contains("hi"));
    drop(c);
    let mut c2 = connect(&h.sock);
    c2.recv();
    assert!(c2.read_until_contains("hi"));
}

#[test]
fn forwards_input_and_resize() {
    let h = spawn_hold(&["sh"]);
    let mut c = connect(&h.sock);
    c.recv();
    c.send(HoldIn::Resize {
        cols: 100,
        rows: 40,
    });
    c.send(HoldIn::Input {
        data: b"stty size\n".to_vec(),
    });
    assert!(c.read_until_contains("40 100"));
}

#[test]
fn writes_exit_code_and_quits() {
    let h = spawn_hold(&["sh", "-c", "exit 3"]);
    wait_until(2000, || h.exit.exists());
    assert_eq!(std::fs::read_to_string(&h.exit).unwrap().trim(), "3");
    wait_until(2000, || !h.sock.exists());
}

#[test]
fn child_starts_in_cwd_with_extra_env() {
    let h = spawn_hold_with_env(
        &["sh", "-c", "printf '%s|%s' \"$FOO\" \"$(pwd -P)\"; cat"],
        &["FOO=a=b"],
    );
    let want = format!("a=b|{}", h._dir.path().canonicalize().unwrap().display());
    let mut c = connect(&h.sock);
    c.recv();
    assert!(c.read_until_contains(&want));
}

#[test]
fn hello_reports_child_pid_and_first_output_is_the_ring() {
    let h = spawn_hold(&["sh", "-c", "printf first; cat"]);
    let mut c = connect(&h.sock);
    let HoldOut::Hello { version, child_pid } = c.recv() else {
        panic!("the first message isn't Hello");
    };
    assert_eq!(version, 1);
    assert!(child_pid > 1);
    assert_ne!(child_pid, h.child.id());
    assert!(matches!(c.recv(), HoldOut::Output { .. }));
}

#[test]
fn new_connection_replaces_the_old_one() {
    let h = spawn_hold(&["sh"]);
    let mut c1 = connect(&h.sock);
    c1.recv();
    let mut c2 = connect(&h.sock);
    assert!(matches!(c2.recv(), HoldOut::Hello { .. }));
    // The previous connection will be dropped shortly.
    while c1.try_recv().is_some() {}
    // The new connection takes over input and output. The echo contains only `$((6*7))`; `42` appears only in the computed result.
    c2.send(HoldIn::Input {
        data: b"echo $((6*7))\n".to_vec(),
    });
    assert!(c2.read_until_contains("42"));
}

#[test]
fn delivers_final_output_then_exited() {
    let h = spawn_hold(&["sh", "-c", "read x; printf bye; exit 0"]);
    let mut c = connect(&h.sock);
    c.recv();
    c.send(HoldIn::Input {
        data: b"\n".to_vec(),
    });
    assert!(c.read_until_contains("bye"));
    assert_eq!(c.recv(), HoldOut::Exited { code: Some(0) });
}

#[test]
fn replay_is_capped_and_keeps_the_tail() {
    // After pouring out more than the ring limit (2 MiB), the replay a new connection receives is capped at the limit and must be the tail.
    let h = spawn_hold(&[
        "sh",
        "-c",
        "head -c 2500000 /dev/zero | tr '\\0' x; printf END; cat",
    ]);
    let mut c = connect(&h.sock);
    c.recv();
    assert!(c.read_until_contains("END"));
    let mut c2 = connect(&h.sock);
    c2.recv();
    let HoldOut::Output { data } = c2.recv() else {
        panic!("the message after Hello isn't Output");
    };
    assert_eq!(data.len(), 2 * 1024 * 1024);
    assert!(data.ends_with(b"xxxEND"));
}

#[test]
fn kill_hangs_up_the_child_and_records_signal() {
    let h = spawn_hold(&["sh", "-c", "sleep 30"]);
    let mut c = connect(&h.sock);
    c.recv();
    c.send(HoldIn::Kill);
    wait_until(2000, || h.exit.exists());
    assert_eq!(std::fs::read_to_string(&h.exit).unwrap(), "signal");
    wait_until(2000, || !h.sock.exists());
}

#[test]
fn kill_escalates_to_sigkill_when_hup_is_ignored() {
    let h = spawn_hold(&["sh", "-c", "trap '' HUP; echo ready; sleep 30"]);
    let mut c = connect(&h.sock);
    c.recv();
    // Send Kill only after the trap is set, so SIGHUP is ignored.
    assert!(c.read_until_contains("ready"));
    let killed_at = Instant::now();
    c.send(HoldIn::Kill);
    // SIGHUP is ignored and SIGKILL goes out after 2 seconds.
    wait_until(5000, || h.exit.exists());
    assert!(killed_at.elapsed() >= Duration::from_millis(1500));
    assert_eq!(std::fs::read_to_string(&h.exit).unwrap(), "signal");
}

#[test]
fn survives_a_dropped_connection_and_keeps_running() {
    let h = spawn_hold(&["sh"]);
    let mut c = connect(&h.sock);
    c.recv();
    drop(c);
    // Even with no connection, the holder and child keep running and accept the next connection.
    let mut c2 = connect(&h.sock);
    c2.recv();
    c2.send(HoldIn::Input {
        data: b"echo $((6*7))\n".to_vec(),
    });
    assert!(c2.read_until_contains("42"));
    assert!(!h.exit.exists());
}

#[test]
fn stops_accepting_once_killed() {
    // A daemon that has just started a new holder for the same session must not wrongly attach to this one, which is about to end.
    let h = spawn_hold(&["sh", "-c", "trap '' HUP; echo ready; sleep 30"]);
    let mut c = connect(&h.sock);
    c.recv();
    assert!(c.read_until_contains("ready"));
    c.send(HoldIn::Kill);
    // Refuse the new connection before SIGKILL (after 2 seconds).
    wait_until(1000, || UnixStream::connect(&h.sock).is_err());
    wait_until(5000, || h.exit.exists());
}
