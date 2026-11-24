# mine design: a personal terminal multiplexer for agent sessions

- Written: 2026-11-20
- Status: awaiting user review
- Mockups: [layout.html](2026-11-20-mine-mockups/layout.html) (overall layout, expand/collapse, browser pane), [project-rail.html](2026-11-20-mine-mockups/project-rail.html) (project picker. **Option 1, the project rail, is the chosen design**; options 2 and 3 are kept for comparison)

## 1. Intent

Today the user opens tmux in Ghostty and manages two Claude Code accounts (claude-a, claude-b) and several Codex sessions. This setup has four pain points.

- Sessions are scattered across projects, so it isn't clear at a glance what is running where.
- When one account's usage limit runs out, moving to another account means re-explaining the context by hand.
- Moving between sessions on remote machines is a hassle.
- Checking git state takes a separate step.

mine is **a single-user terminal multiplexer that replaces tmux inside Ghostty**. The long-term direction to hold onto is that the tool answers "which agent or account should I give this task to right now?", not "which terminal should I connect to?".

There is one user. This tool is not a service offered to other people.

## 2. Goals · non-goals · failure conditions

**Goals**

1. Create, switch, and end sessions per project. Sessions keep running when the screen is closed or SSH drops.
2. See the status of every project's sessions (Needs approval, Working, Done, Idle) at a glance through the project rail and the status list.
3. Get a warning when an account is close to its usage limit. Hand the same task, with its context, to another account with one keypress.
4. Sessions on remote machines appear in the same list. SSH in from outside and run `mine` to get the same screen.
5. The selected project's git graph is always shown beside the session, close to VS Code's Git Graph.
6. Occasionally check the screen of the browser an agent runs headlessly.
7. Optionally designate one master session, so work keeps moving even if the user talks only to that session.
8. After installation, typing `mine` in any folder runs it.

**Non-goals (not doing for now)**

- A GUI window of its own. Attach one later on the same core with GPUI if needed. → Reversed on 2026-11-23. The main screen is the GPUI desktop app ([2026-11-23-mine-desktop-design.md](2026-11-23-mine-desktop-design.md)).
- Reimplementing stock CLI features. → Changed the same day. claude and codex sessions default to the chat view, and the stock screen remains as "Open in terminal" (desktop design 5.3).
- A browser you can click inside the window. The browser runs headless only, and its screen is checked as an image.
- A mobile app, QR pairing, a relay server.
- Reimplementing work splitting inside a session. Subagents, agent teams, and workflows use Claude Code's own features as they are.
- Automatic account switching without user confirmation. Until the terms of service are checked, a one-keypress confirmation stays.
- Windows support.
- Splitting the screen within one session (tmux pane). One session is one terminal.

**Failure conditions (counted as failure even if it works technically)**

- The Claude Code or Codex screen breaks inside mine, or colors, shortcuts, pasting, or Korean input behave differently from the stock CLI. Or scrolling, selecting, and copying are less comfortable than in the current Ghostty + tmux. In that case the user goes back to Ghostty + tmux.
- Input latency is noticeably higher than using Ghostty directly.
- The user has to re-explain the background when an account is handed off.
- The master runs hard-to-reverse actions such as deploying, pushing, or deleting without asking.
- A session dies because of screen exit, SSH drop, client error, mine upgrade, or daemon error.

## 3. Constraints

- **Platform:** macOS (Apple Silicon) is the baseline. Remote hosts are macOS or Linux.
- **Terminal:** Ghostty is assumed. In a terminal that can't show images (kitty graphics protocol), the git graph is drawn as a text graph instead.
- **Authentication is left to the official CLIs:**
  - Claude accounts log in with a separate `CLAUDE_CONFIG_DIR` per account.
  - Codex uses the user's existing login.
  - mine neither stores nor relays credentials.
- **Usage sources:**
  - Claude: `rate_limits.five_hour` / `seven_day` in the JSON that arrives through the stock session's statusLine command. The values exist only after the first model response.
  - Codex: `account/rateLimits/read` on `codex app-server`.
  - Every value carries its observation time. The "unknown" state is treated as a normal state.
