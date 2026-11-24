//! The client screen (`mine` with no command). Grabs the outer terminal, attaches to the daemon, draws the chosen session's screen and passes input on.
//!
//! This one task holds all the state and waits on stdin (a thread), daemon messages (read tasks), SIGWINCH and redraws with a single
//! `select!`. Only cancellation-safe channel receives and timers go in the `select!`.

pub mod copymode;
pub mod input;
pub mod launcher;
pub mod select;
mod tty;
pub mod ui;

use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use anyhow::{Result, bail};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::{Buffer, Cell};
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::layout::{Position, Rect, Size};
use ratatui::text::Span;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::UnboundedSender;

use copymode::{CopyMode, CopyOut};
use input::{Action, Cmd, InputParser};
use mine_core::conn::Conn;
use mine_core::hooks::now_ms;
use mine_core::keys::KeyModes;
use mine_core::launch::{self, Choice};
use mine_core::paths::State;
use mine_core::proto::{FromDaemon, PresenceReport, ToDaemon};
use mine_core::screen::Screen;
use mine_core::select::{
    MouseEv, MouseKind, encode_sgr, selected_text, session_takes, wants_mouse,
};
use mine_core::session::{NewSession, SessionInfo};
use mine_core::view;
use select::Select;
use ui::{Hit, LinkRun, Model, Prompt};

/// Minimum interval between redraws (60 per second).
const FRAME: Duration = Duration::from_micros(16_667);
/// Number of lines scrolled per wheel notch.
const WHEEL_LINES: i32 = 3;

/// Outer screen coordinates and the cell there.
type Cells = Vec<(u16, u16, Cell)>;

