//! JSON line protocol spoken by the holder process, daemon and clients.
//!
//! One message is one line holding a JSON object (ending in `\n`). Enums name their variant in the `t` field,
//! and byte fields are carried as base64 strings so no newlines get mixed in.

use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::profiles::ProfileInfo;
use crate::session::{NewSession, SessionInfo};

/// Protocol version between client and daemon. `Colors` was added in 2, `Focus` in 3, and in 4 the hello reply's `features` and
/// `daemon`, `Profiles`, `ReloadProfiles`, `Presence`, `Ask`, `Answer` and `AskReply`. Up to 3 a side dropped the connection on a
/// message type it didn't know; from 4 it ignores it ([`ToDaemon::Unknown`], [`FromDaemon::Unknown`]). New behavior is gated on
/// a feature name in the hello reply.
pub const VERSION: u32 = 4;
/// The oldest client version the daemon serves. The app bundle and the CLI are updated separately, so a daemon serves older clients
/// and sends each only what its version knows.
pub const MIN_CLIENT_VERSION: u32 = 3;
/// Protocol version between daemon and holder process.
pub const HOLD_VERSION: u32 = 1;

/// A client is present when it's visible and its last input was within this long (ms). Only a present client viewing a session
/// marks it seen and keeps its approval from notifying.
pub const PRESENT_WINDOW_MS: u64 = 180_000;
/// A visible client sends `Presence` at least this often.
pub const PRESENCE_HEARTBEAT: Duration = Duration::from_secs(30);
/// Input is reported at most this often. An input in between rides on the next send.
const PRESENCE_INPUT_EVERY: Duration = Duration::from_secs(5);

/// Decides when a client sends [`ToDaemon::Presence`]. Both clients feed it their input and state and send what [`Self::due`] returns.
#[derive(Debug, Default)]
pub struct PresenceReport {
    /// What was sent last (visible, viewing) and when. `None` on a new connection.
    sent: Option<(bool, Option<String>, Instant)>,
    /// Input arrived after the last send.
    input: bool,
    last_input_ms: u64,
}

impl PresenceReport {
    /// The user typed or clicked at `now_ms` (Unix ms).
    pub fn input(&mut self, now_ms: u64) {
        self.last_input_ms = now_ms;
        self.input = true;
    }

    /// A new connection: the next [`Self::due`] sends.
    pub fn reset(&mut self) {
        self.sent = None;
    }

    /// The `Presence` to send at `now`, if any: the first one, on a change of `visible` or `viewing`, for input at most every
    /// 5 s, and every heartbeat while visible.
    pub fn due(&mut self, now: Instant, visible: bool, viewing: Option<&str>) -> Option<ToDaemon> {
        let due = match &self.sent {
            None => true,
            Some((was, saw, at)) => {
                *was != visible
                    || saw.as_deref() != viewing
                    || (self.input && now >= *at + PRESENCE_INPUT_EVERY)
                    || (visible && now >= *at + PRESENCE_HEARTBEAT)
            }
        };
        if !due {
            return None;
        }
        let viewing = viewing.map(str::to_string);
        self.sent = Some((visible, viewing.clone(), now));
        self.input = false;
        Some(ToDaemon::Presence {
            visible,
            last_input_ms: self.last_input_ms,
            viewing,
        })
    }

    /// When to call [`Self::due`] again for the heartbeat. `None` while not visible.
    pub fn deadline(&self) -> Option<Instant> {
        match self.sent {
            Some((true, _, at)) => Some(at + PRESENCE_HEARTBEAT),
            _ => None,
        }
    }
}

/// `serde(with)` helper that serializes byte fields as base64 strings.
mod b64 {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&STANDARD.encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text).map_err(de::Error::custom)
    }
}

