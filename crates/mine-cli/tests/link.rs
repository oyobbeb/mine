//! 데스크톱 앱이 쓰는 데몬 연결(`mine_core::link`) 통합 테스트. 실제 `mine` 바이너리로 데몬을 띄운다.
//!
//! 상태 폴더는 따로 만든 짧은 `/tmp` 폴더다. 테스트가 띄운 데몬은 끝에 pid 파일로 찾아 SIGKILL한다.

use std::fs::{self, File, TryLockError};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use mine_core::link::{self, DaemonLink, LinkEvent};
use mine_core::paths::State;
use mine_core::proto::{FromDaemon, ToDaemon};

/// 다시 붙기까지 기다리는 최대 시간.
const WAIT: Duration = Duration::from_secs(30);

/// 테스트 하나의 상태 폴더. 버려질 때 그 폴더의 데몬을 없앤다.
struct St {
    root: PathBuf,
    _dir: tempfile::TempDir,
}

impl St {
    fn new() -> St {
        let dir = tempfile::Builder::new()
            .prefix("m")
            .tempdir_in("/tmp")
            .unwrap();
        St {
            root: dir.path().join("s"),
            _dir: dir,
        }
    }

    fn sock(&self) -> PathBuf {
        self.root.join("daemon.sock")
    }

    /// 살아 있는 데몬의 pid. 데몬은 pid 파일을 잠가 두므로, 잠글 수 있으면 데몬이 없는 것이다.
    fn daemon_pid(&self) -> Option<i32> {
        let path = self.root.join("daemon.pid");
        let file = File::open(&path).ok()?;
        match file.try_lock() {
            Err(TryLockError::WouldBlock) => fs::read_to_string(&path).ok()?.trim().parse().ok(),
            _ => None,
        }
    }

    /// pid 파일의 데몬을 SIGKILL하고 소켓이 더는 연결을 받지 않을 때까지 기다린다.
    fn kill_daemon(&self) -> Option<i32> {
        let pid = self.daemon_pid()?;
        // SAFETY: kill은 정수 인자만 받는다. pid는 이 테스트의 데몬이 잠가 둔 pid 파일에서 읽었다.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let deadline = Instant::now() + WAIT;
        while UnixStream::connect(self.sock()).is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(pid)
    }
}

impl Drop for St {
    fn drop(&mut self) {
        self.kill_daemon();
    }
}

/// `pick`이 참인 일이 올 때까지 다른 일은 건너뛴다. `deadline`까지 오지 않으면 패닉한다.
fn wait_for(
    link: &mut DaemonLink,
    deadline: Instant,
    what: &str,
    pick: impl Fn(&LinkEvent) -> bool,
) -> LinkEvent {
    loop {
        match link.rx.try_recv() {
            Ok(ev) if pick(&ev) => return ev,
            Ok(_) => {}
            Err(_) => {
                assert!(Instant::now() < deadline, "{what}이(가) 오지 않았다");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn sessions(ev: &LinkEvent) -> bool {
    matches!(ev, LinkEvent::Msg(FromDaemon::Sessions { .. }))
}

#[test]
fn link_reconnects_after_daemon_restart() {
    let st = St::new();
    let state = State {
        root: st.root.clone(),
    };
    let mut link = link::start(state, PathBuf::from(env!("CARGO_BIN_EXE_mine")), None);
    let deadline = Instant::now() + WAIT;
    wait_for(&mut link, deadline, "Connected", |ev| {
        *ev == LinkEvent::Connected
    });
    link.tx.unbounded_send(ToDaemon::List).unwrap();
    wait_for(&mut link, deadline, "첫 Sessions", sessions);

    let first = st.kill_daemon().expect("링크가 띄운 데몬이 없다");
    let deadline = Instant::now() + WAIT;
    wait_for(&mut link, deadline, "Disconnected", |ev| {
        matches!(ev, LinkEvent::Disconnected(_))
    });
    wait_for(&mut link, deadline, "다시 Connected", |ev| {
        *ev == LinkEvent::Connected
    });
    link.tx.unbounded_send(ToDaemon::List).unwrap();
    wait_for(&mut link, deadline, "다시 붙은 뒤 Sessions", sessions);
    let second = st.daemon_pid().expect("다시 띄운 데몬이 없다");
    assert_ne!(first, second, "새 데몬이 떠야 한다");

    // 링크를 먼저 버려야 정리하며 죽인 데몬을 다시 띄우지 않는다.
    drop(link);
}
