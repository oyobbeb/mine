//! 세션 데이터 타입과 `meta.json` 저장.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::paths::State;

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
            "Invalid session name: {name:?} (start with a letter or digit, then use only letters, digits, _, ~ and -, up to 64 characters)"
        );
    }
    Ok(())
}

impl Meta {
    /// `dir/meta.json`을 읽는다.
    pub fn load(dir: &Path) -> Result<Meta> {
        let path = dir.join(META_FILE);
        let bytes = fs::read(&path).with_context(|| format!("Couldn't read {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid meta.json format: {}", path.display()))
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
            .with_context(|| format!("Couldn't save {}", path.display()))
            .inspect_err(|_| {
                let _ = fs::remove_file(&tmp);
            })
    }
}

/// 보관 프로세스가 세션 자식으로 띄울 명령.
#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// 데몬을 띄운 셸에서 물려받았지만 세션 자식에게 넘기면 안 되는 환경 변수인가. 보관 프로세스를 띄울 때 지우고,
/// 필요한 값은 [`launch`]가 다시 넣는다(`MINE_SESSION`, claude-b의 `CLAUDE_CONFIG_DIR`).
///
/// tmux 안에서 띄운 데몬이면 `TMUX`가 tmux를 아는 도구를 바깥 tmux로 보내고, claude 세션 안에서 띄운 데몬이면
/// `CLAUDECODE`가 중첩 실행을 막거나 `CLAUDE_CONFIG_DIR`가 claude-a 세션을 claude-b 계정으로 돌린다.
/// 지우는 것은 실행 중인 Claude Code가 자식에게 설정하는 변수뿐이다. 사용자가 일부러 켠 `CLAUDE_*` 설정
/// (`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS`, `CLAUDE_CODE_USE_BEDROCK` 등)은 세션까지 그대로 넘긴다.
/// 접두사로 지우는 `CLAUDE_CODE_SESSION_`은 세션 id, `CLAUDE_CODE_CHILD_`는 자식 세션 정보,
/// `CLAUDE_CODE_MESSAGING_`은 메시징 소켓과 토큰이다. 토큰이 다른 세션으로 새면 안 된다.
/// `MINE_STATE_DIR`는 훅(`mine _event`)이 이 데몬을 찾는 데 쓰므로 남긴다.
// ponytail: 이 목록은 실행 중인 Claude Code 환경에서 관찰한 변수들이다. Claude Code가 새 런타임 변수를 추가하면 늘려야 한다.
// ponytail: SSH_*(SSH_AUTH_SOCK 등)는 그대로 둔다. 원격 호스트를 다루는 5단계에서 정한다.
pub fn is_leaked_env(name: &str) -> bool {
    matches!(
        name,
        "TMUX"
            | "TMUX_PANE"
            | "CLAUDECODE"
            | "CLAUDE_CODE_ENTRYPOINT"
            | "CLAUDE_CODE_EXECPATH"
            | "CLAUDE_CODE_SSE_PORT"
            | "CLAUDE_PID"
            | "CLAUDE_PLUGIN_DATA"
            | "CLAUDE_EFFORT"
            | "CLAUDE_CONFIG_DIR"
            | "MINE_SESSION"
    ) || [
        "CLAUDE_CODE_SESSION_",
        "CLAUDE_CODE_CHILD_",
        "CLAUDE_CODE_MESSAGING_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

/// 로그인 셸 경로. `SHELL`이 비어 있거나 없으면 `/bin/zsh`다.
pub fn login_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_string())
}

/// 셸이 한 단어로 읽도록 POSIX 작은따옴표로 감싼다. 안전한 글자만 있으면 그대로 둔다.
pub(crate) fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | '@' | '%' | '+' | ',')
        });
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// 훅이 부르는 명령 줄 `<exe> _event <세션 폴더>`.
fn hook_command(exe: &Path, session_dir: &Path) -> String {
    format!(
        "{} _event {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&session_dir.to_string_lossy())
    )
}

