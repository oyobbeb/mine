//! Integration test for the daemon and headless commands. Starts the daemon and holder processes with the real binary.
//!
//! Every `mine` command runs in a temporary project directory with a `.git` folder, with a separate short `/tmp` state directory
//! (`MINE_STATE_DIR`) and `SHELL=/bin/sh`. The test process's own environment is left untouched.

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

/// Maximum time for socket reads and waiting on conditions. Generous so that it holds up even when many tests run together and slow things down.
const WAIT: Duration = Duration::from_secs(10);

/// The project directory and state directory one test uses. When dropped, it cleans up the daemon and holder processes.
struct St {
    root: PathBuf,
    project: PathBuf,
    /// If set, it's prepended to `PATH` (a fake `claude`, etc.).
    path_front: Option<PathBuf>,
    /// Environment variables to add to the `mine` command (and the daemon it starts). Overrides the earlier defaults (`SHELL`, etc.).
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
        // Kill the daemon first so it doesn't reattach to the holder processes.
        if let Some(pid) = daemon_pid(self) {
            // SAFETY: kill takes only integer arguments. The pid was read from the pid file locked by the daemon this test started.
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

/// Sends `Kill` to the holder process, then reads and discards the replay until it ends (up to `WAIT`).
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

/// pid of the live daemon. The daemon keeps the pid file locked, so if it can be locked, there is no daemon.
fn daemon_pid(st: &St) -> Option<i32> {
    let path = st.root.join("daemon.pid");
    let file = File::open(&path).ok()?;
    match file.try_lock() {
        Err(TryLockError::WouldBlock) => fs::read_to_string(&path).ok()?.trim().parse().ok(),
        _ => None,
    }
}

/// Runs `mine` and checks that it succeeded.
fn mine(st: &St, args: &[&str]) -> Output {
    let out = mine_raw(st, args);
    assert!(
        out.status.success(),
        "mine {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn mine_raw(st: &St, args: &[&str]) -> Output {
    st.command(args)
        .stdin(Stdio::null())
        .output()
        .expect("couldn't run mine")
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
    assert!(out.status.success(), "mine {args:?} failed");
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

/// SIGKILLs the daemon via the pid file and waits until the socket no longer accepts connections.
fn kill_daemon(st: &St) {
    let pid = daemon_pid(st).expect("the daemon isn't running");
    // SAFETY: kill takes only integer arguments. The pid was read from the pid file that this test's daemon keeps locked.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    wait_until(|| UnixStream::connect(st.sock()).is_err());
}

/// Waits up to `WAIT` for the condition to become true.
fn wait_until(mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(
            Instant::now() < deadline,
            "the condition didn't become true within {WAIT:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A small socket client that imitates `Conn`.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    key: String,
    attached: Option<String>,
}

/// Attaches to an already running daemon and exchanges Hello.
fn client(st: &St) -> Client {
    let stream = UnixStream::connect(st.sock()).expect("couldn't connect to the daemon");
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

    /// The next message. `None` if the connection has ended. Panics if none arrives within `WAIT`.
    fn recv(&mut self) -> Option<FromDaemon> {
        self.reader.get_ref().set_read_timeout(Some(WAIT)).unwrap();
        read_msg(&mut self.reader).expect("couldn't read a message in time")
    }

    /// Attaches at size 80x24 and returns the snapshot as text.
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
            match self.recv().expect("the connection dropped") {
                FromDaemon::Snapshot { id: got, data } if got == id => {
                    return String::from_utf8_lossy(&data).into_owned();
                }
                FromDaemon::Error { message } => panic!("couldn't attach: {message}"),
                _ => {}
            }
        }
    }

    fn input(&mut self, text: &str) {
        let id = self.attached.clone().expect("no attached session");
        self.send(&ToDaemon::Input {
            id,
            data: text.as_bytes().to_vec(),
        });
    }

    /// Reads up to `WAIT` until `needle` appears in the attached session's output.
    fn wait_output(&mut self, needle: &str) {
        self.read_output(
            |seen| String::from_utf8_lossy(seen).contains(needle),
            needle,
            false,
        );
    }

    /// Reads up to `WAIT` until the attached session's output exceeds `n` bytes or the daemon drops this lagging client.
    fn wait_bytes_or_drop(&mut self, n: usize) {
        self.read_output(|seen| seen.len() > n, &format!("{n} bytes"), true);
    }

    fn read_output(&mut self, mut done: impl FnMut(&[u8]) -> bool, what: &str, eof_ok: bool) {
        let id = self.attached.clone().expect("no attached session");
        let deadline = Instant::now() + WAIT;
        let mut seen = Vec::new();
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| {
                    panic!(
                        "{what} not in the output: {:?}",
                        String::from_utf8_lossy(&seen)
                    )
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
                Ok(None) => panic!("the connection dropped"),
                Err(e) => panic!(
                    "{what} not in the output ({e}): {:?}",
                    String::from_utf8_lossy(&seen)
                ),
            }
        }
    }
}

/// Waits until the shell session has fully started and its output has quieted down to Idle. The shell can't tell us when it will print its startup prompt
/// (if it comes late, the session goes back to Working), so it first looks at the result of one command and then waits. The attached connection is dropped before waiting.
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
    // If the executable is missing, the holder process ends without an exit file.
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
    // The daemon restarts automatically and the session is as it was.
    assert_eq!(ls(&st)[0][4], "idle");
    let mut c2 = client(&st);
    assert!(c2.attach("keep").contains("marker-1"));
    // Input goes to the reattached session as is.
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
    // The holder process ends too.
    wait_until(|| !st.root.join("run").join(hold_sock_name(&st, "k")).exists());
    // A closed session doesn't come back even when the daemon restarts.
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
    ls(&st); // Starts the daemon.
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
    // Tells the user the real command that restarts the daemon.
    let want = format!("kill \"$(cat {})\"", st.root.join("daemon.pid").display());
    let error = error.expect("no Error arrived");
    assert!(error.contains(&want), "{error}");
}

#[test]
fn hook_poke_updates_state() {
    let st = state();
    mine(&st, &["new", "h"]);
    mine_stdin(
        &st,
        &["_event", &session_dir(&st, "h")],
        r#"{"hook_event_name":"Notification","message":"Approve npm run deploy?"}"#,
    );
    let r = &ls(&st)[0];
    assert_eq!(r[0], "◆");
    assert_eq!(r[4], "approval");
    assert_eq!(r[5], "Approve npm run deploy?");
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
        r#"{"hook_event_name":"Notification","message":"Approve?"}"#,
    );
    // The changed list arrives even without sending List again.
    loop {
        match c.recv().expect("the connection dropped") {
            FromDaemon::Sessions { sessions } if sessions[0].state == SessionState::Approval => {
                assert_eq!(sessions[0].waiting.as_deref(), Some("Approve?"));
                break;
            }
            _ => {}
        }
    }
}

