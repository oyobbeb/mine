//! Screen model for the project rail and the state-grouped session list. Only computes data, with no I/O.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::profiles::ProfileInfo;
use crate::project::project_key;
use crate::session::{Kind, NewSession, SessionInfo, SessionState, validate_name};

/// Project icon colors. `RailItem::color` is an index into this array.
pub const PROJECT_COLORS: [&str; 6] = [
    "#61afef", "#98c379", "#e06c75", "#d19a66", "#a9b1d6", "#7dcfff",
];

/// One cell of the project rail. If `project` is `None`, it is all projects (∗).
#[derive(Clone, Debug, PartialEq)]
pub struct RailItem {
    pub project: Option<PathBuf>,
    pub initial: char,
    pub color: usize,
    pub approval: bool,
}

/// A state group of the list. `rows` are indexes into the `sessions` slice.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub state: SessionState,
    pub rows: Vec<usize>,
    pub collapsed: bool,
}

/// Color of the all-projects cell. The least eye-catching blue-gray in `PROJECT_COLORS`.
const ALL_COLOR: usize = 4;

/// Builds the project rail. The first cell is all projects (∗), followed by projects ordered by directory name.
/// `current` is included even if it has no sessions. `approval` is on if the project has a Needs approval session.
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

/// The first letter of the directory name, lowercased. `?` if there is no name.
fn initial(root: &Path) -> char {
    root.file_name()
        .and_then(|name| name.to_string_lossy().chars().next())
        .and_then(|c| c.to_lowercase().next())
        .unwrap_or('?')
}

