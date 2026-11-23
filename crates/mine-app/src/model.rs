//! 앱 상태. GPUI 없이 데몬 연결의 일(`LinkEvent`)을 받아 상태를 바꾸고, 데몬에 보낼 메시지(`Effect`)를 돌려준다.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use mine_core::link::LinkEvent;
use mine_core::proto::{FromDaemon, ToDaemon};
use mine_core::session::{SessionInfo, SessionState};
use mine_core::view::{self, Group};

use crate::theme;

/// 모델이 바깥에 시키는 일.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Send(ToDaemon),
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
    /// 등록된 프로젝트(`mine_core::project::registered`).
    pub registered: Vec<PathBuf>,
}

/// 묶음 상태 순서. `Model::toggled`의 값이 이 배열의 인덱스다.
pub const STATES: [SessionState; 4] = [
    SessionState::Approval,
    SessionState::Working,
    SessionState::Done,
    SessionState::Idle,
];

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
            registered: Vec::new(),
        }
    }

    /// 데몬 연결의 일을 받는다.
    ///
    /// 붙을 때마다(다시 붙을 때도) 목록을 구독하고(R18) 터미널 색을 알린다. `Hello`는 연결이 이미 주고받았다.
    /// 끊겨도 세션 목록은 그대로 두고, 다시 붙어 새 목록이 오면 바꾼다.
    pub fn on_link(&mut self, ev: LinkEvent) -> Vec<Effect> {
        match ev {
            LinkEvent::Connected => {
                self.connected = true;
                self.offline = None;
                vec![
                    Effect::Send(ToDaemon::List),
                    Effect::Send(ToDaemon::Colors {
                        fg: theme::hex(theme::TERM_FG),
                        bg: theme::hex(theme::TERM_BG),
                    }),
                ]
            }
            LinkEvent::Disconnected(reason) => {
                self.connected = false;
                self.offline = Some(reason);
                Vec::new()
            }
            LinkEvent::Msg(FromDaemon::Sessions { sessions }) => {
                self.sessions = sessions;
                Vec::new()
            }
            // 화면(Snapshot·Output)과 오류 알림은 세션을 붙이는 Task 3·4가 다룬다.
            LinkEvent::Msg(_) => Vec::new(),
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

    /// `visible_groups`의 `i`번째 묶음을 접거나 편다.
    pub fn toggle_group(&mut self, i: usize) {
        let Some(state) = self.visible_groups().into_iter().nth(i).map(|g| g.state) else {
            return;
        };
        if let Some(slot) = STATES.iter().position(|s| *s == state)
            && !self.toggled.remove(&slot)
        {
            self.toggled.insert(slot);
        }
    }

    /// 프로젝트 고르기의 줄. 첫 줄은 전체이고, 그 뒤에 등록된 프로젝트·세션이 있는 프로젝트·앱을 띄운 프로젝트를
    /// 폴더 이름순으로 둔다. 승인 대기 세션이 있는 프로젝트(전체는 어느 하나라도)에 `approval`이 켜진다.
    pub fn projects(&self) -> Vec<ProjectItem> {
        let mut roots: BTreeMap<&Path, bool> = BTreeMap::new();
        roots.insert(&self.current_project, false);
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

    fn listed(m: &mut Model, sessions: Vec<SessionInfo>) {
        let effects = m.on_link(LinkEvent::Msg(FromDaemon::Sessions { sessions }));
        assert!(effects.is_empty());
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
        let mut m = Model::new(PathBuf::from("/p/mine"));
        m.on_link(LinkEvent::Connected);
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
        m.toggle_group(3);
        assert!(!m.visible_groups()[3].collapsed);
        m.toggle_group(3);
        assert!(m.visible_groups()[3].collapsed);
        // 프로젝트를 고르면 그 프로젝트의 세션만 묶는다.
        m.filter = Some(PathBuf::from("/p/kyoto"));
        let states: Vec<_> = m.visible_groups().into_iter().map(|g| g.state).collect();
        assert_eq!(states, [SessionState::Approval, SessionState::Done]);
        assert_eq!(m.current_project, Path::new("/p/mine"));
    }

    #[test]
    fn disconnect_marks_offline() {
        let mut m = Model::new(PathBuf::from("/p/mine"));
        m.on_link(LinkEvent::Connected);
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
