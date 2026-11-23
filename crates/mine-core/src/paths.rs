//! 상태 폴더(`~/.mine`)의 경로 배치와 폴더 생성.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

/// 상태 폴더 위치를 바꾸는 환경 변수.
const STATE_DIR_ENV: &str = "MINE_STATE_DIR";

/// 유닉스 소켓 경로 한도(macOS `sun_path`, 끝 NUL 포함). 경로 바이트 수가 이보다 작아야 한다.
pub const SOCK_PATH_MAX: usize = 104;

#[derive(Debug, Clone)]
pub struct State {
    pub root: PathBuf,
}

impl State {
    /// `MINE_STATE_DIR`가 있으면 그 경로를, 없으면 `$HOME/.mine`을 상태 폴더로 쓴다.
    pub fn from_env() -> Result<State> {
        State::resolve(std::env::var_os(STATE_DIR_ENV), std::env::var_os("HOME"))
    }

    /// 환경 값을 인자로 받아 상태 폴더를 정한다. 테스트가 프로세스 환경을 건드리지 않게 분리했다.
    fn resolve(state_dir: Option<OsString>, home: Option<OsString>) -> Result<State> {
        let root = match (state_dir, home) {
            (Some(dir), _) if !dir.is_empty() => PathBuf::from(dir),
            (_, Some(home)) if !home.is_empty() => PathBuf::from(home).join(".mine"),
            _ => bail!("상태 폴더를 정할 수 없다. HOME 또는 {STATE_DIR_ENV}를 설정한다"),
        };
        Ok(State {
            root: std::path::absolute(root)?,
        })
    }

    pub fn daemon_sock(&self) -> PathBuf {
        self.root.join("daemon.sock")
    }

    pub fn daemon_pid(&self) -> PathBuf {
        self.root.join("daemon.pid")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(id)
    }

    /// 보관 소켓을 두는 폴더.
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// 보관 소켓은 macOS 소켓 경로 한도(104바이트)를 넘지 않도록 id의 해시 앞 12자로 짓는다.
    pub fn hold_sock(&self, id: &str) -> PathBuf {
        self.run_dir()
            .join(format!("{}.sock", sha256_hex_prefix(id.as_bytes(), 12)))
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
}

/// 소켓 경로가 한도 안인지 본다. 넘으면 바인드·연결 전에 알아보기 쉬운 오류를 낸다.
pub fn check_sock_path(p: &Path) -> Result<()> {
    let len = p.as_os_str().len();
    if len >= SOCK_PATH_MAX {
        bail!(
            "소켓 경로가 너무 길다({len}바이트, {SOCK_PATH_MAX}바이트 미만이어야 한다): {}. {STATE_DIR_ENV}로 짧은 상태 폴더를 쓴다",
            p.display()
        );
    }
    Ok(())
}

/// 폴더를 (상위 포함) 만들고, 새로 만드는 폴더의 권한을 0o700으로 한다.
pub fn ensure_dir(p: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(p)
}

/// SHA-256의 16진수 표기 앞 `len`자.
pub(crate) fn sha256_hex_prefix(data: &[u8], len: usize) -> String {
    let hex: String = Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hex[..len].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("m")
            .tempdir_in("/tmp")
            .unwrap()
    }

    fn state_at(root: &str) -> State {
        State {
            root: PathBuf::from(root),
        }
    }

    #[test]
    fn env_override_wins_over_home() {
        let st = State::resolve(Some("/tmp/mx".into()), Some("/home/u".into())).unwrap();
        assert_eq!(st.root, PathBuf::from("/tmp/mx"));
    }

    #[test]
    fn default_is_dot_mine_under_home() {
        let st = State::resolve(None, Some("/home/u".into())).unwrap();
        assert_eq!(st.root, PathBuf::from("/home/u/.mine"));
        let st = State::resolve(Some("".into()), Some("/home/u".into())).unwrap();
        assert_eq!(st.root, PathBuf::from("/home/u/.mine"));
    }

    #[test]
    fn no_env_and_no_home_is_an_error() {
        assert!(State::resolve(None, None).is_err());
    }

    #[test]
    fn relative_override_becomes_absolute() {
        let st = State::resolve(Some("rel/state".into()), None).unwrap();
        assert!(st.root.is_absolute());
        assert!(st.root.ends_with("rel/state"));
    }

    #[test]
    fn layout_matches_design() {
        let st = state_at("/s");
        assert_eq!(st.daemon_sock(), PathBuf::from("/s/daemon.sock"));
        assert_eq!(st.daemon_pid(), PathBuf::from("/s/daemon.pid"));
        assert_eq!(st.sessions_dir(), PathBuf::from("/s/sessions"));
        assert_eq!(st.session_dir("a1"), PathBuf::from("/s/sessions/a1"));
        assert_eq!(st.projects_dir(), PathBuf::from("/s/projects"));
        assert_eq!(st.profiles_dir(), PathBuf::from("/s/profiles"));
        assert_eq!(st.log_dir(), PathBuf::from("/s/logs"));
    }

    #[test]
    fn hold_sock_is_short_stable_and_distinct() {
        let t = tmp();
        let st = State {
            root: t.path().join("state"),
        };
        let long_id = "x".repeat(200);
        let a = st.hold_sock(&long_id);
        assert_eq!(a, st.hold_sock(&long_id));
        assert_ne!(a, st.hold_sock("other"));
        assert_eq!(a.parent().unwrap(), st.root.join("run"));
        let name = a.file_name().unwrap().to_str().unwrap();
        let stem = name.strip_suffix(".sock").unwrap();
        assert_eq!(stem.len(), 12);
        assert!(stem.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(a.as_os_str().len() < 104, "{} bytes", a.as_os_str().len());
    }

    #[test]
    fn sha256_prefix_matches_known_digest() {
        // sha256("abc") = ba7816bf 8f01cfea 414140de 5dae2223 ...
        assert_eq!(sha256_hex_prefix(b"abc", 12), "ba7816bf8f01");
    }

    #[test]
    fn long_socket_path_is_a_clear_error() {
        assert!(check_sock_path(Path::new(&format!("/{}", "a".repeat(102)))).is_ok());
        let long = format!("/{}", "a".repeat(103));
        let err = check_sock_path(Path::new(&long)).unwrap_err().to_string();
        assert!(err.contains("104"), "{err}");
    }

    #[test]
    fn ensure_dir_creates_nested_dirs_with_0700() {
        let t = tmp();
        let leaf = t.path().join("a/b/c");
        ensure_dir(&leaf).unwrap();
        for p in [t.path().join("a"), t.path().join("a/b"), leaf.clone()] {
            let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", p.display());
        }
        ensure_dir(&leaf).unwrap(); // 이미 있어도 성공한다
    }
}
