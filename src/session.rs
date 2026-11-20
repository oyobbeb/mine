//! 세션 데이터 타입과 `meta.json` 저장.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// 세션이 띄우는 프로그램 종류.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Kind {
    Claude,
    Codex,
    Shell,
}

/// 목록에 보이는 세션 상태.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum SessionState {
    Approval,
    Working,
    Done,
    Idle,
    Ended,
}

/// 데몬이 다시 떠도 세션을 되살리는 방법. 기본값은 kind별로 정한다.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Restore {
    Resume,
    Rerun(String),
    None,
}

/// 새 세션을 만들 때 클라이언트가 보내는 명세.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NewSession {
    pub project: PathBuf,
    pub name: String,
    pub kind: Kind,
    pub account: Option<String>,
    pub model: Option<String>,
}

/// 세션 폴더의 `meta.json`에 저장되는 값.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Meta {
    pub id: String,
    pub project: PathBuf,
    pub name: String,
    pub kind: Kind,
    pub account: Option<String>,
    pub model: Option<String>,
    pub restore: Restore,
    pub created_ms: u64,
    pub resume_id: Option<String>,
    pub closed: bool,
}

/// 데몬이 클라이언트에 알려 주는 세션 한 건.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub meta: Meta,
    pub state: SessionState,
    pub waiting: Option<String>,
    pub alive: bool,
}

const META_FILE: &str = "meta.json";

/// 세션 id는 `{프로젝트 키}-{이름}`이다.
pub fn session_id(project_key: &str, name: &str) -> String {
    format!("{project_key}-{name}")
}

/// 이름은 `^[A-Za-z0-9][A-Za-z0-9_~-]{0,63}$`이어야 한다.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = (1..=64).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '~' | '-'));
    if !ok {
        bail!(
            "세션 이름이 올바르지 않다: {name:?} (영문·숫자로 시작하고 영문·숫자·_~-만 쓰며 64자 이하)"
        );
    }
    Ok(())
}

impl Meta {
    /// `dir/meta.json`을 읽는다.
    pub fn load(dir: &Path) -> Result<Meta> {
        let path = dir.join(META_FILE);
        let bytes = fs::read(&path).with_context(|| format!("읽을 수 없다: {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("meta.json 형식이 올바르지 않다: {}", path.display()))
    }

    /// `dir/meta.json`에 원자적으로 쓴다. 같은 폴더의 임시 파일에 쓴 뒤 이름을 바꾼다.
    pub fn save(&self, dir: &Path) -> Result<()> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = dir.join(META_FILE);
        let tmp = dir.join(format!(
            ".{META_FILE}.{}.{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let write = || -> Result<()> {
            let mut f = File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(self)?)?;
            f.sync_all()?;
            fs::rename(&tmp, &path)?;
            Ok(())
        };
        write()
            .with_context(|| format!("저장할 수 없다: {}", path.display()))
            .inspect_err(|_| {
                let _ = fs::remove_file(&tmp);
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("m")
            .tempdir_in("/tmp")
            .unwrap()
    }

    fn sample_meta() -> Meta {
        Meta {
            id: session_id("0123456789abcdef", "rust-core"),
            project: PathBuf::from("/work/mine"),
            name: "rust-core".into(),
            kind: Kind::Claude,
            account: Some("work".into()),
            model: None,
            restore: Restore::Rerun("cargo run".into()),
            created_ms: 1_763_600_000_000,
            resume_id: Some("s1".into()),
            closed: false,
        }
    }

    #[test]
    fn names() {
        for ok in ["rust-core", "rust-core~b", "a", "A_1", &"a".repeat(64)] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "_x", "~x", "a/b", "a b", "한글", &"a".repeat(65)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn id_joins_key_and_name() {
        assert_eq!(session_id("k1", "n1"), "k1-n1");
    }

    #[test]
    fn meta_roundtrip() {
        let t = tmp();
        let m = sample_meta();
        m.save(t.path()).unwrap();
        assert_eq!(Meta::load(t.path()).unwrap(), m);
        // 덮어써도 성공하고, 임시 파일이 남지 않는다.
        let m2 = Meta {
            closed: true,
            restore: Restore::Resume,
            ..m
        };
        m2.save(t.path()).unwrap();
        assert_eq!(Meta::load(t.path()).unwrap(), m2);
        let files: Vec<_> = std::fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, vec![std::ffi::OsString::from("meta.json")]);
    }

    #[test]
    fn load_missing_is_an_error() {
        let t = tmp();
        assert!(Meta::load(t.path()).is_err());
    }
}
