//! Turns a profile id (`claude-a`, `claude-b`, `codex`, or any id in `[profiles]`) into a short name to show on screen.
//!
//! The name is chosen by label (`<state>/config.toml`) > auto (login info) > id; [`crate::profiles::infos`] decides it and [`Labels`]
//! is a lookup over the result. The profile id a session uses stays the same; only the places where it is shown as text use this name.
//!
//! Credentials are never read or stored. For Claude's auto name, only `oauthAccount.emailAddress` is read from `.claude.json`,
//! and only the part before `@` and the first label of the domain are remembered. `.credentials.json`, the keychain and Codex auth files are never opened.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::paths::State;
use crate::profiles::{self, ProfileInfo, Profiles};

/// Profile id → name to show on screen. Read with [`Labels::load`] or built from [`profiles::infos`] with [`Labels::from_infos`];
/// an unknown id comes back as the id itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Labels {
    names: HashMap<String, String>,
    profiles: Vec<ProfileInfo>,
}

impl Labels {
    /// Reads names in the order label > auto > id, through [`Profiles::load`] and [`profiles::infos`].
    /// A config that is broken or can't be read falls back to the built-in profiles, and each warning is printed.
    ///
    /// `home` is the directory (`~`) that has the default Claude profile's `.claude.json`; without it, that profile has no auto name.
    /// Call it once when the client starts (and for the app, also when the window is activated again).
    ///
    /// ponytail: no file watching. After switching accounts or editing a label, restart the client or
    /// (for the app) activate the window again.
    pub fn load(state: &State, home: Option<&Path>) -> Labels {
        let (profiles, warnings) = Profiles::load(state);
        for warning in warnings {
            eprintln!("Warning: {warning}");
        }
        let mut labels = Labels::from_infos(&profiles::infos(&profiles, state, home));
        // Until the clients read profile infos, they ask for fixed ids: the claude-b row asks for `claude-b` even before its
        // directory exists. Keep a label for an id that isn't a profile, as before. A profile's own name always wins.
        let config = fs::read_to_string(state.root.join("config.toml")).unwrap_or_default();
        for (id, label) in config
            .parse::<toml::Table>()
            .map(|t| aliases(&t))
            .unwrap_or_default()
        {
            labels.names.entry(id).or_insert(label);
        }
        labels
    }

    /// The names of `infos`, which the daemon sends to clients.
    pub fn from_infos(infos: &[ProfileInfo]) -> Labels {
        Labels {
            names: infos
                .iter()
                .map(|info| (info.id.clone(), info.label.clone()))
                .collect(),
            profiles: infos.to_vec(),
        }
    }

    /// The profiles the names came from, for the typed new-session form ([`crate::view::parse_new`]).
    // ponytail: a bridge until the clients take the profile list from the daemon (Task 6).
    pub fn profiles(&self) -> &[ProfileInfo] {
        &self.profiles
    }

    /// The on-screen name of `account`. An unknown id comes back as the id.
    pub fn get<'a>(&'a self, account: &'a str) -> &'a str {
        self.names.get(account).map_or(account, String::as_str)
    }
}

/// The part before `@` in `.claude.json`'s `oauthAccount.emailAddress`. `None` if there is no account info or no email.
pub fn auto_label(claude_json: &serde_json::Value) -> Option<String> {
    let claude = ClaudeJson::deserialize(claude_json).ok()?;
    identity(claude).map(|identity| identity.local)
}

/// The part read from `.claude.json`. Fields other than the email are neither read nor remembered.
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

/// The two pieces kept from the email. The full email is not held.
#[derive(Clone)]
pub(crate) struct Identity {
    /// The part before `@`.
    local: String,
    /// The first label of the domain (`gmail` for `gmail.com`). Empty if there is none.
    domain: String,
}

