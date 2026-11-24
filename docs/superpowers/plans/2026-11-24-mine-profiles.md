# mine profiles, hardening, and approvals (D1.5) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the fixed claude-a/claude-b/codex accounts with any number of Claude and Codex profiles. In the same pass, add the comparison's hardening fixes and approving or denying Claude permission requests from the app and the TUI.

**Architecture:**
- **Profiles:** a new `mine_core::profiles` module loads them from `<state>/config.toml`, with built-in migration. A `mine_core::share` module keeps each profile's shared links and MCP servers.
- **Daemon:** owns the profile list, sends it to clients (protocol v4, with a back-compatible hello), and resolves a session's profile at launch.
- **Approvals:** ride on a held `PermissionRequest` hook (`mine _event --ask`).
- **Presence:** clients send presence heartbeats, which decide notifications, "seen", and whether to hold.

**Tech Stack:** Rust 2024 workspace (mine-core, mine-cli, mine-app). The daemon and IPC are tokio + JSON lines over a Unix socket. The TUI is ratatui; the app is GPUI 0.2.2 + gpui-component 0.4.1.

**Spec:** `docs/superpowers/specs/2026-11-24-mine-profiles-design.md` (section numbers below refer to it).

## Global Constraints

- **Working location:** worktree `/Users/me/.gitbutler-worktrees/mine/feat-desktop`, branch `feat/profiles`. Base: `4315119`.
- **Committing:**
  - Commit with GitButler from the worktree: `but commit -F - <paths>`, no `-b`. Never use git write commands, and don't change git config.
  - Conventional Commits in English: `type(scope): imperative summary`, with a body stating what, why, and verification.
  - The last line is `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`, or the model that did the work.
- **Language:** everything is English: code comments, user-facing strings, docs, and commit messages. Copy follows `.superpowers/sdd/GLOSSARY.md` (in the worktree). `README.ko.md` is the Korean mirror of `README.md` and changes with it.
- **Toolchain and dependencies:** edition 2024, rust-version 1.89. The GPUI pins are `gpui =0.2.2` (runtime_shaders) and `gpui-component =0.4.1`, in mine-app only. The only new dependency allowed is `toml_edit` (for comment-preserving config edits); give its reason in the commit body. No Python.
- **Protocol:**
  - `VERSION = 4` and `MIN_CLIENT_VERSION = 3`. `HOLD_VERSION` stays `1`.
  - The hello reply carries `features` and `daemon { pid, version, started_ms }`.
  - Feature names are added by the task that implements them: `profiles` (Task 4), `presence` (Task 8), `ask` (Task 9).
- **Profile defaults:**
  - **Palette, in order:** gold `#e5c07b`, violet `#c678dd`, teal `#56b6c2`, green `#98c379`, blue `#61afef`, red `#e06c75`.
  - **Chip:** the first character of the label, uppercased.
  - **Claude shareable items:** `skills`, `commands`, `agents`, `CLAUDE.md`, `settings.json`, `plugins`, `mcpServers`.
  - **Codex shareable items:** `AGENTS.md`, `prompts`, `config.toml`.
- **Bypass variables:**
  - Claude: `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, `AWS_BEARER_TOKEN_BEDROCK`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX`.
  - Codex: `OPENAI_API_KEY`, `CODEX_API_KEY`.
  - Shell sessions keep everything.
- **Approvals:**
  - The hook `timeout` is `130` (seconds), and the hold lasts at most 120 s. Tests may shorten the hold with `MINE_ASK_TIMEOUT_MS`.
  - Pass tools: `AskUserQuestion` and `ExitPlanMode`.
  - Deny message: `Denied from mine`.
  - Never emit `updatedPermissions`.
- **Presence:** a client is present when it's visible and its last input was within 180 s. Heartbeat at least every 30 s while visible.
- **Credentials:** never open `.credentials.json`, the Keychain, or Codex `auth.json`; existence checks only. MCP server values are opaque: never log or display them, and never send them to clients.
- **Safety on this machine:**
  - Never touch `~/.mine` or the user's running `~/.cargo/bin/mine daemon`. Tests use temp state dirs.
  - Tests must never run the real `osascript`: the daemon test harness defaults `MINE_NOTIFY` to `/usr/bin/true`.
  - The user is using this Mac. Prefer model-level tests.
  - GUI checks follow D-R4: window-id capture only, picking the large on-screen mine-app window. Key automation follows D-R8: check the frontmost process before every key, and abort if the check fails.
