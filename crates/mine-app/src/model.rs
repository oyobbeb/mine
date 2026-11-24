//! App state. Without GPUI, it takes daemon-link events (`LinkEvent`) and user actions, changes state, and returns the
//! messages to send to the daemon (`Effect`).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mine_core::hooks::now_ms;
use mine_core::launch::{self, Choice};
use mine_core::link::LinkEvent;
use mine_core::profiles::ProfileInfo;
use mine_core::proto::{FromDaemon, PresenceReport, ToDaemon};
use mine_core::screen::Screen;
use mine_core::session::{NewSession, SessionInfo, SessionState};
use mine_core::view::{self, Group, STATES};

use crate::terminal::mouse::Mouse;
use crate::theme;

/// What the model asks the outside to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Send(ToDaemon),
}

/// What ⌘W should do. Decided by [`Model::close`].
#[derive(Clone, Debug, PartialEq)]
pub enum Close {
    /// Closed the launcher that was opened over the attached session.
    Launcher,
    /// Asks whether to close the attached session. The value is the session name to show in the confirmation dialog. The answer comes back through [`Model::confirm_close`].
    Confirm(String),
    /// There is no session to close. Closes the window (the app stays in the Dock).
    Window,
    /// A confirmation dialog is already showing.
    Nothing,
}

/// A session that was sent `New` and is waiting. When (`project`, `name`) shows up in the list, it attaches.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub project: PathBuf,
    pub name: String,
    /// When the `New` was sent.
    pub since: Instant,
}

/// Waits this long for the answer to `New`. Past that, it drops the wait and shows [`START_FAILED`].
pub const PENDING_TIMEOUT: Duration = Duration::from_secs(5);
/// The created session never showed up in the list (the shell ended right away, or no answer came).
const START_FAILED: &str = "Couldn't start the session";
/// Shown when you pick again while waiting for the answer to an earlier `New`.
const CREATING: &str = "Creating…";

/// The text shown when you pick in the launcher with no connection. The link thread drops messages sent before connecting.
const NOT_CONNECTED: &str =
    "Can't create a session while disconnected from the daemon. Pick again once it reconnects.";
/// The text shown when the close confirmation is answered with no connection. The session is kept.
const CLOSE_OFFLINE: &str =
    "Can't close the session while disconnected from the daemon. Try again once it reconnects.";

/// A logged-in profile as the daemon sends it. The chip is the label's first letter, uppercased.
#[cfg(test)]
pub(crate) fn profile(
    id: &str,
    kind: mine_core::profiles::ProfileKind,
    label: &str,
    color: &str,
) -> ProfileInfo {
    ProfileInfo {
        id: id.into(),
        kind,
        label: label.into(),
        color: color.into(),
        chip: label.chars().next().unwrap().to_uppercase().to_string(),
        default: false,
        logged_in: true,
    }
}

/// The built-in three, logged in: claude-a `personal` (the default Claude profile), claude-b `work`, and codex `Codex`.
#[cfg(test)]
pub(crate) fn builtin() -> Vec<ProfileInfo> {
    use mine_core::profiles::ProfileKind::{Claude, Codex};
    let mut list = vec![
        profile("claude-a", Claude, "personal", "#e5c07b"),
        profile("claude-b", Claude, "work", "#c678dd"),
        profile("codex", Codex, "Codex", "#56b6c2"),
    ];
    list[0].default = true;
    list[2].default = true;
    list
}

pub struct Model {
    pub sessions: Vec<SessionInfo>,
    /// The selected project. `None` means all projects.
    pub filter: Option<PathBuf>,
    /// The attached session id.
    pub attached: Option<String>,
    /// Groups the user collapsed or expanded by clicking the header, opposite to the default (only Idle collapsed). Values are [`STATES`] indexes (◆ 0, ◐ 1, ✓ 2, ○ 3).
    pub toggled: HashSet<usize>,
    pub sidebar_open: bool,
    pub connected: bool,
    /// The reason for the last disconnect. Cleared on reconnect. While set, the disconnected strip shows above the list.
    pub offline: Option<String>,
    /// The project the app was launched from. It is in the project picker even when it has no sessions.
    pub current_project: PathBuf,
    /// `current_project` is the home directory substituted for the folder when the app was launched from the Dock (the folder
    /// is `/`). The home directory is not registered as a project, so it is not put in the project picker either.
    pub home_fallback: bool,
    /// Registered projects (`mine_core::project::registered`).
    pub registered: Vec<PathBuf>,
    /// The launcher was opened over the attached session (⌘N, the tab's ＋, New task). With no attached session it is open regardless of this value.
    pub launcher: bool,
    /// The launcher's `n` input box is open.
    pub typing: bool,
    /// The session that was sent `New` and is waiting for the answer. It attaches when it appears in the list.
    pub pending: Option<Pending>,
    /// The session id the close confirmation dialog was shown for.
    pub closing: Option<String>,
    /// An error from the daemon or an input error. Shown at the bottom of the center and cleared on the next action.
    pub message: Option<String>,
    /// The terminal pane size (columns, rows). Known once the pane is first drawn ([`Model::set_grid`]). `Attach` and `Resize` carry it.
    pub grid: Option<(u16, u16)>,
    /// The attached session's screen. On `Snapshot` it is rebuilt at the pane size and fed; `Output` is fed on top. When the
    /// attached session changes, it is dropped and a new `Snapshot` is awaited.
    pub screen: Option<Screen>,
    /// The terminal pane's selection and mouse press. It is screen cells, so it is dropped when the cells under it
    /// change: the view scrolls, the pane is resized, a snapshot replaces the screen, or another session is attached.
    pub mouse: Mouse,
    /// Whether `Attach` for the attached session was sent on this connection. `false` while deferred because the pane size is unknown or there is no connection.
    attach_sent: bool,
    /// The first list on this connection arrived. `Attach` waits for it: the attached session may have ended while
    /// disconnected, and an `Attach` for it would only bring back a "No such session" error.
    listed: bool,
    /// The session to attach to was chosen from the first list received.
    started: bool,
    /// The profiles the daemon sent last (`FromDaemon::Profiles`), in launcher order. Empty until the first list arrives; kept while disconnected.
    pub profiles: Vec<ProfileInfo>,
    /// The app window is the active window ([`Model::set_focused`]).
    focused: bool,
    /// The daemon on this connection announced `presence`: report with `Presence` instead of `Focus`.
    presence: bool,
    /// The daemon on this connection announced `ask`: it holds permission requests, so rows show `pending` and take `Answer`.
    ask: bool,
    report: PresenceReport,
}

impl Model {
    pub fn new(current_project: PathBuf) -> Self {
        Model {
            sessions: Vec::new(),
            filter: None,
            attached: None,
            toggled: HashSet::new(),
            sidebar_open: true,
            connected: false,
            offline: None,
            current_project,
            home_fallback: false,
            registered: Vec::new(),
            launcher: false,
            typing: false,
            pending: None,
            closing: None,
            message: None,
            grid: None,
            screen: None,
            mouse: Mouse::default(),
            attach_sent: false,
            listed: false,
            started: false,
            profiles: Vec::new(),
            focused: false,
            presence: false,
            ask: false,
            report: PresenceReport::default(),
        }
    }

    /// The app window became active or inactive, which counts as input. The daemon routes notifications and "seen" by it, so it
    /// is told on every change: with `Presence` if the daemon announced `presence`, else with `Focus` (also on every connect while
    /// focused, since a new connection starts unfocused in the daemon).
    pub fn set_focused(&mut self, focused: bool) -> Vec<Effect> {
        if self.focused == focused {
            return Vec::new();
        }
        self.focused = focused;
        if focused {
            self.report.input(now_ms());
        }
        if !self.connected {
            return Vec::new();
        }
        if self.presence {
            return self.presence_tick(Instant::now());
        }
        vec![Effect::Send(ToDaemon::Focus { focused })]
    }

    /// A key or click anywhere in the window.
    pub fn user_input(&mut self, now: Instant) -> Vec<Effect> {
        self.report.input(now_ms());
        self.presence_tick(now)
    }

    /// The `Presence` due at `now` ([`PresenceReport::due`]). Visible is the window being active, and the attached session counts
    /// as viewed while its terminal pane shows (no launcher over it). Called after anything that can change those and on the
    /// heartbeat ([`Model::presence_deadline`]).
    pub fn presence_tick(&mut self, now: Instant) -> Vec<Effect> {
        if !(self.connected && self.presence) {
            return Vec::new();
        }
        let viewing = self.attached.as_deref().filter(|_| !self.launcher);
        let due = self.report.due(now, self.focused, viewing);
        due.map(Effect::Send).into_iter().collect()
    }

    /// When the next heartbeat is due: only while connected to a daemon that knows `Presence` and the window is active.
    pub fn presence_deadline(&self) -> Option<Instant> {
        self.report
            .deadline()
            .filter(|_| self.connected && self.presence)
    }

