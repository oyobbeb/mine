//! Shared code for `mine`. The CLI (`mine-cli`) and the desktop app use the same modules.
//! The daemon, holder process and terminal view (ratatui) are not here; they live in each executable.

pub mod accounts;
pub mod conn;
pub mod env;
pub mod hooks;
pub mod keys;
pub mod launch;
pub mod link;
pub mod paths;
pub mod profiles;
pub mod project;
pub mod proto;
pub mod screen;
pub mod search;
pub mod select;
pub mod session;
pub mod view;
