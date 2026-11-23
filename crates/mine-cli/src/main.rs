use std::path::PathBuf;

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
#[command(name = "mine", version, about = "AI 코딩 에이전트 세션 멀티플렉서")]
struct Cli {
    /// 프로젝트 폴더. 없으면 현재 위치에서 찾는다.
    #[arg(long)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 등록된 프로젝트 경로를 한 줄씩 출력한다.
    Projects,
    /// 데몬을 실행한다. 다른 명령이 데몬이 없으면 알아서 띄운다.
    Daemon,
    /// 세션 목록. 한 줄에 `기호 프로젝트폴더 이름 계정 상태 대기문구`를 탭으로 나눠 쓴다.
    Ls,
    /// 새 세션을 만든다.
    New {
        /// 프로젝트 안에서 고유한 세션 이름.
        name: String,
        /// 세션 종류.
        #[arg(long, default_value = "shell", value_parser = parse_kind)]
        kind: Kind,
        /// 계정.
        #[arg(long, value_parser = ["claude-a", "claude-b", "codex"])]
        account: Option<String>,
        /// 모델.
        #[arg(long)]
        model: Option<String>,
    },
    /// 세션을 닫는다. 세션 폴더와 기록은 남는다.
    Kill {
        /// 세션 이름.
        name: String,
    },
    /// 세션 PTY를 쥐는 보관 프로세스. 데몬이 띄운다.
    #[command(hide = true)]
    Hold {
        /// 데몬이 붙는 유닉스 소켓 경로.
        #[arg(long)]
        sock: PathBuf,
        /// 자식 종료 코드를 쓸 파일 경로.
        #[arg(long)]
        exit: PathBuf,
        /// 자식이 시작할 작업 폴더.
        #[arg(long)]
        cwd: PathBuf,
        /// 자식에게 더할 환경 변수(`K=V`). 여러 번 줄 수 있다.
        #[arg(long = "env", value_name = "K=V", value_parser = parse_env)]
        env: Vec<(String, String)>,
        /// 자식 명령과 인자.
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// CLI 훅이 부른다. 표준 입력의 훅 페이로드를 세션 폴더에 기록하고 데몬에 알린다.
    #[command(name = "_event", hide = true)]
    Event {
        /// 세션 폴더(`<상태 폴더>/sessions/<id>`).
        dir: PathBuf,
    },
}

fn parse_kind(s: &str) -> Result<Kind, String> {
    match s {
        "claude" => Ok(Kind::Claude),
        "codex" => Ok(Kind::Codex),
        "shell" => Ok(Kind::Shell),
        _ => Err(format!("claude, codex, shell 중 하나여야 한다: {s}")),
    }
}

/// `--project` 또는 현재 폴더에서 프로젝트 루트를 찾는다.
fn project_root(cli_project: Option<PathBuf>, state: &paths::State) -> Result<PathBuf> {
    let start = match cli_project {
        Some(dir) => dir,
        None => std::env::current_dir().context("현재 폴더를 알 수 없다")?,
    };
    project::project_root(&start, state)
}

/// 요청을 보내고 `Sessions` 답을 받는다.
fn request_sessions(state: &paths::State, msg: &ToDaemon) -> Result<Vec<SessionInfo>> {
    match Conn::connect_or_spawn(state)?.request(msg)? {
        FromDaemon::Sessions { sessions } => Ok(sessions),
        other => bail!("데몬이 예상하지 못한 답을 보냈다: {other:?}"),
    }
}

/// 탭과 줄바꿈이 줄 형식을 깨지 않게 공백으로 바꾼다. 비어 있으면 `-`다.
fn field(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if s.is_empty() { "-".to_string() } else { s }
}

/// `mine ls`의 한 줄.
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

/// `K=V`를 첫 `=` 기준으로 나눈다.
fn parse_env(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_string(), value.to_string())),
        _ => Err(format!("`K=V` 형식이어야 한다: {s}")),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        // 보관 프로세스는 상태 폴더를 쓰지 않으므로 HOME이 없어도 뜬다.
        Some(Cmd::Hold {
            sock,
            exit,
            cwd,
            env,
            argv,
        }) => hold::run(&sock, &exit, &cwd, env, argv)?,
        // 훅은 순정 CLI를 막거나 실패시키면 안 되므로 어떤 오류도 종료 코드에 반영하지 않는다.
        Some(Cmd::Event { dir }) => {
            let _ = paths::State::from_env()
                .and_then(|state| hooks::run_event_cmd(&state, &dir, std::io::stdin().lock()));
        }
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
                .with_context(|| format!("세션을 닫지 못했다: {name}"))?;
        }
        // 명령이 없으면 클라이언트 화면이다. 프로젝트는 `mine new`와 같은 방법으로 찾는다.
        None => {
            let state = paths::State::from_env()?;
            let project = project_root(cli.project, &state)?;
            tokio::runtime::Runtime::new()?.block_on(client::run(state, project))?;
        }
    }
    Ok(())
}
