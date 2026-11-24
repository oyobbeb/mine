//! Integration test for the daemon connection the desktop app uses (`mine_core::link`). It starts the daemon with the real `mine` binary.
//!
//! The state directory is a separate short `/tmp` directory. At the end, the daemon the test started is found via its pid file and sent SIGKILL.

use std::fs::{self, File, TryLockError};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use mine_core::link::{self, DaemonLink, LinkEvent};
use mine_core::paths::State;
use mine_core::proto::{FromDaemon, ToDaemon};

/// Maximum time to wait for the reconnection.
const WAIT: Duration = Duration::from_secs(30);

/// One test's state directory. When dropped, it kills that directory's daemon.
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

    /// pid of the live daemon. The daemon keeps the pid file locked, so if it can be locked, there is no daemon.
    fn daemon_pid(&self) -> Option<i32> {
        let path = self.root.join("daemon.pid");
        let file = File::open(&path).ok()?;
        match file.try_lock() {
            Err(TryLockError::WouldBlock) => fs::read_to_string(&path).ok()?.trim().parse().ok(),
            _ => None,
        }
    }

    /// SIGKILLs the daemon in the pid file and waits until the socket no longer accepts connections.
    fn kill_daemon(&self) -> Option<i32> {
        let pid = self.daemon_pid()?;
        // SAFETY: kill takes only integer arguments. The pid was read from the pid file that this test's daemon keeps locked.
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

/// Skips other events until one for which `pick` is true arrives. Panics if none arrives by `deadline`.
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
                assert!(Instant::now() < deadline, "{what} didn't arrive");
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
    // The daemon gets only a no-op `MINE_NOTIFY` (and `MINE_STATE_DIR`), so it can never pop a real notification.
    let env = vec![("MINE_NOTIFY".to_string(), "/usr/bin/true".to_string())];
    let mut link = link::start(state, PathBuf::from(env!("CARGO_BIN_EXE_mine")), Some(env));
    let deadline = Instant::now() + WAIT;
    wait_for(&mut link, deadline, "Connected", |ev| {
        matches!(ev, LinkEvent::Connected(_))
    });
    link.tx.unbounded_send(ToDaemon::List).unwrap();
    wait_for(&mut link, deadline, "first Sessions", sessions);

    let first = st.kill_daemon().expect("the link started no daemon");
    let deadline = Instant::now() + WAIT;
    wait_for(&mut link, deadline, "Disconnected", |ev| {
        matches!(ev, LinkEvent::Disconnected(_))
    });
    wait_for(&mut link, deadline, "Connected again", |ev| {
        matches!(ev, LinkEvent::Connected(_))
    });
    link.tx.unbounded_send(ToDaemon::List).unwrap();
    wait_for(&mut link, deadline, "Sessions after reconnecting", sessions);
    let second = st.daemon_pid().expect("no daemon was started again");
    assert_ne!(first, second, "a new daemon should have started");

    // Drop the link first, or it would restart the daemon we killed during cleanup.
    drop(link);
}
