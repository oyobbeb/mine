//! The desktop app's connection to the daemon. A tokio runtime on a separate thread stays attached to the daemon and reattaches if the connection drops.
//! It talks to GPUI only through futures channels, so this module knows nothing about GPUI.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::conn::{AsyncConn, connect_or_spawn_async_with};
use crate::paths::State;
use crate::proto::{FromDaemon, ToDaemon};

/// How long to wait before the first reattach after a drop. Doubles on each failure.
const RETRY_FIRST: Duration = Duration::from_millis(500);
/// Upper bound on the interval between reattach attempts.
const RETRY_MAX: Duration = Duration::from_secs(5);

/// What the connection thread tells the app.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkEvent {
    /// Attached to the daemon and exchanged `Hello`. The app sends the subscription (`List`) and reattach (`Attach`).
    Connected,
    Msg(FromDaemon),
    /// The connection dropped or could not attach. It will reattach soon.
    Disconnected(String),
}

/// Channels to and from the connection thread. A message sent on `tx` goes to the daemon only while there is a connection, and is discarded otherwise.
/// Dropping the link ends the thread, and after that the daemon is not started again.
pub struct DaemonLink {
    pub tx: UnboundedSender<ToDaemon>,
    pub rx: UnboundedReceiver<LinkEvent>,
}

/// Starts the connection thread. If there is no daemon, it starts `daemon_exe daemon` (`spawn_env` is the same as in
/// [`connect_or_spawn_async_with`]).
///
/// It sends `Connected` when attached, `Msg` for each message received, and `Disconnected` when the connection drops or can't attach. After a drop it
/// reattaches with intervals doubling from `RETRY_FIRST` up to `RETRY_MAX`. The app does the subscription after reattaching.
pub fn start(
    state: State,
    daemon_exe: PathBuf,
    spawn_env: Option<Vec<(String, String)>>,
) -> DaemonLink {
    let (tx, outgoing) = mpsc::unbounded();
    let (events, rx) = mpsc::unbounded();
    std::thread::Builder::new()
        .name("mine-link".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let reason = format!("Couldn't create the daemon connection runtime: {e}");
                    let _ = events.unbounded_send(LinkEvent::Disconnected(reason));
                    return;
                }
            };
            rt.block_on(run(state, daemon_exe, spawn_env, outgoing, events));
        })
        .expect("couldn't spawn the daemon connection thread");
    DaemonLink { tx, rx }
}

/// Stays attached, carries messages, and when the connection drops waits and reattaches, until the app drops the link.
async fn run(
    state: State,
    daemon_exe: PathBuf,
    spawn_env: Option<Vec<(String, String)>>,
    mut outgoing: UnboundedReceiver<ToDaemon>,
    events: UnboundedSender<LinkEvent>,
) {
    let mut wait = RETRY_FIRST;
    // If the app dropped the receiving side, finish without starting the daemon again.
    while !events.is_closed() {
        let reason =
            match connect_or_spawn_async_with(&state, &daemon_exe, spawn_env.as_deref()).await {
                Ok(conn) => {
                    wait = RETRY_FIRST;
                    // Messages queued so far arrived while there was no connection, so they are discarded. The subscription and reattach needed after attaching
                    // are sent fresh by the app once it receives `Connected`.
                    while outgoing.try_recv().is_ok() {}
                    if events.unbounded_send(LinkEvent::Connected).is_err() {
                        return;
                    }
                    match pump(conn, &mut outgoing, &events).await {
                        Some(reason) => reason,
                        None => return,
                    }
                }
                Err(e) => format!("{e:#}"),
            };
        if events
            .unbounded_send(LinkEvent::Disconnected(reason))
            .is_err()
        {
            return;
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(RETRY_MAX);
    }
}

/// Carries messages both ways until the connection drops. Returns the reason if it dropped, or `None` if the app dropped the link.
///
/// Only cancel-safe channel receives go into `select!`. Socket reads (`proto::recv`) are handled by the single read task of `AsyncConn`.
async fn pump(
    conn: AsyncConn,
    outgoing: &mut UnboundedReceiver<ToDaemon>,
    events: &UnboundedSender<LinkEvent>,
) -> Option<String> {
    let AsyncConn { tx, mut rx } = conn;
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(msg) => events.unbounded_send(LinkEvent::Msg(msg)).ok()?,
                None => return Some("Disconnected from the daemon".into()),
            },
            msg = outgoing.next() => {
                // If it is `None`, the app dropped the link. If sending fails, the write task has ended, and `rx` will soon report the end.
                let _ = tx.send(msg?);
            }
        }
    }
}

