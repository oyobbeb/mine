# mine desktop D1 implementation plan: window skeleton and terminal pane

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Running `mine-app` opens a GPUI window. The window has a task list grouped by state, session tabs, a launcher, and a terminal pane. With this you can run the existing claude, codex and shell sessions and spend a whole day on them instead of the TUI and tmux.

**Architecture:**
- Split the repository into a Cargo workspace monorepo: `mine-core` (shared lib), `mine-cli` (executable name `mine`), `mine-app` (GPUI).
- The app is only a client of the daemon. It connects over the same Unix-socket JSON-lines protocol as stage 1. The daemon connection is handled by a tokio runtime on a separate thread and handed to GPUI over futures channels.
- Logic lives in pure modules without GPUI and is unit tested. GPUI views stay thin and are verified with window captures.

**Tech Stack:**
- Existing: Rust (edition 2024), tokio, alacritty_terminal 0.26, serde_json.
- New: gpui 0.2.2 (runtime_shaders), gpui-component 0.4.1, futures.

**Spec:** `docs/superpowers/specs/2026-11-23-mine-desktop-design.md` (the D1 part: the workspace and app structure in 5.1, plus 5.5, 5.6, 5.9). The base document is `docs/superpowers/specs/2026-11-20-mine-multiplexer-design.md`.

## Global Constraints

- **Working location:** worktree `/Users/me/.gitbutler-worktrees/mine/feat-desktop`, branch `feat/desktop`. This branch starts from the tip of `feat/rust-core` (3866764). Do not commit code to the main checkout (`~/dev/mine`, where Codex is working on `dev`).
- **Packages:**
  - `crates/mine-core`: lib, code name `mine_core`
  - `crates/mine-cli`: package `mine-cli`, executable name `mine`
  - `crates/mine-app`: executable name `mine-app`
  - Shared workspace settings are `edition = "2024"` and `rust-version = "1.89"`. The local rustc is 1.94.1.
- **GPUI version pins:** `gpui = { version = "=0.2.2", features = ["runtime_shaders"] }`, `gpui-component = "=0.4.1"`.
  - Both go only into `mine-app`. `mine-core` and `mine-cli` do not pull in GPUI.
  - Why `runtime_shaders` is on: this Mac has no Metal Toolchain. This combination builds in about 1 minute on rustc 1.94.1, and the controller confirmed that the window and Hangul render.
  - Tests may use `gpui`'s `test-support` feature as a dev-dependency.
