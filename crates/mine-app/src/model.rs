//! 앱 상태. GPUI 없이 데몬 연결의 일(`LinkEvent`)과 사용자 동작을 받아 상태를 바꾸고, 데몬에 보낼 메시지(`Effect`)를
//! 돌려준다.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mine_core::accounts::Labels;
use mine_core::launch::{self, Choice};
use mine_core::link::LinkEvent;
use mine_core::paths::State;
use mine_core::proto::{FromDaemon, ToDaemon};
use mine_core::screen::Screen;
use mine_core::session::{NewSession, SessionInfo, SessionState};
use mine_core::view::{self, Group, STATES};

use crate::theme;

/// 모델이 바깥에 시키는 일.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Send(ToDaemon),
}

/// ⌘W가 할 일. [`Model::close`]가 정한다.
#[derive(Clone, Debug, PartialEq)]
pub enum Close {
    /// 붙은 세션 위에 연 실행 화면을 닫았다.
    Launcher,
    /// 붙은 세션을 닫을지 묻는다. 값은 확인 창에 띄울 세션 이름이다. 답은 [`Model::confirm_close`]로 받는다.
    Confirm(String),
    /// 닫을 세션이 없다. 창을 닫는다(앱은 Dock에 남는다).
    Window,
    /// 확인 창이 이미 떠 있다.
    Nothing,
}

/// `New`를 보내고 기다리는 세션. 목록에 (`project`, `name`)이 나타나면 붙는다.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub project: PathBuf,
    pub name: String,
    /// `New`를 보낸 때.
    pub since: Instant,
}

/// `New`의 답을 이만큼 기다린다. 넘으면 버리고 [`START_FAILED`]를 띄운다.
pub const PENDING_TIMEOUT: Duration = Duration::from_secs(5);
/// 만든 세션이 목록에 나타나지 않았다(셸이 곧바로 끝났거나 답이 오지 않았다).
const START_FAILED: &str = "세션을 시작하지 못했어요";
/// 앞서 보낸 `New`의 답을 기다리는 동안 다시 고르면 띄운다.
const CREATING: &str = "만드는 중이에요";

/// 연결이 없을 때 실행 화면에서 고르면 띄우는 글. 연결 스레드는 연결 전에 보낸 메시지를 버린다.
const NOT_CONNECTED: &str =
    "데몬에 연결되지 않아 세션을 만들 수 없어요. 다시 연결되면 골라 주세요.";

/// 별칭 설정(`config.toml` 내용)으로 만든 이름들. 화면 문자열 테스트가 쓴다. 홈이 없으니 자동 이름은 codex뿐이다.
#[cfg(test)]
pub(crate) fn labels_from(config: &str) -> Labels {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config).unwrap();
    let state = State {
        root: dir.path().to_path_buf(),
    };
    Labels::load(&state, None)
}

