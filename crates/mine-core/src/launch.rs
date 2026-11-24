//! The parts of the launcher (start screen) that don't depend on drawing: what can be picked, choosing a new name, the session spec a picked row creates, and shortening paths.

use std::collections::HashSet;
use std::path::Path;

use crate::accounts::Labels;
use crate::session::{Kind, NewSession, SessionInfo};

/// What can be picked on the start screen. Only `Typed` does not create a session right away; it takes the kind, name and model as typed input
/// ([`view::parse_new`](crate::view::parse_new) format).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    ClaudeA,
    ClaudeB,
    Codex,
    Shell,
    Typed,
}

impl Choice {
    /// Kind word: the first word of [`view::parse_new`](crate::view::parse_new). Except for shell, it is the same as the session's account name (chip). Custom input (`Typed`)
    /// is `None`.
    pub const fn word(self) -> Option<&'static str> {
        match self {
            Choice::ClaudeA => Some("claude-a"),
            Choice::ClaudeB => Some("claude-b"),
            Choice::Codex => Some("codex"),
            Choice::Shell => Some("shell"),
            Choice::Typed => None,
        }
    }

    /// Name shown as text on a launch-screen row. Rows with an account (claude-a, claude-b, codex) use the name from `labels`, and shell
    /// has no account, so it stays `shell`. Custom input is a description. The word used when creating a session is always [`Choice::word`].
    pub fn label(self, labels: &Labels) -> &str {
        match self {
            Choice::ClaudeA | Choice::ClaudeB | Choice::Codex => {
                labels.get(self.word().unwrap_or_default())
            }
            Choice::Shell => "shell",
            Choice::Typed => "Custom name and model",
        }
    }
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

/// The spec of the new session a picked row creates. `Typed` opens input rather than being a spec, so it is `None`.
/// The daemon checks the profile id when it starts the session.
pub fn new_session(choice: Choice, name: &str, project: &Path) -> Option<NewSession> {
    let kind = match choice {
        Choice::ClaudeA | Choice::ClaudeB => Kind::Claude,
        Choice::Codex => Kind::Codex,
        Choice::Shell => Kind::Shell,
        Choice::Typed => return None,
    };
    Some(NewSession {
        project: project.to_path_buf(),
        name: name.to_string(),
        kind,
        account: (kind != Kind::Shell).then(|| choice.word().unwrap_or_default().to_string()),
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
    use crate::paths::State;
    use crate::session::{Kind, Meta, Restore, SessionState};

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

    #[test]
    fn words_are_the_parse_new_kinds() {
        use Choice::*;
        let words: Vec<_> = [ClaudeA, ClaudeB, Codex, Shell, Typed]
            .into_iter()
            .map(Choice::word)
            .collect();
        assert_eq!(
            words,
            [
                Some("claude-a"),
                Some("claude-b"),
                Some("codex"),
                Some("shell"),
                None
            ]
        );
    }

    #[test]
    fn row_labels_follow_accounts_and_leave_shell_and_typed_alone() {
        let dir = tempfile::tempdir().unwrap();
        let state = State {
            root: dir.path().to_path_buf(),
        };
        let config =
            "[accounts.claude-a]\nlabel = \"personal\"\n[accounts.shell]\nlabel = \"terminal\"\n";
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let labels = Labels::load(&state, None);
        let row = |c: Choice| c.label(&labels);
        // With a label, the label; without one, the auto name (codex) or the id (claude-b).
        assert_eq!(row(Choice::ClaudeA), "personal");
        assert_eq!(row(Choice::ClaudeB), "claude-b");
        assert_eq!(row(Choice::Codex), "Codex");
        // Shell has no account, so it stays as is even if it has a label.
        assert_eq!(row(Choice::Shell), "shell");
        assert_eq!(row(Choice::Typed), "Custom name and model");
        // The default when names weren't read is the kind word. The word that creates a session is independent of the name.
        let ids = Labels::default();
        assert_eq!(Choice::ClaudeA.label(&ids), "claude-a");
        assert_eq!(Choice::ClaudeA.word(), Some("claude-a"));
    }

    #[test]
    fn choices_become_new_session_specs() {
        let spec = |c, name| new_session(c, name, Path::new("/p"));
        let got = |c| {
            let s = spec(c, "t4").unwrap();
            (s.kind, s.account, s.name, s.model, s.project)
        };
        let p = PathBuf::from("/p");
        assert_eq!(
            got(Choice::ClaudeA),
            (
                Kind::Claude,
                Some("claude-a".into()),
                "t4".into(),
                None,
                p.clone()
            )
        );
        assert_eq!(got(Choice::ClaudeB).1.as_deref(), Some("claude-b"));
        assert_eq!(
            (got(Choice::Codex).0, got(Choice::Codex).1.as_deref()),
            (Kind::Codex, Some("codex"))
        );
        assert_eq!(
            (got(Choice::Shell).0, got(Choice::Shell).1),
            (Kind::Shell, None)
        );
        assert!(spec(Choice::Typed, "t4").is_none());
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