/// 세션 종류별 실행 명령을 만든다. claude와 codex에는 상태 훅을 주입한다.
pub fn launch(meta: &Meta, state: &State, exe: &Path) -> Launch {
    let mut env = vec![("MINE_SESSION".to_string(), meta.id.clone())];
    let command = hook_command(exe, &state.session_dir(&meta.id));
    let mut argv = Vec::new();
    match meta.kind {
        Kind::Shell => argv.extend([login_shell(), "-l".to_string()]),
        Kind::Claude => {
            let c = json!({"type": "command", "command": command});
            let h = json!({"hooks": [c]});
            let settings = json!({"hooks": {
                "UserPromptSubmit": [h],
                "Stop": [h],
                "Notification": [{"matcher": "permission_prompt", "hooks": [c]}],
            }});
            argv.extend([
                "claude".to_string(),
                "--settings".to_string(),
                settings.to_string(),
            ]);
            if let Some(model) = &meta.model {
                argv.extend(["--model".to_string(), model.clone()]);
            }
            if let Some(id) = &meta.resume_id {
                argv.extend(["--resume".to_string(), id.clone()]);
            }
            if meta.account.as_deref() == Some("claude-b") {
                let dir = state.profiles_dir().join("claude-b");
                env.push((
                    "CLAUDE_CONFIG_DIR".to_string(),
                    dir.to_string_lossy().into_owned(),
                ));
            }
        }
        Kind::Codex => {
            // `-c` 값은 JSON이 아니라 TOML이다. JSON 문자열 이스케이프는 TOML 기본 문자열에도 통한다.
            let value = format!(
                "[{{hooks=[{{type=\"command\",command={}}}]}}]",
                json!(command)
            );
            argv.push("codex".to_string());
            for event in ["UserPromptSubmit", "Stop"] {
                argv.extend(["-c".to_string(), format!("hooks.{event}={value}")]);
            }
            if let Some(model) = &meta.model {
                argv.extend(["--model".to_string(), model.clone()]);
            }
            if let Some(id) = &meta.resume_id {
                argv.extend(["resume".to_string(), id.clone()]);
            }
        }
    }
    Launch {
        argv,
        env,
        cwd: meta.project.clone(),
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

    fn st() -> State {
        State {
            root: PathBuf::from("/s"),
        }
    }

    fn meta(kind: Kind, account: Option<&str>) -> Meta {
        Meta {
            id: "k1-rust".into(),
            project: PathBuf::from("/work/mine"),
            name: "rust".into(),
            kind,
            account: account.map(Into::into),
            model: None,
            restore: Restore::Resume,
            created_ms: 0,
            resume_id: None,
            closed: false,
        }
    }

    fn has_pair(argv: &[String], a: &str, b: &str) -> bool {
        argv.windows(2).any(|w| w[0] == a && w[1] == b)
    }

    fn session_env(l: &Launch) -> Option<&str> {
        l.env
            .iter()
            .find(|(k, _)| k == "MINE_SESSION")
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn shell_launch_uses_login_shell() {
        let l = launch(&meta(Kind::Shell, None), &st(), Path::new("/x/mine"));
        assert_eq!(l.argv, vec![login_shell(), "-l".to_string()]);
        assert_eq!(l.cwd, PathBuf::from("/work/mine"));
        assert_eq!(session_env(&l), Some("k1-rust"));
    }

    #[test]
    fn claude_a_launch_keeps_default_config_dir() {
        let l = launch(
            &meta(Kind::Claude, Some("claude-a")),
            &st(),
            Path::new("/x/mine"),
        );
        assert_eq!(l.argv[0], "claude");
        assert_eq!(l.argv[1], "--settings");
        assert!(!l.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
        assert_eq!(session_env(&l), Some("k1-rust"));
        assert_eq!(l.cwd, PathBuf::from("/work/mine"));
        let v: serde_json::Value = serde_json::from_str(&l.argv[2]).unwrap();
        assert_eq!(
            v["hooks"]["Notification"][0]["matcher"],
            "permission_prompt"
        );
        let command = "/x/mine _event /s/sessions/k1-rust";
        for event in ["UserPromptSubmit", "Stop"] {
            let c = &v["hooks"][event][0]["hooks"][0];
            assert_eq!(c["type"], "command");
            assert_eq!(c["command"], command);
        }
        assert_eq!(
            v["hooks"]["Notification"][0]["hooks"][0]["command"],
            command
        );
    }

    #[test]
    fn claude_without_account_keeps_default_config_dir() {
        let l = launch(&meta(Kind::Claude, None), &st(), Path::new("/x/mine"));
        assert!(!l.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn claude_b_launch_sets_profile_dir() {
        let l = launch(
            &meta(Kind::Claude, Some("claude-b")),
            &st(),
            Path::new("/x/mine"),
        );
        assert!(l.env.contains(&(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/s/profiles/claude-b".to_string()
        )));
        assert_eq!(session_env(&l), Some("k1-rust"));
    }

    #[test]
    fn claude_model_and_resume_add_flags() {
        let mut m = meta(Kind::Claude, Some("claude-a"));
        m.model = Some("opus".into());
        m.resume_id = Some("s1".into());
        let l = launch(&m, &st(), Path::new("/x/mine"));
        assert!(has_pair(&l.argv, "--model", "opus"));
        assert!(has_pair(&l.argv, "--resume", "s1"));
        let plain = launch(
            &meta(Kind::Claude, Some("claude-a")),
            &st(),
            Path::new("/x/mine"),
        );
        assert_eq!(plain.argv.len(), 3);
    }

    #[test]
    fn claude_resume_adds_flag() {
        let mut m = meta(Kind::Claude, Some("claude-a"));
        m.resume_id = Some("s1".into());
        let l = launch(&m, &st(), Path::new("/x/mine"));
        assert!(has_pair(&l.argv, "--resume", "s1"));
    }

    #[test]
    fn codex_launch_injects_toml_hooks() {
        let l = launch(
            &meta(Kind::Codex, Some("codex")),
            &st(),
            Path::new("/x/mine"),
        );
        assert_eq!(l.argv[0], "codex");
        assert!(l.argv.iter().any(|a| {
            a.starts_with("hooks.UserPromptSubmit=[{hooks=[{type=\"command\",command=\"")
        }));
        assert!(
            l.argv
                .iter()
                .any(|a| { a.starts_with("hooks.Stop=[{hooks=[{type=\"command\",command=\"") })
        );
        assert_eq!(l.argv.iter().filter(|a| *a == "-c").count(), 2);
        assert_eq!(session_env(&l), Some("k1-rust"));
        assert!(!l.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
        // TOML 값 안의 문자열은 JSON 문자열로 읽혀 원래 명령이 되어야 한다.
        let arg = l
            .argv
            .iter()
            .find(|a| a.starts_with("hooks.Stop="))
            .unwrap();
        let quoted = arg
            .strip_prefix("hooks.Stop=[{hooks=[{type=\"command\",command=")
            .and_then(|r| r.strip_suffix("}]}]"))
            .unwrap();
        let command: String = serde_json::from_str(quoted).unwrap();
        assert_eq!(command, "/x/mine _event /s/sessions/k1-rust");
    }

    #[test]
    fn codex_resume_uses_subcommand() {
        let mut m = meta(Kind::Codex, Some("codex"));
        m.model = Some("gpt-5".into());
        m.resume_id = Some("t1".into());
        let l = launch(&m, &st(), Path::new("/x/mine"));
        assert!(has_pair(&l.argv, "--model", "gpt-5"));
        assert_eq!(l.argv[l.argv.len() - 2..], ["resume", "t1"]);
    }

    #[test]
    fn leaked_env_covers_tmux_claude_and_mine_variables() {
        for leaked in [
            "TMUX",
            "TMUX_PANE",
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_EXECPATH",
            "CLAUDE_CODE_SSE_PORT",
            "CLAUDE_PID",
            "CLAUDE_PLUGIN_DATA",
            "CLAUDE_EFFORT",
            "CLAUDE_CONFIG_DIR",
            "MINE_SESSION",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
        ] {
            assert!(is_leaked_env(leaked), "{leaked}");
        }
        // 사용자가 정한 CLAUDE_* 설정은 세션까지 넘어간다.
        for kept in [
            "PATH",
            "HOME",
            "MINE_STATE_DIR",
            "SSH_AUTH_SOCK",
            "CLAUDE",
            "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_SESSION",
            "CLAUDE_API_KEY",
        ] {
            assert!(!is_leaked_env(kept), "{kept}");
        }
    }

    #[test]
    fn hook_command_quotes_unsafe_paths() {
        assert_eq!(shell_quote("/x/mine"), "/x/mine");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(x)"), "'$(x)'");
        let st = State {
            root: PathBuf::from("/s s"),
        };
        let l = launch(&meta(Kind::Claude, None), &st, Path::new("/my bin/mine"));
        let v: serde_json::Value = serde_json::from_str(&l.argv[2]).unwrap();
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            "'/my bin/mine' _event '/s s/sessions/k1-rust'"
        );
    }
}
