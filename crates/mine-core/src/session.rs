//! Session data types and `meta.json` storage.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::paths::State;
use crate::profiles::{Profile, ProfileKind, Profiles};

/// Kind of program a session runs.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Claude,
    Codex,
    Shell,
}

impl From<ProfileKind> for Kind {
    fn from(kind: ProfileKind) -> Kind {
        match kind {
            ProfileKind::Claude => Kind::Claude,
            ProfileKind::Codex => Kind::Codex,
        }
    }
}

/// Session state shown in the list.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum SessionState {
    Approval,
    Working,
    Done,
    Idle,
    Ended,
}

/// How a session is brought back even after the daemon restarts. The default is chosen per kind.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Restore {
    Resume,
    Rerun(String),
    None,
}

/// Spec the client sends when creating a new session.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NewSession {
    pub project: PathBuf,
    pub name: String,
    pub kind: Kind,
    pub account: Option<String>,
    pub model: Option<String>,
}

/// Value stored in the session directory's `meta.json`.
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

/// One session the daemon reports to clients.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub meta: Meta,
    pub state: SessionState,
    pub waiting: Option<String>,
    pub alive: bool,
}

const META_FILE: &str = "meta.json";

/// A session id is `{project key}-{name}`.
pub fn session_id(project_key: &str, name: &str) -> String {
    format!("{project_key}-{name}")
}

/// The name must match `^[A-Za-z0-9][A-Za-z0-9_~-]{0,63}$`.
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
    /// Reads `dir/meta.json`.
    pub fn load(dir: &Path) -> Result<Meta> {
        let path = dir.join(META_FILE);
        let bytes = fs::read(&path).with_context(|| format!("Couldn't read {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid meta.json format: {}", path.display()))
    }

    /// Writes `dir/meta.json` atomically: writes to a temporary file in the same directory, then renames it.
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

/// Command the holder process starts as the session child.
#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// Whether an environment variable was inherited from the shell that started the daemon but must not be passed on to the session child. It is removed when the holder process is started,
/// and [`launch`] puts back the values that are needed (`MINE_SESSION`, a profile's `CLAUDE_CONFIG_DIR` or `CODEX_HOME`).
///
/// If the daemon was started inside tmux, `TMUX` sends tmux-aware tools to the outer tmux; if it was started inside a claude session,
/// `CLAUDECODE` blocks nested runs or `CLAUDE_CONFIG_DIR` runs a claude-a session under the claude-b account. `CODEX_HOME` would do the same for codex:
/// a profile's home comes only from [`launch`].
/// Only the variables a running Claude Code sets for its children are removed. `CLAUDE_*` settings the user turned on on purpose
/// (`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS`, etc.) are passed through to the session unchanged, except the ones on the
/// [`bypass_vars`] list.
/// Of the prefixes removed, `CLAUDE_CODE_SESSION_` is the session id, `CLAUDE_CODE_CHILD_` is child session info,
/// and `CLAUDE_CODE_MESSAGING_` is the messaging socket and token. A token must not leak into another session.
/// `MINE_STATE_DIR` is kept because the hook (`mine _event`) uses it to find this daemon.
// ponytail: this list is the variables observed in a running Claude Code environment. Extend it if Claude Code adds new runtime variables.
// ponytail: SSH_* (SSH_AUTH_SOCK, etc.) is left as is. It will be decided in stage 5, which handles remote hosts.
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
            | "CODEX_HOME"
            | "MINE_SESSION"
    ) || [
        "CLAUDE_CODE_SESSION_",
        "CLAUDE_CODE_CHILD_",
        "CLAUDE_CODE_MESSAGING_",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

const CLAUDE_BYPASS: [&str; 6] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
];
const CODEX_BYPASS: [&str; 2] = ["OPENAI_API_KEY", "CODEX_API_KEY"];

/// Variables that would route a CLI around its profile's login onto a key or another provider (design 10.2). The holder's env
/// cleaning removes them for that kind of session. A shell keeps everything.
// ponytail: no opt-out. A per-profile `env` table to opt back in comes when someone needs an API-key profile.
pub fn bypass_vars(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Claude => &CLAUDE_BYPASS,
        Kind::Codex => &CODEX_BYPASS,
        Kind::Shell => &[],
    }
}

