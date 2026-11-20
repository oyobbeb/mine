// 이후 태스크가 쓰기 전까지 공개 함수 일부가 사용되지 않는다.
#![allow(dead_code)]

mod paths;
mod project;
mod proto;
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let state = paths::State::from_env()?;
    match cli.cmd {
        Some(Cmd::Projects) => {
            for p in project::registered(&state)? {
                println!("{}", p.display());
            }
        }
        None => Cli::command().print_help()?,
    }
    Ok(())
}
