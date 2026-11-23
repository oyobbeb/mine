//! 계정 id(`claude-a`, `claude-b`, `codex`)를 화면에 보일 짧은 이름으로 바꾼다.
//!
//! 이름은 별칭(`<state>/config.toml`) > 자동(로그인 정보) > id 순으로 정한다. 세션이 쓰는 계정 id는 그대로이고
//! 화면에 글로 보이는 자리만 이 이름을 쓴다.
//!
//! 자격 증명은 읽거나 저장하지 않는다. Claude의 자동 이름을 위해 `.claude.json`에서 `oauthAccount.emailAddress`
//! 하나만 읽고, 그 `@` 앞부분과 도메인 첫 마디만 기억한다. `.credentials.json`, 키체인, Codex 인증 파일은 열지 않는다.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::paths::State;

const CLAUDE_A: &str = "claude-a";
const CLAUDE_B: &str = "claude-b";
const CODEX: &str = "codex";

/// 계정 id → 화면에 보일 이름. [`Labels::load`]로 읽고, 모르는 id는 id를 그대로 돌려준다.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Labels {
    names: HashMap<String, String>,
}

impl Labels {
    /// 별칭 > 자동 > id 순으로 이름을 읽는다. 읽을 수 없는 파일은 없는 것으로 본다.
    ///
    /// `home`은 claude-a의 `.claude.json`이 있는 폴더(`~`)이고, 없으면 claude-a의 자동 이름이 없다.
    /// 클라이언트가 시작할 때 한 번 부른다(앱은 창이 다시 활성화될 때도).
    ///
    /// ponytail: 파일 감시는 하지 않는다. 계정을 바꾸거나 별칭을 고친 뒤에는 클라이언트를 다시 띄우거나
    /// (앱은) 창을 다시 활성화한다.
    pub fn load(state: &State, home: Option<&Path>) -> Labels {
        let mut names = HashMap::from([(CODEX.to_string(), "Codex".to_string())]);
        let a = home.and_then(|home| read_identity(&home.join(".claude.json")));
        let b = read_identity(&state.profiles_dir().join(CLAUDE_B).join(".claude.json"));
        // 두 계정의 앞부분이 같으면 둘 다 도메인을 붙여 가른다.
        let same = matches!((&a, &b), (Some(a), Some(b)) if a.local == b.local);
        for (id, identity) in [(CLAUDE_A, a), (CLAUDE_B, b)] {
            if let Some(identity) = identity {
                let name = if same {
                    identity.with_domain()
                } else {
                    identity.local
                };
                names.insert(id.to_string(), name);
            }
        }
        names.extend(read_aliases(state));
        Labels { names }
    }

    /// `account`의 화면 이름. 모르는 id는 id 그대로다.
    pub fn get<'a>(&'a self, account: &'a str) -> &'a str {
        self.names.get(account).map_or(account, String::as_str)
    }
}

/// `.claude.json`의 `oauthAccount.emailAddress`에서 `@` 앞부분. 계정 정보나 이메일이 없으면 `None`이다.
pub fn auto_label(claude_json: &serde_json::Value) -> Option<String> {
    let claude = ClaudeJson::deserialize(claude_json).ok()?;
    identity(claude).map(|identity| identity.local)
}

/// `.claude.json`에서 읽는 부분. 이메일 말고 다른 필드는 읽지도 기억하지도 않는다.
#[derive(Deserialize)]
struct ClaudeJson {
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<OauthAccount>,
}

#[derive(Deserialize)]
struct OauthAccount {
    #[serde(rename = "emailAddress")]
    email_address: Option<String>,
}

/// 이메일에서 기억하는 두 조각. 이메일 전체는 들고 있지 않는다.
struct Identity {
    /// `@` 앞부분.
    local: String,
    /// 도메인 첫 마디(`gmail.com`이면 `gmail`). 없으면 빈 값이다.
    domain: String,
}

impl Identity {
    /// 같은 앞부분을 가르는 `앞부분@도메인첫마디`. 도메인이 없으면 앞부분이다.
    fn with_domain(self) -> String {
        if self.domain.is_empty() {
            self.local
        } else {
            format!("{}@{}", self.local, self.domain)
        }
    }
}

fn identity(claude: ClaudeJson) -> Option<Identity> {
    let email = claude.oauth_account?.email_address?;
    let (local, domain) = email.trim().split_once('@')?;
    Some(Identity {
        local: clean(local)?,
        domain: clean(domain.split('.').next().unwrap_or_default()).unwrap_or_default(),
    })
}

/// `path`의 `.claude.json`에서 이메일 조각만 읽는다. 없거나 깨졌으면 `None`이다.
fn read_identity(path: &Path) -> Option<Identity> {
    let bytes = fs::read(path).ok()?;
    let claude = serde_json::from_slice::<ClaudeJson>(&bytes).ok()?;
    identity(claude)
}