/// The variables in this process's environment that a claude or codex child must not inherit: [`is_leaked_env`] and [`bypass_vars`].
/// The daemon removes them when it starts a holder process, and `mine profile login` removes them from the CLI it runs, so a session
/// and a login get the same environment. Nothing is removed for a shell, except what [`is_leaked_env`] names.
pub fn env_to_remove(kind: Kind) -> impl Iterator<Item = std::ffi::OsString> {
    let bypass = bypass_vars(kind);
    std::env::vars_os().map(|(key, _)| key).filter(move |key| {
        key.to_str()
            .is_some_and(|key| is_leaked_env(key) || bypass.contains(&key))
    })
}

/// Login shell path. `/bin/zsh` if `SHELL` is empty or unset.
pub fn login_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_string())
}

/// Wraps in POSIX single quotes so the shell reads it as one word. Left as is if it has only safe characters.
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

/// The command line the hook runs: `<exe> _event <session dir>`.
fn hook_command(exe: &Path, session_dir: &Path) -> String {
    format!(
        "{} _event {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&session_dir.to_string_lossy())
    )
}

/// The profile a session runs on: `account` holds its id. `None` for a session without one, which runs at the CLI's own home.
/// An id that isn't in `profiles`, or one of another kind, is an error: a session never falls back to another login.
pub fn profile<'a>(
    kind: Kind,
    account: Option<&str>,
    profiles: &'a Profiles,
) -> Result<Option<&'a Profile>> {
    let Some(id) = account else {
        return Ok(None);
    };
    let Some(profile) = profiles.get(id) else {
        bail!("Unknown profile: {id}");
    };
    if Kind::from(profile.kind) != kind {
        bail!("Profile {id} is a {} profile", profile.kind.word());
    }
    Ok(Some(profile))
}