/// Messages the holder process sends to the daemon.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t")]
pub enum HoldOut {
    Hello {
        version: u32,
        child_pid: u32,
    },
    Output {
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    Exited {
        code: Option<i32>,
    },
}

/// Messages the daemon sends to the holder process.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t")]
pub enum HoldIn {
    Input {
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Kill,
}

/// Messages a client sends to the daemon.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t")]
pub enum ToDaemon {
    Hello {
        version: u32,
    },
    List,
    New {
        spec: NewSession,
    },
    Attach {
        id: String,
        cols: u16,
        rows: u16,
    },
    Input {
        id: String,
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    Resize {
        id: String,
        cols: u16,
        rows: u16,
    },
    Kill {
        id: String,
    },
    Poke {
        id: String,
    },
    /// The outer terminal's default foreground and background colors (`#rrggbb`). The daemon uses them to answer color queries on every screen.
    Colors {
        fg: String,
        bg: String,
    },
    /// Whether this client's window is the one the user is looking at. Kept for v3 clients: the daemon reads it as `Presence` with
    /// `visible = focused` and input now, keeping the `viewing` it had.
    Focus {
        focused: bool,
    },
    /// What the user can see of this client. `visible`: its window is on screen and in front (a terminal's focus report, the app's
    /// active window). `last_input_ms`: when the user last typed or clicked in it (Unix ms). `viewing`: the session whose terminal
    /// it shows. Sent only to a daemon that announced `presence`, on change, on input and every [`PRESENCE_HEARTBEAT`] while visible.
    Presence {
        visible: bool,
        last_input_ms: u64,
        viewing: Option<String>,
    },
    /// Reread the profiles in `config.toml` and send the new list to every client.
    ReloadProfiles,
    /// From the `PermissionRequest` hook (`_event --ask`), on a connection of its own: Claude asks to run `tool` in session `id`. The
    /// daemon holds it until a client answers, then replies [`FromDaemon::AskReply`] on that connection. Accepted whatever the
    /// hook's protocol version, like `Poke`, and only from a daemon that announced `ask`.
    Ask {
        id: String,
        tool: String,
        summary: String,
    },
    /// The user's answer to held request `request` of session `id` ([`crate::session::PendingAsk`]). One that is no longer pending
    /// is ignored.
    Answer {
        id: String,
        request: u64,
        allow: bool,
    },
    /// A message type from a newer client. Ignored.
    #[serde(other)]
    Unknown,
}

/// Who the daemon is, in its hello reply.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DaemonInfo {
    pub pid: u32,
    /// The `mine` package version the daemon was built from.
    pub version: String,
    /// When the daemon started (Unix milliseconds).
    pub started_ms: u64,
}

/// Messages the daemon sends to a client.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t")]
pub enum FromDaemon {
    /// `version` is the one both sides speak: the client's, or the daemon's if the client is newer. A daemon older than 4 sends
    /// neither `features` nor `daemon`.
    Hello {
        version: u32,
        #[serde(default)]
        features: Vec<String>,
        #[serde(default)]
        daemon: Option<DaemonInfo>,
    },
    /// Every profile, in launcher order. Sent to a v4+ client right after `Hello` and again after a reload.
    Profiles {
        profiles: Vec<ProfileInfo>,
    },
    Sessions {
        sessions: Vec<SessionInfo>,
    },
    Snapshot {
        id: String,
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    Output {
        id: String,
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    Error {
        message: String,
    },
    /// The answer to an `Ask`, on its connection only.
    AskReply {
        decision: Decision,
    },
    /// A message type from a newer daemon. Ignored.
    #[serde(other)]
    Unknown,
}

/// What a held permission request comes to. `Pass` leaves it to Claude's own prompt.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
    Pass,
}

/// Turns a message into one line of JSON (including the trailing `\n`). Returned as a single buffer so it is written in one go.
fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(msg).map_err(io::Error::other)?;
    line.push(b'\n');
    Ok(line)
}

/// Parses a line that was read into a message. A malformed line is `InvalidData`.
fn decode<T: DeserializeOwned>(line: &str) -> io::Result<T> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes a message as one line of JSON and flushes.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    w.write_all(&encode(msg)?)?;
    w.flush()
}

/// Reads one line and parses it into a message. `None` at end of input.
pub fn read_msg<R: BufRead, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    decode(&line).map(Some)
}

/// Async version of [`write_msg`].
pub async fn send<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    w.write_all(&encode(msg)?).await?;
    w.flush().await
}

