//! `mine profile`: list, add, log in, relink, and remove profiles.
//!
//! `add` and `rm` edit `<state>/config.toml` in place with `toml_edit`, so comments and the order of everything else survive. They
//! write the whole file atomically, and they tell a running daemon to reread it ([`Conn::reload_profiles`]). None of these commands
//! starts a daemon. Nothing here opens `.credentials.json`, the Keychain, or Codex's `auth.json`, and nothing deletes a profile's home.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use mine_core::accounts;
use mine_core::conn::Conn;
use mine_core::paths::{State, ensure_dir};
use mine_core::profiles::{self, Home, Profile, ProfileKind, Profiles, Share};
use mine_core::session::{self, Kind, validate_name};
use mine_core::share::{self, LinkState, write_atomic};
use toml_edit::{Array, DocumentMut, Item, Table, value};

use crate::field;

#[derive(Subcommand)]
pub enum Cmd {
    /// List the profiles. Each line is tab-separated: `id kind label home logged-in share`.
    Ls,
    /// Add a profile to config.toml and set up its home.
    Add(AddArgs),
    /// Log in to a profile: runs the CLI's login with the profile's config home, in this terminal.
    Login {
        /// Profile id.
        id: String,
    },
    /// Put shared links back that a CLI replaced with a real file. The replaced file is kept as `<item>.mine-bak`.
    Relink {
        /// Profile id.
        id: String,
        /// One item, such as `settings.json`. Without it, every item mine linked and lost is restored.
        item: Option<String>,
    },
    /// Remove a profile from config.toml. Its home directory is kept.
    Rm {
        /// Profile id.
        id: String,
    },
}

#[derive(clap::Args)]
pub struct AddArgs {
    /// Profile id: the stable key sessions store. Use `--label` for a display name.
    id: String,
    #[arg(long, value_parser = ["claude", "codex"])]
    kind: String,
    /// Name to show instead of the login's.
    #[arg(long)]
    label: Option<String>,
    /// `default` for the CLI's own home, or an existing directory. Without it, mine makes `<state>/profiles/<id>`.
    #[arg(long)]
    home: Option<String>,
    /// Share only these items with the default profile, separated by commas.
    #[arg(long, value_delimiter = ',', conflicts_with = "no_share")]
    share: Option<Vec<String>>,
    /// Share nothing.
    #[arg(long)]
    no_share: bool,
}

pub fn run(cmd: Cmd) -> Result<()> {
    let state = State::from_env()?;
    let user_home = std::env::home_dir().context("Couldn't determine the home directory")?;
    match cmd {
        Cmd::Ls => ls(&state, &user_home),
        Cmd::Add(args) => add(&state, &user_home, args),
        Cmd::Login { id } => login(&state, &user_home, &id),
        Cmd::Relink { id, item } => relink(&state, &user_home, &id, item.as_deref()),
        Cmd::Rm { id } => rm(&state, &id),
    }
}

/// The profiles, with the load warnings on stderr.
fn load(state: &State) -> Profiles {
    let (profiles, warnings) = Profiles::load(state);
    warnings
        .iter()
        .for_each(|warning| eprintln!("Warning: {warning}"));
    profiles
}

fn find<'a>(profiles: &'a Profiles, id: &str) -> Result<&'a Profile> {
    profiles
        .get(id)
        .ok_or_else(|| anyhow!("Unknown profile: {id}"))
}

/// Tells a running daemon to reread the profiles. If that fails the command still did its work: the daemon keeps the old list until
/// it is told again or restarted.
fn notify_daemon(state: &State) {
    if let Err(e) = Conn::reload_profiles(state) {
        eprintln!("Warning: couldn't tell the running daemon to reload the profiles: {e:#}");
    }
}