    /// Takes the daemon link's events.
    ///
    /// On every connect (including reconnects) it subscribes to the list (R18), reports the terminal colors, and once
    /// the first list shows the attached session still exists, reattaches to it. The link already exchanged `Hello`.
    /// If the connection drops, the session list and screen stay as they are; when it reconnects, they are replaced
    /// once a new list and `Snapshot` arrive.
    pub fn on_link(&mut self, ev: LinkEvent) -> Vec<Effect> {
        match ev {
            LinkEvent::Connected(features) => {
                self.connected = true;
                self.offline = None;
                self.listed = false;
                self.presence = features.iter().any(|f| f == "presence");
                self.ask = features.iter().any(|f| f == "ask");
                self.report.reset();
                let mut effects = vec![
                    Effect::Send(ToDaemon::List),
                    Effect::Send(ToDaemon::Colors {
                        fg: theme::hex(theme::TERM_FG),
                        bg: theme::hex(theme::TERM_BG),
                    }),
                ];
                // A new connection starts unfocused in the daemon.
                if self.presence {
                    effects.extend(self.presence_tick(Instant::now()));
                } else if self.focused {
                    effects.push(Effect::Send(ToDaemon::Focus { focused: true }));
                }
                effects
            }
            LinkEvent::Disconnected(reason) => {
                self.connected = false;
                self.offline = Some(reason);
                // Nothing was sent on the next connection yet. A heartbeat left armed would come due during the outage.
                self.report.reset();
                // The new daemon doesn't know which session this connection was attached to. Send `Attach` anew when reattaching.
                self.attach_sent = false;
                // There is no way to know whether a sent `New` reached the daemon before the drop. Let the user pick again without
                // waiting for an answer. If it did arrive, that session shows up in the list that comes after reconnecting.
                self.pending = None;
                Vec::new()
            }
            LinkEvent::Msg(FromDaemon::Sessions { sessions }) => {
                // The daemon announces only when the list changed (publish). A list that didn't change is the answer to a request this app made.
                let unchanged = sessions == self.sessions;
                self.sessions = sessions;
                self.listed = true;
                self.on_sessions(unchanged)
            }
            LinkEvent::Msg(FromDaemon::Error { message }) => {
                self.pending = None;
                self.message = Some(message);
                Vec::new()
            }
            LinkEvent::Msg(FromDaemon::Snapshot { id, data }) if self.is_attached(&id) => {
                // Output that had piled up is also in the snapshot. Drop the previous screen.
                if let Some((cols, rows)) = self.grid {
                    let mut screen = Screen::new(cols, rows);
                    screen.feed(&data);
                    screen.take_replies();
                    self.screen = Some(screen);
                    self.mouse.clear();
                }
                Vec::new()
            }
            LinkEvent::Msg(FromDaemon::Output { id, data }) if self.is_attached(&id) => {
                if let Some(screen) = &mut self.screen {
                    screen.feed(&data);
                    // The session's queries (DA1, cursor position, colors) are answered by the daemon's screen. Drop these so the same query isn't answered twice.
                    screen.take_replies();
                }
                Vec::new()
            }
            // Sent after hello and after every reload: the launcher rows and chips follow the last one.
            LinkEvent::Msg(FromDaemon::Profiles { profiles }) => {
                self.profiles = profiles;
                Vec::new()
            }
            // The connection checked the version. A screen for another session was sent before the attached session changed, so drop it.
            // The connection already skips unknown messages, and `AskReply` only goes to a hook's own connection.
            LinkEvent::Msg(
                FromDaemon::Hello { .. }
                | FromDaemon::Snapshot { .. }
                | FromDaemon::Output { .. }
                | FromDaemon::AskReply { .. }
                | FromDaemon::Unknown,
            ) => Vec::new(),
        }
    }

    fn is_attached(&self, id: &str) -> bool {
        self.attached.as_deref() == Some(id)
    }

    /// Takes a new list. If a session that was being waited for appears, attaches to it. If it is the first list received or
    /// the attached session vanished, attaches to the first visible session, and if there is none, the launcher shows.
    /// Otherwise it sends the `Attach` that waited for the first list after a reconnect.
    ///
    /// A list that didn't change, arriving with no session being waited for, is the answer to `New`. If the shell ends right
    /// away, the daemon leaves no session and answers success. Drop that wait and report it. A list that did change may have
    /// come from other sessions while creating, so keep waiting ([`PENDING_TIMEOUT`] backs this up).
    fn on_sessions(&mut self, unchanged: bool) -> Vec<Effect> {
        if let Some(p) = &self.pending {
            let found = self
                .sessions
                .iter()
                .find(|s| s.meta.project == p.project && s.meta.name == p.name);
            if let Some(s) = found {
                let id = s.meta.id.clone();
                self.pending = None;
                return self.attach(&id);
            }
            if unchanged {
                self.fail_pending();
            }
        }
        let gone = self
            .attached
            .as_ref()
            .is_some_and(|id| !self.sessions.iter().any(|s| s.meta.id == *id));
        if gone {
            self.detach();
        }
        if !self.started || gone {
            self.started = true;
            return self.attach_first();
        }
        self.send_attach()
    }

    /// Attaches to the first visible session. If there are no visible rows (the Idle group starts collapsed), it also looks into collapsed groups.
    fn attach_first(&mut self) -> Vec<Effect> {
        let groups = self.visible_groups();
        let first = view::order(&groups)
            .first()
            .copied()
            .or_else(|| groups.iter().find_map(|g| g.rows.first().copied()));
        match first {
            Some(i) => {
                let id = self.sessions[i].meta.id.clone();
                self.attach(&id)
            }
            None => Vec::new(),
        }
    }

    /// The session found by id.
    pub fn session(&self, id: &str) -> Option<&SessionInfo> {
        self.sessions.iter().find(|s| s.meta.id == id)
    }

    /// The launcher covers the center. It is always open when no session is attached.
    pub fn launcher_open(&self) -> bool {
        self.attached.is_none() || self.launcher
    }

    /// The project the launcher creates a new session in: the selected project, or if all projects, the attached session's
    /// project, or if there is none, the project the app was launched from.
    pub fn project(&self) -> &Path {
        let attached = self.attached.as_deref().and_then(|id| self.session(id));
        self.filter
            .as_deref()
            .or(attached.map(|s| s.meta.project.as_path()))
            .unwrap_or(&self.current_project)
    }

    /// Attaches to a session. Closes the launcher and the input box. If it is already the attached session, it doesn't
    /// tell the daemon again, unless its `Attach` is still deferred.
    ///
    /// `Attach` changes the attached session's PTY to the size it carries. So it is sent only when the pane size is known.
    /// If the pane hasn't been drawn yet, it is sent when first drawn ([`Model::set_grid`]); if there is no connection,
    /// when reattaching, after the first list.
    pub fn attach(&mut self, id: &str) -> Vec<Effect> {
        self.launcher = false;
        self.typing = false;
        self.message = None;
        if self.is_attached(id) {
            return self.send_attach();
        }
        self.detach();
        self.attached = Some(id.to_string());
        self.send_attach()
    }

    /// Lets go of the attached session. The daemon has no message for detaching, so that session's output keeps arriving but is dropped.
    fn detach(&mut self) {
        self.attached = None;
        self.screen = None;
        // A press the old session holds is dropped too: its release must not go to the next session.
        self.mouse = Mouse::default();
        self.attach_sent = false;
    }

    /// Sends the not-yet-sent `Attach` of the attached session at the current pane size. Deferred if the pane size is
    /// unknown, there is no connection, or the connection's first list hasn't arrived yet.
    fn send_attach(&mut self) -> Vec<Effect> {
        match (&self.attached, self.grid) {
            (Some(id), Some((cols, rows)))
                if self.connected && self.listed && !self.attach_sent =>
            {
                self.attach_sent = true;
                vec![Effect::Send(ToDaemon::Attach {
                    id: id.clone(),
                    cols,
                    rows,
                })]
            }
            _ => Vec::new(),
        }
    }

    /// Attaches to the neighboring session by `delta` in visible order (the list order without collapsed groups). Wraps from the end to the start.
    pub fn step_session(&mut self, delta: i32) -> Vec<Effect> {
        let order = view::order(&self.visible_groups());
        let Some(n) = i32::try_from(order.len()).ok().filter(|&n| n > 0) else {
            return Vec::new();
        };
        let here = self
            .attached
            .as_ref()
            .and_then(|id| order.iter().position(|&i| self.sessions[i].meta.id == *id));
        let next = match here {
            Some(at) => (at as i32 + delta).rem_euclid(n),
            None if delta > 0 => 0,
            None => n - 1,
        };
        let id = self.sessions[order[next as usize]].meta.id.clone();
        self.attach(&id)
    }

    /// Picks the neighboring project by `delta` in project-picker order (including All projects). Wraps from the end to the start.
    pub fn step_project(&mut self, delta: i32) {
        let items = self.projects();
        let at = items
            .iter()
            .position(|p| p.root == self.filter)
            .unwrap_or(0);
        let next = (at as i32 + delta).rem_euclid(items.len() as i32) as usize;
        self.set_filter(items[next].root.clone());
    }

    /// Picks a project. If the attached session isn't in that project, it is let go (the launcher shows). The daemon has no
    /// message for detaching, so there is nothing to send.
    pub fn set_filter(&mut self, filter: Option<PathBuf>) {
        self.filter = filter;
        let shown = self
            .attached
            .as_deref()
            .and_then(|id| self.session(id))
            .is_some_and(|s| self.filter.as_ref().is_none_or(|p| s.meta.project == *p));
        if !shown {
            self.detach();
        }
    }

