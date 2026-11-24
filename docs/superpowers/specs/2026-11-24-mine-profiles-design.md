# mine profiles, hardening, and approvals from the app (D1.5)

- Date: 2026-11-24
- Status: awaiting user review
- Builds on: [multiplexer design](2026-11-20-mine-multiplexer-design.md) (5.4 accounts), [desktop design](2026-11-23-mine-desktop-design.md)
- Comes before: D2 (chat view). D2 is built on top of profiles from the start.
- Input: the Orca / Paseo comparison (2026-11-24), which found what to adopt; the user's decisions are recorded in section 3.

## 1. Intent

mine's core promise is that sessions keep going across accounts. Today "accounts" are three hard-coded ids: `claude-a` (`~/.claude`), `claude-b` (`<state>/profiles/claude-b`), and `codex` (`~/.codex`). The user has more than two subscriptions to juggle, on both Claude and Codex. Stage 2's handoff on usage limits only becomes useful when there are N accounts to move between.

This stage replaces the fixed accounts with **profiles**: any number of named Claude or Codex logins, each with its own config home. In the same pass, it adds the small fixes the comparison found and one large win: **approving or denying a permission request from the app or TUI** without opening the session's terminal.

## 2. Goals, non-goals, anti-goals

**Goals**

1. Profiles: any number of profiles per kind (claude, codex). Each profile has an id, a kind, a config home, an optional label and color, and a sharing setting.
2. The existing setup keeps working with no action: old `meta.json` files and `[accounts.*]` labels still resolve, and `claude-b`'s directory is reused as is.
3. Common Claude settings (skills, commands, agents, CLAUDE.md, settings.json) are shared across Claude profiles by default, through links. Any item can be split off per profile later by replacing its link with a real file. The same mechanism covers Codex.
4. Profile management from the CLI: list, add, log in, remove (config only, never deletes data).
5. The launcher (TUI and app) lists every profile; one key starts a session on it.
6. Hardening:
   - A session stuck in Working after an interrupt goes back to Idle.
   - Env vars that would bypass a profile's subscription login are stripped.
   - `mine _event` never breaks or blocks the agent by accident.
   - The hello reply carries a feature list and the daemon's identity.
7. Attention: clients report presence (visible, last input, focused session), including the TUI through terminal focus events. The daemon routes notifications and "seen" from presence.
8. Approvals from the app and TUI: a Claude permission request in a session nobody is looking at is held, shown with Allow and Deny in the task list and the TUI, and answered from there. If nobody answers, Claude falls back to its own prompt.

**Non-goals (later stages)**

- Usage percentages, limit warnings, and handoff (stage 2). This stage only makes the profile model ready for them.
- The chat view (D2), worktree-per-task, remote hosts, OSC 7501, and mobile.
- Approvals for Codex sessions (D2 handles them through the app-server). Codex `PermissionRequest` only drives the Needs approval state here.
- Editing profiles from the GUI. The CLI and `config.toml` are enough for one user.

**Anti-goals (failure even if it works)**

- mine reads, copies, or logs login credentials (`.credentials.json`, the Keychain, Codex `auth.json`). Checking whether a file exists is allowed; opening it is not.
  - MCP server definitions are the user's own config, not login credentials, so the MCP sync in section 6 may copy them.
  - Even so, their values are treated as opaque: never logged, displayed, or sent to clients.
- An existing session can no longer resume, or a profile silently runs on a different account than its label says.
- A held permission request blocks the user who is looking at the session's terminal.
- An "always allow" decision is made on the user's behalf. One answer covers exactly one request.

## 3. Decisions the user made (2026-11-24)

- Profiles for both Claude and Codex.
- Profiles first, then D2.
- Shared common settings, structured so items can be split per profile later.
- The comparison's small fixes and approvals from the app are part of this work.

## 4. Approaches considered

**Who owns the profile list.**

| | Approach | Trade-off |
|---|---|---|
| A (chosen) | The daemon loads profiles from `config.toml` and sends them to clients | One source of truth. Works unchanged when the daemon is remote (stage 5). Clients never read config or `.claude.json`. Costs a protocol message. |
| B | Each client reads `config.toml` itself, as labels work today | No protocol change, but a remote daemon's profiles would be invisible, and two clients can disagree. |

