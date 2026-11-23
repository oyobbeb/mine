//! 보관 프로세스·데몬·클라이언트가 주고받는 JSON 줄 프로토콜.
//!
//! 메시지 하나가 JSON 객체 한 줄(`\n`으로 끝남)이다. 열거형은 `t` 필드로 종류를 나타내고,
//! 바이트 필드는 base64 문자열로 실어 줄바꿈이 섞이지 않게 한다.

use std::io::{self, BufRead, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::session::{NewSession, SessionInfo};

/// 클라이언트와 데몬 사이 프로토콜 버전. 2에서 `Colors`가 생겼다(1인 데몬은 모르는 메시지에 연결을 끊는다).
pub const VERSION: u32 = 2;
/// 데몬과 보관 프로세스 사이 프로토콜 버전.
pub const HOLD_VERSION: u32 = 1;

/// 바이트 필드를 base64 문자열로 직렬화하는 `serde(with)` 도우미.
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

/// 보관 프로세스가 데몬에 보내는 메시지.
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

/// 데몬이 보관 프로세스에 보내는 메시지.
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

/// 클라이언트가 데몬에 보내는 메시지.
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
    /// 바깥 터미널의 기본 전경색과 배경색(`#rrggbb`). 데몬이 모든 화면의 색 질의 답에 쓴다.
    Colors {
        fg: String,
        bg: String,
    },
}

/// 데몬이 클라이언트에 보내는 메시지.
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

/// 메시지를 JSON 한 줄(끝 `\n` 포함)로 만든다. 한 번에 쓰도록 버퍼 하나로 돌려준다.
fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(msg).map_err(io::Error::other)?;
    line.push(b'\n');
    Ok(line)
}

/// 읽은 한 줄을 메시지로 해석한다. 형식이 틀리면 `InvalidData`다.
fn decode<T: DeserializeOwned>(line: &str) -> io::Result<T> {
    serde_json::from_str(line.trim_end_matches(['\n', '\r']))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// 메시지를 JSON 한 줄로 쓰고 비운다.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    w.write_all(&encode(msg)?)?;
    w.flush()
}

/// 한 줄을 읽어 메시지로 해석한다. 입력 끝이면 `None`이다.
pub fn read_msg<R: BufRead, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    decode(&line).map(Some)
}

/// [`write_msg`]의 비동기 판.
pub async fn send<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    w.write_all(&encode(msg)?).await?;
    w.flush().await
}

/// [`read_msg`]의 비동기 판.
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
