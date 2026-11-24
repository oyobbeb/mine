//! Project detection: the nearest Git working tree root, else a registered directory, else the starting directory.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};

use crate::paths::{State, ensure_dir, sha256_hex_prefix};

/// Walks up from `start` to find the project root.
///
/// `.git` marks a root whether it is a directory or a file (a file means a separate worktree). Nested repositories and separate worktrees
/// follow the nearest marker, so each is its own project. Registered directories are checked in the same order.
/// If nothing is found by the top, the starting directory (resolved to its real path) is the project.
/// The home directory and `/` don't count as registered directories ([`register`]).
pub fn project_root(start: &Path, state: &State) -> Result<PathBuf> {
    project_root_in(start, state, home().as_deref())
}

fn project_root_in(start: &Path, state: &State, home: Option<&Path>) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("Couldn't resolve the path: {}", start.display()))?;
    let found = start
        .ancestors()
        .find(|dir| dir.join(".git").exists() || is_registered(state, dir, home));
    Ok(found.unwrap_or(&start).to_path_buf())
}

/// The home directory resolved to its real path. Uses `std::env::home_dir()` if `HOME` is empty or unset.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| !h.as_os_str().is_empty())
        .or_else(std::env::home_dir)
        .and_then(|h| h.canonicalize().ok())
}

/// The first 16 hex chars of the SHA-256 of the real path bytes.
pub fn project_key(root: &Path) -> String {
    sha256_hex_prefix(root.as_os_str().as_bytes(), 16)
}

/// The home directory (`home` is a real path) and `/` are not registered as projects. If they were, every directory under home without a Git repository
/// would be grouped into the home project. `root` must be a real path.
fn is_never_registered(root: &Path, home: Option<&Path>) -> bool {
    root == Path::new("/") || home == Some(root)
}

/// So that a home registration file left by an earlier version doesn't pull in subdirectories, home and `/` are treated as unregistered even if the file exists.
fn is_registered(state: &State, root: &Path, home: Option<&Path>) -> bool {
    !is_never_registered(root, home) && registration_file(state, root).is_file()
}

fn registration_file(state: &State, root: &Path) -> PathBuf {
    state
        .projects_dir()
        .join(format!("{}.json", project_key(root)))
}

/// Registers a project. Writes the real path string as JSON to `projects/<key>.json`. The home directory and `/` are not registered and
/// it returns success. A session started in home still has home as its project, but no registration is left.
/// Writes to a temporary file in the same directory and then renames it, so a reader never sees a half-written file.
pub fn register(state: &State, root: &Path) -> Result<()> {
    register_in(state, root, home().as_deref())
}

fn register_in(state: &State, root: &Path, home: Option<&Path>) -> Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let root = root
        .canonicalize()
        .with_context(|| format!("Couldn't resolve the path: {}", root.display()))?;
    if is_never_registered(&root, home) {
        return Ok(());
    }
    let text = root
        .to_str()
        .ok_or_else(|| anyhow!("Can't register a non-UTF-8 path: {}", root.display()))?;
    ensure_dir(&state.projects_dir())?;
    let file = registration_file(state, &root);
    // The extension isn't `.json`, so `registered` doesn't read it.
    let tmp = state.projects_dir().join(format!(
        ".{}.{}.{}.tmp",
        project_key(&root),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tmp, serde_json::to_string(text)?)
        .and_then(|()| fs::rename(&tmp, &file))
        .with_context(|| format!("Couldn't write the registration file: {}", file.display()))
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
}

/// Returns the registered project paths sorted. A registration file that can't be read or is malformed is warned about and skipped.
pub fn registered(state: &State) -> Result<Vec<PathBuf>> {
    let dir = state.projects_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("Couldn't read {}", dir.display())),
    };
    let mut roots = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let parsed = fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(serde_json::from_str::<String>(&text)?));
        match parsed {
            Ok(root) => roots.push(PathBuf::from(root)),
            Err(e) => eprintln!(
                "Warning: skipping the registration file: {} ({e})",
                path.display()
            ),
        }
    }
    roots.sort();
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::State;
    use std::path::Path;

    /// A short temporary directory under `/tmp`, and the state rooted at `s` inside it.
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
    fn registered_skips_malformed_entries() {
        let (td, st) = fixture();
        let t = td.path();
        std::fs::create_dir_all(t.join("a")).unwrap();
        register(&st, &t.join("a")).unwrap();
        std::fs::write(st.projects_dir().join("broken.json"), "not json").unwrap();
        assert_eq!(registered(&st).unwrap(), vec![real(&t.join("a"))]);
        // Registration leaves no temporary file.
        let names: Vec<_> = std::fs::read_dir(st.projects_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().all(|n| n.ends_with(".json")), "{names:?}");
    }

    #[test]
    fn key_is_16_hex_and_stable() {
        let k = project_key(Path::new("/tmp/x"));
        assert_eq!(k.len(), 16);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(k, project_key(Path::new("/tmp/x")));
        assert_ne!(k, project_key(Path::new("/tmp/y")));
    }

    /// A temporary directory that has a home directory `h`. The home it returns is a real path.
    fn fake_home(t: &Path) -> PathBuf {
        std::fs::create_dir_all(t.join("h/sub/x")).unwrap();
        real(&t.join("h"))
    }

    #[test]
    fn registering_home_or_root_is_a_noop() {
        let (td, st) = fixture();
        let home = fake_home(td.path());
        register_in(&st, &home, Some(&home)).unwrap();
        register_in(&st, Path::new("/"), Some(&home)).unwrap();
        assert!(registered(&st).unwrap().is_empty());
        assert!(!is_registered(&st, &home, None));
        assert!(!is_registered(&st, Path::new("/"), None));
        // If it isn't home, the same call registers it.
        register_in(&st, &home.join("sub"), Some(&home)).unwrap();
        assert_eq!(registered(&st).unwrap(), vec![home.join("sub")]);
    }

    #[test]
    fn stale_home_registration_does_not_capture_subfolders() {
        let (td, st) = fixture();
        let home = fake_home(td.path());
        // A home registration file left by an earlier version.
        std::fs::create_dir_all(st.projects_dir()).unwrap();
        std::fs::write(
            registration_file(&st, &home),
            serde_json::to_string(home.to_str().unwrap()).unwrap(),
        )
        .unwrap();
        // Contrast: if home is unknown, the old registration pulls subdirectories into home.
        assert_eq!(
            project_root_in(&home.join("sub/x"), &st, None).unwrap(),
            home
        );
        // If home is known, a subdirectory without .git is its own project, and a session started in home has home as its project.
        assert_eq!(
            project_root_in(&home.join("sub/x"), &st, Some(&home)).unwrap(),
            home.join("sub/x")
        );
        assert_eq!(project_root_in(&home, &st, Some(&home)).unwrap(), home);
        // A Git repository under home and a registered directory are still found as before.
        std::fs::create_dir_all(home.join("g/.git")).unwrap();
        assert_eq!(
            project_root_in(&home.join("g"), &st, Some(&home)).unwrap(),
            home.join("g")
        );
    }
}