- **Language:** Rust. The daemon and the UI both live in the single `mine` binary. The Python prototype in the current repository is to be replaced and is used only as a behavior reference.
- **Development rules:** follow the README. Linear `main`/`dev` branches, atomic commits, `type(scope): imperative English summary`, tests pass on every commit.

## 4. Approaches considered

| Decision | Options considered | Chosen | Reason |
|---|---|---|---|
| Display medium | ① TUI inside Ghostty ② a separate desktop app (Tauri or Electron) | ① TUI | The user likes Ghostty. The same screen is available anywhere over SSH. Less to build. ② only made sense if a clickable browser inside the window was needed, and that reason disappeared once the browser became headless. |
| Later GUI | Tauri / Electron / GPUI | GPUI (later) | Without a webview, Tauri has almost no advantage. GPUI keeps everything from core to screen in Rust. It's pre-1.0, so it isn't used now. |
| Session engine | ① build on top of tmux ② mine as its own multiplexer (the zellij way) ③ drive tmux control mode (`-CC`) and render | ② | ① breaks images inside tmux, so the git graph can't be drawn as an image, and the layout is tied to tmux. ③ makes keeping the two sides' state in sync tricky. ② has been proven by zellij as the same structure in Rust, and terminal rendering can use `alacritty_terminal`, which Zed has validated. |
| Language | Rust / Go / Go daemon + Rust UI | Rust | The UI (Ratatui) and the daemon share the same types and logic. Ships as one binary. Splitting into two languages means managing cross-process compatibility from the first version. |
| Browser | ① a browser you click inside the window ② headless + screen image | ② | The user decided "the agent runs it headless and I only check occasionally". |
| Session list | expanded tree / tab groups / collapse per project / by status | status list | The user's choice. A project picker and an "All projects" view were added on top. |
| Project picker | ① left project rail ② tabs above the list ③ selector at the very top | ① | The user's choice. It holds up with many projects, and the ◆ badge shows at once which projects are waiting on me. It has the same shape as Chrome's collapsed vertical tabs. |
| Master autonomy | a suggest only / b distribute work on its own / c do everything on its own | b, optional | The user's choice. A master may or may not be designated. |
| Work splitting inside a session | ① mine's master does it directly ② Claude Code's subagents, agent teams, workflows | ② | The user's suggestion. Delegation inside one session is already done by Claude Code. But Claude Code can't split work across other accounts, Codex, or other projects (the official docs give no way). Only that part is left to the mine master. |

## 5. Design

### 5.1 Structure

```
Ghostty
 └─ mine (client, Ratatui TUI)
      └─ Unix socket ── mine daemon (local)
                         └─ ssh office-mac mine daemon --stdio ── mine daemon (remote)

mine daemon:
  session PTYs (claude / codex / shell)
  session and project lists, status
  usage collection, handoff records
  git watching
  headless browser
  MCP tools for the master
```

- The single `mine` binary contains all of the following.
  - `mine`: the client. Starts the daemon if there isn't one.
  - `mine daemon`: the daemon.
  - `mine mcp`: the stdio MCP bridge the master session attaches to.
  - Headless subcommands: `mine ls`, `mine new`, and so on.
- **Each session's PTY is held by its own small holder process (`mine hold`).** The daemon connects to the holder processes over Unix sockets and manages the sessions. If the daemon restarts after an upgrade or an error, it finds the live holder processes and reattaches. Meanwhile the session processes (claude, codex, shell) keep running.
  - A holder process only relays PTY input/output and keeps a recent-output buffer; no features go into it. That way it almost never needs upgrading.
  - The protocol between holder processes and the daemon is versioned too. A new daemon must be able to attach to an old holder process.
