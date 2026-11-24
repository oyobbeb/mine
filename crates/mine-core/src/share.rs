//! Sharing between profiles: links to the default profile's items, MCP server sync, and auth-setting warnings.
//!
//! A profile that shares gets a symlink for each shareable item (`ProfileKind::shareable`) that its `share` setting allows, pointing
//! at the same item in the home of `Profiles::default_of(kind)`. Claude's `mcpServers` can't be linked (they sit in `.claude.json`
//! beside the login), so they are copied in. The links mine made are recorded in `<state>/profiles/<id>.links`, which is how a link
//! that later turned into a real file is told apart from an item that was never linked.
//!
//! Nothing here reads `.credentials.json` or `auth.json`: only the shareable items are looked at, and `.claude.json` is read for its
//! `mcpServers` and written back whole. Values are never logged or put in a message; warnings carry key names only.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::paths::{State, ensure_dir};
use crate::profiles::{Home, Profile, ProfileKind, Profiles, Share};

/// The shareable item that is copied into `.claude.json` instead of linked.
const MCP: &str = "mcpServers";

/// Variables that point Claude at a key instead of the profile's login (design 10.2). A `settings.json` `env` entry for one of
/// these would put every profile that reads the file on that key.
const CLAUDE_BYPASS_ENV: [&str; 6] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
];

/// What one shareable item looks like in a profile's home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// A link to the default profile's item. For `mcpServers`: the servers were copied in, and are copied again at each launch.
    Linked,
    /// A real file or directory that mine never linked: the item was split off on purpose. It is left alone.
    Own,
    /// mine linked it, but it is now a real file. The CLI usually does this by saving through a temp file and a rename.
    /// `relink` puts the link back.
    Split,
    /// A link to something other than the default profile's item. It is left alone.
    Elsewhere(PathBuf),
    /// The default profile has no such item (or, for `mcpServers`, no servers), so there is nothing to share.
    NoSource,
    /// `share` leaves the item out.
    Off,
}

#[derive(Debug, Default)]
pub struct ShareReport {
    /// Every shareable item of the profile's kind, in the kind's order. Empty for a profile that has no links.
    pub items: Vec<(String, LinkState)>,
    /// Auth settings found in the profile's `settings.json` (key names only).
    pub warnings: Vec<String>,
}

/// Where a profile's links point.
struct Plan<'a> {
    home: PathBuf,
    source: &'a Profile,
    source_home: PathBuf,
}

/// `None` when the profile has no links: it keeps the CLI's own home, or it is itself the default of its kind.
fn plan<'a>(
    profile: &Profile,
    profiles: &'a Profiles,
    state: &State,
    user_home: &Path,
) -> Option<Plan<'a>> {
    let source = profiles.default_of(profile.kind)?;
    let (home, source_home) = (
        profile.home_dir(state, user_home),
        source.home_dir(state, user_home),
    );
    (profile.home != Home::Default && home != source_home).then_some(Plan {
        home,
        source,
        source_home,
    })
}

fn wants(profile: &Profile, item: &str) -> bool {
    match &profile.share {
        Share::All => true,
        Share::Off => false,
        Share::Items(items) => items.iter().any(|i| i == item),
    }
}

/// Gets a profile ready for a launch: creates its home (mode 0700), makes the missing links, copies the MCP servers in, and
/// reports what it found. A real file or a link to something else is left alone. A profile that keeps the CLI's own home, or
/// is the default of its kind, has no links: the report is empty and nothing is touched.
///
/// Fails when the home, a link, `.links`, or `.claude.json` can't be written, or when a `.claude.json` that has to be changed
/// isn't JSON (it is never overwritten). The caller decides whether that stops the launch.
pub fn prepare(
    profile: &Profile,
    profiles: &Profiles,
    state: &State,
    user_home: &Path,
) -> Result<ShareReport> {
    let mut report = ShareReport::default();
    let Some(plan) = plan(profile, profiles, state, user_home) else {
        return Ok(report);
    };
    ensure_dir(&plan.home).with_context(|| format!("Couldn't create {}", plan.home.display()))?;
    let mut links = Links::load(state, &profile.id);
    for &name in profile.kind.shareable() {
        let item = if !wants(profile, name) {
            LinkState::Off
        } else if name == MCP {
            let from = plan.source.claude_json(state, user_home);
            if copy_mcp(&from, &profile.claude_json(state, user_home))? {
                LinkState::Linked
            } else {
                LinkState::NoSource
            }
        } else {
            let (dest, from) = (plan.home.join(name), plan.source_home.join(name));
            let item = match probe(&dest, &from, links.has(name))? {
                Some(item) => item,
                None if !from.exists() => LinkState::NoSource,
                None => {
                    symlink(&from, &dest).with_context(|| {
                        format!("Couldn't link {} to {}", dest.display(), from.display())
                    })?;
                    LinkState::Linked
                }
            };
            match item {
                LinkState::Linked => links.add(name),
                // A split item stays recorded, so it keeps showing as split. Anything else isn't ours.
                LinkState::Split => {}
                _ => links.remove(name),
            }
            item
        };
        report.items.push((name.to_string(), item));
    }
    links.save(state, profile.kind)?;
    if profile.kind == ProfileKind::Claude {
        report.warnings = auth_warnings(&plan.home.join("settings.json"));
    }
    Ok(report)
}