fn tilde(path: &Path, user_home: &Path) -> String {
    match path.strip_prefix(user_home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn ls(state: &State, user_home: &Path) -> Result<()> {
    let profiles = load(state);
    let infos = profiles::infos(&profiles, state, Some(user_home));
    for (profile, info) in profiles.list.iter().zip(&infos) {
        let share = match share::inspect(profile, &profiles, state, user_home) {
            Ok(report) => {
                for warning in &report.warnings {
                    eprintln!("Warning: {}: {warning}", profile.id);
                }
                share_column(&report.items)
            }
            Err(e) => {
                eprintln!("Warning: {}: {e:#}", profile.id);
                "?".to_string()
            }
        };
        let row: [&str; 6] = [
            &info.id,
            profile.kind.word(),
            &info.label,
            &tilde(&profile.home_dir(state, user_home), user_home),
            if info.logged_in { "yes" } else { "no" },
            &share,
        ];
        println!("{}", row.map(field).join("\t"));
    }
    Ok(())
}

/// `linked:N own:a,b split:c`: what is linked, and which items need a look. `-` for a profile with no links, `off` for one that
/// shares nothing. Items with nothing to share, and items left out of `share`, don't show.
fn share_column(items: &[(String, LinkState)]) -> String {
    if items.is_empty() {
        return "-".to_string();
    }
    if items.iter().all(|(_, item)| *item == LinkState::Off) {
        return "off".to_string();
    }
    let group = |item: &LinkState| match item {
        LinkState::Linked => "linked",
        LinkState::Own => "own",
        LinkState::Split => "split",
        LinkState::Elsewhere(_) => "elsewhere",
        LinkState::Missing => "missing",
        LinkState::NoSource | LinkState::Off => "",
    };
    let linked = items.iter().filter(|(_, s)| group(s) == "linked").count();
    let mut parts = vec![format!("linked:{linked}")];
    for label in ["own", "split", "elsewhere", "missing"] {
        let names: Vec<&str> = (items.iter())
            .filter(|(_, s)| group(s) == label)
            .map(|(name, _)| name.as_str())
            .collect();
        if !names.is_empty() {
            parts.push(format!("{label}:{}", names.join(",")));
        }
    }
    parts.join(" ")
}

fn config_path(state: &State) -> PathBuf {
    state.root.join("config.toml")
}

/// `config.toml` as an editable document. A missing file is an empty one. A file that isn't TOML is an error: it is never rewritten.
fn read_doc(state: &State) -> Result<DocumentMut> {
    let text = match std::fs::read_to_string(config_path(state)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).context("Couldn't read config.toml"),
    };
    text.parse().map_err(|e: toml_edit::TomlError| {
        let upto = e.span().map_or(0, |span| span.start.min(text.len()));
        let line = 1 + text.as_bytes()[..upto]
            .iter()
            .filter(|&&b| b == b'\n')
            .count();
        anyhow!(
            "Couldn't edit config.toml. Fix line {line} first: {}",
            e.message()
        )
    })
}

fn write_doc(state: &State, doc: &DocumentMut) -> Result<()> {
    ensure_dir(&state.root).with_context(|| format!("Couldn't create {}", state.root.display()))?;
    write_atomic(&config_path(state), doc.to_string().as_bytes())
}

/// The `[profiles]` table of `doc`. When the list in use is the built-in one (no table, an empty one, or one with no usable
/// profile), the built-ins are written into it first, so adding or removing a profile never makes the others disappear. An unusable
/// entry the user wrote is left as it is; if it has the id of a built-in, that is an error, since the built-in can't take its place
/// without losing what they wrote.
fn profiles_table<'a>(doc: &'a mut DocumentMut, current: &Profiles) -> Result<&'a mut Table> {
    let fix = "Couldn't edit config.toml: profiles isn't a [profiles.<id>] table. Fix it first";
    if !doc.contains_key("profiles") {
        let mut table = Table::new();
        // No `[profiles]` line of its own: only `[profiles.<id>]` headers.
        table.set_implicit(true);
        doc.insert("profiles", Item::Table(table));
    }
    let table = doc["profiles"].as_table_mut().ok_or_else(|| anyhow!(fix))?;
    if current.builtin {
        if let Some(clash) = current.list.iter().find(|p| table.contains_key(&p.id)) {
            bail!(
                "Couldn't edit config.toml: [profiles.{0}] isn't usable, so the built-in {0} is in use. Fix it first",
                clash.id
            );
        }
        for profile in &current.list {
            table.insert(&profile.id, Item::Table(profile_table(profile)));
        }
    }
    Ok(table)
}

