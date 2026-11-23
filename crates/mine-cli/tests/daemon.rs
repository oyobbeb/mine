//! 데몬과 헤드리스 명령 통합 테스트. 실제 바이너리로 데몬과 보관 프로세스를 띄운다.
//!
//! 모든 `mine` 명령은 `.git` 폴더가 있는 임시 프로젝트 폴더에서, 따로 만든 짧은 `/tmp` 상태 폴더
//! (`MINE_STATE_DIR`)와 `SHELL=/bin/sh`로 실행한다. 테스트 프로세스의 환경은 건드리지 않는다.

use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use mine_core::paths::State;
use mine_core::project::project_key;
use mine_core::proto::{FromDaemon, HoldIn, HoldOut, ToDaemon, VERSION, read_msg, write_msg};
use mine_core::session::{Kind, Meta, Restore, SessionState, session_id};

/// 소켓 읽기와 조건 기다리기의 최대 시간. 테스트를 여럿 함께 돌려 느려져도 넉넉하게 잡는다.
const WAIT: Duration = Duration::from_secs(10);

/// 테스트 하나가 쓰는 프로젝트 폴더와 상태 폴더. 버려질 때 데몬과 보관 프로세스를 정리한다.
struct St {
    root: PathBuf,
    project: PathBuf,
    /// 있으면 `PATH` 앞에 붙인다(가짜 `claude` 등).
    path_front: Option<PathBuf>,
    /// `mine` 명령(과 그 명령이 띄우는 데몬)에 더할 환경 변수. 앞의 기본값(`SHELL` 등)을 덮어쓴다.
    env: Vec<(&'static str, OsString)>,
    _state: tempfile::TempDir,
    _project: tempfile::TempDir,
}

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("m")
        .tempdir_in("/tmp")
        .unwrap()
}

fn state() -> St {
    let state = tmp();
    let project = tmp();
    fs::create_dir(project.path().join(".git")).unwrap();
    St {
        root: state.path().join("s"),
        project: project.path().canonicalize().unwrap(),
        path_front: None,
        env: Vec::new(),
        _state: state,
        _project: project,
    }
}

fn state_with_path(dir: &Path) -> St {
    let mut st = state();
    st.path_front = Some(dir.to_path_buf());
    st
}

impl St {
    fn sock(&self) -> PathBuf {
        self.root.join("daemon.sock")
    }

    fn id(&self, name: &str) -> String {
        session_id(&project_key(&self.project), name)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mine"));
        cmd.args(args)
            .current_dir(&self.project)
            .env("MINE_STATE_DIR", &self.root)
            .env("SHELL", "/bin/sh");
        if let Some(front) = &self.path_front {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut joined = front.clone().into_os_string();
            joined.push(":");
            joined.push(path);
            cmd.env("PATH", joined);
        }
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        cmd
    }
}

