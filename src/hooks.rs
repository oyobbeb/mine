//! CLI 훅 이벤트를 세션 폴더의 `events.jsonl`에 쌓고, 그 기록으로 세션 상태를 계산한다.

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

/// 데몬에 알리는 연결과 쓰기에 줄 수 있는 시간. 훅이 순정 CLI를 오래 붙잡으면 안 된다.
const POKE_TIMEOUT: Duration = Duration::from_millis(200);

/// 세션 상태를 바꾸는 사건 종류.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum EventKind {
    /// 사용자가 프롬프트를 보냈다.
    Prompt,
    /// 에이전트의 턴이 끝났다.
    Stop,
    /// 에이전트가 승인을 기다린다.
    Approval,
    /// 사용자가 세션에 입력했다. 데몬이 승인 대기 중일 때만 기록한다.
    Input,
}

/// `events.jsonl`의 한 줄.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Event {
    /// 유닉스 시각(밀리초).
    pub ms: u64,
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_id: Option<String>,
}

/// 지금 시각(유닉스 밀리초).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// 훅 페이로드를 이벤트로 바꾼다. 하위 에이전트 훅이나 모르는 훅은 `None`이다.
pub fn parse_hook(payload: &Value, now_ms: u64) -> Option<Event> {
    // 하위 에이전트의 턴은 세션 상태에 섞지 않는다.
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

/// `dir/events.jsonl`에 이벤트 한 줄을 붙인다. 여러 훅 프로세스가 겹쳐도 섞이지 않게 잠근다.
pub fn append(dir: &Path, ev: &Event) -> Result<()> {
    let path = dir.join(EVENTS_FILE);
    let mut line = serde_json::to_vec(ev)?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("열 수 없다: {}", path.display()))?;
    file.lock()
        .with_context(|| format!("잠글 수 없다: {}", path.display()))?;
    // 잠금은 파일을 닫을 때 풀린다.
    file.write_all(&line)
        .and_then(|()| file.flush())
        .with_context(|| format!("쓸 수 없다: {}", path.display()))
}

/// `dir/events.jsonl`의 이벤트를 쓴 순서대로 읽는다. 파일이 없으면 빈 목록이다.
/// 해석할 수 없는 줄(쓰다 끊긴 마지막 줄 등)은 건너뛴다.
pub fn read_all(dir: &Path) -> Result<Vec<Event>> {
    let path = dir.join(EVENTS_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("읽을 수 없다: {}", path.display())),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// 이벤트 기록에서 (상태, 대기 문구, resume id)를 계산한다.
///
/// 마지막 의미 있는 이벤트가 상태를 정한다. `viewed_ms`는 사용자가 세션을 마지막으로 연 시각이고,
/// 마지막 `Stop`과 같거나 그 뒤면 끝남(안 읽음)이 아니라 대기다.
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
            // 승인 대기 중에 사용자가 입력하면 다시 작업 중이다. 다른 상태에서는 아무 일도 없다.
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

/// 데몬에 `Hello`와 `Poke`를 보낸다. 연결이나 쓰기가 `POKE_TIMEOUT`을 넘기면 포기한다.
fn poke(state: &State, id: &str) -> Result<()> {
    let sock = state.daemon_sock();
    // std에는 연결 제한 시간이 없어서 별도 스레드에서 연결하고 기다린다.
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

/// 훅이 부르는 `mine _event <세션 폴더>`의 본체.
///
/// 표준 입력의 훅 페이로드를 읽어 `events.jsonl`에 기록하고 데몬에 알린다.
/// 훅이 순정 CLI를 막거나 실패시키면 안 되므로, 읽을 수 없거나 해석할 수 없는 페이로드와
/// 무시할 훅은 오류 없이 끝내고, 데몬 쪽 오류도 모두 삼킨다. 기록 자체가 실패하면 `Err`다.
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
        // 열어 본 뒤 새 Stop이 오면 다시 안 읽은 끝남이다.
        assert_eq!(
            fold(&[ev(2, Stop), ev(4, Prompt), ev(5, Stop)], Some(3)).0,
            SessionState::Done
        );
        // 열어 본 시각이 Stop보다 이르면 아직 안 읽었다.
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
        let events = [ev(1, Prompt), ev_text(2, Approval, "npm run deploy 승인?")];
        let (state, waiting, _) = fold(&events, None);
        assert_eq!(state, SessionState::Approval);
        assert_eq!(waiting.as_deref(), Some("npm run deploy 승인?"));
    }

    #[test]
    fn waiting_clears_when_state_moves_on() {
        let approval = ev_text(2, Approval, "승인?");
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
        // 값이 비어 있거나 null이면 하위 에이전트가 아니다.
        assert!(parse_hook(&json!({"hook_event_name":"Stop","agent_id":null}), 0).is_some());
        assert!(parse_hook(&json!({"hook_event_name":"Stop","agent_id":""}), 0).is_some());
    }

    #[test]
    fn hook_names_map_to_event_kinds() {
        let p = parse_hook(
            &json!({"hook_event_name":"UserPromptSubmit","prompt":"안녕","session_id":"s1"}),
            7,
        )
        .unwrap();
        assert_eq!(
            p,
            Event {
                ms: 7,
                kind: Prompt,
                text: Some("안녕".into()),
                resume_id: Some("s1".into()),
            }
        );
        let s = parse_hook(
            &json!({"hook_event_name":"Stop","last_assistant_message":"끝"}),
            8,
        )
        .unwrap();
        assert_eq!((s.kind, s.text.as_deref()), (Stop, Some("끝")));
        let n = parse_hook(
            &json!({"hook_event_name":"Notification","message":"승인?"}),
            9,
        )
        .unwrap();
        assert_eq!((n.kind, n.text.as_deref()), (Approval, Some("승인?")));
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
            ..ev_text(1, Prompt, "줄바꿈\n포함 \"따옴표\"")
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
            Err(std::io::Error::other("깨진 입력"))
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
        // 받지 않는 데몬: 연결은 되지만 아무것도 읽지 않는다.
        let _listener = UnixListener::bind(state.daemon_sock()).unwrap();
        let payload = br#"{"hook_event_name":"Stop"}"#;
        let started = std::time::Instant::now();
        run_event_cmd(&state, &dir, &payload[..]).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