/// `[profiles.<id>]` with the keys a profile sets. The defaults (managed home, sharing everything, no color) are left out.
fn profile_table(profile: &Profile) -> Table {
    let mut table = Table::new();
    table["kind"] = value(profile.kind.word());
    match &profile.home {
        Home::Default => table["home"] = value("default"),
        Home::Path(path) => table["home"] = value(path.to_string_lossy().as_ref()),
        Home::Managed => {}
    }
    if let Some(label) = &profile.label {
        table["label"] = value(label.as_str());
    }
    if let Some(color) = &profile.color {
        table["color"] = value(color.as_str());
    }
    match &profile.share {
        Share::All => {}
        Share::Off => table["share"] = value(false),
        Share::Items(items) => {
            table["share"] = value(items.iter().map(String::as_str).collect::<Array>())
        }
    }
    if profile.default {
        table["default"] = value(true);
    }
    table
}

fn add(state: &State, user_home: &Path, args: AddArgs) -> Result<()> {
    let AddArgs {
        id,
        kind,
        label,
        home,
        share,
        no_share,
    } = args;
    validate_name(&id).map_err(|_| {
        anyhow!(
            "Invalid profile id: {id:?} (start with a letter or digit, then use only letters, digits, _, ~ and -, up to 64 characters)"
        )
    })?;
    if id == "shell" {
        bail!("Invalid profile id: shell is reserved for plain shell sessions");
    }
    let kind = match kind.as_str() {
        "claude" => ProfileKind::Claude,
        _ => ProfileKind::Codex,
    };
    let label = label
        .map(|text| {
            accounts::clean(&text)
                .ok_or_else(|| anyhow!("Invalid label: use text without control characters"))
        })
        .transpose()?;
    let share = match (share, no_share) {
        (_, true) => Share::Off,
        (None, false) => Share::All,
        (Some(items), false) => {
            if let Some(item) = items
                .iter()
                .find(|i| !kind.shareable().contains(&i.as_str()))
            {
                bail!(
                    "{} profiles can't share {item:?}. They can share: {}",
                    kind.word(),
                    kind.shareable().join(", ")
                );
            }
            Share::Items(items)
        }
    };
    let current = load(state);
    if current.get(&id).is_some() {
        bail!("Profile {id} already exists");
    }
    let home = match home.as_deref() {
        None => Home::Managed,
        Some("default") => {
            if let Some(other) = current
                .list
                .iter()
                .find(|p| p.kind == kind && p.home == Home::Default)
            {
                bail!(
                    "{} already uses the default home of {}. Only one profile can",
                    other.id,
                    kind.word()
                );
            }
            Home::Default
        }
        Some(path) => existing_dir(path, user_home)?,
    };
    let profile = Profile {
        id: id.clone(),
        kind,
        home,
        label,
        color: None,
        share,
        default: false,
    };
    let mut doc = read_doc(state)?;
    let table = profiles_table(&mut doc, &current)?;
    if table.contains_key(&id) {
        bail!("Profile {id} already exists");
    }
    table.insert(&id, Item::Table(profile_table(&profile)));
    // The profile as the daemon will read it, for the home and the links. If the file wouldn't keep it, nothing is written.
    let (after, _) = Profiles::parse(Some(&doc.to_string()), state);
    let profile = after
        .get(&id)
        .ok_or_else(|| anyhow!("Couldn't add {id}: config.toml would not keep it"))?;
    let home = profile.home_dir(state, user_home);
    if profile.home == Home::Managed {
        ensure_dir(&home).with_context(|| format!("Couldn't create {}", home.display()))?;
    }
    write_doc(state, &doc)?;
    println!(
        "Added {id} ({}), home {}",
        kind.word(),
        tilde(&home, user_home)
    );
    // The profile is saved: a problem with its links is reported, and the next launch tries again.
    match share::prepare(profile, &after, state, user_home) {
        Ok(report) => (report.warnings.iter().chain(&report.errors))
            .for_each(|problem| eprintln!("Warning: {id}: {problem}")),
        Err(e) => eprintln!("Warning: {id}: {e:#}"),
    }
    println!("Log in with: mine profile login {id}");
    notify_daemon(state);
    Ok(())
}

