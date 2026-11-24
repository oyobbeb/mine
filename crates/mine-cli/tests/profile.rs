//! Integration test for `mine profile`. Runs the real binary with a temporary state directory and `HOME`, and a fake `claude` and
//! `codex` at the front of `PATH`. No daemon is running, which is also what shows that these commands never start one.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use mine_core::paths::State;
use mine_core::profiles::{Home, Profiles};

/// A temporary state directory, `HOME`, and bin directory. The real `~` and `~/.mine` are never touched.
struct Env {
    _dir: tempfile::TempDir,
    state: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::Builder::new()
            .prefix("mp")
            .tempdir_in("/tmp")
            .unwrap();
        let (state, home, bin) = (
            dir.path().join("s"),
            dir.path().join("h"),
            dir.path().join("b"),
        );
        for path in [&state, &home, &bin] {
            fs::create_dir(path).unwrap();
        }
        Env {
            _dir: dir,
            state,
            home,
            bin,
        }
    }

    fn config_path(&self) -> PathBuf {
        self.state.join("config.toml")
    }

    fn config(&self) -> String {
        fs::read_to_string(self.config_path()).unwrap()
    }

    fn set_config(&self, text: &str) {
        fs::write(self.config_path(), text).unwrap();
    }

    fn profiles_dir(&self) -> PathBuf {
        self.state.join("profiles")
    }

    /// The profiles `config.toml` holds now, with the warnings that came with them (there must be none).
    fn profiles(&self) -> Profiles {
        let state = State {
            root: self.state.clone(),
        };
        let (profiles, warnings) = Profiles::parse(Some(&self.config()), &state);
        assert!(warnings.is_empty(), "{warnings:?}\n{}", self.config());
        profiles
    }

    fn ids(&self) -> Vec<String> {
        self.profiles().list.into_iter().map(|p| p.id).collect()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mine"))
            .arg("profile")
            .args(args)
            .env("MINE_STATE_DIR", &self.state)
            .env("HOME", &self.home)
            .env("PATH", self.path())
            .stdin(Stdio::null())
            .output()
            .expect("couldn't run mine")
    }

    /// Runs `mine profile <args>`, checks that it succeeded, and returns stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "mine profile {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Runs `mine profile <args>`, checks that it failed, and returns stderr.
    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            !out.status.success(),
            "mine profile {args:?} should have failed: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8(out.stderr).unwrap()
    }

    fn path(&self) -> std::ffi::OsString {
        let mut path = self.bin.clone().into_os_string();
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        path
    }

    /// A fake `name` CLI that records its arguments and environment next to itself, then exits with `code`.
    fn fake_cli(&self, name: &str, code: i32) {
        let path = self.bin.join(name);
        let (argv, env) = (self.bin.join(format!("{name}.argv")), self.dump(name));
        let script = format!(
            "#!/bin/sh\necho \"$@\" > '{}'\nenv > '{}'\nexit {code}\n",
            argv.display(),
            env.display()
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn dump(&self, name: &str) -> PathBuf {
        self.bin.join(format!("{name}.env"))
    }

    fn argv(&self, name: &str) -> String {
        fs::read_to_string(self.bin.join(format!("{name}.argv")))
            .unwrap()
            .trim()
            .to_string()
    }

    fn env_of(&self, name: &str) -> Vec<String> {
        fs::read_to_string(self.dump(name))
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn profile_add_materializes_builtins_first() {
    let env = Env::new();
    fs::create_dir_all(env.profiles_dir().join("claude-b")).unwrap();
    env.set_config("[accounts.claude-b]\nlabel = \"work\"\n");
    env.ok(&["add", "team", "--kind", "claude"]);
    assert_eq!(env.ids(), ["claude-a", "claude-b", "codex", "team"]);
    let profiles = env.profiles();
    // The built-in profiles keep their homes and their labels, so nothing changed for the sessions that use them.
    assert_eq!(profiles.get("claude-a").unwrap().home, Home::Default);
    assert_eq!(profiles.get("codex").unwrap().home, Home::Default);
    let b = profiles.get("claude-b").unwrap();
    assert_eq!(
        (&b.home, b.label.as_deref()),
        (&Home::Managed, Some("work"))
    );
    assert_eq!(profiles.get("team").unwrap().home, Home::Managed);
    // The managed home exists and is private.
    let mode = fs::metadata(env.profiles_dir().join("team"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
    // Nothing started a daemon.
    assert!(!env.state.join("daemon.pid").exists());
    assert!(!env.state.join("daemon.sock").exists());
}

#[test]
fn profile_add_writes_the_options() {
    let env = Env::new();
    let other = env.home.join("older");
    fs::create_dir(&other).unwrap();
    env.ok(&[
        "add",
        "a",
        "--kind",
        "codex",
        "--label",
        "Side",
        "--no-share",
    ]);
    env.ok(&["add", "b", "--kind", "claude", "--share", "skills,agents"]);
    env.ok(&[
        "add",
        "c",
        "--kind",
        "claude",
        "--home",
        other.to_str().unwrap(),
    ]);
    let profiles = env.profiles();
    let a = profiles.get("a").unwrap();
    assert_eq!(
        (a.label.as_deref(), &a.share),
        (Some("Side"), &mine_core::profiles::Share::Off)
    );
    assert_eq!(
        profiles.get("b").unwrap().share,
        mine_core::profiles::Share::Items(vec!["skills".into(), "agents".into()])
    );
    assert_eq!(profiles.get("c").unwrap().home, Home::Path(other));
    // A second claude profile can't take the CLI's default home, and an item has to be shareable.
    let why = env.fails(&["add", "d", "--kind", "claude", "--home", "default"]);
    assert!(why.contains("already uses"), "{why}");
    let why = env.fails(&["add", "d", "--kind", "codex", "--share", "skills"]);
    assert!(why.contains("skills"), "{why}");
    let why = env.fails(&["add", "d", "--kind", "claude", "--home", "/nope/nowhere"]);
    assert!(why.contains("/nope/nowhere"), "{why}");
    assert!(!env.ids().contains(&"d".to_string()));
}

#[test]
fn profile_add_keeps_comments() {
    let env = Env::new();
    env.set_config(
        "# my settings\n[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\" # the personal login\n",
    );
    let kept = |text: String| {
        assert!(text.contains("# my settings"), "{text}");
        assert!(text.contains("# the personal login"), "{text}");
    };
    env.ok(&["add", "team", "--kind", "claude"]);
    kept(env.config());
    assert_eq!(env.ids(), ["claude-a", "team"]);
    env.ok(&["rm", "team"]);
    kept(env.config());
    assert_eq!(env.ids(), ["claude-a"]);
}

#[test]
fn profile_add_rejects_duplicate_and_bad_id() {
    let env = Env::new();
    env.ok(&["add", "team", "--kind", "claude"]);
    let before = env.config();
    let why = env.fails(&["add", "team", "--kind", "claude"]);
    assert!(why.contains("Profile team already exists"), "{why}");
    // A built-in id counts too, even before it was written to the file.
    let why = env.fails(&["add", "codex", "--kind", "codex"]);
    assert!(why.contains("Profile codex already exists"), "{why}");
    for bad in ["a/b", "_x", "", "shell"] {
        let why = env.fails(&["add", bad, "--kind", "claude"]);
        assert!(why.contains("Invalid profile id"), "{bad:?}: {why}");
    }
    assert_eq!(env.config(), before);
}

#[test]
fn profile_rm_keeps_directory() {
    let env = Env::new();
    env.ok(&["add", "team", "--kind", "claude"]);
    let home = env.profiles_dir().join("team");
    write(&home.join("note"), "mine");
    let out = env.ok(&["rm", "team"]);
    assert!(out.contains("Kept"), "{out}");
    assert!(out.contains(home.to_str().unwrap()), "{out}");
    assert_eq!(fs::read_to_string(home.join("note")).unwrap(), "mine");
    assert!(!env.ids().contains(&"team".to_string()));
    let why = env.fails(&["rm", "team"]);
    assert!(why.contains("Unknown profile: team"), "{why}");
}

#[test]
fn profile_rm_of_a_builtin_writes_the_rest_first() {
    // No `[profiles]` table yet: removing `codex` must not bring it back, or lose `claude-a`.
    let env = Env::new();
    env.ok(&["rm", "codex"]);
    assert_eq!(env.ids(), ["claude-a"]);
    // The last profile stays: with none, the built-in list would come back with a warning on every command.
    let why = env.fails(&["rm", "claude-a"]);
    assert!(why.contains("last profile"), "{why}");
}

#[test]
fn profile_ls_reports_link_states() {
    let env = Env::new();
    let src = env.home.join(".claude");
    for dir in ["skills", "commands"] {
        write(&src.join(dir).join("one"), "x");
    }
    write(&src.join("settings.json"), "{}");
    env.set_config("[profiles.claude-a]\nkind = \"claude\"\nhome = \"default\"\n");
    let team = env.profiles_dir().join("team");
    // One item the profile already has of its own, before the links are made.
    write(&team.join("settings.json"), "{\"own\":true}");
    env.ok(&[
        "add",
        "team",
        "--kind",
        "claude",
        "--label",
        "Team",
        "--share",
        "skills,commands,settings.json",
    ]);
    // A CLI saved `skills` through a rename: a real directory where mine's link was.
    fs::remove_file(team.join("skills")).unwrap();
    write(&team.join("skills").join("mine"), "x");
    let out = env.ok(&["ls"]);
    let rows: Vec<Vec<&str>> = out.lines().map(|l| l.split('\t').collect()).collect();
    let team = rows.iter().find(|r| r[0] == "team").expect(&out);
    assert_eq!(team[1..4], ["claude", "Team", team[3]], "{out}");
    assert_eq!(team[4], "no", "{out}");
    assert_eq!(team[5], "linked:1 own:settings.json split:skills", "{out}");
    let a = rows.iter().find(|r| r[0] == "claude-a").expect(&out);
    // The default profile has no links, and its home is shown in the `~` form.
    assert_eq!((a[3], a[5]), ("~/.claude", "-"), "{out}");
    // `ls` never changes anything: the split item is still a directory, and nothing was linked.
    assert!(
        !fs::symlink_metadata(env.profiles_dir().join("team/skills"))
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn profile_ls_marks_logged_in_and_off() {
    let env = Env::new();
    write(&env.home.join(".codex/auth.json"), "{}");
    env.ok(&["add", "solo", "--kind", "claude", "--no-share"]);
    let out = env.ok(&["ls"]);
    let rows: Vec<Vec<&str>> = out.lines().map(|l| l.split('\t').collect()).collect();
    let row = |id: &str| rows.iter().find(|r| r[0] == id).expect(&out).clone();
    assert_eq!(row("codex")[4], "yes", "{out}");
    assert_eq!(row("claude-a")[4], "no", "{out}");
    assert_eq!(row("solo")[5], "off", "{out}");
    // Every row has the six columns, with `-` for an empty one.
    assert!(
        rows.iter()
            .all(|r| r.len() == 6 && r.iter().all(|c| !c.is_empty()))
    );
}

#[test]
fn profile_ls_warns_about_auth_in_a_shared_settings() {
    let env = Env::new();
    write(
        &env.home.join(".claude/settings.json"),
        r#"{"apiKeyHelper":"/secret/helper"}"#,
    );
    env.ok(&["add", "team", "--kind", "claude"]);
    let out = env.run(&["ls"]);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(stderr.contains("apiKeyHelper"), "{stderr}");
    assert!(stderr.contains("team"), "{stderr}");
    assert!(!stdout.contains("apiKeyHelper") && !stderr.contains("/secret/helper"));
}

#[test]
fn profile_relink_restores_a_split_item() {
    let env = Env::new();
    let src = env.home.join(".claude");
    write(&src.join("skills/one"), "x");
    env.ok(&["add", "team", "--kind", "claude", "--share", "skills"]);
    let skills = env.profiles_dir().join("team/skills");
    fs::remove_file(&skills).unwrap();
    write(&skills.join("mine"), "x");
    let out = env.ok(&["relink", "team"]);
    assert!(out.contains("skills"), "{out}");
    assert_eq!(fs::read_link(&skills).unwrap(), src.join("skills"));
    assert!(
        env.profiles_dir()
            .join("team/skills.mine-bak/mine")
            .exists()
    );
    let why = env.fails(&["relink", "claude-a"]);
    assert!(why.contains("no links"), "{why}");
}

#[test]
fn profile_login_runs_cli_with_profile_env() {
    let env = Env::new();
    env.fake_cli("claude", 0);
    env.fake_cli("codex", 0);
    env.ok(&["add", "team", "--kind", "claude"]);
    let leaky = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_mine"))
            .arg("profile")
            .args(args)
            .env("MINE_STATE_DIR", &env.state)
            .env("HOME", &env.home)
            .env("PATH", env.path())
            // What the shell that started mine may have exported: none of it may reach the login.
            .env("CLAUDE_CONFIG_DIR", "/leak/claude")
            .env("CODEX_HOME", "/leak/codex")
            .env("ANTHROPIC_API_KEY", "sk-leak")
            .env("OPENAI_API_KEY", "sk-leak")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let out = leaky(&["login", "team"]);
    assert!(out.status.success(), "{out:?}");
    let team = env.profiles_dir().join("team");
    assert_eq!(env.argv("claude"), "auth login");
    let claude = env.env_of("claude");
    assert!(
        claude.contains(&format!("CLAUDE_CONFIG_DIR={}", team.display())),
        "{claude:?}"
    );
    assert!(!claude.iter().any(|kv| kv.starts_with("ANTHROPIC_API_KEY=")));
    // Codex at the CLI's own default home: mine sets nothing, and the exported value is gone.
    let out = leaky(&["login", "codex"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(env.argv("codex"), "login");
    let codex = env.env_of("codex");
    assert!(
        !codex.iter().any(|kv| kv.starts_with("CODEX_HOME=")),
        "{codex:?}"
    );
    assert!(!codex.iter().any(|kv| kv.starts_with("OPENAI_API_KEY=")));
    // Still no daemon.
    assert!(!env.state.join("daemon.sock").exists());
}

#[test]
fn profile_login_passes_on_the_cli_exit_status() {
    let env = Env::new();
    env.fake_cli("codex", 3);
    let out = env.run(&["login", "codex"]);
    assert_eq!(out.status.code(), Some(3));
    let why = env.fails(&["login", "nope"]);
    assert!(why.contains("Unknown profile: nope"), "{why}");
}

#[test]
fn profile_login_and_relink_refuse_on_a_broken_config() {
    // The config moved claude-b to `~/work`, then broke: nothing may log in or link at the built-in claude-b home.
    let env = Env::new();
    env.fake_cli("claude", 0);
    fs::create_dir_all(env.profiles_dir().join("claude-b")).unwrap();
    env.set_config("[profiles.claude-b]\nkind = \"claude\"\nhome = \"~/work\n");
    for args in [&["login", "claude-b"][..], &["relink", "claude-b"]] {
        let why = env.fails(args);
        assert!(why.contains("config.toml line 3"), "{why}");
    }
    assert!(!env.bin.join("claude.argv").exists());
}

#[test]
fn profile_add_materializes_builtins_for_an_empty_table() {
    // `[profiles]` with nothing in it still means the built-in list. Adding to it must not leave `team` alone in the file.
    let env = Env::new();
    fs::create_dir_all(env.profiles_dir().join("claude-b")).unwrap();
    env.set_config("# top\n[profiles]\n");
    env.ok(&["add", "team", "--kind", "claude"]);
    assert_eq!(env.ids(), ["claude-a", "claude-b", "codex", "team"]);
    assert!(env.config().contains("# top"), "{}", env.config());
    // Same for `rm`: the built-in it removes was never in the file.
    env.set_config("[profiles]\n");
    env.ok(&["rm", "codex"]);
    assert_eq!(env.ids(), ["claude-a", "claude-b"]);
}

#[test]
fn profile_add_materializes_builtins_when_no_entry_is_usable() {
    let env = Env::new();
    env.set_config("[profiles.typo]\nkind = \"claud\"\n");
    let out = env.run(&["add", "team", "--kind", "claude"]);
    assert!(out.status.success(), "{out:?}");
    // The broken entry stays as the user wrote it (it still warns), and the built-ins are written next to it.
    let text = env.config();
    assert!(text.contains("[profiles.typo]\nkind = \"claud\""), "{text}");
    let (profiles, _) = Profiles::parse(
        Some(&text),
        &State {
            root: env.state.clone(),
        },
    );
    assert!(!profiles.builtin);
    let ids: Vec<&str> = profiles.list.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["claude-a", "codex", "team"]);
}

#[test]
fn profile_add_refuses_a_broken_entry_with_a_builtin_id() {
    // `claude-a` is in use as the built-in. Writing the built-in over the user's own broken `claude-a` would lose their keys.
    let env = Env::new();
    env.set_config("[profiles.claude-a]\nkind = \"claud\"\nhome = \"~/.claude-work\"\n");
    let before = env.config();
    for args in [&["add", "team", "--kind", "claude"][..], &["rm", "codex"]] {
        let why = env.fails(args);
        assert!(why.contains("claude-a"), "{why}");
        assert!(why.contains("Fix"), "{why}");
        assert_eq!(env.config(), before);
    }
}

#[test]
fn profile_add_writes_through_a_symlinked_config() {
    // A dotfiles setup: config.toml links to the real file. The link stays a link, and the real file gets the change.
    let env = Env::new();
    let dotfiles = env.home.join("dotfiles");
    let real = dotfiles.join("mine.toml");
    fs::create_dir(&dotfiles).unwrap();
    fs::write(&real, "# mine\n").unwrap();
    std::os::unix::fs::symlink(&real, env.config_path()).unwrap();
    env.ok(&["add", "team", "--kind", "claude"]);
    let link = fs::symlink_metadata(env.config_path()).unwrap();
    assert!(link.file_type().is_symlink());
    let text = fs::read_to_string(&real).unwrap();
    assert!(text.contains("# mine\n"), "{text}");
    assert!(text.contains("[profiles.team]"), "{text}");
    // The temp file was renamed into place, beside the target.
    assert_eq!(fs::read_dir(&dotfiles).unwrap().count(), 1);
}