- A client can attach to and detach from the daemon. The daemon keeps each session's terminal screen state (the `alacritty_terminal` grid) and, when a client reattaches, restores it by sending the current screen. If the daemon itself restarted, the screen is rebuilt from the holder process's output buffer.
- **The local daemon owns the remote connections.** The local daemon keeps one SSH connection per remote daemon and relays through it. The client sees only the one local daemon. So even with no client attached, the master can see remote sessions, create them, and receive notifications. Local and remote daemons speak the same protocol; only the transport changes to SSH stdio. The protocol is versioned. `mine` must also be installed on the remote machine.
- Several clients can attach to one daemon at once, for example using it on the Mac and then SSHing in from outside. Input is accepted from any client. The session size follows the client that typed last.

### 5.2 Concepts

- **Host → project → session.** Project rules are carried over from the prototype unchanged.
  - Walk up from the current location to the nearest Git working tree root; if none, use the registered folder.
  - A separate worktree and a nested repository are each independent projects.
  - A project is identified by its real path.
- A **session** consists of one PTY and one process, in three kinds.
  - `claude`: account claude-a or claude-b, with a model choice.
  - `codex`: with a model choice.
  - `shell`: no account. Added because replacing tmux needs ordinary shell sessions for dev servers, git, and the like.
  - A session gets a name unique within its project (for example `rust-core`).
- **Session status:** `◆ Needs approval`, `◐ Working`, `✓ Done (unread)`, `○ Idle`.
  - A claude session's status is decided by stock CLI hooks that the daemon injects.
    - `UserPromptSubmit` → Working
    - `Stop` → Done
    - `Notification` (`permission_prompt`) or `PermissionRequest` → Needs approval
  - codex sessions use the `UserPromptSubmit` and `Stop` hooks. How to detect Codex's Needs approval (hook, app-server event, output pattern) is for the builder to confirm.
  - When the user opens a session, Done becomes Idle.
  - A shell session is Working while there is output and Idle otherwise.
- **Sessions whose process has ended:**
  - claude and codex sessions are not removed from the list; they stay in the Done group with an `Exited` marker. Choosing one relaunches the same conversation with `--resume` / `codex resume`.
  - A shell session disappears from the list when the shell exits, like tmux.
  - A session the user closed directly (`⌃a x`) is removed from the list, leaving only its record files.
- **Conversation record:** the user input and final responses received through hooks are appended to a per-session JSONL. Handoff and the master use this record.
- **Restore after reboot:** a process can't survive a reboot. Instead, at login launchd (a systemd user service on Linux) starts the daemon, and the daemon revives each session by its rule.
  - There are three rules, changeable per session in the session settings menu or with `mine set <session> restore=…`.

    | Rule | Behavior after reboot | Default for |
    |---|---|---|
    | `resume` | `--resume` / `codex resume` with the same account and model to continue the conversation. Records "interrupted by reboot" at the point it was cut | claude, codex |
    | `rerun` | Opens a new shell in the same folder and reruns the given command (for example `npm run dev`) | — |
    | `none` | Not revived; removed from the list. Record files remain | shell |

  - A turn in progress is cut off by the reboot. `resume` continues the conversation and shows the last user request, but does not automatically resend it.
  - A session revived after a reboot is marked with `↻` in the list, and the marker disappears once it has been opened.

### 5.3 Screen

The layout follows [layout.html](2026-11-20-mine-mockups/layout.html). The left list, however, is replaced by option 1 of [project-rail.html](2026-11-20-mine-mockups/project-rail.html) (project rail + status list).

```
┌──┬────────────────────┬──────────────────────────┬────────────────┐
│∗ │shop-front          │                          │graph browser   │
│m │▾◆ Needs approval  1│   Terminal of the        │ files          │
│p◆│  refund-flow      B│   selected session       │                │
│k │▾◐ Working         1│   (stock CLI as is)      │  git graph     │
│3⇄│  web-ui           C│                          │  (image)       │
│  │▸○ Idle            0│                          │                │
│＋│ Limit A▮▮ 98%      │                          │                │
└──┴────────────────────┴──────────────────────────┴────────────────┘
 Status line: location · account · limit (when the list is collapsed) · key hints
```

