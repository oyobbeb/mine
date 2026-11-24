//! 데스크톱 앱의 데몬 연결. 별도 스레드의 tokio 런타임이 데몬에 붙어 있다가, 끊기면 다시 붙는다.
//! GPUI와는 futures 채널로만 주고받아서 이 모듈은 GPUI를 모른다.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::conn::{AsyncConn, connect_or_spawn_async_with};
use crate::paths::State;
use crate::proto::{FromDaemon, ToDaemon};

/// 끊긴 뒤 처음 다시 붙기까지 기다리는 시간. 실패할 때마다 두 배로 늘린다.
const RETRY_FIRST: Duration = Duration::from_millis(500);
/// 다시 붙기 사이 간격의 상한.
const RETRY_MAX: Duration = Duration::from_secs(5);

/// 연결 스레드가 앱에 알리는 일.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkEvent {
    /// 데몬에 붙어 `Hello`까지 주고받았다. 구독(`List`)과 다시 붙기(`Attach`)는 앱이 보낸다.
    Connected,
    Msg(FromDaemon),
    /// 연결이 끊겼거나 붙지 못했다. 곧 다시 붙는다.
    Disconnected(String),
}

/// 연결 스레드와 주고받는 채널. `tx`로 보낸 메시지는 연결이 있을 때만 데몬에 가고, 없을 때는 버린다.
/// 링크를 버리면 스레드가 끝나고, 그 뒤로는 데몬을 다시 띄우지 않는다.
pub struct DaemonLink {
    pub tx: UnboundedSender<ToDaemon>,
    pub rx: UnboundedReceiver<LinkEvent>,
}

/// 연결 스레드를 띄운다. 데몬이 없으면 `daemon_exe daemon`을 띄운다(`spawn_env`는
/// [`connect_or_spawn_async_with`]와 같다).
///
/// 붙으면 `Connected`, 받은 메시지는 `Msg`, 끊기거나 붙지 못하면 `Disconnected`를 보낸다. 끊긴 뒤에는
/// `RETRY_FIRST`부터 두 배씩, 최대 `RETRY_MAX` 간격으로 다시 붙는다. 다시 붙은 뒤의 구독은 앱이 한다.
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

/// 앱이 링크를 버릴 때까지 붙고, 나르고, 끊기면 기다렸다 다시 붙는다.
async fn run(
    state: State,
    daemon_exe: PathBuf,
    spawn_env: Option<Vec<(String, String)>>,
    mut outgoing: UnboundedReceiver<ToDaemon>,
    events: UnboundedSender<LinkEvent>,
) {
    let mut wait = RETRY_FIRST;
    // 앱이 받는 쪽을 버렸으면 데몬을 다시 띄우지 않고 끝낸다.
    while !events.is_closed() {
        let reason =
            match connect_or_spawn_async_with(&state, &daemon_exe, spawn_env.as_deref()).await {
                Ok(conn) => {
                    wait = RETRY_FIRST;
                    // 지금까지 쌓인 메시지는 연결이 없을 때 온 것이라 버린다. 붙은 뒤에 필요한 구독과 다시 붙기는
                    // 앱이 `Connected`를 받고 새로 보낸다.
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

/// 연결이 끊길 때까지 양쪽 메시지를 나른다. 끊기면 사유를, 앱이 링크를 버렸으면 `None`을 돌려준다.
///
/// `select!`에는 취소에 안전한 채널 수신만 넣는다. 소켓 읽기(`proto::recv`)는 `AsyncConn`의 읽기 작업 하나가 맡는다.
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
                // `None`이면 앱이 링크를 버렸다. 보내기가 실패하면 쓰기 작업이 끝난 것이고, 곧 `rx`가 끝을 알린다.
                let _ = tx.send(msg?);
            }
        }
    }
}

/// 앱이 데몬을 띄울 `mine` 실행 파일. `MINE_BIN`(`override_var`), 앱 실행 파일 옆(`exe_dir`), `PATH` 순서로 찾는다.
///
/// `MINE_BIN`은 비어 있지 않으면 그대로 쓴다. 나머지 둘은 실행 권한이 있는 파일만 치고, 빈 `PATH` 칸은 건너뛴다.
///
/// 돌려주는 경로는 늘 절대 경로다. 상대 경로는 지금 폴더 기준으로 바꾼다. 데몬은 `/`에서 띄우므로
/// 상대 경로를 그대로 넘기면 엉뚱한 곳을 찾는다.
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

    /// `dir/mine`에 실행 권한이 있는 가짜 파일을 둔다.
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
        // 아무 데도 없으면 None이다. 실행 권한이 없는 파일은 치지 않는다.
        fs::create_dir_all(&bin1).unwrap();
        fs::write(bin1.join("mine"), "").unwrap();
        assert_eq!(find_mine(None, Some(&exe_dir), path_var()), None);
        assert_eq!(find_mine(None, None, None), None);
        // PATH에서는 앞 폴더부터 찾는다.
        let on_path = fake_mine(&bin2);
        assert_eq!(find_mine(None, Some(&exe_dir), path_var()), Some(on_path));
        let first = fake_mine(&bin1);
        assert_eq!(
            find_mine(None, Some(&exe_dir), path_var()),
            Some(first.clone())
        );
        // 실행 파일 옆이 PATH보다 먼저다.
        let sibling = fake_mine(&exe_dir);
        assert_eq!(find_mine(None, Some(&exe_dir), path_var()), Some(sibling));
        assert_eq!(find_mine(None, None, path_var()), Some(first));
        // MINE_BIN이 있으면 그것이다. 비어 있으면 없는 것으로 본다.
        let bin = tmp.path().join("custom-mine");
        assert_eq!(
            find_mine(Some(bin.clone().into()), Some(&exe_dir), path_var()),
            Some(bin)
        );
        assert_eq!(
            find_mine(Some(OsString::new()), Some(&exe_dir), None),
            Some(exe_dir.join("mine"))
        );
        // 상대 경로 MINE_BIN은 지금 폴더 기준의 절대 경로로 바꾼다. 데몬은 `/`에서 띄우기 때문이다.
        let cwd = std::env::current_dir().unwrap();
        let relative = find_mine(Some("sub/mine".into()), None, None).unwrap();
        assert!(relative.is_absolute(), "{relative:?}");
        assert_eq!(relative, cwd.join("sub/mine"));
        // 빈 PATH 칸(지금 폴더)은 건너뛰고 다음 칸을 본다.
        let with_empty = std::env::join_paths([Path::new(""), &bin2]).unwrap();
        assert_eq!(find_mine(None, None, Some(":".into())), None);
        assert_eq!(
            find_mine(None, None, Some(with_empty)),
            Some(bin2.join("mine"))
        );
    }

    /// 절대 경로 `path`를 지금 폴더에서 본 상대 경로(`../../…`)로 바꾼다.
    fn relative_to_cwd(path: &Path) -> PathBuf {
        let cwd = std::env::current_dir().unwrap();
        let mut rel = PathBuf::new();
        for _ in 1..cwd.components().count() {
            rel.push("..");
        }
        rel.join(path.strip_prefix("/").unwrap())
    }

    /// 상대 경로로 찾은 `mine`도 데몬을 띄우는 방법(`/`에서 fork 뒤 exec)으로 실행된다.
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
            // SAFETY: fork 뒤 exec 전에 아무것도 하지 않는다. `spawn_daemon`처럼 fork+exec 경로를 타게 한다.
            unsafe { cmd.pre_exec(|| Ok(())) };
            let status = cmd.status().unwrap_or_else(|e| panic!("{found:?}: {e}"));
            assert!(status.success());
        }
    }
}