**How approvals reach the app before D2.**

| | Approach | Trade-off |
|---|---|---|
| A (chosen) | Hold Claude's `PermissionRequest` hook; the daemon answers from the app or TUI | Works for every terminal session today. Needs care so it never blocks a user who is watching the terminal (section 9). |
| B | Wait for D2's chat view (`can_use_tool`) | Only chat sessions, and only after D2. |

**Attention signal.**

| | Approach | Trade-off |
|---|---|---|
| A (chosen) | Presence heartbeat from every client, including the TUI through terminal focus reporting (`CSI ?1004`) | Fixes the idle-SSH-TUI case and "seen" (final review M4). |
| B | Keep `Focus { focused }` from the app only | The TUI never counts, so notifications and "seen" stay wrong when the TUI is used. |

## 5. Profile model

A profile is `{ id, kind, home, label?, color?, share }`.

- **id:** the stable key stored in `meta.json`. Same rules as session names (letters, digits, `_~-`, up to 64 characters). Never renamed; use `label` for display.
- **kind:** `claude` or `codex`.
- **home:** the CLI's config home. It's an explicit, stable property of the profile, never derived from `default`.
  - `home = "default"` means the CLI's own default (`~/.claude` with `~/.claude.json`, `~/.codex`). mine sets no env var for it. At most one profile per kind may use it.
  - `home = "<path>"` points at an existing directory (for example, an older second login).
  - No `home` means `<state>/profiles/<id>/`.
  - For anything but `"default"`, mine sets `CLAUDE_CONFIG_DIR` or `CODEX_HOME`.
- **default:** chooses only the launcher's Enter row and the share source of its kind (section 6). Moving `default = true` to another profile never moves anyone's home.
- **label:** optional alias. Without it:
  - Claude: the local part of the login email (current rule, including the collision rule).
  - Codex: `Codex` for the default profile, the id for others. Codex login identity is not read, because it lives in a credentials file.
- **color:** optional palette name or hex. Without it, colors are assigned in config order from the app palette: gold `#e5c07b`, violet `#c678dd`, teal `#56b6c2`, green `#98c379`, blue `#61afef`, red `#e06c75`. The chip letter is the first character of the label, uppercased.
- **share:** see section 6.

### 5.1 Config

```toml
# <state>/config.toml
[profiles.claude-a]          # the default Claude profile, at the CLI's own default home
kind = "claude"
home = "default"
default = true
label = "personal"

[profiles.claude-b]
kind = "claude"
label = "work"

[profiles.team]
kind = "claude"
share = ["skills", "commands"]   # only these are shared

[profiles.codex]
kind = "codex"
home = "default"

[profiles.codex-team]
kind = "codex"
share = false
```

- Order in the file is the order in the launcher and the color order.
- Without `default = true`, the profile with `home = "default"` is the default of its kind. If there isn't one, the first profile of that kind is the default.
- Unknown keys are ignored with a warning, like today's broken-config handling (line number and reason only).

### 5.2 Migration (no user action)

- No `[profiles]` table means the built-in list:
  - `claude-a` (`home = "default"`)
  - `claude-b`, if `<state>/profiles/claude-b` exists (no `home`, so the same directory as today)
  - `codex` (`home = "default"`)
- The built-in `claude-b` shares by default, like any profile (the user chose sharing). Every item already in its directory is a real file, so it stays untouched and shows as `own`. Only items it doesn't have yet get linked.
- Old `[accounts.<id>] label = …` entries still apply as labels.
- `meta.json`'s `account` field is read as the profile id (serde alias). An id that no longer exists makes that session fail to start with a clear message. It never falls back to another account.
- `mine new --account` stays as an alias of `--profile`.

## 6. Sharing

Claude's shareable items, relative to the profile home: `skills/`, `commands/`, `agents/`, `CLAUDE.md`, `settings.json`, `plugins/`. Never shared: login and identity (`.claude.json`, `.credentials.json`), conversations (`projects/`, `todos/`, `history.jsonl`), and caches.

