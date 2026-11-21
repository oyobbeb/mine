//! 아이콘 줄과 상태별 세션 목록의 화면 모델. 입출력 없이 데이터만 계산한다.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::project::project_key;
use crate::session::{Kind, NewSession, SessionInfo, SessionState, validate_name};

/// 프로젝트 아이콘 색. `RailItem::color`가 이 배열의 인덱스다.
pub const PROJECT_COLORS: [&str; 6] = [
    "#61afef", "#98c379", "#e06c75", "#d19a66", "#a9b1d6", "#7dcfff",
];

/// 아이콘 줄의 한 칸. `project`가 `None`이면 모든 프로젝트(∗)다.
#[derive(Clone, Debug, PartialEq)]
pub struct RailItem {
    pub project: Option<PathBuf>,
    pub initial: char,
    pub color: usize,
    pub approval: bool,
}

/// 목록의 상태 묶음. `rows`는 `sessions` 슬라이스의 인덱스다.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub state: SessionState,
    pub rows: Vec<usize>,
    pub collapsed: bool,
}

/// 모든 프로젝트 칸의 색. `PROJECT_COLORS`에서 가장 눈에 띄지 않는 회청색이다.
const ALL_COLOR: usize = 4;

/// 아이콘 줄을 만든다. 첫 칸은 모든 프로젝트(∗)이고, 그 뒤에 프로젝트를 폴더 이름순으로 둔다.
/// 세션이 없어도 `current`는 넣는다. `approval`은 그 프로젝트에 승인 대기 세션이 있으면 켜진다.
pub fn rail(sessions: &[SessionInfo], current: &Path) -> Vec<RailItem> {
    let mut projects: BTreeMap<&Path, bool> = BTreeMap::new();
    projects.entry(current).or_insert(false);
    for s in sessions {
        let approval = projects.entry(s.meta.project.as_path()).or_insert(false);
        *approval |= s.state == SessionState::Approval;
    }
    let mut projects: Vec<_> = projects.into_iter().collect();
    projects.sort_by(|(a, _), (b, _)| a.file_name().cmp(&b.file_name()).then_with(|| a.cmp(b)));
    let all = RailItem {
        project: None,
        initial: '∗',
        color: ALL_COLOR,
        approval: projects.iter().any(|(_, approval)| *approval),
    };
    let items = projects.into_iter().map(|(root, approval)| RailItem {
        project: Some(root.to_path_buf()),
        initial: initial(root),
        color: color(root),
        approval,
    });
    std::iter::once(all).chain(items).collect()
}

/// 폴더 이름의 첫 글자를 소문자로. 이름이 없으면 `?`다.
fn initial(root: &Path) -> char {
    root.file_name()
        .and_then(|name| name.to_string_lossy().chars().next())
        .and_then(|c| c.to_lowercase().next())
        .unwrap_or('?')
}

/// `project_key` 해시 앞 8자리를 색 개수로 나눈 나머지.
fn color(root: &Path) -> usize {
    let key = project_key(root);
    u64::from_str_radix(&key[..8], 16).map_or(0, |n| (n % PROJECT_COLORS.len() as u64) as usize)
}

/// 상태별 묶음. Approval, Working, Done(Ended 포함), Idle 순서이고 빈 묶음은 뺀다.
/// 묶음 안은 이름순이다. Idle만 처음에 접혀 있다. `filter`가 있으면 그 프로젝트 세션만 남긴다.
pub fn groups(sessions: &[SessionInfo], filter: Option<&Path>) -> Vec<Group> {
    const STATES: [SessionState; 4] = [
        SessionState::Approval,
        SessionState::Working,
        SessionState::Done,
        SessionState::Idle,
    ];
    let mut rows: [Vec<usize>; 4] = Default::default();
    for (i, s) in sessions.iter().enumerate() {
        if filter.is_some_and(|p| s.meta.project != p) {
            continue;
        }
        let slot = match s.state {
            SessionState::Approval => 0,
            SessionState::Working => 1,
            SessionState::Done | SessionState::Ended => 2,
            SessionState::Idle => 3,
        };
        rows[slot].push(i);
    }
    STATES
        .into_iter()
        .zip(rows)
        .filter(|(_, rows)| !rows.is_empty())
        .map(|(state, mut rows)| {
            rows.sort_by(|&a, &b| {
                let (a_name, b_name) = (&sessions[a].meta.name, &sessions[b].meta.name);
                a_name.cmp(b_name).then(a.cmp(&b))
            });
            Group {
                collapsed: state == SessionState::Idle,
                state,
                rows,
            }
        })
        .collect()
}