/// The project directory name. A path with no name (the root) gives the whole path.
pub fn folder_name(root: &Path) -> String {
    root.file_name().map_or_else(
        || root.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The first 8 digits of the `project_key` hash modulo the number of colors.
fn color(root: &Path) -> usize {
    let key = project_key(root);
    u64::from_str_radix(&key[..8], 16).map_or(0, |n| (n % PROJECT_COLORS.len() as u64) as usize)
}

/// State order of the list groups. [`Group::state`] is always one of these four (Ended goes in the Done group).
pub const STATES: [SessionState; 4] = [
    SessionState::Approval,
    SessionState::Working,
    SessionState::Done,
    SessionState::Idle,
];

/// State groups. In the order Approval, Working, Done (including Ended), Idle, and empty groups are left out.
/// Within a group, rows are ordered by name. Only Idle starts collapsed. If `filter` is set, only that project's sessions remain.
pub fn groups(sessions: &[SessionInfo], filter: Option<&Path>) -> Vec<Group> {
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

/// Layers client state over the default collapsing (`groups`). A group whose state is in `toggled` is collapsed or expanded opposite to the default,
/// and a group containing the attached session (`attached`, an index into `sessions`) is always expanded.
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

/// The order of the rows visible on screen, without collapsed groups. Takes the groups the list draws (the ones that went through `expand`).
/// `⌃a n/p` moves in this order.
pub fn order(groups: &[Group]) -> Vec<usize> {
    groups
        .iter()
        .filter(|g| !g.collapsed)
        .flat_map(|g| g.rows.iter().copied())
        .collect()
}

/// Format of the typed new-session input (the launcher's `n`).
pub const NEW_USAGE: &str = "<profile|shell> <name> [model]";

/// Turns `"<profile|shell> <name> [model]"` into a new session spec. The kind comes from the profile; `profiles` is the list to
/// pick from, and the daemon checks the id again when it starts the session.
pub fn parse_new(input: &str, project: &Path, profiles: &[ProfileInfo]) -> Result<NewSession> {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    let Some(&first) = tokens.first() else {
        bail!("Usage: {NEW_USAGE}");
    };
    let (kind, account) = match first {
        "shell" => (Kind::Shell, None),
        id => match profiles.iter().find(|p| p.id == id) {
            Some(p) => (p.kind.into(), Some(id.to_string())),
            None => bail!("Unknown profile: {id} (usage: {NEW_USAGE})"),
        },
    };
    if tokens.len() > 3 {
        bail!("Too many arguments (usage: {NEW_USAGE})");
    }
    let Some(&name) = tokens.get(1) else {
        bail!("A session name is required (usage: {NEW_USAGE})");
    };
    validate_name(name)?;
    let model = tokens.get(2).map(|m| m.to_string());
    if kind == Kind::Shell && model.is_some() {
        bail!("Can't set a model for a shell session");
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
    use crate::profiles::ProfileKind;
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
            pending: None,
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
        // The color is the first 8 digits of the project_key hash modulo 6 (a value worked out separately with shasum).
        assert_eq!((r[1].color, r[2].color), (1, 5));
        // The all-projects cell is on if any project has a Needs approval session.
        assert!(r[0].approval);
        // Sorted by name, and several sessions of the same project are still one cell.
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
        // A path with no name (the root) shows as '?'.
        assert_eq!(rail(&[], Path::new("/"))[1].initial, '?');
    }

    #[test]
    fn folder_name_is_the_last_component_or_the_whole_root() {
        assert_eq!(folder_name(Path::new("/p/kyoto")), "kyoto");
        assert_eq!(folder_name(Path::new("/")), "/");
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
        // Done and Ended are one group, and within a group rows are ordered by name. Empty groups are left out.
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
        // If the user expands the Idle group, those rows are in the order too.
        assert_eq!(order(&expand(g.clone(), &[Idle], None)), vec![1, 0, 2]);
        // A group that started expanded can be collapsed.
        assert!(order(&expand(g.clone(), &[Working], None)).is_empty());
        // The group of the attached session is expanded regardless of the default or the user's collapsing.
        assert_eq!(order(&expand(g.clone(), &[], Some(2))), vec![1, 0, 2]);
        assert_eq!(order(&expand(g, &[Idle, Working], Some(2))), vec![0, 2]);
    }

    fn info(id: &str, kind: ProfileKind) -> ProfileInfo {
        ProfileInfo {
            id: id.into(),
            kind,
            label: id.into(),
            color: "#e5c07b".into(),
            chip: "X".into(),
            default: false,
            logged_in: true,
        }
    }

    fn profiles() -> Vec<ProfileInfo> {
        vec![
            info("claude-a", ProfileKind::Claude),
            info("claude-b", ProfileKind::Claude),
            info("team", ProfileKind::Claude),
            info("codex", ProfileKind::Codex),
        ]
    }

    fn parse(text: &str) -> Result<NewSession> {
        parse_new(text, Path::new("/p"), &profiles())
    }

    #[test]
    fn parse_new_uses_profiles() {
        let n = parse("team t1 opus").unwrap();
        assert_eq!(
            (
                n.kind,
                n.account.as_deref(),
                n.name.as_str(),
                n.model.as_deref()
            ),
            (Kind::Claude, Some("team"), "t1", Some("opus"))
        );
        let s = parse("shell dev").unwrap();
        assert_eq!((s.kind, s.account), (Kind::Shell, None));
        let e = parse("nope x").unwrap_err();
        assert!(e.to_string().contains("Unknown profile: nope"), "{e}");
        // Only the given profiles exist: without a list, only shell can be started.
        assert!(parse_new("claude-a x", Path::new("/p"), &[]).is_err());
        assert!(parse_new("shell x", Path::new("/p"), &[]).is_ok());
    }

    #[test]
    fn parse_new_variants() {
        let n = parse("claude-b rust-core opus").unwrap();
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
        assert_eq!(parse("shell dev").unwrap().account, None);
        let a = parse("  claude-a   x  ").unwrap();
        assert_eq!(
            (a.kind, a.account.as_deref(), a.model),
            (Kind::Claude, Some("claude-a"), None)
        );
        let c = parse("codex x gpt-5").unwrap();
        assert_eq!(
            (c.kind, c.account.as_deref(), c.model.as_deref()),
            (Kind::Codex, Some("codex"), Some("gpt-5"))
        );
        assert!(parse("gpt x").is_err());
        assert!(parse("codex").is_err());
        assert!(parse("").is_err());
        assert!(parse("shell dev opus").is_err());
        assert!(parse("claude-a x opus extra").is_err());
    }

    #[test]
    fn parse_new_rejects_invalid_name() {
        for bad in ["shell -x", "claude-a a/b", "codex 한글"] {
            let e = parse(bad).unwrap_err();
            assert!(e.to_string().contains("session name"), "{bad}: {e}");
        }
    }
}