/// Puts links back (`mine profile relink <id> [item]`). Returns the items it linked.
///
/// Without `item`, it restores what mine linked and lost (`Split`) and makes missing links, and leaves a real file of the user's
/// own or a link to somewhere else alone. With `item`, it links that item whatever is there. Whatever it replaces is kept as
/// `<item>.mine-bak` (`.mine-bak.2`, ... if that exists already). `mcpServers` isn't a link: it is copied in at each launch.
pub fn relink(
    profile: &Profile,
    profiles: &Profiles,
    state: &State,
    user_home: &Path,
    item: Option<&str>,
) -> Result<Vec<String>> {
    let id = &profile.id;
    let Some(plan) = plan(profile, profiles, state, user_home) else {
        bail!("{id} keeps its own config home, so it has no links to restore");
    };
    let names: Vec<&str> = match item {
        Some(MCP) => bail!("{MCP} is copied in at each launch, so it has no link to restore"),
        Some(item) => {
            if !profile.kind.shareable().contains(&item) {
                bail!("{item} isn't an item that {id} can share");
            }
            if !wants(profile, item) {
                bail!("The share setting of {id} leaves out {item}");
            }
            vec![item]
        }
        None => profile
            .kind
            .shareable()
            .iter()
            .copied()
            .filter(|&name| name != MCP && wants(profile, name))
            .collect(),
    };
    ensure_dir(&plan.home).with_context(|| format!("Couldn't create {}", plan.home.display()))?;
    let mut links = Links::load(state, id);
    let mut done = Vec::new();
    for name in names {
        let (dest, from) = (plan.home.join(name), plan.source_home.join(name));
        if !from.exists() {
            if item.is_some() {
                bail!("The default profile has no {name} to link to");
            }
            continue;
        }
        match probe(&dest, &from, links.has(name))? {
            Some(LinkState::Linked) => continue,
            // The user's own file, or a link they pointed somewhere else: only an explicit item replaces it.
            Some(LinkState::Own | LinkState::Elsewhere(_)) if item.is_none() => continue,
            Some(_) => move_aside(&dest)?,
            None => {}
        }
        symlink(&from, &dest)
            .with_context(|| format!("Couldn't link {} to {}", dest.display(), from.display()))?;
        links.add(name);
        done.push(name.to_string());
    }
    links.save(state, profile.kind)?;
    Ok(done)
}

/// What is at `dest`, without changing anything. `None`: nothing there. `from` is the default profile's item, `recorded`
/// whether mine linked `dest` earlier.
fn probe(dest: &Path, from: &Path, recorded: bool) -> Result<Option<LinkState>> {
    let meta = match fs::symlink_metadata(dest) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Couldn't look at {}", dest.display())),
    };
    if !meta.is_symlink() {
        return Ok(Some(if recorded {
            LinkState::Split
        } else {
            LinkState::Own
        }));
    }
    let target = fs::read_link(dest)
        .with_context(|| format!("Couldn't read the link {}", dest.display()))?;
    // A link that reaches the item through another link is the same link.
    let same = target == from
        || fs::canonicalize(dest)
            .ok()
            .zip(fs::canonicalize(from).ok())
            .is_some_and(|(a, b)| a == b);
    Ok(Some(match (same, from.exists()) {
        (false, _) => LinkState::Elsewhere(target),
        (true, true) => LinkState::Linked,
        // Our link, but the item it pointed to is gone.
        (true, false) => LinkState::NoSource,
    }))
}

/// Renames `path` to `<path>.mine-bak`, or `.mine-bak.2`, `.mine-bak.3`, ... so an earlier backup is never replaced.
fn move_aside(path: &Path) -> Result<()> {
    let with = |suffix: &str| {
        let mut name = OsString::from(path.as_os_str());
        name.push(suffix);
        PathBuf::from(name)
    };
    let mut backup = with(".mine-bak");
    let mut n = 1;
    while fs::symlink_metadata(&backup).is_ok() {
        n += 1;
        backup = with(&format!(".mine-bak.{n}"));
    }
    fs::rename(path, &backup)
        .with_context(|| format!("Couldn't move {} to {}", path.display(), backup.display()))
}