/// Builds the launch command for each session kind. State hooks are injected into claude and codex, and a profile with its own
/// home gets `CLAUDE_CONFIG_DIR` or `CODEX_HOME`. Fails for an unknown or mismatched profile ([`profile`]).
///
/// Only builds the command: getting the profile's home ready (`share::prepare`) is up to the caller.
pub fn launch(
    meta: &Meta,
    state: &State,
    exe: &Path,
    profiles: &Profiles,
    user_home: &Path,
) -> Result<Launch> {
    let mut env = vec![("MINE_SESSION".to_string(), meta.id.clone())];
    if let Some((key, home)) = profile(meta.kind, meta.account.as_deref(), profiles)?
        .and_then(|profile| profile.env(state, user_home))
    {
        env.push((key.to_string(), home.to_string_lossy().into_owned()));
    }
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
        }
        Kind::Codex => {
            // The `-c` value is TOML, not JSON. JSON string escaping also works for TOML basic strings.
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
    Ok(Launch {
        argv,
        env,
        cwd: meta.project.clone(),
    })
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
        // Overwriting succeeds, and no temporary file is left behind.
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

    fn env_of<'a>(l: &'a Launch, key: &str) -> Option<&'a str> {
        l.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn session_env(l: &Launch) -> Option<&str> {
        env_of(l, "MINE_SESSION")
    }

    /// claude-a and codex at the CLIs' default homes, and a managed profile of each kind.
    const CONFIG: &str = "[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\"\n\
        [profiles.team]\nkind = \"claude\"\n\
        [profiles.codex]\nkind = \"codex\"\nhome = \"default\"\n\
        [profiles.codex-team]\nkind = \"codex\"\n";

    fn try_run(m: &Meta, state: &State, exe: &str) -> Result<Launch> {
        let (profiles, warnings) = Profiles::parse(Some(CONFIG), state);
        assert!(warnings.is_empty(), "{warnings:?}");
        launch(m, state, Path::new(exe), &profiles, Path::new("/h"))
    }

    fn run(m: &Meta) -> Launch {
        try_run(m, &st(), "/x/mine").unwrap()
    }

    #[test]
    fn shell_launch_uses_login_shell() {
        let l = run(&meta(Kind::Shell, None));
        assert_eq!(l.argv, vec![login_shell(), "-l".to_string()]);
        assert_eq!(l.cwd, PathBuf::from("/work/mine"));
        assert_eq!(session_env(&l), Some("k1-rust"));
    }

    #[test]
    fn claude_a_launch_keeps_default_config_dir() {
        let l = run(&meta(Kind::Claude, Some("claude-a")));
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
    fn launch_sets_env_for_managed_profile() {
        let l = run(&meta(Kind::Claude, Some("team")));
        assert_eq!(env_of(&l, "CLAUDE_CONFIG_DIR"), Some("/s/profiles/team"));
        assert_eq!(env_of(&l, "CODEX_HOME"), None);
        assert_eq!(session_env(&l), Some("k1-rust"));
        let l = run(&meta(Kind::Codex, Some("codex-team")));
        assert_eq!(env_of(&l, "CODEX_HOME"), Some("/s/profiles/codex-team"));
        assert_eq!(env_of(&l, "CLAUDE_CONFIG_DIR"), None);
    }

    #[test]
    fn default_profile_sets_no_env() {
        // A profile at the CLI's own home, or no profile at all (an older session), sets nothing but `MINE_SESSION`.
        for (kind, account) in [
            (Kind::Claude, Some("claude-a")),
            (Kind::Claude, None),
            (Kind::Codex, Some("codex")),
            (Kind::Codex, None),
        ] {
            let l = run(&meta(kind, account));
            let keys: Vec<&str> = l.env.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(keys, ["MINE_SESSION"], "{account:?}");
        }
    }

    #[test]
    fn legacy_claude_b_meta_resumes_same_dir() {
        // No `[profiles]` table: the built-in claude-b stands in once its directory exists, at the same path as before.
        let t = tmp();
        let state = State {
            root: t.path().to_path_buf(),
        };
        let dir = state.profiles_dir().join("claude-b");
        std::fs::create_dir_all(&dir).unwrap();
        let (profiles, warnings) = Profiles::load(&state);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut m = meta(Kind::Claude, Some("claude-b"));
        m.resume_id = Some("s1".into());
        let l = launch(&m, &state, Path::new("/x/mine"), &profiles, Path::new("/h")).unwrap();
        assert_eq!(env_of(&l, "CLAUDE_CONFIG_DIR"), dir.to_str());
        assert!(has_pair(&l.argv, "--resume", "s1"));
    }

    #[test]
    fn unknown_or_mismatched_profile_refuses_to_launch() {
        // Never falls back to another login.
        let err = |m: &Meta| try_run(m, &st(), "/x/mine").unwrap_err().to_string();
        assert_eq!(
            err(&meta(Kind::Claude, Some("gone"))),
            "Unknown profile: gone"
        );
        assert_eq!(
            err(&meta(Kind::Codex, Some("team"))),
            "Profile team is a claude profile"
        );
        assert_eq!(
            err(&meta(Kind::Claude, Some("codex"))),
            "Profile codex is a codex profile"
        );
    }

    #[test]
    fn claude_model_and_resume_add_flags() {
        let mut m = meta(Kind::Claude, Some("claude-a"));
        m.model = Some("opus".into());
        m.resume_id = Some("s1".into());
        let l = run(&m);
        assert!(has_pair(&l.argv, "--model", "opus"));
        assert!(has_pair(&l.argv, "--resume", "s1"));
        let plain = run(&meta(Kind::Claude, Some("claude-a")));
        assert_eq!(plain.argv.len(), 3);
    }

    #[test]
    fn codex_launch_injects_toml_hooks() {
        let l = run(&meta(Kind::Codex, Some("codex")));
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
        // A string inside a TOML value must read as a JSON string and give back the original command.
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
        let l = run(&m);
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
            "CODEX_HOME",
            "MINE_SESSION",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
        ] {
            assert!(is_leaked_env(leaked), "{leaked}");
        }
        // `CLAUDE_*` settings the user chose are passed through to the session.
        for kept in [
            "PATH",
            "HOME",
            "MINE_STATE_DIR",
            "SSH_AUTH_SOCK",
            "CLAUDE",
            "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS",
            "CLAUDE_CODE_SESSION",
            "CLAUDE_API_KEY",
        ] {
            assert!(!is_leaked_env(kept), "{kept}");
        }
    }

    #[test]
    fn bypass_vars_by_kind() {
        assert_eq!(
            bypass_vars(Kind::Claude),
            [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "AWS_BEARER_TOKEN_BEDROCK",
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX",
            ]
        );
        assert_eq!(
            bypass_vars(Kind::Codex),
            ["OPENAI_API_KEY", "CODEX_API_KEY"]
        );
        // A shell keeps everything, so these aren't leaked variables.
        assert!(bypass_vars(Kind::Shell).is_empty());
        for kind in [Kind::Claude, Kind::Codex] {
            assert!(!bypass_vars(kind).iter().any(|v| is_leaked_env(v)));
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
        let l = try_run(&meta(Kind::Claude, None), &st, "/my bin/mine").unwrap();
        let v: serde_json::Value = serde_json::from_str(&l.argv[2]).unwrap();
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            "'/my bin/mine' _event '/s s/sessions/k1-rust'"
        );
    }
}