Codex's shareable items: `AGENTS.md`, `prompts/`, `config.toml`. Never shared: `auth.json`, `sessions/`, `history.jsonl`. The builder confirms this list against the installed `codex`.

- `share = true` (default): link every shareable item from the default profile's home.
- `share = false`: no links.
- `share = [names]`: only those.
- **Links:**
  - Links are symlinks to the default profile's item.
  - `mine profile add` creates them, and they're checked before each session launch.
  - A missing link is created.
  - A real file or directory at that path is left alone (that is how an item is split off), and `mine profile ls` shows it as `own`.
  - A link pointing elsewhere is left alone and reported.
- **Tracking:**
  - mine records the links it made, in `<state>/profiles/<id>.links`.
  - If a recorded link later turns into a real file (a CLI saved it by writing a temp file and renaming it), `mine profile ls` shows it as `split (changed by <kind>)`. Nothing is re-linked automatically.
  - `mine profile relink <id> [item]` puts the link back. It keeps the replaced file as `<item>.mine-bak`.
  - The builder checks whether Claude and Codex write these files in place (which keeps the link) or by rename.
- **MCP servers (Claude).** User-scope MCP servers live in `~/.claude.json`, which can't be linked: it also holds the login. Multiplexer design 5.4 requires them in the second account.
  - Before each launch of a non-default Claude profile, mine copies the `mcpServers` object from the default `.claude.json` into the profile's `.claude.json`. Servers with the same name are overwritten; the profile's other servers are kept.
  - It writes only that key, atomically, leaving every other field as it was.
  - `share` controls this like any other item, under the name `mcpServers`.
- **Auth settings in a shared settings.json.** A shared `settings.json` with an `apiKeyHelper` or an `env` entry from the bypass list (section 10.2) would put every profile on that key.
  - mine reads only the key names, never the values.
  - It shows a warning in `mine profile ls` and at launch, and doesn't edit the file.
- The default profile itself never gets links.

## 7. CLI

```sh
mine profile ls                                   # id, kind, label, home, logged in?, shared items
mine profile add team --kind claude [--label L] [--home PATH] [--share LIST|--no-share]
mine profile login team                           # runs the CLI's login with the profile env, in this terminal
mine profile relink team [settings.json]          # restores shared links that were replaced (section 6)
mine profile rm team                              # removes the config entry; prints the home path; deletes nothing
mine new review --profile team                    # --account still works
```

- **"Logged in?"** is decided by existence only:
  - Claude: `oauthAccount` in `.claude.json`. This JSON field is already read for labels.
  - Codex: `auth.json` exists. It is checked, never opened.
- **`profile login`:**
  - Claude: runs `claude /login`, or the builder's verified equivalent.
  - Codex: runs `codex login`.
  - Both run with the profile env set, in the foreground.
- **`profile add`/`rm`:**
  - Edit `config.toml` in place, keeping comments where the TOML library allows. Otherwise they append a section and say so.
  - Tell a running daemon to reload profiles (`ToDaemon::ReloadProfiles`).

## 8. Launch, launcher, UI

- **Launch:** `session::launch` resolves the profile.
  - It sets `CLAUDE_CONFIG_DIR`/`CODEX_HOME` for non-default profiles and repairs share links.
  - It strips the bypass variables for every Claude and Codex session (section 10.2).
- **Launcher, TUI and app:**
  - One row per profile in config order, then shell.
  - Keys `1`–`9` start the profile on that row. `s` starts a shell. Enter starts the first row (the default Claude profile). `n` is the typed form, now `<profile|shell> <name> [model]`.
  - Profiles that aren't logged in show "not logged in · mine profile login <id>" and don't start.
- **Chips and labels:**
  - Rows, tabs, and the status line use the profile's chip letter and color.
  - `mine ls` prints the profile id in the account column; its name stays `account` for script compatibility.
- **Profile list to clients:**
  - The daemon sends `ToClient::Profiles(Vec<ProfileInfo>)` after hello and after a reload.
  - `ProfileInfo = { id, kind, label, color, chip, default, logged_in }`.
  - Clients stop reading `config.toml` and `.claude.json`.

## 9. Approvals from the app and TUI

