//! Profiles: named Claude or Codex logins, each with its own config home.
//!
//! The list comes from `[profiles.<id>]` tables in `<state>/config.toml`, in file order. Without a `[profiles]` table the built-in list
//! (`claude-a`, `claude-b` if its directory exists, `codex`) stands in, so an existing setup keeps working with no action.
//! Old `[accounts.<id>] label` entries still apply to profiles that have no label of their own.
//!
//! A config that is broken or half-written never strands the user: they get the built-in list and a warning. Warnings give the
//! line number and the reason (or the profile id and the key name), and never quote the file's values. That list only shows: no
//! session or login starts on it ([`Profiles::usable`]), since the config may have moved a built-in id to another home.
//!
//! Credentials are never read. Whether a profile is logged in is decided by existence only: `oauthAccount` in a Claude
//! `.claude.json`, or the existence of Codex's `auth.json`, which is checked and never opened.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::accounts::{self, Identity};
use crate::paths::State;
use crate::session::validate_name;

/// The launcher palette. Profiles without a `color` take these in config order, wrapping around.
pub const PALETTE: [(&str, &str); 6] = [
    ("gold", "#e5c07b"),
    ("violet", "#c678dd"),
    ("teal", "#56b6c2"),
    ("green", "#98c379"),
    ("blue", "#61afef"),
    ("red", "#e06c75"),
];

/// Items a Claude profile can share, relative to its home. Login, identity, conversations and caches are never on this list.
const CLAUDE_SHARE: [&str; 7] = [
    "skills",
    "commands",
    "agents",
    "CLAUDE.md",
    "settings.json",
    "plugins",
    "mcpServers",
];

/// Items a Codex profile can share, relative to its home.
const CODEX_SHARE: [&str; 3] = ["AGENTS.md", "prompts", "config.toml"];

/// Keys a `[profiles.<id>]` table understands. Anything else is ignored with a warning.
const KEYS: [&str; 6] = ["kind", "home", "label", "color", "share", "default"];

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProfileKind {
    Claude,
    Codex,
}

impl ProfileKind {
    /// The items a profile of this kind can share. `share = [...]` is checked against this list.
    pub fn shareable(self) -> &'static [&'static str] {
        match self {
            ProfileKind::Claude => &CLAUDE_SHARE,
            ProfileKind::Codex => &CODEX_SHARE,
        }
    }

    /// The word used in `config.toml` and in messages.
    pub fn word(self) -> &'static str {
        match self {
            ProfileKind::Claude => "claude",
            ProfileKind::Codex => "codex",
        }
    }

    /// The variable that points the CLI at a config home.
    fn env_var(self) -> &'static str {
        match self {
            ProfileKind::Claude => "CLAUDE_CONFIG_DIR",
            ProfileKind::Codex => "CODEX_HOME",
        }
    }

    /// The CLI's own default home, relative to the user's home.
    fn default_dir(self) -> &'static str {
        match self {
            ProfileKind::Claude => ".claude",
            ProfileKind::Codex => ".codex",
        }
    }
}