/// 기본 접힘(`groups`)에 클라이언트 상태를 얹는다. `toggled`에 든 상태의 묶음은 기본과 반대로 접거나 펴고,
/// 붙은 세션(`attached`, `sessions` 인덱스)이 든 묶음은 늘 펼친다.
pub fn expand(
    mut groups: Vec<Group>,
    toggled: &[SessionState],
    attached: Option<usize>,
) -> Vec<Group> {
    for g in &mut groups {
        g.collapsed ^= toggled.contains(&g.state);
        if attached.is_some_and(|i| g.rows.contains(&i)) {
            g.collapsed = false;
        }
    }
    groups
}

/// 접힌 묶음을 뺀, 화면에 보이는 줄의 순서. 목록이 그리는 묶음(`expand`를 거친 것)을 받는다.
/// `⌃a n/p`가 이 순서로 움직인다.
pub fn order(groups: &[Group]) -> Vec<usize> {
    groups
        .iter()
        .filter(|g| !g.collapsed)
        .flat_map(|g| g.rows.iter().copied())
        .collect()
}

/// `⌃a c` 입력 형식.
pub const NEW_USAGE: &str = "<claude-a|claude-b|codex|shell> <이름> [모델]";

/// `"<claude-a|claude-b|codex|shell> <이름> [모델]"`을 새 세션 명세로 바꾼다.
pub fn parse_new(input: &str, project: &Path) -> Result<NewSession> {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    let Some(&first) = tokens.first() else {
        bail!("형식: {NEW_USAGE}");
    };
    let (kind, account) = match first {
        "claude-a" | "claude-b" => (Kind::Claude, Some(first.to_string())),
        "codex" => (Kind::Codex, Some(first.to_string())),
        "shell" => (Kind::Shell, None),
        _ => bail!("알 수 없는 종류다: {first} (형식: {NEW_USAGE})"),
    };
    if tokens.len() > 3 {
        bail!("인자가 너무 많다 (형식: {NEW_USAGE})");
    }
    let Some(&name) = tokens.get(1) else {
        bail!("세션 이름이 필요하다 (형식: {NEW_USAGE})");
    };
    validate_name(name)?;
    let model = tokens.get(2).map(|m| m.to_string());
    if kind == Kind::Shell && model.is_some() {
        bail!("shell 세션에는 모델을 줄 수 없다");
    }
    Ok(NewSession {
        project: project.to_path_buf(),
        name: name.to_string(),
        kind,
        account,
        model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Kind, Meta, Restore, SessionState::*};

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

    fn states(g: &[Group]) -> Vec<SessionState> {
        g.iter().map(|g| g.state.clone()).collect()
    }

    #[test]
    fn rail_starts_with_all_and_flags_approval() {
        let r = rail(
            &[s("/p/kyoto", "u", Approval), s("/p/mine", "r", Working)],
            Path::new("/p/mine"),
        );
        assert_eq!(r[0].project, None);
        assert_eq!(r.len(), 3);
        assert_eq!(r[1].initial, 'k');
        assert!(r[1].approval);
        assert!(!r[2].approval);
        // 색은 project_key 해시 앞 8자리를 6으로 나눈 나머지다(shasum으로 따로 구한 값).
        assert_eq!((r[1].color, r[2].color), (1, 5));
        // 모든 프로젝트 칸은 어느 프로젝트에든 승인 대기가 있으면 켜진다.
        assert!(r[0].approval);
        // 이름순으로 정렬하고, 같은 프로젝트의 세션이 여럿이어도 한 칸이다.
        let r = rail(
            &[
                s("/p/mine", "a", Working),
                s("/p/Zed", "b", Idle),
                s("/p/mine", "c", Idle),
                s("/p/app", "d", Done),
            ],
            Path::new("/p/mine"),
        );
        let initials: Vec<_> = r.iter().map(|i| i.initial).collect();
        assert_eq!(initials, vec!['∗', 'z', 'a', 'm']);
        let order: Vec<_> = r.iter().map(|i| i.project.clone()).collect();
        assert_eq!(
            order,
            vec![
                None,
                Some(PathBuf::from("/p/Zed")),
                Some(PathBuf::from("/p/app")),
                Some(PathBuf::from("/p/mine")),
            ]
        );
    }

    #[test]
    fn rail_includes_current_project_without_sessions() {
        let r = rail(&[], Path::new("/p/mine"));
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].project, None);
        assert_eq!(r[1].project, Some(PathBuf::from("/p/mine")));
        assert_eq!(r[1].initial, 'm');
        assert!(!r[1].approval);
        // 이름이 없는 경로(루트)는 '?'로 보인다.
        assert_eq!(rail(&[], Path::new("/"))[1].initial, '?');
    }

    #[test]
    fn groups_order_and_idle_collapsed() {
        let ss = [
            s("/p", "a", Idle),
            s("/p", "b", Working),
            s("/p", "c", Approval),
            s("/p", "d", Ended),
        ];
        let g = groups(&ss, None);
        assert_eq!(states(&g), vec![Approval, Working, Done, Idle]);
        assert!(g[3].collapsed && !g[0].collapsed);
        assert_eq!(g[2].rows, vec![3]);
        // Done과 Ended는 한 묶음이고, 같은 묶음 안에서는 이름순이다. 빈 묶음은 뺀다.
        let ss = [
            s("/p", "zeta", Done),
            s("/p", "alpha", Ended),
            s("/p", "mid", Done),
        ];
        let g = groups(&ss, None);
        assert_eq!(states(&g), vec![Done]);
        assert_eq!(g[0].rows, vec![1, 2, 0]);
        assert!(!g[0].collapsed);
    }

    #[test]
    fn filter_by_project() {
        let ss = [
            s("/p/a", "x", Working),
            s("/p/b", "y", Working),
            s("/p/a", "z", Idle),
        ];
        let g = groups(&ss, Some(Path::new("/p/a")));
        assert_eq!(states(&g), vec![Working, Idle]);
        let rows: Vec<usize> = g.iter().flat_map(|g| g.rows.clone()).collect();
        assert_eq!(rows, vec![0, 2]);
        assert!(!rows.contains(&1));
        assert_eq!(order(&groups(&ss, Some(Path::new("/p/b")))), vec![1]);
        assert!(groups(&ss, Some(Path::new("/p/none"))).is_empty());
    }

    #[test]
    fn order_skips_collapsed() {
        let ss = [
            s("/p", "a", Idle),
            s("/p", "b", Working),
            s("/p", "c", Approval),
            s("/p", "d", Ended),
        ];
        assert_eq!(order(&groups(&ss, None)), vec![2, 1, 3]);
    }

    #[test]
    fn expand_applies_toggles_and_keeps_attached_group_open() {
        let ss = [
            s("/p", "a", Idle),
            s("/p", "b", Working),
            s("/p", "c", Idle),
        ];
        let g = groups(&ss, None);
        assert_eq!(order(&g), vec![1]);
        // 사용자가 대기 묶음을 펼치면 그 줄도 순서에 든다.
        assert_eq!(order(&expand(g.clone(), &[Idle], None)), vec![1, 0, 2]);
        // 처음에 펼친 묶음은 접을 수 있다.
        assert!(order(&expand(g.clone(), &[Working], None)).is_empty());
        // 붙은 세션의 묶음은 기본이나 사용자 접힘과 상관없이 펼친다.
        assert_eq!(order(&expand(g.clone(), &[], Some(2))), vec![1, 0, 2]);
        assert_eq!(order(&expand(g, &[Idle, Working], Some(2))), vec![0, 2]);
    }

    #[test]
    fn parse_new_variants() {
        let n = parse_new("claude-b rust-core opus", Path::new("/p")).unwrap();
        assert_eq!(
            (
                n.kind,
                n.account.as_deref(),
                n.name.as_str(),
                n.model.as_deref()
            ),
            (Kind::Claude, Some("claude-b"), "rust-core", Some("opus"))
        );
        assert_eq!(n.project, PathBuf::from("/p"));
        assert_eq!(
            parse_new("shell dev", Path::new("/p")).unwrap().account,
            None
        );
        let a = parse_new("  claude-a   x  ", Path::new("/p")).unwrap();
        assert_eq!(
            (a.kind, a.account.as_deref(), a.model),
            (Kind::Claude, Some("claude-a"), None)
        );
        let c = parse_new("codex x gpt-5", Path::new("/p")).unwrap();
        assert_eq!(
            (c.kind, c.account.as_deref(), c.model.as_deref()),
            (Kind::Codex, Some("codex"), Some("gpt-5"))
        );
        assert!(parse_new("gpt x", Path::new("/p")).is_err());
        assert!(parse_new("codex", Path::new("/p")).is_err());
        assert!(parse_new("", Path::new("/p")).is_err());
        assert!(parse_new("shell dev opus", Path::new("/p")).is_err());
        assert!(parse_new("claude-a x opus extra", Path::new("/p")).is_err());
    }

    #[test]
    fn parse_new_rejects_invalid_name() {
        for bad in ["shell -x", "claude-a a/b", "codex 한글"] {
            let e = parse_new(bad, Path::new("/p")).unwrap_err();
            assert!(e.to_string().contains("세션 이름"), "{bad}: {e}");
        }
    }
}
