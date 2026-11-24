//! 시작 화면(런처)의 화면과 상관없는 부분: 고를 수 있는 것, 새 이름 정하기, 고른 줄이 만들 세션 명세, 경로 줄이기.

use std::collections::HashSet;
use std::path::Path;

use crate::accounts::Labels;
use crate::session::{NewSession, SessionInfo};
use crate::view;

/// 시작 화면에서 고를 수 있는 것. `Typed`만 세션을 바로 만들지 않고, 종류·이름·모델을 직접 입력받는다
/// ([`view::parse_new`] 형식).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    ClaudeA,
    ClaudeB,
    Codex,
    Shell,
    Typed,
}

impl Choice {
    /// 종류 말: [`view::parse_new`]의 첫 낱말이다. shell 말고는 세션의 계정 이름(칩)과 같다. 직접 입력(`Typed`)은
    /// `None`이다.
    pub const fn word(self) -> Option<&'static str> {
        match self {
            Choice::ClaudeA => Some("claude-a"),
            Choice::ClaudeB => Some("claude-b"),
            Choice::Codex => Some("codex"),
            Choice::Shell => Some("shell"),
            Choice::Typed => None,
        }
    }

    /// 실행 화면 줄에 글로 보일 이름. 계정이 있는 줄(claude-a, claude-b, codex)은 `labels`의 이름이고, shell은
    /// 계정이 없어 `shell` 그대로다. 직접 입력은 설명이다. 세션을 만들 때 쓰는 말은 늘 [`Choice::word`]다.
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

/// `project`의 세션이 쓰지 않은 가장 작은 `t<N>`(N은 1부터). 끝난 세션의 이름도 쓴 것으로 센다.
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

/// 고른 줄이 만들 새 세션 명세. `Typed`는 명세가 아니라 입력을 여니 `None`이다.
pub fn new_session(choice: Choice, name: &str, project: &Path) -> Option<NewSession> {
    let word = choice.word()?;
    view::parse_new(&format!("{word} {name}"), project).ok()
}

/// 홈 아래 경로의 홈 부분을 `~`로 줄인다. 홈이 `/`나 빈 값이면 줄이지 않는다.
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
        // 빈 자리를 채운다.
        assert_eq!(names("/p", &[("/p", "t1", Idle), ("/p", "t3", Idle)]), "t2");
        // 끝난 세션의 이름도 쓴 것으로 센다.
        assert_eq!(
            names("/p", &[("/p", "t1", Ended), ("/p", "t2", Done)]),
            "t3"
        );
        // 다른 프로젝트의 세션은 세지 않는다.
        assert_eq!(names("/p", &[("/q", "t1", Idle), ("/q", "t2", Idle)]), "t1");
        // `t<N>` 꼴이 아닌 이름과 앞에 0이 붙은 이름은 `t1`을 막지 않는다.
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
        // 넷 모두 `parse_new`가 받는 말이고, 계정은 shell만 없다.
        for c in [ClaudeA, ClaudeB, Codex, Shell] {
            let word = c.word().unwrap();
            let spec = view::parse_new(&format!("{word} x"), Path::new("/p")).unwrap();
            assert_eq!(spec.account.as_deref(), (c != Shell).then_some(word));
        }
    }

    #[test]
    fn row_labels_follow_accounts_and_leave_shell_and_typed_alone() {
        let dir = tempfile::tempdir().unwrap();
        let state = State {
            root: dir.path().to_path_buf(),
        };
        let config = "[accounts.claude-a]\nlabel = \"개인\"\n[accounts.shell]\nlabel = \"쉘\"\n";
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let labels = Labels::load(&state, None);
        let row = |c: Choice| c.label(&labels);
        // 별칭이 있으면 별칭, 없으면 자동 이름(codex)이나 id(claude-b)다.
        assert_eq!(row(Choice::ClaudeA), "개인");
        assert_eq!(row(Choice::ClaudeB), "claude-b");
        assert_eq!(row(Choice::Codex), "Codex");
        // shell은 계정이 없어 별칭이 있어도 그대로다.
        assert_eq!(row(Choice::Shell), "shell");
        assert_eq!(row(Choice::Typed), "Custom name and model");
        // 이름을 읽지 않은 기본값은 종류 말이다. 세션을 만드는 말은 이름과 무관하다.
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
        // 홈이 `/`나 빈 값이면 모든 경로를 `~`로 줄이지 않는다.
        assert_eq!(t("/srv/app", Some(Path::new("/"))), "/srv/app");
        assert_eq!(t("/srv/app", Some(Path::new(""))), "/srv/app");
    }
}
