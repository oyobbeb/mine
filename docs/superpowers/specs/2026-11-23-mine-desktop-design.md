# mine desktop app design: a Paseo-style GPUI window

- Written: 2026-11-23
- Status: approved by the user (2026-11-23)
- Base document: [2026-11-20-mine-multiplexer-design.md](2026-11-20-mine-multiplexer-design.md). Anything not written in this document follows the base document.
- Reference: [Paseo](https://paseo.sh/). After seeing its screen layout, the user said "this kind of UI is fine".

## 1. Intent

After trying the phase 1 core (daemon, holder process, TUI) in Ghostty, the user's reaction was this: the first screen was empty, they didn't know which key "⌃a" was, and the usability was poor from the start. After then seeing Paseo's screen, they chose and decided the following.

- Go with a Paseo-like **desktop app**: one window holding the task list, session tabs, conversation, and changes.
- Build it with **GPUI**. It is Rust and is the UI framework Zed uses.
- The center screen is decided the way Paseo does it. Claude and Codex sessions get a **chat view**, and shells and dev servers get a **terminal**.
- The build order was approved like this: (1) the window skeleton and terminal pane, (2) the chat view, (3) the Changes panel on the right, (4) after that, phases 2–8 of the base document.
- The existing TUI stays as a secondary screen for attaching over SSH. In the TUI, a chat session is viewed and answered as a text conversation. Sending a one-line message, approving with y/n, and stopping all work (user decision, 5.10).

The long-term direction is the same as in the base document. mine is a tool that answers "which agent or account should I give this task to right now?". What changes is that the main screen moves from a TUI inside a terminal to a desktop window.

### Decisions reversed from the base document

- The base document's section 2 non-goal "a GUI window of its own" is reversed. The desktop window becomes the main screen.
- The base document's section 2 non-goal "Reimplementing stock CLI features. Session screens use stock Claude Code and Codex as they are" is changed.
  - Claude and Codex sessions are by default a chat view that mine draws.
  - **Open in terminal** remains for when the stock screen is needed. Terminal-only things such as agent teams and `/login` are such cases (5.3).
- Of the base document's failure conditions, "the Claude Code or Codex screen breaks inside mine" now applies only to terminal-mode sessions. For chat sessions, the failure conditions in section 7 below apply instead.

## 2. Goals · non-goals · failure conditions

**Goals (D1–D3, covered by this document)**

1. **D1 Window skeleton.** Running `mine-app` opens a window. On the left is a task list grouped by status, at the top are session tabs, and in the center is the session screen. The existing claude·codex·shell sessions can be opened and used as terminal panes. This phase alone must be enough to use for a whole day in place of the TUI and tmux.
2. **D2 Chat view.** The user can do the following.
   - Create Claude and Codex sessions as chat views and continue them.
   - Send messages from the input box, and send more while the agent is working so they queue up.
   - Change the model and effort, and stop what the agent is doing.
   - Answer approval requests with buttons, and view subagent activity grouped together.
3. **D3 Changes panel.** See the selected project's uncommitted changes as a file tree with +/− line counts, and click a file to see its diff. Also see the recent commit list.
4. Sessions keep running when the app is closed. Reopening it brings back the same conversation and screen. Base document goal 1 is kept on the desktop too.

**Non-goals**

- Paseo's other features: schedules, voice, plugins, mobile app, Hub, PR view, code editor pane.
- A web client, multiple windows, Windows support.
- Reproducing every Claude Code feature in the chat view. What doesn't work is solved with Open in terminal.
- Base document phases 2–8: account handoff, restore after reboot, git graph, remote, master, browser, distribution. These are planned separately, phase by phase, after this document. The git graph starts from D3's commit list.

## 3. Constraints

- **Language:** Rust only, no Python. Fake agents for tests are built with shell scripts or Rust.
- **GPUI:** It is still pre-1.0, so the API can break when versions go up. Pin the version. Pin one of `gpui`, `gpui-component`, or `gpui-kit` (which bundles the two) to a git rev or an exact version (the builder decides, section 6). GPUI-related dependencies go only into the app package; the daemon and CLI binaries don't pull in GPUI.
- **Platform:** macOS (Apple Silicon) is the baseline. For Linux, only keep the build from breaking.
- **The agent connection protocols change from version to version.**
  - Claude Code's stream-json control messages are defined only in the official Agent SDK package.
  - The Codex app-server schema is extracted per version with `codex app-server generate-json-schema`.
  - So write the confirmed versions in the document, and branch handling on feature detection (Claude `system/init.capabilities`).
  - Even if an unknown message arrives, don't die; show its raw text as an "unknown item".
- **Authentication:** Same as the base document. claude-a uses `~/.claude`, claude-b uses `CLAUDE_CONFIG_DIR=<state>/profiles/claude-b`, and Codex uses its existing login. mine doesn't handle credentials. `--bare` is not used, because `--bare` doesn't read the subscription login.

## 4. Approaches considered

| Approach | Pros | Cons | Decision |
|---|---|---|---|
| **A. A GPUI app on top of the mine daemon** | Reuses the phase 1 core as is (holder processes, recovery after restart, status). Shell and dev server tabs are already solved by PTY sessions. Rust from start to finish. | The most to build. GPUI is pre-1.0. | **Chosen**. Approved by the user |
| B. Fork Paseo | The screens already exist, so it is the fastest. Apache-2.0. | TypeScript and Electron. The opposite of the user's chosen GPUI and Rust. | Rejected |
| C. New, chat-only | Simple structure. | Throws away the PTY core. Shell tabs and recovery after restart would have to be rebuilt. | Rejected |

## 5. Design

### 5.1 Structure

```
mine-app (GPUI window) ──┐
mine (TUI, for SSH) ─────┼── Unix socket, JSON lines ── mine daemon ── mine hold (one per session)
mine ls/new/kill ────────┘                                             ├─ PTY mode: shell, dev server, claude·codex opened as terminal
                                                                       └─ pipe mode: claude -p / codex app-server of chat sessions
```

- **The repository becomes a Cargo workspace monorepo (user's suggestion).** Packages go under `crates/` and are named `mine-<role>`. This is the same approach as Helix (`helix-core`, `helix-term`) and Zed (`crates/*`).
  - `crates/mine-core` (lib): what several binaries share. `paths`, `project`, `session`, `proto`, `screen`, `hooks`, `conn`, plus the conversation item model and adapters in 5.4.
  - `crates/mine-cli` (bin, executable name `mine`): headless commands, `mine daemon`, `mine hold`, the TUI. The user types `mine` as now. The daemon and holder process stay as subcommands of this binary.
  - `crates/mine-app` (bin, executable name `mine-app`, bundle name `mine.app`): the GPUI window.
  - In code, underscore names (`mine_core`) are used. The GPUI dependency goes only into `mine-app`; `mine-core` and `mine-cli` don't pull in GPUI.
  - Splitting the daemon out into its own `mine-daemon` crate is done when it becomes necessary. For now, `mine-cli` is the only binary that uses the daemon.
- **The app is only a client of the daemon (my decision).**
  - If there is no daemon, start it as now (`connect_or_spawn`).
  - Closing the app leaves the daemon and sessions untouched.
  - The daemon is not embedded in the app.
- **Chat sessions are also held by a holder process (my decision).**
  - Add a **pipe mode** to the holder process. It connects the child's stdin and stdout through pipes and sends stderr to the session log.
  - The `HoldIn`/`HoldOut` messages and the 2 MiB ring stay as they are. `Input` is the bytes going to the child's stdin, and `Output` is the stdout bytes.
  - The mode is chosen with a command-line flag of `mine hold`. The message format is unchanged, so `HOLD_VERSION` stays at 1.
  - This way chat sessions survive even when the daemon comes back up. It is how the base document's failure condition is kept as is.
- **The daemon keeps a conversation record (transcript) for each chat session.**
  - It takes the JSON lines from the child's stdout and converts them into the common items of 5.4 below.
  - It holds them in memory and appends them to `sessions/<id>/transcript.jsonl`. The raw JSON lines are also kept in `sessions/<id>/raw.jsonl` for debugging and reinterpretation.
  - When the daemon comes back up and receives the holder process's ring again, it **must not insert already-recorded lines twice or miss any**. The builder decides how. For example, the ring could carry a cumulative byte offset, and the last processed offset could be saved in the metadata.

### 5.2 Protocol (client↔daemon `VERSION = 3`)

Existing messages stay as they are. New messages:

- **`FromDaemon`**
  - `Transcript { id, items }`: sent in place of `Snapshot` when attaching to a chat session. It is the full conversation record. An in-progress item carries the text accumulated so far.
  - `Item { id, item }`: sent when a new item appears or an existing one changes. An item has a unique id, and the same id means an update.
  - `ItemDelta { id, item_id, text }`: appends text to an in-progress `Assistant` or `Thinking` item. It is for character-by-character display. Deltas aren't saved to the file; when an item finishes, the completed `Item` is saved.
- **`ToDaemon`**
  - `Send { id, content, priority }`. `content` is text and attachments (images, file paths). `priority` corresponds to Claude's `now`·`next`·`later`.
  - `Answer { id, request_id, decision }`: answers an approval request, a plan approval (ExitPlanMode), or a question (AskUserQuestion).
  - `Interrupt { id }`
  - `SetModel { id, model }`
  - `SetEffort { id, effort }`
  - `SetPermissionMode { id, mode }`: changes the permission mode (5.3).
  - `Focus { focused }`: tells the daemon whether the app window is in front. Used to avoid stacking notifications (5.9).
- **Add a `view` field to `NewSession`:** `Chat` or `Terminal`. The default for claude·codex is `Chat`, and shell is always `Terminal`. It is also saved in the metadata.

### 5.3 Sessions and status

- **Kinds**
  - For claude·codex sessions, whether to open as chat or terminal is chosen at creation. It isn't changed while running, because they are different processes.
  - A session opened as terminal is the stock CLI, exactly like phase 1 today.
  - A chat session's conversation continues through `--resume`, so reopening the same conversation as a terminal is possible (D2's "Open in terminal" command).
- **Moving between chat and terminal is one-way, chat → terminal (my decision).**
  - "Open in terminal" continues the same conversation with `--resume` and opens a new terminal session. So that two processes don't hold the same conversation at once, the original chat session is closed (after asking before it runs). The conversation isn't forked into two (`--fork-session`).
  - Opening a terminal session as chat is done only as "a new chat that continues the same conversation". `--resume` in `-p` mode doesn't re-emit past turns, so the chat view is left with only a `Note` saying "Earlier conversation not shown". Importing and showing the past conversation is outside this document's scope.
- **Permission mode (my decision).**
  - Claude chat sessions start with `--permission-mode`. The default is that account's `defaultMode` setting. The user uses auto now. The input box switches among default·acceptEdits·plan·auto·bypassPermissions, and a change while running is sent as a control message.
  - plan mode's `ExitPlanMode` is shown separately as a "plan approval" card (5.7).
  - Codex chat sessions pass the approval policy and sandbox to `thread/start`. The defaults follow the Codex settings and can be changed in the input box.
- **What doesn't work in a chat session** is solved with Open in terminal. The launcher says so.
  - Claude Code agent teams. In `-p` mode it doesn't create teammates (Claude Code docs).
  - Terminal-only commands such as `/login` and `/theme`.
- **A chat session's status.** Decided directly from the stream, not hooks.
  - ◆ Needs approval: there is an approval request or question that hasn't been answered.
  - ◐ Working: a turn is running.
  - ✓ Done: the turn has ended and the user hasn't seen it yet.
  - ○ Idle: anything else.
  - Exited: the process has ended. Choosing it relaunches with `--resume`. Same as the base document.
- **A terminal session's status** is the same as phase 1. Decided by hooks, and for shell by its output.

### 5.4 Conversation items (common model)

Claude's and Codex's output is converted into the items below. The screen knows only these items.

| Item | Content | From Claude | From Codex |
|---|---|---|---|
| `User` | The sent message; a queued (not yet sent) indicator | The `user` message sent to stdin (+ confirmation via `--replay-user-messages`) | `turn/start` input |
| `Assistant` | Markdown text | The text block of `assistant` | message item |
| `Thinking` | Collapsed thinking | thinking block | reasoning item |
| `Tool` | Name, one-line summary, input, status (running/succeeded/failed), output | `tool_use` + the following `tool_result` | commandExecution·fileChange and other items |
| `Approval` | Request id, tool, summary, choices (Allow/Deny/Always allow) | `control_request` `can_use_tool` (`--permission-prompt-tool=stdio`) | `item/commandExecution/requestApproval`, `item/fileChange/requestApproval` |
| `Question` | AskUserQuestion's question and choices | `can_use_tool`, `tool_name: "AskUserQuestion"` | N/A |
| `Subagent` | One subagent's group (title, status, items inside) | Messages with the same `agent_id`, `task_*` system messages | N/A |
| `TurnEnd` | Result, cost, usage, error | `result` | `turn/completed` |
| `Note` | Compaction, API retries, raw text of unknown messages | `system/*`, anything else | anything else |

**How Claude is run (confirmed version: Claude Code 2.1.3xx)**

- **Launch:** `claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --permission-prompt-tool=stdio --forward-subagent-text --replay-user-messages`, with `--permission-mode`, `--resume <id>` (when continuing), `--model`, and `--effort` added.
- **Character-by-character display:** `stream_event` (`content_block_delta`) from `--include-partial-messages` is converted to `ItemDelta`. Subagent text doesn't arrive as deltas, so it appears all at once when the block ends.
- **Input:** one JSON object per line. The format is `{"type":"user","message":{"role":"user","content":...},"parent_tool_use_id":null,"uuid":...,"priority":...}`. Whether to use `client_composed: true` when sending the user's typed text as is is up to the builder. Commands starting with `/` are left to be expanded.
- **Control:** done with `control_request` / `control_response`.
  - `interrupt`, `set_model`, `apply_flag_settings` (effort), and the `can_use_tool` response belong here.
  - `control_cancel_request` withdraws a pending approval.
  - Feature detection uses `system/init.capabilities`.
  - The format is based on `sdk.d.ts` in the official Agent SDK package (`@anthropic-ai/claude-agent-sdk`).
- **Environment:** if the user has set `CLAUDE_CODE_EFFORT_LEVEL`, changing effort while running has no effect (Claude Code docs). In that case, lock the effort choice in the input box and show why.
- **Session id:** taken from `system/init` and `result` and saved as `resume_id` in the metadata.

**How Codex is run**

- Uses `codex app-server` (stdio, JSON-RPC, no `"jsonrpc"` header).
- The flow is `initialize` → `thread/start` (or continue) → `turn/start` → item notifications → `turn/completed`.
- Approvals are answered as replies to requests the server sends.
- Extract the schema of the version in use with `codex app-server generate-json-schema`, put it in the repository, and match the types to that version. Extract it again when the version goes up.

### 5.5 Window layout (D1)

```
┌────────────────────────────┬──────────────────────────────────────────────┬──────────────────────┐
│ ＋ New task            ⌘N  │ mine · t1   [A] claude-a            ⏹ Stop ⋯ │ Changes  Files       │
│ ┌ All projects ▾ ────────┐ │ [◐ t1  A] [✓ review  C] [dev  sh]  ＋        │ main                 │
│ │ Pick project           │ ├──────────────────────────────────────────────┤ Uncommitted +120 −31 │
│ └────────────────────────┘ │                                              │ ▾ src/client         │
│ ◆ Needs approval        1  │   (chat view or terminal pane)               │   ui.rs  +56 −18     │
│  refund-flow          [B]  │                                              │   mod.rs +12 −3      │
│  photo · approve deploy? 1m│                                              │ ▸ Commits            │
│ ◐ Working               2  │                                              │                      │
│  t1                   [A]  │ ┌──────────────────────────────────────────┐ │                      │
│  mine · 3m                 │ │ Message…             Opus ▾  Medium ▾  ⏎ │ │                      │
│ ✓ Done                  1  │ └──────────────────────────────────────────┘ │                      │
│ ○ Idle ▸                3  │                                              │                      │
│ ＋ Add project             │                                              │                      │
└────────────────────────────┴──────────────────────────────────────────────┴──────────────────────┘
```

- **Left list**
  - At the top is "New task", which opens the launcher when clicked.
  - Below it is the project picker. Choose "All projects" or a single project; a project with a session that Needs approval gets a ◆ mark. It replaces the TUI's project rail.
  - Below that are the status groups. The order and symbols are the same as the base document (◆ → ◐ → ✓ → ○).
  - Each entry is two lines. The first line is the name and account chip (A/B/C). The second line shows the project, the waiting text or last activity, and how many minutes ago.
  - The Idle group starts collapsed, and clicking its header expands it.
- **Center top:** the selected project's sessions are laid out as tabs. A tab carries a status symbol and a chip. Clicking "＋" brings up the launcher.
- **Launcher**
  - It carries over the format the user chose in the TUI. Pick with one key or a click: ↵ claude-a, b claude-b, x codex, s shell.
  - It adds an "Open in terminal" choice and "Custom name and model".
  - The name is assigned automatically as `t<N>`.
- **Right Changes panel:** built in D3. In D1 only a collapsed slot is kept.
- **Colors:** use the mockup colors the user approved. Background `#1e2128`, list `#23252b`, selected row `#2f323a`, border `#2c2f36`, chips A `#e5c07b` / B `#c678dd` / C `#56b6c2`. Fonts are the system font for the UI and a monospace font for terminals and code (the builder decides).
- **Shortcuts:** the app needs no prefix key.

  | Shortcut | Action |
  |---|---|
  | ⌘N | New task |
  | ⌘W | Close session (with confirmation) |
  | ⌘1–9 | Select tab |
  | ⌘[ / ⌘] | Previous / next session |
  | ⌘⇧[ / ⌘⇧] | Previous / next project |
  | ⌘B | Collapse the left list |
  | ⌘⇧C | Changes panel |
  | Esc | Stop in chat |

  When a terminal pane has focus, every key without ⌘ goes to the session.

### 5.6 Terminal pane (D1)

- **Screen:** the client-side `Screen` (alacritty) is drawn as GPUI text runs. Zed's terminal uses the same combination. The following are drawn with the same results as the phase 1 TUI.
  - Cell colors and attributes
  - Wide characters
  - The cursor
  - OSC 8 links. Clicking opens them.
- **Key input:** the app converts keys to bytes itself. It encodes according to the modes the session turned on (DECCKM, keypad, kitty keyboard, modifyOtherKeys), reusing the mode tracking built in QA1. So Shift+Enter goes in the form the session wants.
- **Korean input:** GPUI's input method handling (InputHandler) draws the composing characters directly at the cursor position. It doesn't rely on the outer terminal.
- **Mouse:** the wheel scrolls the scrollback. Dragging selects, and ⌘C copies to the system clipboard. Paste is ⌘V, and if the session turned on bracketed paste, it is wrapped when sent. If the session turned on mouse reporting, it is passed through as SGR. Same as the rules in phase 1 Task 10.
- **Size:** when the pane size changes, send `Resize`.
- **Copy mode:** not provided. GUI scrolling and dragging take its place. Search (⌘F) is a scrollback text search in D1.

### 5.7 Chat view (D2)

- **List:** draws the items from top to bottom. To keep long conversations from stuttering, only the visible part is drawn.
  - `User` is a right-aligned bubble. A message that hasn't been sent yet and is queued is shown dimmed.
  - `Assistant` is Markdown. It draws paragraphs, lists, code blocks, inline code, links, and tables.
  - `Tool` collapses to one line. Example: `Shell  cargo test   ✓ 3.2s`. Expanding shows the input and output. Edit and Write are shown as diffs.
  - `Subagent` is drawn as a card, and expanding shows the items inside. Same as Paseo's "3 subagents" card.
  - `Approval` is a highlighted card. The buttons are Allow, Deny, and, if there is a suggestion, "Always allow". On the keyboard, y and n.
  - A plan approval in plan mode is a card with the plan body (Markdown) and "Approve · Keep planning" buttons.
  - While an answer is being produced, text appears character by character (`ItemDelta`).
  - `Question` is a card of choice buttons.
  - `TurnEnd` shows the result and cost on a thin line.
- **Input box**
  - Multi-line. Enter sends, Shift+Enter inserts a newline.
  - Sending while the agent is working queues the message. The default `priority` follows Claude's default.
  - It has a model picker, an effort picker, a permission mode picker, and an account chip (display only, can't be changed).
  - Pasting an image (⌘V) or dropping a file sends it as an attachment. To Claude it goes as an image block and a file path; to Codex in the input format that version accepts. The user often pastes screenshots to send.
  - The stop button is the same as Esc.
  - Typing `/` autocompletes from the `system/init.slash_commands` list.
- **Notifications:** follow 5.9.
- **Open in terminal:** continues the same conversation with `--resume` and opens a new terminal session. It asks whether to close the original chat session.

### 5.8 Changes panel (D3)

- **Target:** the selected project's root. A chat session uses that session's working folder.
- **Data:** reads `git status --porcelain=v2`, `git diff --numstat`, and `git log -n 30 --format=...`. It runs no write commands at all.
- **Refresh:** re-reads in these three cases.
  - When the panel is opened
  - When a turn of that project's session ends
  - When the user presses refresh
- **Screen:** shows a folder tree, per-file +/− and a status mark (modified/added/deleted). Clicking a file opens a diff view as a tab in the center. The commit list is below.
- The git graph of base document phase 4 will later grow out of this commit list's place.

### 5.9 Launch and notifications (D1)

- **How to launch (my decision):** both are provided.
  - In D1, `mine-app` is built as an `.app` bundle (Dock, Spotlight). A shell script in the repository builds the bundle from the `cargo build --release -p mine-app` output and installs it to `~/Applications`. Signing and Homebrew distribution are done in base document phase 8.
  - `mine app` in a terminal opens the bundle (`open`). If there is no bundle, it runs the `mine-app` binary directly. The `mine` binary doesn't depend on GPUI.
  - Closing the window leaves the app in the Dock, per macOS convention. Quitting the app with ⌘Q leaves the daemon and sessions running.
- **Environment:** an app launched from the Dock has a short PATH and can't find `claude`·`codex`. So when the app starts the daemon, it reads the login shell environment once (`$SHELL -l -c env`) and passes it along. A daemon that is already running uses its own environment as is.
- **The daemon owns notifications (my decision).**
  - When a new approval request appears, the daemon posts a macOS notification. This is because the notification must arrive even when the app is closed (section 7).
  - When the app window is in front (`Focus { focused: true }`), the daemon doesn't post notifications.
  - How to post the notification is up to the builder (`display notification` via `osascript`, and so on).

### 5.10 Chat sessions in the TUI (D2, user decision)

- When the TUI attaches to a chat session, it receives `Transcript` and `Item`·`ItemDelta` and shows them as a text conversation. The display format is as follows.
  - `User` is shown with a `> ` prefix.
  - `Assistant` is shown as the raw text, without Markdown styling.
  - `Tool` is a one-line summary.
  - `Subagent` is a one-line summary.
  - `Approval`·`Question` are shown as a highlighted line with numbered choices.
- Input works as follows.
  - Send with Enter on the one-line input at the bottom.
  - When an approval request is up, answer with `y`/`n`. Answer a question by number.
  - Esc stops.
- Markdown styling, expanding, attachments, and changing the model·effort·permission mode are not done. Those are done in the app.

## 6. Left to the builder

- How to pull in GPUI (a single `gpui-kit`, or `gpui` + `gpui-component` separately) and which version to pin.
- The Markdown renderer: build it directly, or on top of `pulldown-cmark`.
- How to prevent duplicate transcript entries (5.1). But "never insert twice or miss any" must be kept.
- Pixel sizes, spacing, font sizes.
- Whether to use `client_composed` in Claude input.
- Where in the repository to put the Codex schema file.

## 7. Failure conditions

Even if it works technically, it counts as a failure if any of the following applies.

- A session dies when the app is closed or the app crashes. A chat session dies when the daemon comes back up.
- Chat history is missing or shown twice when the app or daemon is restarted.
- An approval request arrives and the user can miss it without knowing: none of the ◆ in the list, the tab marker, or the notification is present.
- There is something a chat session can't do, and there is no way to open it as a terminal, or that way can't be found.
- The terminal pane's input latency, Korean composition, Shift+Enter, or copy and paste is noticeably worse than using Ghostty directly.
- Scrolling or input stutters in a long conversation (thousands of items).

## 8. Verification

- **Adapter unit tests.** Put recorded real output into the repository as fixtures: stream-json lines for Claude, JSON-RPC messages for Codex. Check that converting these fixtures into the common items gives the expected results. Also check that unknown messages go to `Note`.
- **Daemon integration tests.** Put fake `claude`·`codex` scripts on PATH: scripts that emit the fixtures and record stdin. Use them to check the following.
  - That sending a message, queueing, stopping, and answering an approval go out as the right control messages
  - That the record continues without duplicates after a daemon restart
  - That the status changes among ◆ ◐ ✓ ○
- **UI logic tests.** Use GPUI's test context to check list groups, tabs, launcher keys, and key encoding.
- **Visual check.** The controller launches the app and captures the window (`screencapture -l <window id>`) to check layout and colors.
- **User QA.** Use it for a day with real Claude and Codex. D1's acceptance criterion is a day of use in place of the TUI and tmux.

## 9. My decisions (need user confirmation)

- `mine-app` is built only as a daemon client, and the daemon is not embedded in the app (5.1).
- Chat sessions are also held by a holder process so they survive daemon restarts (5.1).
- For each claude·codex session, chat or terminal is chosen, and the default is chat (5.3). If agent teams are needed, open as terminal.
- App shortcuts are ⌘-based, and there is no prefix key (5.5).
- The terminal pane has no copy mode. GUI scrolling, dragging, and ⌘F take its place (5.6).
- The Changes panel uses only git read commands (5.8).
- The daemon posts approval request notifications as macOS notifications. They arrive even when the app is closed. The default is on (5.9).
- Only chat → terminal is provided. Opening as terminal closes the original chat session. Importing past conversation is not done (5.3).
- Permission modes are included from the start. The default is the account setting's `defaultMode` (5.3).
- Answers are shown character by character (`--include-partial-messages`, `ItemDelta`) (5.2, 5.4).
- Pasting images and dropping files into the input box are supported (5.7).
- In D1, both the `.app` bundle and the `mine app` command are built. The login shell environment is passed along (5.9).

Decided by the user: a Paseo-style desktop app, GPUI, the monorepo and the `mine-core`·`mine-cli`·`mine-app` names (5.1), the chat·terminal center screen, the D1→D2→D3 order, the TUI as a secondary screen for SSH, and viewing and answering chat sessions as a text conversation in the TUI (5.10).
