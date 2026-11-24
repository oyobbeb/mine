//! The pieces of a profile's display name that come from its login: the Claude login email's local part, and the old
//! `[accounts.<id>] label` entries. [`crate::profiles::infos`] puts them together (label > auto > id) and the daemon sends the result.
//!
//! Credentials are never read or stored. For Claude's auto name, only `oauthAccount.emailAddress` is read from `.claude.json`,
//! and only the part before `@` and the first label of the domain are remembered. `.credentials.json`, the keychain and Codex auth files are never opened.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

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
pub fn clean(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty() && !text.chars().any(char::is_control)).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::paths::State;
    use crate::profiles::Profiles;

    /// The auto name `.claude.json` content `v` gives: the part before `@` of `oauthAccount.emailAddress`.
    fn auto_label(v: &serde_json::Value) -> Option<String> {
        identity(ClaudeJson::deserialize(v).ok()?).map(|identity| identity.local)
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
    fn bad_config_warning_never_quotes_the_file() {
        // The warning goes to the log. Even if the broken line has an email, the file contents are not included.
        let dir = tempfile::tempdir().unwrap();
        let state = State {
            root: dir.path().to_path_buf(),
        };
        let text = "[accounts.claude-a]\nlabel = \"yb@gmail.com\n";
        let (_, warnings) = Profiles::parse(Some(text), &state);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("line 2"), "{warnings:?}");
        assert!(
            !warnings[0].contains("yb") && !warnings[0].contains("gmail"),
            "{warnings:?}"
        );
    }

    #[test]
    fn aliases_are_trimmed_and_bad_ones_skipped() {
        let config: toml::Table = "[accounts.claude-a]\nlabel = \"  personal  \"\n\
             [accounts.claude-b]\nlabel = 7\n\
             [accounts.codex]\nlabel = \"   \"\n\
             [accounts.work]\nlabel = \"office\"\n\
             [accounts.bad]\nlabel = \"a\\nb\"\n\
             [hosts]\nbox = \"ssh\"\n"
            .parse()
            .unwrap();
        let aliases = aliases(&config);
        // Labels that aren't strings, are empty, or contain control characters are skipped. Any id is accepted.
        let mut got: Vec<_> = aliases.iter().collect();
        got.sort();
        assert_eq!(
            got,
            [
                (&"claude-a".to_string(), &"personal".to_string()),
                (&"work".to_string(), &"office".to_string()),
            ]
        );
        // `accounts` that isn't a table has no entries.
        assert!(super::aliases(&"accounts = 5\n".parse().unwrap()).is_empty());
    }
}
