//! Accumulates CLI hook events in the session directory's `events.jsonl` and computes the session state from that record.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths::State;
use crate::proto::{ToDaemon, VERSION, write_msg};
use crate::session::SessionState;

const EVENTS_FILE: &str = "events.jsonl";

/// How long the connection to the daemon and the write may take. A hook must not hold up the stock CLI for long.
const POKE_TIMEOUT: Duration = Duration::from_millis(200);

/// Kinds of events that change the session state.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum EventKind {
    /// The user sent a prompt.
    Prompt,
    /// The agent's turn ended.
    Stop,
    /// The agent is waiting for approval.
    Approval,
    /// The user typed into the session. Recorded only while the daemon sees it waiting for approval.
    Input,
}

/// One line of `events.jsonl`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Event {
    /// Unix time (milliseconds).
    pub ms: u64,
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_id: Option<String>,
}

/// The current time (Unix milliseconds).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Turns a hook payload into an event. A subagent hook or an unknown hook gives `None`.
pub fn parse_hook(payload: &Value, now_ms: u64) -> Option<Event> {
    // A subagent's turn is not mixed into the session state.
    match payload.get("agent_id") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if s.is_empty() => {}
        Some(_) => return None,
    }
    let field = |name: &str| -> Option<String> {
        payload.get(name).and_then(Value::as_str).map(str::to_owned)
    };
    let (kind, text) = match payload.get("hook_event_name")?.as_str()? {
        "UserPromptSubmit" => (EventKind::Prompt, field("prompt")),
        "Stop" => (EventKind::Stop, field("last_assistant_message")),
        "Notification" => (EventKind::Approval, field("message")),
        _ => return None,
    };
    Some(Event {
        ms: now_ms,
        kind,
        text,
        resume_id: field("session_id").filter(|s| !s.is_empty()),
    })
}

/// Appends one event line to `dir/events.jsonl`. Locks so that overlapping hook processes don't interleave.
pub fn append(dir: &Path, ev: &Event) -> Result<()> {
    let path = dir.join(EVENTS_FILE);
    let mut line = serde_json::to_vec(ev)?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Couldn't open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("Couldn't lock {}", path.display()))?;
    // The lock is released when the file is closed.
    file.write_all(&line)
        .and_then(|()| file.flush())
        .with_context(|| format!("Couldn't write {}", path.display()))
}

/// Reads the events in `dir/events.jsonl` in the order they were written. An empty list if the file is missing.
/// Lines that can't be parsed (such as a last line cut off mid-write) are skipped.
pub fn read_all(dir: &Path) -> Result<Vec<Event>> {
    let path = dir.join(EVENTS_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("Couldn't read {}", path.display())),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// Computes (state, waiting text, resume id) from the event record.
///
/// The last meaningful event decides the state. `viewed_ms` is when the user last opened the session,
/// and if it is at or after the last `Stop`, the state is Idle rather than Done (unread).
pub fn fold(
    events: &[Event],
    viewed_ms: Option<u64>,
) -> (SessionState, Option<String>, Option<String>) {
    let mut state = SessionState::Idle;
    let mut waiting = None;
    let mut resume_id = None;
    let mut stopped_ms = None;
    for ev in events {
        if let Some(id) = &ev.resume_id {
            resume_id = Some(id.clone());
        }
        match ev.kind {
            EventKind::Prompt => {
                state = SessionState::Working;
                waiting = None;
            }
            EventKind::Approval => {
                state = SessionState::Approval;
                waiting = ev.text.clone();
            }
            // If the user types while waiting for approval, it is Working again. In other states nothing happens.
            EventKind::Input => {
                if state == SessionState::Approval {
                    state = SessionState::Working;
                    waiting = None;
                }
            }
            EventKind::Stop => {
                state = SessionState::Done;
                waiting = None;
                stopped_ms = Some(ev.ms);
            }
        }
    }
    if let (SessionState::Done, Some(stop), Some(viewed)) = (&state, stopped_ms, viewed_ms)
        && viewed >= stop
    {
        state = SessionState::Idle;
    }
    (state, waiting, resume_id)
}

/// Sends `Hello` and `Poke` to the daemon. Gives up if the connection or the write exceeds `POKE_TIMEOUT`.
fn poke(state: &State, id: &str) -> Result<()> {
    let sock = state.daemon_sock();
    // std has no connect timeout, so connect and wait on a separate thread.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(UnixStream::connect(sock));
    });
    let mut stream = rx.recv_timeout(POKE_TIMEOUT)??;
    stream.set_write_timeout(Some(POKE_TIMEOUT))?;
    write_msg(&mut stream, &ToDaemon::Hello { version: VERSION })?;
    write_msg(&mut stream, &ToDaemon::Poke { id: id.to_string() })?;
    Ok(())
}

