//! The mine desktop app (`mine-app`). It connects to the daemon as a client and shows the session list grouped by state.
//!
//! Closing the window does not quit the app: it stays in the Dock (macOS convention), and clicking the Dock icon
//! reopens the window with the same view. ⌘Q quits only the app; the daemon and sessions stay.

mod actions;
mod launcher;
mod model;
mod sidebar;
mod tabs;
mod terminal;
mod theme;
mod workspace;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    AnyView, App, AppContext, Application, Bounds, Global, Menu, MenuItem, WindowBounds,
    WindowOptions, px, size,
};
use gpui_component::{Root, TitleBar};
use mine_core::env::login_env;
use mine_core::link;
use mine_core::paths::State;
use mine_core::{project, session};

use actions::Quit;
use model::Model;
use workspace::{Problem, Workspace};

/// The longest the app waits at startup for the login shell to print its environment.
const LOGIN_ENV_WAIT: Duration = Duration::from_secs(3);

/// The root view that outlives the window. A window reopened from the Dock icon draws this view too.
struct MainView(AnyView);

impl Global for MainView {}

fn main() {
    let app = Application::new();
    // Called when the Dock icon is clicked while no window is visible. If a minimized window remains, restore it instead of opening a new one.
    app.on_reopen(|cx| {
        match cx.windows().first() {
            Some(window) => {
                let _ = window.update(cx, |_, window, _| window.activate_window());
            }
            None => open_window(cx),
        }
        cx.activate(true);
    });
    app.run(|cx| {
        gpui_component::init(cx);
        theme::install(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys(actions::bindings());
        cx.set_menus(vec![Menu {
            name: "mine".into(),
            items: vec![MenuItem::action("Quit mine", Quit)],
        }]);
        let view = root_view(cx);
        cx.set_global(MainView(view));
        open_window(cx);
        cx.activate(true);
    });
}

/// The view that looks up the state folder and `mine` and starts the daemon link. If it can't find them, the view shows the reason.
fn root_view(cx: &mut App) -> AnyView {
    let state = match State::from_env() {
        Ok(state) => state,
        Err(e) => return cx.new(|_| Problem(format!("{e:#}"))).into(),
    };
    // On macOS, current_exe() returns the symlink path as is. Look next to the real file, not next to the link (~/bin/mine-app).
    let exe_dir = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    // Launched from the Dock, the app has launchd's bare environment. The daemon (and so every session) gets the login shell's
    // environment instead, or the app's own if that fails. `mine` is looked up in the login shell's PATH first, then the app's.
    // ponytail: read on the main thread before the window opens, so a slow profile delays startup by up to LOGIN_ENV_WAIT.
    let login = login_env(&session::login_shell(), LOGIN_ENV_WAIT);
    let login_path = login.iter().flatten().find(|(k, _)| k == "PATH");
    let path = login_path
        .map(|(_, v)| OsString::from(v))
        .into_iter()
        .chain(std::env::var_os("PATH"))
        .reduce(|mut a, b| {
            a.push(":");
            a.push(b);
            a
        });
    let Some(mine) = link::find_mine(std::env::var_os("MINE_BIN"), exe_dir.as_deref(), path) else {
        let message = "Couldn't find the mine executable · install it with cargo install --path crates/mine-cli";
        return cx.new(|_| Problem(message.into())).into();
    };
    let (current, home_fallback) = current_project(&state);
    let mut model = Model::new(current);
    model.home_fallback = home_fallback;
    model.registered = project::registered(&state).unwrap_or_default();
    let link = link::start(state.clone(), mine, login);
    cx.new(|cx| Workspace::new(model, state, link, cx)).into()
}

/// The project of the folder the app was launched from. Launched from the Dock the folder is `/`, so it looks in the home directory instead, and also reports whether it made that substitution.
fn current_project(state: &State) -> (PathBuf, bool) {
    let (start, home_fallback) = match std::env::current_dir() {
        Ok(dir) if dir != Path::new("/") => (dir, false),
        _ => (
            std::env::home_dir().unwrap_or_else(|| PathBuf::from("/")),
            true,
        ),
    };
    let root = project::project_root(&start, state).unwrap_or(start);
    (root, home_fallback)
}

/// Opens the 1200×800 window in the center of the screen. The title bar is drawn by hand in the mockup colors.
fn open_window(cx: &mut App) {
    let view = cx.global::<MainView>().0.clone();
    let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("mine".into()),
            ..TitleBar::title_bar_options()
        }),
        window_min_size: Some(size(px(640.), px(400.))),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        cx.new(|cx| Root::new(view, window, cx))
    });
    if let Err(e) = opened {
        eprintln!("Couldn't open the window: {e:#}");
    }
}