- **Project rail:**
  - The top `∗` is "All projects". If a master is designated, it becomes the master's slot.
  - Below it, each project has a colored icon. It uses the first letter of the project name, and the color is assigned automatically.
  - Badges: `◆` means the project has a session that Needs approval, and `⇄` means a remote host.
  - The `＋` at the bottom opens a project.
  - Hovering shows the project name and host.
- **Status list:**
  - Shows the selected project's sessions grouped in the order `◆ Needs approval → ◐ Working → ✓ Done → ○ Idle`.
  - Each row ends with an account chip. A is yellow, B is purple, and C (codex) is teal. Shell sessions have no chip.
  - A session that Needs approval gets one more line showing what it is waiting for.
  - Groups can be collapsed, and the Idle group starts collapsed.
  - There is no blank line between groups; rows are packed tightly, like Linear.
  - `⌃k` searches sessions and projects.
  - Selecting `∗ All projects` gathers every project's sessions by status, with a project-colored square in front of each row.
  - Below the list are per-account 5-hour limit bars.
- **Collapsing the list:** `⌃a b` leaves only the project rail. The limits then move down to the status line.
- **Center:** the selected session's terminal. mine draws it with `alacritty_terminal`.
  - Because it is a full-screen TUI, Ghostty's own scrollback, selection, and ⌘F are unavailable. The baseline is the current Ghostty + tmux level.
  - mine keeps the scrollback per session, and trackpad scrolling moves it. If the session's program takes the mouse directly, mouse input is passed to that program.
  - Dragging selects, and on release the selection is copied to the clipboard with OSC 52. This approach is used because Ghostty intercepts ⌘C.
  - `⌃a [` enters a tmux-style copy mode, in which the scrollback can be searched.
- **Right pane:** shows one of `graph` (default), `browser`, or `files`. `⌃a g` / `⌃a w` / `⌃a f` switch between them, and pressing the same key again hides the pane.
  - `files` shows the selected project's changes not yet committed. It lists git status, and choosing a file opens its diff. The diff view is the same one the git graph pane uses. No file tree or preview.
- **Colors and fonts:** the terminal's (Ghostty's) palette and font are used as they are. Only accent colors are defined.
- **Controls:** the prefix key is `⌃a`. Pressing `⌃a ⌃a` sends `⌃a` itself to the session. The project rail and list also accept mouse clicks. The rest of the key layout is up to the builder, kept familiar to tmux users.

### 5.4 Accounts and handing off at the limit

- The account list and the remote host list live in `~/.mine/config.toml`. Initially there are three: claude-a, claude-b, codex.
- **The two Claude accounts must behave identically except for the login.** The user's `~/.claude` holds plugins, rules, skills, a permission allowlist, and a statusLine. `~/.claude.json` holds user-scope MCP servers. If the Claude inside mine can't read these, it diverges from stock and approval prompts flood in.
  - claude-a uses the currently logged-in `~/.claude` as is. `CLAUDE_CONFIG_DIR` is not changed.
  - claude-b uses `~/.mine/profiles/claude-b`. Only the login info and `.claude.json` are kept separately; `settings.json`, `CLAUDE.md`, rules, skills, plugins, agents, and commands are shared through links pointing to `~/.claude`.
  - The user-scope MCP list (`mcpServers` in `.claude.json`) is copied during `mine login claude-b`. If `~/.claude.json` changes, it is synced again. How to sync is for the builder to decide.
- **The statusLine is wrapped, not overwritten.** The statusLine command mine inserts runs the user's existing statusLine command with the same input and shows its output unchanged. The usage values are recorded separately from the same input.
- **Account identification:** since the same subscription can also be used on remote hosts, an account is identified by the login identity the CLI reports (email or organization). If the identity is the same, the most recent value observed on any host is used as that account's limit.
- **Usage collection:**
  - Claude: when the daemon starts a session it injects a statusLine command and receives the values. This is how the prototype did it with `--settings`.
  - Codex: while at least one session is alive, the app-server is queried periodically.
- **Warnings:**
  - When the 5-hour or 7-day limit reaches 90% or more, a warning appears on that account's chip and on the status line.
  - The warning also shows a recommended account. The recommendation is based on recently observed limits, whether a session already knows that task, and whether it is a review request. It carries over the prototype's `recommendation` rule.