    /// Tabs: the session indexes of the selected project (every project when all), in name order.
    pub fn tabs(&self) -> Vec<usize> {
        let mut tabs: Vec<usize> = (0..self.sessions.len())
            .filter(|&i| {
                let project = &self.sessions[i].meta.project;
                self.filter.as_ref().is_none_or(|p| project == p)
            })
            .collect();
        tabs.sort_by(|&a, &b| {
            let (a_meta, b_meta) = (&self.sessions[a].meta, &self.sessions[b].meta);
            (&a_meta.name, &a_meta.project, a).cmp(&(&b_meta.name, &b_meta.project, b))
        });
        tabs
    }

    /// ⌘1–⌘9: attaches to the `i`th tab (0-based).
    pub fn select_tab(&mut self, i: usize) -> Vec<Effect> {
        match self.tabs().get(i) {
            Some(&s) => {
                let id = self.sessions[s].meta.id.clone();
                self.attach(&id)
            }
            None => Vec::new(),
        }
    }

    /// Picks a launcher row. The session is created right away under a new name (`t<N>`), and attaching happens after it shows up in the list.
    /// `Typed` opens the input box. A profile that isn't logged in doesn't start, and the message says how to log in.
    pub fn launch(&mut self, choice: Choice) -> Vec<Effect> {
        self.message = None;
        if choice == Choice::Typed {
            self.typing = true;
            return Vec::new();
        }
        if let Some(refusal) = launch::refusal(choice.account(), &self.profiles) {
            self.message = Some(refusal);
            return Vec::new();
        }
        // While waiting for the answer to an earlier `New`, don't accept another. This keeps two sends under the same name from tangling.
        // If it has waited too long (even if the timer hasn't run yet), drop it and accept the new pick.
        if let Some(p) = &self.pending {
            if Instant::now().duration_since(p.since) < PENDING_TIMEOUT {
                self.message = Some(CREATING.into());
                return Vec::new();
            }
            self.pending = None;
        }
        if !self.connected {
            self.message = Some(NOT_CONNECTED.into());
            return Vec::new();
        }
        let project = self.project().to_path_buf();
        let name = launch::next_name(&self.sessions, &project);
        match launch::new_session(&choice, &name, &project, &self.profiles) {
            Some(spec) => self.create(spec),
            None => Vec::new(),
        }
    }

    /// Creates a session from the input box's text (`view::parse_new` format). If it is wrong, or names a profile that isn't logged
    /// in, it shows the reason and leaves the box open.
    pub fn submit_typed(&mut self, text: &str) -> Vec<Effect> {
        if !self.connected {
            self.message = Some(NOT_CONNECTED.into());
            return Vec::new();
        }
        let spec = view::parse_new(text, self.project(), &self.profiles)
            .map_err(|e| format!("{e:#}"))
            .and_then(
                |spec| match launch::refusal(spec.account.as_deref(), &self.profiles) {
                    Some(refusal) => Err(refusal),
                    None => Ok(spec),
                },
            );
        match spec {
            Ok(spec) => {
                self.typing = false;
                self.message = None;
                self.create(spec)
            }
            Err(message) => {
                self.message = Some(message);
                Vec::new()
            }
        }
    }

    fn create(&mut self, spec: NewSession) -> Vec<Effect> {
        self.pending = Some(Pending {
            project: spec.project.clone(),
            name: spec.name.clone(),
            since: Instant::now(),
        });
        vec![Effect::Send(ToDaemon::New { spec })]
    }

    /// ⌘N, the tab's ＋, New task: opens the launcher.
    pub fn new_task(&mut self) {
        self.launcher = true;
        self.typing = false;
        self.message = None;
    }

    /// Esc. If the input box is open, returns to the launcher; if it is a launcher opened over an attached session, closes it.
    /// With no attached session there is nowhere to go back to, so the launcher stays.
    pub fn dismiss(&mut self) {
        self.message = None;
        if self.typing {
            self.typing = false;
        } else if self.attached.is_some() {
            self.launcher = false;
        }
    }

    /// ⌘W. If it is a launcher opened over an attached session, closes only that. If a session is attached, starts the close confirmation.
    pub fn close(&mut self) -> Close {
        if self.closing.is_some() {
            return Close::Nothing;
        }
        let Some(id) = self.attached.clone() else {
            return Close::Window;
        };
        if self.launcher {
            self.launcher = false;
            self.typing = false;
            return Close::Launcher;
        }
        let name = self
            .session(&id)
            .map_or(id.clone(), |s| s.meta.name.clone());
        self.closing = Some(id);
        Close::Confirm(name)
    }

    /// Drops a `New` that has waited past [`PENDING_TIMEOUT`] and shows [`START_FAILED`]. Returns `true` if it dropped one.
    pub fn expire_pending(&mut self, now: Instant) -> bool {
        let expired = self
            .pending
            .as_ref()
            .is_some_and(|p| now.duration_since(p.since) >= PENDING_TIMEOUT);
        if expired {
            self.fail_pending();
        }
        expired
    }

    fn fail_pending(&mut self) {
        self.pending = None;
        self.message = Some(START_FAILED.into());
    }

    /// The answer of the close confirmation dialog. If `yes`, sends `Kill` for the session the confirmation was asked about. When it leaves the list, it attaches to another session (`on_sessions`).
    /// With no connection the link would drop the `Kill`, so it keeps the session and says so.
    pub fn confirm_close(&mut self, yes: bool) -> Vec<Effect> {
        match self.closing.take() {
            Some(_) if yes && !self.connected => {
                self.message = Some(CLOSE_OFFLINE.into());
                Vec::new()
            }
            Some(id) if yes => vec![Effect::Send(ToDaemon::Kill { id })],
            _ => Vec::new(),
        }
    }

    /// `root` was registered with "＋ Add project". `registered` is the list re-read after registering. If it is there,
    /// selects that project; if not (the home directory and `/` are not registered), shows the reason.
    pub fn project_added(&mut self, root: PathBuf, registered: Vec<PathBuf>) {
        let listed = registered.contains(&root);
        self.registered = registered;
        if listed {
            self.message = None;
            self.set_filter(Some(root));
        } else {
            self.message = Some(format!(
                "The home directory and / can't be added as projects: {}",
                root.display()
            ));
        }
    }

    /// The terminal pane was drawn and is (`cols`, `rows`) (window size, list collapse). Only when it changed, fits the screen and sends `Resize`.
    /// If a deferred `Attach` exists, it attaches at that size. With no connection, it only remembers and sends it when reattaching.
    pub fn set_grid(&mut self, cols: u16, rows: u16) -> Vec<Effect> {
        if self.grid == Some((cols, rows)) {
            return Vec::new();
        }
        self.grid = Some((cols, rows));
        self.mouse.clear();
        if let Some(screen) = &mut self.screen {
            screen.resize(cols, rows);
        }
        match &self.attached {
            // `attach_sent` is on only while there is a connection (turned off when it drops).
            Some(id) if self.attach_sent => vec![Effect::Send(ToDaemon::Resize {
                id: id.clone(),
                cols,
                rows,
            })],
            _ => self.send_attach(),
        }
    }

    /// Scrolls only the screen copy by `lines` lines (positive is toward the past). Nothing is sent to the session. Returns
    /// `true` if the visible position changed (a redraw is needed); that also drops the selection.
    pub fn scroll(&mut self, lines: i32) -> bool {
        let Some(screen) = &mut self.screen else {
            return false;
        };
        let before = screen.term().grid().display_offset();
        screen.scroll(lines);
        let moved = screen.term().grid().display_offset() != before;
        if moved {
            self.mouse.clear();
        }
        moved
    }

    /// The held request the Allow and Deny buttons of row or tab `s` answer. `None` hides them: the row holds nothing, the daemon
    /// didn't announce `ask`, or there is no connection to answer over.
    pub fn answerable(&self, s: &SessionInfo) -> Option<u64> {
        s.pending
            .as_ref()
            .filter(|_| self.ask && self.connected)
            .map(|p| p.request)
    }

    /// Allow (`allow`) or Deny for `request` of session `id`: the one the clicked row or tab showed, not whichever is pending now. A
    /// request that has been decided or let go since is ignored by the daemon.
    pub fn answer(&self, id: &str, request: u64, allow: bool) -> Vec<Effect> {
        if !(self.ask && self.connected) {
            return Vec::new();
        }
        vec![Effect::Send(ToDaemon::Answer {
            id: id.to_string(),
            request,
            allow,
        })]
    }

    /// Sends key input and paste to the attached session. A scrolled-back screen is returned to the bottom. With no
    /// connection, nothing is sent (it would only pile up in the link thread's queue and be dropped on reconnect).
    pub fn input(&mut self, data: Vec<u8>) -> Vec<Effect> {
        let Some(id) = self.attached.clone().filter(|_| self.connected) else {
            return Vec::new();
        };
        let scrolled = self
            .screen
            .as_ref()
            .map_or(0, |s| s.term().grid().display_offset());
        self.scroll(-(scrolled as i32));
        vec![Effect::Send(ToDaemon::Input { id, data })]
    }