Claude's `PermissionRequest` hook is added to each Claude session's `--settings`. The command is `mine _event <dir> --ask` with a hook timeout of 130 s.

1. `_event --ask` sends `Ask { session, tool, summary }` to the daemon over the socket and waits for the reply.
2. The daemon decides whether to **hold**.
   - **Tools that always pass.** Some tools need more than yes/no and always get `pass`: `AskUserQuestion` (needs real answers) and `ExitPlanMode` (would approve a plan nobody read), plus any tool the builder finds that collects input. The list is a constant in mine-core.
   - **The viewing rule.** It holds only if no client is both visible and viewing that session's terminal. This rule is separate from the 180 s presence rule in section 11: someone reading the terminal without typing still counts as watching.
   - If someone is viewing, it replies `pass` at once, and `_event` prints nothing. Claude then shows its own prompt.
3. While holding:
   - The session is Needs approval, with waiting text such as `Allow Bash(npm run deploy)?`.
   - **App:** **Allow** and **Deny** appear on that session's task-list row and its tab. Each button answers exactly that session's request.
   - **TUI:**
     - `⌃a a` opens an approvals panel listing pending requests, oldest first, with session and summary.
     - ↑/↓ selects a request, `y` allows the selected one, `n` denies it, and Esc closes the panel.
     - The panel never answers a request the user hasn't selected. The `⌃a n` and `⌃a p` session keys are unchanged.
   - The OS notification follows the usual routing.
4. **Answer:** the daemon replies `allow` or `deny`. `_event` prints `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}` (or `deny` with a short message) and exits 0.
   - It never sends `updatedPermissions` or "always allow".