- **Handoff (`⌃a s`):**
  1. Choose an account. The recommended account is the default.
  2. If the original session is working, choose one of two options.
     - "Hand off when the turn ends" (default): schedule it, and hand off when the `Stop` hook arrives.
     - "Stop now and hand off"
  3. Start a new session in the same project. The name is formed like `rust-core~b`.
  4. Put the handoff content in as the first input.
  5. The original session is not closed. It stays in the Done state so it can be viewed again at any time.
- **Handoff content:**
  - The original goal (that session's first user request)
  - Recent conversation record (the last 16,000 characters)
  - A summary of `git status` and the diff
  - The path to the full record file
  - The files are in the same working tree, so nothing is moved separately.
- **Returning to the original account:** resume that account's existing session (`--resume` / `codex resume`). Only the record accumulated in between is added. This is the prototype's `ask` rule.
- **Level of automation:** if there is a master, the master suggests the handoff. Actually handing off is always the user's one keypress. Once the terms of service are checked, this confirmation step may be dropped.

### 5.5 Remote

- A host entry in `config.toml` is a single SSH target name. SSH key authentication is assumed to be set up.
- The local daemon connects to each remote host with `ssh <host> mine daemon --stdio` and relays (5.1). Apart from a `⇄` on the project rail, remote projects behave exactly like local ones. The master's tools and notifications include remote sessions too.
- If the connection drops, remote sessions keep running on the remote daemon. On reconnect the screen is restored. While disconnected, the corresponding icon is dimmed.
- When working remotely, SSH into the Mac from any device and run `mine`. It attaches to the same daemon and shows the same screen (simultaneous connections in 5.1).

### 5.6 Git graph

- Draws all branches of the selected project as a graph. Redraws when `.git` changes.
- In Ghostty it draws kitty graphics images with curves and colors (the same approach as `serie`). In other terminals it draws a text graph.
- Controls:
  - Choosing a commit shows its changed files, and pressing `d` opens the diff.
  - Branch names are attached as color tags.
  - Along with the commit author, the account chip of the session that made the commit is also shown. It is shown only when it can be linked through the session record and the commit time.
- GitButler virtual branches are drawn only as far as they appear as Git refs. No GitButler-specific display.

### 5.7 Browser pane

- When a session asks, the daemon launches a headless Chromium dedicated to that session. The daemon manages the CDP port.
- The agent receives that browser's CDP address and uses it, for example passing it to the chrome-devtools MCP with `--browserUrl`. How to inject it is for the builder to decide.
- If the right pane is `browser` and that session is selected, the daemon takes a screenshot over CDP every 2 seconds and sends it as an image. Console errors are sent along with it.
- Pressing `o` opens the current URL in the user's Chrome. It notes that browser state such as logins is not carried over.
- Clicking inside the screen is a non-goal.

### 5.8 Master session (optional)

- **Boundary of roles:** splitting work inside one session is handled by Claude Code's subagents, agent teams (`CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1`), and workflows. The mine master only works between sessions: it decides which account, which project, or Codex to give work to.
- **Agent team display:** Claude Code's split-screen for teammates works only inside tmux or iTerm2; elsewhere teammates appear as panels (in-process) under the leader prompt. Inside mine this panel mode is used.
  - How Claude Code creates tmux panes is undocumented, and there is no way to slot in a different screen. So a tmux imitation (a fake `tmux` command and `TMUX` variable) is not used as a foundation.
  - In the spike, record the tmux commands Claude Code calls; if an imitation looks reliably possible, revisit it as an experimental feature in phase 6.
  - Later, mine's list could show teammates as sub-rows under a session. It would only read `~/.claude/teams/{team}/config.json` and `~/.claude/tasks/{team}/` as given in the docs.

- **Designation:** pressing `⌃a m` in a claude or codex session makes that session the master. There is only one master at a time, and pressing again releases it.
  - An MCP server or instructions can't be added to a running process later. So only an Idle session can be designated. On designation, mine relaunches that session with `--resume` / `codex resume`, attaching `mine mcp` and the master instructions. Releasing detaches them the same way.
  - `mine mcp` is not attached to non-master sessions, to keep them stock.
  - When creating a new session just for the master, the working folder is `~/.mine/master`. It isn't tied to a project.
  - If the master's account is near its limit, the master role is moved to a session on another account with the same handoff as in 5.4.
- **Screen:** with a master, choosing `∗` shows the master session in the center, and the list shows all sessions by status. Without a master, `∗` shows only the full list.
- **Tools:** the daemon exposes MCP tools through `mine mcp`. This MCP server is connected when the master session is launched.

  | Tool | What it does |
  |---|---|
  | `list_projects`, `list_sessions` | Project and session lists, status, account, limits |
  | `new_session(project, kind, account, model, name, prompt)` | Creates a work session and gives it its first instruction |
  | `send(session, text)` | Sends an instruction to an idle work session |
  | `read(session, since)` | Reads a work session's conversation record |
  | `propose_handoff(session, account)` | Proposes an account handoff. Execution happens after user confirmation |
  | `usage()` | Per-account limits and observation times |

- **Autonomy b:**
  - The master alone creates sessions, splits work, sends instructions, and reads results.
  - It does not press approval prompts on a work session's behalf. Routine commands are handled by each CLI's existing allowlist, and the remaining approvals are left as the human's job.
  - Before directly ordering hard-to-reverse actions such as deploying, pushing, or deleting, it asks the user. This rule goes into the instructions given to the master.
- **Notifications:** when a work session finishes or Needs approval, the daemon feeds an event summary into the master's input if the master is idle. Example: `[mine] shop-front/web-ui Done: …`. This is so the master knows the situation even when the user types nothing.

### 5.9 Storage locations

```
~/.mine/
  config.toml           accounts, remote hosts
  daemon.sock
  projects/             project registrations (real paths)
  sessions/<id>/        metadata, conversation.jsonl, handoff records
  usage/<account>.json  observed limits and observation times
  profiles/claude-a|b/  CLAUDE_CONFIG_DIR
  master/               master working folder
  logs/
```

The location can be changed with `MINE_STATE_DIR`. Tests use this variable too. The Python prototype's `~/.mine/rooms` records are not migrated (5.11).

### 5.10 Phases

Each phase must be actually usable when it ends. Higher-risk items are handled first.

0. **Spike:** run stock Claude Code and Codex inside a Ratatui pane with `alacritty_terminal`. To check:
   - Colors, cursor, pasting, Korean input, mouse, resizing
   - Input latency
   - Whether kitty images are drawn side by side inside Ghostty

   If this hits a failure condition (section 2), reopen the design.
1. **Daemon and sessions:** daemon, per-session holder processes, client, claude·codex·shell sessions, project rail and status list, status detection by hooks, reattaching. There are two acceptance criteria. It must be usable for a whole day in place of Ghostty + tmux, and sessions must stay alive even if `mine` is newly installed mid-work.
2. **Accounts and limits:** limit collection, warnings, `⌃a s` handoff and its content, resuming the original account.
3. **Restore after reboot:** daemon auto-start at login, per-session `resume` / `rerun` / `none` rules.
4. **Git graph and files pane.**
5. **Remote hosts.**
6. **Master:** handles only distribution between sessions. Showing teammate sub-rows is also done here.
7. **Browser pane.**
8. **Distribution:** make `mine` land on PATH with a one-line install (5.11).

### 5.11 My decisions (need user confirmation)

- I added shell sessions (5.2). I figured replacing tmux needs an ordinary shell.
- The Python prototype's shared conversation screen (`/use`, `mine ask`) is not carried on. Its role is split among stock sessions, handoff, and the master.
- The prototype's existing records are not migrated. They are one day's worth of test data.
- The prefix key is `⌃a`, the warning threshold is 90%, and the browser screen refresh is 2 seconds.
- Distribution: the repository is private, so a Homebrew tap needs token setup. Until then, install with `cargo install --path .`.
- I set the baseline for scrolling, selecting, and copying at the current Ghostty + tmux level (section 2, 5.3). Matching stock Ghostty exactly is impossible in a full-screen TUI.
- claude and codex sessions whose process has ended stay as `Exited` so they can be resumed (5.2).
- The two Claude accounts split only the login and share settings, plugins, allowlist, and MCP. claude-a is the current `~/.claude`. The existing statusLine is wrapped and still shown as is (5.4).
- The local daemon owns remote connections. So remote sessions are included in the master and the limit tally (5.1, 5.5).
- Master designation works only on an idle session, which is relaunched with resume (5.8).
- The `files` pane shows only uncommitted changes and diffs (5.3).
- The restore-after-reboot defaults are `resume` for claude·codex and `none` for shell. `resume` does not automatically resend an interrupted request (5.2).

### 5.12 Spike results

In Task 0, the stock run and the spike (`~/dev/mine-spike`) were compared pane cell by cell inside the same tmux. The subjects were Claude Code 2.1.329 and Codex 0.171.0.

**What matched.** For Claude, text, colors, and key behavior were the same in these items:

- Start screen, Korean and emoji input, a three-line paste
- Shift+Enter. This is because the spike passed `CSI > 4;2 m` (modifyOtherKeys) to the outer terminal.
- The `/` menu, queued messages, the approval dialog, window resizing, Ctrl-C

For Codex, all text matched and only the colors differed (B below).

**What differed, and how the core plan handles it.**

| | Difference | Cause | Handling |
|---|---|---|---|
| A | The first input right after launch is lost, and the screen is blank for 2 seconds | The image library queries over stdin and leaves its read thread behind after the timeout | Finish the query on the same thread before reading stdin. Hand leftover bytes to the input parser. Images are handled in phase 4 (Task 9) |
| B | Codex colors differ | Color queries (OSC 10/11) were answered with fixed values | The client asks the outer terminal for its colors once and tells the daemon with `Colors`. This message made the client↔daemon protocol version 2 (Task 9) |
| C | OSC 8 links disappear | The ratatui pane has no links | The snapshot preserves links (Task 4) and the client writes them back in after drawing (Task 9) |
| D | The last output right after exit (the `--resume <id>` hint) disappears | The spike draws only on the alternate screen | The daemon keeps holding the session screen, so it shows up on reattach |
| E | The DA1 reply differs from the outer terminal (`?6c`) | alacritty answers it itself | Left as is for now |

**Not yet confirmed (to check together in Ghostty).**

- kitty image flicker
- How input latency feels
- Codex colors as seen in Ghostty
- Keyboard mode forwarding: the core client passes the attached session's modifyOtherKeys, kitty keyboard, DECCKM, and keypad modes to the outer terminal, and also reads `⌃a` arriving as CSI in those modes as the prefix key. kitty's key-release and repeat reporting (flag 2) is not forwarded, because it complicates prefix-key handling. While mine reads keys itself (copy mode, the input line, no attached session), the outer modes are reset to defaults. Shift+Enter was confirmed to arrive as `CSI 13;2 u` in tmux; confirming in Ghostty remains.
- IME composition position: even when the session hides the cursor, the outer cursor is moved to the session's cursor position (kept hidden). Whether Korean composition shows up at that spot in Ghostty remains to be confirmed.

## 6. Left to the builder

- Crate choices: async runtime, PTY, Git library, image output, MCP implementation
- Message format between daemon and client
- Detailed fields of `config.toml`
- Key layout details
- Project color assignment
- Usage-limit polling interval
- How to detect Codex's Needs approval
- Screen restoration method
- How to inject the browser's CDP address into work sessions

## 7. Verification

- **Unit and integration tests:** verify with fake `claude`/`codex` executables, as the prototype does.
  - Hook → status transitions
  - Handoff content
  - Resuming the original account
  - Project detection
  - Daemon reconnection
- **PTY tests:** compare the screen grid using a program that emits known output.
- **Manual verification (every phase):** use real Claude Code and Codex inside mine and check the failure conditions (section 2) one by one. Verification that costs paid responses is done together with the user.
