// 이후 태스크가 쓰기 전까지 공개 함수 일부가 사용되지 않는다.
#![allow(dead_code)]

mod hold;
mod paths;
mod project;
mod proto;
mod screen;
mod session;

use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

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
        Some(Cmd::Projects) => {
            let state = paths::State::from_env()?;
            for p in project::registered(&state)? {
                println!("{}", p.display());
            }
        }
        None => Cli::command().print_help()?,
    }
    Ok(())
}
