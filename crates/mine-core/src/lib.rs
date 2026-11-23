//! `mine` 공용 코드. CLI(`mine-cli`)와 데스크톱 앱이 같은 모듈을 쓴다.
//! 데몬·보관 프로세스·터미널 화면(ratatui)은 여기 없고 각 실행 파일 쪽에 있다.

pub mod accounts;
pub mod conn;
pub mod hooks;
pub mod keys;
pub mod launch;
pub mod link;
pub mod paths;
pub mod project;
pub mod proto;
pub mod screen;
pub mod search;
pub mod select;
pub mod session;
pub mod view;