pub struct Model {
    pub sessions: Vec<SessionInfo>,
    /// 고른 프로젝트. `None`이면 전체다.
    pub filter: Option<PathBuf>,
    /// 붙은 세션 id.
    pub attached: Option<String>,
    /// 사용자가 머리를 눌러 기본(대기만 접힘)과 반대로 접거나 편 묶음. 값은 [`STATES`] 인덱스다(◆ 0, ◐ 1, ✓ 2, ○ 3).
    pub toggled: HashSet<usize>,
    pub sidebar_open: bool,
    pub connected: bool,
    /// 마지막으로 끊긴 사유. 다시 붙으면 지운다. 있는 동안 목록 위에 끊김 띠를 띄운다.
    pub offline: Option<String>,
    /// 앱을 띄운 프로젝트. 세션이 없어도 프로젝트 고르기에 있다.
    pub current_project: PathBuf,
    /// `current_project`는 Dock에서 띄워(폴더가 `/`) 홈 폴더로 대신한 값이다. 홈은 프로젝트로 등록하지 않으니
    /// 프로젝트 고르기에도 넣지 않는다.
    pub home_fallback: bool,
    /// 등록된 프로젝트(`mine_core::project::registered`).
    pub registered: Vec<PathBuf>,
    /// 붙은 세션 위에 실행 화면을 열었다(⌘N, 탭의 ＋, 새 작업). 붙은 세션이 없으면 이 값과 상관없이 열려 있다.
    pub launcher: bool,
    /// 실행 화면의 `n` 입력 상자가 열려 있다.
    pub typing: bool,
    /// `New`를 보내고 답을 기다리는 세션. 목록에 나타나면 붙는다.
    pub pending: Option<Pending>,
    /// 닫기 확인 창을 띄운 세션 id.
    pub closing: Option<String>,
    /// 데몬이 보낸 오류나 입력 오류. 가운데 아래에 띄우고 다음 동작에서 지운다.
    pub message: Option<String>,
    /// 터미널 칸 크기(열, 행). 칸이 처음 그려질 때 안다([`Model::set_grid`]). `Attach`와 `Resize`가 싣는다.
    pub grid: Option<(u16, u16)>,
    /// 붙은 세션의 화면. `Snapshot`이 오면 칸 크기로 새로 만들어 먹이고, `Output`은 이어 먹인다. 붙은 세션이
    /// 바뀌면 버리고 새 `Snapshot`을 기다린다.
    pub screen: Option<Screen>,
    /// 붙은 세션의 `Attach`를 이번 연결에서 보냈다. 칸 크기를 모르거나 연결이 없어 미룬 동안은 `false`다.
    attach_sent: bool,
    /// 처음 받은 목록으로 붙을 세션을 정했다.
    started: bool,
    /// 계정 id를 글로 보일 때 쓰는 이름. 앱이 뜰 때 읽고 창이 다시 활성화될 때 다시 읽는다([`Model::reload_labels`]).
    pub labels: Labels,
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
            attach_sent: false,
            started: false,
            labels: Labels::default(),
        }
    }

    /// 계정 이름을 다시 읽는다. 바뀌었으면 `true`다(다시 그려야 한다).
    ///
    /// ponytail: 파일 감시는 하지 않는다. 계정을 바꾸거나 별칭을 고치고 창으로 돌아오면 읽는다.
    pub fn reload_labels(&mut self, state: &State, home: Option<&Path>) -> bool {
        let labels = Labels::load(state, home);
        let changed = labels != self.labels;
        self.labels = labels;
        changed
    }

    /// 데몬 연결의 일을 받는다.
    ///
    /// 붙을 때마다(다시 붙을 때도) 목록을 구독하고(R18) 터미널 색을 알린 뒤, 붙어 있던 세션에 다시 붙는다.
    /// `Hello`는 연결이 이미 주고받았다. 끊겨도 세션 목록과 화면은 그대로 두고, 다시 붙어 새 목록과 `Snapshot`이
    /// 오면 바꾼다.
    pub fn on_link(&mut self, ev: LinkEvent) -> Vec<Effect> {
        match ev {
            LinkEvent::Connected => {
                self.connected = true;
                self.offline = None;
                let mut effects = vec![
                    Effect::Send(ToDaemon::List),
                    Effect::Send(ToDaemon::Colors {
                        fg: theme::hex(theme::TERM_FG),
                        bg: theme::hex(theme::TERM_BG),
                    }),
                ];
                effects.extend(self.send_attach());
                effects
            }
            LinkEvent::Disconnected(reason) => {
                self.connected = false;
                self.offline = Some(reason);
                // 새 데몬은 이 연결이 어느 세션에 붙었는지 모른다. 다시 붙을 때 `Attach`를 새로 보낸다.
                self.attach_sent = false;
                // 보낸 `New`가 끊기기 전에 데몬에 닿았는지 알 수 없다. 답을 기다리지 않고 다시 고를 수 있게 한다.
                // 닿았으면 다시 붙은 뒤 오는 목록에 그 세션이 보인다.
                self.pending = None;
                Vec::new()
            }
            LinkEvent::Msg(FromDaemon::Sessions { sessions }) => {
                // 데몬은 목록이 바뀌었을 때만 알린다(publish). 바뀌지 않은 목록은 이 앱이 한 요청의 답이다.
                let unchanged = sessions == self.sessions;
                self.sessions = sessions;
                self.on_sessions(unchanged)
            }
            LinkEvent::Msg(FromDaemon::Error { message }) => {
                self.pending = None;
                self.message = Some(message);
                Vec::new()
            }
            LinkEvent::Msg(FromDaemon::Snapshot { id, data }) if self.is_attached(&id) => {
                // 쌓아 둔 출력도 스냅숏에 들어 있다. 앞 화면은 버린다.
                if let Some((cols, rows)) = self.grid {
                    let mut screen = Screen::new(cols, rows);
                    screen.feed(&data);
                    screen.take_replies();
                    self.screen = Some(screen);
                }
                Vec::new()
            }
            LinkEvent::Msg(FromDaemon::Output { id, data }) if self.is_attached(&id) => {
                if let Some(screen) = &mut self.screen {
                    screen.feed(&data);
                    // 세션의 질의(DA1, 커서 위치, 색)에는 데몬의 화면이 답한다. 같은 질의에 두 번 답하지 않게 버린다.
                    screen.take_replies();
                }
                Vec::new()
            }
            // 버전은 연결이 확인했다. 다른 세션의 화면은 붙은 세션을 바꾸기 전에 보낸 것이라 버린다.
            LinkEvent::Msg(
                FromDaemon::Hello { .. } | FromDaemon::Snapshot { .. } | FromDaemon::Output { .. },
            ) => Vec::new(),
        }
    }

    fn is_attached(&self, id: &str) -> bool {
        self.attached.as_deref() == Some(id)
    }

    /// 새 목록을 받았다. 기다리던 새 세션이 나타나면 붙는다. 처음 받은 목록이거나 붙은 세션이 사라졌으면 보이는 첫
    /// 세션에 붙고, 없으면 실행 화면이 뜬다.
    ///
    /// 기다리던 세션 없이 바뀌지 않은 목록이 오면 그것이 `New`의 답이다. 셸이 곧바로 끝나면 데몬은 세션을 남기지 않고
    /// 성공으로 답한다. 그 기다림을 버리고 알린다. 바뀐 목록은 만드는 사이 다른 세션 때문에 온 것일 수 있어 계속
    /// 기다린다([`PENDING_TIMEOUT`]이 받쳐 준다).
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
        Vec::new()
    }

    /// 보이는 첫 세션에 붙는다. 보이는 줄이 없으면(대기 묶음은 처음에 접혀 있다) 접힌 묶음까지 본다.
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

    /// id로 찾은 세션.
    pub fn session(&self, id: &str) -> Option<&SessionInfo> {
        self.sessions.iter().find(|s| s.meta.id == id)
    }

    /// 실행 화면이 가운데를 덮고 있다. 붙은 세션이 없으면 늘 열려 있다.
    pub fn launcher_open(&self) -> bool {
        self.attached.is_none() || self.launcher
    }

    /// 실행 화면이 새 세션을 만들 프로젝트: 고른 프로젝트, 전체이면 붙은 세션의 프로젝트, 그것도 없으면 앱을 띄운
    /// 프로젝트다.
    pub fn project(&self) -> &Path {
        let attached = self.attached.as_deref().and_then(|id| self.session(id));
        self.filter
            .as_deref()
            .or(attached.map(|s| s.meta.project.as_path()))
            .unwrap_or(&self.current_project)
    }

    /// 세션에 붙는다. 실행 화면과 입력 상자는 닫는다. 이미 붙은 세션이면 데몬에 다시 알리지 않는다.
    ///
    /// `Attach`는 붙은 세션의 PTY를 실린 크기로 바꾼다. 그래서 칸 크기를 알 때만 보낸다. 칸이 아직 그려지지 않았으면
    /// 처음 그려질 때([`Model::set_grid`]), 연결이 없으면 다시 붙을 때 보낸다.
    pub fn attach(&mut self, id: &str) -> Vec<Effect> {
        self.launcher = false;
        self.typing = false;
        self.message = None;
        if self.is_attached(id) {
            return Vec::new();
        }
        self.detach();
        self.attached = Some(id.to_string());
        self.send_attach()
    }

    /// 붙은 세션을 놓는다. 데몬에는 떨어지는 메시지가 없어 그 세션 출력이 계속 오지만 버린다.
    fn detach(&mut self) {
        self.attached = None;
        self.screen = None;
        self.attach_sent = false;
    }

    /// 붙은 세션에 아직 보내지 않은 `Attach`를 지금 칸 크기로 보낸다. 칸 크기를 모르거나 연결이 없으면 미룬다.
    fn send_attach(&mut self) -> Vec<Effect> {
        match (&self.attached, self.grid) {
            (Some(id), Some((cols, rows))) if self.connected && !self.attach_sent => {
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

    /// 보이는 순서(접힌 묶음은 뺀 목록 순서)로 `delta`만큼 옆 세션에 붙는다. 끝에서는 처음으로 돈다.
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

    /// 프로젝트 고르기 순서(전체 포함)로 `delta`만큼 옆 프로젝트를 고른다. 끝에서는 처음으로 돈다.
    pub fn step_project(&mut self, delta: i32) {
        let items = self.projects();
        let at = items
            .iter()
            .position(|p| p.root == self.filter)
            .unwrap_or(0);
        let next = (at as i32 + delta).rem_euclid(items.len() as i32) as usize;
        self.set_filter(items[next].root.clone());
    }

    /// 프로젝트를 고른다. 붙은 세션이 그 프로젝트 것이 아니면 놓는다(실행 화면이 뜬다). 데몬에는 떨어지는 메시지가
    /// 없어 보낼 것이 없다.
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

    /// 탭: 고른 프로젝트(전체이면 모든 프로젝트)의 세션 인덱스, 이름순.
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

    /// ⌘1–⌘9: `i`번째(0부터) 탭에 붙는다.
    pub fn select_tab(&mut self, i: usize) -> Vec<Effect> {
        match self.tabs().get(i) {
            Some(&s) => {
                let id = self.sessions[s].meta.id.clone();
                self.attach(&id)
            }
            None => Vec::new(),
        }
    }

    /// 실행 화면의 줄을 고른다. 세션은 새 이름(`t<N>`)으로 바로 만들고, 붙는 것은 목록에 나타난 뒤다.
    /// `Typed`는 입력 상자를 연다.
    pub fn launch(&mut self, choice: Choice) -> Vec<Effect> {
        self.message = None;
        if choice == Choice::Typed {
            self.typing = true;
            return Vec::new();
        }
        // 앞서 보낸 `New`의 답을 기다리는 동안은 받지 않는다. 같은 이름으로 또 보내 서로 엉키지 않게 한다.
        // 기다린 지 오래면(타이머가 아직 못 돌았어도) 버리고 새로 고른 것을 받는다.
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
        match launch::new_session(choice, &name, &project) {
            Some(spec) => self.create(spec),
            None => Vec::new(),
        }
    }

    /// 입력 상자의 글(`view::parse_new` 형식)로 세션을 만든다. 틀리면 상자를 열어 둔 채 사유를 띄운다.
    pub fn submit_typed(&mut self, text: &str) -> Vec<Effect> {
        if !self.connected {
            self.message = Some(NOT_CONNECTED.into());
            return Vec::new();
        }
        match view::parse_new(text, self.project()) {
            Ok(spec) => {
                self.typing = false;
                self.message = None;
                self.create(spec)
            }
            Err(e) => {
                self.message = Some(format!("{e:#}"));
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

    /// ⌘N, 탭의 ＋, 새 작업: 실행 화면을 연다.
    pub fn new_task(&mut self) {
        self.launcher = true;
        self.typing = false;
        self.message = None;
    }

    /// Esc. 입력 상자가 열려 있으면 실행 화면으로 돌아가고, 붙은 세션 위에 연 실행 화면이면 닫는다.
    /// 붙은 세션이 없으면 돌아갈 곳이 없어 실행 화면은 그대로다.
    pub fn dismiss(&mut self) {
        self.message = None;
        if self.typing {
            self.typing = false;
        } else if self.attached.is_some() {
            self.launcher = false;
        }
    }

    /// ⌘W. 붙은 세션 위에 연 실행 화면이면 그것만 닫는다. 붙은 세션이 있으면 닫기 확인을 시작한다.
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

    /// 기다린 지 [`PENDING_TIMEOUT`]이 넘은 `New`를 버리고 [`START_FAILED`]를 띄운다. 버렸으면 `true`다.
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

    /// 닫기 확인 창의 답. `yes`이면 확인을 물은 세션을 `Kill`한다. 목록에서 빠지면 다른 세션에 붙는다(`on_sessions`).
    pub fn confirm_close(&mut self, yes: bool) -> Vec<Effect> {
        match self.closing.take() {
            Some(id) if yes => vec![Effect::Send(ToDaemon::Kill { id })],
            _ => Vec::new(),
        }
    }

    /// "＋ 프로젝트 추가"로 `root`를 등록했다. `registered`는 등록한 뒤 다시 읽은 목록이다. 거기 있으면 그 프로젝트를
    /// 고르고, 없으면(홈 폴더와 `/`는 등록하지 않는다) 그 사유를 띄운다.
    pub fn project_added(&mut self, root: PathBuf, registered: Vec<PathBuf>) {
        let listed = registered.contains(&root);
        self.registered = registered;
        if listed {
            self.message = None;
            self.set_filter(Some(root));
        } else {
            self.message = Some(format!(
                "홈 폴더와 /는 프로젝트로 추가하지 않아요: {}",
                root.display()
            ));
        }
    }

    /// 터미널 칸을 그려 보니 (`cols`, `rows`)이다(창 크기, 목록 접기). 바뀌었을 때만 화면을 맞추고 `Resize`를
    /// 보낸다. 미뤄 둔 `Attach`가 있으면 그 크기로 붙는다. 연결이 없으면 기억만 하고 다시 붙을 때 싣는다.
    pub fn set_grid(&mut self, cols: u16, rows: u16) -> Vec<Effect> {
        if self.grid == Some((cols, rows)) {
            return Vec::new();
        }
        self.grid = Some((cols, rows));
        if let Some(screen) = &mut self.screen {
            screen.resize(cols, rows);
        }
        match &self.attached {
            // `attach_sent`는 연결이 있을 때만 켜져 있다(끊기면 끈다).
            Some(id) if self.attach_sent => vec![Effect::Send(ToDaemon::Resize {
                id: id.clone(),
                cols,
                rows,
            })],
            _ => self.send_attach(),
        }
    }

    /// 화면 사본만 `lines`줄 스크롤한다(양수가 과거 쪽). 세션에는 보내지 않는다. 보이는 자리가 바뀌었으면
    /// `true`다(다시 그려야 한다).
    pub fn scroll(&mut self, lines: i32) -> bool {
        let Some(screen) = &mut self.screen else {
            return false;
        };
        let before = screen.term().grid().display_offset();
        screen.scroll(lines);
        screen.term().grid().display_offset() != before
    }

    /// 키 입력과 붙여넣기를 붙은 세션에 보낸다. 스크롤해 둔 화면은 맨 아래로 돌려놓는다. 연결이 없으면 보내지
    /// 않는다(연결 스레드의 큐에 쌓이기만 하고 다시 붙을 때 버려진다).
    #[allow(dead_code, reason = "Task 5의 키 입력이 쓴다")]
    pub fn input(&mut self, data: Vec<u8>) -> Vec<Effect> {
        let Some(id) = self.attached.clone().filter(|_| self.connected) else {
            return Vec::new();
        };
        if let Some(screen) = &mut self.screen {
            let scrolled = screen.term().grid().display_offset();
            screen.scroll(-(scrolled as i32));
        }
        vec![Effect::Send(ToDaemon::Input { id, data })]
    }

    /// 붙은 세션이 끝내지 않은 동기화 갱신(`?2026`)의 기한.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.screen.as_ref()?.sync_deadline()
    }

    /// 동기화 갱신이 기한(`now` 기준)을 넘겼으면 끝내고 모아 둔 출력을 화면에 반영한다. 칸이 멈춘 화면에 갇히지
    /// 않게 한다. 끝냈으면 `true`다(다시 그려야 한다).
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

    /// 목록에 그리는 묶음. 기본 접힘에 `toggled`를 얹고, 붙은 세션의 묶음은 늘 펼친다.
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

    /// `state` 묶음을 접거나 편다. 그린 뒤 목록이 바뀌어 묶음 자리가 밀려도 누른 묶음이 바뀐다.
    pub fn toggle_group(&mut self, state: &SessionState) {
        if let Some(slot) = STATES.iter().position(|s| s == state)
            && !self.toggled.remove(&slot)
        {
            self.toggled.insert(slot);
        }
    }

    /// 프로젝트 고르기의 줄. 첫 줄은 전체이고, 그 뒤에 등록된 프로젝트·세션이 있는 프로젝트·앱을 띄운 프로젝트를
    /// 폴더 이름순으로 둔다. 앱을 띄운 프로젝트가 홈으로 대신한 값이면(`home_fallback`) 넣지 않는다.
    /// 승인 대기 세션이 있는 프로젝트(전체는 어느 하나라도)에 `approval`이 켜진다.
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

/// 프로젝트 고르기의 한 줄. `root`가 `None`이면 전체다.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectItem {
    pub root: Option<PathBuf>,
    pub approval: bool,
}

/// 묶음과 탭에 붙는 상태 기호.
pub fn glyph(state: &SessionState) -> &'static str {
    match state {
        SessionState::Approval => "◆",
        SessionState::Working => "◐",
        SessionState::Done | SessionState::Ended => "✓",
        SessionState::Idle => "○",
    }
}

/// 묶음 이름.
pub fn group_label(state: &SessionState) -> &'static str {
    match state {
        SessionState::Approval => "승인 대기",
        SessionState::Working => "작업 중",
        SessionState::Done | SessionState::Ended => "끝남",
        SessionState::Idle => "대기",
    }
}

/// 세션 줄 둘째 줄의 상태 칸. 대기 문구가 있으면 그것(`true`), 없으면 상태 이름이다. 끝난 세션은 `종료됨`이다.
///
/// ponytail: 바탕 문서의 "N분 전"은 `SessionInfo`에 마지막 활동 시각이 없어 뺐다. 데몬이 상태가 바뀐 시각을 실으면 붙인다.
pub fn status_text(s: &SessionInfo) -> (&str, bool) {
    match (&s.waiting, &s.state) {
        (Some(waiting), _) => (waiting, true),
        (None, SessionState::Ended) => ("종료됨", false),
        (None, state) => (group_label(state), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::grid::Dimensions;
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
        }
    }

    #[test]
    fn reload_labels_picks_up_new_aliases_and_logins() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let state = State {
            root: dir.path().join("state"),
        };
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&state.root).unwrap();
        let mut m = Model::new(PathBuf::from("/p"));
        // 읽기 전에는 id 그대로다.
        assert_eq!(m.labels.get("claude-a"), "claude-a");
        // 처음 읽을 때 바뀐 것은 codex의 자동 이름뿐이다.
        assert!(m.reload_labels(&state, Some(&home)));
        assert_eq!(m.labels.get("claude-a"), "claude-a");
        assert_eq!(m.labels.get("codex"), "Codex");
        // 같은 내용을 다시 읽으면 바뀐 것이 없다(다시 그리지 않는다).
        assert!(!m.reload_labels(&state, Some(&home)));
        // 창이 다시 활성화될 때: 로그인이 생기면 자동 이름이, 별칭이 생기면 별칭이 보인다.
        let login = r#"{"oauthAccount":{"emailAddress":"yb@gmail.com"}}"#;
        std::fs::write(home.join(".claude.json"), login).unwrap();
        assert!(m.reload_labels(&state, Some(&home)));
        assert_eq!(m.labels.get("claude-a"), "yb");
        std::fs::write(
            state.root.join("config.toml"),
            "[accounts.claude-a]\nlabel = \"개인\"\n",
        )
        .unwrap();
        assert!(m.reload_labels(&state, Some(&home)));
        assert_eq!(m.labels.get("claude-a"), "개인");
    }

    /// 데몬이 목록을 보냈다. 모델이 시킨 일을 돌려준다.
    fn listed(m: &mut Model, sessions: Vec<SessionInfo>) -> Vec<Effect> {
        m.on_link(LinkEvent::Msg(FromDaemon::Sessions { sessions }))
    }

    /// 테스트의 터미널 칸 크기(열, 행).
    const GRID: (u16, u16) = (80, 24);

    /// 붙은 모델. 터미널 칸 크기는 [`GRID`]이고, 첫 목록은 비어 있어 아무 세션에도 저절로 붙지 않는다.
    fn connected(project: &str) -> Model {
        let mut m = Model::new(PathBuf::from(project));
        m.on_link(LinkEvent::Connected);
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

    /// 칸 크기를 아는 모델이 `/p`의 세션 `t1`에 붙어 있다.
    fn attached_t1() -> Model {
        let mut m = connected("/p");
        listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]);
        assert_eq!(m.attach("k-t1"), [attach_msg("k-t1")]);
        m
    }

    fn output(id: &str, data: &[u8]) -> LinkEvent {
        LinkEvent::Msg(FromDaemon::Output {
            id: id.into(),
            data: data.to_vec(),
        })
    }

    fn snapshot(id: &str, data: Vec<u8>) -> LinkEvent {
        LinkEvent::Msg(FromDaemon::Snapshot {
            id: id.into(),
            data,
        })
    }

    fn lines(m: &Model) -> Vec<String> {
        m.screen.as_ref().expect("화면이 없다").lines()
    }

    #[test]
    fn first_attach_waits_for_the_pane_size() {
        let mut m = Model::new(PathBuf::from("/p"));
        m.on_link(LinkEvent::Connected);
        // 칸이 아직 그려지지 않아 크기를 모른다. 붙을 세션은 정하지만 `Attach`는 미룬다. 틀린 크기로 붙으면
        // 돌고 있는 세션의 PTY 크기가 바뀌어 화면이 깨진다.
        assert!(listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]).is_empty());
        assert_eq!(m.attached.as_deref(), Some("k-t1"));
        assert!(!m.launcher_open());
        // 처음 그려져 크기를 알면 그 크기로 붙는다(`Resize`가 아니다).
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
        // 붙은 세션이 없으면 크기만 기억하고, 다음에 붙을 때 그 크기를 싣는다.
        m.set_filter(Some(PathBuf::from("/q")));
        assert!(m.attached.is_none());
        assert!(m.set_grid(120, 40).is_empty());
        m.set_filter(None);
        assert_eq!(m.attach("k-t1"), [attach_at("k-t1", 120, 40)]);
    }

    #[test]
    fn reattach_after_reconnect() {
        let mut m = attached_t1();
        assert!(m.on_link(LinkEvent::Disconnected("끊김".into())).is_empty());
        // 끊긴 동안 칸 크기가 바뀌면 보내지 않고 기억한다.
        assert!(m.set_grid(100, 30).is_empty());
        // 다시 붙으면 구독하고 색을 알린 뒤, 같은 세션에 지금 크기로 다시 붙는다.
        let effects = m.on_link(LinkEvent::Connected);
        assert_eq!(effects.len(), 3);
        assert_eq!(effects[0], Effect::Send(ToDaemon::List));
        assert!(matches!(effects[1], Effect::Send(ToDaemon::Colors { .. })));
        assert_eq!(effects[2], attach_at("k-t1", 100, 30));
        // 그 뒤로는 바뀐 크기만 `Resize`로 간다.
        assert_eq!(m.set_grid(100, 32), [resize_at("k-t1", 100, 32)]);
        // 붙은 세션이 없으면 다시 붙을 것이 없다.
        let mut m = connected("/p");
        m.on_link(LinkEvent::Disconnected("끊김".into()));
        assert_eq!(m.on_link(LinkEvent::Connected).len(), 2);
    }

    #[test]
    fn offline_input_and_resize_are_not_queued() {
        // 연결이 없을 때 보낸 메시지는 연결 스레드의 큐에 쌓였다가 버려진다. 끊긴 동안은 보내지 않는다.
        let mut m = attached_t1();
        assert_eq!(
            m.input(b"ls\r".to_vec()),
            [Effect::Send(ToDaemon::Input {
                id: "k-t1".into(),
                data: b"ls\r".to_vec(),
            })]
        );
        m.on_link(LinkEvent::Disconnected("끊김".into()));
        assert!(m.input(b"x".to_vec()).is_empty());
        assert!(m.set_grid(50, 20).is_empty());
        // 연결 전에 고른 세션에도 `Attach`를 보내지 않고, 붙은 뒤에 보낸다.
        let mut m = Model::new(PathBuf::from("/p"));
        assert!(m.set_grid(GRID.0, GRID.1).is_empty());
        assert!(m.attach("k-t1").is_empty());
        assert_eq!(m.on_link(LinkEvent::Connected)[2], attach_msg("k-t1"));
        // 붙은 세션이 없으면 입력을 보낼 곳이 없다.
        let mut m = connected("/p");
        assert!(m.input(b"x".to_vec()).is_empty());
    }

    #[test]
    fn snapshot_builds_screen_at_pane_size() {
        let mut m = attached_t1();
        m.set_grid(100, 30);
        // 다른 세션의 화면은 버린다(붙은 세션을 바꾸기 전에 보낸 것).
        m.on_link(snapshot("k-old", b"old".to_vec()));
        assert!(m.screen.is_none());
        let mut source = Screen::new(100, 30);
        source.feed(b"\x1b[31mhello\x1b[0m\r\nworld");
        assert!(m.on_link(snapshot("k-t1", source.snapshot())).is_empty());
        let grid = m.screen.as_ref().unwrap().term().grid();
        assert_eq!((grid.columns(), grid.screen_lines()), (100, 30));
        assert_eq!(lines(&m)[..2], ["hello", "world"]);
        // 이어 오는 출력을 먹인다. 질의(DA1)의 답은 데몬이 하니 클라이언트 화면의 답은 버린다.
        assert!(m.on_link(output("k-t1", b"!\x1b[c")).is_empty());
        assert_eq!(lines(&m)[1], "world!");
        assert!(m.screen.as_mut().unwrap().take_replies().is_empty());
        // 다른 세션의 출력은 먹이지 않는다.
        m.on_link(output("k-old", b"zzz"));
        assert_eq!(lines(&m)[1], "world!");
        // 칸 크기가 바뀌면 화면도 맞춘다.
        m.set_grid(90, 20);
        let grid = m.screen.as_ref().unwrap().term().grid();
        assert_eq!((grid.columns(), grid.screen_lines()), (90, 20));
        // 다른 세션에 붙으면 화면을 버리고 그 세션의 `Snapshot`을 기다린다.
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
        // 동기화 갱신(`?2026`)을 열고 닫지 않았다. 그동안의 출력은 화면에 나타나지 않는다.
        m.on_link(output("k-t1", b"\x1b[?2026hstuck"));
        let deadline = m.sync_deadline().expect("기한이 없다");
        assert_eq!(lines(&m)[0], "");
        assert!(!m.end_stale_sync(deadline - Duration::from_millis(1)));
        // 기한이 지나면 끝내고 모아 둔 출력을 화면에 반영한다.
        assert!(m.end_stale_sync(deadline));
        assert_eq!(lines(&m)[0], "stuck");
        assert_eq!(m.sync_deadline(), None);
        assert!(!m.end_stale_sync(deadline + Duration::from_secs(1)));
    }

    #[test]
    fn scroll_moves_the_view_and_input_returns_to_the_bottom() {
        let mut m = attached_t1();
        // 화면이 오기 전에는 스크롤할 것이 없다.
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
        // 세션에 입력을 보내면 맨 아래로 돌아간다.
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
    fn connected_subscribes_and_reports_colors() {
        let mut m = Model::new(PathBuf::from("/p/mine"));
        assert!(!m.connected);
        assert_eq!(
            m.on_link(LinkEvent::Connected),
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
        // 머리를 누르면 대기 묶음이 펼쳐지고, 다시 누르면 접힌다.
        m.toggle_group(&SessionState::Idle);
        assert!(!m.visible_groups()[3].collapsed);
        m.toggle_group(&SessionState::Idle);
        assert!(m.visible_groups()[3].collapsed);
        // 프로젝트를 고르면 그 프로젝트의 세션만 묶는다.
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
        // 그린 화면에서 두 번째 머리는 대기 묶음이다.
        let clicked = m.visible_groups()[1].state.clone();
        assert_eq!(clicked, SessionState::Idle);
        // 누르기 전에 새 목록이 와서 승인 대기 묶음이 앞에 끼었다. 두 번째 자리는 이제 작업 중이다.
        listed(
            &mut m,
            vec![
                s("/p", "ask", SessionState::Approval),
                s("/p", "work", SessionState::Working),
                s("/p", "idle", SessionState::Idle),
            ],
        );
        m.toggle_group(&clicked);
        // 누른 대기 묶음만 바뀐다(STATES 인덱스 3).
        assert_eq!(m.toggled, HashSet::from([3]));
        let g = m.visible_groups();
        assert_eq!(g[2].state, SessionState::Idle);
        assert!(!g[2].collapsed);
    }

    #[test]
    fn disconnect_marks_offline() {
        let mut m = connected("/p/mine");
        listed(&mut m, vec![s("/p/mine", "a", SessionState::Working)]);
        assert!(m.on_link(LinkEvent::Disconnected("끊김".into())).is_empty());
        assert!(!m.connected);
        assert_eq!(m.offline.as_deref(), Some("끊김"));
        assert_eq!(m.sessions.len(), 1);
        assert_eq!(m.visible_groups().len(), 1);
        // 다시 붙으면 띠를 거두고 다시 구독한다.
        assert_eq!(
            m.on_link(LinkEvent::Connected)[0],
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
        // 실행 화면은 그래도 만들 곳이 있다.
        assert_eq!(m.project(), Path::new("/Users/me"));
        // 다른 경로로 띄운 앱의 프로젝트는 늘 있다.
        m.home_fallback = false;
        assert_eq!(roots(&m).len(), 3);
    }

    #[test]
    fn launch_creates_t_n_and_attaches_when_listed() {
        let mut m = connected("/p");
        assert_eq!(
            m.launch(Choice::ClaudeA),
            [new_msg("/p", "t1", Kind::Claude, Some("claude-a"))]
        );
        // 답을 기다리는 동안 다른 줄은 받지 않는다. 같은 이름으로 또 보내 엉키지 않게 한다.
        assert!(m.launch(Choice::Shell).is_empty());
        assert!(m.launcher_open());
        // 만든 세션이 목록에 나타나면 붙고 실행 화면은 닫힌다.
        assert_eq!(
            listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]),
            [attach_msg("k-t1")]
        );
        assert_eq!(m.attached.as_deref(), Some("k-t1"));
        assert!(!m.launcher_open() && m.pending.is_none());
        // 다음 이름은 t2다. 데몬이 거절하면 사유를 띄우고 다시 고를 수 있다.
        m.new_task();
        assert_eq!(
            m.launch(Choice::Codex),
            [new_msg("/p", "t2", Kind::Codex, Some("codex"))]
        );
        m.on_link(LinkEvent::Msg(FromDaemon::Error {
            message: "안 된다".into(),
        }));
        assert_eq!(m.message.as_deref(), Some("안 된다"));
        assert_eq!(
            m.launch(Choice::ClaudeB),
            [new_msg("/p", "t2", Kind::Claude, Some("claude-b"))]
        );
        assert_eq!(m.message, None);
    }

    #[test]
    fn launch_needs_the_daemon_and_a_lost_new_does_not_block() {
        // 연결 전에는 보낸 메시지를 연결 스레드가 버린다. 만들지 않고 사유를 띄운다.
        let mut m = Model::new(PathBuf::from("/p"));
        assert!(m.launch(Choice::Shell).is_empty());
        assert!(m.submit_typed("shell dev").is_empty());
        assert!(m.pending.is_none());
        assert!(m.message.as_deref().unwrap().contains("데몬"));
        // `New`를 보낸 뒤 끊기면 답이 오지 않을 수 있다. 기다림을 풀어 다시 고를 수 있게 한다.
        m.on_link(LinkEvent::Connected);
        assert_eq!(m.launch(Choice::Shell).len(), 1);
        m.on_link(LinkEvent::Disconnected("끊김".into()));
        assert!(m.pending.is_none());
        m.on_link(LinkEvent::Connected);
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
        // 답을 기다리는 동안 고르면 만드는 중이라고 알린다.
        assert!(m.launch(Choice::Codex).is_empty());
        assert_eq!(m.message.as_deref(), Some("만드는 중이에요"));
        // 셸이 곧바로 끝나면 데몬은 세션 없이 답한다. 목록이 바뀌지 않아 데몬은 알리지 않고 답만 온다.
        assert!(listed(&mut m, Vec::new()).is_empty());
        assert!(m.pending.is_none());
        assert_eq!(m.message.as_deref(), Some("세션을 시작하지 못했어요"));
        // 실행 화면이 다시 동작한다.
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
        // 만드는 사이 다른 세션이 바뀌어 데몬이 알린 목록은 답이 아니다. 계속 기다린다.
        assert!(listed(&mut m, vec![s("/q", "x", SessionState::Working)]).is_empty());
        assert!(m.pending.is_some() && m.message.is_none());
        assert!(!m.expire_pending(since + Duration::from_secs(4)));
        assert!(m.pending.is_some());
        // 5초가 넘으면 버리고 알린다.
        assert!(m.expire_pending(since + PENDING_TIMEOUT));
        assert!(m.pending.is_none());
        assert_eq!(m.message.as_deref(), Some("세션을 시작하지 못했어요"));
        assert!(!m.expire_pending(since + PENDING_TIMEOUT));
        assert_eq!(m.launch(Choice::Shell).len(), 1);
    }

    #[test]
    fn typed_choice_parses_kind_name_and_model() {
        let mut m = connected("/p");
        assert!(m.launch(Choice::Typed).is_empty());
        assert!(m.typing && m.pending.is_none());
        // 틀린 입력은 상자를 열어 둔 채 사유를 띄운다.
        assert!(m.submit_typed("gpt x").is_empty());
        assert!(m.typing);
        assert!(m.message.as_deref().unwrap().contains("알 수 없는 종류"));
        let Effect::Send(ToDaemon::New { spec }) = &m.submit_typed("claude-b mine opus")[0] else {
            panic!("New가 아니다");
        };
        assert_eq!(
            (
                spec.name.as_str(),
                spec.account.as_deref(),
                spec.model.as_deref()
            ),
            ("mine", Some("claude-b"), Some("opus"))
        );
        assert!(!m.typing && m.message.is_none());
        let p = m.pending.as_ref().unwrap();
        assert_eq!(
            (p.project.as_path(), p.name.as_str()),
            (Path::new("/p"), "mine")
        );
    }

    #[test]
    fn launch_goes_to_the_selected_or_attached_project() {
        let mut m = connected("/p/mine");
        listed(&mut m, vec![s("/p/kyoto", "t1", SessionState::Idle)]);
        // 전체에서 아무 세션에도 붙지 않았으면 앱을 띄운 프로젝트다.
        assert_eq!(m.project(), Path::new("/p/mine"));
        // 전체에서 붙은 세션이 있으면 그 세션의 프로젝트다.
        m.attach("k-t1");
        m.new_task();
        assert_eq!(m.project(), Path::new("/p/kyoto"));
        assert_eq!(
            m.launch(Choice::Shell),
            [new_msg("/p/kyoto", "t2", Kind::Shell, None)]
        );
        // 프로젝트를 고르면 그 프로젝트다.
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
        // ⌘2는 두 번째 탭에 붙고, 없는 탭은 아무것도 하지 않는다.
        m.filter = Some(PathBuf::from("/p/mine"));
        assert_eq!(m.select_tab(1), [attach_msg("k-t2")]);
        assert!(m.select_tab(3).is_empty());
        assert_eq!(m.attached.as_deref(), Some("k-t2"));
        // 이미 붙은 탭을 다시 고르면 데몬에 다시 알리지 않는다.
        assert!(m.select_tab(1).is_empty());
    }

    #[test]
    fn step_session_wraps() {
        let mut m = Model::new(PathBuf::from("/p"));
        m.on_link(LinkEvent::Connected);
        m.set_grid(GRID.0, GRID.1);
        // 처음 받은 목록이면 보이는 첫 세션에 붙는다(승인 대기가 먼저다).
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
        // 보이는 순서는 c, a, b다. 접힌 대기 묶음의 d는 건너뛴다.
        let mut visit = |delta| {
            m.step_session(delta);
            m.attached.clone().unwrap()
        };
        assert_eq!(visit(1), "k-a");
        assert_eq!(visit(1), "k-b");
        assert_eq!(visit(1), "k-c");
        assert_eq!(visit(-1), "k-b");
        assert_eq!(m.step_session(-1), [attach_msg("k-a")]);
        // 붙은 세션이 없으면 다음은 처음, 이전은 끝이다.
        m.attached = None;
        assert_eq!(m.step_session(1), [attach_msg("k-c")]);
        m.attached = None;
        assert_eq!(m.step_session(-1), [attach_msg("k-b")]);
        // 세션이 하나도 없으면 아무것도 하지 않는다.
        m.set_filter(Some(PathBuf::from("/q")));
        assert!(m.step_session(1).is_empty());
    }

    #[test]
    fn step_project_cycles_through_the_picker() {
        let mut m = connected("/p/mine");
        listed(&mut m, vec![s("/p/kyoto", "t1", SessionState::Idle)]);
        m.attach("k-t1");
        // 프로젝트 고르기 순서는 전체, kyoto, mine이다.
        m.step_project(1);
        assert_eq!(m.filter, Some(PathBuf::from("/p/kyoto")));
        assert_eq!(m.attached.as_deref(), Some("k-t1"));
        // 붙은 세션이 고른 프로젝트 것이 아니면 놓고 실행 화면을 띄운다.
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
        // 닫을 세션이 없으면 창을 닫는다.
        assert_eq!(m.close(), Close::Window);
        listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]);
        m.attach("k-t1");
        // 확인 창의 답이 오기 전에는 아무것도 보내지 않고, 창이 떠 있는 동안 ⌘W는 다시 묻지 않는다.
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
        // 답이 두 번 와도 한 번만 닫는다.
        assert!(m.confirm_close(true).is_empty());
        // 붙은 세션 위에 연 실행 화면은 ⌘W가 실행 화면만 닫는다.
        m.new_task();
        assert_eq!(m.close(), Close::Launcher);
        assert!(!m.launcher_open() && m.closing.is_none());
        // 닫힌 세션이 목록에서 빠지면 남은 세션이 없으니 실행 화면이 뜬다.
        assert!(listed(&mut m, Vec::new()).is_empty());
        assert!(m.attached.is_none() && m.launcher_open());
    }

    #[test]
    fn no_session_shows_launcher() {
        let mut m = Model::new(PathBuf::from("/p"));
        assert!(m.launcher_open());
        m.on_link(LinkEvent::Connected);
        assert!(listed(&mut m, Vec::new()).is_empty());
        assert!(m.launcher_open() && m.attached.is_none());
        // 처음 목록 뒤에 나타난 세션에는 저절로 붙지 않는다.
        assert!(listed(&mut m, vec![s("/p", "t1", SessionState::Idle)]).is_empty());
        assert!(m.launcher_open());
        // Esc는 붙은 세션이 없으면 돌아갈 곳이 없어 아무것도 하지 않는다.
        m.dismiss();
        assert!(m.launcher_open());
        m.attach("k-t1");
        assert!(!m.launcher_open());
        // ⌘N으로 세션 위에 연 실행 화면은 Esc로 닫는다. 입력 상자가 열려 있으면 먼저 실행 화면으로 돌아간다.
        m.new_task();
        assert!(m.launcher_open());
        m.launch(Choice::Typed);
        m.dismiss();
        assert!(!m.typing && m.launcher_open());
        m.dismiss();
        assert!(!m.launcher_open());
        // 붙은 세션이 사라지고 남은 세션도 없으면 실행 화면이 다시 뜬다.
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
        // 등록 목록에 없으면(홈 폴더) 고르지 않고 알린다.
        m.project_added(PathBuf::from("/Users/me"), vec![kyoto.clone()]);
        assert_eq!(m.filter, Some(kyoto));
        assert!(m.message.as_deref().unwrap().contains("홈 폴더"));
    }

    #[test]
    fn status_prefers_waiting_text() {
        let mut ask = s("/p", "a", SessionState::Approval);
        ask.waiting = Some("Bash(npm run deploy)".into());
        assert_eq!(status_text(&ask), ("Bash(npm run deploy)", true));
        assert_eq!(
            status_text(&s("/p", "e", SessionState::Ended)),
            ("종료됨", false)
        );
        assert_eq!(
            status_text(&s("/p", "w", SessionState::Working)),
            ("작업 중", false)
        );
        assert_eq!(group_label(&SessionState::Ended), "끝남");
    }
}