- **Check command:** `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
  - Known pre-existing flake: `session_size_follows_last_input_client` (about 1 in 75 under load). Rerun it once if it alone fails, and say so.

## Review Focus

1. A session whose `meta.json` names a profile that no longer exists must refuse to start with `Unknown profile: <id>`, never fall back to another account. Covered by Task 3 `unknown_profile_refuses_to_start`.
2. A broken or half-written `[profiles]` table must not strand the user. They get the built-in profiles plus a warning naming the line. Covered by Task 1 `broken_profiles_table_falls_back_with_warning`.
3. Two clients answering the same held request (the app and the TUI) must produce exactly one decision. A stale answer is ignored. Covered by Task 9 `second_answer_is_ignored`.
4. A shareable item that doesn't exist in the default home (for example no `~/.claude/agents`) must not create a dangling link. Covered by Task 2 `missing_source_item_is_not_linked`.
5. Bytes that contain `\x03` or `\x1b` but aren't a bare interrupt (an arrow key, a paste) must leave a Working session alone. Covered by Task 7 `escape_sequence_keeps_working` and `paste_with_ctrl_c_keeps_working`.

---

### Task 1: Profile model, config, migration, labels

**Files:**
- Create: `crates/mine-core/src/profiles.rs` (+ `pub mod profiles;` in `lib.rs`)
- Modify: `crates/mine-core/src/accounts.rs` (keep `auto_label` and the collision rule. `Labels` becomes a view over profile infos.)

**Interfaces:**
- **Consumes:**
  - `paths::State` (`profiles_dir()`, the config path used by `accounts.rs` today)
  - `accounts::auto_label(&serde_json::Value) -> Option<String>`
- **Produces:**
  ```rust
  #[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)] #[serde(rename_all = "lowercase")]
  pub enum ProfileKind { Claude, Codex }
  pub enum Home { Default, Path(PathBuf), Managed }          // section 5: "default" | "<path>" | absent
  pub enum Share { All, Off, Items(Vec<String>) }
  pub struct Profile { pub id: String, pub kind: ProfileKind, pub home: Home, pub label: Option<String>,
                       pub color: Option<String>, pub share: Share, pub default: bool }
  pub struct Profiles { pub list: Vec<Profile> }
  impl Profiles {
      pub fn load(state: &State) -> (Profiles, Vec<String>);             // warnings: line number + reason only
      pub fn parse(text: Option<&str>, state: &State) -> (Profiles, Vec<String>);
      pub fn get(&self, id: &str) -> Option<&Profile>;
      pub fn default_of(&self, kind: ProfileKind) -> Option<&Profile>;
  }
  impl Profile {
      pub fn home_dir(&self, state: &State, user_home: &Path) -> PathBuf; // Default: ~/.claude | ~/.codex
      pub fn claude_json(&self, state: &State, user_home: &Path) -> PathBuf; // Default: ~/.claude.json
      pub fn env(&self, state: &State, user_home: &Path) -> Option<(&'static str, PathBuf)>; // CLAUDE_CONFIG_DIR | CODEX_HOME
  }
  #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
  pub struct ProfileInfo { pub id: String, pub kind: ProfileKind, pub label: String, pub color: String,
                           pub chip: String, pub default: bool, pub logged_in: bool }
  pub fn infos(p: &Profiles, state: &State, user_home: Option<&Path>) -> Vec<ProfileInfo>;
  pub const PALETTE: [(&str, &str); 6]; // ("gold", "#e5c07b"), … in the Global Constraints order
  ```

- [ ] **Step 1: Write the failing tests** in `profiles.rs`. Each uses a temp state dir and a temp user home.
  - `builtin_profiles_when_table_missing`: no `[profiles]` → ids `["claude-a","codex"]`. With `<state>/profiles/claude-b` present → `["claude-a","claude-b","codex"]`. claude-a and codex are `Home::Default`; claude-b is `Home::Managed`.
  - `accounts_labels_still_apply`: `[accounts.claude-b] label = "work"` with no `[profiles]` → claude-b's info label is `"work"`.
  - `profiles_table_order_and_defaults`:
    - Config order is kept.
    - With claude-a `home = "default"`, `default_of(Claude)` is claude-a.
    - Putting `default = true` on `team` makes `default_of(Claude)` team, while claude-a's home stays `Default`.
    - With no default-home codex profile, `default_of(Codex)` is the first codex profile.
  - `home_tilde_expands`: `home = "~/x"` → `<user_home>/x`.
  - `only_one_default_home_per_kind`: a second claude profile with `home = "default"` is skipped, with a warning naming it.
  - `bad_ids_and_kinds_skipped_with_warning`: id `a/b` and `kind = "gpt"` are skipped, with warnings. Valid ones are kept.
  - `broken_profiles_table_falls_back_with_warning`: a TOML syntax error → the built-in list, plus one warning containing `line`.
  - `colors_and_chips`:
    - Auto colors follow the palette by position.
    - `color = "violet"` → `#c678dd`. `color = "#123456"` is kept. An invalid color falls back to auto, with a warning.
    - label `personal` → chip `"P"`.
  - `claude_labels_from_login_email`: a managed profile's `.claude.json` with `oauthAccount.emailAddress` gives the local part. The existing collision rule (`yb@gmail` / `yb@acme`) still holds across all Claude profiles.
  - `codex_labels`: the default-home codex profile → `"Codex"`, other codex profiles → their id, and an alias always wins.
  - `logged_in_is_existence_only`:
    - Claude is logged in when `.claude.json` has `oauthAccount`.
    - Codex is logged in when `auth.json` exists. Create `auth.json` as a FIFO and assert that `infos` returns within 2 s (run it on a thread with a timeout), proving it was never opened.
  - `env_for_profile`:
    - Default → `None`.
    - Managed claude → `("CLAUDE_CONFIG_DIR", <state>/profiles/<id>)`.
    - Managed codex → `("CODEX_HOME", …)`.
    - `Home::Path` → that path.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-core profiles` → FAIL (module missing).
- [ ] **Step 3: Implement.**
  - Parse with the existing `toml` crate. Ids follow `session::validate_name`.
  - Unknown keys produce a warning. Don't fail on them.
  - `[accounts.<id>] label` applies when that profile has no `label`.
  - `Labels` keeps `get(id) -> &str` and gains `Labels::from_infos(&[ProfileInfo])`. `Labels::load` builds through `Profiles::load` + `infos`, so existing clients keep working until Task 6.
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS.
- [ ] **Step 5: Commit.** `feat(profiles): load any number of Claude and Codex profiles from config`

### Task 2: Sharing links, MCP sync, auth-setting warnings

**Files:**
- Create: `crates/mine-core/src/share.rs` (+ `pub mod share;`)

**Interfaces:**
- **Consumes:** Task 1 `Profile`, `Profiles`, `ProfileKind`, `Share`, `Profile::home_dir` / `claude_json`
- **Produces:**
  ```rust
  pub const CLAUDE_SHARE: &[&str] = &["skills","commands","agents","CLAUDE.md","settings.json","plugins","mcpServers"];
  pub const CODEX_SHARE: &[&str] = &["AGENTS.md","prompts","config.toml"];
  #[derive(Debug, PartialEq)] pub enum LinkState { Linked, Own, Split, Elsewhere(PathBuf), NoSource, Off }
  pub struct ShareReport { pub items: Vec<(String, LinkState)>, pub warnings: Vec<String> }
  pub fn prepare(profile: &Profile, profiles: &Profiles, state: &State, user_home: &Path) -> Result<ShareReport>;
  pub fn relink(profile: &Profile, profiles: &Profiles, state: &State, user_home: &Path, item: Option<&str>) -> Result<Vec<String>>;
  pub fn sync_mcp(default_json: &Path, profile_json: &Path) -> Result<()>;
  pub fn auth_warnings(settings_json: &Path) -> Vec<String>;
  ```
  - `prepare` does everything below, and is a no-op returning an empty report for a `Home::Default` profile:
    - creates a missing home with mode 0700;
    - makes the links;
    - records them in `<state>/profiles/<id>.links` (one item name per line);
    - runs `sync_mcp` when `mcpServers` is shared;
    - collects `auth_warnings` of the effective `settings.json`.
  - The share source is `profiles.default_of(profile.kind)`'s home.

- [ ] **Step 1: Write the failing tests** in `share.rs`, with temp dirs only:
  - `share_all_links_items`: every source item that exists becomes a symlink → `Linked`, and the `.links` file lists them.
  - `default_profile_gets_no_links`.
  - `real_file_is_own_and_untouched`: a pre-existing real `settings.json` stays byte-identical → `Own`.
  - `missing_source_item_is_not_linked`: no source `agents/` → no link created → `NoSource`.
  - `link_elsewhere_reported`: a symlink pointing at another target is left alone → `Elsewhere(target)`.
  - `split_after_rename_detected`: `prepare` links `settings.json`; the test replaces the link with a real file; `prepare` again → `Split`, and the file is unchanged.
  - `relink_restores_and_backs_up`: after the split, `relink(.., Some("settings.json"))` makes it a link again and keeps the old file as `settings.json.mine-bak`.
  - `share_items_subset` and `share_off_no_links`.
  - `mcp_sync_copies_only_mcpservers`:
    - Start from default `.claude.json` = `{"mcpServers":{"a":{"command":"x"}},"oauthAccount":{"emailAddress":"d@x"}}` and profile `.claude.json` = `{"oauthAccount":{"emailAddress":"p@x"},"mcpServers":{"a":{"command":"old"},"b":{"command":"y"}}}`.
    - After the sync, the profile has `oauthAccount.emailAddress == "p@x"` and `mcpServers == {"a":{"command":"x"},"b":{"command":"y"}}`, and every other field is unchanged.
    - The write is atomic: temp file + rename in the same directory.
  - `mcp_sync_skipped_when_share_excludes`: `share = ["skills"]` → the profile JSON is unchanged.
  - `never_opens_credentials`:
    - FIFOs sit at `<default>/.credentials.json`, `<profile>/.credentials.json`, and a codex `auth.json`.
    - `prepare` for a claude and a codex profile returns within 2 s.
    - No `.credentials.json` or `auth.json` appears in any report or `.links` file.
  - `auth_warnings_by_key_name`: a `settings.json` with `"apiKeyHelper":"/bin/k"` and `"env":{"ANTHROPIC_API_KEY":"sk-secret"}` gives 2 warnings. Neither contains `sk-secret` or `/bin/k`.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-core share` → FAIL.
- [ ] **Step 3: Implement.**
  - Symlinks are absolute (`std::os::unix::fs::symlink`).
  - `Split` means the item is in `.links` but is no longer a symlink.
  - `sync_mcp` parses both files as `serde_json::Value`, merges only the `mcpServers` object, and preserves key order (`serde_json` `preserve_order` is already a dependency if present; otherwise accept reordering and note it in the report). It writes atomically.
  - Missing `.claude.json` files are fine: there's no source to copy from, or the profile file gets created with just `mcpServers`.
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS.
- [ ] **Step 5: Commit.** `feat(profiles): share common settings and MCP servers across profiles through tracked links`

### Task 3: Launch sessions on profiles

**Files:**
- Modify: `crates/mine-core/src/session.rs` (`launch`, the env-cleaning lists), `crates/mine-core/src/view.rs` (`parse_new`, `NEW_USAGE`), `crates/mine-cli/src/main.rs` (`new --profile`), `crates/mine-cli/src/daemon.rs` (resolve the profile and `share::prepare` before `start_holder`, in both `create` and `restore`), `crates/mine-cli/src/hold.rs` (strip the bypass vars)
- Test: `crates/mine-cli/tests/daemon.rs`, unit tests in `session.rs` / `view.rs`

**Interfaces:**
- **Consumes:** Task 1 `Profiles`, `Profile::env`; Task 2 `share::prepare`
- **Produces:**
  ```rust
  pub fn launch(meta: &Meta, state: &State, exe: &Path, profiles: &Profiles, user_home: &Path) -> Result<Launch>;
  pub fn bypass_vars(kind: Kind) -> &'static [&'static str];   // Global Constraints lists; Shell → &[]
  // view.rs
  pub const NEW_USAGE: &str = "<profile|shell> <name> [model]";
  pub fn parse_new(text: &str, project: &Path, profiles: &[ProfileInfo]) -> Result<NewSession>;
  ```
  - `NewSession.account` and `Meta.account` keep their field names and now hold a profile id. On-disk and wire compatibility stays unchanged.
  - `NewSession.kind` comes from the profile's kind.
  - **Launch errors:** `Unknown profile: <id>`. A kind mismatch gives `Profile <id> is a <kind> profile`.
  - **Daemon:** keeps `profiles: Profiles` and `user_home: PathBuf` (loaded once at start; Task 4 adds reload). Share warnings go to `daemon.log`.

- [ ] **Step 1: Write the failing tests.**
  - `launch_sets_env_for_managed_profile`: a managed claude profile → env has `CLAUDE_CONFIG_DIR=<state>/profiles/team`. A managed codex profile → `CODEX_HOME=…`.
  - `default_profile_sets_no_env`.
  - `legacy_claude_b_meta_resumes_same_dir`: `meta.account = Some("claude-b")` with built-ins → `CLAUDE_CONFIG_DIR=<state>/profiles/claude-b`, and `--resume <id>` is kept.
  - `parse_new_uses_profiles`:
    - `"team t1 opus"` → kind Claude, account `team`, model `opus`.
    - `"shell dev"` → Shell.
    - `"nope x"` → error containing `Unknown profile: nope`.
  - Daemon test `unknown_profile_refuses_to_start`: write a session dir whose `meta.json` has `account: "gone"`, then start the daemon. Assert that the session isn't started, that it shows as not alive, and that `daemon.log` contains `Unknown profile: gone`. No holder process exists for it.
  - Daemon test `bypass_vars_stripped_for_agents_only`:
    - Start the daemon with `ANTHROPIC_API_KEY=sk-a`, `OPENAI_API_KEY=sk-o`, `FOO=1`.
    - A claude session's env dump (the existing fake-claude env approach) lacks both keys and has `FOO=1`.
    - A shell session's dump has all three.
  - Daemon test `creating_a_managed_profile_session_links_shared_items`: after `New` for a managed claude profile, `<state>/profiles/team/skills` is a symlink to the default home's `skills`.
  - CLI parse test: `mine new x --profile team` and `mine new x --account team` give the same spec.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test --workspace -- profile bypass legacy_claude_b parse_new` → FAIL.
- [ ] **Step 3: Implement.**
  - `CLAUDE_CODE_USE_BEDROCK` is no longer passed through.
  - The holder removes `bypass_vars(meta.kind)` from the child env, next to the existing leaked-var cleaning.
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS.
- [ ] **Step 5: Commit.** `feat(profiles): start sessions on any profile and strip auth bypass variables`

### Task 4: Protocol v4: features, client compatibility, profile list

**Files:**
- Modify: `crates/mine-core/src/proto.rs`, `crates/mine-core/src/conn.rs`, `crates/mine-cli/src/daemon.rs`
- Test: `crates/mine-cli/tests/daemon.rs`, `crates/mine-cli/tests/link.rs` if the link reads `Hello`

**Interfaces:**
- **Consumes:** Task 1 `Profiles::load`, `infos`, `ProfileInfo`
- **Produces:**
  ```rust
  pub const VERSION: u32 = 4;
  pub const MIN_CLIENT_VERSION: u32 = 3;
  pub struct DaemonInfo { pub pid: u32, pub version: String /* CARGO_PKG_VERSION */, pub started_ms: u64 }
  FromDaemon::Hello { version: u32, #[serde(default)] features: Vec<String>, #[serde(default)] daemon: Option<DaemonInfo> }
  FromDaemon::Profiles { profiles: Vec<ProfileInfo> }
  ToDaemon::ReloadProfiles
  // both enums gain `#[serde(other)] Unknown` (unit variant) so an unknown "t" parses instead of failing
  ```
  - The daemon stores each client's version.
  - `Profiles` is sent only to clients with version ≥ 4: once right after `Hello`, and again to all of them after `ReloadProfiles`.
  - `Unknown` is logged once per client and otherwise ignored, on both sides.
  - The features list in this task is `["profiles"]`.

- [ ] **Step 1: Write the failing tests** (daemon integration):
  - `v4_hello_has_features_and_identity`: the reply has `features` containing `"profiles"` and a `daemon.pid` equal to the pid file.
  - `v4_client_gets_profiles_after_hello`: the ids match the built-ins.
  - `v3_client_still_served`: `Hello{version:3}` → gets `Hello` and `Sessions`, and never `Profiles`. `New` and `Attach` still work.
  - `too_old_client_refused`: `Hello{version:2}` → `Error` containing the `restart_hint` text.
  - `unknown_message_is_ignored`: send `{"t":"FromTheFuture","x":1}` then `List` → still gets `Sessions`, and the connection stays open.
  - `reload_profiles_rebroadcasts`: append a `[profiles.team]` section to config, then send `ReloadProfiles` → a new `Profiles` arrives containing `team`.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-cli --test daemon -- hello reload unknown_message v3_client too_old` → FAIL.
- [ ] **Step 3: Implement.** Keep the Poke-across-versions rule. The app and TUI connect with `VERSION`.
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS.
- [ ] **Step 5: Commit.** `feat(proto): announce features, accept v3 clients, and send the profile list`

### Task 5: `mine profile` commands

**Files:**
- Create: `crates/mine-cli/src/profile_cmd.rs`
- Modify: `crates/mine-cli/src/main.rs` (subcommand), `crates/mine-cli/Cargo.toml` (`toml_edit`), `README.md`, `README.ko.md` (a "Profiles" section under Configuration, replacing "Account labels")

**Interfaces:**
- **Consumes:** Task 1 (`Profiles`, `infos`), Task 2 (`prepare`, `relink`, `LinkState`), Task 4 (`ToDaemon::ReloadProfiles`)
- **Produces:**
  ```sh
  mine profile ls
  mine profile add <id> --kind claude|codex [--label L] [--home PATH] [--share a,b|--no-share]
  mine profile login <id>
  mine profile relink <id> [item]
  mine profile rm <id>
  ```
  - **`ls` columns** (tab-separated, `-` for empty):
    - `id`, `kind`, `label`, `home` (tilde form), `logged-in` (`yes`/`no`)
    - `share`, which reads `linked:N own:a,b split:c` (or `off`)
    - Warnings go to stderr.
  - **`add`:**
    - If the config has no `[profiles]` table yet, first writes the current built-in list (with their labels from `[accounts.*]`), so nothing disappears.
    - Then appends the new profile.
    - Creates the managed home (mode 0700), runs `share::prepare`, and sends `ReloadProfiles` if a daemon is reachable. It never spawns one.
  - **`login`:**
    - Execs the CLI's login with the profile's env, in the foreground: Claude `claude` + the builder-verified login entry (`/login` argument or interactive), Codex `codex login`. Verify both against the installed CLIs and record what you found in the report.
  - **`rm`:**
    - Removes the section, prints `Kept <home>`, deletes nothing, and sends `ReloadProfiles`.

- [ ] **Step 1: Write the failing tests** (integration tests run the built `mine` with a temp `MINE_STATE_DIR` and `HOME`):
  - `profile_add_materializes_builtins_first`: with an existing `<state>/profiles/claude-b`, `add team --kind claude` leaves a config with `claude-a`, `claude-b`, `codex`, `team` in that order.
  - `profile_add_keeps_comments`: a comment line in `config.toml` survives `add` and `rm`.
  - `profile_add_rejects_duplicate_and_bad_id`: a duplicate id exits non-zero with `Profile team already exists`, and `a/b` is rejected.
  - `profile_rm_keeps_directory`: after `rm team`, `<state>/profiles/team` still exists, and stdout has `Kept`.
  - `profile_ls_reports_link_states`: one linked item, one own item, one split item → the share column reads `linked:1 own:settings.json split:skills` (or the matching names).
  - `profile_login_runs_cli_with_profile_env`: a fake `claude` and `codex` on PATH record argv and env. `login team` shows `CLAUDE_CONFIG_DIR=<state>/profiles/team`. `login codex` (default home) shows no `CODEX_HOME`.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-cli profile_` → FAIL.
- [ ] **Step 3: Implement.** Edit `config.toml` with `toml_edit`. The README section covers the profile model, `config.toml` keys (`kind`, `home`, `default`, `label`, `color`, `share`), sharing and `split`/`relink`, and the CLI. Mirror it in `README.ko.md`.
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS.
- [ ] **Step 5: Commit.** `feat(cli): add mine profile commands to list, add, log in, relink, and remove profiles`

### Task 6: Clients use profiles: launcher, chips, typed form

**Files:**
- Modify: `crates/mine-core/src/launch.rs`, `crates/mine-cli/src/client/{mod.rs,launcher.rs,ui.rs}`, `crates/mine-app/src/{model.rs,launcher.rs,sidebar.rs,tabs.rs}`, `crates/mine-core/src/accounts.rs` (delete `Labels` if nothing uses it after this task), `README.md`, `README.ko.md` (launcher table)
- Test: unit tests in those files

**Interfaces:**
- **Consumes:** Task 4 `FromDaemon::Profiles`; Task 1 `ProfileInfo`; Task 3 `parse_new`
- **Produces:**
  ```rust
  pub enum Choice { Profile(String), Shell, Typed }
  pub fn rows(profiles: &[ProfileInfo]) -> Vec<(Choice, Option<char>)>;   // keys '1'..='9' for the first nine profiles, 's', 'n'
  pub fn enter_choice(profiles: &[ProfileInfo]) -> Option<Choice>;          // default claude, else first profile
  pub fn new_session(choice: &Choice, name: &str, project: &Path, profiles: &[ProfileInfo]) -> Option<NewSession>;
  ```
  - **Clients:** keep the last `Profiles` received. Until it arrives, the launcher shows only shell and the typed row.
  - **Rows for a profile with `logged_in == false`:** show `Not logged in · mine profile login <id>` in dim text and don't start. Pressing their key sets the status `Not logged in. Run: mine profile login <id>`.
  - **Chips:** use `ProfileInfo.chip` and `.color` everywhere (TUI `Color::Rgb`, the app's `rgb`). Shell has no chip.

- [ ] **Step 1: Write the failing tests.**
  - `rows_follow_profiles` (core): 3 profiles → keys `1`,`2`,`3`, then `s`, `n`.
  - `enter_starts_default_claude`: the `default: true` claude profile wins over order.
  - `only_first_nine_profiles_have_keys`: 11 profiles → rows 10 and 11 have `None`.
  - TUI `digit_starts_that_profile`: key `2` → `New` for the second profile.
  - TUI `not_logged_in_row_refuses`: no `New` is sent, and the status text matches.
  - TUI `chip_uses_profile_color`: the rendered chip cell has the profile's chip letter and the RGB of its color.
  - App `app_launcher_rows_follow_profiles` and `app_digit_key_starts_profile` (model level).
  - Typed form: `n` then `team t9` → `New` with account `team`. `NEW_USAGE` is shown on error.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test --workspace -- rows_follow enter_starts digit_starts not_logged chip_uses app_launcher` → FAIL.
- [ ] **Step 3: Implement.** The launcher widths follow the longest row text. Update the README launcher table: `1`–`9` start a profile, Enter starts the default Claude profile, `s` starts a shell, `n` is the typed form.
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS. Optionally capture the app launcher (D-R4) with three temp profiles.
- [ ] **Step 5: Commit.** `feat(ui): list every profile in the launcher with its own chip`

### Task 7: Interrupt fallback, fail-open hooks, Codex approval state

**Files:**
- Modify: `crates/mine-core/src/hooks.rs` (`EventKind::Interrupt`, `fold`, `run_event_cmd`), `crates/mine-cli/src/main.rs` (`_event` exit code), `crates/mine-cli/src/daemon.rs` (`Input` handling), `crates/mine-core/src/session.rs` (Codex hooks)
- Test: `hooks.rs` units, `crates/mine-cli/tests/daemon.rs`

**Interfaces:**
- **Produces:**
  - `EventKind::Interrupt`: the daemon appends it (through `hooks::append`) when a claude/codex session is `Working` and `Input.data` is exactly `b"\x03"` or `b"\x1b"`, then refreshes.
  - `fold` maps `Interrupt` → `Idle`; any later event wins.
  - `mine _event` always exits 0. Failures write one line to `<state>/logs/hooks.log` and nothing to stdout.
  - Codex hooks add `PermissionRequest` → Needs approval, if the installed Codex supports that event. Verify with the installed `codex`, and say what you found in the report. If it's unsupported, skip it and leave a `ponytail:` note.

- [ ] **Step 1: Write the failing tests.**
  - `fold_interrupt_goes_idle` and `later_hook_event_wins_after_interrupt` (unit).
  - Daemon `bare_ctrl_c_while_working_goes_idle`: a prompt event → Working; `Input` `\x03` → Idle.
  - Daemon `escape_sequence_keeps_working`: `Input` `\x1b[A` → stays Working.
  - Daemon `paste_with_ctrl_c_keeps_working`: `Input` `\x1b[200~a\x03b\x1b[201~` → stays Working.
  - `event_cmd_fails_open`: the built `mine _event` with a missing session dir, then with garbage stdin, then with no daemon → exit status 0 every time, and empty stdout.
  - `codex_permission_request_sets_approval` (only if supported).
- [ ] **Step 2: Confirm the failure.** Run: `cargo test --workspace -- interrupt escape_sequence paste_with fails_open` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS.
- [ ] **Step 5: Commit.** `fix(daemon): clear Working on a bare interrupt and never let a hook fail the agent`

### Task 8: Presence and attention routing

**Files:**
- Modify: `crates/mine-core/src/proto.rs`, `crates/mine-cli/src/daemon.rs` (`refresh`, `notify` decision, the `viewed_by_someone` replacement), `crates/mine-cli/src/client/{mod.rs,input.rs,tty.rs}` (focus reporting), `crates/mine-app/src/{workspace.rs,model.rs}`
- Test: `crates/mine-cli/tests/daemon.rs`, TUI and app units

**Interfaces:**
- **Produces:**
  ```rust
  ToDaemon::Presence { visible: bool, last_input_ms: u64, viewing: Option<String> }
  pub const PRESENT_WINDOW_MS: u64 = 180_000;
  pub const PRESENCE_HEARTBEAT: Duration = Duration::from_secs(30);
  ```
  - **Daemon:**
    - Stores presence per client. `Focus { focused }` maps to `visible = focused`, `last_input_ms = now`, keeping the previous `viewing`.
    - A client is **present** when `visible && now - last_input_ms <= PRESENT_WINDOW_MS`.
    - Notifications (approvals only) fire unless a present client views that session.
    - **Seen** (Done → Idle) happens only when a present client views it.
    - Features gain `"presence"`.
  - **TUI:**
    - Writes `\x1b[?1004h` on start and `\x1b[?1004l` on exit and detach. The input parser turns `\x1b[I` / `\x1b[O` into focus events, which aren't forwarded to the session unless the session itself enabled focus reporting (track mode 1004 like the other key modes).
    - Visible defaults to true while attached, until a focus-out arrives.
    - Sends `Presence` on change, on input (at most once per 5 s), and every `PRESENCE_HEARTBEAT` while visible.
  - **App:** sends `Presence` on window activate/deactivate, on input (at most once per 5 s), every 30 s while active, and on attach change. `viewing` is the attached session while the terminal pane shows it.

- [ ] **Step 1: Write the failing tests.**
  - Daemon `idle_viewer_does_not_suppress_notification`: a client with `visible=true`, `viewing=s`, and `last_input_ms` 200 s ago → the approval notification fires (fake `MINE_NOTIFY` records it).
  - Daemon `present_viewer_suppresses_notification`.
  - Daemon `focus_out_client_not_visible`: `visible=false` → notifies.
  - Daemon `seen_requires_present_viewer`: a Done session viewed only by a non-present client stays Done.
  - Daemon `v3_focus_still_works`: a v3 client's `Focus{focused:true}` while attached suppresses as before.
  - TUI `focus_events_parsed_not_forwarded`: `\x1b[I` and `\x1b[O` produce focus events and no `Input`. With session mode 1004 on, they're also forwarded.
  - App `presence_sent_on_activate_and_input` (model level, throttle respected).
- [ ] **Step 2: Confirm the failure.** Run: `cargo test --workspace -- presence focus_ seen_requires idle_viewer v3_focus` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS.
- [ ] **Step 5: Commit.** `feat(daemon): route notifications and seen by client presence, including TUI focus`

### Task 9: Held approvals: hook, daemon, protocol

**Files:**
- Modify: `crates/mine-core/src/session.rs` (Claude `--settings` gains `PermissionRequest`), `crates/mine-core/src/hooks.rs` (`run_ask_cmd`), `crates/mine-cli/src/main.rs` (`_event <dir> --ask`), `crates/mine-core/src/proto.rs`, `crates/mine-cli/src/daemon.rs`
- Test: `crates/mine-cli/tests/daemon.rs` (a fake `claude` that runs the configured hook command with a JSON payload on stdin and records its stdout)

**Interfaces:**
- **Consumes:** Task 8's presence (the viewing rule uses `visible && viewing == session`, ignoring the 180 s window)
- **Produces:**
  ```rust
  ToDaemon::Ask { id: String, tool: String, summary: String }          // own short connection, accepted at any version like Poke
  ToDaemon::Answer { id: String, request: u64, allow: bool }
  FromDaemon::AskReply { decision: Decision }
  #[serde(rename_all = "lowercase")] pub enum Decision { Allow, Deny, Pass }
  pub struct PendingAsk { pub request: u64, pub summary: String, pub since_ms: u64 }
  SessionInfo { …, #[serde(default)] pub pending: Option<PendingAsk> }
  pub const PASS_TOOLS: &[&str] = &["AskUserQuestion", "ExitPlanMode"];
  pub fn ask_summary(tool: &str, input: &serde_json::Value) -> String; // "Bash(npm run deploy)", "Edit(src/a.rs)", else tool; ≤ 80 chars
  ```
  - **Hook entry:** `"PermissionRequest": [{"hooks":[{"type":"command","command":"<exe> _event <dir> --ask","timeout":130}]}]`.
  - **`--ask` output:**
    - Allow → `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}`.
    - Deny → the same, with `"behavior":"deny","message":"Denied from mine"`.
    - Pass, or any failure, or the client-side 125 s timeout → no output.
    - Always exit 0.
  - **Daemon:**
    - A `PASS_TOOLS` tool, or any client that is visible and viewing the session → `Pass` at once.
    - Otherwise it holds: the session becomes Needs approval, with waiting text `Allow <summary>?` and a notification per routing.
    - **Releases with Pass** on any of these:
      - a client starts viewing that session;
      - `Input` arrives for that session (release first, then forward);
      - the hold reaches 120 s (or `MINE_ASK_TIMEOUT_MS`);
      - the daemon stops.
    - If the Ask connection drops, the pending entry is removed.
    - `Answer` with a matching `request` replies Allow/Deny. A stale or duplicate answer is ignored with one log line.
    - Features gain `"ask"`.

- [ ] **Step 1: Write the failing tests** (daemon integration, with fake claude):
  - `ask_held_when_nobody_views`: the session list shows `pending`. `Answer{allow:true}` → the hook stdout equals the allow JSON exactly.
  - `ask_denied_has_message`: deny → `behavior":"deny"` and `Denied from mine`.
  - `ask_passes_when_viewed`: a visible client viewing that session → the hook stdout is empty, and it returns within 1 s.
  - `ask_released_when_viewer_arrives`.
  - `ask_released_on_input`: the input is still delivered to the session afterwards.
  - `ask_times_out_to_pass`: `MINE_ASK_TIMEOUT_MS=300` → empty stdout after about 300 ms.
  - `pass_tools_never_held`: `AskUserQuestion` → empty stdout at once.
  - `second_answer_is_ignored`: two `Answer`s → exactly one decision is written, and a later `Answer` with an old `request` doesn't touch a new pending request.
  - `decision_never_has_updated_permissions`: the stdout JSON has no `updatedPermissions` key in either case.
  - `ask_fails_open_without_daemon`: with no daemon socket → exit 0 and empty stdout.
  - Unit `ask_summary_formats`: `Bash` with `{"command":"npm run deploy"}` → `Bash(npm run deploy)`. A 200-char command is truncated to ≤ 80 chars with `…`.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test --workspace -- ask_ pass_tools second_answer decision_never` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS. Run the daemon suite twice more, then `pgrep -fl "mine (daemon|hold)"` → only the user's daemon.
- [ ] **Step 5: Commit.** `feat(daemon): hold Claude permission requests so the app and TUI can allow or deny them`

### Task 10: Allow and Deny in the app and the TUI

**Files:**
- Modify: `crates/mine-app/src/{model.rs,sidebar.rs,tabs.rs}`, `crates/mine-cli/src/client/{mod.rs,ui.rs}` (+ a new `crates/mine-cli/src/client/approvals.rs` panel), `README.md`, `README.ko.md` (an approvals subsection and the `⌃a a` key)
- Test: units in those files

**Interfaces:**
- **Consumes:** Task 9 `SessionInfo.pending`, `ToDaemon::Answer`
- **Produces:**
  - **App:** a session row and its tab with `pending` show **Allow** and **Deny** buttons. Each sends `Answer { id, request, allow }` for that row's `pending.request`.
  - **TUI:**
    - `⌃a a` opens the approvals panel: pending requests oldest first, each as `project/session · summary`.
    - ↑/↓ select, `y` allows the selected request, `n` denies it, Esc closes. With none pending, it shows `No pending approvals`.
    - `⌃a n` / `⌃a p` are unchanged outside the panel.
  - **Strings:** `Allow`, `Deny`, `Approvals`, `No pending approvals`.

- [ ] **Step 1: Write the failing tests.**
  - App `allow_button_sends_answer_with_request_id` and `deny_button_sends_deny`.
  - App `buttons_only_on_pending_rows`.
  - TUI `approvals_panel_lists_oldest_first`.
  - TUI `panel_y_answers_selected_only`: with two pending, select the second → `y` → `Answer` for the second's `request`.
  - TUI `panel_empty_message`.
  - TUI `ctrl_a_n_still_next_session`.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test --workspace -- allow_button deny_button approvals_panel panel_ ctrl_a_n` → FAIL.
- [ ] **Step 3: Implement.** The buttons use the existing colors (`#e5c07b` accent for Allow, dim text for Deny). README: add an "Approvals" subsection under Usage and the `⌃a a` row in the keys table. Mirror both in `README.ko.md`.
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS. Optionally capture the app (D-R4) with a held request from a fake claude in a temp state dir.
- [ ] **Step 5: Commit.** `feat(ui): allow or deny held permission requests from the task list and the TUI`