/// `<state>/profiles/<id>.links`: the items mine linked, one name per line.
struct Links {
    file: PathBuf,
    /// The file as it was read, so an unchanged list isn't written again.
    old: String,
    names: HashSet<String>,
}

impl Links {
    fn load(state: &State, id: &str) -> Links {
        let file = state.profiles_dir().join(format!("{id}.links"));
        let old = fs::read_to_string(&file).unwrap_or_default();
        let names = old
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();
        Links { file, old, names }
    }

    fn has(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    fn add(&mut self, name: &str) {
        self.names.insert(name.to_string());
    }

    fn remove(&mut self, name: &str) {
        self.names.remove(name);
    }

    /// Writes the list in the kind's item order, which also drops any name that isn't shareable.
    fn save(&self, state: &State, kind: ProfileKind) -> Result<()> {
        let text: String = kind
            .shareable()
            .iter()
            .filter(|name| self.has(name))
            .map(|name| format!("{name}\n"))
            .collect();
        if text == self.old {
            return Ok(());
        }
        let dir = state.profiles_dir();
        ensure_dir(&dir).with_context(|| format!("Couldn't create {}", dir.display()))?;
        write_atomic(&self.file, text.as_bytes())
    }
}

/// Copies the `mcpServers` object of the default profile's `.claude.json` into the profile's, and writes nothing else of the
/// default's. Servers with the same name are overwritten and the profile's other servers are kept. A missing file on either
/// side is fine: there is nothing to copy, or the profile's file is created with just `mcpServers`.
///
/// The profile's file is written whole, atomically (a temp file in the same directory, then a rename), and only when something
/// changed, with its mode kept (0600 for a new file). It never overwrites a file that isn't a JSON object.
///
/// `serde_json` is built without `preserve_order` for mine-core alone, so the keys of the rewritten file come out sorted unless
/// the workspace build turns the feature on. The data is the same either way.
pub fn sync_mcp(default_json: &Path, profile_json: &Path) -> Result<()> {
    copy_mcp(default_json, profile_json).map(drop)
}

/// [`sync_mcp`], returning whether the default has servers to share.
fn copy_mcp(default_json: &Path, profile_json: &Path) -> Result<bool> {
    let servers = read_json(default_json)?.and_then(|mut config| match config.remove(MCP) {
        Some(Value::Object(servers)) if !servers.is_empty() => Some(servers),
        _ => None,
    });
    let Some(servers) = servers else {
        return Ok(false);
    };
    let mut config = read_json(profile_json)?.unwrap_or_default();
    let before = config.clone();
    match config.get_mut(MCP) {
        Some(Value::Object(own)) => own.extend(servers),
        _ => {
            config.insert(MCP.to_string(), Value::Object(servers));
        }
    }
    if config != before {
        let bytes = serde_json::to_vec_pretty(&config)
            .with_context(|| format!("Couldn't write {}", profile_json.display()))?;
        write_atomic(profile_json, &bytes)?;
    }
    Ok(true)
}

/// A `.claude.json` as a JSON object. `None` for a missing or empty file. Errors name the file and the position, never the content.
fn read_json(path: &Path) -> Result<Option<Map<String, Value>>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Couldn't read {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    match serde_json::from_str(&text) {
        Ok(Value::Object(config)) => Ok(Some(config)),
        Ok(_) => bail!("Couldn't read {}: it isn't a JSON object", path.display()),
        Err(e) => bail!("Couldn't read {}: {e}", path.display()),
    }
}

/// Writes `bytes` to a temp file beside `path` and renames it over `path`. A new file is private (0600); an existing file keeps its mode.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(name);
    let written = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        if let Ok(old) = fs::metadata(path) {
            file.set_permissions(old.permissions())?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written.with_context(|| format!("Couldn't write {}", path.display()))
}