/// The `mine` executable the app starts the daemon with. Looked up in this order: `MINE_BIN` (`override_var`), next to the app executable (`exe_dir`), then `PATH`.
///
/// `MINE_BIN` is used as is if it is not empty. The other two count only files with execute permission, and skip empty `PATH` entries.
///
/// The returned path is always absolute. A relative path is resolved against the current directory. The daemon is started from `/`,
/// so passing a relative path through unchanged would look in the wrong place.
pub fn find_mine(
    override_var: Option<OsString>,
    exe_dir: Option<&Path>,
    path_var: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(bin) = override_var.filter(|v| !v.is_empty()) {
        return std::path::absolute(bin).ok();
    }
    let on_path = path_var
        .iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| !dir.as_os_str().is_empty());
    let found = exe_dir
        .map(Path::to_path_buf)
        .into_iter()
        .chain(on_path)
        .map(|dir| dir.join("mine"))
        .find(|path| is_executable(path))?;
    std::path::absolute(found).ok()
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Puts a fake file with execute permission at `dir/mine`.
    fn fake_mine(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("mine");
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn find_mine_order() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("app");
        let bin1 = tmp.path().join("bin1");
        let bin2 = tmp.path().join("bin2");
        let path_var = || Some(std::env::join_paths([&bin1, &bin2]).unwrap());
        // If it is nowhere, the result is None. Files without execute permission don't count.
        fs::create_dir_all(&bin1).unwrap();
        fs::write(bin1.join("mine"), "").unwrap();
        assert_eq!(find_mine(None, Some(&exe_dir), path_var()), None);
        assert_eq!(find_mine(None, None, None), None);
        // In PATH, directories are searched from the front.
        let on_path = fake_mine(&bin2);
        assert_eq!(find_mine(None, Some(&exe_dir), path_var()), Some(on_path));
        let first = fake_mine(&bin1);
        assert_eq!(
            find_mine(None, Some(&exe_dir), path_var()),
            Some(first.clone())
        );
        // Next to the executable comes before PATH.
        let sibling = fake_mine(&exe_dir);
        assert_eq!(find_mine(None, Some(&exe_dir), path_var()), Some(sibling));
        assert_eq!(find_mine(None, None, path_var()), Some(first));
        // If MINE_BIN is set, that is the result. If it is empty, it counts as unset.
        let bin = tmp.path().join("custom-mine");
        assert_eq!(
            find_mine(Some(bin.clone().into()), Some(&exe_dir), path_var()),
            Some(bin)
        );
        assert_eq!(
            find_mine(Some(OsString::new()), Some(&exe_dir), None),
            Some(exe_dir.join("mine"))
        );
        // A relative MINE_BIN is turned into an absolute path from the current directory. The daemon is started from `/`.
        let cwd = std::env::current_dir().unwrap();
        let relative = find_mine(Some("sub/mine".into()), None, None).unwrap();
        assert!(relative.is_absolute(), "{relative:?}");
        assert_eq!(relative, cwd.join("sub/mine"));
        // An empty PATH entry (the current directory) is skipped and the next entry is checked.
        let with_empty = std::env::join_paths([Path::new(""), &bin2]).unwrap();
        assert_eq!(find_mine(None, None, Some(":".into())), None);
        assert_eq!(
            find_mine(None, None, Some(with_empty)),
            Some(bin2.join("mine"))
        );
    }

    /// Turns the absolute path `path` into a path relative to the current directory (`../../…`).
    fn relative_to_cwd(path: &Path) -> PathBuf {
        let cwd = std::env::current_dir().unwrap();
        let mut rel = PathBuf::new();
        for _ in 1..cwd.components().count() {
            rel.push("..");
        }
        rel.join(path.strip_prefix("/").unwrap())
    }

    /// A `mine` found through a relative path also runs the way the daemon is started (fork from `/`, then exec).
    #[test]
    fn found_mine_spawns_from_root() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let bin = fake_mine(tmp.path());
        let rel_dir = relative_to_cwd(tmp.path());
        assert!(rel_dir.is_relative());
        let from_path = find_mine(None, None, Some(rel_dir.clone().into())).unwrap();
        let from_override = find_mine(Some(rel_dir.join("mine").into()), None, None).unwrap();
        for found in [from_path, from_override] {
            assert!(found.is_absolute(), "{found:?}");
            assert_eq!(found.canonicalize().unwrap(), bin.canonicalize().unwrap());
            let mut cmd = Command::new(&found);
            cmd.current_dir("/");
            // SAFETY: nothing is done between fork and exec. It takes the same fork+exec path as `spawn_daemon`.
            unsafe { cmd.pre_exec(|| Ok(())) };
            let status = cmd.status().unwrap_or_else(|e| panic!("{found:?}: {e}"));
            assert!(status.success());
        }
    }
}
