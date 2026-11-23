//! 클라이언트 화면(명령 없이 `mine`). 바깥 터미널을 잡고 데몬에 붙어, 고른 세션 화면을 그리고 입력을 넘긴다.
//!
//! 이 작업 하나가 상태를 모두 쥐고 표준 입력(스레드)·데몬 메시지(읽기 작업)·SIGWINCH·다시 그릴 때를
//! `select!` 하나로 기다린다. `select!`에는 취소에 안전한 채널 수신과 타이머만 넣는다.

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
use mine_core::accounts::Labels;
use mine_core::conn::Conn;
use mine_core::keys::KeyModes;
use mine_core::launch::{self, Choice};
use mine_core::paths::State;
use mine_core::proto::{FromDaemon, ToDaemon};
use mine_core::screen::Screen;
use mine_core::select::{
    MouseEv, MouseKind, encode_sgr, selected_text, session_takes, wants_mouse,
};
use mine_core::session::{NewSession, SessionInfo};
use mine_core::view;
use select::Select;
use ui::{Hit, LinkRun, Model, Prompt};

/// 다시 그리는 최소 간격(초당 60번).
const FRAME: Duration = Duration::from_micros(16_667);
/// 휠 한 칸에 스크롤하는 줄 수.
const WHEEL_LINES: i32 = 3;

/// 바깥 화면 좌표와 그 칸.
type Cells = Vec<(u16, u16, Cell)>;

/// 클라이언트를 실행한다. `⌃a d`로 나오면 `Ok`다. 데몬과 세션은 계속 돈다.
pub async fn run(state: State, project: PathBuf) -> Result<()> {
    // 계정 이름은 시작할 때 한 번 읽는다. 읽다 남기는 경고가 화면을 깨지 않게 바깥 터미널을 잡기 전에 읽는다.
    let labels = Labels::load(&state, std::env::home_dir().as_deref());
    let _tty = tty::Tty::enter()?;
    // 표준 입력 읽기 스레드보다 먼저 묻고 끝낸다(R13). 그사이 들어온 키는 아래에서 먼저 먹인다.
    let (colors, early) = tty::probe_colors();
    let conn = Conn::connect_or_spawn_async(&state).await?;
    let mut stdin = tty::spawn_stdin_reader();
    let mut winch = signal(SignalKind::window_change())?;
    // 종료·끊김 신호도 `⌃a d`처럼 끝낸다. 키 인코딩 모드를 되돌리고 `Tty`가 바깥 터미널을 되돌린다.
    let mut term = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut terminal = Terminal::new(CrosstermBackend::new(BufWriter::with_capacity(
        1 << 16,
        io::stdout(),
    )))?;
    let mut rx = conn.rx;
    let mut app = App::new(conn.tx, project, terminal.size()?);
    app.m.labels = labels;
    if let Some((fg, bg)) = colors {
        app.send(ToDaemon::Colors {
            fg: fg.to_string(),
            bg: bg.to_string(),
        });
    }
    // 목록 변경을 받으려면 List를 한 번 보내 구독해야 한다(R18). 답이 오면 붙을 세션을 고른다.
    app.send(ToDaemon::List);
    app.on_input(&early);
    let mut last_draw: Option<Instant> = None;
    loop {
        app.end_stale_sync();
        // 키 인코딩 모드는 상태가 바뀐 바로 다음, 프레임 사이에 화면과 같은 쓰기 통로로 건다.
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
                None => bail!("데몬 연결이 끊겼다. mine을 다시 실행하면 세션에 다시 붙는다"),
            },
            _ = winch.recv() => app.on_resize(terminal.size()?),
            _ = term.recv() => app.quit = true,
            _ = hangup.recv() => app.quit = true,
            () = tokio::time::sleep_until(wake.into()) => {}
        }
    }
    Ok(())
}

/// 바깥에 거는 kitty 플래그.
///
/// ponytail: 플래그 2(뗌·반복 이벤트 보고)는 빼고 건다. 뗌·반복까지 오면 접두 키 처리가 훨씬 복잡해지고,
/// CLI 앱은 거의 쓰지 않는다. 이 플래그를 켠 앱은 누름만 받는다.
const KITTY_MIRRORED: u8 = !2;

/// 바깥 터미널에 `applied`가 걸려 있을 때 `wanted`로 바꾸는 바이트. 달라진 모드만 쓴다.
/// kitty 플래그는 시작할 때 쌓은 한 단(`tty::ENTER`)에 통째로 건다.
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

/// 복사 모드에서 돌아다니는 얼린 화면. `lines`는 들어갈 때의 스크롤백 + 보이는 줄이다.
struct Copying {
    mode: CopyMode,
    lines: Vec<String>,
    /// 그동안 온 세션 출력. 나갈 때 화면에 먹인다.
    held: Vec<u8>,
}

/// Esc로 복사 모드를 나온 뒤, 따로 읽혀 온 단독 Esc를 세션에 넘기지 않고 떼는 시간.
/// Esc를 두 번 눌러 나오는 사람이 많고, 둘째 Esc는 보통 다른 읽기로 온다.
///
/// ponytail: 타이머를 걸지 않고 다음 입력이 올 때 지난 시간만 본다. 입력 간격을 재는 정교한 방법은 필요해지면 한다.
const ESC_GRACE: Duration = Duration::from_millis(300);

/// 복사 모드 동안 쌓아 두는 출력의 최대 바이트.
///
/// ponytail: 넘으면 복사 모드를 끝내고 따라잡는다. 디스크로 넘기거나 오래된 출력을 줄이는 일은 필요해지면 한다.
const MAX_HELD: usize = 8 << 20;