/// Runs the client. Returns `Ok` when it exits via `⌃a d`. The daemon and sessions keep running.
pub async fn run(state: State, project: PathBuf) -> Result<()> {
    let _tty = tty::Tty::enter()?;
    // Ask before starting the stdin reader thread and finish (R13). Keys that arrive in the meantime are fed first below.
    let (colors, early) = tty::probe_colors();
    let conn = Conn::connect_or_spawn_async(&state).await?;
    let mut stdin = tty::spawn_stdin_reader();
    let mut winch = signal(SignalKind::window_change())?;
    // A terminate or hangup signal also ends it like `⌃a d`. The key encoding modes are restored and `Tty` restores the outer terminal.
    let mut term = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut terminal = Terminal::new(CrosstermBackend::new(BufWriter::with_capacity(
        1 << 16,
        io::stdout(),
    )))?;
    let mut rx = conn.rx;
    let mut app = App::new(conn.tx, project, terminal.size()?);
    app.presence = conn.features.iter().any(|f| f == "presence");
    // Running `mine` was the user's input.
    app.report.input(now_ms());
    if let Some((fg, bg)) = colors {
        app.send(ToDaemon::Colors {
            fg: fg.to_string(),
            bg: bg.to_string(),
        });
    }
    // To receive list changes, it must subscribe by sending List once (R18). When the answer arrives, it picks a session to attach to.
    app.send(ToDaemon::List);
    app.on_input(&early);
    let mut last_draw: Option<Instant> = None;
    loop {
        app.end_stale_sync();
        app.sync_presence(Instant::now());
        // The key encoding modes are applied right after the state changes, between frames, over the same write path as the screen.
        let keys = app.sync_keys();
        if !keys.is_empty() {
            let out = terminal.backend_mut();
            out.write_all(&keys)?;
            Write::flush(out)?;
        }
        if app.quit {
            break;
        }
        let next_frame = last_draw.map_or_else(Instant::now, |t| t + FRAME);
        if app.dirty && Instant::now() >= next_frame {
            app.draw(&mut terminal)?;
            last_draw = Some(Instant::now());
        }
        let wake = [
            app.dirty
                .then(|| last_draw.map_or_else(Instant::now, |t| t + FRAME)),
            app.sync_deadline(),
            app.presence_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        tokio::select! {
            bytes = stdin.recv() => match bytes {
                Some(bytes) => app.on_input(&bytes),
                None => break,
            },
            msg = rx.recv() => match msg {
                Some(msg) => app.on_msg(msg),
                None => bail!("Disconnected from the daemon. Run mine again to reattach to your sessions"),
            },
            _ = winch.recv() => app.on_resize(terminal.size()?),
            _ = term.recv() => app.quit = true,
            _ = hangup.recv() => app.quit = true,
            () = tokio::time::sleep_until(wake.into()) => {}
        }
    }
    Ok(())
}

/// kitty flags to set on the outer terminal.
///
/// ponytail: Flag 2 (reporting release and repeat events) is left out. With release and repeat, prefix key handling gets much more complicated, and
/// CLI apps rarely use it. An app that turns this flag on receives only presses.
const KITTY_MIRRORED: u8 = !2;

/// The bytes that change the outer terminal from `applied` to `wanted`. Writes only the modes that differ.
/// The kitty flags are set wholesale on the one level pushed at startup (`tty::ENTER`).
fn mode_diff(applied: KeyModes, wanted: KeyModes) -> Vec<u8> {
    let mut out = Vec::new();
    if applied.app_cursor != wanted.app_cursor {
        out.extend_from_slice(if wanted.app_cursor {
            b"\x1b[?1h"
        } else {
            b"\x1b[?1l"
        });
    }
    if applied.app_keypad != wanted.app_keypad {
        out.extend_from_slice(if wanted.app_keypad {
            b"\x1b="
        } else {
            b"\x1b>"
        });
    }
    if applied.modify_other_keys != wanted.modify_other_keys {
        let _ = write!(out, "\x1b[>4;{}m", wanted.modify_other_keys);
    }
    let kitty = wanted.kitty & KITTY_MIRRORED;
    if applied.kitty & KITTY_MIRRORED != kitty {
        let _ = write!(out, "\x1b[={kitty};1u");
    }
    out
}

/// The frozen screen being moved around in copy mode. `lines` is the scrollback + visible lines at the time of entering.
struct Copying {
    mode: CopyMode,
    lines: Vec<String>,
    /// Session output that arrived meanwhile. Fed to the screen on exit.
    held: Vec<u8>,
}

/// How long, after leaving copy mode with Esc, a lone Esc that was read separately is dropped instead of passed to the session.
/// Many people press Esc twice to get out, and the second Esc usually arrives in a separate read.
///
/// ponytail: No timer is set; the elapsed time is checked only when the next input arrives. A more precise way of measuring the interval between inputs can wait until it's needed.
const ESC_GRACE: Duration = Duration::from_millis(300);

/// Maximum bytes of output kept while in copy mode.
///
/// ponytail: If exceeded, copy mode ends and catches up. Spilling to disk or trimming the oldest output can wait until it's needed.
const MAX_HELD: usize = 8 << 20;

struct App {
    m: Model,
    /// The attached session's screen. Built from the snapshot the daemon sent and fed the output. Query answers are the daemon's job, so they're discarded.
    screen: Option<Screen>,
    tx: UnboundedSender<ToDaemon>,
    parser: InputParser,
    /// The outer terminal size.
    area: Rect,
    /// The session to attach to was chosen from the first list received.
    started: bool,
    /// The (project, name) of the session `New` was sent for. It attaches once it appears in the list.
    pending: Option<(PathBuf, String)>,
    /// Spans of the previous screen that were rewritten as hyperlinks.
    links: Vec<LinkRun>,
    /// The drag selection in the terminal pane.
    select: Select,
    /// Clipboard OSC 52 to write to the outer terminal. Emitted over the same write path as the screen when drawing.
    clipboard: Vec<u8>,
    /// Copy mode. The screen is frozen on entry, and output that arrives meanwhile is kept.
    copy: Option<Copying>,
    /// When copy mode was left with Esc. A lone Esc in front of input arriving within `ESC_GRACE` is dropped.
    esc_exit: Option<Instant>,
    /// A press passed to the session is still held. While it is, drags and releases outside the pane are clamped into the pane and passed on so the session receives the release.
    held: bool,
    /// The key encoding modes currently set on the outer terminal.
    keys: KeyModes,
    /// The daemon announced `presence`. Without it the client reports nothing, as before.
    presence: bool,
    /// The outer terminal has focus: true until a focus-out report (a terminal without focus reporting counts as visible).
    visible: bool,
    report: PresenceReport,
    dirty: bool,
    quit: bool,
}

impl App {
    fn new(tx: UnboundedSender<ToDaemon>, project: PathBuf, size: Size) -> App {
        App {
            m: Model {
                sessions: Vec::new(),
                filter: Some(project.clone()),
                current_project: project,
                attached: None,
                list_open: true,
                prompt: None,
                message: String::new(),
                toggled: Vec::new(),
                status: None,
                launcher: false,
                profiles: Vec::new(),
            },
            screen: None,
            tx,
            parser: InputParser::new(),
            area: Rect::from((Position::ORIGIN, size)),
            started: false,
            pending: None,
            links: Vec::new(),
            select: Select::default(),
            clipboard: Vec::new(),
            copy: None,
            esc_exit: None,
            held: false,
            keys: KeyModes::default(),
            presence: false,
            visible: true,
            report: PresenceReport::default(),
            dirty: true,
            quit: false,
        }
    }

    /// Sends `Presence` if it's due ([`PresenceReport::due`]). The attached session counts as viewed unless the launcher covers it.
    fn sync_presence(&mut self, now: Instant) {
        if !self.presence {
            return;
        }
        let viewing = self.m.attached.as_deref().filter(|_| !self.m.launcher);
        if let Some(msg) = self.report.due(now, self.visible, viewing) {
            self.send(msg);
        }
    }

    /// When the next heartbeat is due.
    fn presence_deadline(&self) -> Option<Instant> {
        self.report.deadline()
    }

    /// The bytes that adjust the key encoding modes to set on the outer terminal. Assumes the returned bytes were written and updates the applied state.
    ///
    /// Follows the attached session screen's modes, but uses the defaults when there's no session screen, during copy mode, status line input and the launcher where mine reads keys itself,
    /// and when exiting. In those cases keys must arrive in plain (legacy) encoding.
    fn sync_keys(&mut self) -> Vec<u8> {
        let reading = self.copy.is_some() || self.m.prompt.is_some() || self.m.launcher;
        let wanted = match &self.screen {
            Some(screen) if !reading && !self.quit => KeyModes::of(screen),
            _ => KeyModes::default(),
        };
        let out = mode_diff(self.keys, wanted);
        self.keys = wanted;
        out
    }

    /// If the write task has ended (the connection dropped), discard it. `rx` will report the end shortly.
    fn send(&self, msg: ToDaemon) {
        let _ = self.tx.send(msg);
    }

    fn say(&mut self, message: &str) {
        self.m.message = message.to_string();
        self.dirty = true;
    }

    /// The terminal pane size. `Attach` and `Resize` send this size.
    fn term_size(&self) -> (u16, u16) {
        let term = ui::layout(self.area, self.m.list_open).term;
        (term.width.max(1), term.height.max(1))
    }

    fn on_msg(&mut self, msg: FromDaemon) {
        let attached = |id: &str| self.m.attached.as_deref() == Some(id);
        match msg {
            FromDaemon::Sessions { sessions } => self.on_sessions(sessions),
            FromDaemon::Snapshot { id, data } if attached(&id) => {
                // The new screen replaces the frozen lines. The output that was kept is already in the snapshot.
                self.copy = None;
                self.m.status = None;
                let (cols, rows) = self.term_size();
                let mut screen = Screen::new(cols, rows);
                screen.feed(&data);
                screen.take_replies();
                self.screen = Some(screen);
                self.select.clear();
                self.held = false;
                self.dirty = true;
            }
            FromDaemon::Output { id, data } if attached(&id) => {
                if let Some(copy) = &mut self.copy {
                    // So that line numbers don't shift, it isn't fed to the frozen screen and is kept instead.
                    copy.held.extend_from_slice(&data);
                    if copy.held.len() > MAX_HELD {
                        self.exit_copy();
                        self.say("Too much output, so copy mode was ended");
                    }
                } else if let Some(screen) = &mut self.screen {
                    screen.feed(&data);
                    screen.take_replies();
                    // The inversion is tied to visible cells, so if output pushes the cells it covers the wrong characters. While it's being cleared, it's read on release.
                    if !self.select.dragging() {
                        self.select.clear();
                    }
                    self.dirty = true;
                }
            }
            FromDaemon::Error { message } => {
                self.pending = None;
                self.say(&message);
            }
            // Sent after hello and after every reload: the launcher rows, chips and labels follow the last one.
            FromDaemon::Profiles { profiles } => {
                self.m.profiles = profiles;
                self.dirty = true;
            }
            // The version was checked at connect time, and the other sessions' screens are no longer viewed. The connection already
            // skips unknown messages.
            FromDaemon::Hello { .. }
            | FromDaemon::Snapshot { .. }
            | FromDaemon::Output { .. }
            | FromDaemon::Unknown => {}
        }
    }

    fn on_sessions(&mut self, sessions: Vec<SessionInfo>) {
        self.m.sessions = sessions;
        self.dirty = true;
        if let Some((project, name)) = &self.pending
            && let Some(s) = self
                .m
                .sessions
                .iter()
                .find(|s| s.meta.project == *project && s.meta.name == *name)
        {
            let id = s.meta.id.clone();
            self.pending = None;
            return self.attach(id);
        }
        let gone = self
            .m
            .attached
            .as_ref()
            .is_some_and(|id| !self.m.sessions.iter().any(|s| s.meta.id == *id));
        if !self.started || gone {
            self.started = true;
            self.attach_first();
        }
    }

    /// Attaches to the session on the first row of the current list. If there are no visible rows (the Idle group starts out collapsed), looks through collapsed groups too.
    /// Once attached, that group is drawn expanded (`Model::visible_groups`).
    fn attach_first(&mut self) {
        let groups = self.m.visible_groups();
        let first = view::order(&groups)
            .first()
            .copied()
            .or_else(|| groups.iter().find_map(|g| g.rows.first().copied()));
        match first {
            Some(i) => self.attach(self.m.sessions[i].meta.id.clone()),
            None => self.detach(),
        }
    }

    fn attach(&mut self, id: String) {
        self.esc_exit = None;
        let (cols, rows) = self.term_size();
        self.send(ToDaemon::Attach {
            id: id.clone(),
            cols,
            rows,
        });
        if self.m.attached.as_ref() != Some(&id) {
            self.screen = None;
        }
        self.exit_copy();
        self.m.attached = Some(id);
        self.m.launcher = false;
        self.select.clear();
        self.held = false;
        self.dirty = true;
    }

    /// Lets go of the attached session. The daemon has no message for dropping, so that session's output keeps arriving but is discarded.
    /// With no attached session, the launcher fills the terminal pane.
    fn detach(&mut self) {
        self.m.attached = None;
        self.m.launcher = true;
        self.screen = None;
        self.exit_copy();
        self.select.clear();
        self.held = false;
        self.dirty = true;
    }

    /// Chooses a project. If the attached session isn't in that project, switches to the project's first row session.
    fn set_filter(&mut self, filter: Option<PathBuf>) {
        self.m.filter = filter;
        self.dirty = true;
        let m = &self.m;
        let shown = m.attached.as_ref().is_some_and(|id| {
            m.sessions
                .iter()
                .any(|s| s.meta.id == *id && m.filter.as_ref().is_none_or(|p| s.meta.project == *p))
        });
        if !shown {
            self.attach_first();
        }
    }

    fn on_input(&mut self, bytes: &[u8]) {
        for action in self.parser.feed(bytes) {
            self.on_action(action);
        }
    }

    fn on_action(&mut self, action: Action) {
        if let Action::Focus(focused) = action {
            return self.on_focus(focused);
        }
        self.report.input(now_ms());
        if !matches!(action, Action::Mouse(_)) {
            // Pressing a key clears the selection inversion and the notice.
            if self.select.clear() || !self.m.message.is_empty() {
                self.m.message.clear();
                self.dirty = true;
            }
        }
        match action {
            Action::Pass(bytes) if self.m.prompt.is_some() => self.prompt_keys(&bytes),
            Action::Pass(bytes) if self.copy.is_some() => self.copy_keys(&bytes),
            Action::Pass(bytes) if self.m.launcher => self.launcher_keys(&bytes),
            Action::Pass(data) => {
                let data = self.after_esc_exit(data);
                if !data.is_empty() {
                    self.input(data);
                }
            }
            Action::Paste(text) if self.m.prompt.is_some() => self.prompt_paste(&text),
            // In copy mode and the launcher, input doesn't go to the session.
            Action::Paste(_) if self.copy.is_some() || self.m.launcher => {}
            Action::Paste(text) => {
                let bracketed = self
                    .screen
                    .as_ref()
                    .is_some_and(|s| s.mode().contains(TermMode::BRACKETED_PASTE));
                let data = if bracketed {
                    [b"\x1b[200~".as_slice(), &text, b"\x1b[201~"].concat()
                } else {
                    text
                };
                self.input(data);
            }
            Action::Hangul => self.hangul_hint(),
            Action::Cmd(cmd) => {
                if self.m.prompt.take().is_some() {
                    self.dirty = true;
                }
                self.command(cmd);
            }
            Action::Mouse(ev) => self.mouse(ev),
            Action::Focus(_) => {} // Handled above.
        }
    }

    /// The outer terminal's focus report. Focusing counts as input, as `Focus` does in the daemon. A session that turned focus
    /// reporting on itself (mode 1004) gets the report too.
    fn on_focus(&mut self, focused: bool) {
        self.visible = focused;
        if focused {
            self.report.input(now_ms());
        }
        let wants = self
            .screen
            .as_ref()
            .is_some_and(|s| s.mode().contains(TermMode::FOCUS_IN_OUT));
        if wants {
            self.forward(if focused { b"\x1b[I" } else { b"\x1b[O" }.to_vec());
        }
    }

    /// If copy mode was just left with Esc (within `ESC_GRACE`), drops a lone Esc in front of the input. If input remains after dropping, the window closes.
    fn after_esc_exit(&mut self, data: Vec<u8>) -> Vec<u8> {
        let Some(at) = self.esc_exit else {
            return data;
        };
        if at.elapsed() >= ESC_GRACE {
            self.esc_exit = None;
            return data;
        }
        let rest = copymode::drop_lone_esc(&data);
        if !rest.is_empty() {
            self.esc_exit = None;
        }
        rest.to_vec()
    }

    /// Passes key input and paste to the session. A scrolled screen is returned to the bottom before passing.
    fn input(&mut self, data: Vec<u8>) {
        if let Some(screen) = &mut self.screen {
            let scrolled = screen.term().grid().display_offset();
            if scrolled > 0 {
                screen.scroll(-(scrolled as i32));
                self.dirty = true;
            }
        }
        self.forward(data);
    }

    fn forward(&self, data: Vec<u8>) {
        if let Some(id) = &self.m.attached {
            self.send(ToDaemon::Input {
                id: id.clone(),
                data,
            });
        }
    }

    fn command(&mut self, cmd: Cmd) {
        self.esc_exit = None;
        // Running another command inside copy mode ends copy mode and then runs it.
        if cmd != Cmd::CopyMode {
            self.exit_copy();
        }
        match cmd {
            Cmd::NextSession => self.step_session(1),
            Cmd::PrevSession => self.step_session(-1),
            Cmd::NextProject => self.step_project(1),
            Cmd::PrevProject => self.step_project(-1),
            Cmd::AllProjects => self.set_filter(None),
            Cmd::NewSession => {
                self.m.launcher = true;
                self.dirty = true;
            }
            Cmd::KillSession => match self.m.attached.clone() {
                Some(id) => {
                    self.m.prompt = Some(Prompt::ConfirmKill(id));
                    self.dirty = true;
                }
                None => self.say("No session to close"),
            },
            Cmd::ToggleList => {
                self.m.list_open = !self.m.list_open;
                self.resize_session();
            }
            Cmd::CopyMode => self.enter_copy(),
            Cmd::Detach => self.quit = true,
        }
    }

    /// Attaches to the next or previous session in the order of rows visible in the list. At the ends it wraps around to the start.
    fn step_session(&mut self, step: isize) {
        let order = view::order(&self.m.visible_groups());
        let Some(n) = isize::try_from(order.len()).ok().filter(|&n| n > 0) else {
            return;
        };
        let here = self.m.attached.as_ref().and_then(|id| {
            order
                .iter()
                .position(|&i| self.m.sessions[i].meta.id == *id)
        });
        let next = match here {
            Some(at) => (at as isize + step).rem_euclid(n),
            None if step > 0 => 0,
            None => n - 1,
        };
        self.attach(self.m.sessions[order[next as usize]].meta.id.clone());
    }

    /// Chooses the next or previous project in project rail order (including ∗).
    fn step_project(&mut self, step: isize) {
        let rail = view::rail(&self.m.sessions, &self.m.current_project);
        let at = rail
            .iter()
            .position(|item| item.project == self.m.filter)
            .unwrap_or(0);
        let next = (at as isize + step).rem_euclid(rail.len() as isize) as usize;
        self.set_filter(rail[next].project.clone());
    }

    fn mouse(&mut self, ev: MouseEv) {
        if self.m.prompt.is_some() {
            return;
        }
        let areas = ui::layout(self.area, self.m.list_open);
        // If the selection is being cleared, dragging or releasing outside the terminal pane is clamped into the pane and continues.
        if self.select.dragging() && matches!(ev.kind, MouseKind::Drag | MouseKind::Up) {
            let at = select::local(areas.term, ev.col, ev.row);
            return self.select_event(ev, at);
        }
        // If a press passed to the session is still held, dragging or releasing outside the pane is clamped into the pane and passed on. So the app doesn't miss the release.
        if self.held && matches!(ev.kind, MouseKind::Drag | MouseKind::Up) {
            let at = select::local(areas.term, ev.col, ev.row);
            return self.term_mouse(ev, at);
        }
        if ev.kind == MouseKind::Down && self.select.clear() {
            self.dirty = true;
        }
        let hit = ui::hit(&areas, &self.m, ev.col, ev.row);
        if let Hit::Term(x, y) = hit {
            // In copy mode, the terminal pane's mouse is neither passed to the session nor used for selection. Only the wheel moves the cursor.
            if self.copy.is_some() {
                match ev.kind {
                    MouseKind::WheelUp => self.copy_wheel(-WHEEL_LINES),
                    MouseKind::WheelDown => self.copy_wheel(WHEEL_LINES),
                    _ => {}
                }
                return;
            }
            return self.term_mouse(ev, (x, y));
        }
        if ev.kind != MouseKind::Down || ev.button != 0 {
            return;
        }
        match hit {
            Hit::Rail(i) => {
                let rail = view::rail(&self.m.sessions, &self.m.current_project);
                if let Some(item) = rail.get(i) {
                    self.set_filter(item.project.clone());
                }
            }
            Hit::Group(i) => {
                self.m.toggle_group(i);
                self.dirty = true;
            }
            Hit::Row(i) => self.attach(self.m.sessions[i].meta.id.clone()),
            Hit::Launch(choice) => self.launch(choice),
            Hit::Term(..) | Hit::None => {}
        }
    }

    /// The terminal pane's mouse (`at` is relative to the pane). If the session uses the mouse, passes it on as is; otherwise the wheel scrolls and
    /// left-button drag selects. While a press inside the pane is held, drags and releases from outside the pane are clamped and arrive here too.
    fn term_mouse(&mut self, ev: MouseEv, at: (u16, u16)) {
        let Some(screen) = &mut self.screen else {
            return;
        };
        if wants_mouse(screen) {
            if session_takes(screen, &ev) {
                match ev.kind {
                    MouseKind::Down => self.held = true,
                    MouseKind::Up => self.held = false,
                    _ => {}
                }
                self.forward(encode_sgr(&ev, at.0, at.1));
            }
            return;
        }
        // If the session turned the mouse off, a press that was held is considered ended too.
        self.held = false;
        let step = match ev.kind {
            MouseKind::WheelUp => WHEEL_LINES,
            MouseKind::WheelDown => -WHEEL_LINES,
            _ => return self.select_event(ev, at),
        };
        // Scrolls only the client's copy of the screen. Nothing is sent to the session.
        let before = screen.term().grid().display_offset();
        screen.scroll(step);
        if screen.term().grid().display_offset() != before {
            self.select.clear();
            self.dirty = true;
        }
    }

    /// Advances the selection with left-button press, drag and release. If there is a selection on release, copies it.
    fn select_event(&mut self, ev: MouseEv, at: (u16, u16)) {
        if ev.button != 0 {
            return;
        }
        match ev.kind {
            MouseKind::Down => self.select.down(at),
            MouseKind::Drag => self.select.drag(at),
            MouseKind::Up => {
                let sel = self.select.up(at);
                let text = sel
                    .zip(self.screen.as_ref())
                    .map(|(sel, screen)| selected_text(screen, &sel));
                if let Some((osc, message)) = text.as_deref().and_then(select::copy) {
                    self.clipboard.extend(osc);
                    self.say(&message);
                }
            }
            MouseKind::WheelUp | MouseKind::WheelDown => {}
        }
        self.dirty = true;
    }

    /// `⌃a [`: freezes the current screen (scrollback included) and enters copy mode. If already in, leaves it as is.
    fn enter_copy(&mut self) {
        if self.copy.is_some() {
            return;
        }
        // If the launcher covers the session pane, there's no screen visible to copy.
        let Some(screen) = self.screen.as_ref().filter(|_| !self.m.launcher) else {
            return self.say("No session screen to copy");
        };
        // The page size for page keys (the number of lines in the terminal pane) is read from the screen by `freeze`. If the size changes, copy mode ends.
        let (lines, mode) = copymode::freeze(screen);
        self.copy = Some(Copying {
            mode,
            lines,
            held: Vec::new(),
        });
        self.select.clear();
        self.held = false;
        self.esc_exit = None;
        self.sync_copy();
    }

    /// Wheel in copy mode: moves the cursor by `step` lines (positive is down) and makes the screen follow.
    fn copy_wheel(&mut self, step: i32) {
        if let Some(copy) = &mut self.copy {
            copy.mode.move_lines(&copy.lines, step as isize);
        }
        self.sync_copy();
    }

    /// Feeds copy mode keys. When an ending key (`q`, `y`, Esc) arrives, the bytes after it aren't copy mode keys, so
    /// they go to the session through the usual input path as if they came in the next read. But if it ended with Esc, a lone Esc right after is dropped
    /// (so the habit of pressing Esc twice to get out doesn't send an Esc to the session).
    fn copy_keys(&mut self, bytes: &[u8]) {
        let Some(copy) = &mut self.copy else {
            return;
        };
        let mut out = CopyOut::Stay;
        let mut rest = &bytes[bytes.len()..];
        let mut by_esc = false;
        let mut hangul = false;
        for (key, end) in copymode::split(bytes) {
            out = match key {
                copymode::Key::Byte(b) => {
                    // When not typing a search query, this is the command key position. Hangul does nothing, so tell the user.
                    hangul |= !copy.mode.typing && input::hangul_at(&bytes[end - 1..]);
                    copy.mode.key(&copy.lines, b)
                }
                copymode::Key::Arrow(dir) => copy.mode.arrow(&copy.lines, dir),
                // While typing a search query, ⌃b and ⌃f are ignored, so they aren't mixed into the input like an arrow.
                copymode::Key::PageUp => copy.mode.key(&copy.lines, 0x02),
                copymode::Key::PageDown => copy.mode.key(&copy.lines, 0x06),
            };
            if out != CopyOut::Stay {
                rest = &bytes[end..];
                by_esc = key == copymode::Key::Byte(0x1b);
                self.esc_exit = by_esc.then(Instant::now);
                break;
            }
        }
        copy.mode.hangul |= hangul;
        match out {
            CopyOut::Stay => self.sync_copy(),
            CopyOut::Exit => self.exit_copy(),
            CopyOut::Copy(text) => {
                self.exit_copy();
                if let Some((osc, message)) = select::copy(&text) {
                    self.clipboard.extend(osc);
                    self.say(&message);
                }
            }
        }
        if by_esc {
            rest = copymode::drop_lone_esc(rest);
        }
        if !rest.is_empty() {
            // If characters continue, what followed Esc is over.
            self.esc_exit = None;
            self.input(rest.to_vec());
        }
    }

    /// Scrolls the screen so the cursor line is visible and replaces the status line with the copy mode text.
    fn sync_copy(&mut self) {
        if let (Some(copy), Some(screen)) = (&mut self.copy, &mut self.screen) {
            copy.mode.reveal(screen);
            self.m.status = Some(copy.mode.status(copy.lines.len()));
        }
        self.dirty = true;
    }

    /// Ends copy mode. Feeds the kept output to the screen and returns to the bottom.
    fn exit_copy(&mut self) {
        let Some(copy) = self.copy.take() else {
            return;
        };
        self.m.status = None;
        if let Some(screen) = &mut self.screen {
            screen.feed(&copy.held);
            screen.take_replies();
            let scrolled = screen.term().grid().display_offset();
            screen.scroll(-(scrolled as i32));
        }
        self.dirty = true;
    }

    /// Feeds a key to the status line being typed into. A `⌃a x` confirmation closes only on `y`; any other key cancels.
    fn prompt_keys(&mut self, bytes: &[u8]) {
        self.dirty = true;
        match self.m.prompt.take() {
            Some(Prompt::ConfirmKill(id)) => {
                if bytes == b"y" || bytes == b"Y" {
                    self.send(ToDaemon::Kill { id });
                }
            }
            Some(Prompt::New(mut text)) => {
                // A lone Esc cancels; other escape sequences such as arrows are ignored.
                if bytes.contains(&0x1b) {
                    if bytes != b"\x1b" {
                        self.m.prompt = Some(Prompt::New(text));
                    }
                    return;
                }
                for c in String::from_utf8_lossy(bytes).chars() {
                    match c {
                        '\r' | '\n' => return self.submit_new(&text),
                        '\x7f' | '\x08' => {
                            text.pop();
                        }
                        '\x03' => return,
                        '\x15' => text.clear(),
                        c if c.is_control() => {}
                        c => text.push(c),
                    }
                }
                self.m.prompt = Some(Prompt::New(text));
            }
            None => {}
        }
    }

    fn prompt_paste(&mut self, text: &[u8]) {
        self.dirty = true;
        match &mut self.m.prompt {
            Some(Prompt::New(input)) => input.extend(
                String::from_utf8_lossy(text)
                    .chars()
                    .filter(|c| !c.is_control()),
            ),
            _ => self.m.prompt = None,
        }
    }

    /// Interprets the input of `n` in the launcher and sends `New`. When the new session appears in the list, it attaches (`on_sessions`).
    /// A profile that isn't logged in is refused like a row's key.
    fn submit_new(&mut self, text: &str) {
        match view::parse_new(text, self.m.project(), &self.m.profiles) {
            Ok(spec) => match launch::refusal(spec.account.as_deref(), &self.m.profiles) {
                Some(refusal) => self.say(&refusal),
                None => self.create(spec),
            },
            Err(e) => self.say(&format!("{e:#}")),
        }
    }

    fn create(&mut self, spec: NewSession) {
        self.pending = Some((spec.project.clone(), spec.name.clone()));
        self.send(ToDaemon::New { spec });
    }

    /// Keys of the launcher. Esc closes a launcher opened over a session (with no attached session there's nowhere to return to, so it does nothing).
    /// Accepts only keys that choose a row; the rest are discarded without sending them to the session. `⌃a` commands are taken first by the input parser.
    fn launcher_keys(&mut self, bytes: &[u8]) {
        if bytes == b"\x1b" {
            if self.m.attached.is_some() {
                self.m.launcher = false;
                self.dirty = true;
            }
        } else if let Some(choice) = launcher::choice_of_key(bytes, &self.m.profiles) {
            self.launch(choice);
        } else if String::from_utf8_lossy(bytes).chars().any(input::is_hangul) {
            self.hangul_hint();
        }
    }

    /// Hangul arrived where a command key goes (launcher, after the prefix, copy mode). Does nothing and tells the user to switch to English.
    /// Copy mode uses the whole status line for that text, so it's layered on there.
    fn hangul_hint(&mut self) {
        if let Some(copy) = &mut self.copy {
            copy.mode.hangul = true;
            self.sync_copy();
        } else {
            self.say(input::HANGUL_HINT);
        }
    }

    /// Starts the chosen row. The session is created right away with a new name (`t<N>`), and attaching happens after it appears in the list (`on_sessions`).
    /// A profile that isn't logged in doesn't start; the status line says how to log in.
    fn launch(&mut self, choice: Choice) {
        if choice == Choice::Typed {
            self.m.prompt = Some(Prompt::New(String::new()));
            self.dirty = true;
            return;
        }
        if let Some(refusal) = launch::refusal(choice.account(), &self.m.profiles) {
            return self.say(&refusal);
        }
        // While waiting for the answer to a `New` sent earlier, don't accept another. Keeps sending the same name again from tangling.
        if self.pending.is_some() {
            return;
        }
        let project = self.m.project().to_path_buf();
        let name = launch::next_name(&self.m.sessions, &project);
        if let Some(spec) = launch::new_session(&choice, &name, &project, &self.m.profiles) {
            self.create(spec);
        }
    }

    fn on_resize(&mut self, size: Size) {
        self.area = Rect::from((Position::ORIGIN, size));
        self.resize_session();
    }

    /// The terminal pane size changed (window size, list collapse). Fits the screen and the session PTY.
    fn resize_session(&mut self) {
        // If the size changes, line wrapping changes and the frozen lines and the screen get out of step.
        self.exit_copy();
        let (cols, rows) = self.term_size();
        if let Some(screen) = &mut self.screen {
            screen.resize(cols, rows);
        }
        self.select.clear();
        if let Some(id) = &self.m.attached {
            self.send(ToDaemon::Resize {
                id: id.clone(),
                cols,
                rows,
            });
        }
        self.dirty = true;
    }

    fn sync_deadline(&self) -> Option<Instant> {
        // During copy mode the screen is frozen. Keeps an unfinished synchronized update from changing lines.
        if self.copy.is_some() {
            return None;
        }
        self.screen.as_ref()?.sync_deadline()
    }

    /// If a synchronized update (`?2026`) the session never ended has exceeded its time, ends it and redraws. Keeps the pane from staying frozen.
    fn end_stale_sync(&mut self) {
        if self.copy.is_some() {
            return;
        }
        if let Some(screen) = &mut self.screen
            && screen.sync_deadline().is_some_and(|t| t <= Instant::now())
        {
            screen.end_sync();
            screen.take_replies();
            self.dirty = true;
        }
    }

    /// Draws one screen. Wrapped so the outer terminal shows it at once as a synchronized update.
    ///
    /// ponytail: ratatui doesn't know OSC 8 hyperlinks. So after ratatui draws, this frame's link spans are written once more
    /// inside `ESC ] 8 ; ; URL` using the cells of the buffer just drawn (same characters and colors). The terminal remembers the link per
    /// cell, so cells ratatui didn't rewrite keep the link too. Spans that were only in the previous screen are rewritten without a link,
    /// so a cell that ratatui skipped because its characters were the same doesn't keep the old link. The cursor is restored with DECSC/DECRC.
    /// If many links make it slow, write only the changed spans.
    fn draw<W: Write>(&mut self, terminal: &mut Terminal<CrosstermBackend<W>>) -> io::Result<()> {
        // The clipboard OSC 52 goes over the same write path as the screen, between frames rather than mid-draw.
        if !self.clipboard.is_empty() {
            terminal.backend_mut().write_all(&self.clipboard)?;
            self.clipboard.clear();
        }
        queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let frame = terminal.draw(|f| {
            ui::draw(f, &self.m, self.screen.as_ref());
            let areas = ui::layout(f.area(), self.m.list_open);
            if let (Some(copy), Some(screen)) = (&self.copy, &self.screen) {
                // The copy mode cursor and selection are painted with the same inversion. While typing a search query, the cursor stays in the status line.
                if let Some(sel) = copy.mode.highlight(screen) {
                    select::paint_selection(f.buffer_mut(), areas.term, &sel);
                }
                if copy.mode.typing {
                    let x = 1 + Span::raw(copy.mode.input.as_str()).width() as u16;
                    f.set_cursor_position((
                        areas.status.x + x.min(areas.status.width.saturating_sub(1)),
                        areas.status.y,
                    ));
                }
            } else if let Some(sel) = self.select.selection() {
                select::paint_selection(f.buffer_mut(), areas.term, sel);
            }
        })?;
        let term = ui::layout(frame.area, self.m.list_open).term;
        let anchor = self
            .screen
            .as_ref()
            .and_then(|screen| ime_anchor(&self.m, screen, term));
        // While the launcher covers the session pane, the session screen's links aren't applied.
        let runs = self
            .screen
            .as_ref()
            .filter(|_| !self.m.launcher)
            .map(|screen| ui::link_runs(screen, term))
            .unwrap_or_default();
        let mut patches: Vec<(Option<&str>, Cells)> = self
            .links
            .iter()
            .filter(|run| !runs.contains(run))
            .map(|run| (None, run_cells(frame.buffer, run)))
            .collect();
        patches.extend(
            runs.iter()
                .map(|run| (Some(run.url.as_str()), run_cells(frame.buffer, run))),
        );
        let backend = terminal.backend_mut();
        if !patches.is_empty() {
            backend.write_all(b"\x1b7")?;
            for (url, cells) in &patches {
                if let Some(url) = url {
                    write!(backend, "\x1b]8;;{url}\x1b\\")?;
                }
                backend.draw(cells.iter().map(|(x, y, cell)| (*x, *y, cell)))?;
                if url.is_some() {
                    backend.write_all(b"\x1b]8;;\x1b\\")?;
                }
            }
            backend.write_all(b"\x1b8")?;
        }
        // Must be written after the DECRC (`\x1b8`) above. Written before it, DECRC would put the cursor back.
        if let Some(pos) = anchor {
            queue!(backend, MoveTo(pos.x, pos.y))?;
        }
        queue!(backend, EndSynchronizedUpdate)?;
        Write::flush(backend)?;
        self.links = runs;
        self.dirty = false;
        Ok(())
    }
}

/// Where to put the outer cursor when the session hid its cursor. `None` if there is none.
///
/// Hangul composition (IME preedit) shows at the outer terminal's real cursor position, even when it's hidden. Claude Code hides the cursor and puts it on its own input line,
/// so the outer cursor must be moved there while staying hidden for the composition to show on the input line. The visible cursor is `ui::term_cursor`'s job.
/// For status line input (prompts, copy mode search), the status line cursor takes priority, and while scrolled back in time or while the launcher covers the pane,
/// it isn't moved into the pane.
///
/// ponytail: The in-pane check is the same two lines as `ui::term_cursor`. It lives here rather than editing ui.rs, to avoid growing ui.rs.
fn ime_anchor(m: &Model, sc: &Screen, term: Rect) -> Option<Position> {
    if m.attached.is_none()
        || m.launcher
        || m.status.is_some()
        || m.prompt.is_some()
        || sc.mode().contains(TermMode::SHOW_CURSOR)
        || sc.term().grid().display_offset() != 0
    {
        return None;
    }
    let (x, y) = sc.cursor();
    (x < term.width && y < term.height).then(|| Position::new(term.x + x, term.y + y))
}

/// Takes the cells covered by a link span out of the buffer just drawn. Skips the next cell covered by a wide character.
fn run_cells(buf: &Buffer, run: &LinkRun) -> Cells {
    let end = run
        .x
        .saturating_add(Span::raw(run.text.as_str()).width() as u16);
    let mut cells = Vec::new();
    let mut x = run.x;
    while x < end {
        let Some(cell) = buf.cell((x, run.y)) else {
            break;
        };
        let width = Span::raw(cell.symbol()).width().max(1) as u16;
        cells.push((x, run.y, cell.clone()));
        x += width;
    }
    cells
}

#[cfg(test)]
mod tests {
    use ratatui::{TerminalOptions, Viewport};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;
    use mine_core::session::Kind;

    /// Outer terminal 120x40 with the list open, so the terminal pane is 86x39 at (34, 0). Attached to session `s1`, with `feed` already
    /// fed to the screen, and the built-in profiles ([`ui::builtin`]) received.
    fn app(feed: &[u8]) -> (App, UnboundedReceiver<ToDaemon>) {
        let (tx, rx) = unbounded_channel();
        let mut app = App::new(tx, PathBuf::from("/p"), Size::new(120, 40));
        app.m.profiles = ui::builtin();
        app.m.attached = Some("s1".into());
        let mut screen = Screen::new(86, 39);
        screen.feed(feed);
        app.screen = Some(screen);
        (app, rx)
    }

    fn lines(n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|i| format!("line{i}\r\n").into_bytes())
            .collect()
    }

    fn ev(kind: MouseKind, col: u16, row: u16) -> MouseEv {
        MouseEv {
            kind,
            button: 0,
            col,
            row,
        }
    }

    fn offset(app: &App) -> usize {
        app.screen.as_ref().unwrap().term().grid().display_offset()
    }

    fn sent(rx: &mut UnboundedReceiver<ToDaemon>) -> Vec<ToDaemon> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn input(data: &[u8]) -> ToDaemon {
        ToDaemon::Input {
            id: "s1".into(),
            data: data.to_vec(),
        }
    }

    /// Draws one frame and returns the bytes written to the outer terminal (120x40). Opens a fixed area so it doesn't query the size.
    fn frame(app: &mut App) -> String {
        let mut out = Vec::new();
        let viewport = Viewport::Fixed(Rect::new(0, 0, 120, 40));
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(&mut out),
            TerminalOptions { viewport },
        )
        .unwrap();
        app.draw(&mut terminal).unwrap();
        // Terminal's Drop writes the sequence that shows a hidden cursor. Skip Drop to see only what the frame wrote.
        std::mem::forget(terminal);
        String::from_utf8(out).unwrap()
    }

    /// Hides the session cursor (row 3, column 5). The terminal pane starts at (34, 0), so on the outer screen it's row 3, column 39 (CUP `4;40`).
    const HIDDEN_AT_3_5: &[u8] = b"\x1b[?25l\x1b[4;6H";
    const MOVE: &str = "\x1b[4;40H";
    const SHOW: &str = "\x1b[?25h";

    #[test]
    fn ime_anchor_follows_the_hidden_cursor_inside_the_pane() {
        let (app, _rx) = app(HIDDEN_AT_3_5);
        let sc = app.screen.as_ref().unwrap();
        let term = ui::layout(app.area, app.m.list_open).term;
        assert_eq!(ime_anchor(&app.m, sc, term), Some(Position::new(39, 3)));
        // If it's outside the pane (an area narrower than the pane), don't move it.
        assert_eq!(ime_anchor(&app.m, sc, Rect::new(34, 0, 5, 39)), None);
        // If there's no attached session, don't move it.
        let mut detached = app.m;
        detached.attached = None;
        assert_eq!(ime_anchor(&detached, sc, term), None);
    }

    #[test]
    fn ime_anchor_leaves_a_visible_cursor_to_the_existing_path() {
        let (app, _rx) = app(b"\x1b[4;6H");
        let term = ui::layout(app.area, app.m.list_open).term;
        assert_eq!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term), None);
    }

    #[test]
    fn ime_anchor_stays_out_of_scrollback_prompts_and_copy_mode() {
        let (mut app, _rx) = app(&[lines(100), HIDDEN_AT_3_5.to_vec()].concat());
        let term = ui::layout(app.area, app.m.list_open).term;
        assert!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term).is_some());
        app.mouse(ev(MouseKind::WheelUp, 40, 5));
        assert_eq!(offset(&app), 3);
        assert_eq!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term), None);
        // After returning to the bottom, if the launcher (`⌃a c`) opens, the session pane is covered, so don't move it.
        app.on_action(Action::Pass(b"a".to_vec()));
        assert!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term).is_some());
        app.on_input(b"\x01c");
        assert!(app.m.launcher);
        assert_eq!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term), None);
        // A prompt opened from there with `n` gives priority to the status line cursor.
        app.on_input(b"n");
        assert!(app.m.prompt.is_some());
        assert_eq!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term), None);
        app.on_input(b"\x1b");
        app.on_input(b"\x1b");
        assert!(app.m.prompt.is_none() && !app.m.launcher);
        assert!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term).is_some());
        // Copy mode uses the status line, so don't move it whether or not a search query is being typed.
        enter_copy(&mut app);
        app.on_input(b"/ab");
        assert_eq!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term), None);
    }

    #[test]
    fn hidden_cursor_is_parked_at_its_spot_after_the_frame_and_never_shown() {
        let (mut app, _rx) = app(HIDDEN_AT_3_5);
        let out = frame(&mut app);
        // The moving sequence is written at the end of the frame, right before closing the synchronized update.
        assert!(out.ends_with(&format!("{MOVE}\x1b[?2026l")), "{out:?}");
        assert!(!out.contains(SHOW), "{out:?}");
        assert!(out.contains("\x1b[?25l"));
    }

    #[test]
    fn visible_cursor_keeps_the_existing_set_cursor_path() {
        let (mut app, _rx) = app(b"\x1b[4;6H");
        let out = frame(&mut app);
        // ratatui shows the cursor and puts it there. The end of the frame is just those two and the synchronized-update close, and nothing else moves it.
        // (Only the order at the end is checked, regardless of what was drawn in the pane.)
        assert!(
            out.ends_with(&format!("{SHOW}{MOVE}\x1b[?2026l")),
            "{out:?}"
        );
    }

    #[test]
    fn park_move_comes_after_the_link_patch_restores_the_cursor() {
        // The first row has an OSC 8 link and the session cursor is hidden. The link patch restores the cursor with DECSC/DECRC.
        let feed = [
            b"\x1b]8;;https://x.example\x1b\\link\x1b]8;;\x1b\\".as_slice(),
            HIDDEN_AT_3_5,
        ]
        .concat();
        let (mut app, _rx) = app(&feed);
        let out = frame(&mut app);
        let (restore, park, end) = (
            out.rfind("\x1b8").expect("DECRC"),
            out.rfind(MOVE).expect("park move"),
            out.rfind("\x1b[?2026l").expect("sync end"),
        );
        assert!(restore < park && park < end, "{out:?}");
        assert!(out.contains("\x1b]8;;https://x.example\x1b\\"), "{out:?}");
    }

    #[test]
    fn scrollback_and_prompt_frames_do_not_park_the_cursor_in_the_pane() {
        let (mut scrolled, _rx) = app(&[lines(100), HIDDEN_AT_3_5.to_vec()].concat());
        scrolled.mouse(ev(MouseKind::WheelUp, 40, 5));
        let out = frame(&mut scrolled);
        assert!(out.ends_with("\x1b[?25l\x1b[?2026l"), "{out:?}");
        // Opening a prompt shows the cursor in the status line (the bottom row) and doesn't move it into the pane.
        let (mut prompting, _rx) = app(HIDDEN_AT_3_5);
        prompting.on_input(b"\x01cn");
        let out = frame(&mut prompting);
        assert!(out.contains(SHOW) && !out.contains(MOVE), "{out:?}");
        // While the launcher covers the session pane, the cursor is neither shown nor moved into the pane.
        let (mut launching, _rx) = app(HIDDEN_AT_3_5);
        launching.on_input(b"\x01c");
        let out = frame(&mut launching);
        assert!(!out.contains(SHOW) && !out.contains(MOVE), "{out:?}");
    }

    #[test]
    fn launcher_over_a_session_drops_its_hyperlinks() {
        let (mut app, _rx) = app(b"\x1b]8;;https://x.example\x1b\\link\x1b]8;;\x1b\\");
        assert!(frame(&mut app).contains("\x1b]8;;https://x.example\x1b\\"));
        app.on_input(b"\x01c");
        let out = frame(&mut app);
        // Links from the previous screen are rewritten without a link, and no new links are applied.
        assert!(!out.contains("https://x.example"), "{out:?}");
        assert!(app.links.is_empty());
    }

    #[test]
    fn wheel_scrolls_the_client_copy_and_sends_nothing() {
        let (mut app, mut rx) = app(&lines(100));
        app.mouse(ev(MouseKind::WheelUp, 40, 5));
        assert_eq!(offset(&app), 3);
        app.mouse(ev(MouseKind::WheelUp, 40, 5));
        assert_eq!(offset(&app), 6);
        app.mouse(ev(MouseKind::WheelDown, 40, 5));
        assert_eq!(offset(&app), 3);
        assert!(sent(&mut rx).is_empty());
        // A wheel outside the terminal pane doesn't scroll.
        app.mouse(ev(MouseKind::WheelUp, 10, 5));
        assert_eq!(offset(&app), 3);
    }

    #[test]
    fn mouse_goes_to_the_session_in_term_local_coords_when_it_asks() {
        let (mut app, mut rx) = app(&[lines(100), b"\x1b[?1000h\x1b[?1002h".to_vec()].concat());
        app.mouse(ev(MouseKind::Down, 40, 5));
        app.mouse(ev(MouseKind::Drag, 41, 5));
        app.mouse(ev(MouseKind::Up, 41, 5));
        app.mouse(ev(MouseKind::WheelUp, 40, 5));
        assert_eq!(
            sent(&mut rx),
            [
                input(b"\x1b[<0;7;6M"),
                input(b"\x1b[<32;8;6M"),
                input(b"\x1b[<0;8;6m"),
                input(b"\x1b[<64;7;6M"),
            ]
        );
        // The wheel was only passed on; the client screen doesn't scroll and no selection starts.
        assert_eq!(offset(&app), 0);
        assert!(app.select.selection().is_none());
        // Outside the terminal pane isn't passed on.
        app.mouse(ev(MouseKind::Down, 10, 5));
        app.mouse(ev(MouseKind::WheelUp, 2, 5));
        assert!(sent(&mut rx).is_empty());
    }

    #[test]
    fn held_forwarded_press_gets_clamped_drag_and_release_outside_the_pane() {
        let (mut app, mut rx) = app(b"\x1b[?1000h\x1b[?1002h");
        // The press is inside the pane. Dragging goes over the list, then over the status line, and is released there.
        app.mouse(ev(MouseKind::Down, 40, 5));
        app.mouse(ev(MouseKind::Drag, 10, 5));
        app.mouse(ev(MouseKind::Drag, 50, 39));
        app.mouse(ev(MouseKind::Up, 50, 39));
        assert_eq!(
            sent(&mut rx),
            [
                input(b"\x1b[<0;7;6M"),
                input(b"\x1b[<32;1;6M"),
                input(b"\x1b[<32;17;39M"),
                input(b"\x1b[<0;17;39m"),
            ]
        );
        // The release ends the press, so drags and releases outside the pane are no longer passed on.
        app.mouse(ev(MouseKind::Drag, 10, 5));
        app.mouse(ev(MouseKind::Up, 10, 5));
        assert!(sent(&mut rx).is_empty());
        // A press that started outside the pane isn't picked up.
        app.mouse(ev(MouseKind::Down, 10, 5));
        app.mouse(ev(MouseKind::Drag, 40, 5));
        app.mouse(ev(MouseKind::Up, 40, 5));
        assert_eq!(
            sent(&mut rx),
            [input(b"\x1b[<32;7;6M"), input(b"\x1b[<0;7;6m")]
        );
    }

    #[test]
    fn typing_or_pasting_while_scrolled_returns_to_the_bottom_first() {
        let (mut app, mut rx) = app(&lines(100));
        app.mouse(ev(MouseKind::WheelUp, 40, 5));
        assert_eq!(offset(&app), 3);
        app.on_action(Action::Pass(b"a".to_vec()));
        assert_eq!(offset(&app), 0);
        app.mouse(ev(MouseKind::WheelUp, 40, 5));
        app.on_action(Action::Paste(b"b".to_vec()));
        assert_eq!(offset(&app), 0);
        assert_eq!(sent(&mut rx), [input(b"a"), input(b"b")]);
    }

    #[test]
    fn drag_copies_with_osc52_and_the_highlight_stays_until_a_key() {
        let (mut app, _rx) = app(b"hello world");
        app.mouse(ev(MouseKind::Down, 34, 0));
        app.mouse(ev(MouseKind::Drag, 36, 0));
        assert!(app.clipboard.is_empty());
        app.mouse(ev(MouseKind::Up, 38, 0));
        assert_eq!(app.clipboard, select::osc52("hello"));
        assert_eq!(app.m.message, "Copied 5 chars");
        assert!(app.select.selection().is_some());
        // The copy notice isn't cleared by the mouse; pressing a key removes the notice and the inversion together.
        app.mouse(ev(MouseKind::WheelDown, 40, 5));
        assert_eq!(app.m.message, "Copied 5 chars");
        app.on_action(Action::Pass(b"x".to_vec()));
        assert!(app.select.selection().is_none());
        assert!(app.m.message.is_empty());
    }

    #[test]
    fn plain_click_copies_nothing_and_clears_the_highlight() {
        let (mut app, _rx) = app(b"hello world");
        app.mouse(ev(MouseKind::Down, 34, 0));
        app.mouse(ev(MouseKind::Drag, 38, 0));
        app.mouse(ev(MouseKind::Up, 38, 0));
        app.clipboard.clear();
        app.mouse(ev(MouseKind::Down, 40, 0));
        app.mouse(ev(MouseKind::Up, 40, 0));
        assert!(app.clipboard.is_empty());
        assert!(app.select.selection().is_none());
    }

    #[test]
    fn dragging_outside_the_pane_clamps_into_it() {
        let (mut app, _rx) = app(b"hello world\r\nsecond");
        app.mouse(ev(MouseKind::Down, 34 + 6, 0));
        // Even going off over the list, the selection continues and is pressed at the left edge of the terminal pane.
        app.mouse(ev(MouseKind::Drag, 10, 1));
        app.mouse(ev(MouseKind::Up, 10, 1));
        assert_eq!(app.clipboard, select::osc52("world\ns"));
    }

    #[test]
    fn selection_goes_away_with_scroll_output_and_session_switch() {
        let (mut app, _rx) = app(&lines(100));
        let select = |app: &mut App| {
            app.mouse(ev(MouseKind::Down, 34, 0));
            app.mouse(ev(MouseKind::Drag, 38, 0));
            app.mouse(ev(MouseKind::Up, 38, 0));
            assert!(app.select.selection().is_some());
        };
        select(&mut app);
        app.mouse(ev(MouseKind::WheelUp, 40, 5));
        assert!(app.select.selection().is_none());
        select(&mut app);
        app.on_msg(FromDaemon::Output {
            id: "s1".into(),
            data: b"more".to_vec(),
        });
        assert!(app.select.selection().is_none());
        select(&mut app);
        app.attach("s2".into());
        assert!(app.select.selection().is_none());
        assert!(app.screen.is_none());
        // Output that arrives while it's being cleared doesn't break the selection.
        app.m.attached = Some("s1".into());
        app.screen = Some(Screen::new(86, 39));
        app.mouse(ev(MouseKind::Down, 34, 0));
        app.mouse(ev(MouseKind::Drag, 38, 0));
        app.on_msg(FromDaemon::Output {
            id: "s1".into(),
            data: b"more".to_vec(),
        });
        assert!(app.select.selection().is_some());
    }

    /// Enters copy mode with `⌃a [`.
    fn enter_copy(app: &mut App) {
        app.on_input(b"\x01[");
        assert!(app.copy.is_some());
    }

    fn history(app: &App) -> Vec<String> {
        app.screen.as_ref().unwrap().history_text()
    }

    #[test]
    fn copy_mode_swallows_keys_and_only_exit_sends_them_on() {
        let (mut app, mut rx) = app(&lines(100));
        enter_copy(&mut app);
        // l and s are copy mode keys, and Enter with no selection ends it. None of them goes to the session.
        app.on_input(b"ls\r");
        assert!(app.copy.is_none());
        assert!(sent(&mut rx).is_empty());
        app.on_input(b"x");
        assert_eq!(sent(&mut rx), [input(b"x")]);
        // Arrows and paste don't go either.
        enter_copy(&mut app);
        app.on_input(b"\x1b[A\x1bOB");
        app.on_action(Action::Paste(b"pasted".to_vec()));
        assert!(app.copy.is_some());
        assert!(sent(&mut rx).is_empty());
        // A lone Esc ends it.
        app.on_input(b"\x1b");
        assert!(app.copy.is_none());
    }

    #[test]
    fn clicks_and_drags_in_the_term_pane_are_ignored_in_copy_mode() {
        let (mut app, mut rx) = app(&[lines(100), b"\x1b[?1000h\x1b[?1002h".to_vec()].concat());
        enter_copy(&mut app);
        let before = (offset(&app), copy_line(&app));
        app.mouse(ev(MouseKind::Down, 40, 5));
        app.mouse(ev(MouseKind::Drag, 45, 5));
        app.mouse(ev(MouseKind::Up, 45, 5));
        assert_eq!((offset(&app), copy_line(&app)), before);
        assert!(app.select.selection().is_none() && app.clipboard.is_empty());
        assert!(sent(&mut rx).is_empty());
        assert!(app.copy.is_some());
    }

    fn copy_line(app: &App) -> usize {
        app.copy.as_ref().unwrap().mode.line
    }

    #[test]
    fn wheel_in_copy_mode_moves_the_cursor_three_lines_and_the_view_with_it() {
        // Even if the session uses the mouse, the screen is frozen so nothing goes to the session.
        let (mut app, mut rx) = app(&[lines(100), b"\x1b[?1000h\x1b[?1002h".to_vec()].concat());
        enter_copy(&mut app);
        assert_eq!((copy_line(&app), offset(&app)), (99, 0));
        app.mouse(ev(MouseKind::WheelUp, 40, 5));
        assert_eq!((copy_line(&app), offset(&app)), (96, 3));
        app.mouse(ev(MouseKind::WheelUp, 40, 5));
        assert_eq!((copy_line(&app), offset(&app)), (93, 6));
        app.mouse(ev(MouseKind::WheelDown, 40, 5));
        assert_eq!((copy_line(&app), offset(&app)), (96, 3));
        assert!(app.m.status.as_deref().unwrap().contains("│ 97/101 │"));
        // A wheel outside the terminal pane doesn't move the cursor.
        app.mouse(ev(MouseKind::WheelUp, 10, 5));
        assert_eq!(copy_line(&app), 96);
        assert!(app.copy.is_some() && sent(&mut rx).is_empty());
        // Moving with the wheel clears "No matches".
        app.on_input(b"/nothing\r");
        assert!(app.m.status.as_deref().unwrap().contains("No matches"));
        app.mouse(ev(MouseKind::WheelDown, 40, 5));
        assert!(!app.m.status.as_deref().unwrap().contains("No matches"));
    }

    #[test]
    fn bytes_after_the_key_that_ends_copy_mode_go_to_the_session() {
        let (mut app, mut rx) = app(&lines(100));
        enter_copy(&mut app);
        // The `ls\r` after q is session input, not copy mode keys.
        app.on_input(b"qls\r");
        assert!(app.copy.is_none());
        assert_eq!(sent(&mut rx), [input(b"ls\r")]);
        // The same for y, which copies and ends.
        enter_copy(&mut app);
        app.on_input(b"gvlyecho\r");
        assert!(app.copy.is_none());
        assert_eq!(app.clipboard, select::osc52("li"));
        assert_eq!(sent(&mut rx), [input(b"echo\r")]);
        // Enter with no selection is an ending key too.
        enter_copy(&mut app);
        app.on_input(b"\rx");
        assert_eq!(sent(&mut rx), [input(b"x")]);
        // An Esc after ending with q goes to the session as is.
        enter_copy(&mut app);
        app.on_input(b"q\x1b");
        assert!(app.copy.is_none());
        assert_eq!(sent(&mut rx), [input(b"\x1b")]);
    }

    #[test]
    fn esc_that_ends_copy_mode_drops_the_bare_escs_right_after_it() {
        // Many people press Esc twice quickly to get out. If an Esc reached the session (Claude Code), it would interrupt the turn, and Esc Esc opens rewind.
        let (mut app, mut rx) = app(&lines(100));
        enter_copy(&mut app);
        app.on_input(b"\x1b\x1b");
        assert!(app.copy.is_none());
        assert!(sent(&mut rx).is_empty());
        // Input that follows goes through as usual.
        enter_copy(&mut app);
        app.on_input(b"\x1b\x1bls\r");
        assert!(app.copy.is_none());
        assert_eq!(sent(&mut rx), [input(b"ls\r")]);
        // Even if it comes three times, all are dropped.
        enter_copy(&mut app);
        app.on_input(b"\x1b\x1b\x1bx");
        assert_eq!(sent(&mut rx), [input(b"x")]);
        // The start of an escape sequence (CSI) isn't a lone Esc, so it's kept.
        enter_copy(&mut app);
        app.on_input(b"\x1b\x1b\x1b[A");
        assert_eq!(sent(&mut rx), [input(b"\x1b[A")]);
        // Two Escs while typing a search query: the first folds up the input and the second ends. Nothing is sent.
        enter_copy(&mut app);
        app.on_input(b"/ab\x1b\x1b");
        assert!(app.copy.is_none());
        assert!(sent(&mut rx).is_empty());
        // After ending with q, y or Enter, an Esc goes through as is.
        enter_copy(&mut app);
        app.on_input(b"q\x1b\x1b");
        assert_eq!(sent(&mut rx), [input(b"\x1b\x1b")]);
        enter_copy(&mut app);
        app.on_input(b"\r\x1b");
        assert_eq!(sent(&mut rx), [input(b"\x1b")]);
    }

    #[test]
    fn a_second_esc_read_separately_is_dropped_within_the_grace_window() {
        let (mut app, mut rx) = app(&lines(100));
        enter_copy(&mut app);
        // The first Esc ends copy mode and records the time.
        app.on_input(b"\x1b");
        assert!(app.copy.is_none() && app.esc_exit.is_some());
        assert!(sent(&mut rx).is_empty());
        // The second Esc doesn't go to the session even if read separately, because it's within the window. The same if they arrive back to back.
        app.esc_exit = Some(Instant::now());
        app.on_input(b"\x1b");
        app.on_input(b"\x1b");
        assert!(sent(&mut rx).is_empty());
        // If a character follows Esc, the character goes through, and the window is closed after that.
        app.on_input(b"\x1bx");
        assert_eq!(sent(&mut rx), [input(b"x")]);
        assert!(app.esc_exit.is_none());
        app.on_input(b"\x1b");
        assert_eq!(sent(&mut rx), [input(b"\x1b")]);
    }

    #[test]
    fn esc_after_the_grace_window_or_after_q_goes_to_the_session() {
        let (mut app, mut rx) = app(&lines(100));
        enter_copy(&mut app);
        app.on_input(b"\x1b");
        // The window has passed (simulated by setting the time back).
        app.esc_exit = Some(Instant::now() - 2 * ESC_GRACE);
        app.on_input(b"\x1b");
        assert_eq!(sent(&mut rx), [input(b"\x1b")]);
        assert!(app.esc_exit.is_none());
        // After ending with q, y or Enter, there is no window.
        enter_copy(&mut app);
        app.on_input(b"q");
        assert!(app.esc_exit.is_none());
        app.on_input(b"\x1b");
        assert_eq!(sent(&mut rx), [input(b"\x1b")]);
        // Entering copy mode again closes the window.
        enter_copy(&mut app);
        app.on_input(b"\x1b");
        assert!(app.esc_exit.is_some());
        enter_copy(&mut app);
        assert!(app.esc_exit.is_none());
    }

    #[test]
    fn kitty_encoded_esc_is_dropped_within_the_grace_window_too() {
        let (mut app, mut rx) = app(&lines(100));
        enter_copy(&mut app);
        app.on_input(b"\x1b");
        // If the session turns on the kitty keyboard, the second Esc arrives as `CSI 27 u`. All three forms of an Esc press without modifiers are dropped.
        for esc in [&b"\x1b[27u"[..], b"\x1b[27;1u", b"\x1b[27;1:1u"] {
            app.on_input(esc);
        }
        assert!(sent(&mut rx).is_empty());
        // One with a modifier (⌃Esc) is kept.
        app.on_input(b"\x1b[27;5u");
        assert_eq!(sent(&mut rx), [input(b"\x1b[27;5u")]);
        assert!(app.esc_exit.is_none());
        // After the window has passed, it goes through as is.
        enter_copy(&mut app);
        app.on_input(b"\x1b");
        app.esc_exit = Some(Instant::now() - 2 * ESC_GRACE);
        app.on_input(b"\x1b[27u");
        assert_eq!(sent(&mut rx), [input(b"\x1b[27u")]);
    }

    #[test]
    fn text_forwarded_after_the_exit_esc_closes_the_grace_window() {
        let (mut app, mut rx) = app(&lines(100));
        enter_copy(&mut app);
        app.on_input(b"\x1b\x1bx");
        assert_eq!(sent(&mut rx), [input(b"x")]);
        assert!(app.esc_exit.is_none());
        app.on_input(b"\x1b");
        assert_eq!(sent(&mut rx), [input(b"\x1b")]);
    }

    #[test]
    fn commands_and_attach_close_the_grace_window() {
        let (mut app, mut rx) = app(&lines(100));
        enter_copy(&mut app);
        app.on_input(b"\x1b");
        assert!(app.esc_exit.is_some());
        // `⌃a n` is a command. With no sessions at all it doesn't move anywhere, but the window closes.
        app.on_input(b"\x01n");
        assert!(app.esc_exit.is_none());
        app.on_input(b"\x1b");
        assert_eq!(sent(&mut rx), [input(b"\x1b")]);
        // It closes when attached to a session too.
        enter_copy(&mut app);
        app.on_input(b"\x1b");
        assert!(app.esc_exit.is_some());
        app.attach("s1".into());
        assert!(app.esc_exit.is_none());
    }

    #[test]
    fn paging_moves_the_cursor_and_the_view_by_a_page() {
        // A 39-line screen with 62 lines of scrollback. A page is 39 lines.
        let (mut app, _rx) = app(&lines(100));
        enter_copy(&mut app);
        let at = |app: &App| (copy_line(app), offset(app));
        assert_eq!(at(&app), (99, 0));
        app.on_input(b"\x1b[5~");
        assert_eq!(at(&app), (60, 39));
        app.on_input(b"\x1b[6~");
        assert_eq!(at(&app), (99, 0));
        // A half page is 19 lines.
        app.on_input(b"\x15");
        assert_eq!(at(&app), (80, 19));
        app.on_input(b"\x04");
        assert_eq!(at(&app), (99, 0));
        // It stops at the ends. ⌃b and ⌃f are the same page movement.
        app.on_input(b"\x1b[6~\x06");
        assert_eq!(at(&app), (100, 0));
        app.on_input(b"\x02\x02\x02\x02");
        assert_eq!(at(&app), (0, 62));
        app.on_input(b"\x1b[5~");
        assert_eq!(at(&app), (0, 62));
        assert!(app.m.status.as_deref().unwrap().contains("│ 1/101 │"));
    }

    #[test]
    fn output_during_copy_mode_is_held_and_fed_on_exit() {
        let (mut app, _rx) = app(b"hello");
        enter_copy(&mut app);
        let frozen = history(&app);
        app.on_msg(FromDaemon::Output {
            id: "s1".into(),
            data: b"\r\nlater".to_vec(),
        });
        // So line numbers don't shift, the screen stays as is and output is kept.
        assert_eq!(history(&app), frozen);
        assert_eq!(app.copy.as_ref().unwrap().held, b"\r\nlater");
        // Output from other sessions isn't kept.
        app.on_msg(FromDaemon::Output {
            id: "s2".into(),
            data: b"other".to_vec(),
        });
        assert_eq!(app.copy.as_ref().unwrap().held, b"\r\nlater");
        app.on_input(b"q");
        assert_eq!(app.screen.as_ref().unwrap().lines()[1], "later");
        assert_eq!(offset(&app), 0);
        assert!(app.copy.is_none() && app.m.status.is_none());
        // Output after leaving goes straight to the screen.
        app.on_msg(FromDaemon::Output {
            id: "s1".into(),
            data: b"!".to_vec(),
        });
        assert_eq!(app.screen.as_ref().unwrap().lines()[1], "later!");
    }

    #[test]
    fn yank_writes_osc52_through_the_clipboard_path_and_says_how_much() {
        let (mut app, mut rx) = app(b"alpha\r\nbeta");
        enter_copy(&mut app);
        app.on_input(b"gvjl");
        assert!(app.clipboard.is_empty());
        app.on_input(b"y");
        assert_eq!(app.clipboard, select::osc52("alpha\nbe"));
        assert_eq!(app.m.message, "Copied 8 chars");
        assert!(app.copy.is_none() && app.m.status.is_none());
        assert!(sent(&mut rx).is_empty());
        // y with no selection ends without copying.
        app.clipboard.clear();
        app.m.message.clear();
        enter_copy(&mut app);
        app.on_input(b"y");
        assert!(app.copy.is_none() && app.clipboard.is_empty() && app.m.message.is_empty());
    }

    #[test]
    fn status_and_view_follow_the_cursor() {
        // A 39-line screen with 100 lines written: 62 lines of scrollback + 39 visible lines, with line99 on the last line that has characters.
        let (mut app, _rx) = app(&lines(100));
        enter_copy(&mut app);
        assert_eq!(
            app.m.status.as_deref(),
            Some("Copy mode │ 100/101 │ v select · y copy · / ? search · q quit")
        );
        assert_eq!(offset(&app), 0);
        app.on_input(b"g");
        assert_eq!(offset(&app), 62);
        assert!(app.m.status.as_deref().unwrap().contains("│ 1/101 │"));
        // The query shows in the status line, and when found, it scrolls so that line is visible.
        app.on_input(b"/line5");
        assert_eq!(app.m.status.as_deref(), Some("/line5"));
        app.on_input(b"\r");
        assert_eq!(app.copy.as_ref().unwrap().mode.line, 5);
        assert!(app.m.status.as_deref().unwrap().contains("│ 6/101 │"));
        assert_eq!(offset(&app), 62);
        app.on_input(b"G");
        assert_eq!(offset(&app), 0);
        // If not found, it notifies on the spot.
        app.on_input(b"/nothing\r");
        assert!(app.m.status.as_deref().unwrap().contains("No matches"));
    }

    #[test]
    fn snapshot_switch_resize_detach_and_commands_leave_copy_mode() {
        let (mut app, _rx) = app(&lines(100));
        enter_copy(&mut app);
        app.on_msg(FromDaemon::Snapshot {
            id: "s1".into(),
            data: b"fresh".to_vec(),
        });
        assert!(app.copy.is_none() && app.m.status.is_none());
        assert_eq!(app.screen.as_ref().unwrap().lines()[0], "fresh");

        enter_copy(&mut app);
        app.attach("s2".into());
        assert!(app.copy.is_none() && app.m.status.is_none());

        app.m.attached = Some("s1".into());
        app.screen = Some(Screen::new(86, 39));
        enter_copy(&mut app);
        app.on_resize(Size::new(100, 30));
        assert!(app.copy.is_none() && app.m.status.is_none());

        enter_copy(&mut app);
        app.detach();
        assert!(app.copy.is_none() && app.m.status.is_none());

        // Other prefix commands end copy mode and run. `⌃a [` inside copy mode changes nothing.
        app.attach("s1".into());
        app.screen = Some(Screen::new(86, 39));
        enter_copy(&mut app);
        app.on_input(b"\x01[");
        assert!(app.copy.is_some());
        app.on_input(b"\x01b");
        assert!(app.copy.is_none() && app.m.status.is_none());
        assert!(!app.m.list_open);
    }

    #[test]
    fn rail_clicks_still_work_and_switching_leaves_copy_mode() {
        let (mut app, _rx) = app(&lines(100));
        enter_copy(&mut app);
        // Pressing the first project rail item (∗) chooses the project, and since the attached session disappears, copy mode ends too.
        app.mouse(ev(MouseKind::Down, 1, 1));
        assert_eq!(app.m.filter, None);
        assert!(app.copy.is_none());
    }

    #[test]
    fn too_much_output_ends_copy_mode_and_catches_up() {
        let (mut app, _rx) = app(b"hello");
        enter_copy(&mut app);
        app.on_msg(FromDaemon::Output {
            id: "s1".into(),
            data: vec![b'x'; MAX_HELD - 10],
        });
        assert!(app.copy.is_some());
        app.on_msg(FromDaemon::Output {
            id: "s1".into(),
            data: b"\r\nend".to_vec().into_iter().chain([b'!'; 10]).collect(),
        });
        assert!(app.copy.is_none() && app.m.status.is_none());
        assert_eq!(app.m.message, "Too much output, so copy mode was ended");
        // Even the overflowing chunk was fed to the screen, and it has returned to the bottom.
        assert_eq!(offset(&app), 0);
        assert!(
            app.screen
                .as_ref()
                .unwrap()
                .lines()
                .iter()
                .any(|l| l.starts_with("end!"))
        );
    }

    /// The key encoding mode bytes to write to the outer terminal (only what differs from last time).
    fn keys(app: &mut App) -> String {
        String::from_utf8(app.sync_keys()).unwrap()
    }

    /// The key encoding modes the session turned on (DECCKM, modifyOtherKeys 2, kitty 1), and the bytes that set and unset them on the outside.
    const KEY_MODES: &[u8] = b"\x1b[?1h\x1b[>4;2m\x1b[>1u";
    const APPLY: &str = "\x1b[?1h\x1b[>4;2m\x1b[=1;1u";
    const RESET: &str = "\x1b[?1l\x1b[>4;0m\x1b[=0;1u";

    #[test]
    fn mode_diff_writes_only_what_changed() {
        let none = KeyModes::default();
        let all = KeyModes {
            app_cursor: true,
            app_keypad: true,
            modify_other_keys: 2,
            kitty: 1,
        };
        let diff = |a, w| String::from_utf8(mode_diff(a, w)).unwrap();
        assert_eq!(diff(none, all), "\x1b[?1h\x1b=\x1b[>4;2m\x1b[=1;1u");
        assert_eq!(diff(all, all), "");
        let changed = KeyModes {
            modify_other_keys: 1,
            kitty: 9,
            ..all
        };
        assert_eq!(diff(all, changed), "\x1b[>4;1m\x1b[=9;1u");
        assert_eq!(diff(all, none), "\x1b[?1l\x1b>\x1b[>4;0m\x1b[=0;1u");
        // kitty flag 2 (event types) isn't set. If only 2 differs, there's nothing to write.
        let kitty = |kitty| KeyModes { kitty, ..none };
        assert_eq!(diff(none, kitty(3)), "\x1b[=1;1u");
        assert_eq!(diff(none, kitty(31)), "\x1b[=29;1u");
        assert_eq!(diff(kitty(1), kitty(3)), "");
        assert_eq!(diff(none, kitty(2)), "");
    }

    #[test]
    fn output_enabling_kitty_flags_reaches_the_outer_terminal() {
        let (mut app, _rx) = app(b"");
        assert_eq!(keys(&mut app), "");
        let output = |id: &str, data: &[u8]| FromDaemon::Output {
            id: id.into(),
            data: data.to_vec(),
        };
        app.on_msg(output("s1", b"\x1b[>1u"));
        assert_eq!(keys(&mut app), "\x1b[=1;1u");
        // A mode that's already set isn't written again.
        assert_eq!(keys(&mut app), "");
        app.on_msg(output("s1", b"\x1b[?1h\x1b=\x1b[>4;2m"));
        assert_eq!(keys(&mut app), "\x1b[?1h\x1b=\x1b[>4;2m");
        // Output from other sessions isn't followed.
        app.on_msg(output("s2", b"\x1b[<u\x1b[>4m"));
        assert_eq!(keys(&mut app), "");
        // When the app turns it off, it follows and turns it off.
        app.on_msg(output("s1", b"\x1b[<u\x1b[>4m\x1b[?1l\x1b>"));
        assert_eq!(keys(&mut app), "\x1b[?1l\x1b>\x1b[>4;0m\x1b[=0;1u");
    }

    #[test]
    fn session_switch_resets_then_applies_the_new_sessions_modes() {
        let (mut app, _rx) = app(KEY_MODES);
        assert_eq!(keys(&mut app), APPLY);
        app.attach("s2".into());
        assert_eq!(keys(&mut app), RESET);
        let mut s2 = Screen::new(86, 39);
        s2.feed(b"\x1b=\x1b[>4;1m");
        app.on_msg(FromDaemon::Snapshot {
            id: "s2".into(),
            data: s2.snapshot(),
        });
        assert_eq!(keys(&mut app), "\x1b=\x1b[>4;1m");
    }

    #[test]
    fn copy_mode_and_prompts_get_plain_keys_and_give_the_modes_back() {
        let (mut app, mut rx) = app(KEY_MODES);
        assert_eq!(keys(&mut app), APPLY);
        // The session turned on kitty, so `⌃a` arrives encoded.
        app.on_input(b"\x1b[97;5u[");
        assert!(app.copy.is_some());
        assert_eq!(keys(&mut app), RESET);
        app.on_input(b"q");
        assert!(app.copy.is_none());
        assert_eq!(keys(&mut app), APPLY);
        // The `⌃a c` launcher is the same. It covers the session pane, so keys must arrive plain.
        app.on_input(b"\x1b[97;5uc");
        assert!(app.m.launcher);
        assert_eq!(keys(&mut app), RESET);
        // Status line input opened there with `n` is the same, and Esc steps back one level at a time.
        app.on_input(b"n");
        assert!(app.m.prompt.is_some());
        assert_eq!(keys(&mut app), "");
        app.on_input(b"\x1b");
        assert!(app.m.prompt.is_none() && app.m.launcher);
        assert_eq!(keys(&mut app), "");
        app.on_input(b"\x1b");
        assert!(!app.m.launcher);
        assert_eq!(keys(&mut app), APPLY);
        assert!(sent(&mut rx).is_empty());
    }

    #[test]
    fn detach_and_losing_the_session_reset_the_modes() {
        let (mut quitting, _rx) = app(KEY_MODES);
        assert_eq!(keys(&mut quitting), APPLY);
        quitting.on_input(b"\x1b[27;5;97~d");
        assert!(quitting.quit);
        assert_eq!(keys(&mut quitting), RESET);

        let (mut alone, _rx) = app(KEY_MODES);
        assert_eq!(keys(&mut alone), APPLY);
        alone.detach();
        assert_eq!(keys(&mut alone), RESET);
    }

    #[test]
    fn focus_events_parsed_not_forwarded() {
        let (mut app, mut rx) = app(b"");
        app.on_input(b"\x1b[O\x1b[I");
        assert!(sent(&mut rx).is_empty());
        assert!(app.visible);
        // A session that turned focus reporting on gets them as they came.
        app.on_msg(FromDaemon::Output {
            id: "s1".into(),
            data: b"\x1b[?1004h".to_vec(),
        });
        app.on_input(b"\x1b[O\x1b[I");
        assert_eq!(sent(&mut rx), [input(b"\x1b[O"), input(b"\x1b[I")]);
    }

    #[test]
    fn presence_follows_focus_attach_and_input() {
        let presence = |rx: &mut UnboundedReceiver<ToDaemon>| -> Vec<(bool, u64, Option<String>)> {
            sent(rx)
                .into_iter()
                .filter_map(|m| match m {
                    ToDaemon::Presence {
                        visible,
                        last_input_ms,
                        viewing,
                    } => Some((visible, last_input_ms, viewing)),
                    _ => None,
                })
                .collect()
        };
        let s1 = || Some("s1".to_string());
        let secs = Duration::from_secs;
        let (mut app, mut rx) = app(b"");
        let t0 = Instant::now();
        // A daemon that doesn't know `Presence` hears nothing.
        app.sync_presence(t0);
        assert!(sent(&mut rx).is_empty());
        assert_eq!(app.presence_deadline(), None);
        app.presence = true;
        // Visible until the terminal says otherwise, and viewing the attached session.
        app.sync_presence(t0);
        assert_eq!(presence(&mut rx), [(true, 0, s1())]);
        app.on_input(b"\x1b[O");
        app.sync_presence(t0 + secs(1));
        assert_eq!(presence(&mut rx), [(false, 0, s1())]);
        // Coming back and typing is input, and the change goes out at once.
        let typed = now_ms();
        app.on_input(b"\x1b[Ix");
        app.sync_presence(t0 + secs(2));
        let [(true, first, _)] = presence(&mut rx)[..] else {
            panic!("no Presence on focus-in");
        };
        assert!(first >= typed);
        // More typing within 5 s of that waits for the next send past it.
        app.on_input(b"y");
        app.sync_presence(t0 + secs(3));
        assert!(presence(&mut rx).is_empty());
        app.sync_presence(t0 + secs(7));
        let [(true, second, _)] = presence(&mut rx)[..] else {
            panic!("the input wasn't reported");
        };
        assert!(second >= first);
        // The launcher over the session hides its terminal. While visible, the heartbeat comes 30 s after the last send.
        app.on_input(b"\x01c");
        app.sync_presence(t0 + secs(8));
        assert_eq!(presence(&mut rx), [(true, second, None)]);
        assert_eq!(
            app.presence_deadline(),
            Some(t0 + secs(8) + mine_core::proto::PRESENCE_HEARTBEAT)
        );
    }

    #[test]
    fn frozen_screen_does_not_end_a_stale_sync_update() {
        // A synchronized update (`?2026`) is in progress, so `buffered` hasn't been reflected on the screen yet.
        let (mut app, _rx) = app(b"hello\x1b[?2026hbuffered");
        assert!(app.sync_deadline().is_some());
        enter_copy(&mut app);
        assert!(app.sync_deadline().is_none());
        std::thread::sleep(Duration::from_millis(300));
        app.end_stale_sync();
        assert_eq!(history(&app)[0], "hello");
        // After leaving, the update that exceeded its time is ended and reflected on the screen.
        app.on_input(b"q");
        assert!(app.sync_deadline().is_some());
        app.end_stale_sync();
        assert_eq!(app.screen.as_ref().unwrap().lines()[0], "hellobuffered");
    }

    fn shell(name: &str) -> SessionInfo {
        use mine_core::session::{Kind, Meta, Restore, SessionState};
        SessionInfo {
            meta: Meta {
                id: format!("p-{name}"),
                project: PathBuf::from("/p"),
                name: name.into(),
                kind: Kind::Shell,
                account: None,
                model: None,
                restore: Restore::None,
                created_ms: 0,
                resume_id: None,
                closed: false,
            },
            state: SessionState::Idle,
            waiting: None,
            alive: true,
        }
    }

    /// A client that received the built-in profiles ([`ui::builtin`]) and then the first list saying project `/p` has no sessions at all.
    fn empty_app() -> (App, UnboundedReceiver<ToDaemon>) {
        let (tx, rx) = unbounded_channel();
        let mut app = App::new(tx, PathBuf::from("/p"), Size::new(120, 40));
        app.on_msg(FromDaemon::Profiles {
            profiles: ui::builtin(),
        });
        app.on_msg(FromDaemon::Sessions { sessions: vec![] });
        (app, rx)
    }

    /// A client that received the built-in profiles and the first list where project `/p`'s sessions are `names`. It attaches to the
    /// first row's session, and the `Attach` sent then is drained.
    fn started_app(names: &[&str]) -> (App, UnboundedReceiver<ToDaemon>) {
        let (tx, mut rx) = unbounded_channel();
        let mut app = App::new(tx, PathBuf::from("/p"), Size::new(120, 40));
        app.on_msg(FromDaemon::Profiles {
            profiles: ui::builtin(),
        });
        app.on_msg(FromDaemon::Sessions {
            sessions: names.iter().map(|n| shell(n)).collect(),
        });
        sent(&mut rx);
        (app, rx)
    }

    /// The `New` the launcher sends to create in `/p`.
    fn new_session(kind: Kind, account: Option<&str>, name: &str) -> ToDaemon {
        use mine_core::session::NewSession;
        ToDaemon::New {
            spec: NewSession {
                project: PathBuf::from("/p"),
                name: name.into(),
                kind,
                account: account.map(str::to_string),
                model: None,
            },
        }
    }

    #[test]
    fn no_sessions_shows_the_launcher_and_enter_starts_claude_a_t1_and_attaches() {
        let (mut app, mut rx) = empty_app();
        assert!(app.m.launcher && app.m.attached.is_none());
        assert!(sent(&mut rx).is_empty());
        app.on_input(b"\r");
        assert_eq!(
            sent(&mut rx),
            [new_session(Kind::Claude, Some("claude-a"), "t1")]
        );
        // When the created session appears in the list, it attaches as usual and the launcher closes.
        app.on_msg(FromDaemon::Sessions {
            sessions: vec![shell("t1")],
        });
        assert_eq!(app.m.attached.as_deref(), Some("p-t1"));
        assert!(!app.m.launcher && app.pending.is_none());
        assert_eq!(
            sent(&mut rx),
            [ToDaemon::Attach {
                id: "p-t1".into(),
                cols: 86,
                rows: 39
            }]
        );
    }

    #[test]
    fn digit_starts_that_profile() {
        // t1 and t3 exist, so the new name is the free slot t2. `⌃a c` opens the launcher over the attached session.
        for (key, kind, account) in [
            (b"1", Kind::Claude, Some("claude-a")),
            (b"2", Kind::Claude, Some("claude-b")),
            (b"3", Kind::Codex, Some("codex")),
            (b"s", Kind::Shell, None),
        ] {
            let (mut app, mut rx) = started_app(&["t1", "t3"]);
            assert_eq!(app.m.attached.as_deref(), Some("p-t1"));
            app.on_input(b"\x01c");
            assert!(app.m.launcher);
            app.on_input(key);
            assert_eq!(sent(&mut rx), [new_session(kind, account, "t2")], "{key:?}");
        }
        // The rows follow whatever list the daemon sent last, in its order.
        let (mut app, mut rx) = empty_app();
        let mut list = ui::builtin();
        list.insert(
            0,
            ui::profile(
                "team",
                mine_core::profiles::ProfileKind::Claude,
                "team",
                "#98c379",
            ),
        );
        app.on_msg(FromDaemon::Profiles { profiles: list });
        app.on_input(b"2");
        assert_eq!(
            sent(&mut rx),
            [new_session(Kind::Claude, Some("claude-a"), "t1")]
        );
    }

    #[test]
    fn not_logged_in_row_refuses() {
        let (mut app, mut rx) = empty_app();
        let mut list = ui::builtin();
        list[0].logged_in = false;
        list[1].logged_in = false;
        app.on_msg(FromDaemon::Profiles { profiles: list });
        app.on_input(b"2");
        assert!(sent(&mut rx).is_empty());
        assert_eq!(
            app.m.message,
            "Not logged in. Run: mine profile login claude-b"
        );
        assert!(app.m.launcher && app.pending.is_none());
        // Enter on a default profile that isn't logged in refuses the same way; the other rows still start.
        app.on_input(b"\r");
        assert!(sent(&mut rx).is_empty());
        assert_eq!(
            app.m.message,
            "Not logged in. Run: mine profile login claude-a"
        );
        app.on_input(b"3");
        assert_eq!(
            sent(&mut rx),
            [new_session(Kind::Codex, Some("codex"), "t1")]
        );
    }

    #[test]
    fn before_the_profile_list_arrives_only_shell_and_typed_start() {
        let (tx, mut rx) = unbounded_channel();
        let mut app = App::new(tx, PathBuf::from("/p"), Size::new(120, 40));
        app.on_msg(FromDaemon::Sessions { sessions: vec![] });
        app.on_input(b"\r");
        app.on_input(b"1");
        assert!(sent(&mut rx).is_empty() && app.pending.is_none());
        app.on_input(b"s");
        assert_eq!(sent(&mut rx), [new_session(Kind::Shell, None, "t1")]);
    }

    #[test]
    fn typed_form_starts_any_profile_and_shows_the_usage_on_error() {
        let (mut app, mut rx) = empty_app();
        let mut list = ui::builtin();
        list.push(ui::profile(
            "team",
            mine_core::profiles::ProfileKind::Claude,
            "team",
            "#98c379",
        ));
        app.on_msg(FromDaemon::Profiles { profiles: list });
        app.on_input(b"n");
        app.on_input(b"gpt t9\r");
        assert!(sent(&mut rx).is_empty());
        assert!(app.m.message.contains(view::NEW_USAGE), "{}", app.m.message);
        app.on_input(b"n");
        app.on_input(b"team t9\r");
        assert_eq!(
            sent(&mut rx),
            [new_session(Kind::Claude, Some("team"), "t9")]
        );
    }

    #[test]
    fn typed_form_refuses_a_profile_that_isnt_logged_in() {
        let (mut app, mut rx) = empty_app();
        let mut list = ui::builtin();
        list[1].logged_in = false;
        app.on_msg(FromDaemon::Profiles { profiles: list });
        app.on_input(b"n");
        app.on_input(b"claude-b t9\r");
        assert!(sent(&mut rx).is_empty());
        assert_eq!(
            app.m.message,
            "Not logged in. Run: mine profile login claude-b"
        );
        assert!(app.pending.is_none());
        // A shell and a logged-in profile still start from the typed form.
        app.on_input(b"n");
        app.on_input(b"claude-a t9\r");
        assert_eq!(
            sent(&mut rx),
            [new_session(Kind::Claude, Some("claude-a"), "t9")]
        );
    }

    #[test]
    fn a_choice_while_the_last_one_is_pending_is_ignored() {
        let (mut app, mut rx) = empty_app();
        app.on_input(b"\r");
        app.on_input(b"s");
        app.on_input(b"\r");
        assert_eq!(
            sent(&mut rx),
            [new_session(Kind::Claude, Some("claude-a"), "t1")]
        );
        // If the daemon refuses, you can pick again.
        app.on_msg(FromDaemon::Error {
            message: "no".into(),
        });
        app.on_input(b"s");
        assert_eq!(sent(&mut rx), [new_session(Kind::Shell, None, "t1")]);
    }

    #[test]
    fn other_keys_and_pastes_are_ignored_and_prefix_commands_still_work() {
        let (mut app, mut rx) = empty_app();
        app.on_input(b"hello\x7f ");
        app.on_input("한글".as_bytes());
        app.on_action(Action::Paste(b"pasted".to_vec()));
        app.on_input(b"\x1b[A");
        assert!(sent(&mut rx).is_empty());
        assert!(app.m.launcher && app.m.prompt.is_none() && app.pending.is_none());
        // `⌃a b` collapses the list, and `⌃a d` exits.
        app.on_input(b"\x01b");
        assert!(!app.m.list_open);
        app.on_input(b"\x01d");
        assert!(app.quit);
    }

    #[test]
    fn ctrl_a_c_over_a_session_opens_the_launcher_and_esc_closes_it() {
        let (mut app, mut rx) = app(b"hello");
        app.on_input(b"\x01c");
        assert!(app.m.launcher && app.m.attached.is_some());
        // While the session pane is covered, keys and paste don't go to the session.
        app.on_input(b"a");
        app.on_action(Action::Paste(b"p".to_vec()));
        assert!(sent(&mut rx).is_empty());
        app.on_input(b"\x1b");
        assert!(!app.m.launcher);
        assert_eq!(app.m.attached.as_deref(), Some("s1"));
        // After closing, keys go to the session again. The session screen is as it was.
        app.on_input(b"a");
        assert_eq!(sent(&mut rx), [input(b"a")]);
        assert_eq!(app.screen.as_ref().unwrap().lines()[0], "hello");
    }

    #[test]
    fn esc_does_nothing_when_no_session_is_attached() {
        let (mut app, mut rx) = empty_app();
        app.on_input(b"\x1b");
        assert!(app.m.launcher && app.m.prompt.is_none());
        assert!(sent(&mut rx).is_empty());
    }

    #[test]
    fn n_opens_the_typed_prompt_and_esc_comes_back_to_the_launcher() {
        let (mut app, mut rx) = empty_app();
        app.on_input(b"n");
        assert_eq!(app.m.prompt, Some(Prompt::New(String::new())));
        assert!(sent(&mut rx).is_empty());
        app.on_input(b"\x1b");
        assert!(app.m.prompt.is_none() && app.m.launcher);
        // Creates one by typing the profile, name and model.
        app.on_input(b"n");
        app.on_input(b"claude-b mine opus\r");
        let mut spec = new_session(Kind::Claude, Some("claude-b"), "mine");
        if let ToDaemon::New { spec } = &mut spec {
            spec.model = Some("opus".into());
        }
        assert_eq!(sent(&mut rx), [spec]);
        assert!(app.m.prompt.is_none() && app.pending.is_some());
    }

    #[test]
    fn clicking_an_option_row_starts_it_and_the_rest_of_the_pane_does_nothing() {
        let (mut app, mut rx) = empty_app();
        // The terminal pane is 86x39 at (34, 0). The menu is 24 wide and eleven rows tall, so it starts at (65, 14), and the choice rows begin four rows below.
        let (left, top) = (34 + (86 - 24) / 2, (39 - 11) / 2);
        app.mouse(ev(MouseKind::Down, left + 3, top + 5));
        assert_eq!(
            sent(&mut rx),
            [new_session(Kind::Claude, Some("claude-b"), "t1")]
        );
        app.on_msg(FromDaemon::Error {
            message: "no".into(),
        });
        // Outside a row, elsewhere in the terminal pane, and the wheel do nothing.
        app.mouse(ev(MouseKind::Down, left - 2, top + 5));
        app.mouse(ev(MouseKind::Down, left + 3, top));
        app.mouse(ev(MouseKind::WheelUp, left + 3, top + 5));
        app.mouse(MouseEv {
            button: 2,
            ..ev(MouseKind::Down, left + 3, top + 5)
        });
        assert!(sent(&mut rx).is_empty());
        // The `n` row opens the prompt.
        app.mouse(ev(MouseKind::Down, left, top + 8));
        assert!(app.m.prompt.is_some());
    }

    #[test]
    fn clicks_over_a_session_launcher_never_reach_the_session() {
        let (mut app, mut rx) = app(b"\x1b[?1000h\x1b[?1002h");
        app.on_input(b"\x01c");
        app.mouse(ev(MouseKind::Down, 40, 5));
        app.mouse(ev(MouseKind::Up, 40, 5));
        app.mouse(ev(MouseKind::WheelUp, 40, 5));
        assert!(sent(&mut rx).is_empty());
        // A new session started by a click closes the launcher when it attaches.
        app.mouse(ev(MouseKind::Down, 70, 20));
        assert!(matches!(sent(&mut rx)[..], [ToDaemon::New { .. }]));
    }

    #[test]
    fn switching_session_closes_the_launcher_and_losing_it_reopens_it() {
        let (mut app, _rx) = started_app(&["a", "b"]);
        assert_eq!(app.m.attached.as_deref(), Some("p-a"));
        app.on_input(b"\x01c");
        assert!(app.m.launcher);
        app.on_input(b"\x01n");
        assert!(!app.m.launcher && app.m.attached.as_deref() == Some("p-b"));
        // If the attached session disappears and there is no other session, the launcher shows again.
        app.on_msg(FromDaemon::Sessions {
            sessions: Vec::new(),
        });
        assert!(app.m.launcher && app.m.attached.is_none() && app.screen.is_none());
    }

    #[test]
    fn copy_mode_is_refused_while_the_launcher_covers_the_session() {
        let (mut app, _rx) = app(&lines(100));
        app.on_input(b"\x01c");
        app.on_input(b"\x01[");
        assert!(app.copy.is_none() && app.m.launcher);
        assert_eq!(app.m.message, "No session screen to copy");
    }

    #[test]
    fn launcher_state_is_drawn_into_the_frame() {
        let (mut app, _rx) = empty_app();
        // Wide characters are written with a cursor move per cell, so look at the ASCII text only.
        let out = frame(&mut app);
        assert!(out.contains("work") && out.contains("shell"), "{out:?}");
        // The status line hint in the launcher.
        assert!(
            out.contains("↵ personal · 1–3 profiles · s shell"),
            "{out:?}"
        );
        // Opening status line input with `n` shows the cursor in the status line.
        app.on_input(b"n");
        assert!(frame(&mut app).contains("\x1b[?25h"));
    }

    #[test]
    fn hangul_in_the_launcher_is_not_acted_on_and_says_to_switch_to_english() {
        let (mut app, mut rx) = empty_app();
        // `s` as Hangul jamo (ㄴ), a syllable, and several characters.
        for key in ["ㄴ", "한", "한글"] {
            app.m.message.clear();
            app.on_input(key.as_bytes());
            assert_eq!(app.m.message, input::HANGUL_HINT, "{key}");
        }
        assert!(sent(&mut rx).is_empty());
        assert!(app.m.launcher && app.m.prompt.is_none() && app.pending.is_none());
        // Pressing an English key starts as usual.
        app.on_input(b"s");
        assert_eq!(sent(&mut rx), [new_session(Kind::Shell, None, "t1")]);
        // A non-Hangul key is ignored without a hint.
        let (mut app, _rx) = empty_app();
        app.on_input(b"q");
        assert!(app.m.message.is_empty());
    }

    #[test]
    fn hangul_typed_into_the_prompt_or_the_session_is_unaffected() {
        let (mut launching, mut rx) = empty_app();
        launching.on_input(b"n");
        launching.on_input("한글".as_bytes());
        assert_eq!(launching.m.prompt, Some(Prompt::New("한글".into())));
        assert!(launching.m.message.is_empty());
        assert!(sent(&mut rx).is_empty());
        // It goes to the session as is.
        let (mut attached, mut rx) = app(b"");
        attached.on_input("한글ㅊ".as_bytes());
        assert_eq!(sent(&mut rx), [input("한글ㅊ".as_bytes())]);
        assert!(attached.m.message.is_empty() && !attached.m.launcher);
    }

    #[test]
    fn hangul_after_the_prefix_is_dropped_with_a_hint_and_never_reaches_the_session() {
        let (mut app, mut rx) = app(b"");
        // The shape of pressing `⌃a c` while in Hangul input (ㅊ).
        app.on_input(b"\x01");
        app.on_input("ㅊ".as_bytes());
        assert_eq!(app.m.message, input::HANGUL_HINT);
        assert!(sent(&mut rx).is_empty());
        assert!(!app.m.launcher && app.m.prompt.is_none());
        // The prefix was released, so the next key goes to the session. Switching to English and pressing makes it a command.
        app.on_input(b"x");
        assert_eq!(sent(&mut rx), [input(b"x")]);
        app.on_input(b"\x01c");
        assert!(app.m.launcher);
    }

    #[test]
    fn hangul_in_copy_mode_is_ignored_with_a_hint_unless_a_search_query_is_being_typed() {
        let (mut app, mut rx) = app(&lines(100));
        enter_copy(&mut app);
        let at = copy_line(&app);
        // `k` (ㅏ) doesn't move up, and the status line shows a hint.
        app.on_input("ㅏ".as_bytes());
        assert_eq!(copy_line(&app), at);
        let status = app.m.status.clone().unwrap();
        assert!(status.contains(input::HANGUL_HINT), "{status}");
        assert!(status.starts_with("Copy mode │"));
        // Pressing an English key clears the hint and moves as usual.
        app.on_input(b"k");
        assert_eq!(copy_line(&app), at - 1);
        assert!(
            !app.m
                .status
                .as_deref()
                .unwrap()
                .contains("Switch your keyboard")
        );
        // A Hangul syllable is the same, and the wheel clears it too.
        app.on_input("한".as_bytes());
        assert!(
            app.m
                .status
                .as_deref()
                .unwrap()
                .contains(input::HANGUL_HINT)
        );
        app.mouse(ev(MouseKind::WheelUp, 40, 5));
        assert!(
            !app.m
                .status
                .as_deref()
                .unwrap()
                .contains("Switch your keyboard")
        );
        // Hangul after the prefix is also reported in the copy mode status line. Copy mode doesn't end.
        app.on_input("\x01ㅊ".as_bytes());
        assert!(
            app.m
                .status
                .as_deref()
                .unwrap()
                .contains(input::HANGUL_HINT)
        );
        assert!(app.copy.is_some());
        // Hangul while typing a search query is the query.
        app.on_input(b"/");
        app.on_input("한글".as_bytes());
        assert_eq!(app.m.status.as_deref(), Some("/한글"));
        assert!(!app.copy.as_ref().unwrap().mode.hangul);
        // `/한글` arriving at once is also a query. The leading Hangul (`ㅏ/`) is in the command position, so it's reported.
        app.on_input(b"\r");
        app.on_input("/한".as_bytes());
        assert_eq!(app.m.status.as_deref(), Some("/한"));
        assert!(sent(&mut rx).is_empty());
    }
}