/// `--home PATH`: an existing directory, as an absolute path or one starting with `~/` (what `config.toml` accepts).
fn existing_dir(path: &str, user_home: &Path) -> Result<Home> {
    let home = if path == "~" || path.starts_with("~/") {
        PathBuf::from(path)
    } else {
        std::path::absolute(path).with_context(|| format!("Couldn't resolve {path}"))?
    };
    let expanded = match home.strip_prefix("~") {
        Ok(rest) => user_home.join(rest),
        Err(_) => home.clone(),
    };
    if !expanded.is_dir() {
        bail!("{} isn't an existing directory", home.display());
    }
    Ok(Home::Path(home))
}

fn rm(state: &State, id: &str) -> Result<()> {
    let current = load(state);
    let profile = find(&current, id)?;
    if current.list.len() == 1 {
        bail!("{id} is the last profile. Add another before removing it");
    }
    let home = profile.home_dir(state, &std::env::home_dir().unwrap_or_default());
    let mut doc = read_doc(state)?;
    profiles_table(&mut doc, &current)?.remove(id);
    write_doc(state, &doc)?;
    println!("Removed {id}");
    println!("Kept {}", home.display());
    notify_daemon(state);
    Ok(())
}

fn relink(state: &State, user_home: &Path, id: &str, item: Option<&str>) -> Result<()> {
    let profiles = load(state);
    let profile = profiles.usable(id)?;
    let done = share::relink(profile, &profiles, state, user_home, item)?;
    if done.is_empty() {
        println!("Nothing to relink for {id}");
    }
    for name in &done {
        println!("Relinked {name}");
    }
    if !done.is_empty() {
        println!("What was there is kept as <item>.mine-bak");
    }
    notify_daemon(state);
    Ok(())
}

/// Runs the CLI's login with the profile's config home, in this terminal, and waits for it. Claude has `claude auth login` for this
/// (it exits when the login is done); Codex has `codex login`. The environment is the one a session gets: what the shell exported
/// for another login (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, an API key) is dropped, and the profile's own home is set.
fn login(state: &State, user_home: &Path, id: &str) -> Result<()> {
    let profiles = load(state);
    let profile = profiles.usable(id)?;
    let (program, args): (&str, &[&str]) = match profile.kind {
        ProfileKind::Claude => ("claude", &["auth", "login"]),
        ProfileKind::Codex => ("codex", &["login"]),
    };
    let mut cmd = Command::new(program);
    cmd.args(args);
    for key in session::env_to_remove(Kind::from(profile.kind)) {
        cmd.env_remove(key);
    }
    if let Some((key, home)) = profile.env(state, user_home) {
        ensure_dir(&home).with_context(|| format!("Couldn't create {}", home.display()))?;
        cmd.env(key, home);
    }
    let status = cmd
        .status()
        .with_context(|| format!("Couldn't run {program}. Is it installed and on your PATH?"))?;
    // Whether it worked or not, the profile may be logged in now: the clients show the new state.
    notify_daemon(state);
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