#[test]
fn client_colours_answer_inner_colour_queries() {
    // Both a session created earlier (a screen that already exists) and one created after the colors were received (a new screen) answer with the received colors.
    let st = state();
    mine(&st, &["new", "old"]);
    let mut c = client(&st);
    c.send(&ToDaemon::Colors {
        fg: "#0a0b0c".into(),
        bg: "#010203".into(),
    });
    // Messages on the same connection are handled in order, so once the List answer arrives, Colors has been handled too.
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
        r#"{"hook_event_name":"Notification","message":"Approve?"}"#,
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
    // Looks at the hook state only after the Working state decided by shell output has gone back to Idle.
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
    // A turn that ended while it was being viewed counts as already read.
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

/// Creates in `dir` a fake `claude` that writes its arguments to a trace file and exits right away, and returns the trace path.
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
    // The fake `claude` placed at the front of PATH writes its arguments to the trace file and exits right away.
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
    // A holder process that accepts the socket but sends a Hello of a different version. It may be alive, so the session must not be closed or
    // marked Done, and even when attached, a second claude must not be started in the same conversation.
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
    // On attach it tries to reattach, but only reports the failure and doesn't start a new process.
    let mut c = client(&st);
    c.send(&ToDaemon::Attach {
        id: id.clone(),
        cols: 80,
        rows: 24,
    });
    let mut saw_error = false;
    loop {
        match c.recv().expect("the connection dropped") {
            FromDaemon::Snapshot { .. } => break,
            FromDaemon::Error { .. } => saw_error = true,
            _ => {}
        }
    }
    assert!(saw_error);
    assert!(!trace.exists());
    assert!(!Meta::load(&dir).unwrap().closed);
    assert_ne!(ls(&st)[0][4], "ended");
    // Clear away the fake socket so the cleanup step doesn't attach to it.
    fs::remove_file(&sock).unwrap();
}

#[test]
fn daemon_answers_cursor_position_query() {
    // The test client doesn't answer queries, so the answer is one the daemon sent.
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
    // Measured after the shell has fully started. When it overlapped with input typed ahead during startup, the size check occasionally came out wrong (see the report).
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
    // Measured while the daemon is busy feeding the screen. This client stops reading after that, so its queue fills and it gets dropped.
    c.wait_bytes_or_drop(1_000_000);
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until {
        let t = Instant::now();
        assert_eq!(ls(&st).len(), 1);
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }
    // The same daemon answered (it didn't die and restart).
    assert_eq!(daemon_pid(&st), pid);
}

