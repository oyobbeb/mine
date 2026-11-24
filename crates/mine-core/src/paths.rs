//! Layout of the state directory (`~/.mine`) and directory creation.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

/// Environment variable that overrides the state directory location.
const STATE_DIR_ENV: &str = "MINE_STATE_DIR";

/// Unix socket path limit (macOS `sun_path`, including the trailing NUL). The path byte count must be below this.
pub const SOCK_PATH_MAX: usize = 104;

#[derive(Debug, Clone)]
pub struct State {
    pub root: PathBuf,
}

impl State {
    /// Uses the `MINE_STATE_DIR` path as the state directory if set, otherwise `$HOME/.mine`.
    pub fn from_env() -> Result<State> {
        State::resolve(std::env::var_os(STATE_DIR_ENV), std::env::var_os("HOME"))
    }

    /// Picks the state directory from environment values passed as arguments. Split out so tests don't touch the process environment.
    fn resolve(state_dir: Option<OsString>, home: Option<OsString>) -> Result<State> {
        let root = match (state_dir, home) {
            (Some(dir), _) if !dir.is_empty() => PathBuf::from(dir),
            (_, Some(home)) if !home.is_empty() => PathBuf::from(home).join(".mine"),
            _ => bail!("Couldn't determine the state directory. Set HOME or {STATE_DIR_ENV}"),
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

    /// Directory that holds the holder sockets.
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// The holder socket is named with the first 12 chars of the id's hash so it stays under the macOS socket path limit (104 bytes).
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

/// Checks that the socket path is within the limit. If it is over, fails with an easy-to-recognize error before bind/connect.
pub fn check_sock_path(p: &Path) -> Result<()> {
    let len = p.as_os_str().len();
    if len >= SOCK_PATH_MAX {
        bail!(
            "The socket path is too long ({len} bytes; it must be under {SOCK_PATH_MAX} bytes): {}. Use a shorter state directory via {STATE_DIR_ENV}",
            p.display()
        );
    }
    Ok(())
}

/// Creates the directory (including parents) and sets permissions of newly created directories to 0o700.
pub fn ensure_dir(p: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(p)
}

/// The first `len` chars of the hex form of the SHA-256.
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
        ensure_dir(&leaf).unwrap(); // succeeds even if it already exists
    }
}