/// Where a profile's config lives. This is explicit and stable: moving `default = true` never moves anyone's home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Home {
    /// The CLI's own default (`~/.claude` with `~/.claude.json`, or `~/.codex`). mine sets no environment variable for it.
    Default,
    /// An existing directory, absolute or starting with `~`. The `~` is expanded when the home is used.
    Path(PathBuf),
    /// `<state>/profiles/<id>`.
    Managed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Share {
    /// Every shareable item of the profile's kind.
    All,
    Off,
    /// Only these. Each one is on the kind's shareable list.
    Items(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// The stable key stored in `meta.json`. Follows the session-name rules. Never renamed: use `label` for display.
    pub id: String,
    pub kind: ProfileKind,
    pub home: Home,
    /// An alias from `label` (or the old `[accounts.<id>]` entry). Already trimmed and free of control characters.
    pub label: Option<String>,
    /// A hex color. A palette name in the config is already turned into its hex.
    pub color: Option<String>,
    pub share: Share,
    /// `default = true` in the config. See [`Profiles::default_of`] for how the default of a kind is chosen.
    pub default: bool,
}

impl Profile {
    fn plain(id: &str, kind: ProfileKind, home: Home) -> Profile {
        Profile {
            id: id.to_string(),
            kind,
            home,
            label: None,
            color: None,
            share: Share::All,
            default: false,
        }
    }

    /// `user_home` is needed to find the CLI's default home and to expand `~`.
    fn needs_user_home(&self) -> bool {
        match &self.home {
            Home::Default => true,
            Home::Path(path) => path.starts_with("~"),
            Home::Managed => false,
        }
    }

    /// The config home: the default one is `~/.claude` or `~/.codex`.
    pub fn home_dir(&self, state: &State, user_home: &Path) -> PathBuf {
        match &self.home {
            Home::Default => user_home.join(self.kind.default_dir()),
            Home::Path(path) => match path.strip_prefix("~") {
                Ok(rest) if rest.as_os_str().is_empty() => user_home.to_path_buf(),
                Ok(rest) => user_home.join(rest),
                Err(_) => path.clone(),
            },
            Home::Managed => state.profiles_dir().join(&self.id),
        }
    }

    /// Claude's `.claude.json`. The default home keeps it beside the directory (`~/.claude.json`); any other home keeps it inside.
    pub fn claude_json(&self, state: &State, user_home: &Path) -> PathBuf {
        match self.home {
            Home::Default => user_home.join(".claude.json"),
            _ => self.home_dir(state, user_home).join(".claude.json"),
        }
    }

    /// The variable to set so the CLI uses this profile's home. `None` for the default home: the CLI finds its own.
    pub fn env(&self, state: &State, user_home: &Path) -> Option<(&'static str, PathBuf)> {
        (self.home != Home::Default).then(|| (self.kind.env_var(), self.home_dir(state, user_home)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profiles {
    /// In config order, which is the launcher order and the color order.
    pub list: Vec<Profile>,
    /// `list` is the built-in list, not the config's: there is no `[profiles]` table, or it is empty or has no usable profile, or the
    /// file couldn't be read. `mine profile add` and `rm` write the built-ins out before they change anything, so they don't disappear.
    pub builtin: bool,
    /// Why the config couldn't be used, when `list` stands in for it (`builtin` is then set). The line and the reason, never a value.
    /// `None` when there is simply no `[profiles]` table.
    pub broken: Option<String>,
}

impl Profiles {
    /// Reads `<state>/config.toml`. Always returns a usable list; the warnings say what was skipped or why the built-in list is used.
    /// A missing file is the normal case and gives no warning.
    pub fn load(state: &State) -> (Profiles, Vec<String>) {
        match fs::read_to_string(state.root.join("config.toml")) {
            Ok(text) => Profiles::parse(Some(&text), state),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Profiles::parse(None, state),
            Err(e) => {
                let (mut profiles, mut warnings) = Profiles::parse(None, state);
                let why = format!("config.toml can't be read ({})", e.kind());
                warnings.insert(0, format!("{why}; using the built-in profiles"));
                profiles.broken = Some(why);
                (profiles, warnings)
            }
        }
    }

    /// Builds the list from the text of `config.toml` (`None` for no file). `state` is only used to see whether the built-in
    /// `claude-b` exists. Only `[profiles]` and `[accounts]` are read: the rest of the file, such as the desktop app's `[appearance]`,
    /// is ignored without a warning.
    pub fn parse(text: Option<&str>, state: &State) -> (Profiles, Vec<String>) {
        let mut warnings = Vec::new();
        let mut broken = None;
        let table = match text.map(str::parse::<toml::Table>) {
            None => toml::Table::new(),
            Some(Ok(table)) => table,
            Some(Err(e)) => {
                let line = text.zip(e.span()).map_or(0, |(text, span)| {
                    let upto = span.start.min(text.len());
                    1 + text.as_bytes()[..upto]
                        .iter()
                        .filter(|&&b| b == b'\n')
                        .count()
                });
                broken = Some(format!("config.toml line {line}: {}", e.message()));
                toml::Table::new()
            }
        };
        let (mut list, is_builtin) = match table.get("profiles") {
            // An empty `[profiles]` header moves no id, so it is the same as none.
            None => (builtin(state), true),
            Some(toml::Value::Table(entries)) if entries.is_empty() => (builtin(state), true),
            Some(toml::Value::Table(entries)) => {
                let mut list: Vec<Profile> = Vec::new();
                for (id, value) in entries {
                    let mut profile = match entry(id, value, &mut warnings) {
                        Ok(profile) => profile,
                        Err(why) => {
                            warnings.push(format!("profile {id:?} skipped: {why}"));
                            continue;
                        }
                    };
                    // Per kind, one profile may use the CLI's default home and one may be marked `default`; the first wins.
                    let taken = |same: fn(&Profile) -> bool| {
                        list.iter().any(|o| o.kind == profile.kind && same(o))
                    };
                    if profile.home == Home::Default && taken(|o| o.home == Home::Default) {
                        warnings.push(format!(
                            "profile {id:?} skipped: another {} profile already uses home = \"default\"",
                            profile.kind.word()
                        ));
                        continue;
                    }
                    if profile.default && taken(|o| o.default) {
                        warnings.push(format!(
                            "profile {id:?}: another {} profile is already the default, default = true ignored",
                            profile.kind.word()
                        ));
                        profile.default = false;
                    }
                    list.push(profile);
                }
                if list.is_empty() {
                    broken = Some("[profiles] has no usable profile".into());
                    (builtin(state), true)
                } else {
                    (list, false)
                }
            }
            Some(_) => {
                broken = Some("profiles must be a table ([profiles.<id>])".into());
                (builtin(state), true)
            }
        };
        if let Some(why) = &broken {
            warnings.push(format!("{why}; using the built-in profiles"));
        }
        // Old `[accounts.<id>] label` entries fill in the profiles that have no label of their own.
        let aliases = accounts::aliases(&table);
        for profile in list.iter_mut().filter(|p| p.label.is_none()) {
            profile.label = aliases.get(&profile.id).cloned();
        }
        (
            Profiles {
                list,
                builtin: is_builtin,
                broken,
            },
            warnings,
        )
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.list.iter().find(|p| p.id == id)
    }

    /// The profile `id`, to start a session or a login on. While the config is `broken` every id is refused: the built-in list may
    /// put an id the config moved back on its built-in home, which is another login.
    pub fn usable(&self, id: &str) -> anyhow::Result<&Profile> {
        if let Some(why) = &self.broken {
            anyhow::bail!("Can't use profile {id}: {why}. Fix config.toml first");
        }
        self.get(id)
            .ok_or_else(|| anyhow::anyhow!("Unknown profile: {id}"))
    }

    /// The default of `kind`: the profile marked `default = true`, else the one at the CLI's default home, else the first of that kind.
    /// It picks the launcher's Enter row and the share source; it never moves a home.
    pub fn default_of(&self, kind: ProfileKind) -> Option<&Profile> {
        let mut of_kind = self.list.iter().filter(|p| p.kind == kind);
        of_kind
            .clone()
            .find(|p| p.default)
            .or_else(|| of_kind.clone().find(|p| p.home == Home::Default))
            .or_else(|| of_kind.next())
    }
}

/// The list used when there is no usable `[profiles]` table.
fn builtin(state: &State) -> Vec<Profile> {
    let mut list = vec![Profile::plain(
        "claude-a",
        ProfileKind::Claude,
        Home::Default,
    )];
    if state.profiles_dir().join("claude-b").is_dir() {
        list.push(Profile::plain(
            "claude-b",
            ProfileKind::Claude,
            Home::Managed,
        ));
    }
    list.push(Profile::plain("codex", ProfileKind::Codex, Home::Default));
    list
}

/// One `[profiles.<id>]` table. `Err` is the reason to skip the profile; `warnings` gets the problems that only drop one key.
///
/// A key that decides which account or files a profile touches (`kind`, `home`, `share`) skips the whole profile when it can't be
/// read, because guessing could run a session on the wrong account or share more than the user asked for. A cosmetic key
/// (`label`, `color`) or `default` is ignored with a warning instead.
fn entry(
    id: &str,
    value: &toml::Value,
    warnings: &mut Vec<String>,
) -> Result<Profile, &'static str> {
    validate_name(id).map_err(|_| {
        "ids start with a letter or digit, then use only letters, digits, _, ~ and - (up to 64 characters)"
    })?;
    // `mine new shell` starts a shell, so a profile with this id could never be picked by name.
    if id == "shell" {
        return Err("shell is reserved for plain shell sessions");
    }
    let table = value.as_table().ok_or("it must be a table")?;
    for key in table.keys().filter(|key| !KEYS.contains(&key.as_str())) {
        warnings.push(format!("profile {id:?}: unknown key {key:?}, ignored"));
    }
    let kind = match table.get("kind").and_then(toml::Value::as_str) {
        Some("claude") => ProfileKind::Claude,
        Some("codex") => ProfileKind::Codex,
        _ => return Err("kind must be \"claude\" or \"codex\""),
    };
    let home = match table.get("home") {
        None => Home::Managed,
        Some(toml::Value::String(home)) if home == "default" => Home::Default,
        // A relative path would depend on where the daemon was started, and `~user` isn't supported.
        Some(toml::Value::String(home))
            if Path::new(home).is_absolute() || Path::new(home).starts_with("~") =>
        {
            Home::Path(PathBuf::from(home))
        }
        Some(_) => {
            return Err("home must be \"default\", an absolute path, or a path starting with ~/");
        }
    };
    let share = match table.get("share") {
        None | Some(toml::Value::Boolean(true)) => Share::All,
        Some(toml::Value::Boolean(false)) => Share::Off,
        Some(toml::Value::Array(items)) => {
            let names: Option<Vec<&str>> = items.iter().map(toml::Value::as_str).collect();
            let names = names.ok_or("share must be true, false, or a list of item names")?;
            let (kept, dropped): (Vec<&str>, Vec<&str>) = names
                .into_iter()
                .partition(|name| kind.shareable().contains(name));
            if !dropped.is_empty() {
                warnings.push(format!(
                    "profile {id:?}: {} profiles can't share these items, ignored: {dropped:?}",
                    kind.word()
                ));
            }
            Share::Items(kept.into_iter().map(String::from).collect())
        }
        Some(_) => return Err("share must be true, false, or a list of item names"),
    };
    let label = match table.get("label") {
        None => None,
        Some(label) => {
            let label = label.as_str().and_then(accounts::clean);
            if label.is_none() {
                warnings.push(format!(
                    "profile {id:?}: label must be text without control characters, ignored"
                ));
            }
            label
        }
    };
    let color = match table.get("color") {
        None => None,
        Some(color) => {
            let color = color.as_str().and_then(resolve_color);
            if color.is_none() {
                warnings.push(format!(
                    "profile {id:?}: color must be a palette name or #rrggbb, using the automatic color"
                ));
            }
            color
        }
    };
    let default = match table.get("default") {
        None => false,
        Some(toml::Value::Boolean(default)) => *default,
        Some(_) => {
            warnings.push(format!(
                "profile {id:?}: default must be true or false, ignored"
            ));
            false
        }
    };
    Ok(Profile {
        id: id.to_string(),
        kind,
        home,
        label,
        color,
        share,
        default,
    })
}

/// A palette name becomes its hex; `#rrggbb` is kept as written; anything else is `None`.
fn resolve_color(text: &str) -> Option<String> {
    let named = PALETTE.iter().find(|(name, _)| *name == text);
    let hex = text
        .strip_prefix('#')
        .is_some_and(|hex| hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    named
        .map(|(_, hex)| hex.to_string())
        .or_else(|| hex.then(|| text.to_string()))
}

/// What a client shows for a profile. The daemon sends these, so clients never read `config.toml` or `.claude.json` themselves.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ProfileInfo {
    pub id: String,
    pub kind: ProfileKind,
    /// The alias, else Claude's login email (local part), else `Codex` for the default-home Codex profile, else the id.
    pub label: String,
    /// `#rrggbb`.
    pub color: String,
    /// The first character of the label, uppercased.
    pub chip: String,
    /// Whether this is the default of its kind ([`Profiles::default_of`]).
    pub default: bool,
    pub logged_in: bool,
}

impl ProfileInfo {
    /// `color` as `0xrrggbb`, for drawing the chip. A color that isn't `#rrggbb` (a daemon from another build) is gray.
    pub fn rgb(&self) -> u32 {
        self.color
            .strip_prefix('#')
            .filter(|hex| hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .unwrap_or(0x808080)
    }
}

/// The display data of every profile, in list order.
///
/// `user_home` is the user's home directory (`~`). Without it, the profiles at the CLI's default home (and any `~` path) can't be
/// found, so they show as not logged in and without an auto name.
///
/// This only checks that login files exist, and reads the email out of Claude's `.claude.json`. It never opens `.credentials.json`
/// or Codex's `auth.json`.
pub fn infos(p: &Profiles, state: &State, user_home: Option<&Path>) -> Vec<ProfileInfo> {
    let logins: Vec<accounts::Login> = p
        .list
        .iter()
        .map(|profile| login(profile, state, user_home))
        .collect();
    let identities: Vec<Option<Identity>> = logins.iter().map(|l| l.identity.clone()).collect();
    let auto = accounts::auto_names(&identities);
    p.list
        .iter()
        .zip(logins)
        .zip(auto)
        .enumerate()
        .map(|(position, ((profile, login), auto))| {
            let label = profile.label.clone().or(auto).unwrap_or_else(|| {
                match (profile.kind, &profile.home) {
                    (ProfileKind::Codex, Home::Default) => "Codex".to_string(),
                    _ => profile.id.clone(),
                }
            });
            let color = profile
                .color
                .clone()
                .unwrap_or_else(|| PALETTE[position % PALETTE.len()].1.to_string());
            ProfileInfo {
                id: profile.id.clone(),
                kind: profile.kind,
                chip: label
                    .chars()
                    .next()
                    .and_then(|c| c.to_uppercase().next())
                    .map(String::from)
                    .unwrap_or_default(),
                label,
                color,
                default: p
                    .default_of(profile.kind)
                    .is_some_and(|default| default.id == profile.id),
                logged_in: login.logged_in,
            }
        })
        .collect()
}

fn login(profile: &Profile, state: &State, user_home: Option<&Path>) -> accounts::Login {
    let home = match user_home {
        Some(home) => home,
        None if profile.needs_user_home() => {
            return accounts::Login {
                logged_in: false,
                identity: None,
            };
        }
        // Unused: this profile's home doesn't depend on the user's home.
        None => Path::new(""),
    };
    match profile.kind {
        ProfileKind::Claude => accounts::read_login(&profile.claude_json(state, home)),
        // Existence only: `auth.json` holds the login, so it is never opened.
        ProfileKind::Codex => accounts::Login {
            logged_in: profile.home_dir(state, home).join("auth.json").exists(),
            identity: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    /// Temporary user home and state directory. The real `~` and `~/.mine` are never touched.
    struct Env {
        _dir: tempfile::TempDir,
        home: PathBuf,
        state: State,
    }

    impl Env {
        fn new() -> Env {
            let dir = tempfile::Builder::new()
                .prefix("mp")
                .tempdir_in("/tmp")
                .unwrap();
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

        fn config(&self, text: &str) {
            fs::write(self.state.root.join("config.toml"), text).unwrap();
        }

        fn load(&self) -> (Profiles, Vec<String>) {
            Profiles::load(&self.state)
        }

        fn profiles(&self) -> Profiles {
            self.load().0
        }

        fn infos(&self) -> Vec<ProfileInfo> {
            infos(&self.profiles(), &self.state, Some(&self.home))
        }

        fn info(&self, id: &str) -> ProfileInfo {
            self.infos().into_iter().find(|i| i.id == id).unwrap()
        }

        /// The managed home of profile `id` (`<state>/profiles/<id>`).
        fn managed(&self, id: &str) -> PathBuf {
            self.state.profiles_dir().join(id)
        }
    }

    fn write(path: PathBuf, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A `.claude.json` with a login for `email`.
    fn login(email: &str) -> String {
        json!({
            "numStartups": 3,
            "oauthAccount": {"emailAddress": email, "displayName": "Someone"},
        })
        .to_string()
    }

    fn ids(p: &Profiles) -> Vec<&str> {
        p.list.iter().map(|p| p.id.as_str()).collect()
    }

    fn mkfifo(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(Command::new("mkfifo").arg(path).status().unwrap().success());
    }

    #[test]
    fn builtin_profiles_when_table_missing() {
        let env = Env::new();
        // No config file at all.
        let (p, warnings) = env.load();
        assert_eq!(ids(&p), ["claude-a", "codex"]);
        assert!(warnings.is_empty(), "{warnings:?}");
        // A config without a `[profiles]` table gives the same list.
        env.config("[hosts]\nbox = \"ssh\"\n");
        let (p, warnings) = env.load();
        assert_eq!(ids(&p), ["claude-a", "codex"]);
        assert!(warnings.is_empty(), "{warnings:?}");
        // claude-b joins the list when its directory exists.
        fs::create_dir_all(env.managed("claude-b")).unwrap();
        let p = env.profiles();
        assert_eq!(ids(&p), ["claude-a", "claude-b", "codex"]);
        assert_eq!(p.get("claude-a").unwrap().home, Home::Default);
        assert_eq!(p.get("claude-b").unwrap().home, Home::Managed);
        assert_eq!(p.get("codex").unwrap().home, Home::Default);
        assert_eq!(p.get("claude-a").unwrap().kind, ProfileKind::Claude);
        assert_eq!(p.get("claude-b").unwrap().kind, ProfileKind::Claude);
        assert_eq!(p.get("codex").unwrap().kind, ProfileKind::Codex);
        assert!(p.list.iter().all(|p| p.share == Share::All));
        assert!(p.get("nope").is_none());
    }

    #[test]
    fn accounts_labels_still_apply() {
        let env = Env::new();
        fs::create_dir_all(env.managed("claude-b")).unwrap();
        env.config("[accounts.claude-b]\nlabel = \"work\"\n");
        assert_eq!(env.info("claude-b").label, "work");
        // A label in the profile itself wins over the old `[accounts]` entry.
        env.config(
            "[profiles.claude-b]\nkind = \"claude\"\nlabel = \"mine\"\n\
             [accounts.claude-b]\nlabel = \"work\"\n",
        );
        assert_eq!(env.info("claude-b").label, "mine");
        // And an `[accounts]` entry still fills a profile that has none.
        env.config(
            "[profiles.claude-b]\nkind = \"claude\"\n[accounts.claude-b]\nlabel = \"work\"\n",
        );
        assert_eq!(env.info("claude-b").label, "work");
    }

    #[test]
    fn profiles_table_order_and_defaults() {
        let env = Env::new();
        // Ids are deliberately out of alphabetical order: the config order must win.
        env.config(
            "[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\"\n\
             [profiles.team]\nkind = \"claude\"\n\
             [profiles.zeta]\nkind = \"codex\"\n\
             [profiles.alpha]\nkind = \"codex\"\n",
        );
        let (p, warnings) = env.load();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(ids(&p), ["claude-a", "team", "zeta", "alpha"]);
        // The profile at the CLI's default home is the default of its kind.
        assert_eq!(p.default_of(ProfileKind::Claude).unwrap().id, "claude-a");
        // No codex profile has the default home: the first codex profile is the default.
        assert_eq!(p.default_of(ProfileKind::Codex).unwrap().id, "zeta");

        // `default = true` moves the default, never the home.
        env.config(
            "[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\"\n\
             [profiles.team]\nkind = \"claude\"\ndefault = true\n",
        );
        let p = env.profiles();
        assert_eq!(p.default_of(ProfileKind::Claude).unwrap().id, "team");
        assert_eq!(p.get("claude-a").unwrap().home, Home::Default);
        assert_eq!(p.get("team").unwrap().home, Home::Managed);
        // The infos flag exactly one default per kind; kinds with no profile have none.
        let infos = env.infos();
        let defaults: Vec<&str> = infos
            .iter()
            .filter(|i| i.default)
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(defaults, ["team"]);
        assert!(p.default_of(ProfileKind::Codex).is_none());

        // A second `default = true` for the same kind is ignored, with a warning that names it.
        env.config(
            "[profiles.a]\nkind = \"claude\"\ndefault = true\n\
             [profiles.b]\nkind = \"claude\"\ndefault = true\n",
        );
        let (p, warnings) = env.load();
        assert_eq!(p.default_of(ProfileKind::Claude).unwrap().id, "a");
        assert_eq!(ids(&p), ["a", "b"]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("\"b\""), "{warnings:?}");
    }

    #[test]
    fn home_tilde_expands() {
        let env = Env::new();
        env.config(
            "[profiles.tilde]\nkind = \"claude\"\nhome = \"~/x\"\n\
             [profiles.bare]\nkind = \"claude\"\nhome = \"~\"\n\
             [profiles.abs]\nkind = \"claude\"\nhome = \"/opt/claude-old\"\n\
             [profiles.rel]\nkind = \"claude\"\nhome = \"old/claude\"\n\
             [profiles.user]\nkind = \"claude\"\nhome = \"~other/x\"\n",
        );
        let (p, warnings) = env.load();
        let home = |id: &str| p.get(id).unwrap().home_dir(&env.state, &env.home);
        assert_eq!(home("tilde"), env.home.join("x"));
        assert_eq!(home("bare"), env.home);
        assert_eq!(home("abs"), PathBuf::from("/opt/claude-old"));
        // A relative path would depend on where the daemon was started, so it is refused.
        // `~user` isn't supported either.
        assert_eq!(ids(&p), ["tilde", "bare", "abs"]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings.iter().any(|w| w.contains("\"rel\"")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("\"user\"")),
            "{warnings:?}"
        );
    }

    #[test]
    fn only_one_default_home_per_kind() {
        let env = Env::new();
        env.config(
            "[profiles.first]\nkind = \"claude\"\nhome = \"default\"\n\
             [profiles.second]\nkind = \"claude\"\nhome = \"default\"\n\
             [profiles.cx]\nkind = \"codex\"\nhome = \"default\"\n",
        );
        let (p, warnings) = env.load();
        // The first one keeps the home; the kinds are counted separately.
        assert_eq!(ids(&p), ["first", "cx"]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("\"second\""), "{warnings:?}");
    }

    #[test]
    fn bad_ids_and_kinds_skipped_with_warning() {
        let env = Env::new();
        env.config(
            "[profiles]\nnotatable = 3\n\
             [profiles.\"a/b\"]\nkind = \"claude\"\n\
             [profiles.gpt-one]\nkind = \"gpt\"\n\
             [profiles.nokind]\nlabel = \"x\"\n\
             [profiles.shell]\nkind = \"claude\"\n\
             [profiles.ok]\nkind = \"claude\"\n\
             [profiles.fine]\nkind = \"codex\"\n",
        );
        let (p, warnings) = env.load();
        // Valid profiles are kept. `shell` is reserved: `mine new shell` starts a shell, not a profile.
        assert_eq!(ids(&p), ["ok", "fine"]);
        // One warning per skipped profile, each naming the profile (and none quoting a value).
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        for name in ["a/b", "gpt-one", "nokind", "shell", "notatable"] {
            assert!(
                warnings.iter().any(|w| w.contains(&format!("{name:?}"))),
                "{name}: {warnings:?}"
            );
        }
    }

    #[test]
    fn unknown_keys_warn_but_keep_the_profile() {
        let env = Env::new();
        env.config("[profiles.team]\nkind = \"claude\"\ncolour = \"red\"\nshare = true\n");
        let (p, warnings) = env.load();
        assert_eq!(ids(&p), ["team"]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("\"colour\""), "{warnings:?}");
        assert!(warnings[0].contains("\"team\""), "{warnings:?}");
    }

    #[test]
    fn share_forms() {
        let env = Env::new();
        env.config(
            "[profiles.all]\nkind = \"claude\"\n\
             [profiles.yes]\nkind = \"claude\"\nshare = true\n\
             [profiles.off]\nkind = \"claude\"\nshare = false\n\
             [profiles.some]\nkind = \"claude\"\nshare = [\"skills\", \"commands\"]\n\
             [profiles.codex-some]\nkind = \"codex\"\nshare = [\"prompts\"]\n\
             [profiles.cut]\nkind = \"claude\"\nshare = [\"skills\", \".credentials.json\", \"../x\"]\n\
             [profiles.text]\nkind = \"claude\"\nshare = \"skills\"\n\
             [profiles.mixed]\nkind = \"claude\"\nshare = [\"skills\", 7]\n\
             [profiles.crossed]\nkind = \"codex\"\nshare = [\"skills\"]\n",
        );
        let (p, warnings) = env.load();
        let share = |id: &str| p.get(id).unwrap().share.clone();
        assert_eq!(share("all"), Share::All);
        assert_eq!(share("yes"), Share::All);
        assert_eq!(share("off"), Share::Off);
        assert_eq!(
            share("some"),
            Share::Items(vec!["skills".into(), "commands".into()])
        );
        assert_eq!(share("codex-some"), Share::Items(vec!["prompts".into()]));
        // Only the kind's own shareable items count: login, conversation files and paths are dropped.
        assert_eq!(share("cut"), Share::Items(vec!["skills".into()]));
        assert_eq!(share("crossed"), Share::Items(vec![]));
        // A share setting that can't be read would silently over-share, so the profile is skipped instead.
        assert!(p.get("text").is_none() && p.get("mixed").is_none());
        for name in ["cut", "text", "mixed", "crossed"] {
            assert!(
                warnings.iter().any(|w| w.contains(&format!("{name:?}"))),
                "{name}: {warnings:?}"
            );
        }
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        // The lists in the design.
        assert_eq!(
            ProfileKind::Claude.shareable(),
            [
                "skills",
                "commands",
                "agents",
                "CLAUDE.md",
                "settings.json",
                "plugins",
                "mcpServers"
            ]
        );
        assert_eq!(
            ProfileKind::Codex.shareable(),
            ["AGENTS.md", "prompts", "config.toml"]
        );
    }

    #[test]
    fn appearance_table_is_ignored_by_profiles() {
        let env = Env::new();
        // `[appearance]` belongs to the desktop app: no warning, and the profiles are not "broken", whatever it holds.
        for appearance in [
            "[appearance]\nmode = \"dark\"\nglass = false\n",
            "[appearance]\nmode = \"blue\"\n",
            "appearance = 5\n",
        ] {
            env.config(&format!(
                "{appearance}[profiles.team]\nkind = \"claude\"\n[profiles.zeta]\nkind = \"codex\"\n"
            ));
            let (p, warnings) = env.load();
            assert!(warnings.is_empty(), "{appearance:?}: {warnings:?}");
            assert_eq!(p.broken, None, "{appearance:?}");
            assert_eq!(ids(&p), ["team", "zeta"], "{appearance:?}");
        }
        // Alone it is the same as no config: the built-in list, still without a warning.
        env.config("[appearance]\nmode = \"light\"\n");
        let (p, warnings) = env.load();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(p.broken, None);
        assert!(p.builtin);
    }

    #[test]
    fn broken_profiles_table_falls_back_with_warning() {
        let env = Env::new();
        fs::create_dir_all(env.managed("claude-b")).unwrap();
        // A syntax error: the built-in list, plus one warning that names the line.
        env.config("[profiles.team]\nkind = \"claude\"\nlabel = \"yb@gmail.com\n");
        let (p, warnings) = env.load();
        assert_eq!(ids(&p), ["claude-a", "claude-b", "codex"]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("line 3"), "{warnings:?}");
        // The warning is the line and the reason only: it never quotes the file.
        assert!(
            !warnings[0].contains("yb") && !warnings[0].contains("gmail"),
            "{warnings:?}"
        );
        // Valid TOML that isn't a usable table doesn't strand the user either.
        for text in [
            "profiles = 5\n",
            "[profiles]\nteam = 3\n",
            "[profiles.team]\nkind = \"gpt\"\n",
        ] {
            env.config(text);
            let (p, warnings) = env.load();
            assert_eq!(ids(&p), ["claude-a", "claude-b", "codex"], "{text:?}");
            assert!(!warnings.is_empty(), "{text:?}");
        }
        // A config that can't be read is also a warning, not a silent fallback; a missing one isn't.
        fs::remove_file(env.state.root.join("config.toml")).unwrap();
        fs::create_dir(env.state.root.join("config.toml")).unwrap();
        let (p, warnings) = env.load();
        assert_eq!(ids(&p), ["claude-a", "claude-b", "codex"]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        fs::remove_dir(env.state.root.join("config.toml")).unwrap();
        assert!(env.load().1.is_empty());
    }

    #[test]
    fn broken_config_starts_no_profile() {
        let env = Env::new();
        // The built-in claude-b home exists, but the config moved claude-b elsewhere: a broken config must not start it there.
        fs::create_dir_all(env.managed("claude-b")).unwrap();
        for (text, why) in [
            (
                "[profiles.claude-b]\nkind = \"claude\"\nhome = \"~/work\n",
                "line 3",
            ),
            ("profiles = 5\n", "must be a table"),
            (
                "[profiles.claude-b]\nkind = \"claude\"\nhome = \"work\"\n",
                "no usable profile",
            ),
        ] {
            env.config(text);
            let p = env.profiles();
            assert_eq!(ids(&p), ["claude-a", "claude-b", "codex"], "{text:?}");
            for id in ["claude-a", "claude-b", "codex"] {
                let e = p.usable(id).unwrap_err().to_string();
                assert!(e.contains("config.toml") && e.contains(why), "{e}");
                assert!(!e.contains("work"), "the error quotes the file: {e}");
            }
        }
        fs::remove_file(env.state.root.join("config.toml")).unwrap();
        fs::create_dir(env.state.root.join("config.toml")).unwrap();
        assert!(env.profiles().usable("claude-a").is_err());
        fs::remove_dir(env.state.root.join("config.toml")).unwrap();
        // No file, a config without `[profiles]`, and a usable one start their profiles; an id that isn't there is unknown.
        assert!(env.profiles().usable("claude-b").is_ok());
        env.config("[accounts.claude-b]\nlabel = \"Work\"\n");
        assert!(env.profiles().usable("claude-b").is_ok());
        env.config("[profiles.w]\nkind = \"claude\"\n");
        let p = env.profiles();
        assert_eq!(p.usable("w").unwrap().id, "w");
        let e = p.usable("claude-b").unwrap_err().to_string();
        assert_eq!(e, "Unknown profile: claude-b");
    }

    #[test]
    fn empty_profiles_table_is_builtins_not_broken() {
        // An empty `[profiles]` header moves no id, so it is the built-in list, like no table, and starts sessions.
        let env = Env::new();
        env.config("[profiles]\n");
        let (p, warnings) = env.load();
        assert_eq!(p.broken, None);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(p.builtin);
        assert_eq!(ids(&p), ["claude-a", "codex"]);
        assert_eq!(p.usable("claude-a").unwrap().id, "claude-a");
    }

    #[test]
    fn colors_and_chips() {
        assert_eq!(
            PALETTE,
            [
                ("gold", "#e5c07b"),
                ("violet", "#c678dd"),
                ("teal", "#56b6c2"),
                ("green", "#98c379"),
                ("blue", "#61afef"),
                ("red", "#e06c75"),
            ]
        );
        let env = Env::new();
        env.config(
            "[profiles.a]\nkind = \"claude\"\n\
             [profiles.b]\nkind = \"claude\"\ncolor = \"green\"\n\
             [profiles.c]\nkind = \"codex\"\ncolor = \"#123456\"\n\
             [profiles.d]\nkind = \"claude\"\ncolor = \"nope\"\n\
             [profiles.e]\nkind = \"claude\"\n\
             [profiles.f]\nkind = \"codex\"\n\
             [profiles.g]\nkind = \"claude\"\n\
             [profiles.h]\nkind = \"claude\"\ncolor = \"violet\"\n\
             [profiles.i]\nkind = \"claude\"\ncolor = \"#12345\"\n\
             [profiles.personal]\nkind = \"claude\"\nlabel = \"personal\"\n",
        );
        let (p, warnings) = env.load();
        let colors: Vec<String> = infos(&p, &env.state, Some(&env.home))
            .into_iter()
            .map(|i| i.color)
            .collect();
        assert_eq!(
            colors,
            [
                "#e5c07b", // a: position 0
                "#98c379", // b: named
                "#123456", // c: hex is kept
                "#98c379", // d: invalid, so position 3
                "#61afef", // e: position 4
                "#e06c75", // f: position 5
                "#e5c07b", // g: wraps around
                "#c678dd", // h: violet
                "#56b6c2", // i: invalid, so position 8 (wraps to teal)
                "#98c379", // personal: position 9 (green)
            ]
        );
        // The two bad colors each warn once and name the profile.
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("\"d\"")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("\"i\"")), "{warnings:?}");
        // The chip is the first character of the label, uppercased.
        let infos = infos(&p, &env.state, Some(&env.home));
        let chip = |id: &str| infos.iter().find(|i| i.id == id).unwrap().chip.clone();
        assert_eq!(chip("personal"), "P");
        assert_eq!(chip("a"), "A");
        // Clients draw the color as a number; anything that isn't `#rrggbb` is gray.
        let mut info = infos[2].clone();
        assert_eq!(info.rgb(), 0x123456);
        for bad in ["123456", "#12345", "#12345g", "#+12345", "", "#1234567"] {
            info.color = bad.into();
            assert_eq!(info.rgb(), 0x808080, "{bad:?}");
        }
    }

    #[test]
    fn claude_labels_from_login_email() {
        let env = Env::new();
        env.config(
            "[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\"\n\
             [profiles.team]\nkind = \"claude\"\n\
             [profiles.old]\nkind = \"claude\"\nhome = \"~/old-claude\"\n\
             [profiles.fresh]\nkind = \"claude\"\n",
        );
        write(env.home.join(".claude.json"), &login("yb@gmail.com"));
        write(
            env.managed("team").join(".claude.json"),
            &login("other@acme.xyz"),
        );
        write(
            env.home.join("old-claude/.claude.json"),
            &login("yb@acme.xyz"),
        );
        // A profile that hasn't logged in yet is shown by its id.
        let label = |id: &str| env.info(id).label;
        // The same local part on two profiles gets the domain on both, across all Claude profiles.
        assert_eq!(label("claude-a"), "yb@gmail");
        assert_eq!(label("old"), "yb@acme");
        assert_eq!(label("team"), "other");
        assert_eq!(label("fresh"), "fresh");
        // Once the names differ, nothing is expanded.
        write(
            env.home.join("old-claude/.claude.json"),
            &login("zed@acme.xyz"),
        );
        assert_eq!(label("claude-a"), "yb");
        assert_eq!(label("old"), "zed");
        // An alias always wins.
        env.config("[profiles.team]\nkind = \"claude\"\nlabel = \"work\"\n");
        assert_eq!(env.info("team").label, "work");
        // Without the user's home, the default-home profile has no auto name, and nothing else changes.
        env.config("");
        let none = infos(&env.profiles(), &env.state, None);
        let claude_a = none.iter().find(|i| i.id == "claude-a").unwrap();
        assert_eq!(claude_a.label, "claude-a");
        assert!(!claude_a.logged_in);
    }

    #[test]
    fn codex_labels() {
        let env = Env::new();
        env.config(
            "[profiles.codex]\nkind = \"codex\"\nhome = \"default\"\n\
             [profiles.codex-team]\nkind = \"codex\"\n",
        );
        assert_eq!(env.info("codex").label, "Codex");
        assert_eq!(env.info("codex-team").label, "codex-team");
        // An alias always wins, from either place.
        env.config(
            "[profiles.codex]\nkind = \"codex\"\nhome = \"default\"\nlabel = \"gpt\"\n\
             [profiles.codex-team]\nkind = \"codex\"\n\
             [accounts.codex-team]\nlabel = \"team\"\n",
        );
        assert_eq!(env.info("codex").label, "gpt");
        assert_eq!(env.info("codex-team").label, "team");
        // The built-in codex profile is labelled the same way.
        let builtin = Env::new();
        assert_eq!(builtin.info("codex").label, "Codex");
        assert_eq!(builtin.info("codex").chip, "C");
    }

    #[test]
    fn logged_in_is_existence_only() {
        let env = Env::new();
        env.config(
            "[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\"\n\
             [profiles.in]\nkind = \"claude\"\n\
             [profiles.out]\nkind = \"claude\"\n\
             [profiles.blank]\nkind = \"claude\"\n\
             [profiles.codex]\nkind = \"codex\"\nhome = \"default\"\n\
             [profiles.cx]\nkind = \"codex\"\n\
             [profiles.cy]\nkind = \"codex\"\n",
        );
        write(
            env.managed("in").join(".claude.json"),
            &login("yb@gmail.com"),
        );
        write(
            env.managed("out").join(".claude.json"),
            "{\"numStartups\": 1}",
        );
        // Codex: a regular file counts. Its contents are never read.
        write(
            env.managed("cy").join("auth.json"),
            "{\"tokens\": \"secret\"}",
        );
        // Opening a FIFO blocks until the other end opens it, so a read of these would hang the test.
        mkfifo(&env.home.join(".codex/auth.json"));
        mkfifo(&env.managed("cx").join("auth.json"));
        mkfifo(&env.home.join(".claude/.credentials.json"));
        mkfifo(&env.managed("in").join(".credentials.json"));

        let (tx, rx) = mpsc::channel();
        let (p, state, home) = (env.profiles(), env.state.clone(), env.home.clone());
        std::thread::spawn(move || tx.send(infos(&p, &state, Some(&home))).unwrap());
        let infos = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("infos opened a credential file");
        let logged_in = |id: &str| infos.iter().find(|i| i.id == id).unwrap().logged_in;
        assert!(logged_in("in"));
        assert!(!logged_in("out"));
        assert!(!logged_in("blank"));
        assert!(!logged_in("claude-a"));
        assert!(logged_in("codex"));
        assert!(logged_in("cx"));
        assert!(logged_in("cy"));
        assert_eq!(infos.len(), 7);
    }

    #[test]
    fn env_for_profile() {
        let env = Env::new();
        env.config(
            "[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\"\n\
             [profiles.team]\nkind = \"claude\"\n\
             [profiles.codex]\nkind = \"codex\"\nhome = \"default\"\n\
             [profiles.cx]\nkind = \"codex\"\n\
             [profiles.old]\nkind = \"claude\"\nhome = \"/opt/old\"\n\
             [profiles.cold]\nkind = \"codex\"\nhome = \"~/cold\"\n",
        );
        let p = env.profiles();
        let get = |id: &str| p.get(id).unwrap();
        let (state, home) = (&env.state, env.home.as_path());
        // The default home sets nothing: the CLI finds its own.
        assert_eq!(get("claude-a").env(state, home), None);
        assert_eq!(get("codex").env(state, home), None);
        assert_eq!(get("claude-a").home_dir(state, home), home.join(".claude"));
        assert_eq!(
            get("claude-a").claude_json(state, home),
            home.join(".claude.json")
        );
        assert_eq!(get("codex").home_dir(state, home), home.join(".codex"));
        // A managed home is `<state>/profiles/<id>`.
        assert_eq!(
            get("team").env(state, home),
            Some(("CLAUDE_CONFIG_DIR", env.managed("team")))
        );
        assert_eq!(
            get("team").claude_json(state, home),
            env.managed("team").join(".claude.json")
        );
        assert_eq!(
            get("cx").env(state, home),
            Some(("CODEX_HOME", env.managed("cx")))
        );
        // An explicit path is used as is, after `~` expansion.
        assert_eq!(
            get("old").env(state, home),
            Some(("CLAUDE_CONFIG_DIR", PathBuf::from("/opt/old")))
        );
        assert_eq!(
            get("old").claude_json(state, home),
            PathBuf::from("/opt/old/.claude.json")
        );
        assert_eq!(
            get("cold").env(state, home),
            Some(("CODEX_HOME", home.join("cold")))
        );
    }

    #[test]
    fn builtin_flag_says_where_the_list_came_from() {
        let env = Env::new();
        // The built-in list: no file, no table, an empty table, a table with nothing usable, a table of the wrong type, a bad file.
        for text in [
            None,
            Some("[hosts]\nbox = \"ssh\"\n"),
            Some("[profiles]\n"),
            Some("[profiles.a]\nkind = \"claud\"\n"),
            Some("profiles = 3\n"),
            Some("[profiles\n"),
        ] {
            if let Some(text) = text {
                env.config(text);
            }
            let (p, _) = env.load();
            assert!(p.builtin, "{text:?}");
            assert_eq!(ids(&p), ["claude-a", "codex"], "{text:?}");
        }
        // One usable profile is the config's own list, even next to a broken one.
        env.config("[profiles.a]\nkind = \"claud\"\n[profiles.b]\nkind = \"codex\"\n");
        let (p, _) = env.load();
        assert!(!p.builtin);
        assert_eq!(ids(&p), ["b"]);
    }

    #[test]
    fn info_json_shape() {
        let env = Env::new();
        let info = env.info("claude-a");
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(value["kind"], "claude");
        assert_eq!(value["id"], "claude-a");
        assert_eq!(serde_json::from_value::<ProfileInfo>(value).unwrap(), info);
        assert_eq!(serde_json::to_value(ProfileKind::Codex).unwrap(), "codex");
    }
}