impl Identity {
    /// `local@first-domain-label`, which tells same local parts apart. Just the local part if there is no domain.
    fn with_domain(&self) -> String {
        if self.domain.is_empty() {
            self.local.clone()
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

/// What a profile's login files say. Only existence and the email pieces are kept, never a credential.
pub(crate) struct Login {
    pub logged_in: bool,
    pub identity: Option<Identity>,
}

/// Reads the `.claude.json` at `path`. A missing or broken file is not logged in. Logged in means it has an `oauthAccount`.
pub(crate) fn read_login(path: &Path) -> Login {
    let claude = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ClaudeJson>(&bytes).ok());
    Login {
        logged_in: claude.as_ref().is_some_and(|c| c.oauth_account.is_some()),
        identity: claude.and_then(identity),
    }
}

/// The auto names for these logins, in order. If two share the same local part, both get the domain appended to tell them apart.
pub(crate) fn auto_names(identities: &[Option<Identity>]) -> Vec<Option<String>> {
    identities
        .iter()
        .map(|identity| {
            let identity = identity.as_ref()?;
            let shared = identities
                .iter()
                .flatten()
                .filter(|other| other.local == identity.local)
                .count()
                > 1;
            Some(if shared {
                identity.with_domain()
            } else {
                identity.local.clone()
            })
        })
        .collect()
}

/// The `[accounts.<id>] label` entries of a parsed `config.toml`. Entries that aren't strings or can't be used are skipped.
pub(crate) fn aliases(config: &toml::Table) -> HashMap<String, String> {
    let accounts = config.get("accounts").and_then(toml::Value::as_table);
    accounts
        .into_iter()
        .flatten()
        .filter_map(|(id, entry)| Some((id.clone(), clean(entry.get("label")?.as_str()?)?)))
        .collect()
}

/// Text to show on screen. Trims surrounding whitespace; if it is empty or has control characters it could break the display, so it isn't used.
pub(crate) fn clean(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty() && !text.chars().any(char::is_control)).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    /// Temporary home and state directory. The user's real `~/.claude.json` is never read.
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

        /// claude-a's `.claude.json`.
        fn claude_a(&self, email: &str) {
            Env::write(self.home.join(".claude.json"), &claude_json(email));
        }

        /// claude-b's `.claude.json`.
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
        // With no account info, no email, or something that isn't shaped like an email, there is no auto name.
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
        env.config("[accounts.claude-a]\nlabel = \"personal\"\n");
        let labels = env.load();
        // An account with a label shows the label; one with only a `.claude.json` shows the auto name.
        assert_eq!(labels.get("claude-a"), "personal");
        assert_eq!(labels.get("claude-b"), "work");

        // An account with nothing uses its id.
        let empty = Env::new();
        let labels = empty.load();
        assert_eq!(labels.get("claude-a"), "claude-a");
        assert_eq!(labels.get("claude-b"), "claude-b");
        // An unknown id and the empty string for a shell with no account come back unchanged too.
        assert_eq!(labels.get("shell"), "shell");
        assert_eq!(labels.get(""), "");
        // Without a home, claude-a has no auto name.
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

        // If the names differ, don't expand.
        env.claude_b("other@acme.xyz");
        let labels = env.load();
        assert_eq!(labels.get("claude-a"), "yb");
        assert_eq!(labels.get("claude-b"), "other");

        // With only one side present, there is nothing to overlap.
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
        // A config that can't be read (a directory, or not UTF-8) also counts as none.
        fs::remove_file(env.state.root.join("config.toml")).unwrap();
        fs::create_dir(env.state.root.join("config.toml")).unwrap();
        assert_eq!(env.load().get("claude-a"), "yb");
        fs::remove_dir(env.state.root.join("config.toml")).unwrap();
        fs::write(env.state.root.join("config.toml"), [0xff, 0xfe, 0xfd]).unwrap();
        assert_eq!(env.load().get("claude-a"), "yb");
        // A broken `.claude.json` means there is no auto name.
        Env::write(env.home.join(".claude.json"), "{not json");
        assert_eq!(env.load().get("claude-a"), "claude-a");
    }

    #[test]
    fn bad_config_warning_never_quotes_the_file() {
        // The warning goes to the log. Even if the broken line has an email, the file contents are not included.
        let env = Env::new();
        let text = "[accounts.claude-a]\nlabel = \"yb@gmail.com\n";
        let (_, warnings) = Profiles::parse(Some(text), &env.state);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("line 2"), "{warnings:?}");
        assert!(
            !warnings[0].contains("yb") && !warnings[0].contains("gmail"),
            "{warnings:?}"
        );
    }

    #[test]
    fn codex_defaults_to_codex() {
        let env = Env::new();
        assert_eq!(env.load().get("codex"), "Codex");
        // A label wins if there is one.
        env.config("[accounts.codex]\nlabel = \"gpt\"\n");
        assert_eq!(env.load().get("codex"), "gpt");
    }

    #[test]
    fn aliases_are_trimmed_and_bad_ones_skipped() {
        let env = Env::new();
        env.claude_a("yb@gmail.com");
        env.claude_b("work@acme.xyz");
        env.config(
            "[accounts.claude-a]\nlabel = \"  personal  \"\n\
             [accounts.claude-b]\nlabel = 7\n\
             [accounts.codex]\nlabel = \"   \"\n\
             [accounts.work]\nlabel = \"office\"\n\
             [accounts.bad]\nlabel = \"a\\nb\"\n\
             [hosts]\nbox = \"ssh\"\n",
        );
        let labels = env.load();
        assert_eq!(labels.get("claude-a"), "personal");
        // Labels that aren't strings, are empty, or contain control characters are skipped and the auto name is used.
        assert_eq!(labels.get("claude-b"), "work");
        assert_eq!(labels.get("codex"), "Codex");
        assert_eq!(labels.get("bad"), "bad");
        // Labels for other account ids are accepted too.
        assert_eq!(labels.get("work"), "office");
    }

    #[test]
    fn labels_follow_infos() {
        let env = Env::new();
        env.claude_a("yb@gmail.com");
        env.config("[profiles.team]\nkind = \"claude\"\nlabel = \"work\"\n[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\"\n");
        let (profiles, _) = Profiles::load(&env.state);
        let infos = profiles::infos(&profiles, &env.state, Some(&env.home));
        let labels = Labels::from_infos(&infos);
        assert_eq!(labels.get("team"), "work");
        assert_eq!(labels.get("claude-a"), "yb");
        // Not a profile: shown as the id.
        assert_eq!(labels.get("shell"), "shell");
        assert_eq!(labels, env.load());
    }

    #[test]
    fn default_labels_show_ids() {
        let labels = Labels::default();
        assert_eq!(labels.get("claude-a"), "claude-a");
        assert_eq!(labels.get("codex"), "codex");
    }
}