/// Creates in `dir` a fake login shell that ignores SIGHUP, prints `ready`, and then runs for a long time. The holder process that received `Kill`
/// must end with SIGKILL after 2 seconds, so a holder process that is about to end remains in the meantime.
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

/// Creates a `dev` session with `stubborn_shell` and waits until the trap is set and SIGHUP is ignored.
fn new_stubborn_session(bin: &Path) -> St {
    let mut st = state();
    st.env.push(("SHELL", stubborn_shell(bin).into_os_string()));
    mine(&st, &["new", "dev"]);
    wait_until(|| client(&st).attach("dev").contains("ready"));
    st
}

/// The session `name` is in the list and attached to a holder process.
fn listed_alive(st: &St, name: &str) -> bool {
    let mut c = client(st);
    c.send(&ToDaemon::List);
    match c.recv() {
        Some(FromDaemon::Sessions { sessions }) => {
            sessions.iter().any(|s| s.meta.name == name && s.alive)
        }
        other => panic!("List didn't get Sessions: {other:?}"),
    }
}

#[test]
fn kill_then_new_with_same_name_keeps_the_new_session() {
    // If it's recreated with the same name while a holder process that is about to end (SIGHUP ignored, SIGKILL after 2 seconds) remains, the daemon attached to the old holder
    // process so the new session disappeared, or the old one removed the new socket and wrote an exit file into the new session directory.
    let bin = tmp();
    let st = new_stubborn_session(bin.path());
    mine(&st, &["kill", "dev"]);
    mine(&st, &["new", "dev"]);
    assert!(listed_alive(&st, "dev"));
    client(&st).attach("dev");
    // The old holder process should already have ended, but if it remains, this leaves time enough to wait until SIGKILL ends it.
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
    // The holder process hasn't ended yet. The same name is refused.
    let out = mine_raw(&st, &["new", "dev"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("still closing"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The reply to Kill comes after the holder process ends with SIGKILL.
    loop {
        match c.recv().expect("the connection dropped") {
            FromDaemon::Sessions { sessions } => {
                assert!(sessions.is_empty());
                break;
            }
            FromDaemon::Error { message } => panic!("Kill failed: {message}"),
            _ => {}
        }
    }
    assert!(killed_at.elapsed() >= Duration::from_millis(1500));
    mine(&st, &["new", "dev"]);
    assert!(listed_alive(&st, "dev"));
}

#[test]
fn sessions_do_not_inherit_the_daemon_spawners_environment() {
    // Even if the daemon was started inside tmux or a claude-b session, that environment must not leak into the session child.
    // The fake login shell and the fake `claude` write their environment to `<dump>/<MINE_SESSION>`.
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
        // Settings the user turned on deliberately must pass through to the session child.
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
    // Values may contain tokens, so the failure message gives only the variable name.
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
        assert!(leaked.is_empty(), "{name}: leaked variables {leaked:?}");
        for want in [
            "TERM=xterm-256color".to_string(),
            "COLORTERM=truecolor".to_string(),
            format!("MINE_SESSION={}", st.id(name)),
            "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1".to_string(),
            "CLAUDE_CODE_USE_BEDROCK=1".to_string(),
        ] {
            assert!(
                env.contains(&want),
                "{name}: {} is missing, variables present {:?}",
                want.split_once('=').map_or(want.as_str(), |(k, _)| k),
                names(&env)
            );
        }
    }
    assert!(env_of("b").contains(&profile));
}

#[test]
fn shell_output_shows_working_then_idle_after_quiet() {
    // A shell session has no hooks, so with output it's Working, and with none for 2 seconds it's Idle (design 5.2).
    let st = state();
    mine(&st, &["new", "w"]);
    wait_shell_quiet(&st, "w");
    let mut c = client(&st);
    c.attach("w");
    c.send(&ToDaemon::List);
    let state_push = |c: &mut Client| loop {
        if let FromDaemon::Sessions { sessions } = c.recv().expect("the connection dropped") {
            break sessions[0].state.clone();
        }
    };
    assert_eq!(state_push(&mut c), SessionState::Idle);
    c.input("echo hi\n");
    // It announces only on change, so the next list is Working and the one after is Idle after it went quiet.
    assert_eq!(state_push(&mut c), SessionState::Working);
    let working_at = Instant::now();
    let row = &ls(&st)[0];
    assert_eq!((row[0].as_str(), row[4].as_str()), ("◐", "working"));
    assert_eq!(state_push(&mut c), SessionState::Idle);
    assert!(working_at.elapsed() >= Duration::from_millis(1500));
    assert_eq!(ls(&st)[0][4], "idle");
}
