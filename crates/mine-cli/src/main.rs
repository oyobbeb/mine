use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use mine_core::conn::Conn;
use mine_core::proto::{FromDaemon, ToDaemon};
use mine_core::session::{Kind, NewSession, SessionInfo, SessionState, session_id};
use mine_core::{hooks, paths, project};

mod client;
mod daemon;
mod hold;

#[derive(Parser)]
#[command(name = "mine", version, about = "AI coding agent session multiplexer")]
struct Cli {
    /// Project directory. If omitted, it is looked up from the current directory.
    #[arg(long)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the registered project paths, one per line.
    Projects,
    /// Run the daemon. Other commands start it automatically if it isn't running.
    Daemon,
    /// Print the session list. Each line is tab-separated: `symbol project-folder name account state waiting-text`.
    Ls,
    /// Create a new session.
    New {
        /// Session name, unique within the project.
        name: String,
        /// Session kind.
        #[arg(long, default_value = "shell", value_parser = parse_kind)]
        kind: Kind,
        /// Account.
        #[arg(long, value_parser = ["claude-a", "claude-b", "codex"])]
        account: Option<String>,
        /// Model.
        #[arg(long)]
        model: Option<String>,
    },
    /// Close a session. The session directory and its history are kept.
    Kill {
        /// Session name.
        name: String,
    },
    /// Open the desktop app: the installed mine.app, else a `mine-app` next to `mine` or on PATH.
    App,
    /// Holder process that owns a session's PTY. The daemon starts it.
    #[command(hide = true)]
    Hold {
        /// Unix socket path the daemon connects to.
        #[arg(long)]
        sock: PathBuf,
        /// File path to write the child's exit code to.
        #[arg(long)]
        exit: PathBuf,
        /// Working directory the child starts in.
        #[arg(long)]
        cwd: PathBuf,
        /// Environment variable to add for the child (`K=V`). Can be given multiple times.
        #[arg(long = "env", value_name = "K=V", value_parser = parse_env)]
        env: Vec<(String, String)>,
        /// Child command and its arguments.
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Called by CLI hooks. Records the hook payload from standard input in the session directory and notifies the daemon.
    #[command(name = "_event", hide = true)]
    Event {
        /// Session directory (`<state directory>/sessions/<id>`).
        dir: PathBuf,
    },
}

fn parse_kind(s: &str) -> Result<Kind, String> {
    match s {
        "claude" => Ok(Kind::Claude),
        "codex" => Ok(Kind::Codex),
        "shell" => Ok(Kind::Shell),
        _ => Err(format!("must be one of claude, codex, shell: {s}")),
    }
}

/// Finds the project root from `--project` or the current directory.
fn project_root(cli_project: Option<PathBuf>, state: &paths::State) -> Result<PathBuf> {
    let start = match cli_project {
        Some(dir) => dir,
        None => std::env::current_dir().context("Couldn't determine the current directory")?,
    };
    project::project_root(&start, state)
}

/// Sends the request and receives the `Sessions` reply.
fn request_sessions(state: &paths::State, msg: &ToDaemon) -> Result<Vec<SessionInfo>> {
    match Conn::connect_or_spawn(state)?.request(msg)? {
        FromDaemon::Sessions { sessions } => Ok(sessions),
        other => bail!("The daemon sent an unexpected reply: {other:?}"),
    }
}

/// Replaces tabs and newlines with spaces so they can't break the line format. An empty value becomes `-`.
fn field(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if s.is_empty() { "-".to_string() } else { s }
}

/// One line of `mine ls`.
fn ls_line(info: &SessionInfo) -> String {
    let (symbol, word) = match info.state {
        SessionState::Approval => ("◆", "approval"),
        SessionState::Working => ("◐", "working"),
        SessionState::Done => ("✓", "done"),
        SessionState::Idle => ("○", "idle"),
        SessionState::Ended => ("✓", "ended"),
    };
    let meta = &info.meta;
    let folder = meta
        .project
        .file_name()
        .unwrap_or(meta.project.as_os_str())
        .to_string_lossy();
    [
        symbol.to_string(),
        field(&folder),
        field(&meta.name),
        field(meta.account.as_deref().unwrap_or("")),
        word.to_string(),
        field(info.waiting.as_deref().unwrap_or("")),
    ]
    .join("\t")
}

/// Splits `K=V` at the first `=`.
fn parse_env(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_string(), value.to_string())),
        _ => Err(format!("must be in `K=V` form: {s}")),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        // The holder process doesn't use the state directory, so it starts even without HOME.
        Some(Cmd::Hold {
            sock,
            exit,
            cwd,
            env,
            argv,
        }) => hold::run(&sock, &exit, &cwd, env, argv)?,
        // A hook must not block or fail the stock CLI, so no error is reflected in the exit code.
        Some(Cmd::Event { dir }) => {
            let _ = paths::State::from_env()
                .and_then(|state| hooks::run_event_cmd(&state, &dir, std::io::stdin().lock()));
        }
        Some(Cmd::App) => run_app()?,
        Some(Cmd::Projects) => {
            let state = paths::State::from_env()?;
            for p in project::registered(&state)? {
                println!("{}", p.display());
            }
        }
        Some(Cmd::Daemon) => {
            let state = paths::State::from_env()?;
            tokio::runtime::Runtime::new()?.block_on(daemon::run(state))?;
        }
        Some(Cmd::Ls) => {
            let state = paths::State::from_env()?;
            for info in request_sessions(&state, &ToDaemon::List)? {
                println!("{}", ls_line(&info));
            }
        }
        Some(Cmd::New {
            name,
            kind,
            account,
            model,
        }) => {
            let state = paths::State::from_env()?;
            let project = project_root(cli.project, &state)?;
            let spec = NewSession {
                project,
                name,
                kind,
                account,
                model,
            };
            request_sessions(&state, &ToDaemon::New { spec })?;
        }
        Some(Cmd::Kill { name }) => {
            let state = paths::State::from_env()?;
            let project = project_root(cli.project, &state)?;
            let id = session_id(&project::project_key(&project), &name);
            request_sessions(&state, &ToDaemon::Kill { id })
                .with_context(|| format!("Couldn't close the session: {name}"))?;
        }
        // With no command it's the client screen. The project is found the same way as for `mine new`.
        None => {
            let state = paths::State::from_env()?;
            let project = project_root(cli.project, &state)?;
            tokio::runtime::Runtime::new()?.block_on(client::run(state, project))?;
        }
    }
    Ok(())
}

