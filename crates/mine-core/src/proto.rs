//! JSON line protocol spoken by the holder process, daemon and clients.
//!
//! One message is one line holding a JSON object (ending in `\n`). Enums name their variant in the `t` field,
//! and byte fields are carried as base64 strings so no newlines get mixed in.

use std::io::{self, BufRead, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::session::{NewSession, SessionInfo};

/// Protocol version between client and daemon. `Colors` was added in 2 (a version 1 daemon drops the connection on a message it doesn't know).
pub const VERSION: u32 = 2;
/// Protocol version between daemon and holder process.
pub const HOLD_VERSION: u32 = 1;

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
}

/// Messages the daemon sends to a client.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t")]
pub enum FromDaemon {
    Hello {
        version: u32,
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