    /// The deadline of the attached session's unfinished synchronized update (`?2026`).
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.screen.as_ref()?.sync_deadline()
    }

    /// If a synchronized update is past its deadline (relative to `now`), ends it and applies the gathered output to the
    /// screen. Keeps the pane from being stuck on a frozen screen. Returns `true` if it ended one (a redraw is needed).
    pub fn end_stale_sync(&mut self, now: Instant) -> bool {
        match &mut self.screen {
            Some(screen) if screen.sync_deadline().is_some_and(|t| t <= now) => {
                screen.end_sync();
                screen.take_replies();
                true
            }
            _ => false,
        }
    }

    /// The groups drawn in the list. Takes the default collapsing, applies `toggled` on top, and always expands the attached session's group.
    pub fn visible_groups(&self) -> Vec<Group> {
        let attached = self
            .attached
            .as_ref()
            .and_then(|id| self.sessions.iter().position(|s| s.meta.id == *id));
        let toggled: Vec<SessionState> = self
            .toggled
            .iter()
            .filter_map(|&i| STATES.get(i).cloned())
            .collect();
        view::expand(
            view::groups(&self.sessions, self.filter.as_deref()),
            &toggled,
            attached,
        )
    }

    /// Collapses or expands the `state` group. Even if the list changes after drawing and the group positions shift, the clicked group is the one that changes.
    pub fn toggle_group(&mut self, state: &SessionState) {
        if let Some(slot) = STATES.iter().position(|s| s == state)
            && !self.toggled.remove(&slot)
        {
            self.toggled.insert(slot);
        }
    }

    /// The rows of the project picker. The first row is All projects, followed by registered projects, projects that have
    /// sessions, and the project the app was launched from, in folder-name order. If the launched-from project is the
    /// home-directory substitute (`home_fallback`), it is left out.
    /// `approval` is on for a project that has a session needing approval (for All projects, any one).
    pub fn projects(&self) -> Vec<ProjectItem> {
        let mut roots: BTreeMap<&Path, bool> = BTreeMap::new();
        if !self.home_fallback {
            roots.insert(&self.current_project, false);
        }
        for root in &self.registered {
            roots.entry(root).or_insert(false);
        }
        for s in &self.sessions {
            *roots.entry(&s.meta.project).or_insert(false) |= s.state == SessionState::Approval;
        }
        let mut roots: Vec<_> = roots.into_iter().collect();
        roots.sort_by(|(a, _), (b, _)| a.file_name().cmp(&b.file_name()).then_with(|| a.cmp(b)));
        let all = ProjectItem {
            root: None,
            approval: roots.iter().any(|(_, approval)| *approval),
        };
        let items = roots.into_iter().map(|(root, approval)| ProjectItem {
            root: Some(root.to_path_buf()),
            approval,
        });
        std::iter::once(all).chain(items).collect()
    }
}

/// One row of the project picker. `None` for `root` means All projects.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectItem {
    pub root: Option<PathBuf>,
    pub approval: bool,
}

/// The status symbol attached to groups and tabs.
pub fn glyph(state: &SessionState) -> &'static str {
    match state {
        SessionState::Approval => "◆",
        SessionState::Working => "◐",
        SessionState::Done | SessionState::Ended => "✓",
        SessionState::Idle => "○",
    }
}

/// Group name.
pub fn group_label(state: &SessionState) -> &'static str {
    match state {
        SessionState::Approval => "Needs approval",
        SessionState::Working => "Working",
        SessionState::Done | SessionState::Ended => "Done",
        SessionState::Idle => "Idle",
    }
}