5. **Release** with `pass` (Claude's own prompt appears), when any of these happen:
   - a client starts viewing that session's terminal;
   - `Input` arrives for that session (the daemon releases first, then forwards the input);
   - 120 s pass;
   - the daemon restarts;
   - the socket drops.

   On every failure path, `_event` exits 0 with no output.

Codex: the `PermissionRequest` hook, if the installed Codex supports it, only sets Needs approval. There's no hold.

## 10. Hardening

1. **Interrupt fallback.** While a claude or codex session is Working, an `Input` that is exactly `\x03` or `\x1b` (not part of a longer sequence or a paste) sets it to Idle. Any later hook event wins.
2. **Env hygiene.**
   - For Claude sessions, strip `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, `AWS_BEARER_TOKEN_BEDROCK`, `CLAUDE_CODE_USE_BEDROCK`, and `CLAUDE_CODE_USE_VERTEX`. Today the code passes `CLAUDE_CODE_USE_BEDROCK` through; that changes. A profile is a subscription login, and these would silently route around it.
   - For Codex sessions, strip `OPENAI_API_KEY` and `CODEX_API_KEY`. The builder verifies which variables Codex honors over its login.
   - Stripping happens in the holder's env cleaning, so it also covers a daemon started with a login-shell env.
   - `ponytail:` a per-profile `env` table to opt back in, added when someone needs an API-key profile.
3. **Fail-open hooks.**
   - `_event` always exits 0 and writes to stdout only the PermissionRequest decision.
   - A missing session dir, a dead daemon, or bad JSON gets one log line and is otherwise a no-op.
   - The ignore-the-version rule for Poke from the D1 final review stays.
4. **Hello features, identity, and compatibility.**
   - The hello reply carries `features: [...]` and `daemon: { pid, version, started_ms }`.
   - New behavior is gated on a feature name, not on `VERSION` alone.
   - `VERSION` becomes 4 (Profiles, Presence, Ask), and features from now on: `profiles`, `presence`, `ask`.
   - From now on, the daemon accepts any client with `VERSION >= MIN_CLIENT_VERSION` (3 for this stage), because the app bundle and the CLI are updated separately.
   - An older client gets only what its version knows. A v3 client gets no `Profiles` message and keeps reading labels itself.
   - The daemon ignores a message type it doesn't know (one log line) instead of disconnecting.
   - Clients do the same with unknown daemon messages.

## 11. Presence and attention

- Every client sends `Presence { visible, last_input_ms, viewing: Option<session id> }`.
  - It sends on change, and at least every 30 s while visible.
  - `viewing` is the attached session whose terminal is on screen.
- **App:** visible means window active; input means any key or click.
- **TUI:** enables focus reporting (`CSI ?1004h`) and treats `CSI I` / `CSI O` as visible or not, plus its own input.
  - Terminals without focus reporting count as visible while attached. This is the current behavior.
- **Daemon:**
  - A client is **present** if it's visible and its last input was within 180 s.
  - **OS notification** (approvals only, as today; Done notifications are not added in this stage): only if no present client is viewing that session.
  - **Seen** (Done → Idle): only when a present client views it. This fixes final-review M4.
  - **Hold for an approval:** uses the viewing rule in section 9 (visible and viewing, no input requirement), not presence.
- `Focus { focused }` from D1 maps to `Presence { visible: focused, … }` and is kept for one version.

## 12. Protocol summary (VERSION 4)

- **To daemon:** `Presence {…}`, `Answer { session, request, allow: bool }` (`request` is the pending request's id, so a stale answer can't hit a newer request), `ReloadProfiles`. The hook-side `Ask {…}` runs on its own short-lived connection, like Poke. Like Poke, it is accepted regardless of the client version.
- **Compatibility:** `MIN_CLIENT_VERSION = 3`. Unknown messages are ignored in both directions (section 10.4).
- **To client:** hello reply `{ features, daemon }`, `Profiles(Vec<ProfileInfo>)`, and a per-session `pending: Option<PendingAsk { summary, since_ms }>` in the session list.
- **Hold version:** HOLD_VERSION stays 1. Env stripping happens in the daemon before spawn.

## 13. Testing and review focus

- **Migration.**
  - Old `meta.json` with `account: "claude-b"` resumes under the same `CLAUDE_CONFIG_DIR`.
  - `[accounts.*]` labels still apply.
  - No `[profiles]` table gives the built-in list.
- **Sharing.**
  - Links are created.
  - A real file at a link path is left alone and reported as `own`.
  - The default profile gets no links.
  - Credentials and conversations are never linked.
- **No credential reads.** A test asserts mine never opens `.credentials.json` or `auth.json`, for example by placing a FIFO there that would block on open.
- **Ask.**
  - Held while no one is viewing.
  - Passed at once when someone is viewing.
  - Released when a viewer arrives, on the 120 s timeout, and on daemon restart.
  - Exactly one decision per request; no `updatedPermissions`.
  - Tests use a fake `claude` that runs the hook command and records its stdout.
- **Interrupt fallback.** A bare `\x03` or `\x1b` → Idle; an arrow key's `\x1b[A` doesn't change the state.
- **Presence.**
  - An idle TUI (no input for 180 s) doesn't suppress notifications.
  - A TUI focus-out counts as not visible.
- **Env.** Bypass vars are absent in a session's env dump; user variables survive.

## 14. Calls made after the builder check (the controller's, open to the user's correction)

1. A held approval uses "visible and viewing", not the 180 s presence rule. Typing into that session releases it.
2. MCP servers: the `mcpServers` copy from multiplexer design 5.4 is kept, as a sharable item. Values are opaque and never logged.
3. `home` is explicit and stable. `default` only picks the Enter row and the share source.
4. The daemon accepts clients down to version 3 and ignores unknown messages.
5. The migrated `claude-b` shares like any profile, and its existing files stay its own.
6. Links that a CLI replaces are reported as `split` and restored only by `mine profile relink`.
7. `AskUserQuestion` and `ExitPlanMode` are never held.
8. The TUI answers through an approvals panel (`⌃a a`), not single keys.
9. No Done notifications in this stage. The Bedrock/Vertex switches are stripped too. Auth settings in a shared `settings.json` trigger a warning, read by key name only.

## 15. Left to the builder

- Exact Claude and Codex login commands, the Codex hook event names and versions, and Codex's shareable file list, verified against the installed CLIs.
- How `config.toml` edits keep comments (`toml_edit` is allowed as a new dependency if needed; give the reason in the commit).
- Allow/Deny button placement details within the existing list row and tab, using the existing colors.