struct App {
    m: Model,
    /// 붙은 세션의 화면. 데몬이 보낸 스냅숏으로 만들고 출력을 먹인다. 질의 답은 데몬이 하므로 버린다.
    screen: Option<Screen>,
    tx: UnboundedSender<ToDaemon>,
    parser: InputParser,
    /// 바깥 터미널 크기.
    area: Rect,
    /// 처음 받은 목록으로 붙을 세션을 정했다.
    started: bool,
    /// `New`를 보낸 세션의 (프로젝트, 이름). 목록에 나타나면 붙는다.
    pending: Option<(PathBuf, String)>,
    /// 지난 화면에서 하이퍼링크로 다시 쓴 구간.
    links: Vec<LinkRun>,
    /// 터미널 칸의 드래그 선택.
    select: Select,
    /// 바깥 터미널에 쓸 클립보드 OSC 52. 그릴 때 화면과 같은 쓰기 통로로 내보낸다.
    clipboard: Vec<u8>,
    /// 복사 모드. 들어갈 때 화면을 얼려 두고, 그동안 온 출력은 쌓아 둔다.
    copy: Option<Copying>,
    /// Esc로 복사 모드를 나온 때. `ESC_GRACE` 안에 오는 입력 앞의 단독 Esc는 뗀다.
    esc_exit: Option<Instant>,
    /// 세션에 넘긴 누름이 아직 눌려 있다. 이 동안 칸 밖 끌기·뗌도 칸 안으로 눌러 넘겨 세션이 뗌을 받게 한다.
    held: bool,
    /// 바깥 터미널에 지금 걸린 키 인코딩 모드.
    keys: KeyModes,
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
                labels: Labels::default(),
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
            dirty: true,
            quit: false,
        }
    }

    /// 바깥 터미널에 걸 키 인코딩 모드를 맞추는 바이트. 돌려준 바이트를 썼다고 보고 걸린 상태를 바꾼다.
    ///
    /// 붙은 세션 화면의 모드를 따르되, 세션 화면이 없을 때, mine이 키를 직접 읽는 복사 모드·상태줄 입력·시작 화면 동안,
    /// 끝낼 때는 기본값으로 돌린다. 이때 키는 평범한(레거시) 인코딩으로 와야 한다.
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

    /// 쓰기 작업이 끝났으면(연결이 끊겼으면) 버린다. 곧 `rx`가 끝을 알린다.
    fn send(&self, msg: ToDaemon) {
        let _ = self.tx.send(msg);
    }

    fn say(&mut self, message: &str) {
        self.m.message = message.to_string();
        self.dirty = true;
    }

    /// 터미널 칸 크기. `Attach`와 `Resize`가 이 크기를 보낸다.
    fn term_size(&self) -> (u16, u16) {
        let term = ui::layout(self.area, self.m.list_open).term;
        (term.width.max(1), term.height.max(1))
    }

    fn on_msg(&mut self, msg: FromDaemon) {
        let attached = |id: &str| self.m.attached.as_deref() == Some(id);
        match msg {
            FromDaemon::Sessions { sessions } => self.on_sessions(sessions),
            FromDaemon::Snapshot { id, data } if attached(&id) => {
                // 새 화면이 얼려 둔 줄을 대신한다. 쌓아 둔 출력도 스냅숏에 들어 있다.
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
                    // 줄 번호가 흔들리지 않게 얼린 화면에는 먹이지 않고 쌓아 둔다.
                    copy.held.extend_from_slice(&data);
                    if copy.held.len() > MAX_HELD {
                        self.exit_copy();
                        self.say("복사 모드 종료: 출력이 너무 많다");
                    }
                } else if let Some(screen) = &mut self.screen {
                    screen.feed(&data);
                    screen.take_replies();
                    // 반전은 보이는 칸에 걸려 있어 출력이 칸을 밀면 엉뚱한 글자를 가린다. 끄는 중에는 뗄 때 읽는다.
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
            // 버전은 연결할 때 확인했고, 다른 세션의 화면은 이제 보지 않는다.
            FromDaemon::Hello { .. } | FromDaemon::Snapshot { .. } | FromDaemon::Output { .. } => {}
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

    /// 지금 목록의 첫 줄 세션에 붙는다. 보이는 줄이 없으면(대기 묶음은 처음에 접혀 있다) 접힌 묶음까지 본다.
    /// 붙으면 그 묶음은 펼쳐 그린다(`Model::visible_groups`).
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

    /// 붙은 세션을 놓는다. 데몬에는 떨어지는 메시지가 없어 그 세션 출력이 계속 오지만 버린다.
    /// 붙은 세션이 없으면 시작 화면이 터미널 칸을 채운다.
    fn detach(&mut self) {
        self.m.attached = None;
        self.m.launcher = true;
        self.screen = None;
        self.exit_copy();
        self.select.clear();
        self.held = false;
        self.dirty = true;
    }

    /// 프로젝트를 고른다. 붙은 세션이 그 프로젝트 것이 아니면 그 프로젝트의 첫 줄 세션으로 옮긴다.
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
        if !matches!(action, Action::Mouse(_)) {
            // 키를 누르면 선택 반전과 알림은 걷는다.
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
            // 복사 모드와 시작 화면에서는 입력이 세션으로 가지 않는다.
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
        }
    }

    /// Esc로 복사 모드를 막 나왔으면(`ESC_GRACE` 안) 입력 앞의 단독 Esc를 뗀다. 뗀 뒤에 남은 입력이 있으면 창은 닫힌다.
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

    /// 키 입력과 붙여넣기를 세션에 넘긴다. 스크롤해 둔 화면은 맨 아래로 돌려놓고 넘긴다.
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
        // 복사 모드 안에서 다른 명령을 하면 복사 모드를 끝내고 한다.
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
                None => self.say("닫을 세션이 없다"),
            },
            Cmd::ToggleList => {
                self.m.list_open = !self.m.list_open;
                self.resize_session();
            }
            Cmd::CopyMode => self.enter_copy(),
            Cmd::Detach => self.quit = true,
        }
    }

    /// 목록에 보이는 줄 순서로 다음·이전 세션에 붙는다. 끝에서는 처음으로 돈다.
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

    /// 아이콘 줄 순서(∗ 포함)로 다음·이전 프로젝트를 고른다.
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
        // 선택을 끄는 중이면 터미널 칸 밖에서 끌거나 떼도 칸 안으로 눌러 이어 간다.
        if self.select.dragging() && matches!(ev.kind, MouseKind::Drag | MouseKind::Up) {
            let at = select::local(areas.term, ev.col, ev.row);
            return self.select_event(ev, at);
        }
        // 세션에 넘긴 누름이 눌려 있으면 칸 밖에서 끌거나 떼도 칸 안으로 눌러 넘긴다. 앱이 뗌을 놓치지 않게 한다.
        if self.held && matches!(ev.kind, MouseKind::Drag | MouseKind::Up) {
            let at = select::local(areas.term, ev.col, ev.row);
            return self.term_mouse(ev, at);
        }
        if ev.kind == MouseKind::Down && self.select.clear() {
            self.dirty = true;
        }
        let hit = ui::hit(&areas, &self.m, ev.col, ev.row);
        if let Hit::Term(x, y) = hit {
            // 복사 모드에서는 터미널 칸의 마우스를 세션에 넘기지도 선택에 쓰지도 않는다. 휠만 커서를 옮긴다.
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

    /// 터미널 칸의 마우스(`at`은 칸 기준). 세션이 마우스를 쓰면 그대로 넘기고, 아니면 휠은 스크롤하고
    /// 왼쪽 단추 끌기는 선택한다. 칸 안 누름이 눌려 있는 동안은 칸 밖에서 온 끌기·뗌도 여기로 눌려 온다.
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
        // 세션이 마우스를 껐으면 누르고 있던 것도 끝난 셈이다.
        self.held = false;
        let step = match ev.kind {
            MouseKind::WheelUp => WHEEL_LINES,
            MouseKind::WheelDown => -WHEEL_LINES,
            _ => return self.select_event(ev, at),
        };
        // 클라이언트가 가진 화면 사본만 스크롤한다. 세션에는 아무것도 보내지 않는다.
        let before = screen.term().grid().display_offset();
        screen.scroll(step);
        if screen.term().grid().display_offset() != before {
            self.select.clear();
            self.dirty = true;
        }
    }

    /// 왼쪽 단추의 누름·끌기·뗌으로 선택을 진행한다. 뗄 때 선택이 있으면 복사한다.
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

    /// `⌃a [`: 지금 화면(스크롤백 포함)을 얼려 두고 복사 모드에 들어간다. 이미 들어가 있으면 그대로 둔다.
    fn enter_copy(&mut self) {
        if self.copy.is_some() {
            return;
        }
        // 시작 화면이 세션 칸을 덮고 있으면 복사할 화면이 보이지 않는다.
        let Some(screen) = self.screen.as_ref().filter(|_| !self.m.launcher) else {
            return self.say("복사할 세션 화면이 없다");
        };
        // 쪽 넘김의 한 쪽(터미널 칸의 줄 수)은 `freeze`가 화면에서 읽는다. 크기가 바뀌면 복사 모드가 끝난다.
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

    /// 복사 모드의 휠: 커서를 `step`줄(아래가 양수) 옮기고 화면이 따라가게 한다.
    fn copy_wheel(&mut self, step: i32) {
        if let Some(copy) = &mut self.copy {
            copy.mode.move_lines(&copy.lines, step as isize);
        }
        self.sync_copy();
    }

    /// 복사 모드의 키를 먹인다. 끝나는 키(`q`, `y`, Esc)가 오면 그 뒤 바이트는 복사 모드 키가 아니니,
    /// 다음 읽기로 온 것처럼 평소 입력 경로로 세션에 넘긴다. 다만 Esc로 끝냈으면 바로 뒤의 단독 Esc는 뗀다
    /// (Esc를 두 번 눌러 나오는 습관이 세션에 Esc를 보내지 않게).
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
                    // 검색어를 입력하는 중이 아니면 명령 키 자리다. 한글은 동작하지 않으니 알린다.
                    hangul |= !copy.mode.typing && input::hangul_at(&bytes[end - 1..]);
                    copy.mode.key(&copy.lines, b)
                }
                copymode::Key::Arrow(dir) => copy.mode.arrow(&copy.lines, dir),
                // 검색어를 입력하는 중이면 ⌃b·⌃f는 무시되니 화살표처럼 입력에 섞이지 않는다.
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
            // 글자가 이어 가면 Esc 뒤의 일은 끝난 것이다.
            self.esc_exit = None;
            self.input(rest.to_vec());
        }
    }

    /// 커서 줄이 보이게 화면을 스크롤하고 상태줄을 복사 모드 글로 바꾼다.
    fn sync_copy(&mut self) {
        if let (Some(copy), Some(screen)) = (&mut self.copy, &mut self.screen) {
            copy.mode.reveal(screen);
            self.m.status = Some(copy.mode.status(copy.lines.len()));
        }
        self.dirty = true;
    }

    /// 복사 모드를 끝낸다. 쌓아 둔 출력을 화면에 먹이고 맨 아래로 돌아간다.
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

    /// 입력 중인 상태줄에 키를 넣는다. `⌃a x` 확인은 `y`만 닫고 다른 키는 취소다.
    fn prompt_keys(&mut self, bytes: &[u8]) {
        self.dirty = true;
        match self.m.prompt.take() {
            Some(Prompt::ConfirmKill(id)) => {
                if bytes == b"y" || bytes == b"Y" {
                    self.send(ToDaemon::Kill { id });
                }
            }
            Some(Prompt::New(mut text)) => {
                // Esc 단독은 취소, 화살표 같은 다른 이스케이프 시퀀스는 무시한다.
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

    /// 시작 화면의 `n` 입력을 해석해 `New`를 보낸다. 새 세션이 목록에 나타나면 붙는다(`on_sessions`).
    fn submit_new(&mut self, text: &str) {
        match view::parse_new(text, self.m.project()) {
            Ok(spec) => self.create(spec),
            Err(e) => self.say(&format!("{e:#}")),
        }
    }

    fn create(&mut self, spec: NewSession) {
        self.pending = Some((spec.project.clone(), spec.name.clone()));
        self.send(ToDaemon::New { spec });
    }

    /// 시작 화면의 키. Esc는 세션 위에 연 시작 화면을 닫는다(붙은 세션이 없으면 돌아갈 곳이 없어 아무것도 안 한다).
    /// 줄을 고르는 키만 받고 나머지는 세션으로도 보내지 않고 버린다. `⌃a` 명령은 입력 해석기가 먼저 가져간다.
    fn launcher_keys(&mut self, bytes: &[u8]) {
        if bytes == b"\x1b" {
            if self.m.attached.is_some() {
                self.m.launcher = false;
                self.dirty = true;
            }
        } else if let Some(choice) = launcher::choice_of_key(bytes) {
            self.launch(choice);
        } else if String::from_utf8_lossy(bytes).chars().any(input::is_hangul) {
            self.hangul_hint();
        }
    }

    /// 한글이 명령 키 자리(시작 화면, 접두 뒤, 복사 모드)에 왔다. 동작하지 않고 영문으로 바꾸라고 알린다.
    /// 복사 모드는 상태줄 전체를 그 글이 쓰니 거기에 얹는다.
    fn hangul_hint(&mut self) {
        if let Some(copy) = &mut self.copy {
            copy.mode.hangul = true;
            self.sync_copy();
        } else {
            self.say(input::HANGUL_HINT);
        }
    }

    /// 고른 줄을 시작한다. 세션은 새 이름(`t<N>`)으로 바로 만들고, 붙는 것은 목록에 나타난 뒤(`on_sessions`)다.
    fn launch(&mut self, choice: Choice) {
        if choice == Choice::Typed {
            self.m.prompt = Some(Prompt::New(String::new()));
            self.dirty = true;
            return;
        }
        // 앞서 보낸 `New`의 답을 기다리는 동안은 받지 않는다. 같은 이름으로 또 보내 서로 엉키지 않게 한다.
        if self.pending.is_some() {
            return;
        }
        let project = self.m.project().to_path_buf();
        let name = launch::next_name(&self.m.sessions, &project);
        if let Some(spec) = launch::new_session(choice, &name, &project) {
            self.create(spec);
        }
    }

    fn on_resize(&mut self, size: Size) {
        self.area = Rect::from((Position::ORIGIN, size));
        self.resize_session();
    }

    /// 터미널 칸 크기가 바뀌었다(창 크기, 목록 접기). 화면과 세션 PTY를 맞춘다.
    fn resize_session(&mut self) {
        // 크기가 바뀌면 줄 바꿈이 달라져 얼려 둔 줄과 화면이 어긋난다.
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
        // 복사 모드 동안은 화면을 얼려 둔다. 끝내지 않은 동기화 갱신이 줄을 바꾸지 않게 한다.
        if self.copy.is_some() {
            return None;
        }
        self.screen.as_ref()?.sync_deadline()
    }

    /// 세션이 끝내지 않은 동기화 갱신(`?2026`)이 시간을 넘겼으면 끝내고 다시 그린다. 칸이 멈춘 채 남지 않게 한다.
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

    /// 한 화면을 그린다. 바깥 터미널이 동기화 갱신으로 한 번에 보여 주도록 감싼다.
    ///
    /// ponytail: ratatui는 OSC 8 하이퍼링크를 모른다. 그래서 ratatui가 그린 뒤, 이번 화면의 링크 구간을
    /// 막 그린 버퍼의 칸(같은 글자·색)으로 `ESC ] 8 ; ; URL` 안에서 한 번 더 쓴다. 터미널은 칸마다 링크를
    /// 기억하므로 ratatui가 다시 쓰지 않은 칸도 링크가 남는다. 지난 화면에만 있던 구간은 링크 없이 다시 써서,
    /// 글자가 같아 ratatui가 건너뛴 칸에 지난 링크가 남지 않게 한다. 커서는 DECSC/DECRC로 되돌린다.
    /// 링크가 많아 느려지면 바뀐 구간만 쓴다.
    fn draw<W: Write>(&mut self, terminal: &mut Terminal<CrosstermBackend<W>>) -> io::Result<()> {
        // 클립보드 OSC 52는 화면과 같은 쓰기 통로로, 그리는 도중이 아닌 프레임 사이에 내보낸다.
        if !self.clipboard.is_empty() {
            terminal.backend_mut().write_all(&self.clipboard)?;
            self.clipboard.clear();
        }
        queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let frame = terminal.draw(|f| {
            ui::draw(f, &self.m, self.screen.as_ref());
            let areas = ui::layout(f.area(), self.m.list_open);
            if let (Some(copy), Some(screen)) = (&self.copy, &self.screen) {
                // 복사 모드의 커서와 선택은 같은 반전으로 칠한다. 검색어를 입력하는 중이면 커서는 상태줄에 둔다.
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
        // 시작 화면이 세션 칸을 덮는 동안은 세션 화면의 링크를 걸지 않는다.
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
        // 반드시 위 DECRC(`\x1b8`) 뒤에 쓴다. 앞에 쓰면 DECRC가 커서를 되돌려 버린다.
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

/// 세션이 커서를 숨겼을 때 바깥 커서를 둘 자리. 없으면 `None`이다.
///
/// 한글 조합(IME preedit)은 바깥 터미널의 실제 커서 자리에 뜨고, 숨겨도 그렇다. Claude Code는 커서를 숨기고 자기 입력줄에
/// 두므로, 바깥 커서는 숨긴 채 그 자리로 옮겨야 조합이 입력줄에 뜬다. 보이는 커서는 `ui::term_cursor`가 맡는다.
/// 상태줄 입력(프롬프트·복사 모드 검색)은 상태줄 커서가 우선이고, 스크롤해 과거를 보는 중이나 시작 화면이 칸을 덮은 동안은
/// 칸 안으로 옮기지 않는다.
///
/// ponytail: 칸 안 판정은 `ui::term_cursor`와 같은 두 줄이다. ui.rs를 키우지 않으려고 그쪽을 고치지 않고 여기에 두었다.
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

/// 링크 구간이 덮는 칸을 막 그린 버퍼에서 꺼낸다. 넓은 글자가 가린 다음 칸은 뺀다.
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

    /// 바깥 터미널 120x40, 목록이 열려 터미널 칸은 (34, 0)에서 86x39다. 세션 `s1`에 붙어 있고 화면에 `feed`를 먹였다.
    fn app(feed: &[u8]) -> (App, UnboundedReceiver<ToDaemon>) {
        let (tx, rx) = unbounded_channel();
        let mut app = App::new(tx, PathBuf::from("/p"), Size::new(120, 40));
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

    /// 한 프레임을 그리고 바깥 터미널(120x40)에 쓴 바이트를 돌려준다. 크기를 묻지 않도록 고정 영역으로 연다.
    fn frame(app: &mut App) -> String {
        let mut out = Vec::new();
        let viewport = Viewport::Fixed(Rect::new(0, 0, 120, 40));
        let mut terminal = Terminal::with_options(
            CrosstermBackend::new(&mut out),
            TerminalOptions { viewport },
        )
        .unwrap();
        app.draw(&mut terminal).unwrap();
        // Terminal의 Drop은 숨긴 커서를 보이는 시퀀스를 쓴다. 프레임이 쓴 것만 보려고 Drop을 건너뛴다.
        std::mem::forget(terminal);
        String::from_utf8(out).unwrap()
    }

    /// 세션 커서(행 3, 열 5)를 숨긴다. 터미널 칸이 (34, 0)에서 시작하므로 바깥 화면에서는 행 3, 열 39(CUP `4;40`)다.
    const HIDDEN_AT_3_5: &[u8] = b"\x1b[?25l\x1b[4;6H";
    const MOVE: &str = "\x1b[4;40H";
    const SHOW: &str = "\x1b[?25h";

    #[test]
    fn ime_anchor_follows_the_hidden_cursor_inside_the_pane() {
        let (app, _rx) = app(HIDDEN_AT_3_5);
        let sc = app.screen.as_ref().unwrap();
        let term = ui::layout(app.area, app.m.list_open).term;
        assert_eq!(ime_anchor(&app.m, sc, term), Some(Position::new(39, 3)));
        // 칸 밖(칸보다 좁은 영역)이면 옮기지 않는다.
        assert_eq!(ime_anchor(&app.m, sc, Rect::new(34, 0, 5, 39)), None);
        // 붙은 세션이 없으면 옮기지 않는다.
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
        // 맨 아래로 돌아오고 나서 시작 화면(`⌃a c`)이 열리면 세션 칸이 가려지니 옮기지 않는다.
        app.on_action(Action::Pass(b"a".to_vec()));
        assert!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term).is_some());
        app.on_input(b"\x01c");
        assert!(app.m.launcher);
        assert_eq!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term), None);
        // 거기서 `n`으로 연 프롬프트는 상태줄 커서가 우선이다.
        app.on_input(b"n");
        assert!(app.m.prompt.is_some());
        assert_eq!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term), None);
        app.on_input(b"\x1b");
        app.on_input(b"\x1b");
        assert!(app.m.prompt.is_none() && !app.m.launcher);
        assert!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term).is_some());
        // 복사 모드는 상태줄을 쓰니 검색어를 치는 중이든 아니든 옮기지 않는다.
        enter_copy(&mut app);
        app.on_input(b"/ab");
        assert_eq!(ime_anchor(&app.m, app.screen.as_ref().unwrap(), term), None);
    }

    #[test]
    fn hidden_cursor_is_parked_at_its_spot_after_the_frame_and_never_shown() {
        let (mut app, _rx) = app(HIDDEN_AT_3_5);
        let out = frame(&mut app);
        // 옮기는 시퀀스는 프레임 끝, 동기화 갱신을 닫기 바로 앞에 쓴다.
        assert!(out.ends_with(&format!("{MOVE}\x1b[?2026l")), "{out:?}");
        assert!(!out.contains(SHOW), "{out:?}");
        assert!(out.contains("\x1b[?25l"));
    }

    #[test]
    fn visible_cursor_keeps_the_existing_set_cursor_path() {
        let (mut app, _rx) = app(b"\x1b[4;6H");
        let out = frame(&mut app);
        // ratatui가 커서를 보이고 그 자리에 놓는다. 프레임 끝은 그 둘과 동기화 갱신 닫기뿐이고, 따로 더 옮기지 않는다.
        // (칸에 무엇이 그려졌는지와 무관하게, 끝부분의 순서만 본다.)
        assert!(
            out.ends_with(&format!("{SHOW}{MOVE}\x1b[?2026l")),
            "{out:?}"
        );
    }

    #[test]
    fn park_move_comes_after_the_link_patch_restores_the_cursor() {
        // 첫 줄에 OSC 8 링크가 있고 세션 커서는 숨겨져 있다. 링크 패치는 DECSC/DECRC로 커서를 되돌린다.
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
        // 프롬프트를 열면 상태줄(맨 아래 줄)에 커서를 보이고, 칸 안으로는 옮기지 않는다.
        let (mut prompting, _rx) = app(HIDDEN_AT_3_5);
        prompting.on_input(b"\x01cn");
        let out = frame(&mut prompting);
        assert!(out.contains(SHOW) && !out.contains(MOVE), "{out:?}");
        // 시작 화면이 세션 칸을 덮는 동안은 커서를 보이지도 칸 안으로 옮기지도 않는다.
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
        // 지난 화면의 링크는 링크 없이 다시 쓰고, 새 링크는 걸지 않는다.
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
        // 터미널 칸 밖의 휠은 스크롤하지 않는다.
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
        // 휠은 넘겼을 뿐 클라이언트 화면은 스크롤하지 않고, 선택도 시작하지 않는다.
        assert_eq!(offset(&app), 0);
        assert!(app.select.selection().is_none());
        // 터미널 칸 밖은 넘기지 않는다.
        app.mouse(ev(MouseKind::Down, 10, 5));
        app.mouse(ev(MouseKind::WheelUp, 2, 5));
        assert!(sent(&mut rx).is_empty());
    }

    #[test]
    fn held_forwarded_press_gets_clamped_drag_and_release_outside_the_pane() {
        let (mut app, mut rx) = app(b"\x1b[?1000h\x1b[?1002h");
        // 누르기는 칸 안이다. 끌다가 목록 위로, 다시 상태줄 위로 벗어나 거기서 뗀다.
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
        // 뗌으로 누름이 끝났으니 칸 밖 끌기와 뗌은 다시 넘기지 않는다.
        app.mouse(ev(MouseKind::Drag, 10, 5));
        app.mouse(ev(MouseKind::Up, 10, 5));
        assert!(sent(&mut rx).is_empty());
        // 칸 밖에서 시작한 누름은 잡지 않는다.
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
        assert_eq!(app.m.message, "복사됨 5자");
        assert!(app.select.selection().is_some());
        // 복사 알림은 마우스로는 지워지지 않고, 키를 누르면 알림과 반전이 함께 사라진다.
        app.mouse(ev(MouseKind::WheelDown, 40, 5));
        assert_eq!(app.m.message, "복사됨 5자");
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
        // 목록 위로 벗어나도 선택은 이어지고 터미널 칸 왼쪽 끝으로 눌린다.
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
        // 끄는 도중에 온 출력은 선택을 끊지 않는다.
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

    /// `⌃a [`로 복사 모드에 들어간다.
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
        // l, s는 복사 모드 키고, 선택이 없는 Enter는 끝낸다. 어느 것도 세션에 가지 않는다.
        app.on_input(b"ls\r");
        assert!(app.copy.is_none());
        assert!(sent(&mut rx).is_empty());
        app.on_input(b"x");
        assert_eq!(sent(&mut rx), [input(b"x")]);
        // 화살표와 붙여넣기도 가지 않는다.
        enter_copy(&mut app);
        app.on_input(b"\x1b[A\x1bOB");
        app.on_action(Action::Paste(b"pasted".to_vec()));
        assert!(app.copy.is_some());
        assert!(sent(&mut rx).is_empty());
        // 단독 Esc가 끝낸다.
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
        // 세션이 마우스를 쓰고 있어도 얼린 화면이라 세션에는 아무것도 가지 않는다.
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
        // 터미널 칸 밖의 휠은 커서를 옮기지 않는다.
        app.mouse(ev(MouseKind::WheelUp, 10, 5));
        assert_eq!(copy_line(&app), 96);
        assert!(app.copy.is_some() && sent(&mut rx).is_empty());
        // 휠로 움직이면 "검색 결과 없음"이 걷힌다.
        app.on_input(b"/nothing\r");
        assert!(app.m.status.as_deref().unwrap().contains("검색 결과 없음"));
        app.mouse(ev(MouseKind::WheelDown, 40, 5));
        assert!(!app.m.status.as_deref().unwrap().contains("검색 결과 없음"));
    }

    #[test]
    fn bytes_after_the_key_that_ends_copy_mode_go_to_the_session() {
        let (mut app, mut rx) = app(&lines(100));
        enter_copy(&mut app);
        // q 뒤의 `ls\r`은 복사 모드 키가 아니라 세션 입력이다.
        app.on_input(b"qls\r");
        assert!(app.copy.is_none());
        assert_eq!(sent(&mut rx), [input(b"ls\r")]);
        // 복사하고 끝내는 y도 같다.
        enter_copy(&mut app);
        app.on_input(b"gvlyecho\r");
        assert!(app.copy.is_none());
        assert_eq!(app.clipboard, select::osc52("li"));
        assert_eq!(sent(&mut rx), [input(b"echo\r")]);
        // 선택 없는 Enter도 끝내는 키다.
        enter_copy(&mut app);
        app.on_input(b"\rx");
        assert_eq!(sent(&mut rx), [input(b"x")]);
        // q로 끝낸 뒤의 Esc는 그대로 세션으로 간다.
        enter_copy(&mut app);
        app.on_input(b"q\x1b");
        assert!(app.copy.is_none());
        assert_eq!(sent(&mut rx), [input(b"\x1b")]);
    }

    #[test]
    fn esc_that_ends_copy_mode_drops_the_bare_escs_right_after_it() {
        // Esc를 빠르게 두 번 눌러 나오는 사람이 많다. 세션(Claude Code)에 Esc가 가면 턴을 끊고 Esc Esc는 되감기를 연다.
        let (mut app, mut rx) = app(&lines(100));
        enter_copy(&mut app);
        app.on_input(b"\x1b\x1b");
        assert!(app.copy.is_none());
        assert!(sent(&mut rx).is_empty());
        // 그 뒤에 이어 오는 입력은 평소처럼 간다.
        enter_copy(&mut app);
        app.on_input(b"\x1b\x1bls\r");
        assert!(app.copy.is_none());
        assert_eq!(sent(&mut rx), [input(b"ls\r")]);
        // 세 번 와도 모두 뗀다.
        enter_copy(&mut app);
        app.on_input(b"\x1b\x1b\x1bx");
        assert_eq!(sent(&mut rx), [input(b"x")]);
        // 이스케이프 시퀀스의 시작(CSI)은 단독 Esc가 아니라서 남긴다.
        enter_copy(&mut app);
        app.on_input(b"\x1b\x1b\x1b[A");
        assert_eq!(sent(&mut rx), [input(b"\x1b[A")]);
        // 검색어를 입력하는 중의 Esc 둘: 첫째는 입력을 접고 둘째가 끝낸다. 보내는 것은 없다.
        enter_copy(&mut app);
        app.on_input(b"/ab\x1b\x1b");
        assert!(app.copy.is_none());
        assert!(sent(&mut rx).is_empty());
        // q·y·Enter로 끝낸 뒤는 Esc도 그대로 간다.
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
        // 첫 Esc가 복사 모드를 끝내고 시각을 적어 둔다.
        app.on_input(b"\x1b");
        assert!(app.copy.is_none() && app.esc_exit.is_some());
        assert!(sent(&mut rx).is_empty());
        // 둘째 Esc는 따로 읽혀도 창 안이라 세션에 가지 않는다. 연달아 와도 같다.
        app.esc_exit = Some(Instant::now());
        app.on_input(b"\x1b");
        app.on_input(b"\x1b");
        assert!(sent(&mut rx).is_empty());
        // Esc 뒤에 글자가 이어지면 글자는 가고, 그 뒤로는 창이 닫힌다.
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
        // 창이 지났다(시각을 되돌려 흉내 낸다).
        app.esc_exit = Some(Instant::now() - 2 * ESC_GRACE);
        app.on_input(b"\x1b");
        assert_eq!(sent(&mut rx), [input(b"\x1b")]);
        assert!(app.esc_exit.is_none());
        // q·y·Enter로 끝낸 뒤에는 창이 없다.
        enter_copy(&mut app);
        app.on_input(b"q");
        assert!(app.esc_exit.is_none());
        app.on_input(b"\x1b");
        assert_eq!(sent(&mut rx), [input(b"\x1b")]);
        // 복사 모드에 다시 들어가면 창은 닫힌다.
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
        // 세션이 kitty 키보드를 켜면 둘째 Esc는 `CSI 27 u`로 온다. 수정 키 없는 누름 세 꼴을 모두 뗀다.
        for esc in [&b"\x1b[27u"[..], b"\x1b[27;1u", b"\x1b[27;1:1u"] {
            app.on_input(esc);
        }
        assert!(sent(&mut rx).is_empty());
        // 수정 키가 붙은 것(⌃Esc)은 남긴다.
        app.on_input(b"\x1b[27;5u");
        assert_eq!(sent(&mut rx), [input(b"\x1b[27;5u")]);
        assert!(app.esc_exit.is_none());
        // 창이 지난 뒤에는 그대로 간다.
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
        // `⌃a n`은 명령이다. 세션이 하나도 없어 옮겨 가지는 않지만 창은 닫힌다.
        app.on_input(b"\x01n");
        assert!(app.esc_exit.is_none());
        app.on_input(b"\x1b");
        assert_eq!(sent(&mut rx), [input(b"\x1b")]);
        // 세션에 붙어도 닫힌다.
        enter_copy(&mut app);
        app.on_input(b"\x1b");
        assert!(app.esc_exit.is_some());
        app.attach("s1".into());
        assert!(app.esc_exit.is_none());
    }

    #[test]
    fn paging_moves_the_cursor_and_the_view_by_a_page() {
        // 39줄 화면에 스크롤백 62줄이다. 한 쪽은 39줄이다.
        let (mut app, _rx) = app(&lines(100));
        enter_copy(&mut app);
        let at = |app: &App| (copy_line(app), offset(app));
        assert_eq!(at(&app), (99, 0));
        app.on_input(b"\x1b[5~");
        assert_eq!(at(&app), (60, 39));
        app.on_input(b"\x1b[6~");
        assert_eq!(at(&app), (99, 0));
        // 반 쪽은 19줄이다.
        app.on_input(b"\x15");
        assert_eq!(at(&app), (80, 19));
        app.on_input(b"\x04");
        assert_eq!(at(&app), (99, 0));
        // 끝에서는 멈춘다. ⌃b·⌃f도 같은 쪽 넘김이다.
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
        // 줄 번호가 흔들리지 않게 화면은 그대로고, 출력은 쌓아 둔다.
        assert_eq!(history(&app), frozen);
        assert_eq!(app.copy.as_ref().unwrap().held, b"\r\nlater");
        // 다른 세션의 출력은 쌓지 않는다.
        app.on_msg(FromDaemon::Output {
            id: "s2".into(),
            data: b"other".to_vec(),
        });
        assert_eq!(app.copy.as_ref().unwrap().held, b"\r\nlater");
        app.on_input(b"q");
        assert_eq!(app.screen.as_ref().unwrap().lines()[1], "later");
        assert_eq!(offset(&app), 0);
        assert!(app.copy.is_none() && app.m.status.is_none());
        // 나온 뒤의 출력은 바로 화면으로 간다.
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
        assert_eq!(app.m.message, "복사됨 8자");
        assert!(app.copy.is_none() && app.m.status.is_none());
        assert!(sent(&mut rx).is_empty());
        // 선택 없이 y는 복사하지 않고 끝낸다.
        app.clipboard.clear();
        app.m.message.clear();
        enter_copy(&mut app);
        app.on_input(b"y");
        assert!(app.copy.is_none() && app.clipboard.is_empty() && app.m.message.is_empty());
    }

    #[test]
    fn status_and_view_follow_the_cursor() {
        // 100줄을 쓴 39줄 화면: 스크롤백 62줄 + 보이는 39줄에 line99가 마지막 글자 줄이다.
        let (mut app, _rx) = app(&lines(100));
        enter_copy(&mut app);
        assert_eq!(
            app.m.status.as_deref(),
            Some("복사 모드 │ 100/101 │ v 선택 · y 복사 · / ? 검색 · q 끝")
        );
        assert_eq!(offset(&app), 0);
        app.on_input(b"g");
        assert_eq!(offset(&app), 62);
        assert!(app.m.status.as_deref().unwrap().contains("│ 1/101 │"));
        // 검색어는 상태줄에 보이고, 찾으면 그 줄이 보이게 스크롤한다.
        app.on_input(b"/line5");
        assert_eq!(app.m.status.as_deref(), Some("/line5"));
        app.on_input(b"\r");
        assert_eq!(app.copy.as_ref().unwrap().mode.line, 5);
        assert!(app.m.status.as_deref().unwrap().contains("│ 6/101 │"));
        assert_eq!(offset(&app), 62);
        app.on_input(b"G");
        assert_eq!(offset(&app), 0);
        // 못 찾으면 그 자리에서 알린다.
        app.on_input(b"/nothing\r");
        assert!(app.m.status.as_deref().unwrap().contains("검색 결과 없음"));
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

        // 다른 접두 명령은 복사 모드를 끝내고 실행된다. 복사 모드 안의 `⌃a [`는 아무것도 바꾸지 않는다.
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
        // 아이콘 줄 첫 항목(∗)을 누르면 프로젝트를 고르고, 붙은 세션이 사라지니 복사 모드도 끝난다.
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
        assert_eq!(app.m.message, "복사 모드 종료: 출력이 너무 많다");
        // 넘친 덩이까지 화면에 먹였고 맨 아래로 돌아와 있다.
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

    /// 바깥 터미널에 쓸 키 인코딩 모드 바이트(지난번과 달라진 것만).
    fn keys(app: &mut App) -> String {
        String::from_utf8(app.sync_keys()).unwrap()
    }

    /// 세션이 켠 키 인코딩 모드(DECCKM, modifyOtherKeys 2, kitty 1)와, 그것을 바깥에 걸고 푸는 바이트.
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
        // kitty 플래그 2(이벤트 종류)는 걸지 않는다. 2만 달라지면 쓸 것이 없다.
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
        // 이미 건 모드는 다시 쓰지 않는다.
        assert_eq!(keys(&mut app), "");
        app.on_msg(output("s1", b"\x1b[?1h\x1b=\x1b[>4;2m"));
        assert_eq!(keys(&mut app), "\x1b[?1h\x1b=\x1b[>4;2m");
        // 다른 세션의 출력은 따르지 않는다.
        app.on_msg(output("s2", b"\x1b[<u\x1b[>4m"));
        assert_eq!(keys(&mut app), "");
        // 앱이 끄면 따라 끈다.
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
        // 세션이 kitty를 켰으니 `⌃a`는 인코딩되어 온다.
        app.on_input(b"\x1b[97;5u[");
        assert!(app.copy.is_some());
        assert_eq!(keys(&mut app), RESET);
        app.on_input(b"q");
        assert!(app.copy.is_none());
        assert_eq!(keys(&mut app), APPLY);
        // `⌃a c` 시작 화면도 같다. 세션 칸을 가리니 키는 평범하게 와야 한다.
        app.on_input(b"\x1b[97;5uc");
        assert!(app.m.launcher);
        assert_eq!(keys(&mut app), RESET);
        // 거기서 `n`으로 연 상태줄 입력도 같고, Esc는 한 단계씩 되돌린다.
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
    fn frozen_screen_does_not_end_a_stale_sync_update() {
        // 동기화 갱신(`?2026`) 중이라 `buffered`는 아직 화면에 반영되지 않았다.
        let (mut app, _rx) = app(b"hello\x1b[?2026hbuffered");
        assert!(app.sync_deadline().is_some());
        enter_copy(&mut app);
        assert!(app.sync_deadline().is_none());
        std::thread::sleep(Duration::from_millis(300));
        app.end_stale_sync();
        assert_eq!(history(&app)[0], "hello");
        // 나온 뒤에는 시간이 넘은 갱신을 끝내 화면에 반영한다.
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

    /// 프로젝트 `/p`에 세션이 하나도 없다는 첫 목록을 받은 클라이언트.
    fn empty_app() -> (App, UnboundedReceiver<ToDaemon>) {
        let (tx, rx) = unbounded_channel();
        let mut app = App::new(tx, PathBuf::from("/p"), Size::new(120, 40));
        app.on_msg(FromDaemon::Sessions { sessions: vec![] });
        (app, rx)
    }

    /// 프로젝트 `/p`의 세션이 `names`인 첫 목록을 받은 클라이언트. 첫 줄 세션에 붙고, 그때 보낸 `Attach`는 비워 둔다.
    fn started_app(names: &[&str]) -> (App, UnboundedReceiver<ToDaemon>) {
        let (tx, mut rx) = unbounded_channel();
        let mut app = App::new(tx, PathBuf::from("/p"), Size::new(120, 40));
        app.on_msg(FromDaemon::Sessions {
            sessions: names.iter().map(|n| shell(n)).collect(),
        });
        sent(&mut rx);
        (app, rx)
    }

    /// 시작 화면이 `/p`에 만들라고 보내는 `New`.
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
        // 만든 세션이 목록에 나타나면 평소처럼 붙고 시작 화면은 닫힌다.
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
    fn b_x_s_start_the_other_kinds_with_the_next_free_name() {
        // t1과 t3이 있으니 새 이름은 빈 자리 t2다. `⌃a c`로 붙은 세션 위에 시작 화면을 연다.
        for (key, kind, account) in [
            (b"b", Kind::Claude, Some("claude-b")),
            (b"x", Kind::Codex, Some("codex")),
            (b"s", Kind::Shell, None),
        ] {
            let (mut app, mut rx) = started_app(&["t1", "t3"]);
            assert_eq!(app.m.attached.as_deref(), Some("p-t1"));
            app.on_input(b"\x01c");
            assert!(app.m.launcher);
            app.on_input(key);
            assert_eq!(sent(&mut rx), [new_session(kind, account, "t2")], "{key:?}");
        }
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
        // 데몬이 거절하면 다시 고를 수 있다.
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
        // `⌃a b`는 목록을 접고, `⌃a d`는 나간다.
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
        // 세션 칸을 가리는 동안 키와 붙여넣기는 세션에 가지 않는다.
        app.on_input(b"a");
        app.on_action(Action::Paste(b"p".to_vec()));
        assert!(sent(&mut rx).is_empty());
        app.on_input(b"\x1b");
        assert!(!app.m.launcher);
        assert_eq!(app.m.attached.as_deref(), Some("s1"));
        // 닫은 뒤에는 키가 다시 세션으로 간다. 세션 화면도 그대로다.
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
        // 이름과 모델을 직접 입력해 만든다.
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
        // 터미널 칸은 (34, 0)에서 86x39다. 메뉴는 폭 22, 열한 줄이라 (66, 14)에서 시작하고 선택 줄은 네 줄 아래부터다.
        let (left, top) = (34 + (86 - 22) / 2, (39 - 11) / 2);
        app.mouse(ev(MouseKind::Down, left + 3, top + 5));
        assert_eq!(
            sent(&mut rx),
            [new_session(Kind::Claude, Some("claude-b"), "t1")]
        );
        app.on_msg(FromDaemon::Error {
            message: "no".into(),
        });
        // 줄 밖, 터미널 칸의 다른 곳, 휠은 아무것도 하지 않는다.
        app.mouse(ev(MouseKind::Down, left - 2, top + 5));
        app.mouse(ev(MouseKind::Down, left + 3, top));
        app.mouse(ev(MouseKind::WheelUp, left + 3, top + 5));
        app.mouse(MouseEv {
            button: 2,
            ..ev(MouseKind::Down, left + 3, top + 5)
        });
        assert!(sent(&mut rx).is_empty());
        // 줄의 `n`은 프롬프트를 연다.
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
        // 클릭으로 시작한 새 세션은 붙으면서 시작 화면을 닫는다.
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
        // 붙은 세션이 사라졌는데 다른 세션이 없으면 시작 화면이 다시 뜬다.
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
        assert_eq!(app.m.message, "복사할 세션 화면이 없다");
    }

    #[test]
    fn launcher_state_is_drawn_into_the_frame() {
        let (mut app, _rx) = empty_app();
        // 넓은 글자는 칸마다 커서를 옮겨 쓰므로 ASCII 글자로 본다.
        let out = frame(&mut app);
        assert!(out.contains("claude-b") && out.contains("shell"), "{out:?}");
        // 시작 화면의 상태줄 안내.
        assert!(
            out.contains("↵ claude-a · b claude-b · x codex · s shell"),
            "{out:?}"
        );
        // `n`으로 상태줄 입력을 열면 상태줄에 커서가 보인다.
        app.on_input(b"n");
        assert!(frame(&mut app).contains("\x1b[?25h"));
    }

    #[test]
    fn hangul_in_the_launcher_is_not_acted_on_and_says_to_switch_to_english() {
        let (mut app, mut rx) = empty_app();
        // `s`의 한글 자모(ㄴ), 음절, 여러 글자.
        for key in ["ㄴ", "한", "한글"] {
            app.m.message.clear();
            app.on_input(key.as_bytes());
            assert_eq!(app.m.message, input::HANGUL_HINT, "{key}");
        }
        assert!(sent(&mut rx).is_empty());
        assert!(app.m.launcher && app.m.prompt.is_none() && app.pending.is_none());
        // 영문 키를 누르면 평소대로 시작한다.
        app.on_input(b"s");
        assert_eq!(sent(&mut rx), [new_session(Kind::Shell, None, "t1")]);
        // 한글 아닌 키는 안내 없이 무시한다.
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
        // 세션에는 그대로 간다.
        let (mut attached, mut rx) = app(b"");
        attached.on_input("한글ㅊ".as_bytes());
        assert_eq!(sent(&mut rx), [input("한글ㅊ".as_bytes())]);
        assert!(attached.m.message.is_empty() && !attached.m.launcher);
    }

    #[test]
    fn hangul_after_the_prefix_is_dropped_with_a_hint_and_never_reaches_the_session() {
        let (mut app, mut rx) = app(b"");
        // `⌃a c`를 한글 입력 상태에서 누른 꼴이다(ㅊ).
        app.on_input(b"\x01");
        app.on_input("ㅊ".as_bytes());
        assert_eq!(app.m.message, input::HANGUL_HINT);
        assert!(sent(&mut rx).is_empty());
        assert!(!app.m.launcher && app.m.prompt.is_none());
        // 접두는 풀렸으니 다음 키는 세션으로 간다. 영문으로 바꿔 누르면 명령이 된다.
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
        // `k`(ㅏ)가 위로 가지 않고 상태줄이 안내한다.
        app.on_input("ㅏ".as_bytes());
        assert_eq!(copy_line(&app), at);
        let status = app.m.status.clone().unwrap();
        assert!(status.contains(input::HANGUL_HINT), "{status}");
        assert!(status.starts_with("복사 모드 │"));
        // 영문 키를 누르면 안내가 걷히고 평소처럼 움직인다.
        app.on_input(b"k");
        assert_eq!(copy_line(&app), at - 1);
        assert!(!app.m.status.as_deref().unwrap().contains("한/영"));
        // 한글 음절도 같고, 휠로도 걷힌다.
        app.on_input("한".as_bytes());
        assert!(
            app.m
                .status
                .as_deref()
                .unwrap()
                .contains(input::HANGUL_HINT)
        );
        app.mouse(ev(MouseKind::WheelUp, 40, 5));
        assert!(!app.m.status.as_deref().unwrap().contains("한/영"));
        // 접두 뒤의 한글도 복사 모드 상태줄에 알린다. 복사 모드는 끝나지 않는다.
        app.on_input("\x01ㅊ".as_bytes());
        assert!(
            app.m
                .status
                .as_deref()
                .unwrap()
                .contains(input::HANGUL_HINT)
        );
        assert!(app.copy.is_some());
        // 검색어를 입력하는 중의 한글은 검색어다.
        app.on_input(b"/");
        app.on_input("한글".as_bytes());
        assert_eq!(app.m.status.as_deref(), Some("/한글"));
        assert!(!app.copy.as_ref().unwrap().mode.hangul);
        // 한 번에 온 `/한글`도 검색어다. 앞의 한글(`ㅏ/`)은 명령 자리라 알린다.
        app.on_input(b"\r");
        app.on_input("/한".as_bytes());
        assert_eq!(app.m.status.as_deref(), Some("/한"));
        assert!(sent(&mut rx).is_empty());
    }
}