/// The desktop app's executable, inside `mine.app/Contents/MacOS` and when installed with cargo.
const APP_BIN: &str = "mine-app";

/// How `mine app` starts the desktop app.
#[derive(Debug, PartialEq)]
enum AppLaunch {
    /// The `.app` bundle, started with `open`.
    Open(PathBuf),
    /// The `mine-app` binary, started detached.
    Exec(PathBuf),
    Missing,
}

/// Picks how to start the desktop app: the first existing bundle, then `mine-app` next to this executable, then the
/// `mine-app` found on PATH (`path_lookup`, already resolved by the caller).
fn app_launch(
    bundles: &[PathBuf],
    exe_dir: Option<&Path>,
    path_lookup: Option<PathBuf>,
) -> AppLaunch {
    if let Some(bundle) = bundles.iter().find(|b| b.is_dir()) {
        return AppLaunch::Open(bundle.clone());
    }
    exe_dir
        .map(|dir| dir.join(APP_BIN))
        .filter(|bin| bin.is_file())
        .or(path_lookup)
        .map_or(AppLaunch::Missing, AppLaunch::Exec)
}

/// Starts the desktop app and returns right away.
fn run_app() -> Result<()> {
    let bundles: Vec<PathBuf> = std::env::home_dir()
        .into_iter()
        .map(|home| home.join("Applications/mine.app"))
        .chain([PathBuf::from("/Applications/mine.app")])
        .collect();
    // Look next to the real file, not next to a symlink to it.
    let exe_dir = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(APP_BIN))
            .find(|bin| bin.is_file())
    });
    match app_launch(&bundles, exe_dir.as_deref(), on_path) {
        AppLaunch::Open(app) => {
            let status = Command::new("open")
                .arg(&app)
                .status()
                .context("Couldn't run open")?;
            if !status.success() {
                bail!("Couldn't open {}: {status}", app.display());
            }
        }
        AppLaunch::Exec(bin) => {
            let mut cmd = Command::new(&bin);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // SAFETY: after fork and before exec, only setsid, which is async-signal-safe, is called.
            unsafe {
                cmd.pre_exec(|| match libc::setsid() {
                    -1 => Err(std::io::Error::last_os_error()),
                    _ => Ok(()),
                });
            }
            // Not waited on: the app outlives this command and launchd adopts it.
            cmd.spawn()
                .with_context(|| format!("Couldn't start {}", bin.display()))?;
        }
        AppLaunch::Missing => {
            eprintln!(
                "Couldn't find mine-app. Build it with scripts/bundle-app.sh or run cargo install --path crates/mine-app"
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn app_launch_prefers_bundle_then_sibling_then_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let bundles = [
            root.join("home/Applications/mine.app"),
            root.join("Applications/mine.app"),
        ];
        let exe_dir = root.join("bin");
        let sibling = exe_dir.join("mine-app");
        let on_path = root.join("path/mine-app");
        let launch = |path| app_launch(&bundles, Some(&exe_dir), path);

        // Nothing exists.
        assert_eq!(launch(None), AppLaunch::Missing);
        assert_eq!(app_launch(&bundles, None, None), AppLaunch::Missing);
        // Only on PATH.
        assert_eq!(
            launch(Some(on_path.clone())),
            AppLaunch::Exec(on_path.clone())
        );
        // Next to this executable beats PATH.
        fs::create_dir_all(&exe_dir).unwrap();
        fs::write(&sibling, "").unwrap();
        assert_eq!(
            launch(Some(on_path.clone())),
            AppLaunch::Exec(sibling.clone())
        );
        assert_eq!(app_launch(&bundles, None, None), AppLaunch::Missing);
        // Any bundle beats the sibling, and the earlier bundle wins.
        fs::create_dir_all(&bundles[1]).unwrap();
        assert_eq!(
            launch(Some(on_path.clone())),
            AppLaunch::Open(bundles[1].clone())
        );
        fs::create_dir_all(&bundles[0]).unwrap();
        assert_eq!(launch(Some(on_path)), AppLaunch::Open(bundles[0].clone()));
    }
}