/// The body of `mine _event <session dir>`, which hooks run.
///
/// Reads the hook payload from stdin, records it in `events.jsonl`, and tells the daemon.
/// A hook must not block or fail the stock CLI, so a payload that can't be read or parsed and
/// a hook to ignore finish without an error, and all errors on the daemon side are swallowed. If the recording itself fails, it is `Err`.
pub fn run_event_cmd(state: &State, dir: &Path, mut stdin: impl Read) -> Result<()> {
    let mut input = Vec::new();
    if stdin.read_to_end(&mut input).is_err() {
        return Ok(());
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&input) else {
        return Ok(());
    };
    let Some(ev) = parse_hook(&payload, now_ms()) else {
        return Ok(());
    };
    append(dir, &ev)?;
    if let Some(id) = dir.file_name().and_then(|n| n.to_str()) {
        let _ = poke(state, id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    use serde_json::json;

    use super::EventKind::*;
    use super::*;
    use crate::proto::read_msg;

    fn tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("m")
            .tempdir_in("/tmp")
            .unwrap()
    }

    fn ev(ms: u64, kind: EventKind) -> Event {
        Event {
            ms,
            kind,
            text: None,
            resume_id: None,
        }
    }

    fn ev_text(ms: u64, kind: EventKind, text: &str) -> Event {
        Event {
            text: Some(text.into()),
            ..ev(ms, kind)
        }
    }

    #[test]
    fn prompt_then_stop_is_done() {
        assert_eq!(
            fold(&[ev(1, Prompt), ev(2, Stop)], None).0,
            SessionState::Done
        );
    }

    #[test]
    fn prompt_is_working() {
        assert_eq!(fold(&[ev(1, Prompt)], None).0, SessionState::Working);
    }

    #[test]
    fn stop_seen_is_idle() {
        assert_eq!(fold(&[ev(2, Stop)], Some(3)).0, SessionState::Idle);
        // After it was opened, a new Stop makes it an unread Done again.
        assert_eq!(
            fold(&[ev(2, Stop), ev(4, Prompt), ev(5, Stop)], Some(3)).0,
            SessionState::Done
        );
        // If the time it was opened is earlier than the Stop, it is still unread.
        assert_eq!(fold(&[ev(2, Stop)], Some(1)).0, SessionState::Done);
    }

    #[test]
    fn viewing_does_not_hide_working_or_approval() {
        assert_eq!(
            fold(&[ev(1, Stop), ev(2, Prompt)], Some(9)).0,
            SessionState::Working
        );
        assert_eq!(fold(&[ev(1, Approval)], Some(9)).0, SessionState::Approval);
    }

    #[test]
    fn approval_carries_message() {
        let events = [
            ev(1, Prompt),
            ev_text(2, Approval, "Approve npm run deploy?"),
        ];
        let (state, waiting, _) = fold(&events, None);
        assert_eq!(state, SessionState::Approval);
        assert_eq!(waiting.as_deref(), Some("Approve npm run deploy?"));
    }

    #[test]
    fn waiting_clears_when_state_moves_on() {
        let approval = ev_text(2, Approval, "Approve?");
        assert_eq!(fold(&[approval.clone(), ev(3, Stop)], None).1, None);
        assert_eq!(fold(&[approval.clone(), ev(3, Prompt)], None).1, None);
        assert_eq!(fold(&[approval, ev(3, Input)], None).1, None);
    }

    #[test]
    fn input_after_approval_is_working() {
        assert_eq!(
            fold(&[ev(1, Approval), ev(2, Input)], None).0,
            SessionState::Working
        );
    }

    #[test]
    fn input_after_done_stays_done() {
        assert_eq!(
            fold(&[ev(1, Stop), ev(2, Input)], None).0,
            SessionState::Done
        );
        assert_eq!(
            fold(&[ev(1, Stop), ev(2, Input)], Some(5)).0,
            SessionState::Idle
        );
    }

    #[test]
    fn input_while_idle_or_working_changes_nothing() {
        assert_eq!(fold(&[ev(1, Input)], None).0, SessionState::Idle);
        assert_eq!(
            fold(&[ev(1, Prompt), ev(2, Input)], None).0,
            SessionState::Working
        );
    }

    #[test]
    fn no_events_is_idle() {
        assert_eq!(fold(&[], None), (SessionState::Idle, None, None));
    }

    #[test]
    fn subagent_events_ignored() {
        assert!(parse_hook(&json!({"hook_event_name":"Stop","agent_id":"a"}), 0).is_none());
        // If the value is empty or null, it is not a subagent.
        assert!(parse_hook(&json!({"hook_event_name":"Stop","agent_id":null}), 0).is_some());
        assert!(parse_hook(&json!({"hook_event_name":"Stop","agent_id":""}), 0).is_some());
    }

    #[test]
    fn hook_names_map_to_event_kinds() {
        let p = parse_hook(
            &json!({"hook_event_name":"UserPromptSubmit","prompt":"hello","session_id":"s1"}),
            7,
        )
        .unwrap();
        assert_eq!(
            p,
            Event {
                ms: 7,
                kind: Prompt,
                text: Some("hello".into()),
                resume_id: Some("s1".into()),
            }
        );
        let s = parse_hook(
            &json!({"hook_event_name":"Stop","last_assistant_message":"done"}),
            8,
        )
        .unwrap();
        assert_eq!((s.kind, s.text.as_deref()), (Stop, Some("done")));
        let n = parse_hook(
            &json!({"hook_event_name":"Notification","message":"Approve?"}),
            9,
        )
        .unwrap();
        assert_eq!((n.kind, n.text.as_deref()), (Approval, Some("Approve?")));
        assert!(parse_hook(&json!({"hook_event_name":"PreToolUse"}), 0).is_none());
        assert!(parse_hook(&json!({"prompt":"x"}), 0).is_none());
        assert!(parse_hook(&json!("Stop"), 0).is_none());
    }

    #[test]
    fn resume_id_taken_from_session_id() {
        let e = parse_hook(&json!({"hook_event_name":"Stop","session_id":"s9"}), 1).unwrap();
        assert_eq!(fold(&[e], None).2.as_deref(), Some("s9"));
    }

    #[test]
    fn resume_id_follows_the_latest_event_that_has_one() {
        let with = |ms, id: &str| Event {
            resume_id: Some(id.into()),
            ..ev(ms, Prompt)
        };
        let events = [with(1, "a"), ev(2, Stop), with(3, "b"), ev(4, Stop)];
        assert_eq!(fold(&events, None).2.as_deref(), Some("b"));
    }

    #[test]
    fn append_and_read_all_roundtrip() {
        let t = tmp();
        assert_eq!(read_all(t.path()).unwrap(), vec![]);
        let a = Event {
            resume_id: Some("s1".into()),
            ..ev_text(1, Prompt, "line break\nincluded \"quotes\"")
        };
        let b = ev(2, Stop);
        append(t.path(), &a).unwrap();
        append(t.path(), &b).unwrap();
        assert_eq!(read_all(t.path()).unwrap(), vec![a, b]);
        let raw = std::fs::read_to_string(t.path().join("events.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 2);
        assert!(raw.ends_with('\n'));
    }

    #[test]
    fn read_all_skips_torn_lines() {
        let t = tmp();
        append(t.path(), &ev(1, Prompt)).unwrap();
        let path = t.path().join("events.jsonl");
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"ms\":2,\"kind\":\"St").unwrap();
        assert_eq!(read_all(t.path()).unwrap(), vec![ev(1, Prompt)]);
    }

    #[test]
    fn concurrent_appends_stay_whole_lines() {
        let t = tmp();
        let dir = t.path().to_path_buf();
        let big = "가".repeat(20_000);
        let handles: Vec<_> = (0..8u64)
            .map(|i| {
                let dir = dir.clone();
                let big = big.clone();
                std::thread::spawn(move || {
                    for j in 0..10 {
                        append(&dir, &ev_text(i * 100 + j, Stop, &big)).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let events = read_all(&dir).unwrap();
        assert_eq!(events.len(), 80);
        assert!(events.iter().all(|e| e.text.as_deref() == Some(&big)));
    }

    fn state_in(t: &tempfile::TempDir) -> (State, PathBuf) {
        let state = State {
            root: t.path().to_path_buf(),
        };
        let dir = state.session_dir("k1-rust");
        std::fs::create_dir_all(&dir).unwrap();
        (state, dir)
    }

    #[test]
    fn run_event_cmd_records_and_pokes_daemon() {
        let t = tmp();
        let (state, dir) = state_in(&t);
        let listener = UnixListener::bind(state.daemon_sock()).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut r = BufReader::new(conn);
            let hello: ToDaemon = read_msg(&mut r).unwrap().unwrap();
            let poke: ToDaemon = read_msg(&mut r).unwrap().unwrap();
            (hello, poke)
        });
        let payload = br#"{"hook_event_name":"UserPromptSubmit","prompt":"hi","session_id":"s1"}"#;
        run_event_cmd(&state, &dir, &payload[..]).unwrap();
        let (hello, poke) = server.join().unwrap();
        assert_eq!(hello, ToDaemon::Hello { version: VERSION });
        assert_eq!(
            poke,
            ToDaemon::Poke {
                id: "k1-rust".into()
            }
        );
        let events = read_all(&dir).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Prompt);
        assert_eq!(events[0].text.as_deref(), Some("hi"));
        assert_eq!(events[0].resume_id.as_deref(), Some("s1"));
        assert!(events[0].ms > 1_700_000_000_000);
    }

    #[test]
    fn run_event_cmd_without_daemon_still_records() {
        let t = tmp();
        let (state, dir) = state_in(&t);
        let payload = br#"{"hook_event_name":"Stop"}"#;
        run_event_cmd(&state, &dir, &payload[..]).unwrap();
        assert_eq!(fold(&read_all(&dir).unwrap(), None).0, SessionState::Done);
    }

    #[test]
    fn run_event_cmd_ignores_unusable_payloads() {
        let t = tmp();
        let (state, dir) = state_in(&t);
        let payloads: [&[u8]; 5] = [
            b"",
            b"not json",
            br#"{"hook_event_name":"Stop","agent_id":"a"}"#,
            br#"{"hook_event_name":"PreToolUse"}"#,
            &[0xff, 0xfe],
        ];
        for payload in payloads {
            run_event_cmd(&state, &dir, payload).unwrap();
        }
        assert!(!dir.join("events.jsonl").exists());
    }

    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("broken input"))
        }
    }

    #[test]
    fn run_event_cmd_survives_unreadable_stdin() {
        let t = tmp();
        let (state, dir) = state_in(&t);
        run_event_cmd(&state, &dir, Broken).unwrap();
        assert!(!dir.join("events.jsonl").exists());
    }

    #[test]
    fn run_event_cmd_does_not_hang_on_a_stuck_daemon() {
        let t = tmp();
        let (state, dir) = state_in(&t);
        // A daemon that doesn't receive: the connection succeeds but nothing is read.
        let _listener = UnixListener::bind(state.daemon_sock()).unwrap();
        let payload = br#"{"hook_event_name":"Stop"}"#;
        let started = std::time::Instant::now();
        run_event_cmd(&state, &dir, &payload[..]).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