/// Async version of [`read_msg`].
pub async fn recv<R: AsyncBufRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
) -> io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    decode(&line).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Kind, NewSession};
    use std::path::PathBuf;

    #[test]
    fn bytes_survive_roundtrip() {
        let m = HoldOut::Output {
            data: vec![0, 1, 0x1b, 0xff, b'\n'],
        };
        let mut buf = Vec::new();
        write_msg(&mut buf, &m).unwrap();
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(read_msg::<_, HoldOut>(&mut &buf[..]).unwrap(), Some(m));
    }

    #[test]
    fn eof_is_none() {
        assert_eq!(read_msg::<_, HoldIn>(&mut &b""[..]).unwrap(), None);
    }

    #[test]
    fn wire_format_is_tagged_with_base64() {
        let mut buf = Vec::new();
        write_msg(
            &mut buf,
            &HoldIn::Input {
                data: b"hi".to_vec(),
            },
        )
        .unwrap();
        assert_eq!(buf, b"{\"t\":\"Input\",\"data\":\"aGk=\"}\n");
        let mut buf = Vec::new();
        write_msg(&mut buf, &HoldIn::Kill).unwrap();
        assert_eq!(buf, b"{\"t\":\"Kill\"}\n");
    }

    #[test]
    fn invalid_base64_is_invalid_data() {
        let err =
            read_msg::<_, HoldIn>(&mut &b"{\"t\":\"Input\",\"data\":\"!!\"}\n"[..]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn consecutive_messages_are_read_in_order() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &ToDaemon::Hello { version: VERSION }).unwrap();
        write_msg(&mut buf, &ToDaemon::List).unwrap();
        let mut r = &buf[..];
        assert_eq!(
            read_msg::<_, ToDaemon>(&mut r).unwrap(),
            Some(ToDaemon::Hello { version: VERSION })
        );
        assert_eq!(
            read_msg::<_, ToDaemon>(&mut r).unwrap(),
            Some(ToDaemon::List)
        );
        assert_eq!(read_msg::<_, ToDaemon>(&mut r).unwrap(), None);
    }

    #[test]
    fn malformed_line_is_invalid_data() {
        let err = read_msg::<_, HoldIn>(&mut &b"not json\n"[..]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn nested_types_roundtrip() {
        let m = ToDaemon::New {
            spec: NewSession {
                project: PathBuf::from("/work/mine"),
                name: "a".into(),
                kind: Kind::Codex,
                account: None,
                model: Some("m".into()),
            },
        };
        let mut buf = Vec::new();
        write_msg(&mut buf, &m).unwrap();
        assert_eq!(read_msg::<_, ToDaemon>(&mut &buf[..]).unwrap(), Some(m));
    }

    #[test]
    fn unknown_message_types_parse_as_unknown() {
        let line = b"{\"t\":\"FromTheFuture\",\"x\":1}\n";
        assert_eq!(
            read_msg::<_, ToDaemon>(&mut &line[..]).unwrap(),
            Some(ToDaemon::Unknown)
        );
        assert_eq!(
            read_msg::<_, FromDaemon>(&mut &line[..]).unwrap(),
            Some(FromDaemon::Unknown)
        );
    }

    #[test]
    fn hello_from_a_v3_daemon_parses() {
        let line = b"{\"t\":\"Hello\",\"version\":3}\n";
        assert_eq!(
            read_msg::<_, FromDaemon>(&mut &line[..]).unwrap(),
            Some(FromDaemon::Hello {
                version: 3,
                features: Vec::new(),
                daemon: None
            })
        );
    }

    #[test]
    fn presence_report_sends_on_change_throttles_input_and_beats_while_visible() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let presence = |visible, last_input_ms, viewing: Option<&str>| {
            Some(ToDaemon::Presence {
                visible,
                last_input_ms,
                viewing: viewing.map(str::to_string),
            })
        };
        let mut r = PresenceReport::default();
        r.input(1_000);
        assert_eq!(r.due(t0, true, None), presence(true, 1_000, None));
        assert_eq!(r.due(t0, true, None), None);
        // A change goes out at once.
        assert_eq!(
            r.due(t0 + s(1), true, Some("a")),
            presence(true, 1_000, Some("a"))
        );
        // Input within 5 s of the last send waits for the next due call past it, and carries the latest input.
        r.input(2_000);
        assert_eq!(r.due(t0 + s(2), true, Some("a")), None);
        r.input(3_000);
        assert_eq!(
            r.due(t0 + s(6), true, Some("a")),
            presence(true, 3_000, Some("a"))
        );
        // Nothing new: the heartbeat is 30 s after the last send.
        assert_eq!(r.deadline(), Some(t0 + s(36)));
        assert_eq!(r.due(t0 + s(35), true, Some("a")), None);
        assert_eq!(
            r.due(t0 + s(36), true, Some("a")),
            presence(true, 3_000, Some("a"))
        );
        // No heartbeat while not visible, and a new connection sends again.
        assert_eq!(
            r.due(t0 + s(37), false, Some("a")),
            presence(false, 3_000, Some("a"))
        );
        assert_eq!(r.deadline(), None);
        assert_eq!(r.due(t0 + s(100), false, Some("a")), None);
        r.reset();
        assert_eq!(
            r.due(t0 + s(101), false, Some("a")),
            presence(false, 3_000, Some("a"))
        );
    }

    #[tokio::test]
    async fn async_send_recv_roundtrip() {
        let m = FromDaemon::Snapshot {
            id: "k-a".into(),
            data: vec![0, 0x1b, b'[', 0xff, b'\n'],
        };
        let mut buf = Vec::new();
        send(&mut buf, &m).await.unwrap();
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 1);
        let mut r = tokio::io::BufReader::new(&buf[..]);
        assert_eq!(recv::<_, FromDaemon>(&mut r).await.unwrap(), Some(m));
        assert_eq!(recv::<_, FromDaemon>(&mut r).await.unwrap(), None);
    }
}
