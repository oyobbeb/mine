//! The parts of the launcher (start screen) that don't depend on drawing: the rows and their keys, choosing a new name, the session
//! spec a picked row creates, and shortening paths. The rows come from the profile list the daemon sends ([`ProfileInfo`]).

use std::collections::HashSet;
use std::path::Path;

use crate::profiles::{ProfileInfo, ProfileKind};
use crate::session::{Kind, NewSession, SessionInfo};

/// What can be picked on the start screen. `Profile` holds a profile id. Only `Typed` does not create a session right away; it takes
/// the profile, name and model as typed input ([`view::parse_new`](crate::view::parse_new) format).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    Profile(String),
    Shell,
    Typed,
}

impl Choice {
    /// Name shown as text on a launcher row: the profile's label (the id if it isn't in `profiles`), `shell`, or a description of
    /// the typed form. The value that creates a session is always the profile id.
    pub fn label<'a>(&'a self, profiles: &'a [ProfileInfo]) -> &'a str {
        match self {
            Choice::Profile(id) => profile(profiles, id).map_or(id, |p| &p.label),
            Choice::Shell => "shell",
            Choice::Typed => "Custom name and model",
        }
    }

    /// The profile id this row starts. `None` for shell and the typed form.
    pub fn account(&self) -> Option<&str> {
        match self {
            Choice::Profile(id) => Some(id),
            Choice::Shell | Choice::Typed => None,
        }
    }
}

/// The profile with this id.
pub fn profile<'a>(profiles: &'a [ProfileInfo], id: &str) -> Option<&'a ProfileInfo> {
    profiles.iter().find(|p| p.id == id)
}

/// The launcher rows in order with the key that picks each: one row per profile (in config order), then shell, then the typed form.
/// The first nine profiles get `1`–`9`; the rest are picked by clicking. Before the list arrives only shell and the typed form are there.
pub fn rows(profiles: &[ProfileInfo]) -> Vec<(Choice, Option<char>)> {
    let keys = ('1'..='9').map(Some).chain(std::iter::repeat(None));
    profiles
        .iter()
        .zip(keys)
        .map(|(p, key)| (Choice::Profile(p.id.clone()), key))
        .chain([(Choice::Shell, Some('s')), (Choice::Typed, Some('n'))])
        .collect()
}

/// The row a key (`1`–`9`, `s`, `n`) picks. Enter is [`enter_choice`].
pub fn choice_of_key(key: char, profiles: &[ProfileInfo]) -> Option<Choice> {
    rows(profiles)
        .into_iter()
        .find(|(_, k)| *k == Some(key))
        .map(|(choice, _)| choice)
}

/// What Enter starts: the default Claude profile, else the first profile. `None` before the list arrives.
pub fn enter_choice(profiles: &[ProfileInfo]) -> Option<Choice> {
    profiles
        .iter()
        .find(|p| p.default && p.kind == ProfileKind::Claude)
        .or(profiles.first())
        .map(|p| Choice::Profile(p.id.clone()))
}

/// The status text when a session on profile `account` can't start because that profile isn't logged in. Every launcher path checks
/// this before sending `New`: a row's key, a click, Enter ([`Choice::account`]) and the typed form (`NewSession::account`). `None`
/// for a shell, a logged-in profile, or an id that isn't in the list (the typed form already reports that).
pub fn refusal(account: Option<&str>, profiles: &[ProfileInfo]) -> Option<String> {
    profile(profiles, account?)
        .filter(|p| !p.logged_in)
        .map(|p| format!("Not logged in. Run: mine profile login {}", p.id))
}

/// The dim note on the row of a profile that isn't logged in.
pub fn login_hint(info: &ProfileInfo) -> Option<String> {
    (!info.logged_in).then(|| format!("Not logged in · mine profile login {}", info.id))
}

/// The smallest `t<N>` (N from 1) that no session of `project` uses. The names of ended sessions count as used too.
pub fn next_name(sessions: &[SessionInfo], project: &Path) -> String {
    let used: HashSet<&str> = sessions
        .iter()
        .filter(|s| s.meta.project == project)
        .map(|s| s.meta.name.as_str())
        .collect();
    let mut n = 1;
    loop {
        let name = format!("t{n}");
        if !used.contains(name.as_str()) {
            return name;
        }
        n += 1;
    }
}