/// Warnings for auth settings in a Claude `settings.json`: `apiKeyHelper`, and any `env` entry on the bypass list (design 10.2).
/// A profile that reads such a file would run on that key instead of its login.
///
/// Only key names are looked at: a warning never holds a value. A file that is missing or isn't JSON gives no warnings.
pub fn auth_warnings(settings_json: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(settings_json) else {
        return Vec::new();
    };
    let Ok(Value::Object(settings)) = serde_json::from_str(&text) else {
        return Vec::new();
    };
    let helper = settings.contains_key("apiKeyHelper").then(|| {
        "settings.json sets apiKeyHelper: sessions would use its key instead of the profile's login"
            .to_string()
    });
    let env = match settings.get("env") {
        Some(Value::Object(env)) => CLAUDE_BYPASS_ENV
            .iter()
            .filter(|key| env.contains_key(**key))
            .map(|key| {
                format!("settings.json sets {key} in env: sessions would use it instead of the profile's login")
            })
            .collect(),
        _ => Vec::new(),
    };
    helper.into_iter().chain(env).collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;

    /// A claude-a (the default home) and a team profile, with `share` as the extra line of `team`.
    fn claude(share: &str) -> String {
        format!(
            "[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\"\n\
             [profiles.team]\nkind = \"claude\"\n{share}\n"
        )
    }

    /// Temporary user home and state directory. The real `~` and `~/.mine` are never touched.
    struct Env {
        _dir: tempfile::TempDir,
        home: PathBuf,
        state: State,
        profiles: Profiles,
    }

    impl Env {
        fn new(config: &str) -> Env {
            let dir = tempfile::Builder::new()
                .prefix("ms")
                .tempdir_in("/tmp")
                .unwrap();
            let home = dir.path().join("home");
            let root = dir.path().join("state");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&root).unwrap();
            let state = State { root };
            let (profiles, warnings) = Profiles::parse(Some(config), &state);
            assert!(warnings.is_empty(), "{warnings:?}");
            Env {
                _dir: dir,
                home,
                state,
                profiles,
            }
        }

        /// Two Claude profiles: the default one with every shareable item, and `team`.
        fn seeded(share: &str) -> Env {
            let env = Env::new(&claude(share));
            let src = env.src();
            for dir in ["skills", "commands", "agents", "plugins"] {
                write(src.join(dir).join("one"), dir);
            }
            write(src.join("CLAUDE.md"), "rules");
            write(src.join("settings.json"), "{\"theme\":\"dark\"}");
            env
        }

        fn profile(&self, id: &str) -> &Profile {
            self.profiles.get(id).unwrap()
        }

        fn prepare(&self, id: &str) -> ShareReport {
            prepare(self.profile(id), &self.profiles, &self.state, &self.home).unwrap()
        }

        fn relink(&self, id: &str, item: Option<&str>) -> Result<Vec<String>> {
            relink(
                self.profile(id),
                &self.profiles,
                &self.state,
                &self.home,
                item,
            )
        }

        /// The default Claude home (`~/.claude`), the source of the links.
        fn src(&self) -> PathBuf {
            self.home.join(".claude")
        }

        /// The managed home of `team`.
        fn team(&self) -> PathBuf {
            self.state.profiles_dir().join("team")
        }

        fn links_file(&self, id: &str) -> PathBuf {
            self.state.profiles_dir().join(format!("{id}.links"))
        }

        fn links(&self, id: &str) -> Vec<String> {
            fs::read_to_string(self.links_file(id))
                .unwrap_or_default()
                .lines()
                .map(String::from)
                .collect()
        }
    }

    fn write(path: PathBuf, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn json_at(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn state_of(report: &ShareReport, name: &str) -> LinkState {
        report
            .items
            .iter()
            .find(|(item, _)| item == name)
            .unwrap_or_else(|| panic!("{name} missing from {:?}", report.items))
            .1
            .clone()
    }

    fn is_link_to(path: &Path, target: &Path) -> bool {
        fs::read_link(path).is_ok_and(|t| t == target)
    }

    fn mkfifo(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(Command::new("mkfifo").arg(path).status().unwrap().success());
    }

    const FILES_AND_DIRS: [&str; 6] = [
        "skills",
        "commands",
        "agents",
        "CLAUDE.md",
        "settings.json",
        "plugins",
    ];

    #[test]
    fn share_all_links_items() {
        let env = Env::seeded("");
        write(
            env.home.join(".claude.json"),
            r#"{"mcpServers":{"a":{"command":"x"}}}"#,
        );
        let report = env.prepare("team");
        for name in FILES_AND_DIRS {
            assert_eq!(state_of(&report, name), LinkState::Linked, "{name}");
            assert!(
                is_link_to(&env.team().join(name), &env.src().join(name)),
                "{name}"
            );
        }
        // The servers were copied in: nothing is linked for `mcpServers`.
        assert_eq!(state_of(&report, "mcpServers"), LinkState::Linked);
        assert_eq!(
            json_at(&env.team().join(".claude.json")),
            json!({"mcpServers": {"a": {"command": "x"}}})
        );
        assert_eq!(report.items.len(), 7);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        // Every link is recorded, and `mcpServers` isn't a link.
        assert_eq!(env.links("team"), FILES_AND_DIRS);
        // The home is private.
        let mode = fs::metadata(env.team()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // A second run changes nothing.
        let again = env.prepare("team");
        assert_eq!(again.items, report.items);
        assert_eq!(env.links("team"), FILES_AND_DIRS);
    }

    #[test]
    fn default_profile_gets_no_links() {
        let env = Env::seeded("");
        let report = env.prepare("claude-a");
        assert!(report.items.is_empty() && report.warnings.is_empty());
        assert!(!env.links_file("claude-a").exists());
        assert_eq!(fs::read_dir(env.src()).unwrap().count(), 6);
        // The profile marked `default = true` is the source, so it has no links either. A profile that keeps the CLI's own
        // home never gets links, even when it isn't the default.
        let env = Env::new(
            "[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\"\n\
             [profiles.team]\nkind = \"claude\"\ndefault = true\n",
        );
        write(env.team().join("skills/one"), "mine");
        for id in ["team", "claude-a"] {
            let report = env.prepare(id);
            assert!(report.items.is_empty(), "{id}");
            assert!(!env.links_file(id).exists(), "{id}");
        }
        assert!(!env.src().exists());
        assert!(!env.team().join("skills").is_symlink());
    }

    #[test]
    fn real_file_is_own_and_untouched() {
        let env = Env::seeded("");
        write(env.team().join("settings.json"), "{\"mine\":true}");
        write(env.team().join("skills/own.md"), "y");
        let report = env.prepare("team");
        assert_eq!(state_of(&report, "settings.json"), LinkState::Own);
        assert_eq!(
            fs::read_to_string(env.team().join("settings.json")).unwrap(),
            "{\"mine\":true}"
        );
        assert!(!env.team().join("settings.json").is_symlink());
        // A real directory is left alone too.
        assert_eq!(state_of(&report, "skills"), LinkState::Own);
        assert!(env.team().join("skills/own.md").is_file());
        assert!(!env.team().join("skills").is_symlink());
        // Neither was linked by mine, so neither is recorded, and the rest are linked as usual.
        assert_eq!(state_of(&report, "commands"), LinkState::Linked);
        assert_eq!(
            env.links("team"),
            ["commands", "agents", "CLAUDE.md", "plugins"]
        );
        assert_eq!(
            state_of(&env.prepare("team"), "settings.json"),
            LinkState::Own
        );
    }

    #[test]
    fn missing_source_item_is_not_linked() {
        let env = Env::seeded("");
        fs::remove_dir_all(env.src().join("agents")).unwrap();
        let report = env.prepare("team");
        assert_eq!(state_of(&report, "agents"), LinkState::NoSource);
        // Nothing, not even a dangling link, was created.
        assert!(fs::symlink_metadata(env.team().join("agents")).is_err());
        assert!(!env.links("team").contains(&"agents".to_string()));
        // No `.claude.json` at the source: no servers to copy, and no file is created.
        assert_eq!(state_of(&report, "mcpServers"), LinkState::NoSource);
        assert!(!env.team().join(".claude.json").exists());
        // The item appears in the default home later: the next run links it.
        write(env.src().join("agents/new.md"), "a");
        assert_eq!(state_of(&env.prepare("team"), "agents"), LinkState::Linked);
    }

    #[test]
    fn link_elsewhere_reported() {
        let env = Env::seeded("");
        let elsewhere = env.home.join("dotfiles/skills");
        write(elsewhere.join("x"), "x");
        fs::create_dir_all(env.team()).unwrap();
        symlink(&elsewhere, env.team().join("skills")).unwrap();
        let report = env.prepare("team");
        assert_eq!(
            state_of(&report, "skills"),
            LinkState::Elsewhere(elsewhere.clone())
        );
        assert!(is_link_to(&env.team().join("skills"), &elsewhere));
        assert!(!env.links("team").contains(&"skills".to_string()));
        // A link that reaches the source through another path is the same link.
        fs::remove_file(env.team().join("skills")).unwrap();
        let moved = env.home.join("moved");
        symlink(env.src().join("skills"), &moved).unwrap();
        symlink(&moved, env.team().join("skills")).unwrap();
        assert_eq!(state_of(&env.prepare("team"), "skills"), LinkState::Linked);
    }

    #[test]
    fn split_after_rename_detected() {
        let env = Env::seeded("");
        env.prepare("team");
        let dest = env.team().join("settings.json");
        assert!(dest.is_symlink());
        // The CLI saved the file through a temp file and a rename: the link is gone.
        fs::remove_file(&dest).unwrap();
        fs::write(&dest, "{\"edited\":1}").unwrap();
        let report = env.prepare("team");
        assert_eq!(state_of(&report, "settings.json"), LinkState::Split);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "{\"edited\":1}");
        assert!(!dest.is_symlink());
        // Still tracked, so it keeps showing as split. The rest of the links are fine.
        assert!(env.links("team").contains(&"settings.json".to_string()));
        assert_eq!(state_of(&report, "skills"), LinkState::Linked);
        assert_eq!(
            state_of(&env.prepare("team"), "settings.json"),
            LinkState::Split
        );
    }

    #[test]
    fn relink_restores_and_backs_up() {
        let env = Env::seeded("");
        env.prepare("team");
        let dest = env.team().join("settings.json");
        let split = |text: &str| {
            fs::remove_file(&dest).ok();
            fs::write(&dest, text).unwrap();
        };
        split("one");
        assert_eq!(
            state_of(&env.prepare("team"), "settings.json"),
            LinkState::Split
        );
        let done = env.relink("team", Some("settings.json")).unwrap();
        assert_eq!(done, ["settings.json"]);
        assert!(is_link_to(&dest, &env.src().join("settings.json")));
        assert_eq!(
            fs::read_to_string(env.team().join("settings.json.mine-bak")).unwrap(),
            "one"
        );
        assert_eq!(
            state_of(&env.prepare("team"), "settings.json"),
            LinkState::Linked
        );
        // The source file is untouched.
        assert_eq!(
            fs::read_to_string(env.src().join("settings.json")).unwrap(),
            "{\"theme\":\"dark\"}"
        );
        // A second split keeps the first backup.
        fs::remove_file(&dest).unwrap();
        split("two");
        env.relink("team", Some("settings.json")).unwrap();
        assert_eq!(
            fs::read_to_string(env.team().join("settings.json.mine-bak")).unwrap(),
            "one"
        );
        assert_eq!(
            fs::read_to_string(env.team().join("settings.json.mine-bak.2")).unwrap(),
            "two"
        );
    }

    #[test]
    fn relink_without_item_only_restores_splits() {
        let env = Env::seeded("");
        let team = env.team();
        write(team.join("skills/own.md"), "own");
        env.prepare("team");
        // Split: restored. Own and elsewhere: the user's choice, left alone. Missing: linked.
        fs::remove_file(team.join("settings.json")).unwrap();
        fs::write(team.join("settings.json"), "edited").unwrap();
        fs::remove_file(team.join("commands")).unwrap();
        symlink(env.home.join("elsewhere"), team.join("commands")).unwrap();
        fs::remove_file(team.join("CLAUDE.md")).unwrap();
        let mut done = env.relink("team", None).unwrap();
        done.sort();
        assert_eq!(done, ["CLAUDE.md", "settings.json"]);
        assert!(is_link_to(
            &team.join("settings.json"),
            &env.src().join("settings.json")
        ));
        assert!(team.join("skills/own.md").is_file());
        assert!(is_link_to(
            &team.join("commands"),
            &env.home.join("elsewhere")
        ));
        // Naming an item forces it, and keeps what was there.
        assert_eq!(env.relink("team", Some("skills")).unwrap(), ["skills"]);
        assert!(is_link_to(&team.join("skills"), &env.src().join("skills")));
        assert_eq!(
            fs::read_to_string(team.join("skills.mine-bak/own.md")).unwrap(),
            "own"
        );
        // Mistakes are errors, not silent no-ops.
        assert!(env.relink("claude-a", None).is_err());
        assert!(env.relink("team", Some(".credentials.json")).is_err());
        assert!(env.relink("team", Some("mcpServers")).is_err());
        fs::remove_dir_all(env.src().join("agents")).unwrap();
        assert!(env.relink("team", Some("agents")).is_err());
        // With `share` narrowed, an item left out can't be relinked.
        let env = Env::seeded("share = [\"skills\"]");
        assert!(env.relink("team", Some("commands")).is_err());
        assert_eq!(env.relink("team", None).unwrap(), ["skills"]);
    }

    #[test]
    fn share_items_subset() {
        let env = Env::seeded("share = [\"skills\", \"settings.json\"]");
        let report = env.prepare("team");
        for name in ["skills", "settings.json"] {
            assert_eq!(state_of(&report, name), LinkState::Linked, "{name}");
        }
        for name in ["commands", "agents", "CLAUDE.md", "plugins", "mcpServers"] {
            assert_eq!(state_of(&report, name), LinkState::Off, "{name}");
            assert!(
                fs::symlink_metadata(env.team().join(name)).is_err(),
                "{name}"
            );
        }
        assert_eq!(env.links("team"), ["skills", "settings.json"]);
    }

    #[test]
    fn share_off_no_links() {
        let env = Env::seeded("share = false");
        let report = env.prepare("team");
        assert_eq!(report.items.len(), 7);
        assert!(report.items.iter().all(|(_, s)| *s == LinkState::Off));
        // The home still exists for the session to use.
        assert!(env.team().is_dir());
        assert_eq!(fs::read_dir(env.team()).unwrap().count(), 0);
        assert!(!env.links_file("team").exists());
        // Narrowing `share` later leaves the links that exist alone, and says they're off.
        let env = Env::seeded("");
        env.prepare("team");
        let narrowed = Profile {
            share: crate::profiles::Share::Off,
            ..env.profile("team").clone()
        };
        let report = prepare(&narrowed, &env.profiles, &env.state, &env.home).unwrap();
        assert!(report.items.iter().all(|(_, s)| *s == LinkState::Off));
        assert!(env.team().join("skills").is_symlink());
        assert_eq!(env.links("team"), FILES_AND_DIRS);
    }

    #[test]
    fn codex_links_without_mcp() {
        let env = Env::new(
            "[profiles.codex]\nkind = \"codex\"\nhome = \"default\"\n\
             [profiles.cx]\nkind = \"codex\"\n",
        );
        let src = env.home.join(".codex");
        write(src.join("AGENTS.md"), "a");
        write(src.join("config.toml"), "model = \"x\"\n");
        let report = env.prepare("cx");
        let names: Vec<&str> = report.items.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["AGENTS.md", "prompts", "config.toml"]);
        assert_eq!(state_of(&report, "AGENTS.md"), LinkState::Linked);
        assert_eq!(state_of(&report, "prompts"), LinkState::NoSource);
        assert_eq!(state_of(&report, "config.toml"), LinkState::Linked);
        let cx = env.state.profiles_dir().join("cx");
        assert!(is_link_to(&cx.join("AGENTS.md"), &src.join("AGENTS.md")));
        assert!(!cx.join(".claude.json").exists());
        assert!(env.prepare("codex").items.is_empty());
    }

    #[test]
    fn mcp_sync_copies_only_mcpservers() {
        let dir = tempfile::Builder::new()
            .prefix("ms")
            .tempdir_in("/tmp")
            .unwrap();
        let (default, profile) = (dir.path().join("default.json"), dir.path().join("p.json"));
        fs::write(
            &default,
            r#"{"mcpServers":{"a":{"command":"x"}},"oauthAccount":{"emailAddress":"d@x"}}"#,
        )
        .unwrap();
        fs::write(
            &profile,
            r#"{"oauthAccount":{"emailAddress":"p@x"},"mcpServers":{"a":{"command":"old"},"b":{"command":"y"}},"numStartups":7,"nested":{"k":[1,2.5,null,"s"]}}"#,
        )
        .unwrap();
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o600)).unwrap();
        sync_mcp(&default, &profile).unwrap();
        let after = json_at(&profile);
        assert_eq!(after["oauthAccount"]["emailAddress"], "p@x");
        assert_eq!(
            after["mcpServers"],
            json!({"a": {"command": "x"}, "b": {"command": "y"}})
        );
        // Every other field is as it was.
        let mut rest = after.clone();
        rest.as_object_mut().unwrap().remove("mcpServers");
        assert_eq!(
            rest,
            json!({
                "oauthAccount": {"emailAddress": "p@x"},
                "numStartups": 7,
                "nested": {"k": [1, 2.5, null, "s"]},
            })
        );
        // The write went through a temp file in the same directory, which is gone, and kept the file's mode.
        let mut names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["default.json", "p.json"]);
        assert_eq!(
            fs::metadata(&profile).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Nothing to change: the file isn't rewritten.
        let inode = fs::metadata(&profile).unwrap().ino();
        sync_mcp(&default, &profile).unwrap();
        assert_eq!(fs::metadata(&profile).unwrap().ino(), inode);
        assert_eq!(json_at(&profile), after);
        // A profile with no `.claude.json` gets one with just the servers, private to the user.
        let fresh = dir.path().join("fresh.json");
        sync_mcp(&default, &fresh).unwrap();
        assert_eq!(
            json_at(&fresh),
            json!({"mcpServers": {"a": {"command": "x"}}})
        );
        assert_eq!(
            fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // No source file, or one without servers: nothing happens, and nothing is created.
        let (none, other) = (dir.path().join("none.json"), dir.path().join("other.json"));
        sync_mcp(&none, &other).unwrap();
        assert!(!other.exists());
        fs::write(&none, r#"{"mcpServers":{}}"#).unwrap();
        sync_mcp(&none, &other).unwrap();
        assert!(!other.exists());
        // A file that isn't JSON is never overwritten.
        fs::write(&other, "{not json").unwrap();
        assert!(sync_mcp(&default, &other).is_err());
        assert_eq!(fs::read_to_string(&other).unwrap(), "{not json");
    }

    #[test]
    fn mcp_sync_skipped_when_share_excludes() {
        let env = Env::seeded("share = [\"skills\"]");
        write(
            env.home.join(".claude.json"),
            r#"{"mcpServers":{"a":{"command":"x"}}}"#,
        );
        let own = r#"{"oauthAccount":{"emailAddress":"p@x"}}"#;
        write(env.team().join(".claude.json"), own);
        let report = env.prepare("team");
        assert_eq!(state_of(&report, "mcpServers"), LinkState::Off);
        assert_eq!(
            fs::read_to_string(env.team().join(".claude.json")).unwrap(),
            own
        );
    }

    #[test]
    fn never_opens_credentials() {
        let env = Env::new(
            "[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\"\n\
             [profiles.team]\nkind = \"claude\"\n\
             [profiles.codex]\nkind = \"codex\"\nhome = \"default\"\n\
             [profiles.cx]\nkind = \"codex\"\n",
        );
        write(env.src().join("CLAUDE.md"), "rules");
        write(env.home.join(".codex/AGENTS.md"), "rules");
        // Opening a FIFO blocks until the other end opens it, so a read of any of these would hang the test.
        mkfifo(&env.src().join(".credentials.json"));
        mkfifo(&env.team().join(".credentials.json"));
        mkfifo(&env.home.join(".codex/auth.json"));
        mkfifo(&env.state.profiles_dir().join("cx/auth.json"));

        let (tx, rx) = mpsc::channel();
        let (profiles, state, home) = (env.profiles.clone(), env.state.clone(), env.home.clone());
        std::thread::spawn(move || {
            let run = |id: &str| prepare(profiles.get(id).unwrap(), &profiles, &state, &home);
            tx.send(vec![run("team").unwrap(), run("cx").unwrap()])
                .unwrap();
        });
        let reports = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("prepare opened a credential file");
        for report in &reports {
            let text = format!("{:?}", (&report.items, &report.warnings));
            assert!(
                !text.contains("credentials") && !text.contains("auth.json"),
                "{text}"
            );
        }
        for id in ["team", "cx"] {
            let links = fs::read_to_string(env.links_file(id)).unwrap();
            assert!(!links.contains("credentials") && !links.contains("auth.json"));
        }
        assert_eq!(env.links("team"), ["CLAUDE.md"]);
        assert_eq!(env.links("cx"), ["AGENTS.md"]);
    }

    #[test]
    fn auth_warnings_by_key_name() {
        let dir = tempfile::Builder::new()
            .prefix("ms")
            .tempdir_in("/tmp")
            .unwrap();
        let settings = dir.path().join("settings.json");
        fs::write(
            &settings,
            r#"{"apiKeyHelper":"/bin/k","env":{"ANTHROPIC_API_KEY":"sk-secret","EDITOR":"vi"},"theme":"dark"}"#,
        )
        .unwrap();
        let warnings = auth_warnings(&settings);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("apiKeyHelper")));
        assert!(warnings.iter().any(|w| w.contains("ANTHROPIC_API_KEY")));
        for w in &warnings {
            assert!(!w.contains("sk-secret") && !w.contains("/bin/k"), "{w}");
        }
        // Each variable on the bypass list warns once, by name.
        fs::write(
            &settings,
            r#"{"env":{"CLAUDE_CODE_USE_BEDROCK":"","CLAUDE_CODE_OAUTH_TOKEN":"t","OPENAI_API_KEY":"x"}}"#,
        )
        .unwrap();
        assert_eq!(auth_warnings(&settings).len(), 2);
        // Nothing to warn about, or nothing to read: no warnings.
        for text in [
            r#"{"theme":"dark","env":{"EDITOR":"vi"}}"#,
            r#"{"env":"ANTHROPIC_API_KEY"}"#,
            "{not json",
            "[]",
            "",
        ] {
            fs::write(&settings, text).unwrap();
            assert!(auth_warnings(&settings).is_empty(), "{text:?}");
        }
        assert!(auth_warnings(&dir.path().join("missing.json")).is_empty());
    }

    #[test]
    fn prepare_reports_auth_warnings_of_the_effective_file() {
        let env = Env::seeded("");
        write(
            env.src().join("settings.json"),
            r#"{"apiKeyHelper":"/bin/k"}"#,
        );
        // Shared: the team profile reads the default's file.
        let report = env.prepare("team");
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(!report.warnings[0].contains("/bin/k"));
        // Its own file is what counts once it is split off.
        fs::remove_file(env.team().join("settings.json")).unwrap();
        write(env.team().join("settings.json"), "{}");
        assert!(env.prepare("team").warnings.is_empty());
    }
}
