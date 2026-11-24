//! Integration test for the daemon and headless commands. Starts the daemon and holder processes with the real binary.
//!
//! Every `mine` command runs in a temporary project directory with a `.git` folder, with a separate short `/tmp` state directory
//! (`MINE_STATE_DIR`), a temporary `HOME` (so the real `~/.claude` is never linked or read), `SHELL=/bin/sh` and a no-op `MINE_NOTIFY`.
//! The test process's own environment is left untouched.

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
use mine_core::proto::{
    Decision, FromDaemon, HoldIn, HoldOut, MIN_CLIENT_VERSION, ToDaemon, VERSION, read_msg,
    write_msg,
};
use mine_core::session::{Kind, Meta, NewSession, PendingAsk, Restore, SessionState, session_id};

/// Maximum time for socket reads and waiting on conditions. Generous so that it holds up even when many tests run together and slow things down.
const WAIT: Duration = Duration::from_secs(10);

/// The project directory and state directory one test uses. When dropped, it cleans up the daemon and holder processes.
struct St {
    root: PathBuf,
    project: PathBuf,
    /// The `HOME` every command gets: an empty directory unless a test fills it.
    home: PathBuf,
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
    let home = state.path().join("h");
    fs::create_dir(&home).unwrap();
    St {
        root: state.path().join("s"),
        project: project.path().canonicalize().unwrap(),
        home,
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
            .env("HOME", &self.home)
            .env("SHELL", "/bin/sh")
            // Never pop real notifications on the user's screen. A test that checks them sets its own fake.
            .env("MINE_NOTIFY", "/usr/bin/true");
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

/// Attaches to an already running daemon without saying Hello.
fn connect(st: &St) -> Client {
    let stream = UnixStream::connect(st.sock()).expect("couldn't connect to the daemon");
    Client {
        writer: stream.try_clone().unwrap(),
        reader: BufReader::new(stream),
        key: project_key(&st.project),
        attached: None,
    }
}

/// Attaches to an already running daemon, exchanges Hello, and takes the profile list that follows it.
fn client(st: &St) -> Client {
    let mut c = connect(st);
    c.send(&ToDaemon::Hello { version: VERSION });
    assert!(matches!(c.recv(), Some(FromDaemon::Hello { version, .. }) if version == VERSION));
    assert!(matches!(c.recv(), Some(FromDaemon::Profiles { .. })));
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
fn too_old_client_refused() {
    let st = state();
    ls(&st); // Starts the daemon.
    let mut c = connect(&st);
    // One below MIN_CLIENT_VERSION (3).
    c.send(&ToDaemon::Hello { version: 2 });
    let mut error = None;
    while let Some(msg) = c.recv() {
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
fn v4_hello_has_features_and_identity() {
    let st = state();
    let before = mine_core::hooks::now_ms();
    ls(&st); // Starts the daemon.
    let started = mine_core::hooks::now_ms();
    // So that a start time taken at connect time would show.
    std::thread::sleep(Duration::from_millis(20));
    let mut c = connect(&st);
    c.send(&ToDaemon::Hello { version: VERSION });
    let Some(FromDaemon::Hello {
        version,
        features,
        daemon,
    }) = c.recv()
    else {
        panic!("no Hello");
    };
    assert_eq!(version, VERSION);
    assert!(features.iter().any(|f| f == "profiles"), "{features:?}");
    assert!(features.iter().any(|f| f == "ask"), "{features:?}");
    let daemon = daemon.expect("no daemon identity");
    assert_eq!(Some(daemon.pid as i32), daemon_pid(&st));
    assert_eq!(daemon.version, env!("CARGO_PKG_VERSION"));
    assert!(
        (before..=started).contains(&daemon.started_ms),
        "{before} <= {} <= {started}",
        daemon.started_ms
    );
}

#[test]
fn v4_client_gets_profiles_after_hello() {
    let st = state();
    ls(&st); // Starts the daemon.
    let mut c = connect(&st);
    c.send(&ToDaemon::Hello { version: VERSION });
    assert!(matches!(c.recv(), Some(FromDaemon::Hello { .. })));
    let Some(FromDaemon::Profiles { profiles }) = c.recv() else {
        panic!("no Profiles right after Hello");
    };
    let ids: Vec<&str> = profiles.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["claude-a", "codex"]);
}

#[test]
fn v3_client_still_served() {
    let st = state();
    ls(&st); // Starts the daemon.
    let mut c = connect(&st);
    c.send(&ToDaemon::Hello { version: 3 });
    // A v3 client accepts only a Hello of its own version.
    assert!(matches!(
        c.recv(),
        Some(FromDaemon::Hello { version: 3, .. })
    ));
    // The next message, never `Profiles`, which a v3 client doesn't know.
    let next = |c: &mut Client| match c.recv().expect("the connection dropped") {
        FromDaemon::Profiles { .. } => panic!("a v3 client got Profiles"),
        FromDaemon::Error { message } => panic!("{message}"),
        msg => msg,
    };
    c.send(&ToDaemon::List);
    assert!(matches!(next(&mut c), FromDaemon::Sessions { .. }));
    c.send(&ToDaemon::New {
        spec: NewSession {
            project: st.project.clone(),
            name: "old".into(),
            kind: Kind::Shell,
            account: None,
            model: None,
        },
    });
    while !matches!(next(&mut c), FromDaemon::Sessions { sessions } if sessions.iter().any(|s| s.meta.name == "old"))
    {
    }
    let id = st.id("old");
    c.send(&ToDaemon::Attach {
        id: id.clone(),
        cols: 80,
        rows: 24,
    });
    while !matches!(next(&mut c), FromDaemon::Snapshot { id: got, .. } if got == id) {}
}

#[test]
fn unknown_message_is_ignored() {
    let st = state();
    ls(&st); // Starts the daemon.
    let mut c = connect(&st);
    // A newer client, which may send message types this daemon doesn't know. It is served at this daemon's version.
    c.send(&ToDaemon::Hello {
        version: VERSION + 1,
    });
    assert!(matches!(c.recv(), Some(FromDaemon::Hello { version, .. }) if version == VERSION));
    assert!(matches!(c.recv(), Some(FromDaemon::Profiles { .. })));
    for _ in 0..2 {
        c.writer
            .write_all(b"{\"t\":\"FromTheFuture\",\"x\":1}\n")
            .unwrap();
        c.send(&ToDaemon::List);
        assert!(matches!(c.recv(), Some(FromDaemon::Sessions { .. })));
    }
    // Logged once per client.
    let log = fs::read_to_string(st.root.join("logs").join("daemon.log")).unwrap();
    assert_eq!(
        log.matches("message type this daemon doesn't know").count(),
        1,
        "{log}"
    );
}

#[test]
fn reload_profiles_rebroadcasts() {
    let st = state();
    ls(&st); // Starts the daemon.
    let mut a = client(&st);
    let mut b = client(&st);
    let mut old = connect(&st);
    old.send(&ToDaemon::Hello { version: 3 });
    assert!(matches!(old.recv(), Some(FromDaemon::Hello { .. })));
    let config = st.root.join("config.toml");
    let mut text = fs::read_to_string(&config).unwrap_or_default();
    text.push_str("\n[profiles.team]\nkind = \"claude\"\n");
    fs::write(&config, text).unwrap();
    a.send(&ToDaemon::ReloadProfiles);
    // Every v4 client gets the new list, the one that asked included.
    for c in [&mut a, &mut b] {
        let Some(FromDaemon::Profiles { profiles }) = c.recv() else {
            panic!("no Profiles after the reload");
        };
        assert!(profiles.iter().any(|p| p.id == "team"), "{profiles:?}");
    }
    // The v3 client got nothing: its next message is the answer to List.
    old.send(&ToDaemon::List);
    assert!(matches!(old.recv(), Some(FromDaemon::Sessions { .. })));
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

/// A hook from another copy of `mine` announces another protocol version. Its poke must still update the state.
#[test]
fn hook_poke_from_another_version_updates_state() {
    let st = state();
    mine(&st, &["new", "h"]);
    // Let the shell go quiet first so that no output timer refreshes the state on its own.
    wait_shell_quiet(&st, "h");
    let mut c = client(&st);
    c.send(&ToDaemon::List);
    assert!(matches!(c.recv(), Some(FromDaemon::Sessions { .. })));
    let dir = PathBuf::from(session_dir(&st, "h"));
    let id = dir.file_name().unwrap().to_str().unwrap().to_string();
    // Too old to be served (the refused-Hello path), older but served, and newer.
    let versions = [
        (MIN_CLIENT_VERSION - 1, "Too old?"),
        (VERSION - 1, "Older?"),
        (VERSION + 1, "Newer?"),
    ];
    for (version, message) in versions {
        // Record the event without a poke, then poke the way that version's `mine _event` would.
        let payload = serde_json::json!({"hook_event_name": "Notification", "message": message});
        let ev = mine_core::hooks::parse_hook(&payload, mine_core::hooks::now_ms()).unwrap();
        mine_core::hooks::append(&dir, &ev).unwrap();
        let mut stream = UnixStream::connect(st.sock()).unwrap();
        write_msg(&mut stream, &ToDaemon::Hello { version }).unwrap();
        write_msg(&mut stream, &ToDaemon::Poke { id: id.clone() }).unwrap();
        drop(stream);
        loop {
            match c.recv().expect("the connection dropped") {
                FromDaemon::Sessions { sessions }
                    if sessions[0].waiting.as_deref() == Some(message) =>
                {
                    break;
                }
                _ => {}
            }
        }
    }
}

/// Creates in `dir` a fake `MINE_NOTIFY` that appends each call's arguments to the calls file as one line. Returns (script, calls file).
fn fake_notify(dir: &Path) -> (PathBuf, PathBuf) {
    let calls = dir.join("calls");
    let path = dir.join("notify");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n",
        calls.display()
    );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    (path, calls)
}

fn notify_calls(calls: &Path) -> Vec<String> {
    fs::read_to_string(calls)
        .map(|text| text.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// Records an approval request for the session the way the claude hook does.
fn ask_approval(st: &St, name: &str, message: &str) {
    let payload = serde_json::json!({"hook_event_name": "Notification", "message": message});
    mine_stdin(
        st,
        &["_event", &session_dir(st, name)],
        &payload.to_string(),
    );
}

#[test]
fn notify_on_approval_when_unfocused() {
    let bin = tmp();
    let (notify, calls) = fake_notify(bin.path());
    let mut st = state();
    st.env.push(("MINE_NOTIFY", notify.into_os_string()));
    mine(&st, &["new", "h"]);
    ask_approval(&st, "h", r#"Run "a\b"?"#);
    wait_until(|| !notify_calls(&calls).is_empty());
    // Quotes and backslashes in the text are escaped for AppleScript.
    let folder = st.project.file_name().unwrap().to_string_lossy();
    assert_eq!(
        notify_calls(&calls),
        [format!(
            r#"-e display notification "Run \"a\\b\"?" with title "mine" subtitle "{folder}/h""#
        )]
    );
    // Rereading the list, another request while still waiting, or a restarted daemon rereading the log is the same
    // transition, so nothing pops again.
    ls(&st);
    ask_approval(&st, "h", "Again?");
    kill_daemon(&st);
    assert_eq!(ls(&st)[0][4], "approval");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(notify_calls(&calls).len(), 1);
}

/// Sessions `a` and `b` on a daemon whose notifications land in the returned calls file (kept by the returned directory).
fn notifying() -> (St, PathBuf, tempfile::TempDir) {
    let bin = tmp();
    let (notify, calls) = fake_notify(bin.path());
    let mut st = state();
    st.env.push(("MINE_NOTIFY", notify.into_os_string()));
    mine(&st, &["new", "a"]);
    mine(&st, &["new", "b"]);
    (st, calls, bin)
}

/// Waits until the daemon handled everything `c` sent: messages on one connection are handled in order, so once `List` is answered,
/// the rest was too.
fn sync(c: &mut Client) {
    c.send(&ToDaemon::List);
    while !matches!(
        c.recv().expect("the connection dropped"),
        FromDaemon::Sessions { .. }
    ) {}
}

/// `Presence` from `c`: visible or not, last input `idle_ms` ago, viewing session `name`.
fn presence(c: &mut Client, st: &St, visible: bool, idle_ms: u64, name: &str) {
    c.send(&ToDaemon::Presence {
        visible,
        last_input_ms: mine_core::hooks::now_ms() - idle_ms,
        viewing: Some(st.id(name)),
    });
    sync(c);
}

/// Asks for approval in `a`, then in `b`, which no client views, and returns the notifications once `b`'s has popped.
fn notified_for_a_then_b(st: &St, calls: &Path) -> Vec<String> {
    ask_approval(st, "a", "Approve a?");
    assert!(
        ls(st)
            .iter()
            .any(|row| row[2] == "a" && row[4] == "approval")
    );
    ask_approval(st, "b", "Approve b?");
    wait_until(|| notify_calls(calls).iter().any(|c| c.contains("Approve b?")));
    // `a`'s, had it popped, started first; give its thread a moment anyway.
    std::thread::sleep(Duration::from_millis(200));
    notify_calls(calls)
}

#[test]
fn present_viewer_suppresses_notification() {
    let (st, calls, _bin) = notifying();
    let mut c = client(&st);
    presence(&mut c, &st, true, 0, "a");
    let got = notified_for_a_then_b(&st, &calls);
    assert_eq!(got.len(), 1, "{got:?}");
}

#[test]
fn idle_viewer_does_not_suppress_notification() {
    let (st, calls, _bin) = notifying();
    let mut c = client(&st);
    presence(&mut c, &st, true, 200_000, "a");
    notified_for_a_then_b(&st, &calls);
    wait_until(|| {
        notify_calls(&calls)
            .iter()
            .any(|c| c.contains("Approve a?"))
    });
}

#[test]
fn focus_out_client_not_visible() {
    let (st, calls, _bin) = notifying();
    let mut c = client(&st);
    presence(&mut c, &st, false, 0, "a");
    notified_for_a_then_b(&st, &calls);
    wait_until(|| {
        notify_calls(&calls)
            .iter()
            .any(|c| c.contains("Approve a?"))
    });
}

/// A v3 client never sends `Presence`: it views the session it's attached to, and `Focus` makes it visible with input now.
/// It no longer silences other sessions' requests.
#[test]
fn v3_focus_still_works() {
    let (st, calls, _bin) = notifying();
    let mut c = connect(&st);
    c.send(&ToDaemon::Hello { version: 3 });
    assert!(matches!(
        c.recv(),
        Some(FromDaemon::Hello { version: 3, .. })
    ));
    c.attach("a");
    c.send(&ToDaemon::Focus { focused: true });
    sync(&mut c);
    let got = notified_for_a_then_b(&st, &calls);
    assert_eq!(got.len(), 1, "{got:?}");
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

/// A fake `claude` and `codex` that stay up in raw mode, as the real ones do: a written `\x03` is a byte, not a SIGINT that ends them.
/// They print `READY` once the terminal is raw. Returns the session (`w`) of that kind, given a prompt so it is Working, with a
/// client attached to it.
fn working_session(kind: &str) -> (tempfile::TempDir, St, Client) {
    let bin = tmp();
    for name in ["claude", "codex"] {
        let path = bin.path().join(name);
        fs::write(
            &path,
            "#!/bin/sh\nstty raw -echo\necho READY\nexec sleep 60\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let st = state_with_path(bin.path());
    mine(&st, &["new", "w", "--kind", kind]);
    let mut c = client(&st);
    // A slow start must not let the first `\x03` reach a terminal that isn't raw yet.
    if !c.attach("w").contains("READY") {
        c.wait_output("READY");
    }
    prompt(&st);
    (bin, st, c)
}

/// The hook of a prompt to session `w`, then waits for the daemon to show it Working.
fn prompt(st: &St) {
    mine_stdin(
        st,
        &["_event", &session_dir(st, "w")],
        r#"{"hook_event_name":"UserPromptSubmit","prompt":"go"}"#,
    );
    wait_until(|| ls(st)[0][4] == "working");
}

/// Sends `data` as input and waits until the daemon has handled it. Messages on one connection are handled in order, so the
/// `Snapshot` that answers a new `Attach` can only arrive after the `Input`.
fn input_and_sync(c: &mut Client, data: &[u8]) {
    let id = c.attached.clone().expect("no attached session");
    c.send(&ToDaemon::Input {
        id,
        data: data.to_vec(),
    });
    c.attach("w");
}

#[test]
fn bare_ctrl_c_while_working_goes_idle() {
    for kind in ["claude", "codex"] {
        let (_bin, st, mut c) = working_session(kind);
        input_and_sync(&mut c, b"\x03");
        wait_until(|| ls(&st)[0][4] == "idle");
        // Any later hook event wins, and a bare Esc interrupts the next turn the same way.
        prompt(&st);
        input_and_sync(&mut c, b"\x1b");
        wait_until(|| ls(&st)[0][4] == "idle");
    }
}

#[test]
fn escape_sequence_keeps_working() {
    let (_bin, st, mut c) = working_session("claude");
    // An arrow key, and a bare byte inside a longer input.
    for data in [&b"\x1b[A"[..], b"\x03\x03", b"a\x03"] {
        input_and_sync(&mut c, data);
        assert_eq!(ls(&st)[0][4], "working", "{data:?}");
    }
}

#[test]
fn paste_with_ctrl_c_keeps_working() {
    let (_bin, st, mut c) = working_session("claude");
    input_and_sync(&mut c, b"\x1b[200~a\x03b\x1b[201~");
    assert_eq!(ls(&st)[0][4], "working");
}

#[test]
fn bare_ctrl_c_outside_working_changes_nothing() {
    let (_bin, st, mut c) = working_session("claude");
    // The attached client is present, so the finished turn counts as read.
    presence(&mut c, &st, true, 0, "w");
    mine_stdin(
        &st,
        &["_event", &session_dir(&st, "w")],
        r#"{"hook_event_name":"Stop"}"#,
    );
    wait_until(|| ls(&st)[0][4] == "idle");
    input_and_sync(&mut c, b"\x03");
    let events =
        fs::read_to_string(Path::new(&session_dir(&st, "w")).join("events.jsonl")).unwrap();
    assert!(!events.contains("Interrupt"), "{events}");
}

#[test]
fn event_cmd_fails_open() {
    let st = state();
    let payload = r#"{"hook_event_name":"Stop","last_assistant_message":"SECRET-REPLY"}"#;
    let exists = st.root.join("sessions").join("k1-rust");
    fs::create_dir_all(&exists).unwrap();
    let missing = st.root.join("sessions").join("gone");
    let check = |out: Output| {
        assert!(out.status.success(), "{:?}", out.status);
        assert!(out.stdout.is_empty(), "{:?}", out.stdout);
    };
    // A session directory that no longer exists, garbage on stdin, and no daemon (the payload is fine, the directory exists).
    check(mine_stdin(
        &st,
        &["_event", &missing.to_string_lossy()],
        payload,
    ));
    check(mine_stdin(
        &st,
        &["_event", &exists.to_string_lossy()],
        "SECRET-NOT-JSON",
    ));
    check(mine_stdin(
        &st,
        &["_event", &exists.to_string_lossy()],
        payload,
    ));
    // Arguments the hook could not parse still must not fail the CLI (exit code 2 would block it).
    check(mine_raw(&st, &["_event"]));
    check(mine_raw(&st, &["_event", "--nonsense"]));
    // One short line per failure, never the payload.
    let log = fs::read_to_string(st.root.join("logs").join("hooks.log")).unwrap();
    assert_eq!(log.lines().count(), 3, "{log}");
    assert!(!log.contains("SECRET"), "{log}");
    // And no daemon was started on the way.
    assert!(daemon_pid(&st).is_none());
}

/// Session `d`, whose turn ended unseen.
fn done_session() -> St {
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
    st
}

#[test]
fn seen_requires_present_viewer() {
    let st = done_session();
    let mut c = client(&st);
    // Visible but idle for 200 s: attaching changes nothing for a client that reports its presence.
    presence(&mut c, &st, true, 200_000, "d");
    c.attach("d");
    assert_eq!(ls(&st)[0][4], "done");
    presence(&mut c, &st, false, 0, "d");
    assert_eq!(ls(&st)[0][4], "done");
    // Becoming present marks it seen at once, and subscribers hear of it without asking.
    c.send(&ToDaemon::Presence {
        visible: true,
        last_input_ms: mine_core::hooks::now_ms(),
        viewing: Some(st.id("d")),
    });
    loop {
        if let FromDaemon::Sessions { sessions } = c.recv().expect("the connection dropped")
            && sessions[0].state == SessionState::Idle
        {
            break;
        }
    }
}

/// A focus report the TUI passes on to a session that turned mode 1004 on reaches the app, but isn't typing: a session waiting
/// for approval keeps waiting, with its text.
#[test]
fn forwarded_focus_report_keeps_approval() {
    let st = state();
    mine(&st, &["new", "a"]);
    ask_approval(&st, "a", "Approve a?");
    let mut c = client(&st);
    c.attach("a");
    c.input("\x1b[I");
    // Messages on one connection are handled in order, so the answer to this `List` comes after the `Input` was handled.
    c.send(&ToDaemon::List);
    let a = loop {
        if let FromDaemon::Sessions { sessions } = c.recv().expect("the connection dropped") {
            break sessions.into_iter().find(|s| s.meta.name == "a").unwrap();
        }
    };
    assert_eq!(
        (a.state, a.waiting.as_deref()),
        (SessionState::Approval, Some("Approve a?"))
    );
}

/// An old TUI never sends `Presence` or `Focus`: it counts as visible while attached, and attaching is input, so it keeps
/// marking sessions seen. After `Focus { focused: false }` it no longer does.
#[test]
fn v3_client_without_focus_marks_seen_while_attached() {
    let st = done_session();
    let mut c = connect(&st);
    c.send(&ToDaemon::Hello { version: 3 });
    assert!(matches!(
        c.recv(),
        Some(FromDaemon::Hello { version: 3, .. })
    ));
    c.attach("d");
    assert_eq!(ls(&st)[0][4], "idle");
    c.send(&ToDaemon::Focus { focused: false });
    sync(&mut c);
    mine_stdin(
        &st,
        &["_event", &session_dir(&st, "d")],
        r#"{"hook_event_name":"Stop"}"#,
    );
    assert_eq!(ls(&st)[0][4], "done");
}

#[test]
fn viewing_a_done_session_makes_it_idle() {
    let st = done_session();
    let mut c = client(&st);
    c.send(&ToDaemon::Presence {
        visible: true,
        last_input_ms: mine_core::hooks::now_ms(),
        viewing: Some(st.id("d")),
    });
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

/// Puts in `bin` a fake login shell `sh` (which stays up) and fake `claude` and `codex` (which exit). Each writes its environment to
/// `<bin>/env/<MINE_SESSION>`. Returns that directory.
fn env_dumpers(bin: &Path) -> PathBuf {
    let dump = bin.join("env");
    fs::create_dir(&dump).unwrap();
    for (name, tail) in [("sh", "exec sleep 60\n"), ("claude", ""), ("codex", "")] {
        let path = bin.join(name);
        let script = format!(
            "#!/bin/sh\nenv > '{}'/\"$MINE_SESSION\"\n{tail}",
            dump.display()
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dump
}

/// The environment session `name`'s child started with (see [`env_dumpers`]), one `K=V` per entry. Waits for it.
fn env_dump(st: &St, dump: &Path, name: &str) -> Vec<String> {
    let path = dump.join(st.id(name));
    wait_until(|| fs::metadata(&path).is_ok_and(|m| m.len() > 0));
    fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Variable names only. Values may hold tokens, so failure messages show only the names.
fn env_names(env: &[String]) -> Vec<&str> {
    env.iter()
        .map(|kv| kv.split_once('=').map_or(kv.as_str(), |(k, _)| k))
        .collect()
}

#[test]
fn sessions_do_not_inherit_the_daemon_spawners_environment() {
    // Even if the daemon was started inside tmux or a claude-b session, that environment must not leak into the session child.
    let bin = tmp();
    let dump = env_dumpers(bin.path());
    let mut st = state_with_path(bin.path());
    st.env.extend([
        ("SHELL", bin.path().join("sh").into_os_string()),
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
    ]);
    // The built-in claude-b profile exists once its directory does.
    fs::create_dir_all(st.root.join("profiles").join("claude-b")).unwrap();
    mine(&st, &["new", "s"]);
    mine(
        &st,
        &["new", "a", "--kind", "claude", "--account", "claude-a"],
    );
    mine(
        &st,
        &["new", "b", "--kind", "claude", "--account", "claude-b"],
    );
    let profile = format!(
        "CLAUDE_CONFIG_DIR={}",
        st.root.join("profiles").join("claude-b").display()
    );
    for name in ["s", "a", "b"] {
        let env = env_dump(&st, &dump, name);
        let leaked: Vec<&str> = env_names(&env)
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
                .contains(k)
            })
            .filter(|k| !(name == "b" && *k == "CLAUDE_CONFIG_DIR" && env.contains(&profile)))
            .collect();
        assert!(leaked.is_empty(), "{name}: leaked variables {leaked:?}");
        for want in [
            "TERM=xterm-256color".to_string(),
            "COLORTERM=truecolor".to_string(),
            format!("MINE_SESSION={}", st.id(name)),
            "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1".to_string(),
        ] {
            assert!(
                env.contains(&want),
                "{name}: {} is missing, variables present {:?}",
                want.split_once('=').map_or(want.as_str(), |(k, _)| k),
                env_names(&env)
            );
        }
    }
    assert!(env_dump(&st, &dump, "b").contains(&profile));
}

#[test]
fn bypass_vars_stripped_for_agents_only() {
    // A key in the daemon's environment would route claude or codex around the profile's login. A shell keeps everything.
    let bin = tmp();
    let dump = env_dumpers(bin.path());
    let mut st = state_with_path(bin.path());
    st.env.extend([
        ("SHELL", bin.path().join("sh").into_os_string()),
        ("ANTHROPIC_API_KEY", "sk-a".into()),
        ("CLAUDE_CODE_USE_BEDROCK", "1".into()),
        ("OPENAI_API_KEY", "sk-o".into()),
        ("FOO", "1".into()),
    ]);
    mine(&st, &["new", "s"]);
    mine(&st, &["new", "a", "--profile", "claude-a"]);
    mine(&st, &["new", "x", "--profile", "codex"]);
    let (shell, claude, codex) = (
        env_dump(&st, &dump, "s"),
        env_dump(&st, &dump, "a"),
        env_dump(&st, &dump, "x"),
    );
    let has = |env: &[String], kv: &str| env.iter().any(|e| e == kv);
    for kv in [
        "ANTHROPIC_API_KEY=sk-a",
        "CLAUDE_CODE_USE_BEDROCK=1",
        "OPENAI_API_KEY=sk-o",
        "FOO=1",
    ] {
        assert!(has(&shell, kv), "shell: {:?}", env_names(&shell));
    }
    let names = env_names(&claude);
    assert!(!names.contains(&"ANTHROPIC_API_KEY"), "claude: {names:?}");
    assert!(
        !names.contains(&"CLAUDE_CODE_USE_BEDROCK"),
        "claude: {names:?}"
    );
    assert!(has(&claude, "FOO=1"), "claude: {names:?}");
    let names = env_names(&codex);
    assert!(!names.contains(&"OPENAI_API_KEY"), "codex: {names:?}");
    assert!(has(&codex, "FOO=1"), "codex: {names:?}");
}

#[test]
fn creating_a_managed_profile_session_links_shared_items() {
    let bin = tmp();
    fake_claude(bin.path());
    let st = state_with_path(bin.path());
    fs::create_dir_all(&st.root).unwrap();
    fs::write(
        st.root.join("config.toml"),
        "[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\"\n[profiles.team]\nkind = \"claude\"\n",
    )
    .unwrap();
    let skills = st.home.join(".claude").join("skills");
    fs::create_dir_all(&skills).unwrap();
    mine(&st, &["new", "t", "--profile", "team"]);
    let link = st.root.join("profiles").join("team").join("skills");
    assert_eq!(fs::read_link(&link).unwrap(), skills);
    assert_eq!(ls(&st)[0][3], "team");
}

#[test]
fn unknown_profile_refuses_to_start() {
    // A session whose profile was removed from the config must not start on any other login.
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
        account: Some("gone".into()),
        model: None,
        restore: Restore::Resume,
        created_ms: 1,
        resume_id: Some("s1".into()),
        closed: false,
    }
    .save(&dir)
    .unwrap();
    // Starting the daemon finds it not running. Attaching is what would start it again.
    assert_eq!(ls(&st)[0][3], "gone");
    let mut c = client(&st);
    c.send(&ToDaemon::Attach {
        id: id.clone(),
        cols: 80,
        rows: 24,
    });
    let mut error = None;
    loop {
        match c.recv().expect("the connection dropped") {
            FromDaemon::Snapshot { .. } => break,
            FromDaemon::Error { message } => error = Some(message),
            _ => {}
        }
    }
    let error = error.expect("no error for the refused start");
    assert!(error.contains("Unknown profile: gone"), "{error}");
    let log = fs::read_to_string(paths.log_dir().join("daemon.log")).unwrap();
    assert!(log.contains("Unknown profile: gone"), "{log}");
    assert!(!trace.exists());
    assert!(!paths.hold_sock(&id).exists());
    c.send(&ToDaemon::List);
    let alive = loop {
        if let Some(FromDaemon::Sessions { sessions }) = c.recv() {
            break sessions.iter().find(|s| s.meta.id == id).map(|s| s.alive);
        }
    };
    assert_eq!(alive, Some(false));
    assert_eq!(ls(&st)[0][4], "ended");
}

#[test]
fn session_without_a_profile_gets_the_default_of_its_kind() {
    // The id is stored at creation, so a session keeps its login even after the default of its kind moves to another profile.
    let bin = tmp();
    env_dumpers(bin.path());
    let st = state_with_path(bin.path());
    fs::create_dir_all(&st.root).unwrap();
    let config = st.root.join("config.toml");
    // `b` is the Claude default (it has the CLI's own home); no Codex profile is marked, so the first one is.
    fs::write(
        &config,
        "[profiles.a]\nkind = \"claude\"\n[profiles.b]\nkind = \"claude\"\nhome = \"default\"\n\
         [profiles.c]\nkind = \"codex\"\n[profiles.d]\nkind = \"codex\"\n",
    )
    .unwrap();
    mine(&st, &["new", "x", "--kind", "claude"]);
    mine(&st, &["new", "y", "--kind", "codex"]);
    mine(&st, &["new", "s"]);
    let account = |name: &str| {
        let dir = st.root.join("sessions").join(st.id(name));
        Meta::load(&dir).unwrap().account
    };
    assert_eq!(account("x").as_deref(), Some("b"));
    assert_eq!(account("y").as_deref(), Some("c"));
    assert_eq!(account("s"), None);
    // An explicit profile still wins.
    mine(&st, &["new", "z", "--profile", "a"]);
    assert_eq!(account("z").as_deref(), Some("a"));
    // Moving the default doesn't move the session: it still runs on `b`.
    fs::write(
        &config,
        "[profiles.a]\nkind = \"claude\"\ndefault = true\n[profiles.b]\nkind = \"claude\"\nhome = \"default\"\n",
    )
    .unwrap();
    assert_eq!(ls(&st).iter().find(|r| r[2] == "x").unwrap()[3], "b");
}

/// Every `Profiles` message the client has already been sent, read until `List` is answered.
fn profile_ids_after_list(c: &mut Client) -> Vec<Vec<String>> {
    c.send(&ToDaemon::List);
    let mut lists = Vec::new();
    loop {
        match c.recv().expect("the connection dropped") {
            FromDaemon::Profiles { profiles } => {
                lists.push(profiles.into_iter().map(|p| p.id).collect());
            }
            FromDaemon::Sessions { .. } => return lists,
            _ => {}
        }
    }
}

#[test]
fn profile_add_and_rm_reach_a_running_daemon() {
    let bin = tmp();
    fake_claude(bin.path());
    let st = state_with_path(bin.path());
    ls(&st); // Starts the daemon.
    let mut c = client(&st);
    mine(&st, &["profile", "add", "team", "--kind", "claude"]);
    // The command doesn't return before the daemon has reloaded, so a new session can use the profile right away.
    mine(&st, &["new", "t", "--profile", "team"]);
    mine(&st, &["profile", "rm", "team"]);
    let lists = profile_ids_after_list(&mut c);
    assert_eq!(lists.len(), 2, "{lists:?}");
    assert!(lists[0].contains(&"team".to_string()), "{lists:?}");
    assert!(!lists[1].contains(&"team".to_string()), "{lists:?}");
    // Nothing else is touched: the running session keeps its profile.
    assert_eq!(ls(&st)[0][3], "team");
}

#[test]
fn profile_commands_never_start_a_daemon() {
    let st = state();
    mine(&st, &["profile", "add", "team", "--kind", "claude"]);
    mine(&st, &["profile", "rm", "team"]);
    assert!(daemon_pid(&st).is_none());
    assert!(!st.sock().exists());
}

#[tokio::test]
async fn spawned_daemon_uses_given_env() {
    // The desktop app starts the daemon with the login shell's environment instead of its own.
    // The fake login shell from that environment writes the session's environment to `dump`.
    let bin = tmp();
    let dump = bin.path().join("env");
    let shell = bin.path().join("sh");
    let script = format!("#!/bin/sh\nenv > '{}'\nexec sleep 60\n", dump.display());
    fs::write(&shell, script).unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
    let st = state();
    let path = format!("{}:/usr/bin:/bin", bin.path().display());
    let env = [
        ("PATH", path.as_str()),
        ("SHELL", shell.to_str().unwrap()),
        ("MINE_NOTIFY", "/usr/bin/true"),
    ]
    .map(|(k, v)| (k.to_string(), v.to_string()));
    let state = State {
        root: st.root.clone(),
    };
    let exe = Path::new(env!("CARGO_BIN_EXE_mine"));
    let conn = mine_core::conn::connect_or_spawn_async_with(&state, exe, Some(&env))
        .await
        .unwrap();
    drop(conn);
    mine(&st, &["new", "s"]);
    wait_until(|| fs::metadata(&dump).is_ok_and(|m| m.len() > 0));
    let lines: Vec<String> = fs::read_to_string(&dump)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    // Values may contain tokens, so the failure message gives only the variable names.
    let names: Vec<&str> = lines
        .iter()
        .map(|kv| kv.split_once('=').map_or(kv.as_str(), |(k, _)| k))
        .collect();
    assert!(lines.contains(&format!("PATH={path}")), "{names:?}");
    // Nothing else of this test process's environment came along (cargo sets this variable for it).
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    assert!(!names.contains(&"CARGO_MANIFEST_DIR"), "{names:?}");
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

/// What `_event --ask` prints for an allow, exactly.
const ALLOW: &str = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#;

/// A fake `claude` that asks for permission when told to. It reads lines from its terminal: `ask <tool>` runs the
/// `PermissionRequest` hook command from its `--settings` with a payload for that tool (a Bash-style `npm run deploy`) and puts
/// the hook's stdout in `@OUT@/<n>` once the hook returns, numbered from 1. Any other line is appended to `@OUT@/typed`.
/// `@OUT@/ready` appears once it reads.
const ASKING_CLAUDE: &str = r#"#!/bin/sh
hook=$(printf '%s\n' "$2" | sed -n 's/.*"PermissionRequest":[^]]*"command":"\([^"]*\)".*/\1/p')
n=0
: > "@OUT@/ready"
while IFS= read -r line; do
  case "$line" in
    "ask "*)
      n=$((n+1))
      printf '{"hook_event_name":"PermissionRequest","tool_name":"%s","tool_input":{"command":"npm run deploy"}}' "${line#ask }" \
        | sh -c "$hook" > "@OUT@/$n.part"
      mv "@OUT@/$n.part" "@OUT@/$n" ;;
    *) printf '%s\n' "$line" >> "@OUT@/typed" ;;
  esac
done
"#;

/// Claude session `c`, running [`ASKING_CLAUDE`].
struct Asking {
    st: St,
    out: PathBuf,
    /// Requests asked so far.
    asked: usize,
    _bin: tempfile::TempDir,
}

/// An [`Asking`] session on a daemon that also gets `env`.
fn asking(env: &[(&'static str, &str)]) -> Asking {
    let bin = tmp();
    let out = bin.path().join("out");
    fs::create_dir(&out).unwrap();
    let claude = bin.path().join("claude");
    fs::write(
        &claude,
        ASKING_CLAUDE.replace("@OUT@", &out.to_string_lossy()),
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let mut st = state_with_path(bin.path());
    st.env
        .extend(env.iter().map(|&(key, value)| (key, OsString::from(value))));
    mine(&st, &["new", "c", "--kind", "claude"]);
    // So that timings start with the fake reading, not with it starting up.
    wait_until(|| out.join("ready").exists());
    Asking {
        st,
        out,
        asked: 0,
        _bin: bin,
    }
}

impl Asking {
    /// Sends `data` to the session from a client that doesn't view it, and waits until the daemon handled it.
    fn input(&self, data: &[u8]) {
        let mut c = client(&self.st);
        c.send(&ToDaemon::Input {
            id: self.st.id("c"),
            data: data.to_vec(),
        });
        sync(&mut c);
    }

    /// Has the fake ask for `tool`. Returns when the daemon passed the line on.
    fn ask(&mut self, tool: &str) -> Instant {
        self.asked += 1;
        self.input(format!("ask {tool}\n").as_bytes());
        Instant::now()
    }

    /// The hook's stdout for the latest request, once the hook has returned.
    fn reply(&self) -> String {
        let path = self.out.join(self.asked.to_string());
        wait_until(|| path.exists());
        fs::read_to_string(path).unwrap()
    }

    fn pending(&self) -> Option<PendingAsk> {
        let mut c = client(&self.st);
        c.send(&ToDaemon::List);
        loop {
            if let FromDaemon::Sessions { sessions } = c.recv().expect("the connection dropped") {
                return sessions.into_iter().find(|s| s.meta.name == "c")?.pending;
            }
        }
    }

    fn wait_pending(&self) -> PendingAsk {
        let mut pending = None;
        wait_until(|| {
            pending = self.pending();
            pending.is_some()
        });
        pending.unwrap()
    }

    fn answer(&self, request: u64, allow: bool) {
        let mut c = client(&self.st);
        c.send(&ToDaemon::Answer {
            id: self.st.id("c"),
            request,
            allow,
        });
        sync(&mut c);
    }
}

#[test]
fn ask_held_when_nobody_views() {
    let mut a = asking(&[]);
    a.ask("Bash");
    let p = a.wait_pending();
    assert_eq!(p.summary, "Bash(npm run deploy)");
    let row = &ls(&a.st)[0];
    assert_eq!(
        (row[4].as_str(), row[5].as_str()),
        ("approval", "Allow Bash(npm run deploy)?")
    );
    a.answer(p.request, true);
    assert_eq!(a.reply(), format!("{ALLOW}\n"));
    assert_eq!(a.pending(), None);
}

#[test]
fn ask_denied_has_message() {
    let mut a = asking(&[]);
    a.ask("Bash");
    let p = a.wait_pending();
    a.answer(p.request, false);
    let out = a.reply();
    assert!(out.contains(r#""behavior":"deny""#), "{out}");
    assert!(out.contains("Denied from mine"), "{out}");
}

#[test]
fn ask_passes_when_viewed() {
    let mut a = asking(&[]);
    // Visible and viewing, but idle for 200 s: holding ignores the presence window.
    let mut viewer = client(&a.st);
    presence(&mut viewer, &a.st, true, 200_000, "c");
    let asked = a.ask("Bash");
    assert_eq!(a.reply(), "");
    assert!(
        asked.elapsed() < Duration::from_secs(1),
        "{:?}",
        asked.elapsed()
    );
}

#[test]
fn ask_released_when_viewer_arrives() {
    let mut a = asking(&[]);
    a.ask("Bash");
    a.wait_pending();
    // A client viewing the session from a hidden window releases nothing.
    let mut viewer = client(&a.st);
    presence(&mut viewer, &a.st, false, 0, "c");
    assert!(a.pending().is_some());
    presence(&mut viewer, &a.st, true, 200_000, "c");
    assert_eq!(a.reply(), "");
    assert_eq!(a.pending(), None);
    // Claude shows its own prompt next, so the session still needs approval.
    assert_eq!(ls(&a.st)[0][4], "approval");
}

#[test]
fn ask_released_on_input() {
    let mut a = asking(&[]);
    a.ask("Bash");
    a.wait_pending();
    // A focus report passed on to the session isn't typing.
    a.input(b"\x1b[I");
    assert!(a.pending().is_some());
    a.input(b"typed-after\n");
    assert_eq!(a.reply(), "");
    let typed = a.out.join("typed");
    wait_until(|| fs::read_to_string(&typed).is_ok_and(|t| t.contains("typed-after")));
}

#[test]
fn ask_times_out_to_pass() {
    let mut a = asking(&[("MINE_ASK_TIMEOUT_MS", "300")]);
    let asked = a.ask("Bash");
    assert_eq!(a.reply(), "");
    let took = asked.elapsed();
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_secs(5),
        "{took:?}"
    );
    assert_eq!(a.pending(), None);
}

#[test]
fn ask_released_when_daemon_stops() {
    let mut a = asking(&[]);
    a.ask("Bash");
    a.wait_pending();
    kill_daemon(&a.st);
    assert_eq!(a.reply(), "");
}

#[test]
fn pass_tools_never_held() {
    let mut a = asking(&[]);
    for tool in ["AskUserQuestion", "ExitPlanMode"] {
        let asked = a.ask(tool);
        assert_eq!(a.reply(), "", "{tool}");
        assert!(
            asked.elapsed() < Duration::from_secs(1),
            "{tool}: {:?}",
            asked.elapsed()
        );
    }
}

#[test]
fn second_answer_is_ignored() {
    let mut a = asking(&[]);
    a.ask("Bash");
    let first = a.wait_pending();
    // The app and the TUI both answer the same request: the first one decides.
    a.answer(first.request, true);
    a.answer(first.request, false);
    assert_eq!(a.reply(), format!("{ALLOW}\n"));
    // An answer to the old request doesn't touch a newer one.
    a.ask("Bash");
    let second = a.wait_pending();
    assert_ne!(second.request, first.request);
    a.answer(first.request, true);
    assert_eq!(a.pending(), Some(second.clone()));
    a.answer(second.request, false);
    assert!(a.reply().contains("Denied from mine"));
    let log = fs::read_to_string(a.st.root.join("logs").join("daemon.log")).unwrap();
    assert_eq!(log.matches("no longer pending").count(), 2, "{log}");
}

#[test]
fn decision_never_has_updated_permissions() {
    let mut a = asking(&[]);
    for allow in [true, false] {
        a.ask("Bash");
        let p = a.wait_pending();
        a.answer(p.request, allow);
        let out = a.reply();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert!(!out.contains("updatedPermissions"), "{out}");
    }
}

#[test]
fn ask_fails_open_without_daemon() {
    let st = state();
    let dir = st.root.join("sessions").join("k1-rust");
    fs::create_dir_all(&dir).unwrap();
    let dir = dir.to_string_lossy();
    let payload = r#"{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"SECRET-CMD"}}"#;
    // No daemon, garbage, and a payload without a tool.
    for input in [payload, "SECRET-NOT-JSON", "{}"] {
        let out = mine_stdin(&st, &["_event", &dir, "--ask"], input);
        assert!(out.stdout.is_empty(), "{:?}", out.stdout);
    }
    assert!(daemon_pid(&st).is_none());
    let log = fs::read_to_string(st.root.join("logs").join("hooks.log")).unwrap();
    assert_eq!(log.lines().count(), 3, "{log}");
    assert!(!log.contains("SECRET"), "{log}");
}

/// Opens a hook's connection that asks about session `c`, the way `_event --ask` speaking `version` would.
fn ask_raw(a: &Asking, version: u32) -> Client {
    let mut hook = connect(&a.st);
    // Set now: macOS refuses it once the daemon has replied and closed the connection.
    hook.reader.get_ref().set_read_timeout(Some(WAIT)).unwrap();
    hook.send(&ToDaemon::Hello { version });
    hook.send(&ToDaemon::Ask {
        id: a.st.id("c"),
        tool: "Bash".into(),
        summary: "Bash(x)".into(),
    });
    hook
}

/// Like `Poke`, a hook's `Ask` is served whatever protocol version it speaks, even one too old for anything else.
#[test]
fn ask_accepted_at_any_version() {
    let a = asking(&[]);
    for version in [MIN_CLIENT_VERSION - 1, VERSION, VERSION + 1] {
        let mut hook = ask_raw(&a, version);
        let p = a.wait_pending();
        a.answer(p.request, true);
        loop {
            if let FromDaemon::AskReply { decision } =
                read_msg(&mut hook.reader).unwrap().expect("no AskReply")
            {
                break assert_eq!(decision, Decision::Allow);
            }
        }
    }
}

#[test]
fn ask_dropped_connection_clears_pending() {
    let a = asking(&[]);
    let hook = ask_raw(&a, VERSION);
    a.wait_pending();
    drop(hook);
    wait_until(|| a.pending().is_none());
}