/// The spec of the new session a picked row creates. `Typed` opens input rather than being a spec, and a profile that has left
/// `profiles` since the row was drawn starts nothing, so both are `None`. The daemon checks the profile id again when it starts the session.
pub fn new_session(
    choice: &Choice,
    name: &str,
    project: &Path,
    profiles: &[ProfileInfo],
) -> Option<NewSession> {
    let (kind, account) = match choice {
        Choice::Profile(id) => (profile(profiles, id)?.kind.into(), Some(id.clone())),
        Choice::Shell => (Kind::Shell, None),
        Choice::Typed => return None,
    };
    Some(NewSession {
        project: project.to_path_buf(),
        name: name.to_string(),
        kind,
        account,
        model: None,
    })
}

/// Shortens the home part of a path under home to `~`. If home is `/` or empty, nothing is shortened.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    let rest = home
        .filter(|h| h.parent().is_some())
        .and_then(|h| path.strip_prefix(h).ok());
    match rest {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::session::{Kind, Meta, Restore, SessionState};

    /// A logged-in profile as the daemon sends it.
    fn info(id: &str, kind: ProfileKind, default: bool) -> ProfileInfo {
        ProfileInfo {
            id: id.into(),
            kind,
            label: format!("{id} label"),
            color: "#e5c07b".into(),
            chip: id[..1].to_uppercase(),
            default,
            logged_in: true,
        }
    }

    fn session(project: &str, name: &str, state: SessionState) -> SessionInfo {
        SessionInfo {
            meta: Meta {
                id: format!("{project}-{name}"),
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

    fn names(project: &str, sessions: &[(&str, &str, SessionState)]) -> String {
        let list: Vec<_> = sessions
            .iter()
            .map(|(p, n, st)| session(p, n, st.clone()))
            .collect();
        next_name(&list, Path::new(project))
    }

    #[test]
    fn next_name_is_the_lowest_unused_t_number_in_the_project() {
        use SessionState::*;
        assert_eq!(names("/p", &[]), "t1");
        assert_eq!(names("/p", &[("/p", "t1", Idle)]), "t2");
        // Fill the gap.
        assert_eq!(names("/p", &[("/p", "t1", Idle), ("/p", "t3", Idle)]), "t2");
        // The names of ended sessions count as used too.
        assert_eq!(
            names("/p", &[("/p", "t1", Ended), ("/p", "t2", Done)]),
            "t3"
        );
        // Sessions of other projects are not counted.
        assert_eq!(names("/p", &[("/q", "t1", Idle), ("/q", "t2", Idle)]), "t1");
        // Names that aren't of the form `t<N>` and names with a leading 0 don't block `t1`.
        assert_eq!(
            names("/p", &[("/p", "dev", Idle), ("/p", "t01", Idle)]),
            "t1"
        );
    }

    fn three() -> Vec<ProfileInfo> {
        vec![
            info("claude-a", ProfileKind::Claude, true),
            info("team", ProfileKind::Claude, false),
            info("codex", ProfileKind::Codex, true),
        ]
    }

    fn profile_choice(id: &str) -> Choice {
        Choice::Profile(id.into())
    }

    #[test]
    fn rows_follow_profiles() {
        let rows = rows(&three());
        assert_eq!(
            rows,
            [
                (profile_choice("claude-a"), Some('1')),
                (profile_choice("team"), Some('2')),
                (profile_choice("codex"), Some('3')),
                (Choice::Shell, Some('s')),
                (Choice::Typed, Some('n')),
            ]
        );
        // Each key picks its own row, and nothing else picks anything.
        for (choice, key) in &rows {
            assert_eq!(choice_of_key(key.unwrap(), &three()).as_ref(), Some(choice));
        }
        for other in ['0', '4', 'b', 'x', 'S', ' '] {
            assert_eq!(choice_of_key(other, &three()), None, "{other:?}");
        }
        // Until the profile list arrives, only shell and the typed form are there.
        assert_eq!(
            super::rows(&[]),
            [(Choice::Shell, Some('s')), (Choice::Typed, Some('n'))]
        );
    }

    #[test]
    fn enter_starts_default_claude() {
        // The default Claude profile wins over order, and a default Codex profile doesn't count.
        let list = vec![
            info("codex", ProfileKind::Codex, true),
            info("team", ProfileKind::Claude, false),
            info("claude-a", ProfileKind::Claude, true),
        ];
        assert_eq!(enter_choice(&list), Some(profile_choice("claude-a")));
        // Without a default Claude profile, the first profile.
        let codex_only = vec![
            info("codex", ProfileKind::Codex, true),
            info("codex-team", ProfileKind::Codex, false),
        ];
        assert_eq!(enter_choice(&codex_only), Some(profile_choice("codex")));
        // Before the list arrives Enter does nothing.
        assert_eq!(enter_choice(&[]), None);
    }

    #[test]
    fn only_first_nine_profiles_have_keys() {
        let list: Vec<_> = (1..=11)
            .map(|n| info(&format!("p{n}"), ProfileKind::Claude, false))
            .collect();
        let rows = rows(&list);
        assert_eq!(rows.len(), 13);
        assert_eq!(rows[8], (profile_choice("p9"), Some('9')));
        assert_eq!(rows[9], (profile_choice("p10"), None));
        assert_eq!(rows[10], (profile_choice("p11"), None));
        assert_eq!(rows[11], (Choice::Shell, Some('s')));
        assert_eq!(choice_of_key('9', &list), Some(profile_choice("p9")));
    }

    #[test]
    fn row_labels_follow_profiles_and_leave_shell_and_typed_alone() {
        let list = three();
        assert_eq!(profile_choice("team").label(&list), "team label");
        // An id that isn't in the list (it was removed after drawing) shows as the id.
        assert_eq!(profile_choice("gone").label(&list), "gone");
        assert_eq!(Choice::Shell.label(&list), "shell");
        assert_eq!(Choice::Typed.label(&list), "Custom name and model");
    }

    #[test]
    fn not_logged_in_profiles_refuse_with_the_login_command() {
        let mut list = three();
        list[1].logged_in = false;
        // A row and a typed id go through the same check.
        let refused = Some("Not logged in. Run: mine profile login team");
        assert_eq!(
            refusal(profile_choice("team").account(), &list).as_deref(),
            refused
        );
        assert_eq!(refusal(Some("team"), &list).as_deref(), refused);
        assert_eq!(
            login_hint(&list[1]).as_deref(),
            Some("Not logged in · mine profile login team")
        );
        for ok in [profile_choice("claude-a"), Choice::Shell, Choice::Typed] {
            assert_eq!(refusal(ok.account(), &list), None, "{ok:?}");
        }
        // An id that isn't in the list is the typed form's "Unknown profile", not a refusal.
        assert_eq!(refusal(Some("gone"), &list), None);
        assert_eq!(login_hint(&list[0]), None);
    }

    #[test]
    fn choices_become_new_session_specs() {
        let list = three();
        let spec = |c: Choice| new_session(&c, "t4", Path::new("/p"), &list);
        let got = |c| {
            let s = spec(c).unwrap();
            (s.kind, s.account, s.name, s.model, s.project)
        };
        assert_eq!(
            got(profile_choice("team")),
            (
                Kind::Claude,
                Some("team".into()),
                "t4".into(),
                None,
                PathBuf::from("/p")
            )
        );
        assert_eq!(
            (
                got(profile_choice("codex")).0,
                got(profile_choice("codex")).1
            ),
            (Kind::Codex, Some("codex".into()))
        );
        assert_eq!(
            (got(Choice::Shell).0, got(Choice::Shell).1),
            (Kind::Shell, None)
        );
        assert!(spec(Choice::Typed).is_none());
        // A profile that left the list since it was drawn starts nothing.
        assert!(spec(profile_choice("gone")).is_none());
    }

    #[test]
    fn tilde_abbreviates_only_paths_under_home() {
        let home = Some(Path::new("/Users/me"));
        let t = |p: &str, h| tilde(Path::new(p), h);
        assert_eq!(t("/Users/me/dev/mine", home), "~/dev/mine");
        assert_eq!(t("/Users/me", home), "~");
        assert_eq!(t("/Users/me2/dev", home), "/Users/me2/dev");
        assert_eq!(t("/srv/app", home), "/srv/app");
        assert_eq!(t("/Users/me/dev", None), "/Users/me/dev");
        // If home is `/` or empty, no path is shortened to `~`.
        assert_eq!(t("/srv/app", Some(Path::new("/"))), "/srv/app");
        assert_eq!(t("/srv/app", Some(Path::new(""))), "/srv/app");
    }
}