- **Other new dependencies:** add only `futures` (for `mine-core`'s daemon connection and for `mine-app`). If any other new dependency is needed, give the reason in the commit body. Do not use Python. Build fake programs for tests with sh or Rust.
- **Protocol:**
  - Client↔daemon `VERSION = 3`. D1 adds just one message, `ToDaemon::Focus { focused: bool }`. Bump the version again when D2 adds chat messages.
  - Daemon↔holder `HOLD_VERSION = 1` stays as it is.
- **Colors:** these are the mockup colors the user approved.

  | Use | Color |
  |---|---|
  | Window background | `#1e2128` |
  | List | `#23252b` |
  | Title bar and strips | `#1b1d23` |
  | Selected row | `#2f323a` |
  | Border | `#2c2f36` |
  | Text | `#d0d3da` |
  | Title text | `#e6e6e6` |
  | Dim text | `#8a8f99` / `#6b707a` |
  | Waiting text | `#c9a85c` |
  | Needs approval ◆ | `#e5c07b` |
  | Chip A (claude-a) | `#e5c07b` |
  | Chip B (claude-b) | `#c678dd` |
  | Chip C (codex) | `#56b6c2` |

  shell has no chip.
- **Terminal colors:**
  - Text `#d0d3da`, background `#1e2128`, cursor `#d0d3da`. On connect, the app sends this text and background color to the daemon with `ToDaemon::Colors`.
  - ANSI 0–7: `#1e2128 #e06c75 #98c379 #e5c07b #61afef #c678dd #56b6c2 #d0d3da`
  - ANSI 8–15: `#5c6370 #ef8a92 #b5d99c #f0d197 #8cc5f5 #d898e6 #7fcbd5 #ffffff`
  - 16–255 are the xterm default 6×6×6 cube and gray ramp.
- **Fonts:** the UI uses the GPUI system font; the terminal uses `Menlo` 13px. A `ponytail:` setting comes later.
- **Status:** symbols and order are `◆ Needs approval → ◐ Working → ✓ Done → ○ Idle`. Ended sessions go in the Done group and show as `Exited`. The Idle group starts collapsed.
- **Shortcuts:** when the terminal pane has focus, every key without ⌘ goes to the session.

  | Shortcut | Action |
  |---|---|
  | ⌘N | New task (launcher) |
  | ⌘W | Close session (goes through a confirmation dialog) |
  | ⌘1–⌘9 | Select tab |
  | ⌘[ / ⌘] | Previous / next session |
  | ⌘⇧[ / ⌘⇧] | Previous / next project |
  | ⌘B | Collapse the left list |
  | ⌘C / ⌘V | Copy / paste |
  | ⌘F | Search scrollback |
  | ⌘Q | Quit the app (sessions stay alive) |

- **Screen verification:** when capturing, shoot only the `mine-app` window area. Get the window position and size with System Events `tell process "mine-app" to get {position, size} of window 1`, and shoot with `screencapture -x -R x,y,w,h <png>`. **Never capture the full screen.** The user's other windows are private.
  - Run the verification app with a short temporary `MINE_STATE_DIR` (`mktemp -d /tmp/mXXXX`) and a temporary project folder (containing a `.git` folder). When done, kill the app, its daemon, and its holder processes.
  - Never touch the user's `~/.cargo/bin/mine daemon`.
- **Check command:** `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
- **Commit rules:**
  - Commit with GitButler from the worktree, using `but commit -F - <ids>` without `-b`. Do not use git write commands. One task is one commit.
  - The subject is `type(scope): imperative English summary`. The body says what changed, why, and how it was verified. The last line is `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **Hangul composition:** when typing Hangul in the terminal pane, the character being composed must show at the cursor position. A committed character goes to the session exactly once. No jamo may be dropped or sent twice. → Task 5 `ime_commits_once`
2. **Mode-dependent keys:** Shift+Enter, ⌃C, Esc, the arrow keys (DECCKM), and ⌃A under kitty flag 1 must go as the bytes the session wants. → Task 5 `encode_key_*`
3. **Daemon restart and app restart:** sessions survive when the app quits or the daemon comes back up. The app reattaches and shows the same screen. → Task 2 `link_reconnects_after_daemon_restart`, Task 4 `reattach_after_reconnect`
4. **Window resize and list collapse:** when the size changes, the session gets the new row and column counts. If the size is the same, do not resend `Resize`. → Task 4 `grid_size_*`, `resize_only_on_change`
5. **Approval requests while the app is closed, and Dock launch:** when an approval request arrives, a notification appears; when the app is in front, it does not. A daemon started by an app launched from the Dock still finds `claude`. → Task 7 `notify_*`, `login_env_*`

---

### Task 1: Split into a monorepo

**Files:**
- Create:
  - `Cargo.toml` (workspace)
  - `crates/mine-core/Cargo.toml`
  - `crates/mine-core/src/lib.rs`
  - `crates/mine-core/src/{launch.rs,search.rs,keys.rs,select.rs}` (pure logic moved over)
  - `crates/mine-cli/Cargo.toml`
- Move:
  - `src/{paths,project,session,proto,screen,hooks,conn}.rs` → `crates/mine-core/src/`
  - `src/client/view.rs` → `crates/mine-core/src/view.rs`
  - `src/{main,daemon,hold}.rs` → `crates/mine-cli/src/`
  - `src/client/*` → `crates/mine-cli/src/client/`
  - `tests/{daemon,hold}.rs` → `crates/mine-cli/tests/`
- Modify: `README.md` (install command), `.gitignore` (if needed)

**Interfaces:**
- **Produces** (public modules of `mine_core`): `paths`, `project`, `session`, `proto`, `screen`, `hooks`, `conn`, `view`, plus these newly moved modules:
  - `launch`: `pub enum Choice`, `pub fn next_name(sessions: &[SessionInfo], project: &Path) -> String`, `pub fn new_session(choice: Choice, name: &str, project: &Path) -> Option<NewSession>`, `pub fn tilde(path: &Path, home: Option<&Path>) -> String`. Move what is in `client/launcher.rs` today as it is. `choice_of_key`, `hit`, `draw`, and `KEYS` stay on the TUI side.
  - `search`: `pub fn find(lines: &[String], query: &str) -> Vec<(usize, usize)>`. Moved from `client/copymode.rs`.
  - `keys`: `pub struct KeyModes { .. }` and `pub fn KeyModes::of(screen: &Screen) -> KeyModes`. Moved from `client/mod.rs`, with the fields opened up as `pub`. `mode_diff` stays in the TUI.
  - `select`: `MouseKind`, `MouseEv` (in `client/input.rs` today), `Selection`, `selected_text`, `wants_mouse`, `session_takes`, `encode_sgr`. `osc52`, `local`, `copy`, `paint_selection`, and `Select` stay in the TUI.
- **Consumes:** none (a mechanical move)

- [ ] **Step 1: Create the workspace and move the files.** The root `Cargo.toml` has `[workspace] members = ["crates/*"] resolver = "3"` and `[workspace.package]` (edition, rust-version). Dependencies may be gathered into `[workspace.dependencies]` (implementer's choice). `mine-cli` has `[[bin]] name = "mine"`. Code in `mine-cli` imports with `mine_core::paths` instead of `crate::paths`.
- [ ] **Step 2: Move the shared pure logic into core and make the TUI use it.** Do not change any behavior. Move the existing tests of the moved functions along with them.
- [ ] **Step 3: Verify.** Run: the check command → the existing tests must all pass with none missing. Today that is 271 unit, 24 daemon, and 12 hold. The unit tests split between `mine-core` and `mine-cli`, but the total must be 271. If the total changes, report why. Integration tests import `mine_core::…`, and the binary is still launched with `env!("CARGO_BIN_EXE_mine")` as now. `cargo tree -p mine-cli | grep -c gpui` must be `0`. `cargo run -p mine-cli -- --help` must show the existing help.
- [ ] **Step 4: Update the install and development commands in the README.** `cargo install --path crates/mine-cli`, and `--workspace` in the check command.
- [ ] **Step 5: Commit.** `refactor(workspace): split the monorepo into mine-core and mine-cli`

---

### Task 2: mine-app skeleton, daemon link, list grouped by state

**Files:**
- Create:
  - `crates/mine-app/Cargo.toml`
  - `crates/mine-app/src/main.rs` (app start, open the window)
  - `src/theme.rs` (color constants, ANSI palette)
  - `crates/mine-core/src/link.rs` (the daemon link thread. It has nothing to do with GPUI, so it lives in core and is tested from `mine-cli`'s integration tests)
- Modify: `crates/mine-core/src/conn.rs` (a connect function that takes the daemon executable and environment)
- Test: `crates/mine-cli/tests/link.rs`
  - `src/model.rs` (pure state)
  - `src/workspace.rs` (root view: list | center | collapsed Changes slot)
  - `src/sidebar.rs`

**Interfaces:**
- **Consumes:** `mine_core::proto::{ToDaemon, FromDaemon}`, `mine_core::view::{groups, expand, order, folder_name}`, `mine_core::paths::State`
- **Produces:**
  ```rust
  // mine_core::conn — today spawn_daemon launches the daemon via current_exe(). In the app that is mine-app, which has no daemon subcommand.
  pub async fn connect_or_spawn_async_with(state: &State, daemon_exe: &Path, spawn_env: Option<&[(String, String)]>) -> Result<AsyncConn>
  // The existing connect_or_spawn_async(state) calls this with current_exe() and None. If spawn_env is Some, the daemon is launched after env_clear with that environment + MINE_STATE_DIR (used by Task 7).
  // mine_core::link
  pub enum LinkEvent { Connected, Msg(FromDaemon), Disconnected(String) }
  pub struct DaemonLink { pub tx: futures::channel::mpsc::UnboundedSender<ToDaemon>,
                          pub rx: futures::channel::mpsc::UnboundedReceiver<LinkEvent> }
  pub fn start(state: State, daemon_exe: PathBuf, spawn_env: Option<Vec<(String, String)>>) -> DaemonLink
  pub fn find_mine(override_var: Option<OsString> /*MINE_BIN*/, exe_dir: Option<&Path>, path_var: Option<OsString>) -> Option<PathBuf>
  // Search order: MINE_BIN → `mine` next to the current executable → `mine` on PATH
  // model.rs
  pub enum Effect { Send(ToDaemon) }
  pub struct Model { pub sessions: Vec<SessionInfo>, pub filter: Option<PathBuf>, pub attached: Option<String>,
                     pub toggled: HashSet<usize>, pub sidebar_open: bool, pub connected: bool, /* … */ }
  impl Model { pub fn new(current_project: PathBuf) -> Self;
               pub fn on_link(&mut self, ev: LinkEvent) -> Vec<Effect>;
               pub fn visible_groups(&self) -> Vec<Group>; }
  ```

- [ ] **Step 1: Create `mine_core::link` and add `connect_or_spawn_async_with` to conn.**
  - Connect with `connect_or_spawn_async` in a tokio runtime on a separate `std::thread`.
  - Once connected, send `LinkEvent::Connected`, and pass received messages on as `Msg`.
  - When the connection drops, send `Disconnected(reason)`. After that, reconnect, starting at 0.5 s and backing off to at most 5 s between tries.
  - The model sends Hello, List and Attach after a reconnect, when it receives `Connected`.
  - Messages received on `tx` are sent only while a connection exists. Messages that arrive while there is no connection are dropped.
- [ ] **Step 2: Write the failing tests.**
  - In `#[cfg(test)]` of `crates/mine-app/src/model.rs`:
    - `connected_subscribes_and_reports_colors`: on `Connected` it emits `[Send(List), Send(Colors{fg:"#d0d3da",bg:"#1e2128"})]`.
    - `sessions_group_by_state`: sessions in Approval, Working, Ended, and Idle states are grouped in the order ◆ ◐ ✓ ○, and Idle is collapsed.
    - `disconnect_marks_offline`: on `Disconnected`, `connected == false` and the session list stays.
  - `find_mine_order` in `mine_core::link`: use `MINE_BIN` if it is set; otherwise look next to the executable; otherwise look on PATH. If none exist, return `None` (test with fake executables in a temp folder).
  - `link_reconnects_after_daemon_restart` in `crates/mine-cli/tests/link.rs`:
    - In a temporary state folder, call `start(state, env!("CARGO_BIN_EXE_mine"), None)` and receive `Connected`.
    - SIGKILL the daemon in `daemon.pid`.
    - Within 30 seconds, `Disconnected` and then `Connected` again arrive, and after that `List` is answered with `Sessions`.
    - Clean up the daemon the test started at the end. Follow the cleanup approach in the existing `tests/daemon.rs`.
- [ ] **Step 3: Confirm the failure.** Run: `cargo test -p mine-app && cargo test -p mine-cli --test link && cargo test -p mine-core link` → FAIL
- [ ] **Step 4: Implement the model, theme, and views.** At startup the app finds `mine` with `find_mine(MINE_BIN, executable folder, PATH)`. If it cannot, the window shows `Couldn't find the mine executable · install it with cargo install --path crates/mine-cli`.
  - The window is 1200×800 with background `#1e2128`.
  - At the top of the left list (260px) are "＋ New task ⌘N" and a project picker. The picker has "All projects" and the registered projects, and a project with a session that needs approval gets a `◆`.
  - Below that are the state groups. A row has two lines: the first is the name and chip; the second is `project · waiting text|status · N min ago`. Clicking a group header collapses or expands it. At the very bottom is "＋ Add project" (Task 3 wires up the behavior).
  - While the connection is down, show `Disconnected from the daemon · reconnecting…` above the list.
  - The center is filled in by Tasks 3 and 4. In this task, leave just a "Select a session" placeholder.
  - On the right is the collapsed Changes slot (width 0, D3).
  - You may use `gpui-component`'s `Root`, theme, and sidebar parts. Colors follow the Global Constraints.
  - Omit "N min ago" if `SessionInfo` has no timestamp, and report what you used.
  - Closing the window does not quit the app; it stays in the Dock (macOS convention). Clicking the Dock icon reopens the window (GPUI `on_reopen`). ⌘Q quits only the app; the daemon and sessions stay.
- [ ] **Step 5: Confirm the tests pass and capture the window.**
  - Run: the check command → PASS.
  - In a temporary state folder, create two sessions with `mine new`, then launch `cargo run -p mine-app`. Capture only the window area and put the path in the report.
- [ ] **Step 6: Commit.** `feat(app): add the GPUI window skeleton and task list grouped by state`

---

### Task 3: Tabs, launcher, shortcuts, creating and closing sessions

**Files:**
- Create: `crates/mine-app/src/{tabs.rs,launcher.rs,actions.rs}`
- Modify: `src/model.rs`, `src/workspace.rs`, `src/sidebar.rs`

**Interfaces:**
- **Consumes:** `mine_core::launch::{Choice, next_name, new_session}`, `mine_core::project::register`, Task 2's `Model` and `Effect`
- **Produces:**
  - Model methods: `Model::attach(&mut self, id: &str) -> Vec<Effect>`, `Model::step_session(&mut self, delta: i32) -> Vec<Effect>`, `Model::step_project(&mut self, delta: i32)`, `Model::launch(&mut self, choice: Choice) -> Vec<Effect>`, `Model::tabs(&self) -> Vec<usize>` (the sessions of the selected project, sorted by name)
  - GPUI actions: `NewTask`, `CloseSession`, `SelectTab(usize)`, `PrevSession`, `NextSession`, `PrevProject`, `NextProject`, `ToggleSidebar`

- [ ] **Step 1: Write the failing tests.** All of them go in `model.rs`.
  - `launch_creates_t_n_and_attaches_when_listed`: `launch(Claude A)` emits `New{name:"t1",kind:Claude,account:Some("claude-a")}`. When that session shows up in the list, it emits `Attach`.
  - `tabs_follow_selected_project`: only the selected project's sessions appear in the tabs, sorted by name.
  - `step_session_wraps`: next and previous session cycle through the visible order, and wrap from the end back to the start.
  - `close_requires_confirmation`: `CloseSession` emits `Kill` only after confirmation. The confirmation dialog uses GPUI `window.prompt`. Test the model through the method that receives the confirmation result.
  - `no_session_shows_launcher`: when no session is attached, the launcher is in the open state.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-app model` → FAIL
- [ ] **Step 3: Implement.**
  - **Tabs:** at the top of the center. A tab has the status symbol, the name, and the chip, with a "＋" at the end.
  - **Launcher:** in the center. Same content as the stage 1 TUI.
    - The project name and its `~` path, "New session"
    - ↵ claude-a [A] · b claude-b [B] · x codex [C] · s shell · n Custom name and model
    - `⌘W close`
  - **Launcher behavior:**
    - A row can also be chosen by clicking it.
    - `n` opens a `gpui-component` input box. The input uses the `mine_core::view::parse_new` format.
    - If it was opened with ⌘N over an attached session, Esc closes it.
    - In D1, all claude and codex sessions are in terminal mode. The "Open in terminal" choice is added in D2.
  - **Shortcuts:** bind them as in the Global Constraints table.
  - **"＋ Add project":** pick a folder (`cx.prompt_for_paths`, folders only), register the chosen folder with `project::register`, and select that project. Follow the rule of not registering the home directory.
- [ ] **Step 4: Confirm the tests pass and capture the window.** Run: the check command → PASS. Capture the window area showing the launcher and the tabs.
- [ ] **Step 5: Commit.** `feat(app): add session tabs and a launcher that starts with one key`

---

### Task 4: Drawing the terminal pane

**Files:**
- Create:
  - `crates/mine-app/src/terminal/mod.rs` (view; attach, size, and scroll state)
  - `crates/mine-app/src/terminal/render.rs` (pure: cells → text runs)
- Modify: `src/model.rs` (keep the screen up to date from Snapshot and Output), `src/workspace.rs`

**Interfaces:**
- **Consumes:** `mine_core::screen::Screen` (`new`, `feed`, `snapshot`, `resize`, `scroll`, `term`, `cursor`, `mode`, `sync_deadline`, `end_sync`, `take_replies`), the ANSI palette from `theme`
- **Produces:**
  ```rust
  // render.rs
  pub struct CellRun { pub col: u16, pub cells: u16, pub text: String, pub fg: u32 /*0xRRGGBB*/, pub bg: Option<u32>,
                       pub bold: bool, pub italic: bool, pub underline: bool, pub strike: bool, pub dim: bool,
                       pub link: Option<String> }
  pub fn row_runs(screen: &Screen, row: usize) -> Vec<CellRun>   // a row visible on screen (0-based)
  pub fn grid_size(pane_w: f32, pane_h: f32, cell_w: f32, line_h: f32) -> (u16, u16)  // minimum (1,1)
  pub fn scroll_lines(acc: &mut f32, delta_px: f32, line_h: f32) -> i32
  ```

- [ ] **Step 1: Write the failing tests.**
  - `render.rs`:
    - `row_runs_merges_same_style`: `"ab\x1b[31mcd"` → two runs. The second run's fg is `0xe06c75`.
    - `row_runs_wide_char_counts_two_cells`: in `"한a"`, `한` has `cells == 2` and the next run has `col == 2`.
    - `row_runs_inverse_swaps_colors`
    - `row_runs_keeps_link`: a run of OSC 8 link cells has `link == Some(url)`.
    - `row_runs_indexed_and_truecolor`: index 196 and `38;2;1;2;3` give `0xff0000` and `0x010203` respectively.
    - `grid_size_floors_and_clamps`: `(800.0, 600.0, 8.0, 17.0)` → `(100, 35)`. `(0.0, 0.0, 8.0, 17.0)` → `(1, 1)`.
    - `scroll_lines_accumulates`
  - `model.rs`:
    - `resize_only_on_change`: reporting the same size twice sends `Resize` only once.
    - `reattach_after_reconnect`: when attached and `Disconnected` then `Connected` arrive, it emits `Attach` again for the same session.
    - `snapshot_builds_screen_at_pane_size`
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-app` → FAIL
- [ ] **Step 3: Implement.**
  - The terminal pane draws only the visible rows. For each row, it builds GPUI text runs and background rectangles from `row_runs`.
  - The cursor shape (block, underline, or bar) follows the session mode. Block if unknown.
  - While scrolled back into the past, hide the cursor.
  - The cell size comes from the width of 'M' and the line height at `Menlo` 13px.
  - When the pane size changes, compute rows and columns with `grid_size`. Send `Resize` only when they changed; at first, send `Attach` with that size.
  - On `Snapshot`, build a new `Screen` of that size and feed it; feed `Output` onto it as it comes. Discard the client `Screen`'s query replies (`take_replies`). The daemon does the replying.
  - When `sync_deadline` passes, call `end_sync` and redraw. Use a GPUI timer.
  - Trackpad scrolling is converted to a line count with `scroll_lines` and passed to `screen.scroll`. If the session has turned on mouse reporting, Task 6 handles it.
  - If the mouse is released at the same spot where it was pressed on a link cell (no drag), open the link with `cx.open_url`.
  - Redraw on every Output, but coalesce to at most once per frame.
- [ ] **Step 4: Confirm the tests pass and capture the window.**
  - Run: the check command → PASS.
  - In a temporary state folder, create a shell session and type `printf '\e[31m빨강\e[0m 한글 테스트\n'; ls -la`. Capture this terminal pane. Input does not exist before Task 5, so substitute `mine new` and a fake shell that runs the command beforehand.
- [ ] **Step 5: Commit.** `feat(app): draw the session screen in the terminal pane and match its size and scroll`

---

### Task 5: Terminal input: keys, Hangul input method, paste

**Files:**
- Create:
  - `crates/mine-app/src/terminal/keys.rs` (pure: key → bytes)
  - `crates/mine-app/src/terminal/ime.rs` (pure: composition state)
- Modify: `src/terminal/mod.rs` (GPUI key events and the `EntityInputHandler` implementation)

**Interfaces:**
- **Consumes:** `mine_core::keys::KeyModes` (Task 1), `Screen::mode` (BRACKETED_PASTE)
- **Produces:**
  ```rust
  pub struct KeyIn { pub key: String, pub key_char: Option<String>, pub ctrl: bool, pub alt: bool, pub shift: bool, pub cmd: bool }
  pub fn encode_key(k: &KeyIn, m: &KeyModes) -> Option<Vec<u8>>      // ⌘ combinations and plain characters give None (characters go through the input-method path)
  pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8>
  pub struct Ime { /* text being composed */ }
  impl Ime { pub fn mark(&mut self, text: &str); pub fn commit(&mut self, text: &str) -> Vec<u8>; pub fn unmark(&mut self);
             pub fn marked(&self) -> Option<&str>; }
  ```

- [ ] **Step 1: Write the failing tests.**
  - `encode_key_*` in `keys.rs`. Each row of the table is one `assert_eq!`. "kitty1" is the mode with kitty flag 1 (disambiguate) on, and "mok2" is modifyOtherKeys 2.

    | Input | Mode | Expected |
    |---|---|---|
    | enter | default | `\r` |
    | shift+enter | default | `\r` |
    | shift+enter | kitty1 | `\x1b[13;2u` |
    | shift+enter | mok2 | `\x1b[27;2;13~` |
    | escape | default | `\x1b` |
    | escape | kitty1 | `\x1b[27u` |
    | ctrl+c | default | `\x03` |
    | ctrl+a | kitty1 | `\x1b[97;5u` |
    | up | default | `\x1b[A` |
    | up | DECCKM | `\x1bOA` |
    | shift+up | default | `\x1b[1;2A` |
    | backspace | default | `\x7f` |
    | tab | default | `\t` |
    | shift+tab | default | `\x1b[Z` |
    | home / end / pageup / pagedown / delete | default | `\x1b[H` `\x1b[F` `\x1b[5~` `\x1b[6~` `\x1b[3~` |
    | f1 / f5 | default | `\x1bOP` `\x1b[15~` |
    | cmd+c | any mode | `None` |
    | plain character `a` | default | `None` |

  - `paste_bytes_wraps_when_bracketed`
  - `ime.rs`:
    - `ime_commits_once`: when `mark("ㅎ")`, `mark("하")`, `mark("한")`, `commit("한")` arrive in order, `commit` returns `"한".as_bytes()`. After that, `marked() == None`.
    - `ime_unmark_sends_nothing`
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-app terminal` → FAIL
- [ ] **Step 3: Implement.**
  - **Key path:** in the terminal pane's key input, if `encode_key` returns `Some`, send it as `Input`.
  - **Character path:** character input is received through GPUI's `EntityInputHandler`.
    - `replace_and_mark_text_in_range` is `Ime::mark`.
    - `replace_text_in_range` is `Ime::commit`, and the returned bytes are sent as `Input`.
    - `bounds_for_range` returns the cursor cell rectangle. It is used for the position of the input method's candidate window.
  - **Composition display:** draw the character being composed at the cursor position with an underline.
  - **kitty flags:** honor only flag 1. `ponytail:` ignore 2, 4, 8, and 16 in D1, and widen this when an app requests them.
  - **Option key:** produce characters as macOS does by default. A setting to use Option as Meta comes later (`ponytail:`).
  - **Paste:** ⌘V sends the clipboard text through `paste_bytes`, following the session's BRACKETED_PASTE mode.
  - If the view was scrolled back when a key is sent, return to the bottom.
- [ ] **Step 4: Confirm the tests pass.**
  - Run: the check command → PASS.
  - In a shell session in a temporary state folder, launch `cat -v`. Check that ⌃C, the arrow keys, and Shift+Enter (after the session turns on modifyOtherKeys with `printf '\e[>4;2m'`) go as the expected bytes. Keys may be sent with System Events `keystroke` and `key code`. Capture the window.
  - Hangul composition is hard to automate, so write "needs user confirmation" in the report.
- [ ] **Step 5: Commit.** `feat(app): send keys and Hangul input in the terminal pane to match the session mode`

---

### Task 6: Mouse selection and copy, mouse reporting, ⌘F find

**Files:**
- Create:
  - `crates/mine-app/src/terminal/mouse.rs` (pure: position → cell, selection state)
  - `crates/mine-app/src/terminal/find.rs` (find box state)
- Modify: `src/terminal/mod.rs`

**Interfaces:**
- **Consumes:** `mine_core::select::{Selection, selected_text, wants_mouse, session_takes, encode_sgr, MouseEv, MouseKind}`, `mine_core::search::find`, `Screen::history_text`
- **Produces:**
  ```rust
  pub fn cell_at(x: f32, y: f32, cell_w: f32, line_h: f32, cols: u16, rows: u16) -> (u16, u16)  // clamped to inside the pane
  pub struct FindState { pub query: String, pub hits: Vec<(usize, usize)>, pub current: usize }
  impl FindState { pub fn search(&mut self, lines: &[String]); pub fn next(&mut self) -> Option<(usize,usize)>; pub fn prev(&mut self) -> Option<(usize,usize)>; }
  ```

- [ ] **Step 1: Write the failing tests.**
  - `cell_at_clamps`
  - `shift_drag_selects_even_when_session_wants_mouse`: even if the session wants the mouse, dragging with Shift held makes a selection. Test it at the model level.
  - `mouse_forwarded_when_session_wants`: SGR bytes go to a session that has mouse reporting on. Coordinates are 1-based.
  - `find_wraps_both_ways`
  - `copy_uses_selected_text`: the range selected by dragging equals `selected_text`. Lines that were auto-wrapped are joined.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-app terminal` → FAIL
- [ ] **Step 3: Implement.**
  - **Selection:** dragging selects. The selected area keeps its text as it is, on a `#2f323a` background. ⌘C copies with `cx.write_to_clipboard`. Clicking clears the selection. New output does not clear the selection. It is based on screen cells, so scrolling clears it.
  - **Mouse reporting:** if the session has mouse reporting on and Shift is not held, send press, release, drag, and wheel with `encode_sgr`. The rules are the same as stage 1 Task 10 (including R26's release outside the pane).
  - **⌘F:** opens a find box at the top right of the terminal pane (a `gpui-component` input box). Typing searches with `find`. Enter is next, Shift+Enter is previous, and Esc closes it. Scroll so the current match line is visible, and paint the matches in the selection color. Matching is case-insensitive.
- [ ] **Step 4: Confirm the tests pass and capture the window.** Run: the check command → PASS. In `seq 1 500` output, find `250` with ⌘F and capture that screen.
- [ ] **Step 5: Commit.** `feat(app): add drag-to-copy, mouse reporting, and find to the terminal pane`

---

### Task 7: Daemon side: Focus, approval notifications, login shell environment

**Files:**
- Modify:
  - `crates/mine-core/src/proto.rs` (`VERSION = 3`, `ToDaemon::Focus`)
  - `crates/mine-core/src/conn.rs` (the environment when launching the daemon)
  - `crates/mine-cli/src/daemon.rs` (per-client focused, notifications)
  - `crates/mine-app/src/{link.rs,workspace.rs}` (report window focus, launch with the login shell environment)
- Create: `crates/mine-core/src/env.rs`
- Test: `crates/mine-cli/tests/daemon.rs`

**Interfaces:**
- **Produces:**
  ```rust
  // proto.rs
  ToDaemon::Focus { focused: bool }
  // env.rs
  pub fn parse_env0(bytes: &[u8]) -> Vec<(String, String)>
  pub fn login_env(shell: &str, timeout: std::time::Duration) -> Option<Vec<(String, String)>>   // `$shell -i -l -c '<NUL marker>; env -0'`, setsid, stdin /dev/null; parses after the marker (D-R13)
  // conn.rs: uses Task 2's connect_or_spawn_async_with(state, daemon_exe, spawn_env) as it is. This task fills in spawn_env.
  ```

- [ ] **Step 1: Write the failing tests.**
  - `env.rs`:
    - `parse_env0_splits_nul_and_first_equals`: `b"PATH=/a:/b\0X=1=2\0"` → `[("PATH","/a:/b"),("X","1=2")]`.
    - `login_env_reads_fake_shell`: if a temporary shell script prints `printf 'PATH=/fake/bin\0'`, `PATH` comes in.
    - `login_env_times_out`: a shell that never finishes gives `None` after the timeout.
  - `tests/daemon.rs`:
    - `notify_on_approval_when_unfocused`: set `MINE_NOTIFY` to a fake script that appends the arguments it receives to a file. Create a session and send an approval hook with `_event`; the notification is called once, and the arguments contain the session name.
    - `no_notify_when_a_client_is_focused`: it is not called after a client sends `Focus{focused:true}`.
    - `spawned_daemon_uses_given_env`: launch the daemon with `connect_or_spawn_async_with(state, CARGO_BIN_EXE_mine, Some(PATH=<temp bin>…))`. The `env` dump of a shell session that daemon creates contains that PATH. The fake-shell approach follows the existing env test.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test --workspace notify login_env parse_env0 spawned_daemon` → FAIL
- [ ] **Step 3: Implement.**
  - **Focus:** the daemon stores `focused` per client. When a connection drops, that client's value goes away.
  - **Notification:** when a session **newly changes** to Needs approval, if there is no focused client at all, show a notification.
    - The notification command is `MINE_NOTIFY`, defaulting to `osascript`.
    - The arguments are `-e 'display notification "<waiting text or Needs approval>" with title "mine" subtitle "<project folder>/<session>"'`. Quotes and backslashes are escaped.
    - Launch it outside the global lock, and do not wait for it. A thread reaps the child.
    - Do not launch it twice for the same transition.
  - **App-side Focus:** the app sends `Focus` when the app window is activated or deactivated.
  - **App launching the daemon:** at startup the app reads `login_env($SHELL or /bin/zsh, 3 seconds)` once and passes it as `link::start`'s `spawn_env`. On failure (the shell fails, times out, or never prints the marker) it is `None`, meaning the app's own environment. `find_mine`'s PATH also uses this environment's PATH first.
  - **Environment of the launched daemon:** after `env_clear`, add only `MINE_STATE_DIR` to the given environment. The holder process's rules for cleaning runtime variables still apply.
  - **TUI:** keeps using `None`.
  - **Version mismatch:** the error text uses the existing `restart_hint`.
- [ ] **Step 4: Confirm the tests pass.** Run: the check command → PASS. Run the daemon test suite 3 times in a row to see that it is not flaky. Use `pgrep -fl "mine (daemon|hold)"` to confirm the tests left no processes behind. The user's daemon is the exception.
- [ ] **Step 5: Commit.** `feat(daemon): take app focus and show approval requests as macOS notifications`

---

### Task 8: .app bundle and `mine app`

**Files:**
- Create: `scripts/bundle-app.sh`
- Modify: `crates/mine-cli/src/main.rs` (the `App` subcommand), `README.md` (desktop app section)

**Interfaces:**
- **Produces:**
  ```rust
  pub enum AppLaunch { Open(PathBuf) /* .app */, Exec(PathBuf) /* mine-app binary */, Missing }
  pub fn app_launch(bundles: &[PathBuf], exe_dir: Option<&Path>, path_lookup: Option<PathBuf>) -> AppLaunch
  ```

- [ ] **Step 1: Write the failing test.** `app_launch_prefers_bundle_then_sibling_then_path`:
  - If the first `.app` exists, it is `Open`.
  - Otherwise, if `mine-app` is next to the current executable, it is `Exec`.
  - Otherwise, if `mine-app` is on PATH, it is `Exec`.
  - If none exist, it is `Missing`.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-cli app_launch` → FAIL
- [ ] **Step 3: Implement.**
  - **`scripts/bundle-app.sh [--dest DIR]`** (default `$HOME/Applications`):
    - Run `cargo build --release -p mine-app -p mine-cli` and put both `mine-app` and `mine` in `mine.app/Contents/MacOS/`. This way the bundled app starts the daemon with the `mine` next to it (the second item in `find_mine`'s order).
    - `Info.plist` gets `CFBundleName=mine`, `CFBundleIdentifier=dev.mine.app`, `CFBundleExecutable=mine-app`, `CFBundlePackageType=APPL`, `CFBundleShortVersionString=<package version>`, `LSMinimumSystemVersion=13.0`, `NSHighResolutionCapable=true`.
    - No icon and no signing (`ponytail:` stage 8).
    - Use `set -euo pipefail`.
  - **`mine app`:**
    - The bundle paths it searches are `~/Applications/mine.app` and `/Applications/mine.app`.
    - On `Open`, run `open <path>`.
    - On `Exec`, launch that binary detached (setsid) and return immediately.
    - On `Missing`, print `Couldn't find mine-app. Build it with scripts/bundle-app.sh or run cargo install --path crates/mine-app` and exit with a non-zero code.
  - **README:** add a "Desktop app" section (building, running, the shortcut table).
- [ ] **Step 4: Verify.**
  - Run: the check command → PASS.
  - Build with `scripts/bundle-app.sh --dest <temp folder>`, then check the bundle structure and `plutil -lint`.
  - Launching `open -n <temp>/mine.app` with a temporary `MINE_STATE_DIR` environment is hard, so run the binary inside the bundle directly with that environment and capture that the window appears.
  - **Do not install into the user's home `~/Applications`.** The controller asks the user and does the installation.
- [ ] **Step 5: Commit.** `feat(app): add the mine.app bundle script and the mine app command`
