//! 클라이언트 화면(명령 없이 `mine`). 바깥 터미널을 잡고 데몬에 붙어, 고른 세션 화면을 그리고 입력을 넘긴다.
//!
//! 이 작업 하나가 상태를 모두 쥐고 표준 입력(스레드)·데몬 메시지(읽기 작업)·SIGWINCH·다시 그릴 때를
//! `select!` 하나로 기다린다. `select!`에는 취소에 안전한 채널 수신과 타이머만 넣는다.

pub mod input;
mod tty;
pub mod ui;
pub mod view;

use std::io::{self, BufWriter, Stdout, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use anyhow::{Result, bail};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::{Buffer, Cell};
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::layout::{Position, Rect, Size};
use ratatui::text::Span;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::UnboundedSender;

use crate::conn::Conn;
use crate::paths::State;
use crate::proto::{FromDaemon, ToDaemon};
use crate::screen::Screen;
use crate::session::SessionInfo;
use input::{Action, Cmd, InputParser, MouseEv, MouseKind};
use ui::{Hit, LinkRun, Model, Prompt};

/// 다시 그리는 최소 간격(초당 60번).
const FRAME: Duration = Duration::from_micros(16_667);

type Term = Terminal<CrosstermBackend<BufWriter<Stdout>>>;
/// 바깥 화면 좌표와 그 칸.
type Cells = Vec<(u16, u16, Cell)>;

/// 클라이언트를 실행한다. `⌃a d`로 나오면 `Ok`다. 데몬과 세션은 계속 돈다.
pub async fn run(state: State, project: PathBuf) -> Result<()> {
    let _tty = tty::Tty::enter()?;
    // 표준 입력 읽기 스레드보다 먼저 묻고 끝낸다(R13). 그사이 들어온 키는 아래에서 먼저 먹인다.
    let (colors, early) = tty::probe_colors();
    let conn = Conn::connect_or_spawn_async(&state).await?;
    let mut stdin = tty::spawn_stdin_reader();
    let mut winch = signal(SignalKind::window_change())?;
    let mut terminal = Terminal::new(CrosstermBackend::new(BufWriter::with_capacity(
        1 << 16,
        io::stdout(),
    )))?;
    let mut rx = conn.rx;
    let mut app = App::new(conn.tx, project, terminal.size()?);
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
    while !app.quit {
        app.end_stale_sync();
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
                None => bail!("데몬 연결이 끊겼다"),
            },
            _ = winch.recv() => app.on_resize(terminal.size()?),
            () = tokio::time::sleep_until(wake.into()) => {}
        }
    }
    Ok(())
}

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
            },
            screen: None,
            tx,
            parser: InputParser::new(),
            area: Rect::from((Position::ORIGIN, size)),
            started: false,
            pending: None,
            links: Vec::new(),
            dirty: true,
            quit: false,
        }
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
                let (cols, rows) = self.term_size();
                let mut screen = Screen::new(cols, rows);
                screen.feed(&data);
                screen.take_replies();
                self.screen = Some(screen);
                self.dirty = true;
            }
            FromDaemon::Output { id, data } if attached(&id) => {
                if let Some(screen) = &mut self.screen {
                    screen.feed(&data);
                    screen.take_replies();
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
            None => {
                self.detach();
                self.say("세션이 없다 · ⌃a c로 새로 만든다");
            }
        }
    }

    fn attach(&mut self, id: String) {
        let (cols, rows) = self.term_size();
        self.send(ToDaemon::Attach {
            id: id.clone(),
            cols,
            rows,
        });
        if self.m.attached.as_ref() != Some(&id) {
            self.screen = None;
        }
        self.m.attached = Some(id);
        self.dirty = true;
    }

    /// 붙은 세션을 놓는다. 데몬에는 떨어지는 메시지가 없어 그 세션 출력이 계속 오지만 버린다.
    fn detach(&mut self) {
        self.m.attached = None;
        self.screen = None;
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
        if !matches!(action, Action::Mouse(_)) && !self.m.message.is_empty() {
            self.m.message.clear();
            self.dirty = true;
        }
        match action {
            Action::Pass(bytes) if self.m.prompt.is_some() => self.prompt_keys(&bytes),
            Action::Pass(data) => self.input(data),
            Action::Paste(text) if self.m.prompt.is_some() => self.prompt_paste(&text),
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
            Action::Cmd(cmd) => {
                if self.m.prompt.take().is_some() {
                    self.dirty = true;
                }
                self.command(cmd);
            }
            Action::Mouse(ev) => self.mouse(ev),
        }
    }

    fn input(&self, data: Vec<u8>) {
        if let Some(id) = &self.m.attached {
            self.send(ToDaemon::Input {
                id: id.clone(),
                data,
            });
        }
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::NextSession => self.step_session(1),
            Cmd::PrevSession => self.step_session(-1),
            Cmd::NextProject => self.step_project(1),
            Cmd::PrevProject => self.step_project(-1),
            Cmd::AllProjects => self.set_filter(None),
            Cmd::NewSession => {
                self.m.prompt = Some(Prompt::New(String::new()));
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
            // 복사 모드는 Task 11에서 한다.
            Cmd::CopyMode => {}
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
        if ev.kind != MouseKind::Down || ev.button != 0 || self.m.prompt.is_some() {
            return;
        }
        let areas = ui::layout(self.area, self.m.list_open);
        match ui::hit(&areas, &self.m, ev.col, ev.row) {
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
            // 터미널 칸의 마우스(스크롤·선택·앱에 넘기기)는 Task 10에서 한다.
            Hit::Term(..) | Hit::None => {}
        }
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

    /// `⌃a c` 입력을 해석해 `New`를 보낸다. 새 세션이 목록에 나타나면 붙는다(`on_sessions`).
    fn submit_new(&mut self, text: &str) {
        let project = self
            .m
            .filter
            .clone()
            .unwrap_or_else(|| self.m.current_project.clone());
        match view::parse_new(text, &project) {
            Ok(spec) => {
                self.pending = Some((spec.project.clone(), spec.name.clone()));
                self.send(ToDaemon::New { spec });
            }
            Err(e) => self.say(&format!("{e:#}")),
        }
    }

    fn on_resize(&mut self, size: Size) {
        self.area = Rect::from((Position::ORIGIN, size));
        self.resize_session();
    }

    /// 터미널 칸 크기가 바뀌었다(창 크기, 목록 접기). 화면과 세션 PTY를 맞춘다.
    fn resize_session(&mut self) {
        let (cols, rows) = self.term_size();
        if let Some(screen) = &mut self.screen {
            screen.resize(cols, rows);
        }
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
        self.screen.as_ref()?.sync_deadline()
    }

    /// 세션이 끝내지 않은 동기화 갱신(`?2026`)이 시간을 넘겼으면 끝내고 다시 그린다. 칸이 멈춘 채 남지 않게 한다.
    fn end_stale_sync(&mut self) {
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
    fn draw(&mut self, terminal: &mut Term) -> io::Result<()> {
        queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let frame = terminal.draw(|f| ui::draw(f, &self.m, self.screen.as_ref()))?;
        let term = ui::layout(frame.area, self.m.list_open).term;
        let runs = self
            .screen
            .as_ref()
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
        queue!(backend, EndSynchronizedUpdate)?;
        Write::flush(backend)?;
        self.links = runs;
        self.dirty = false;
        Ok(())
    }
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
