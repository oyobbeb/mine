//! `mine` 라이브러리. 바이너리(`src/main.rs`)와 통합 테스트가 같은 모듈을 쓴다.

// 이후 태스크가 쓰기 전까지 공개 함수 일부가 사용되지 않는다.
#![allow(dead_code)]

pub mod conn;
pub mod daemon;
pub mod hold;
pub mod hooks;
pub mod paths;
pub mod project;
pub mod proto;
pub mod screen;
pub mod session;