impl Drop for St {
    fn drop(&mut self) {
        // 데몬을 먼저 없애야 보관 프로세스에 다시 붙지 않는다.
        if let Some(pid) = daemon_pid(self) {
            // SAFETY: kill은 정수 인자만 받는다. pid는 이 테스트가 띄운 데몬이 잠가 둔 pid 파일에서 읽었다.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let deadline = Instant::now() + WAIT;
            while UnixStream::connect(self.sock()).is_ok() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let socks: Vec<PathBuf> = fs::read_dir(self.root.join("run"))
            .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        for sock in &socks {
            kill_holder(sock);
        }
    }
}

/// 보관 프로세스에 `Kill`을 보내고, 재생분을 읽어 버리며 끝날 때까지(최대 `WAIT`) 기다린다.
fn kill_holder(sock: &Path) {
    let Ok(stream) = UnixStream::connect(sock) else {
        return;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let _ = write_msg(&mut &stream, &HoldIn::Kill);
    let mut sink = vec![0u8; 64 * 1024];
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline && sock.exists() {
        if let Ok(0) = (&stream).read(&mut sink) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// 살아 있는 데몬의 pid. 데몬은 pid 파일을 잠가 두므로, 잠글 수 있으면 데몬이 없는 것이다.
fn daemon_pid(st: &St) -> Option<i32> {
    let path = st.root.join("daemon.pid");
    let file = File::open(&path).ok()?;
    match file.try_lock() {
        Err(TryLockError::WouldBlock) => fs::read_to_string(&path).ok()?.trim().parse().ok(),
        _ => None,
    }
}

/// `mine`을 실행하고 성공했는지 확인한다.
fn mine(st: &St, args: &[&str]) -> Output {
    let out = mine_raw(st, args);
    assert!(
        out.status.success(),
        "mine {args:?} 실패: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn mine_raw(st: &St, args: &[&str]) -> Output {
    st.command(args)
        .stdin(Stdio::null())
        .output()
        .expect("mine을 실행하지 못했다")
}

fn mine_stdin(st: &St, args: &[&str], input: &str) -> Output {
    let mut child = st
        .command(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "mine {args:?} 실패");
    out
}

fn ls(st: &St) -> Vec<Vec<String>> {
    let out = mine(st, &["ls"]);
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| line.split('\t').map(str::to_owned).collect())
        .collect()
}

fn session_dir(st: &St, name: &str) -> String {
    st.root
        .join("sessions")
        .join(st.id(name))
        .to_string_lossy()
        .into_owned()
}

/// pid 파일로 데몬을 SIGKILL하고, 소켓이 더는 연결을 받지 않을 때까지 기다린다.
fn kill_daemon(st: &St) {
    let pid = daemon_pid(st).expect("데몬이 돌고 있지 않다");
    // SAFETY: kill은 정수 인자만 받는다. pid는 이 테스트의 데몬이 잠가 둔 pid 파일에서 읽었다.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    wait_until(|| UnixStream::connect(st.sock()).is_err());
}

/// 조건이 참이 될 때까지 최대 `WAIT` 동안 기다린다.
fn wait_until(mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(
            Instant::now() < deadline,
            "{WAIT:?} 안에 조건이 참이 되지 않았다"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `Conn`을 흉내 내는 작은 소켓 클라이언트.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    key: String,
    attached: Option<String>,
}

/// 이미 떠 있는 데몬에 붙어 Hello를 주고받는다.
fn client(st: &St) -> Client {
    let stream = UnixStream::connect(st.sock()).expect("데몬에 붙지 못했다");
    let mut c = Client {
        writer: stream.try_clone().unwrap(),
        reader: BufReader::new(stream),
        key: project_key(&st.project),
        attached: None,
    };
    c.send(&ToDaemon::Hello { version: VERSION });
    assert!(matches!(c.recv(), Some(FromDaemon::Hello { version }) if version == VERSION));
    c
}

impl Client {
    fn send(&mut self, msg: &ToDaemon) {
        write_msg(&mut self.writer, msg).unwrap();
    }

    /// 다음 메시지. 연결이 끝났으면 `None`이다. `WAIT` 안에 오지 않으면 패닉한다.
    fn recv(&mut self) -> Option<FromDaemon> {
        self.reader.get_ref().set_read_timeout(Some(WAIT)).unwrap();
        read_msg(&mut self.reader).expect("메시지를 시간 안에 읽지 못했다")
    }

    /// 크기 80x24로 붙고 스냅숏을 글자로 돌려준다.
    fn attach(&mut self, name: &str) -> String {
        self.attach_sized(name, 80, 24)
    }

    fn attach_sized(&mut self, name: &str, cols: u16, rows: u16) -> String {
        let id = session_id(&self.key, name);
        self.send(&ToDaemon::Attach {
            id: id.clone(),
            cols,
            rows,
        });
        self.attached = Some(id.clone());
        loop {
            match self.recv().expect("연결이 끊겼다") {
                FromDaemon::Snapshot { id: got, data } if got == id => {
                    return String::from_utf8_lossy(&data).into_owned();
                }
                FromDaemon::Error { message } => panic!("붙지 못했다: {message}"),
                _ => {}
            }
        }
    }

    fn input(&mut self, text: &str) {
        let id = self.attached.clone().expect("붙은 세션이 없다");
        self.send(&ToDaemon::Input {
            id,
            data: text.as_bytes().to_vec(),
        });
    }

    /// 붙은 세션의 출력에 `needle`이 나올 때까지 최대 `WAIT` 동안 읽는다.
    fn wait_output(&mut self, needle: &str) {
        self.read_output(
            |seen| String::from_utf8_lossy(seen).contains(needle),
            needle,
            false,
        );
    }

    /// 붙은 세션의 출력이 `n`바이트 넘게 오거나, 처진 이 클라이언트를 데몬이 끊을 때까지 최대 `WAIT` 동안 읽는다.
    fn wait_bytes_or_drop(&mut self, n: usize) {
        self.read_output(|seen| seen.len() > n, &format!("{n}바이트"), true);
    }

    fn read_output(&mut self, mut done: impl FnMut(&[u8]) -> bool, what: &str, eof_ok: bool) {
        let id = self.attached.clone().expect("붙은 세션이 없다");
        let deadline = Instant::now() + WAIT;
        let mut seen = Vec::new();
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| {
                    panic!("출력에 {what}가 없다: {:?}", String::from_utf8_lossy(&seen))
                });
            self.reader.get_ref().set_read_timeout(Some(left)).unwrap();
            match read_msg::<_, FromDaemon>(&mut self.reader) {
                Ok(Some(FromDaemon::Output { id: got, data })) if got == id => {
                    seen.extend(data);
                    if done(&seen) {
                        return;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) if eof_ok => return,
                Ok(None) => panic!("연결이 끊겼다"),
                Err(e) => panic!(
                    "출력에 {what}가 없다 ({e}): {:?}",
                    String::from_utf8_lossy(&seen)
                ),
            }
        }
    }
}

/// 셸 세션이 다 뜨고 출력이 잦아들어 대기가 될 때까지 기다린다. 셸은 시작 프롬프트를 언제 낼지 모르므로
/// (늦게 오면 다시 작업 중이 된다) 명령 하나의 결과를 먼저 보고 나서 기다린다. 붙었던 연결은 끊고 기다린다.
fn wait_shell_quiet(st: &St, name: &str) {
    let mut c = client(st);
    c.attach(name);
    c.input("echo up-$((1+1))\n");
    c.wait_output("up-2");
    drop(c);
    wait_until(|| ls(st).iter().any(|row| row[2] == name && row[4] == "idle"));
}

#[test]
fn new_and_ls_show_shell_session() {
    let st = state();
    mine(&st, &["new", "dev"]);
    wait_shell_quiet(&st, "dev");
    let rows = ls(&st);
    assert_eq!(rows.len(), 1);
    let folder = st.project.file_name().unwrap().to_str().unwrap();
    assert_eq!(rows[0], ["○", folder, "dev", "-", "idle", "-"]);
}

#[test]
fn duplicate_name_is_rejected() {
    let st = state();
    mine(&st, &["new", "dev"]);
    let out = mine_raw(&st, &["new", "dev"]);
    assert!(!out.status.success());
    assert!(!out.stderr.is_empty());
    assert_eq!(ls(&st).len(), 1);
}

#[test]
fn invalid_name_is_rejected() {
    let st = state();
    assert!(!mine_raw(&st, &["new", "a/b"]).status.success());
    assert!(ls(&st).is_empty());
}

#[test]
fn failed_start_leaves_no_session() {
    // 실행 파일이 없으면 보관 프로세스가 exit 파일 없이 끝난다.
    let st = state();
    let mut cmd = st.command(&["new", "x", "--kind", "codex"]);
    let out = cmd.env("PATH", "/usr/bin:/bin").output().unwrap();
    assert!(!out.status.success());
    assert!(!out.stderr.is_empty());
    assert!(ls(&st).is_empty());
}

#[test]
fn session_survives_daemon_restart() {
    let st = state();
    mine(&st, &["new", "keep"]);
    let mut c = client(&st);
    c.attach("keep");
    c.input("echo marker-1\n");
    c.wait_output("marker-1");
    kill_daemon(&st);
    // 데몬이 자동으로 다시 뜨고 세션은 그대로다.
    assert_eq!(ls(&st)[0][4], "idle");
    let mut c2 = client(&st);
    assert!(c2.attach("keep").contains("marker-1"));
    // 다시 붙은 세션에 입력이 그대로 간다.
    c2.input("echo $((40+2))\n");
    c2.wait_output("42");
}

#[test]
fn kill_removes_from_list_and_keeps_events() {
    let st = state();
    mine(&st, &["new", "k"]);
    let dir = session_dir(&st, "k");
    mine_stdin(
        &st,
        &["_event", &dir],
        r#"{"hook_event_name":"UserPromptSubmit","prompt":"hi"}"#,
    );
    mine(&st, &["kill", "k"]);
    assert!(ls(&st).is_empty());
    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(Path::new(&dir).join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["closed"], true);
    assert!(Path::new(&dir).join("events.jsonl").exists());
    // 보관 프로세스도 끝난다.
    wait_until(|| !st.root.join("run").join(hold_sock_name(&st, "k")).exists());
    // 닫힌 세션은 데몬이 다시 떠도 돌아오지 않는다.
    kill_daemon(&st);
    assert!(ls(&st).is_empty());
}

fn hold_sock_name(st: &St, name: &str) -> String {
    let state = State {
        root: st.root.clone(),
    };
    state
        .hold_sock(&st.id(name))
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn kill_unknown_session_fails() {
    let st = state();
    assert!(!mine_raw(&st, &["kill", "nope"]).status.success());
}

#[test]
fn stale_socket_is_replaced() {
    let st = state();
    fs::create_dir_all(&st.root).unwrap();
    fs::write(st.sock(), "stale").unwrap();
    assert!(ls(&st).is_empty());
    assert!(UnixStream::connect(st.sock()).is_ok());
}

#[test]
fn version_mismatch_is_rejected() {
    let st = state();
    ls(&st); // 데몬을 띄운다.
    let stream = UnixStream::connect(st.sock()).unwrap();
    let mut r = BufReader::new(stream.try_clone().unwrap());
    stream.set_read_timeout(Some(WAIT)).unwrap();
    write_msg(&mut &stream, &ToDaemon::Hello { version: 999 }).unwrap();
    let mut error = None;
    while let Some(msg) = read_msg::<_, FromDaemon>(&mut r).unwrap() {
        if let FromDaemon::Error { message } = msg {
            error = Some(message);
        }
    }
    // 데몬을 다시 띄우는 실제 명령을 알려 준다.
    let want = format!("kill \"$(cat {})\"", st.root.join("daemon.pid").display());
    let error = error.expect("Error가 오지 않았다");
    assert!(error.contains(&want), "{error}");
}

#[test]
fn hook_poke_updates_state() {
    let st = state();
    mine(&st, &["new", "h"]);
    mine_stdin(
        &st,
        &["_event", &session_dir(&st, "h")],
        r#"{"hook_event_name":"Notification","message":"npm run deploy 승인?"}"#,
    );
    let r = &ls(&st)[0];
    assert_eq!(r[0], "◆");
    assert_eq!(r[4], "approval");
    assert_eq!(r[5], "npm run deploy 승인?");
}

#[test]
fn subscribed_client_gets_pushed_sessions_after_hook() {
    let st = state();
    mine(&st, &["new", "h"]);
    let mut c = client(&st);
    c.send(&ToDaemon::List);
    assert!(matches!(c.recv(), Some(FromDaemon::Sessions { .. })));
    mine_stdin(
        &st,
        &["_event", &session_dir(&st, "h")],
        r#"{"hook_event_name":"Notification","message":"승인?"}"#,
    );
    // List를 다시 보내지 않아도 바뀐 목록이 온다.
    loop {
        match c.recv().expect("연결이 끊겼다") {
            FromDaemon::Sessions { sessions } if sessions[0].state == SessionState::Approval => {
                assert_eq!(sessions[0].waiting.as_deref(), Some("승인?"));
                break;
            }
            _ => {}
        }
    }
}

#[test]
fn client_colours_answer_inner_colour_queries() {
    // 먼저 만든 세션(이미 있는 화면)과 색을 받은 뒤 만든 세션(새 화면) 모두 받은 색으로 답한다.
    let st = state();
    mine(&st, &["new", "old"]);
    let mut c = client(&st);
    c.send(&ToDaemon::Colors {
        fg: "#0a0b0c".into(),
        bg: "#010203".into(),
    });
    // 같은 연결의 메시지는 차례로 처리되므로, List의 답이 오면 Colors도 처리됐다.
    c.send(&ToDaemon::List);
    assert!(matches!(c.recv(), Some(FromDaemon::Sessions { .. })));
    mine(&st, &["new", "new"]);
    for name in ["old", "new"] {
        c.attach(name);
        c.input("stty -icanon -echo; printf '\\033]11;?\\007'; read -d \"$(printf '\\007')\" p; stty icanon echo; case \"$p\" in *rgb:0101/0202/0303*) echo BG-$((1+1));; esac\n");
        c.wait_output("BG-2");
    }
}

#[test]
fn input_during_approval_returns_to_working() {
    let st = state();
    mine(&st, &["new", "h"]);
    mine_stdin(
        &st,
        &["_event", &session_dir(&st, "h")],
        r#"{"hook_event_name":"Notification","message":"승인?"}"#,
    );
    let mut c = client(&st);
    c.attach("h");
    c.input("y");
    wait_until(|| ls(&st)[0][4] == "working");
}

#[test]
fn viewing_a_done_session_makes_it_idle() {
    let st = state();
    mine(&st, &["new", "d"]);
    // 셸 출력으로 정해지는 작업 중이 대기로 돌아간 뒤에 훅 상태만 본다.
    wait_shell_quiet(&st, "d");
    mine_stdin(
        &st,
        &["_event", &session_dir(&st, "d")],
        r#"{"hook_event_name":"Stop"}"#,
    );
    assert_eq!(ls(&st)[0][..1], ["✓"]);
    assert_eq!(ls(&st)[0][4], "done");
    let mut c = client(&st);
    c.attach("d");
    assert_eq!(ls(&st)[0][4], "idle");
    // 보고 있는 동안 끝난 턴은 바로 읽은 것으로 친다.
    mine_stdin(
        &st,
        &["_event", &session_dir(&st, "d")],
        r#"{"hook_event_name":"Stop"}"#,
    );
    assert_eq!(ls(&st)[0][4], "idle");
}

#[test]
fn shell_exit_removes_session() {
    let st = state();
    mine(&st, &["new", "x"]);
    let mut c = client(&st);
    c.attach("x");
    c.input("exit\n");
    wait_until(|| ls(&st).is_empty());
}

/// 인자를 trace 파일에 적고 곧바로 끝나는 가짜 `claude`를 `dir`에 만들고 trace 경로를 돌려준다.
fn fake_claude(dir: &Path) -> PathBuf {
    let trace = dir.join("trace");
    let claude = dir.join("claude");
    fs::write(
        &claude,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\n", trace.display()),
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    trace
}

#[test]
fn ended_claude_session_resumes_on_attach() {
    // PATH 앞에 둔 가짜 `claude`는 인자를 trace 파일에 적고 곧바로 끝난다.
    let bin = tmp();
    let trace = fake_claude(bin.path());
    let st = state_with_path(bin.path());
    mine(
        &st,
        &["new", "c", "--kind", "claude", "--account", "claude-a"],
    );
    mine_stdin(
        &st,
        &["_event", &session_dir(&st, "c")],
        r#"{"hook_event_name":"Stop","session_id":"s1"}"#,
    );
    wait_until(|| ls(&st)[0][4] == "ended");
    let row = &ls(&st)[0];
    assert_eq!((row[0].as_str(), row[3].as_str()), ("✓", "claude-a"));
    let mut c = client(&st);
    c.attach("c");
    wait_until(|| {
        fs::read_to_string(&trace)
            .unwrap_or_default()
            .contains("--resume s1")
    });
}

#[test]
fn badly_answering_holder_keeps_session_on_restart() {
    // 소켓은 받지만 다른 버전의 Hello를 보내는 보관 프로세스. 살아 있을 수 있으므로 세션을 닫거나
    // 끝남으로 돌리지 않고, 붙어도 같은 대화에 두 번째 claude를 띄우지 않아야 한다.
    let bin = tmp();
    let trace = fake_claude(bin.path());
    let st = state_with_path(bin.path());
    let paths = State {
        root: st.root.clone(),
    };
    let id = st.id("c");
    let dir = paths.session_dir(&id);
    fs::create_dir_all(&dir).unwrap();
    Meta {
        id: id.clone(),
        project: st.project.clone(),
        name: "c".into(),
        kind: Kind::Claude,
        account: Some("claude-a".into()),
        model: None,
        restore: Restore::Resume,
        created_ms: 1,
        resume_id: Some("s1".into()),
        closed: false,
    }
    .save(&dir)
    .unwrap();
    fs::create_dir_all(paths.run_dir()).unwrap();
    let sock = paths.hold_sock(&id);
    let listener = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        let mut open = Vec::new();
        for conn in listener.incoming().flatten() {
            let hello = HoldOut::Hello {
                version: 999,
                child_pid: 1,
            };
            let _ = write_msg(&mut &conn, &hello);
            open.push(conn);
        }
    });
    let rows = ls(&st);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][2], "c");
    assert_ne!(rows[0][4], "ended");
    // 붙으면 다시 붙어 보지만 실패를 알릴 뿐 새 프로세스를 띄우지 않는다.
    let mut c = client(&st);
    c.send(&ToDaemon::Attach {
        id: id.clone(),
        cols: 80,
        rows: 24,
    });
    let mut saw_error = false;
    loop {
        match c.recv().expect("연결이 끊겼다") {
            FromDaemon::Snapshot { .. } => break,
            FromDaemon::Error { .. } => saw_error = true,
            _ => {}
        }
    }
    assert!(saw_error);
    assert!(!trace.exists());
    assert!(!Meta::load(&dir).unwrap().closed);
    assert_ne!(ls(&st)[0][4], "ended");
    // 가짜 소켓을 치워 정리 단계가 거기에 붙지 않게 한다.
    fs::remove_file(&sock).unwrap();
}

#[test]
fn daemon_answers_cursor_position_query() {
    // 테스트 클라이언트는 질의에 답하지 않으므로, 답은 데몬이 보낸 것이다.
    let st = state();
    mine(&st, &["new", "q"]);
    let mut c = client(&st);
    c.attach("q");
    c.input("stty -icanon -echo; printf '\\033[6n'; read -d R p; stty icanon echo; case \"$p\" in *\\;*) echo DSR-$((1+1));; esac\n");
    c.wait_output("DSR-2");
}

#[test]
fn session_size_follows_last_input_client() {
    let st = state();
    mine(&st, &["new", "z"]);
    let mut big = client(&st);
    big.attach_sized("z", 100, 30);
    // 셸이 다 뜬 뒤에 잰다. 시작 중에 미리 친 입력과 겹치면 크기 확인이 드물게 어긋났다(보고서 참고).
    big.input("echo ready-$((1+1))\n");
    big.wait_output("ready-2");
    let mut small = client(&st);
    small.attach_sized("z", 90, 20);
    big.input("stty size\n");
    big.wait_output("30 100");
    small.input("stty size\n");
    small.wait_output("20 90");
}

#[test]
fn flood_output_keeps_ls_responsive() {
    let st = state();
    mine(&st, &["new", "f"]);
    let mut c = client(&st);
    c.attach("f");
    let pid = daemon_pid(&st);
    c.input("yes | head -c 20000000\n");
    // 데몬이 한창 화면에 먹이는 동안 잰다. 이 클라이언트는 그 뒤로 읽지 않으므로 큐가 차서 끊긴다.
    c.wait_bytes_or_drop(1_000_000);
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until {
        let t = Instant::now();
        assert_eq!(ls(&st).len(), 1);
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }
    // 같은 데몬이 답했다(죽고 다시 뜬 것이 아니다).
    assert_eq!(daemon_pid(&st), pid);
}

/// SIGHUP을 무시하고 `ready`를 찍은 뒤 오래 도는 가짜 로그인 셸을 `dir`에 만든다. `Kill`을 받은 보관 프로세스가
/// 2초 뒤 SIGKILL을 보내야 끝나므로, 그동안 끝나 가는 보관 프로세스가 남는다.
fn stubborn_shell(dir: &Path) -> PathBuf {
    let shell = dir.join("sh");
    fs::write(
        &shell,
        "#!/bin/sh\ntrap '' HUP\necho ready\nexec sleep 60\n",
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    shell
}

/// `stubborn_shell`로 `dev` 세션을 만들고, trap이 걸려 SIGHUP을 무시하게 될 때까지 기다린다.
fn new_stubborn_session(bin: &Path) -> St {
    let mut st = state();
    st.env.push(("SHELL", stubborn_shell(bin).into_os_string()));
    mine(&st, &["new", "dev"]);
    wait_until(|| client(&st).attach("dev").contains("ready"));
    st
}

/// 세션 `name`이 목록에 있고 보관 프로세스에 붙어 있다.
fn listed_alive(st: &St, name: &str) -> bool {
    let mut c = client(st);
    c.send(&ToDaemon::List);
    match c.recv() {
        Some(FromDaemon::Sessions { sessions }) => {
            sessions.iter().any(|s| s.meta.name == name && s.alive)
        }
        other => panic!("List에 Sessions가 오지 않았다: {other:?}"),
    }
}

#[test]
fn kill_then_new_with_same_name_keeps_the_new_session() {
    // 끝나 가는 보관 프로세스(SIGHUP 무시, 2초 뒤 SIGKILL)가 남은 채 같은 이름으로 다시 만들면, 데몬이 옛 보관
    // 프로세스에 붙어 새 세션이 사라지거나 옛 쪽이 새 소켓을 지우고 새 세션 폴더에 exit 파일을 썼다.
    let bin = tmp();
    let st = new_stubborn_session(bin.path());
    mine(&st, &["kill", "dev"]);
    mine(&st, &["new", "dev"]);
    assert!(listed_alive(&st, "dev"));
    client(&st).attach("dev");
    // 옛 보관 프로세스는 이미 끝났어야 하지만, 남아 있다면 SIGKILL로 끝날 때까지 기다리고도 남는 시간이다.
    std::thread::sleep(Duration::from_secs(3));
    assert!(listed_alive(&st, "dev"));
    assert!(client(&st).attach("dev").contains("ready"));
    assert!(
        st.root
            .join("run")
            .join(hold_sock_name(&st, "dev"))
            .exists()
    );
    assert!(!Path::new(&session_dir(&st, "dev")).join("exit").exists());
}

#[test]
fn new_is_refused_while_the_same_name_is_closing() {
    let bin = tmp();
    let st = new_stubborn_session(bin.path());
    let mut c = client(&st);
    let killed_at = Instant::now();
    c.send(&ToDaemon::Kill { id: st.id("dev") });
    // 보관 프로세스가 아직 끝나지 않았다. 같은 이름은 거절한다.
    let out = mine_raw(&st, &["new", "dev"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("닫는 중"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Kill의 답은 보관 프로세스가 SIGKILL로 끝난 뒤에 온다.
    loop {
        match c.recv().expect("연결이 끊겼다") {
            FromDaemon::Sessions { sessions } => {
                assert!(sessions.is_empty());
                break;
            }
            FromDaemon::Error { message } => panic!("Kill이 실패했다: {message}"),
            _ => {}
        }
    }
    assert!(killed_at.elapsed() >= Duration::from_millis(1500));
    mine(&st, &["new", "dev"]);
    assert!(listed_alive(&st, "dev"));
}

#[test]
fn sessions_do_not_inherit_the_daemon_spawners_environment() {
    // 데몬을 tmux 안이나 claude-b 세션 안에서 띄웠어도 그 환경이 세션 자식에게 새면 안 된다.
    // 가짜 로그인 셸과 가짜 `claude`는 환경을 `<dump>/<MINE_SESSION>`에 적는다.
    let bin = tmp();
    let dump = bin.path().join("env");
    fs::create_dir(&dump).unwrap();
    let write = |name: &str, tail: &str| {
        let path = bin.path().join(name);
        let script = format!(
            "#!/bin/sh\nenv > '{}'/\"$MINE_SESSION\"\n{tail}",
            dump.display()
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let shell = write("sh", "exec sleep 60\n");
    write("claude", "");
    let mut st = state_with_path(bin.path());
    st.env.extend([
        ("SHELL", shell.into_os_string()),
        ("TMUX", "/tmp/tmux-501/default,1,0".into()),
        ("TMUX_PANE", "%1".into()),
        ("CLAUDECODE", "1".into()),
        ("CLAUDE_CODE_ENTRYPOINT", "cli".into()),
        ("CLAUDE_CODE_SESSION_ID", "fake-session-id".into()),
        ("CLAUDE_CODE_MESSAGING_TOKEN", "fake-secret-token".into()),
        ("CLAUDE_PID", "4242".into()),
        ("CLAUDE_CONFIG_DIR", "/tmp/fake-claude-b".into()),
        ("MINE_SESSION", "stale".into()),
        // 사용자가 일부러 켠 설정은 세션 자식까지 넘어가야 한다.
        ("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS", "1".into()),
        ("CLAUDE_CODE_USE_BEDROCK", "1".into()),
    ]);
    mine(&st, &["new", "s"]);
    mine(
        &st,
        &["new", "a", "--kind", "claude", "--account", "claude-a"],
    );
    mine(
        &st,
        &["new", "b", "--kind", "claude", "--account", "claude-b"],
    );
    let env_of = |name: &str| -> Vec<String> {
        let path = dump.join(st.id(name));
        wait_until(|| fs::metadata(&path).is_ok_and(|m| m.len() > 0));
        fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    };
    let profile = format!(
        "CLAUDE_CONFIG_DIR={}",
        st.root.join("profiles").join("claude-b").display()
    );
    // 값에는 토큰이 들어 있을 수 있으므로 실패 메시지에는 변수 이름만 적는다.
    let names = |env: &[String]| -> Vec<String> {
        env.iter()
            .map(|kv| {
                kv.split_once('=')
                    .map_or(kv.as_str(), |(k, _)| k)
                    .to_owned()
            })
            .collect()
    };
    for name in ["s", "a", "b"] {
        let env = env_of(name);
        let leaked: Vec<String> = names(&env)
            .into_iter()
            .filter(|k| {
                [
                    "TMUX",
                    "TMUX_PANE",
                    "CLAUDECODE",
                    "CLAUDE_CODE_ENTRYPOINT",
                    "CLAUDE_CODE_SESSION_ID",
                    "CLAUDE_CODE_MESSAGING_TOKEN",
                    "CLAUDE_PID",
                    "CLAUDE_CONFIG_DIR",
                ]
                .contains(&k.as_str())
            })
            .filter(|k| !(name == "b" && k == "CLAUDE_CONFIG_DIR" && env.contains(&profile)))
            .collect();
        assert!(leaked.is_empty(), "{name}: 새어 나온 변수 {leaked:?}");
        for want in [
            "TERM=xterm-256color".to_string(),
            "COLORTERM=truecolor".to_string(),
            format!("MINE_SESSION={}", st.id(name)),
            "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1".to_string(),
            "CLAUDE_CODE_USE_BEDROCK=1".to_string(),
        ] {
            assert!(
                env.contains(&want),
                "{name}: {} 없음, 있는 변수 {:?}",
                want.split_once('=').map_or(want.as_str(), |(k, _)| k),
                names(&env)
            );
        }
    }
    assert!(env_of("b").contains(&profile));
}

#[test]
fn shell_output_shows_working_then_idle_after_quiet() {
    // shell 세션은 훅이 없으므로 출력이 있으면 작업 중, 2초 동안 없으면 대기다(설계 5.2).
    let st = state();
    mine(&st, &["new", "w"]);
    wait_shell_quiet(&st, "w");
    let mut c = client(&st);
    c.attach("w");
    c.send(&ToDaemon::List);
    let state_push = |c: &mut Client| loop {
        if let FromDaemon::Sessions { sessions } = c.recv().expect("연결이 끊겼다") {
            break sessions[0].state.clone();
        }
    };
    assert_eq!(state_push(&mut c), SessionState::Idle);
    c.input("echo hi\n");
    // 바뀔 때만 알리므로 다음 목록은 작업 중이고, 그다음 목록은 조용해진 뒤의 대기다.
    assert_eq!(state_push(&mut c), SessionState::Working);
    let working_at = Instant::now();
    let row = &ls(&st)[0];
    assert_eq!((row[0].as_str(), row[4].as_str()), ("◐", "working"));
    assert_eq!(state_push(&mut c), SessionState::Idle);
    assert!(working_at.elapsed() >= Duration::from_millis(1500));
    assert_eq!(ls(&st)[0][4], "idle");
}
