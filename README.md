<div align="center">

# mine

**Your coding-agent sessions, never interrupted.**

Claude Code and Codex sessions that keep running across windows, accounts, machines, and dropped connections.

[Install](#install) &nbsp;•&nbsp; [Quick start](#quick-start) &nbsp;•&nbsp; [How it works](#how-it-works) &nbsp;•&nbsp; [Roadmap](#roadmap) &nbsp;•&nbsp; [한국어](README.ko.md)

<img src="docs/images/mine-app.png" alt="The mine desktop app: a task list grouped by status on the left, session tabs on top, and a live Claude Code session in the terminal pane" width="880">

</div>

## About

Agent work rarely fits in one sitting. A refactor outlives your terminal window, an account hits its usage limit halfway through, or you want to check on a task from another machine. Each time, the session — and the context built up in it — is the thing you lose.

mine treats a session as something that outlives everything around it. A small daemon owns every Claude Code, Codex, and shell session. Windows and SSH connections come and go, and you pick the session back up where you left it: in the desktop app, in a terminal over SSH, and eventually on another account or another host.

> [!NOTE]
> mine is early and shaped around one person's workflow on macOS (Apple Silicon). Sessions that survive disconnects work today; account handoff and remote hosts are next. See the [roadmap](#roadmap).

## Highlights

- **Sessions outlive their window.** Each session's PTY is held by its own holder process. Close the app, lose SSH, or restart the daemon, and `claude`, `codex`, and your dev server keep running. Reattach and the screen comes back.
- **Exited conversations resume.** A Claude or Codex session whose process ended stays in the list as *Exited*. Select it and the same conversation starts again.
- **Two Claude accounts, one list.** Run sessions on two separately logged-in Claude accounts side by side. Each shows as your login name or a label you choose.
- **See what needs you.** Sessions from every project are grouped as *Needs approval → Working → Done → Idle*, driven by the stock CLI hooks.
- **One key to start.** The launcher creates a Claude, second-account Claude, Codex, or shell session with a single key.
- **The real CLIs.** Sessions run stock Claude Code and Codex. Keyboard modes (kitty keyboard, modifyOtherKeys) pass through, so Shift+Enter just works.
- **Desktop app and terminal UI.** A GPUI desktop app for daily use and a terminal UI for SSH, both attached to the same daemon.

## Install

Requires macOS and Rust 1.89 or newer. Install and log in to the agent CLIs you want to use (`claude`, `codex`) separately.

```sh
git clone https://github.com/oyobbeb/mine.git
cd mine
cargo install --path crates/mine-cli
```

Make sure `~/.cargo/bin` is on your `PATH` (`command -v mine`).

The desktop app is built separately. See [Desktop app](#desktop-app).

## Quick start

Run mine inside a project. The nearest Git work tree becomes the project.

```sh
mine            # open the terminal UI; starts the daemon if needed
mine new dev    # create a shell session named "dev"
mine ls         # list sessions
```

Inside the terminal UI, press the prefix `⌃a` (Control+a), release it, then press a command key. `⌃a d` detaches: the UI closes and every session keeps running. Run `mine` again to get back.

## Desktop app

The desktop app shows the same sessions as the terminal UI in a window. It attaches to the same daemon and starts it if needed.

### Build

```sh
scripts/bundle-app.sh                  # builds ~/Applications/mine.app
scripts/bundle-app.sh --dest ./out     # or builds ./out/mine.app
```

The script makes a release build and puts `mine-app` and `mine` in `mine.app/Contents/MacOS`, so the app starts the daemon with the `mine` next to it. The bundle has no icon and isn't signed. To skip the bundle, `cargo install --path crates/mine-app` installs just the `mine-app` binary.

### Run

```sh
mine app
```

`mine app` opens `~/Applications/mine.app`, then `/Applications/mine.app`. If neither exists, it starts a `mine-app` next to `mine` or on your `PATH`. You can also open the bundle from Finder or Spotlight. Started as a binary from a project folder, the app opens that project; started from the Dock or Finder, it uses your home folder.

Closing the window keeps the app in the Dock, and clicking the Dock icon brings the window back. ⌘Q quits only the app; the daemon and your sessions keep running.

### Shortcuts

When the terminal pane has focus, every key without ⌘ goes to the session.

| Key | Action |
|---|---|
| ⌘N | New task (launcher) |
| ⌘W | Close the session (asks first) |
| ⌘1–⌘9 | Select a tab |
| ⌘[ / ⌘] | Previous / next session |
| ⌘⇧[ / ⌘⇧] | Previous / next project |
| ⌘B | Collapse / expand the task list |
| ⌘C / ⌘V | Copy / paste |
| ⌘F | Find in the scrollback (Enter next, ⇧Enter previous, Esc close) |
| ⌘Q | Quit the app |

## Usage

### Commands

```sh
mine                                  # open the terminal UI
mine ls                               # list sessions (tab-separated, script-friendly)
mine new review --kind claude --account claude-b --model opus
mine new dev                          # kind defaults to shell
mine kill review                      # close a session (its directory and history stay)
mine projects                         # list registered projects
mine --project ~/dev/app new dev      # pick the project explicitly
```

- `mine ls` prints `symbol project name account state waiting-text` per line, tab-separated, with `-` for empty fields.
- Session names are unique per project: up to 64 characters, starting with a letter or digit, using letters, digits, and `_~-`.
- `--project` goes before the subcommand. mine walks up from that folder to find the project.

### Keys

| Key | Action |
|---|---|
| `⌃a ⌃a` | Send a literal `⌃a` to the session |
| `⌃a n` / `⌃a p` | Next / previous session |
| `⌃a )` / `⌃a (` | Next / previous project |
| `⌃a 0` | All projects |
| `⌃a c` | New session (launcher) |
| `⌃a x` | Close the current session (`y` to confirm) |
| `⌃a b` | Collapse / expand the list |
| `⌃a [` | Copy mode |
| `⌃a d` | Detach (sessions keep running) |

<details>
<summary><b>Launcher</b></summary>

When no session is attached, the terminal pane shows the launcher. `⌃a c` opens it over the current session.

| Key | Action |
|---|---|
| Enter / `b` / `x` / `s` | Create a Claude (first account) / Claude (second account) / Codex / shell session and attach |
| `n` | Type `<claude-a\|claude-b\|codex\|shell> <name> [model]` yourself |
| Esc | Close a launcher opened over a session |

New sessions get the smallest unused name in the project: `t1`, `t2`, …. Clicking a menu row does the same. If a Korean input method is active when you press a command key, the status line asks you to switch to English.

</details>

<details>
<summary><b>Copy mode</b></summary>

Freezes the scrollback so you can move around and copy with the keyboard.

| Key | Action |
|---|---|
| `h` `j` `k` `l` / arrows | Move the cursor |
| `g` / `G` | Top / bottom |
| `0` / `$` | Start / end of line |
| PgUp / PgDn, `⌃b` / `⌃f` | Page up / down |
| `⌃u` / `⌃d` | Half page up / down |
| `/` / `?` | Search forward / backward (case-insensitive) |
| `n` / `N` | Next match, same / opposite direction |
| `v` | Start / clear selection |
| `y` / Enter | Copy and exit |
| `q` / Esc | Exit |

</details>

<details>
<summary><b>Mouse</b></summary>

- Wheel scrolls the session's scrollback, or goes to the program if it requested mouse input.
- Drag selects; releasing copies to the clipboard over OSC 52.
- Click picks a project, a session, a group header (collapse / expand), or a launcher row.

</details>

## Configuration

### Account labels

The UI shows a short name for each account instead of `claude-a` / `claude-b`:

1. A label you set in `~/.mine/config.toml`:

   ```toml
   [accounts.claude-a]
   label = "personal"

   [accounts.claude-b]
   label = "work"
   ```

2. Otherwise, the part before `@` in that Claude profile's login email. Codex shows as `Codex`.
3. Otherwise, the account id.

mine never reads or stores credentials. For automatic names it reads a single field, the login email in `.claude.json`, and never logs it. Labels are display-only: `mine ls`, `mine new --account`, and the launcher's `n` prompt use the account ids.

### State directory

State lives in `~/.mine` (override with `MINE_STATE_DIR`).

```text
~/.mine/
  daemon.sock           Unix socket clients attach to
  daemon.pid            lock file that keeps a single daemon running
  config.toml           account labels (optional)
  projects/<key>.json   registered project paths
  sessions/<id>/        meta.json, events.jsonl (hook events), exit
  run/                  holder process sockets
  logs/                 daemon.log, hold-<id>.log
  profiles/claude-b/    CLAUDE_CONFIG_DIR for the second Claude account
```

### Upgrading

A running daemon keeps its old code after you install a new build. Stop it once and the next `mine` starts the new one. Sessions survive because their holders keep running.

```sh
cargo install --path crates/mine-cli
kill "$(cat ~/.mine/daemon.pid)"
mine
```

If you installed an earlier build whose package was named `mine`, the first reinstall needs `cargo install --path crates/mine-cli --force` (the package is now `mine-cli`).

## How it works

```mermaid
flowchart LR
    App["mine-app<br/>desktop"] --> Daemon
    TUI["mine<br/>terminal UI"] --> Daemon
    CLI["mine ls / new / kill"] --> Daemon
    Daemon["mine daemon<br/>sessions · state · screens"] --> H1["mine hold"] --> S1["claude"]
    Daemon --> H2["mine hold"] --> S2["codex / shell"]
    Hooks["stock CLI hooks"] -->|"mine _event"| Daemon
```

- **Clients** (`mine-app`, `mine`) draw and take input. They talk to the daemon in JSON lines over a Unix socket, and any number can attach at once.
- **The daemon** keeps the session list, each session's state, and a terminal screen per session ([alacritty_terminal](https://github.com/alacritty/alacritty)). The first `mine` starts it.
- **Holders** (`mine hold`), one per session, own the PTY. They are why a session outlives the client and the daemon itself.

| Crate | Role |
|---|---|
| [`mine-core`](crates/mine-core) | Shared library: protocol, session types, screens, daemon link |
| [`mine-cli`](crates/mine-cli) | The `mine` binary: commands, daemon, holder, terminal UI |
| [`mine-app`](crates/mine-app) | The `mine-app` binary: GPUI desktop app |

## Roadmap

| Stage | What | Status |
|---|---|---|
| 1 | Daemon, holders, terminal UI, status-grouped list, copy mode | ✅ Done |
| D1 | Desktop app shell: task list, session tabs, launcher, terminal pane | 🚧 In progress (input, mouse, notifications, `.app` bundle left) |
| D2 | Chat view for Claude and Codex with approval buttons and subagent cards | Planned |
| D3 | Changes panel: uncommitted tree and commit list | Planned |
| 2 | Usage limits and one-key handoff of a task to another account, with context | Planned |
| 3 | Restore sessions after a reboot | Planned |
| 4 | Git graph and files pane | Planned |
| 5 | Remote hosts in the same list, over SSH | Planned |
| 6 | Optional master session that drives the others | Planned |
| 7 | Headless browser pane | Planned |
| 8 | Homebrew install | Planned |

Design docs: [multiplexer design](docs/superpowers/specs/2026-11-20-mine-multiplexer-design.md) · [desktop app design](docs/superpowers/specs/2026-11-23-mine-desktop-design.md).

## Development

```sh
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

- Each commit holds one behavior you can explain and revert on its own, and passes the check above.
- Commit subjects follow [Conventional Commits](https://www.conventionalcommits.org) in English: `type(scope): imperative summary`, with `feat|fix|refactor|docs|chore|style|perf|test`. The body says what changed, why, and how it was verified.
- Design docs and implementation plans live in [`docs/superpowers`](docs/superpowers).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
