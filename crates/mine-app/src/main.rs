//! mine 데스크톱 앱(`mine-app`). 데몬의 클라이언트로 붙어 상태별 세션 목록을 보여 준다.
//!
//! 창을 닫아도 앱은 Dock에 남고(macOS 관례), Dock 아이콘을 누르면 같은 뷰로 창을 다시 연다. ⌘Q는 앱만 끝내고
//! 데몬과 세션은 남는다.

mod actions;
mod launcher;
mod model;
mod sidebar;
mod tabs;
mod theme;
mod workspace;

use std::path::{Path, PathBuf};

use gpui::{
    AnyView, App, AppContext, Application, Bounds, Global, Menu, MenuItem, WindowBounds,
    WindowOptions, px, size,
};
use gpui_component::{Root, TitleBar};
use mine_core::link;
use mine_core::paths::State;
use mine_core::project;

use actions::Quit;
use model::Model;
use workspace::{Problem, Workspace};

/// 창을 닫아도 남는 뿌리 뷰. Dock 아이콘으로 다시 연 창도 이 뷰를 그린다.
struct MainView(AnyView);

impl Global for MainView {}

fn main() {
    let app = Application::new();
    // 보이는 창이 없을 때 Dock 아이콘을 누르면 불린다. 최소화한 창이 남아 있으면 새로 열지 않고 그 창을 되살린다.
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
            items: vec![MenuItem::action("mine 끝내기", Quit)],
        }]);
        let view = root_view(cx);
        cx.set_global(MainView(view));
        open_window(cx);
        cx.activate(true);
    });
}

/// 상태 폴더와 `mine`을 찾아 데몬 연결을 띄운 뷰. 찾지 못하면 그 사유를 보이는 뷰다.
fn root_view(cx: &mut App) -> AnyView {
    let state = match State::from_env() {
        Ok(state) => state,
        Err(e) => return cx.new(|_| Problem(format!("{e:#}"))).into(),
    };
    // macOS의 current_exe()는 심볼릭 링크 경로를 그대로 준다. 링크(~/bin/mine-app)가 아니라 실제 파일 옆에서 찾는다.
    let exe_dir = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let Some(mine) = link::find_mine(
        std::env::var_os("MINE_BIN"),
        exe_dir.as_deref(),
        std::env::var_os("PATH"),
    ) else {
        let message =
            "mine 실행 파일을 찾지 못했어요 · cargo install --path crates/mine-cli로 설치해 주세요";
        return cx.new(|_| Problem(message.into())).into();
    };
    let (current, home_fallback) = current_project(&state);
    let mut model = Model::new(current);
    model.home_fallback = home_fallback;
    model.registered = project::registered(&state).unwrap_or_default();
    let link = link::start(state.clone(), mine, None);
    cx.new(|cx| Workspace::new(model, state, link, cx)).into()
}

/// 앱을 띄운 폴더의 프로젝트. Dock에서 띄우면 폴더가 `/`라서 홈 폴더에서 찾고, 그렇게 대신했는지를 함께 준다.
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

/// 1200×800 창을 화면 가운데에 연다. 제목줄은 목업 색으로 직접 그린다.
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
        eprintln!("창을 열지 못했어요: {e:#}");
    }
}
