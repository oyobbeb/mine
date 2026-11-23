//! `mine hold` 통합 테스트. 실제 바이너리를 띄워 소켓으로 붙는다.

use std::io::{BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mine_core::proto::{HoldIn, HoldOut, read_msg, write_msg};

/// 소켓 읽기와 출력 찾기가 기다리는 최대 시간.
const READ_WAIT: Duration = Duration::from_secs(3);

/// 띄운 보관 프로세스. 버려질 때 자식까지 정리한다.
struct Hold {
    sock: PathBuf,
    exit: PathBuf,
    child: Child,
    _dir: tempfile::TempDir,
}

/// 보관 프로세스를 띄운다. 소켓이 생기길 기다리지 않는다(자식이 곧바로 끝나는 경우도 있다).
fn spawn_hold(argv: &[&str]) -> Hold {
    spawn_hold_with_env(argv, &[])
}

/// `--env K=V`를 더해 보관 프로세스를 띄운다.
fn spawn_hold_with_env(argv: &[&str], env: &[&str]) -> Hold {
    // 유닉스 소켓 경로 한도(macOS 104바이트) 때문에 짧은 경로를 쓴다.
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
        // 자식 프로세스가 남지 않도록 Kill을 먼저 보내 보고, 그래도 안 끝나면 보관 프로세스를 죽인다.
        // 붙은 연결에는 재생분을 보내 주므로, 쓰기가 막히지 않게 끝날 때까지 읽어서 버린다.
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

/// 소켓에 붙는다. 소켓이 아직 없거나 막 만들어지는 중일 수 있어 기다리며 다시 시도한다.
fn connect(sock: &Path) -> Client {
    let deadline = Instant::now() + READ_WAIT;
    let stream = loop {
        match UnixStream::connect(sock) {
            Ok(stream) => break stream,
            Err(e) => assert!(
                Instant::now() < deadline,
                "소켓에 붙지 못했다: {} ({e})",
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
    /// 다음 메시지를 읽는다. 연결이 끝났으면 `None`이다. 시간 안에 오지 않으면 패닉한다.
    fn try_recv(&mut self) -> Option<HoldOut> {
        read_msg(&mut self.reader).expect("메시지를 시간 안에 읽지 못했다")
    }

    fn recv(&mut self) -> HoldOut {
        self.try_recv().expect("연결이 끊겼다")
    }

    fn send(&mut self, msg: HoldIn) {
        write_msg(&mut self.writer, &msg).unwrap();
    }

    /// `Output`을 최대 3초 동안 모아서 `needle`이 나오는지 본다. 연결이 끝나거나 `Exited`가 오면 포기한다.
    fn read_until_contains(&mut self, needle: &str) -> bool {
        let deadline = Instant::now() + READ_WAIT;
        let mut seen = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            // 상대가 이미 끊은 소켓에는 macOS가 타임아웃 설정을 거부하므로 실패해도 넘어간다.
            let _ = self.reader.get_ref().set_read_timeout(Some(left));
            match read_msg::<_, HoldOut>(&mut self.reader) {
                Ok(Some(HoldOut::Output { data })) => {
                    // 매번 전체를 훑지 않도록 새 조각과 그 앞 경계만 본다.
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

/// 조건이 참이 될 때까지 `ms` 밀리초 안에서 기다린다. 시간이 지나면 패닉한다.
fn wait_until(ms: u64, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while !cond() {
        assert!(
            Instant::now() < deadline,
            "{ms}ms 안에 조건이 참이 되지 않았다"
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
        panic!("첫 메시지가 Hello가 아니다");
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
    // 이전 연결은 곧 끊긴다.
    while c1.try_recv().is_some() {}
    // 새 연결이 입력과 출력을 이어받는다. 에코에는 `$((6*7))`만 있고 `42`는 계산 결과에만 있다.
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
    // 링 한도(2 MiB)보다 많이 쏟아낸 뒤 새 연결이 받는 재생분은 한도까지만이고 끝부분이어야 한다.
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
        panic!("Hello 다음이 Output이 아니다");
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
    // trap이 걸린 뒤에 Kill을 보내야 SIGHUP이 무시된다.
    assert!(c.read_until_contains("ready"));
    let killed_at = Instant::now();
    c.send(HoldIn::Kill);
    // SIGHUP은 무시되고 2초 뒤 SIGKILL이 간다.
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
    // 연결이 없어도 보관 프로세스와 자식은 계속 돌고, 다음 연결을 받는다.
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
    // 끝나 가는 보관 프로세스에, 같은 세션을 새로 띄운 데몬이 잘못 붙지 않아야 한다.
    let h = spawn_hold(&["sh", "-c", "trap '' HUP; echo ready; sleep 30"]);
    let mut c = connect(&h.sock);
    c.recv();
    assert!(c.read_until_contains("ready"));
    c.send(HoldIn::Kill);
    // SIGKILL(2초 뒤)보다 먼저 새 연결을 거절한다.
    wait_until(1000, || UnixStream::connect(&h.sock).is_err());
    wait_until(5000, || h.exit.exists());
}