/// The status column of a session row's second line. The waiting text if there is one (`true`), otherwise the status name. An ended session is `Exited`.
///
/// ponytail: the base document's "N min ago" is left out because `SessionInfo` has no last-activity time. Add it when the daemon carries the time the state changed.
pub fn status_text(s: &SessionInfo) -> (&str, bool) {
    match (&s.waiting, &s.state) {
        (Some(waiting), _) => (waiting, true),
        (None, SessionState::Ended) => ("Exited", false),
        (None, state) => (group_label(state), false),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alacritty_terminal::grid::Dimensions;
    use mine_core::profiles::ProfileKind;
    use mine_core::session::{Kind, Meta, Restore};

    fn s(project: &str, name: &str, state: SessionState) -> SessionInfo {
        SessionInfo {
            meta: Meta {
                id: format!("k-{name}"),
                project: PathBuf::from(project),
                name: name.into(),
                kind: Kind::Shell,
                account: None,
                model: None,
                restore: Restore::None,
                created_ms: 0,
                resume_id: None,
                closed: false,
            },
            state,
            waiting: None,
            alive: true,
            pending: None,
        }
    }

    #[test]
    fn profiles_from_the_daemon_replace_the_last_list() {
        let mut m = Model::new(PathBuf::from("/p"));
        assert!(m.profiles.is_empty());
        let profiles = |profiles| LinkEvent::Msg(FromDaemon::Profiles { profiles });
        assert!(m.on_link(profiles(builtin())).is_empty());
        assert_eq!(m.profiles, builtin());
        // After a reload the daemon sends the whole list again, and it replaces the old one.
        let codex = vec![builtin().remove(2)];
        m.on_link(profiles(codex.clone()));
        assert_eq!(m.profiles, codex);
        // A dropped connection keeps the last list.
        m.on_link(LinkEvent::Disconnected("disconnected".into()));
        assert_eq!(m.profiles, codex);
    }

    #[test]
    fn app_digit_key_starts_profile() {
        let key = |source: &str| gpui::Keystroke::parse(source).unwrap();
        let mut m = connected("/p");
        let two = crate::launcher::choice_of_key(&key("2"), &m.profiles).unwrap();
        assert_eq!(
            m.launch(two),
            [new_msg("/p", "t1", Kind::Claude, Some("claude-b"))]
        );
        // A profile that isn't logged in doesn't start, and says how to log in. Enter is the default Claude profile.
        let mut m = connected("/p");
        let mut list = builtin();
        list[0].logged_in = false;
        m.on_link(LinkEvent::Msg(FromDaemon::Profiles { profiles: list }));
        let enter = crate::launcher::choice_of_key(&key("enter"), &m.profiles).unwrap();
        assert_eq!(enter, Choice::Profile("claude-a".into()));
        assert!(m.launch(enter).is_empty());
        assert!(m.pending.is_none());
        assert_eq!(
            m.message.as_deref(),
            Some("Not logged in. Run: mine profile login claude-a")
        );
    }

    /// The daemon sent a list. Returns what the model asked for.
    fn listed(m: &mut Model, sessions: Vec<SessionInfo>) -> Vec<Effect> {
        m.on_link(LinkEvent::Msg(FromDaemon::Sessions { sessions }))
    }

    /// The test's terminal pane size (columns, rows).
    const GRID: (u16, u16) = (80, 24);

    /// An attached model that received the built-in profiles ([`builtin`]). The terminal pane size is [`GRID`], and the first list is
    /// empty, so it doesn't attach to any session on its own.
    fn connected(project: &str) -> Model {
        let mut m = Model::new(PathBuf::from(project));
        m.on_link(LinkEvent::Connected(vec![]));
        m.on_link(LinkEvent::Msg(FromDaemon::Profiles {
            profiles: builtin(),
        }));
        assert!(m.set_grid(GRID.0, GRID.1).is_empty());
        assert!(listed(&mut m, Vec::new()).is_empty());
        m
    }

    fn attach_at(id: &str, cols: u16, rows: u16) -> Effect {
        Effect::Send(ToDaemon::Attach {
            id: id.into(),
            cols,
            rows,
        })
    }

    fn attach_msg(id: &str) -> Effect {
        attach_at(id, GRID.0, GRID.1)
    }

    fn resize_at(id: &str, cols: u16, rows: u16) -> Effect {
        Effect::Send(ToDaemon::Resize {
            id: id.into(),
            cols,
            rows,
        })
    }

    /// A model that knows the pane size is attached to session `t1` of `/p`.
    pub(crate) fn attached_t1() -> Model {
        let mut m = connected("/p");
        listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]);
        assert_eq!(m.attach("k-t1"), [attach_msg("k-t1")]);
        m
    }

    pub(crate) fn output(id: &str, data: &[u8]) -> LinkEvent {
        LinkEvent::Msg(FromDaemon::Output {
            id: id.into(),
            data: data.to_vec(),
        })
    }

    pub(crate) fn snapshot(id: &str, data: Vec<u8>) -> LinkEvent {
        LinkEvent::Msg(FromDaemon::Snapshot {
            id: id.into(),
            data,
        })
    }

    fn lines(m: &Model) -> Vec<String> {
        m.screen.as_ref().expect("no screen").lines()
    }

    #[test]
    fn first_attach_waits_for_the_pane_size() {
        let mut m = Model::new(PathBuf::from("/p"));
        m.on_link(LinkEvent::Connected(vec![]));
        // The pane hasn't been drawn yet, so the size is unknown. It chooses the session to attach to but defers `Attach`.
        // Attaching with a wrong size would change the PTY size of the running session and garble its screen.
        assert!(listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]).is_empty());
        assert_eq!(m.attached.as_deref(), Some("k-t1"));
        assert!(!m.launcher_open());
        // Once it is drawn first and the size is known, it attaches at that size (not a `Resize`).
        assert_eq!(m.set_grid(100, 30), [attach_at("k-t1", 100, 30)]);
        assert!(m.set_grid(100, 30).is_empty());
        assert_eq!(m.set_grid(90, 30), [resize_at("k-t1", 90, 30)]);
    }

    #[test]
    fn resize_only_on_change() {
        let mut m = attached_t1();
        assert_eq!(m.set_grid(100, 30), [resize_at("k-t1", 100, 30)]);
        assert!(m.set_grid(100, 30).is_empty());
        assert_eq!(m.set_grid(100, 31), [resize_at("k-t1", 100, 31)]);
        // With no attached session, it only remembers the size and carries it the next time it attaches.
        m.set_filter(Some(PathBuf::from("/q")));
        assert!(m.attached.is_none());
        assert!(m.set_grid(120, 40).is_empty());
        m.set_filter(None);
        assert_eq!(m.attach("k-t1"), [attach_at("k-t1", 120, 40)]);
    }

    #[test]
    fn reattach_after_reconnect() {
        let mut m = attached_t1();
        assert!(
            m.on_link(LinkEvent::Disconnected("disconnected".into()))
                .is_empty()
        );
        // If the pane size changes while disconnected, it isn't sent but remembered.
        assert!(m.set_grid(100, 30).is_empty());
        // On reconnect, it subscribes and reports colors. Once the list shows the session is still there, it reattaches
        // to it at the current size.
        let effects = m.on_link(LinkEvent::Connected(vec![]));
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0], Effect::Send(ToDaemon::List));
        assert!(matches!(effects[1], Effect::Send(ToDaemon::Colors { .. })));
        assert_eq!(
            listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]),
            [attach_at("k-t1", 100, 30)]
        );
        // After that, only a changed size goes out as `Resize`.
        assert_eq!(m.set_grid(100, 32), [resize_at("k-t1", 100, 32)]);
        // With no attached session there is nothing to reattach.
        let mut m = connected("/p");
        m.on_link(LinkEvent::Disconnected("disconnected".into()));
        assert_eq!(m.on_link(LinkEvent::Connected(vec![])).len(), 2);
    }

    #[test]
    fn offline_input_and_resize_are_not_queued() {
        // Messages sent while there is no connection pile up in the link thread's queue and are dropped. Nothing is sent while disconnected.
        let mut m = attached_t1();
        assert_eq!(
            m.input(b"ls\r".to_vec()),
            [Effect::Send(ToDaemon::Input {
                id: "k-t1".into(),
                data: b"ls\r".to_vec(),
            })]
        );
        m.on_link(LinkEvent::Disconnected("disconnected".into()));
        assert!(m.input(b"x".to_vec()).is_empty());
        assert!(m.set_grid(50, 20).is_empty());
        // `Attach` isn't sent for a session picked before connecting either; it is sent once connected and listed.
        let mut m = Model::new(PathBuf::from("/p"));
        assert!(m.set_grid(GRID.0, GRID.1).is_empty());
        assert!(m.attach("k-t1").is_empty());
        assert_eq!(m.on_link(LinkEvent::Connected(vec![])).len(), 2);
        assert_eq!(
            listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]),
            [attach_msg("k-t1")]
        );
        // With no attached session there is nowhere to send input.
        let mut m = connected("/p");
        assert!(m.input(b"x".to_vec()).is_empty());
    }

    #[test]
    fn snapshot_builds_screen_at_pane_size() {
        let mut m = attached_t1();
        m.set_grid(100, 30);
        // The screen of another session is dropped (it was sent before the attached session changed).
        m.on_link(snapshot("k-old", b"old".to_vec()));
        assert!(m.screen.is_none());
        let mut source = Screen::new(100, 30);
        source.feed(b"\x1b[31mhello\x1b[0m\r\nworld");
        assert!(m.on_link(snapshot("k-t1", source.snapshot())).is_empty());
        let grid = m.screen.as_ref().unwrap().term().grid();
        assert_eq!((grid.columns(), grid.screen_lines()), (100, 30));
        assert_eq!(lines(&m)[..2], ["hello", "world"]);
        // Feed the output that follows. The daemon answers queries (DA1), so the client screen's answers are dropped.
        assert!(m.on_link(output("k-t1", b"!\x1b[c")).is_empty());
        assert_eq!(lines(&m)[1], "world!");
        assert!(m.screen.as_mut().unwrap().take_replies().is_empty());
        // Another session's output isn't fed.
        m.on_link(output("k-old", b"zzz"));
        assert_eq!(lines(&m)[1], "world!");
        // When the pane size changes, the screen is fitted too.
        m.set_grid(90, 20);
        let grid = m.screen.as_ref().unwrap().term().grid();
        assert_eq!((grid.columns(), grid.screen_lines()), (90, 20));
        // On attaching to another session, the screen is dropped and that session's `Snapshot` is awaited.
        listed(
            &mut m,
            vec![
                s("/p", "t1", SessionState::Idle),
                s("/p", "t2", SessionState::Idle),
            ],
        );
        assert_eq!(m.attach("k-t2"), [attach_at("k-t2", 90, 20)]);
        assert!(m.screen.is_none());
    }

    #[test]
    fn stale_sync_ends_after_the_deadline() {
        let mut m = attached_t1();
        m.on_link(snapshot("k-t1", Screen::new(80, 24).snapshot()));
        assert_eq!(m.sync_deadline(), None);
        // A synchronized update (`?2026`) was opened and not closed. Output in the meantime doesn't show on the screen.
        m.on_link(output("k-t1", b"\x1b[?2026hstuck"));
        let deadline = m.sync_deadline().expect("no deadline");
        assert_eq!(lines(&m)[0], "");
        assert!(!m.end_stale_sync(deadline - Duration::from_millis(1)));
        // Once the deadline passes, it ends the update and applies the gathered output to the screen.
        assert!(m.end_stale_sync(deadline));
        assert_eq!(lines(&m)[0], "stuck");
        assert_eq!(m.sync_deadline(), None);
        assert!(!m.end_stale_sync(deadline + Duration::from_secs(1)));
    }

    #[test]
    fn scroll_moves_the_view_and_input_returns_to_the_bottom() {
        let mut m = attached_t1();
        // Before a screen arrives there is nothing to scroll.
        assert!(!m.scroll(3));
        m.on_link(snapshot("k-t1", Screen::new(80, 24).snapshot()));
        let many: String = (1..=40).map(|i| format!("{i}\r\n")).collect();
        m.on_link(output("k-t1", many.as_bytes()));
        let offset = |m: &Model| m.screen.as_ref().unwrap().term().grid().display_offset();
        assert!(m.scroll(3));
        assert_eq!(offset(&m), 3);
        assert!(!m.scroll(0));
        assert!(m.scroll(-1));
        assert_eq!(offset(&m), 2);
        // Sending input to the session returns it to the bottom.
        assert_eq!(m.input(b"x".to_vec()).len(), 1);
        assert_eq!(offset(&m), 0);
    }

    fn new_msg(project: &str, name: &str, kind: Kind, account: Option<&str>) -> Effect {
        Effect::Send(ToDaemon::New {
            spec: NewSession {
                project: PathBuf::from(project),
                name: name.into(),
                kind,
                account: account.map(str::to_string),
                model: None,
            },
        })
    }

    fn names(m: &Model, rows: &[usize]) -> Vec<String> {
        rows.iter()
            .map(|&i| m.sessions[i].meta.name.clone())
            .collect()
    }

    #[test]
    fn window_focus_is_reported_on_change_and_on_every_connect() {
        let focus = |focused| Effect::Send(ToDaemon::Focus { focused });
        let mut m = Model::new(PathBuf::from("/p"));
        // With no connection there is no one to tell. The connect reports it.
        assert!(m.set_focused(true).is_empty());
        assert!(
            m.on_link(LinkEvent::Connected(vec![]))
                .contains(&focus(true))
        );
        assert_eq!(m.set_focused(false), [focus(false)]);
        assert_eq!(m.set_focused(true), [focus(true)]);
        assert!(m.set_focused(true).is_empty());
        // A new connection starts unfocused in the daemon, so a focused window says so again.
        m.on_link(LinkEvent::Disconnected("disconnected".into()));
        assert!(
            m.on_link(LinkEvent::Connected(vec![]))
                .contains(&focus(true))
        );
    }

    #[test]
    fn presence_sent_on_activate_and_input() {
        let presence = |effects: Vec<Effect>| -> Vec<(bool, u64, Option<String>)> {
            effects
                .into_iter()
                .filter_map(|e| match e {
                    Effect::Send(ToDaemon::Presence {
                        visible,
                        last_input_ms,
                        viewing,
                    }) => Some((visible, last_input_ms, viewing)),
                    _ => None,
                })
                .collect()
        };
        let t1 = || Some("k-t1".to_string());
        let mut m = Model::new(PathBuf::from("/p"));
        assert!(m.set_focused(true).is_empty());
        // A daemon that announced `presence` hears `Presence` from the connect on, never `Focus`. Activating the window was input.
        let before = mine_core::hooks::now_ms();
        let effects = m.on_link(LinkEvent::Connected(vec!["presence".into()]));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Send(ToDaemon::Focus { .. }))),
            "{effects:?}"
        );
        let [(true, activated, None)] = presence(effects)[..] else {
            panic!("no Presence on connect");
        };
        assert!(
            activated > 0 && activated <= before,
            "activating counts as input"
        );
        // Attaching shows that session's terminal.
        m.set_grid(GRID.0, GRID.1);
        listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]);
        assert_eq!(
            presence(m.presence_tick(Instant::now())),
            [(true, activated, t1())]
        );
        // Deactivating and activating go out at once.
        assert_eq!(presence(m.set_focused(false)), [(false, activated, t1())]);
        assert_eq!(m.presence_deadline(), None);
        let [(true, activated, _)] = presence(m.set_focused(true))[..] else {
            panic!("no Presence on activate");
        };
        let sent = Instant::now();
        // Input within 5 s of the last send waits for the next tick past it, which carries it.
        let typed = mine_core::hooks::now_ms();
        assert!(m.user_input(sent).is_empty());
        assert!(m.presence_tick(sent + Duration::from_secs(1)).is_empty());
        let [(true, input, _)] = presence(m.presence_tick(sent + Duration::from_secs(6)))[..]
        else {
            panic!("the input wasn't reported");
        };
        assert!(input >= typed && input >= activated);
        // The launcher over the session hides its terminal.
        m.new_task();
        let at = sent + Duration::from_secs(7);
        assert_eq!(presence(m.presence_tick(at)), [(true, input, None)]);
        // While active, a heartbeat 30 s after the last send.
        assert_eq!(
            m.presence_deadline(),
            Some(at + mine_core::proto::PRESENCE_HEARTBEAT)
        );
    }

    #[test]
    fn presence_heartbeat_stops_while_disconnected() {
        let connect = |m: &mut Model| m.on_link(LinkEvent::Connected(vec!["presence".into()]));
        let mut m = Model::new(PathBuf::from("/p"));
        m.set_focused(true);
        connect(&mut m);
        let sent = Instant::now();
        assert!(m.presence_deadline().is_some());
        // A heartbeat that came due during the outage is neither due nor sent: nothing for the heartbeat task to wake for.
        m.on_link(LinkEvent::Disconnected("disconnected".into()));
        assert_eq!(m.presence_deadline(), None);
        assert!(m.presence_tick(sent + Duration::from_secs(60)).is_empty());
        // Reconnecting reports at once and arms the heartbeat again.
        let effects = connect(&mut m);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::Send(ToDaemon::Presence { visible: true, .. }))),
            "{effects:?}"
        );
        assert!(m.presence_deadline().is_some_and(|d| d > sent));
    }

    #[test]
    fn connected_subscribes_and_reports_colors() {
        let mut m = Model::new(PathBuf::from("/p/mine"));
        assert!(!m.connected);
        assert_eq!(
            m.on_link(LinkEvent::Connected(vec![])),
            vec![
                Effect::Send(ToDaemon::List),
                Effect::Send(ToDaemon::Colors {
                    fg: "#d0d3da".into(),
                    bg: "#1e2128".into(),
                }),
            ]
        );
        assert!(m.connected);
    }

    #[test]
    fn sessions_group_by_state() {
        let mut m = connected("/p/mine");
        listed(
            &mut m,
            vec![
                s("/p/mine", "idle", SessionState::Idle),
                s("/p/kyoto", "ended", SessionState::Ended),
                s("/p/mine", "work", SessionState::Working),
                s("/p/kyoto", "ask", SessionState::Approval),
            ],
        );
        let g = m.visible_groups();
        let glyphs: Vec<_> = g.iter().map(|g| glyph(&g.state)).collect();
        assert_eq!(glyphs, ["◆", "◐", "✓", "○"]);
        let rows: Vec<_> = g.iter().map(|g| g.rows.clone()).collect();
        assert_eq!(rows, [vec![3], vec![2], vec![1], vec![0]]);
        let collapsed: Vec<_> = g.iter().map(|g| g.collapsed).collect();
        assert_eq!(collapsed, [false, false, false, true]);
        // Clicking a header expands the Idle group, and clicking again collapses it.
        m.toggle_group(&SessionState::Idle);
        assert!(!m.visible_groups()[3].collapsed);
        m.toggle_group(&SessionState::Idle);
        assert!(m.visible_groups()[3].collapsed);
        // Picking a project groups only that project's sessions.
        m.filter = Some(PathBuf::from("/p/kyoto"));
        let states: Vec<_> = m.visible_groups().into_iter().map(|g| g.state).collect();
        assert_eq!(states, [SessionState::Approval, SessionState::Done]);
        assert_eq!(m.current_project, Path::new("/p/mine"));
    }

    #[test]
    fn group_toggle_follows_the_clicked_state_not_its_position() {
        let mut m = connected("/p");
        listed(
            &mut m,
            vec![
                s("/p", "work", SessionState::Working),
                s("/p", "idle", SessionState::Idle),
            ],
        );
        // In the drawn screen, the second header is the Idle group.
        let clicked = m.visible_groups()[1].state.clone();
        assert_eq!(clicked, SessionState::Idle);
        // A new list arrived before the click, and a Needs approval group got in front. The second position is now Working.
        listed(
            &mut m,
            vec![
                s("/p", "ask", SessionState::Approval),
                s("/p", "work", SessionState::Working),
                s("/p", "idle", SessionState::Idle),
            ],
        );
        m.toggle_group(&clicked);
        // Only the clicked Idle group changes (STATES index 3).
        assert_eq!(m.toggled, HashSet::from([3]));
        let g = m.visible_groups();
        assert_eq!(g[2].state, SessionState::Idle);
        assert!(!g[2].collapsed);
    }

    #[test]
    fn disconnect_marks_offline() {
        let mut m = connected("/p/mine");
        listed(&mut m, vec![s("/p/mine", "a", SessionState::Working)]);
        assert!(
            m.on_link(LinkEvent::Disconnected("disconnected".into()))
                .is_empty()
        );
        assert!(!m.connected);
        assert_eq!(m.offline.as_deref(), Some("disconnected"));
        assert_eq!(m.sessions.len(), 1);
        assert_eq!(m.visible_groups().len(), 1);
        // On reconnect, the strip is taken away and it subscribes again.
        assert_eq!(
            m.on_link(LinkEvent::Connected(vec![]))[0],
            Effect::Send(ToDaemon::List)
        );
        assert!(m.connected);
        assert_eq!(m.offline, None);
    }

    #[test]
    fn projects_start_with_all_and_flag_approval() {
        let mut m = Model::new(PathBuf::from("/p/mine"));
        m.registered = vec![PathBuf::from("/p/zed"), PathBuf::from("/p/mine")];
        listed(
            &mut m,
            vec![
                s("/p/kyoto", "ask", SessionState::Approval),
                s("/p/kyoto", "b", SessionState::Idle),
            ],
        );
        let items = m.projects();
        let roots: Vec<_> = items.iter().map(|p| p.root.clone()).collect();
        assert_eq!(
            roots,
            [
                None,
                Some(PathBuf::from("/p/kyoto")),
                Some(PathBuf::from("/p/mine")),
                Some(PathBuf::from("/p/zed")),
            ]
        );
        let approval: Vec<_> = items.iter().map(|p| p.approval).collect();
        assert_eq!(approval, [true, true, false, false]);
    }

    #[test]
    fn home_fallback_is_not_listed_as_a_project() {
        let mut m = Model::new(PathBuf::from("/Users/me"));
        m.home_fallback = true;
        let roots = |m: &Model| -> Vec<_> { m.projects().into_iter().map(|p| p.root).collect() };
        assert_eq!(roots(&m), [None]);
        m.registered = vec![PathBuf::from("/p/mine")];
        assert_eq!(roots(&m), [None, Some(PathBuf::from("/p/mine"))]);
        // The launcher still has somewhere to create sessions.
        assert_eq!(m.project(), Path::new("/Users/me"));
        // The project of an app launched by another route always exists.
        m.home_fallback = false;
        assert_eq!(roots(&m).len(), 3);
    }

    #[test]
    fn launch_creates_t_n_and_attaches_when_listed() {
        let mut m = connected("/p");
        assert_eq!(
            m.launch(Choice::Profile("claude-a".into())),
            [new_msg("/p", "t1", Kind::Claude, Some("claude-a"))]
        );
        // While waiting for an answer, other rows aren't accepted. This keeps two sends under the same name from tangling.
        assert!(m.launch(Choice::Shell).is_empty());
        assert!(m.launcher_open());
        // When the created session shows up in the list, it attaches and the launcher closes.
        assert_eq!(
            listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]),
            [attach_msg("k-t1")]
        );
        assert_eq!(m.attached.as_deref(), Some("k-t1"));
        assert!(!m.launcher_open() && m.pending.is_none());
        // The next name is t2. If the daemon refuses, it shows the reason and you can pick again.
        m.new_task();
        assert_eq!(
            m.launch(Choice::Profile("codex".into())),
            [new_msg("/p", "t2", Kind::Codex, Some("codex"))]
        );
        m.on_link(LinkEvent::Msg(FromDaemon::Error {
            message: "nope".into(),
        }));
        assert_eq!(m.message.as_deref(), Some("nope"));
        assert_eq!(
            m.launch(Choice::Profile("claude-b".into())),
            [new_msg("/p", "t2", Kind::Claude, Some("claude-b"))]
        );
        assert_eq!(m.message, None);
    }

    #[test]
    fn launch_needs_the_daemon_and_a_lost_new_does_not_block() {
        // Before connecting, the link thread drops sent messages. It doesn't create anything and shows the reason.
        let mut m = Model::new(PathBuf::from("/p"));
        assert!(m.launch(Choice::Shell).is_empty());
        assert!(m.submit_typed("shell dev").is_empty());
        assert!(m.pending.is_none());
        assert!(m.message.as_deref().unwrap().contains("daemon"));
        // If the connection drops after sending `New`, no answer may ever come. Release the wait so you can pick again.
        m.on_link(LinkEvent::Connected(vec![]));
        assert_eq!(m.launch(Choice::Shell).len(), 1);
        m.on_link(LinkEvent::Disconnected("disconnected".into()));
        assert!(m.pending.is_none());
        m.on_link(LinkEvent::Connected(vec![]));
        assert_eq!(
            m.launch(Choice::Shell),
            [new_msg("/p", "t1", Kind::Shell, None)]
        );
    }

    #[test]
    fn a_new_acknowledged_without_a_session_frees_the_launcher() {
        let mut m = connected("/p");
        assert_eq!(
            m.launch(Choice::Shell),
            [new_msg("/p", "t1", Kind::Shell, None)]
        );
        // Picking while waiting for an answer reports that it is being created.
        assert!(m.launch(Choice::Profile("codex".into())).is_empty());
        assert_eq!(m.message.as_deref(), Some("Creating…"));
        // If the shell ends right away, the daemon answers without a session. The list didn't change, so the daemon doesn't announce, and only the answer comes.
        assert!(listed(&mut m, Vec::new()).is_empty());
        assert!(m.pending.is_none());
        assert_eq!(m.message.as_deref(), Some("Couldn't start the session"));
        // The launcher works again.
        assert!(m.launcher_open());
        assert_eq!(
            m.launch(Choice::Shell),
            [new_msg("/p", "t1", Kind::Shell, None)]
        );
        assert_eq!(m.message, None);
    }

    #[test]
    fn an_unrelated_list_keeps_waiting_until_the_timeout() {
        let mut m = connected("/p");
        m.launch(Choice::Shell);
        let since = m.pending.as_ref().unwrap().since;
        // A list the daemon announced because another session changed while creating isn't the answer. Keep waiting.
        assert!(listed(&mut m, vec![s("/q", "x", SessionState::Working)]).is_empty());
        assert!(m.pending.is_some() && m.message.is_none());
        assert!(!m.expire_pending(since + Duration::from_secs(4)));
        assert!(m.pending.is_some());
        // Past 5 seconds, it drops the wait and reports.
        assert!(m.expire_pending(since + PENDING_TIMEOUT));
        assert!(m.pending.is_none());
        assert_eq!(m.message.as_deref(), Some("Couldn't start the session"));
        assert!(!m.expire_pending(since + PENDING_TIMEOUT));
        assert_eq!(m.launch(Choice::Shell).len(), 1);
    }

    #[test]
    fn typed_choice_parses_kind_name_and_model() {
        let mut m = connected("/p");
        let mut list = builtin();
        list.push(profile("team", ProfileKind::Claude, "team", "#98c379"));
        m.on_link(LinkEvent::Msg(FromDaemon::Profiles { profiles: list }));
        assert!(m.launch(Choice::Typed).is_empty());
        assert!(m.typing && m.pending.is_none());
        // Wrong input shows the reason with the usage, and leaves the box open.
        assert!(m.submit_typed("gpt x").is_empty());
        assert!(m.typing);
        let message = m.message.clone().unwrap();
        assert!(message.contains("Unknown profile: gpt"), "{message}");
        assert!(message.contains(view::NEW_USAGE), "{message}");
        // Any profile in the list can be typed, not only the built-in ones.
        let Effect::Send(ToDaemon::New { spec }) = &m.submit_typed("team t9 opus")[0] else {
            panic!("not a New");
        };
        assert_eq!(
            (
                spec.kind,
                spec.name.as_str(),
                spec.account.as_deref(),
                spec.model.as_deref()
            ),
            (Kind::Claude, "t9", Some("team"), Some("opus"))
        );
        assert!(!m.typing && m.message.is_none());
        let p = m.pending.as_ref().unwrap();
        assert_eq!(
            (p.project.as_path(), p.name.as_str()),
            (Path::new("/p"), "t9")
        );
    }

    #[test]
    fn app_typed_form_refuses_a_profile_that_isnt_logged_in() {
        let mut m = connected("/p");
        let mut list = builtin();
        list[1].logged_in = false;
        m.on_link(LinkEvent::Msg(FromDaemon::Profiles { profiles: list }));
        m.launch(Choice::Typed);
        // Like a typing error: nothing is sent, the box stays open, and the message says how to log in.
        assert!(m.submit_typed("claude-b t9").is_empty());
        assert!(m.typing && m.pending.is_none());
        assert_eq!(
            m.message.as_deref(),
            Some("Not logged in. Run: mine profile login claude-b")
        );
        assert_eq!(
            m.submit_typed("claude-a t9"),
            [new_msg("/p", "t9", Kind::Claude, Some("claude-a"))]
        );
    }

    #[test]
    fn launch_goes_to_the_selected_or_attached_project() {
        let mut m = connected("/p/mine");
        listed(&mut m, vec![s("/p/kyoto", "t1", SessionState::Idle)]);
        // In All projects with no attached session, it is the project the app was launched from.
        assert_eq!(m.project(), Path::new("/p/mine"));
        // In All projects with an attached session, it is that session's project.
        m.attach("k-t1");
        m.new_task();
        assert_eq!(m.project(), Path::new("/p/kyoto"));
        assert_eq!(
            m.launch(Choice::Shell),
            [new_msg("/p/kyoto", "t2", Kind::Shell, None)]
        );
        // When a project is picked, it is that project.
        m.filter = Some(PathBuf::from("/p/zed"));
        assert_eq!(m.project(), Path::new("/p/zed"));
    }

    #[test]
    fn tabs_follow_selected_project() {
        let mut m = connected("/p/mine");
        listed(
            &mut m,
            vec![
                s("/p/mine", "zeta", SessionState::Working),
                s("/p/kyoto", "kz", SessionState::Idle),
                s("/p/mine", "alpha", SessionState::Approval),
                s("/p/mine", "t2", SessionState::Ended),
            ],
        );
        m.filter = Some(PathBuf::from("/p/mine"));
        assert_eq!(names(&m, &m.tabs()), ["alpha", "t2", "zeta"]);
        m.filter = Some(PathBuf::from("/p/kyoto"));
        assert_eq!(names(&m, &m.tabs()), ["kz"]);
        m.filter = None;
        assert_eq!(names(&m, &m.tabs()), ["alpha", "kz", "t2", "zeta"]);
        // ⌘2 attaches to the second tab, and a nonexistent tab does nothing.
        m.filter = Some(PathBuf::from("/p/mine"));
        assert_eq!(m.select_tab(1), [attach_msg("k-t2")]);
        assert!(m.select_tab(3).is_empty());
        assert_eq!(m.attached.as_deref(), Some("k-t2"));
        // Picking an already attached tab again doesn't tell the daemon again.
        assert!(m.select_tab(1).is_empty());
    }

    #[test]
    fn step_session_wraps() {
        let mut m = Model::new(PathBuf::from("/p"));
        m.on_link(LinkEvent::Connected(vec![]));
        m.set_grid(GRID.0, GRID.1);
        // For the first list received, it attaches to the first visible session (Needs approval comes first).
        assert_eq!(
            listed(
                &mut m,
                vec![
                    s("/p", "a", SessionState::Working),
                    s("/p", "b", SessionState::Working),
                    s("/p", "c", SessionState::Approval),
                    s("/p", "d", SessionState::Idle),
                ],
            ),
            [attach_msg("k-c")]
        );
        // The visible order is c, a, b. d in the collapsed Idle group is skipped.
        let mut visit = |delta| {
            m.step_session(delta);
            m.attached.clone().unwrap()
        };
        assert_eq!(visit(1), "k-a");
        assert_eq!(visit(1), "k-b");
        assert_eq!(visit(1), "k-c");
        assert_eq!(visit(-1), "k-b");
        assert_eq!(m.step_session(-1), [attach_msg("k-a")]);
        // With no attached session, next is the first and previous is the last.
        m.attached = None;
        assert_eq!(m.step_session(1), [attach_msg("k-c")]);
        m.attached = None;
        assert_eq!(m.step_session(-1), [attach_msg("k-b")]);
        // With no sessions at all, it does nothing.
        m.set_filter(Some(PathBuf::from("/q")));
        assert!(m.step_session(1).is_empty());
    }

    #[test]
    fn step_project_cycles_through_the_picker() {
        let mut m = connected("/p/mine");
        listed(&mut m, vec![s("/p/kyoto", "t1", SessionState::Idle)]);
        m.attach("k-t1");
        // The project-picker order is All projects, kyoto, mine.
        m.step_project(1);
        assert_eq!(m.filter, Some(PathBuf::from("/p/kyoto")));
        assert_eq!(m.attached.as_deref(), Some("k-t1"));
        // If the attached session isn't in the picked project, it is let go and the launcher shows.
        m.step_project(1);
        assert_eq!(m.filter, Some(PathBuf::from("/p/mine")));
        assert!(m.attached.is_none() && m.launcher_open());
        m.step_project(1);
        assert_eq!(m.filter, None);
        m.step_project(-1);
        assert_eq!(m.filter, Some(PathBuf::from("/p/mine")));
    }

    #[test]
    fn close_requires_confirmation() {
        let mut m = connected("/p");
        // With no session to close, it closes the window.
        assert_eq!(m.close(), Close::Window);
        listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]);
        m.attach("k-t1");
        // Nothing is sent before the confirmation answer arrives, and while the dialog is showing ⌘W doesn't ask again.
        assert_eq!(m.close(), Close::Confirm("t1".into()));
        assert_eq!(m.close(), Close::Nothing);
        assert!(m.confirm_close(false).is_empty());
        assert_eq!(m.attached.as_deref(), Some("k-t1"));
        assert_eq!(m.close(), Close::Confirm("t1".into()));
        assert_eq!(
            m.confirm_close(true),
            [Effect::Send(ToDaemon::Kill { id: "k-t1".into() })]
        );
        assert!(m.closing.is_none());
        // Even if the answer comes twice, it closes only once.
        assert!(m.confirm_close(true).is_empty());
        // For a launcher opened over an attached session, ⌘W closes only the launcher.
        m.new_task();
        assert_eq!(m.close(), Close::Launcher);
        assert!(!m.launcher_open() && m.closing.is_none());
        // When the closed session leaves the list, no sessions remain, so the launcher shows.
        assert!(listed(&mut m, Vec::new()).is_empty());
        assert!(m.attached.is_none() && m.launcher_open());
    }

    #[test]
    fn close_while_disconnected_says_so_and_keeps_the_session() {
        let mut m = attached_t1();
        assert_eq!(m.close(), Close::Confirm("t1".into()));
        m.on_link(LinkEvent::Disconnected("disconnected".into()));
        // The link would drop a `Kill` sent now. Nothing is sent, the session stays, and the user is told.
        assert!(m.confirm_close(true).is_empty());
        assert_eq!(m.attached.as_deref(), Some("k-t1"));
        assert_eq!(m.message.as_deref(), Some(CLOSE_OFFLINE));
        assert!(m.closing.is_none());
        // Once it reconnects, closing works again.
        m.on_link(LinkEvent::Connected(vec![]));
        assert_eq!(m.close(), Close::Confirm("t1".into()));
        assert_eq!(
            m.confirm_close(true),
            [Effect::Send(ToDaemon::Kill { id: "k-t1".into() })]
        );
    }

    #[test]
    fn reconnect_drops_the_attach_to_a_session_that_vanished() {
        let mut m = attached_t1();
        m.on_link(LinkEvent::Disconnected("disconnected".into()));
        // `Attach` waits for the first list: the session may have ended while the daemon was down.
        let effects = m.on_link(LinkEvent::Connected(vec![]));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Send(ToDaemon::Attach { .. })))
        );
        // It did end. No `Attach` goes out for it (whose "No such session" error would stay on screen), and the
        // first visible session is attached instead, with no message.
        let effects = listed(&mut m, vec![s("/p", "t2", SessionState::Idle)]);
        assert_eq!(effects, [attach_msg("k-t2")]);
        assert_eq!(m.attached.as_deref(), Some("k-t2"));
        assert_eq!(m.message, None);
        // With nothing left, the launcher shows and nothing is sent.
        m.on_link(LinkEvent::Disconnected("disconnected".into()));
        m.on_link(LinkEvent::Connected(vec![]));
        assert!(listed(&mut m, Vec::new()).is_empty());
        assert!(m.attached.is_none() && m.launcher_open());
    }

    #[test]
    fn no_session_shows_launcher() {
        let mut m = Model::new(PathBuf::from("/p"));
        assert!(m.launcher_open());
        m.on_link(LinkEvent::Connected(vec![]));
        assert!(listed(&mut m, Vec::new()).is_empty());
        assert!(m.launcher_open() && m.attached.is_none());
        // A session that appears after the first list isn't attached to on its own.
        assert!(listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]).is_empty());
        assert!(m.launcher_open());
        // With no attached session, Esc has nowhere to go back to and does nothing.
        m.dismiss();
        assert!(m.launcher_open());
        m.attach("k-t1");
        assert!(!m.launcher_open());
        // A launcher opened over a session with ⌘N is closed with Esc. If the input box is open, it first returns to the launcher.
        m.new_task();
        assert!(m.launcher_open());
        m.launch(Choice::Typed);
        m.dismiss();
        assert!(!m.typing && m.launcher_open());
        m.dismiss();
        assert!(!m.launcher_open());
        // If the attached session vanishes and no sessions remain, the launcher shows again.
        assert!(listed(&mut m, Vec::new()).is_empty());
        assert!(m.attached.is_none() && m.launcher_open());
    }

    #[test]
    fn added_project_is_selected_and_home_is_refused() {
        let mut m = connected("/p/mine");
        let kyoto = PathBuf::from("/p/kyoto");
        m.project_added(kyoto.clone(), vec![kyoto.clone()]);
        assert_eq!(m.filter, Some(kyoto.clone()));
        assert_eq!(m.registered, std::slice::from_ref(&kyoto));
        assert!(m.message.is_none());
        // If it isn't in the registered list (the home directory), it isn't picked and the user is told.
        m.project_added(PathBuf::from("/Users/me"), vec![kyoto.clone()]);
        assert_eq!(m.filter, Some(kyoto));
        assert!(m.message.as_deref().unwrap().contains("home directory"));
    }

    #[test]
    fn status_prefers_waiting_text() {
        let mut ask = s("/p", "a", SessionState::Approval);
        ask.waiting = Some("Bash(npm run deploy)".into());
        assert_eq!(status_text(&ask), ("Bash(npm run deploy)", true));
        assert_eq!(
            status_text(&s("/p", "e", SessionState::Ended)),
            ("Exited", false)
        );
        assert_eq!(
            status_text(&s("/p", "w", SessionState::Working)),
            ("Working", false)
        );
        assert_eq!(group_label(&SessionState::Ended), "Done");
    }

    /// A model on a connection whose daemon announced `ask`, listing `rows`.
    fn asking(rows: Vec<SessionInfo>) -> Model {
        let mut m = Model::new(PathBuf::from("/p"));
        m.on_link(LinkEvent::Connected(vec!["ask".into()]));
        listed(&mut m, rows);
        m
    }

    /// A session of `/p` that is holding request `request`.
    fn held(name: &str, request: u64) -> SessionInfo {
        let mut row = s("/p", name, SessionState::Approval);
        row.pending = Some(mine_core::session::PendingAsk {
            request,
            summary: "Bash(npm run deploy)".into(),
            since_ms: 1_000,
        });
        row
    }

    fn answer_msg(id: &str, request: u64, allow: bool) -> Effect {
        Effect::Send(ToDaemon::Answer {
            id: id.into(),
            request,
            allow,
        })
    }

    #[test]
    fn allow_button_sends_answer_with_request_id() {
        // Two rows hold different requests: each button answers its own row's, never the latest or the first.
        let m = asking(vec![held("t1", 7), held("t2", 9)]);
        let (first, second) = (m.answerable(&m.sessions[0]), m.answerable(&m.sessions[1]));
        assert_eq!((first, second), (Some(7), Some(9)));
        assert_eq!(m.answer("k-t2", 9, true), [answer_msg("k-t2", 9, true)]);
        assert_eq!(m.answer("k-t1", 7, true), [answer_msg("k-t1", 7, true)]);
    }

    #[test]
    fn deny_button_sends_deny() {
        let m = asking(vec![held("t1", 7)]);
        assert_eq!(m.answer("k-t1", 7, false), [answer_msg("k-t1", 7, false)]);
    }

    #[test]
    fn buttons_only_on_pending_rows() {
        let m = asking(vec![held("t1", 7), s("/p", "t2", SessionState::Approval)]);
        assert_eq!(m.answerable(&m.sessions[0]), Some(7));
        // Needs approval by hook state alone (no held request) has nothing to answer.
        assert_eq!(m.answerable(&m.sessions[1]), None);
        // A daemon that didn't announce `ask` never gets an answer, even if a row carries `pending`.
        let mut old = Model::new(PathBuf::from("/p"));
        old.on_link(LinkEvent::Connected(vec!["presence".into()]));
        listed(&mut old, vec![held("t1", 7)]);
        assert_eq!(old.answerable(&old.sessions[0]), None);
        assert!(old.answer("k-t1", 7, true).is_empty());
        // Nothing goes out while disconnected.
        let mut m = asking(vec![held("t1", 7)]);
        m.on_link(LinkEvent::Disconnected("gone".into()));
        assert_eq!(m.answerable(&m.sessions[0]), None);
        assert!(m.answer("k-t1", 7, true).is_empty());
    }
}