/// 화면에 쓸 글. 앞뒤 공백을 떼고, 비었거나 제어 문자가 있으면 화면을 깰 수 있어 쓰지 않는다.
fn clean(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty() && !text.chars().any(char::is_control)).then(|| text.to_string())
}

/// `<state>/config.toml`의 별칭. 파일이 없거나 읽을 수 없으면 없는 것으로 보고, 문법이 틀리면 경고를 남기고 없는 것으로 본다.
fn read_aliases(state: &State) -> HashMap<String, String> {
    let path = state.root.join("config.toml");
    let Ok(text) = fs::read_to_string(&path) else {
        return HashMap::new();
    };
    parse_aliases(&text).unwrap_or_else(|why| {
        eprintln!("경고: {}을 읽지 못해 별칭 없이 써요: {why}", path.display());
        HashMap::new()
    })
}

/// `config.toml`의 `[accounts.<id>] label`들. 문자열이 아니거나 쓸 수 없는 항목은 건너뛴다.
/// 문법이 틀리면 로그에 남길 사유(줄 번호와 오류 종류)를 준다. 파일 내용은 담지 않는다.
fn parse_aliases(text: &str) -> Result<HashMap<String, String>, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        let line = e.span().map_or(0, |span| {
            let upto = span.start.min(text.len());
            1 + text.as_bytes()[..upto]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
        });
        format!("{line}줄: {}", e.message())
    })?;
    let accounts = table.get("accounts").and_then(toml::Value::as_table);
    Ok(accounts
        .into_iter()
        .flatten()
        .filter_map(|(id, entry)| {
            let label = entry.get("label")?.as_str()?;
            Some((id.clone(), clean(label)?))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    /// 임시 홈과 상태 폴더. 사용자의 진짜 `~/.claude.json`은 읽지 않는다.
    struct Env {
        _dir: tempfile::TempDir,
        home: PathBuf,
        state: State,
    }

    impl Env {
        fn new() -> Env {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let root = dir.path().join("state");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&root).unwrap();
            Env {
                home,
                state: State { root },
                _dir: dir,
            }
        }

        fn write(path: PathBuf, text: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        /// claude-a의 `.claude.json`.
        fn claude_a(&self, email: &str) {
            Env::write(self.home.join(".claude.json"), &claude_json(email));
        }

        /// claude-b의 `.claude.json`.
        fn claude_b(&self, email: &str) {
            let dir = self.state.profiles_dir().join("claude-b");
            Env::write(dir.join(".claude.json"), &claude_json(email));
        }

        fn config(&self, text: &str) {
            Env::write(self.state.root.join("config.toml"), text);
        }

        fn load(&self) -> Labels {
            Labels::load(&self.state, Some(&self.home))
        }
    }

    fn claude_json(email: &str) -> String {
        json!({
            "numStartups": 3,
            "oauthAccount": {"emailAddress": email, "displayName": "Someone"},
        })
        .to_string()
    }

    #[test]
    fn auto_label_takes_email_local_part() {
        let v = json!({"oauthAccount": {"emailAddress": "yb@example.com"}});
        assert_eq!(auto_label(&v), Some("yb".to_string()));
        // 계정 정보가 없거나, 이메일이 없거나, 이메일 꼴이 아니면 자동 이름이 없다.
        assert_eq!(auto_label(&json!({})), None);
        assert_eq!(auto_label(&json!({"oauthAccount": {}})), None);
        assert_eq!(auto_label(&json!({"oauthAccount": null})), None);
        assert_eq!(auto_label(&json!({"oauthAccount": "yb@example.com"})), None);
        for email in [
            "",
            "yb",
            "@example.com",
            "  @example.com",
            "y\nb@example.com",
        ] {
            let v = json!({"oauthAccount": {"emailAddress": email}});
            assert_eq!(auto_label(&v), None, "{email:?}");
        }
        let v = json!({"oauthAccount": {"emailAddress": 7}});
        assert_eq!(auto_label(&v), None);
    }

    #[test]
    fn alias_beats_auto_beats_id() {
        let env = Env::new();
        env.claude_a("yb@gmail.com");
        env.claude_b("work@acme.xyz");
        env.config("[accounts.claude-a]\nlabel = \"개인\"\n");
        let labels = env.load();
        // 별칭이 있는 계정은 별칭, `.claude.json`만 있는 계정은 자동 이름이다.
        assert_eq!(labels.get("claude-a"), "개인");
        assert_eq!(labels.get("claude-b"), "work");

        // 아무것도 없는 계정은 id를 쓴다.
        let empty = Env::new();
        let labels = empty.load();
        assert_eq!(labels.get("claude-a"), "claude-a");
        assert_eq!(labels.get("claude-b"), "claude-b");
        // 모르는 id와 계정이 없는 shell의 빈 말도 그대로다.
        assert_eq!(labels.get("shell"), "shell");
        assert_eq!(labels.get(""), "");
        // 홈을 모르면 claude-a는 자동 이름이 없다.
        let bare = Env::new();
        bare.claude_a("yb@gmail.com");
        assert_eq!(Labels::load(&bare.state, None).get("claude-a"), "claude-a");
        assert_eq!(bare.load().get("claude-a"), "yb");
    }

    #[test]
    fn same_auto_labels_get_domain() {
        let env = Env::new();
        env.claude_a("yb@gmail.com");
        env.claude_b("yb@acme.xyz");
        let labels = env.load();
        assert_eq!(labels.get("claude-a"), "yb@gmail");
        assert_eq!(labels.get("claude-b"), "yb@acme");

        // 이름이 다르면 늘리지 않는다.
        env.claude_b("other@acme.xyz");
        let labels = env.load();
        assert_eq!(labels.get("claude-a"), "yb");
        assert_eq!(labels.get("claude-b"), "other");

        // 한쪽만 있으면 겹칠 일이 없다.
        let one = Env::new();
        one.claude_b("yb@acme.xyz");
        assert_eq!(one.load().get("claude-b"), "yb");
    }

    #[test]
    fn bad_config_is_ignored() {
        let env = Env::new();
        env.claude_a("yb@gmail.com");
        for broken in [
            "[accounts.claude-a\nlabel = ",
            "label = \"yb@gmail.com",
            "\u{0}\u{1}not toml at all",
            "[accounts]\nclaude-a = 3\n",
            "accounts = 5\n",
        ] {
            env.config(broken);
            let labels = env.load();
            assert_eq!(labels.get("claude-a"), "yb", "{broken:?}");
            assert_eq!(labels.get("codex"), "Codex", "{broken:?}");
        }
        // 읽을 수 없는 config(폴더이거나 UTF-8이 아님)도 없는 것으로 본다.
        fs::remove_file(env.state.root.join("config.toml")).unwrap();
        fs::create_dir(env.state.root.join("config.toml")).unwrap();
        assert_eq!(env.load().get("claude-a"), "yb");
        fs::remove_dir(env.state.root.join("config.toml")).unwrap();
        fs::write(env.state.root.join("config.toml"), [0xff, 0xfe, 0xfd]).unwrap();
        assert_eq!(env.load().get("claude-a"), "yb");
        // 깨진 `.claude.json`은 자동 이름이 없는 것이다.
        Env::write(env.home.join(".claude.json"), "{not json");
        assert_eq!(env.load().get("claude-a"), "claude-a");
    }

    #[test]
    fn bad_config_warning_never_quotes_the_file() {
        // 경고는 로그에 남는다. 깨진 줄에 이메일이 있어도 파일 내용은 담지 않는다.
        let text = "[accounts.claude-a]\nlabel = \"yb@gmail.com\n";
        let warning = parse_aliases(text).unwrap_err();
        assert!(warning.contains("2줄"), "{warning}");
        assert!(
            !warning.contains("yb") && !warning.contains("gmail"),
            "{warning}"
        );
    }

    #[test]
    fn codex_defaults_to_codex() {
        let env = Env::new();
        assert_eq!(env.load().get("codex"), "Codex");
        // 별칭이 있으면 별칭이다.
        env.config("[accounts.codex]\nlabel = \"gpt\"\n");
        assert_eq!(env.load().get("codex"), "gpt");
    }

    #[test]
    fn aliases_are_trimmed_and_bad_ones_skipped() {
        let env = Env::new();
        env.claude_a("yb@gmail.com");
        env.claude_b("work@acme.xyz");
        env.config(
            "[accounts.claude-a]\nlabel = \"  개인  \"\n\
             [accounts.claude-b]\nlabel = 7\n\
             [accounts.codex]\nlabel = \"   \"\n\
             [accounts.work]\nlabel = \"회사\"\n\
             [accounts.bad]\nlabel = \"a\\nb\"\n\
             [hosts]\nbox = \"ssh\"\n",
        );
        let labels = env.load();
        assert_eq!(labels.get("claude-a"), "개인");
        // 문자열이 아니거나 빈 별칭, 제어 문자가 든 별칭은 건너뛰어 자동 이름을 쓴다.
        assert_eq!(labels.get("claude-b"), "work");
        assert_eq!(labels.get("codex"), "Codex");
        assert_eq!(labels.get("bad"), "bad");
        // 다른 계정 id의 별칭도 받는다.
        assert_eq!(labels.get("work"), "회사");
    }

    #[test]
    fn default_labels_show_ids() {
        let labels = Labels::default();
        assert_eq!(labels.get("claude-a"), "claude-a");
        assert_eq!(labels.get("codex"), "codex");
    }
}
