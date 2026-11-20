//! 프로젝트 판별: 가장 가까운 Git 작업 트리 루트, 없으면 등록된 폴더, 그것도 없으면 시작 폴더.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::paths::{State, ensure_dir, sha256_hex_prefix};

/// `start`에서 위로 올라가며 프로젝트 루트를 찾는다.
///
/// `.git`은 폴더든 파일이든 루트 표시다(파일이면 별도 worktree). 중첩 저장소와 별도 worktree는
/// 가장 가까운 표시를 따르므로 각각 독립 프로젝트가 된다. 등록된 폴더도 같은 순서로 확인한다.
/// 끝까지 못 찾으면 (실제 경로로 바꾼) 시작 폴더가 프로젝트다.
pub fn project_root(start: &Path, state: &State) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("경로를 확인할 수 없다: {}", start.display()))?;
    let found = start
        .ancestors()
        .find(|dir| dir.join(".git").exists() || is_registered(state, dir));
    Ok(found.unwrap_or(&start).to_path_buf())
}

/// 실제 경로 바이트의 SHA-256 16진수 앞 16자.
pub fn project_key(root: &Path) -> String {
    sha256_hex_prefix(root.as_os_str().as_bytes(), 16)
}

fn is_registered(state: &State, root: &Path) -> bool {
    registration_file(state, root).is_file()
}

fn registration_file(state: &State, root: &Path) -> PathBuf {
    state
        .projects_dir()
        .join(format!("{}.json", project_key(root)))
}

/// 프로젝트를 등록한다. `projects/<key>.json`에 실제 경로 문자열을 JSON으로 적는다.
pub fn register(state: &State, root: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("경로를 확인할 수 없다: {}", root.display()))?;
    let text = root
        .to_str()
        .ok_or_else(|| anyhow!("UTF-8이 아닌 경로는 등록할 수 없다: {}", root.display()))?;
    ensure_dir(&state.projects_dir())?;
    let file = registration_file(state, &root);
    fs::write(&file, serde_json::to_string(text)?)
        .with_context(|| format!("등록 파일을 쓸 수 없다: {}", file.display()))?;
    Ok(())
}

/// 등록된 프로젝트 경로를 정렬해서 돌려준다.
pub fn registered(state: &State) -> Result<Vec<PathBuf>> {
    let dir = state.projects_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("읽을 수 없다: {}", dir.display())),
    };
    let mut roots = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("읽을 수 없다: {}", path.display()))?;
        let root: String = serde_json::from_str(&text)
            .with_context(|| format!("등록 파일 형식이 잘못됐다: {}", path.display()))?;
        roots.push(PathBuf::from(root));
    }
    roots.sort();
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::State;
    use std::path::Path;

    /// `/tmp` 아래 짧은 임시 폴더와, 그 안 `s`를 루트로 하는 상태.
    fn fixture() -> (tempfile::TempDir, State) {
        let t = tempfile::Builder::new()
            .prefix("m")
            .tempdir_in("/tmp")
            .unwrap();
        let st = State {
            root: t.path().join("s"),
        };
        (t, st)
    }

    fn real(p: &Path) -> std::path::PathBuf {
        p.canonicalize().unwrap()
    }

    #[test]
    fn subdir_resolves_to_git_root() {
        let (td, st) = fixture();
        let t = td.path();
        std::fs::create_dir_all(t.join("r/.git")).unwrap();
        std::fs::create_dir_all(t.join("r/a/b")).unwrap();
        assert_eq!(
            project_root(&t.join("r/a/b"), &st).unwrap(),
            real(&t.join("r"))
        );
    }

    #[test]
    fn worktree_with_git_file_is_its_own_project() {
        let (td, st) = fixture();
        let t = td.path();
        std::fs::create_dir_all(t.join("r/.git")).unwrap();
        std::fs::create_dir_all(t.join("r/wt/sub")).unwrap();
        std::fs::write(t.join("r/wt/.git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(
            project_root(&t.join("r/wt/sub"), &st).unwrap(),
            real(&t.join("r/wt"))
        );
    }

    #[test]
    fn nested_repo_is_separate() {
        let (td, st) = fixture();
        let t = td.path();
        std::fs::create_dir_all(t.join("r/.git")).unwrap();
        std::fs::create_dir_all(t.join("r/inner/.git")).unwrap();
        std::fs::create_dir_all(t.join("r/inner/x")).unwrap();
        assert_eq!(
            project_root(&t.join("r/inner/x"), &st).unwrap(),
            real(&t.join("r/inner"))
        );
        assert_eq!(project_root(&t.join("r"), &st).unwrap(), real(&t.join("r")));
    }

    #[test]
    fn plain_folder_is_itself() {
        let (td, st) = fixture();
        let t = td.path();
        std::fs::create_dir_all(t.join("plain/a")).unwrap();
        assert_eq!(
            project_root(&t.join("plain/a"), &st).unwrap(),
            real(&t.join("plain/a"))
        );
    }

    #[test]
    fn registered_ancestor_wins_without_git() {
        let (td, st) = fixture();
        let t = td.path();
        std::fs::create_dir_all(t.join("p/x/y")).unwrap();
        register(&st, &t.join("p")).unwrap();
        assert_eq!(
            project_root(&t.join("p/x/y"), &st).unwrap(),
            real(&t.join("p"))
        );
    }

    #[test]
    fn nearest_marker_wins_between_git_and_registration() {
        let (td, st) = fixture();
        let t = td.path();
        std::fs::create_dir_all(t.join("p/g/.git")).unwrap();
        std::fs::create_dir_all(t.join("p/g/x")).unwrap();
        register(&st, &t.join("p")).unwrap();
        assert_eq!(
            project_root(&t.join("p/g/x"), &st).unwrap(),
            real(&t.join("p/g"))
        );
    }

    #[test]
    fn registered_lists_sorted_real_paths_and_is_idempotent() {
        let (td, st) = fixture();
        let t = td.path();
        assert!(registered(&st).unwrap().is_empty());
        std::fs::create_dir_all(t.join("b")).unwrap();
        std::fs::create_dir_all(t.join("a")).unwrap();
        register(&st, &t.join("b")).unwrap();
        register(&st, &t.join("a")).unwrap();
        register(&st, &t.join("a")).unwrap();
        assert_eq!(
            registered(&st).unwrap(),
            vec![real(&t.join("a")), real(&t.join("b"))]
        );
        let file = st
            .projects_dir()
            .join(format!("{}.json", project_key(&real(&t.join("a")))));
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            serde_json::to_string(real(&t.join("a")).to_str().unwrap()).unwrap()
        );
    }

    #[test]
    fn key_is_16_hex_and_stable() {
        let k = project_key(Path::new("/tmp/x"));
        assert_eq!(k.len(), 16);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(k, project_key(Path::new("/tmp/x")));
        assert_ne!(k, project_key(Path::new("/tmp/y")));
    }
}
