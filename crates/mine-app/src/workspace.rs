//! 뿌리 뷰: 제목줄 아래에 왼쪽 목록 | 가운데(탭과 실행 화면 또는 세션 칸) | 접힌 Changes 자리를 둔다.
//!
//! 이 뷰는 창보다 오래 산다. 창을 닫아도 앱 전역(`main::MainView`)이 쥐고 있다가 Dock으로 다시 연 창에 그린다.
//! 상태는 모두 `Model`에 있고, 이 뷰는 GPUI의 일(단축키, 클릭, 확인 창, 폴더 고르기, 입력 상자)을 모델에 잇는다.

use std::path::{Path, PathBuf};
use std::time::Instant;

use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use gpui::prelude::FluentBuilder;
use gpui::{
    AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, PathPromptOptions, PromptButton, PromptLevel, Render, Styled,
    Subscription, Task, Window, WindowId, div, px, rgb,
};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::{TitleBar, h_flex, v_flex};
use mine_core::launch::Choice;
use mine_core::link::{DaemonLink, LinkEvent};
use mine_core::paths::State;
use mine_core::project;
use mine_core::proto::ToDaemon;
use mine_core::session::SessionState;
use mine_core::view;

use crate::actions::{
    CONTEXT, CloseSession, NewTask, NextProject, NextSession, PrevProject, PrevSession, SelectTab,
    ToggleSidebar,
};
use crate::model::{Close, Effect, Model, PENDING_TIMEOUT};
use crate::terminal::{self, Pane};
use crate::theme::*;
use crate::{launcher, sidebar, tabs};

/// 연결 스레드의 일을 한 번에 이만큼까지 모아 먹인다.
const LINK_BATCH: usize = 256;

pub struct Workspace {
    pub model: Model,
    tx: UnboundedSender<ToDaemon>,
    /// "＋ 프로젝트 추가"가 등록할 상태 폴더.
    state: State,
    /// 실행 화면이 열려 있을 때 단축키와 실행 화면 키를 받는 포커스. 입력 상자가 열려 있으면 상자에 있다.
    /// 세션이 붙어 실행 화면이 닫혀 있으면 터미널 칸(`term.focus`)에 있다. 그 칸은 이 뷰 안에 있어 ⌘ 단축키는
    /// 그대로 온다.
    focus: FocusHandle,
    /// 터미널 칸의 뷰 상태(포커스, 칸 자리, 휠 나머지, 링크 누르기, 동기화 타이머).
    pub(crate) term: Pane,
    /// 프로젝트 고르기를 펼쳤다.
    pub picker_open: bool,
    /// 실행 화면의 `n` 입력 상자와 그 Enter 구독. `model.typing`을 따라 만들고 버린다.
    typed: Option<(Entity<InputState>, Subscription)>,
    /// 답을 기다리는 `New`(그 `since`)에 건 타이머. 같은 `New`에 두 번 걸지 않는다.
    pending_timer: Option<Instant>,
    /// 창이 활성화될 때 계정 이름을 다시 읽으려고 건 구독과 그 창. 이 뷰는 창보다 오래 살아 창마다 새로 건다.
    activation: Option<(WindowId, Subscription)>,
    /// 연결 스레드의 일을 모델에 먹이는 작업. 뷰가 버려지면 함께 멈춘다.
    _pump: Task<()>,
}

impl Workspace {
    /// 연결 스레드의 일을 GPUI 전경 실행기에서 받아 모델에 먹인다. 와 있는 일은 한 번에 모아 먹인다.
    pub fn new(model: Model, state: State, link: DaemonLink, cx: &mut Context<Self>) -> Self {
        let DaemonLink { tx, mut rx } = link;
        let pump = cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                let mut batch = vec![ev];
                while batch.len() < LINK_BATCH
                    && let Ok(ev) = rx.try_recv()
                {
                    batch.push(ev);
                }
                if this.update(cx, |ws, cx| ws.on_link(batch, cx)).is_err() {
                    break;
                }
            }
        });
        Workspace {
            model,
            tx,
            state,
            focus: cx.focus_handle(),
            term: Pane::new(cx),
            picker_open: false,
            typed: None,
            pending_timer: None,
            activation: None,
            _pump: pump,
        }
    }

    /// 연결 스레드의 일을 모델에 먹인다. 출력(`Output`)마다 다시 그리지만, `notify`는 GPUI가 다음 프레임에 한 번으로
    /// 묶는다(창이 더러우면 화면 주사 때 한 번 그린다).
    fn on_link(&mut self, batch: Vec<LinkEvent>, cx: &mut Context<Self>) {
        for ev in batch {
            let effects = self.model.on_link(ev);
            self.apply(effects);
        }
        self.watch_sync(cx);
        cx.notify();
    }

    /// 모델이 시킨 일을 한다. 모델은 연결이 없을 때 `New`·`Attach`·`Input`·`Resize`를 내지 않는다. 그래도 끊긴 걸
    /// 모델이 알기 전에 보낸 것은 연결 스레드가 다시 붙을 때 버린다.
    pub(crate) fn apply(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Send(msg) => {
                    let _ = self.tx.unbounded_send(msg);
                }
            }
        }
    }

    /// 창이 활성화되면 계정 이름을 다시 읽도록 이 창에 건다. 그릴 때마다 불러도 창마다 한 번만 건다
    /// (Dock으로 다시 연 창은 새 창이라 다시 건다).
    ///
    /// ponytail: 파일 감시는 하지 않는다. 별칭을 고치거나 계정을 바꾼 뒤 창으로 돌아오면 반영된다.
    fn watch_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = window.window_handle().window_id();
        if self
            .activation
            .as_ref()
            .is_some_and(|(seen, _)| *seen == id)
        {
            return;
        }
        let subscription = cx.observe_window_activation(window, |ws, window, cx| {
            if window.is_window_active() {
                ws.reload_labels(cx);
            }
        });
        self.activation = Some((id, subscription));
    }

    /// 계정 이름을 다시 읽고, 바뀌었으면 다시 그린다.
    fn reload_labels(&mut self, cx: &mut Context<Self>) {
        let home = std::env::home_dir();
        if self.model.reload_labels(&self.state, home.as_deref()) {
            cx.notify();
        }
    }

    /// 실행 화면의 입력 상자. 열려 있을 때만 있다.
    pub fn typed_input(&self) -> Option<&Entity<InputState>> {
        self.typed.as_ref().map(|(input, _)| input)
    }

    /// 모델을 바꾼 뒤에 부른다. 입력 상자를 `model.typing`에 맞춰 열거나 닫고 다시 그린다.
    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.typing && self.typed.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(view::NEW_USAGE));
            let enter =
                cx.subscribe_in(&input, window, |ws, input, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        let text = input.read(cx).value().to_string();
                        let effects = ws.model.submit_typed(&text);
                        ws.apply(effects);
                        ws.changed(window, cx);
                    }
                });
            input.update(cx, |input, cx| input.focus(window, cx));
            self.typed = Some((input, enter));
        } else if !self.model.typing {
            self.close_typed(window, cx);
        }
        if self.model.launcher_open() {
            if self.typed.is_none() {
                // 실행 화면이 열리면 그 키(↵ b x s n, Esc)와 단축키가 이 뷰에 오게 포커스를 가져온다. 터미널 칸에
                // 남은 포커스는 그 칸이 그려지지 않으면 키를 받지 못한다.
                window.focus(&self.focus);
            }
        } else {
            // 세션에 붙어 실행 화면이 닫혔다(목록·탭 누르기, ⌘1–9, ⌘[ ⌘], Esc). 키가 세션으로 가게 칸에 준다.
            window.focus(&self.term.focus);
        }
        self.watch_pending(cx);
        cx.notify();
    }

    /// 보낸 `New`마다 [`PENDING_TIMEOUT`] 뒤에 답이 왔는지 본다. 오지 않았으면 모델이 버리고 알린다.
    fn watch_pending(&mut self, cx: &mut Context<Self>) {
        let Some(since) = self.model.pending.as_ref().map(|p| p.since) else {
            return;
        };
        if self.pending_timer == Some(since) {
            return;
        }
        self.pending_timer = Some(since);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PENDING_TIMEOUT).await;
            let _ = this.update(cx, |ws, cx| {
                if ws.model.expire_pending(Instant::now()) {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 입력 상자를 버린다. 포커스가 상자에 있었으면 이 뷰로 되돌려 단축키와 실행 화면 키가 계속 오게 한다.
    fn close_typed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((input, _)) = self.typed.take()
            && input.read(cx).focus_handle(cx).contains_focused(window, cx)
        {
            window.focus(&self.focus);
        }
    }

    pub fn launch(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        let effects = self.model.launch(choice);
        self.apply(effects);
        self.changed(window, cx);
    }

    pub fn attach(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let effects = self.model.attach(id);
        self.apply(effects);
        self.changed(window, cx);
    }

    pub fn new_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.new_task();
        self.changed(window, cx);
    }

    pub fn toggle_group(&mut self, state: &SessionState, cx: &mut Context<Self>) {
        self.model.toggle_group(state);
        cx.notify();
    }

    pub fn toggle_picker(&mut self, cx: &mut Context<Self>) {
        self.picker_open = !self.picker_open;
        cx.notify();
    }

    pub fn pick_project(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        self.model.set_filter(root);
        self.picker_open = false;
        cx.notify();
    }

    /// "＋ 프로젝트 추가": 폴더 하나를 골라 등록하고 그 프로젝트를 고른다.
    pub fn add_project(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("프로젝트로 추가".into()),
        });
        cx.spawn(async move |this, cx| {
            // 취소했거나 고르기 창을 열지 못했다.
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |ws, cx| {
                ws.register(&dir);
                cx.notify();
            });
        })
        .detach();
    }

    /// 고른 폴더를 실제 경로로 등록하고 등록 목록을 다시 읽는다. 홈 폴더와 `/`는 `project::register`가 등록하지 않으니
    /// 목록에 없고, 모델이 그 사유를 띄운다.
    fn register(&mut self, dir: &Path) {
        let added = dir
            .canonicalize()
            .map_err(anyhow::Error::from)
            .and_then(|root| {
                project::register(&self.state, &root)?;
                Ok((root, project::registered(&self.state)?))
            });
        match added {
            Ok((root, registered)) => self.model.project_added(root, registered),
            Err(e) => self.model.message = Some(format!("프로젝트를 추가하지 못했어요: {e:#}")),
        }
    }

    /// 실행 화면의 키. Esc는 입력 상자나 세션 위에 연 실행 화면을 닫는다(입력 상자의 Esc도 여기로 올라온다).
    /// 줄을 고르는 키는 입력 상자가 닫혀 있을 때만 받는다.
    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.launcher_open() {
            return;
        }
        let key = &ev.keystroke;
        if launcher::is_escape(key) {
            self.model.dismiss();
            self.changed(window, cx);
            cx.stop_propagation();
        } else if !self.model.typing
            && let Some(choice) = launcher::choice_of_key(key)
        {
            self.launch(choice, window, cx);
            cx.stop_propagation();
        }
    }

    fn on_new_task(&mut self, _: &NewTask, window: &mut Window, cx: &mut Context<Self>) {
        self.new_task(window, cx);
    }

    /// ⌘W. 세션을 닫을 때는 확인 창(`window.prompt`)의 답을 받아 모델에 넘긴다.
    fn on_close(&mut self, _: &CloseSession, window: &mut Window, cx: &mut Context<Self>) {
        match self.model.close() {
            Close::Launcher => self.changed(window, cx),
            Close::Window => window.remove_window(),
            Close::Nothing => {}
            Close::Confirm(name) => {
                let answer = window.prompt(
                    PromptLevel::Warning,
                    &format!("{name} 세션을 닫을까요?"),
                    Some("세션의 프로세스를 끝내고 목록에서 빼요."),
                    &close_buttons(),
                    cx,
                );
                cx.spawn(async move |this, cx| {
                    // 창이 닫혀 답이 오지 않아도 취소로 본다.
                    let yes = close_confirmed(answer.await.ok());
                    let _ = this.update(cx, |ws, cx| {
                        let effects = ws.model.confirm_close(yes);
                        ws.apply(effects);
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    fn on_select_tab(&mut self, tab: &SelectTab, window: &mut Window, cx: &mut Context<Self>) {
        let effects = self.model.select_tab(tab.0);
        self.apply(effects);
        self.changed(window, cx);
    }

    fn step_session(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        let effects = self.model.step_session(delta);
        self.apply(effects);
        self.changed(window, cx);
    }

    fn step_project(&mut self, delta: i32, cx: &mut Context<Self>) {
        self.model.step_project(delta);
        cx.notify();
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.model.sidebar_open = !self.model.sidebar_open;
        cx.notify();
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Workspace {
    /// 그릴 때 포커스를 맞춘다. 창 없이 바뀐 상태(데몬의 일로 세션에 붙거나 붙은 세션이 사라짐)와 처음 연 창(Dock으로
    /// 다시 연 창 포함, 포커스가 없다)을 다룬다. 그릴 때 포커스를 옮기면 다시 그리지 않고 다음 키부터 그리로 간다.
    /// - 실행 화면이 열려 있고 입력 상자가 없으면: 이 뷰. 실행 화면 키와 단축키가 와야 한다.
    /// - 세션이 붙어 칸이 보이면: 포커스가 없거나 이 뷰 자체에 있을 때 칸으로 넘긴다. 목록 묶음 머리나 프로젝트
    ///   고르기를 누르면 마우스 누름이 이 뷰에 포커스를 주는데, 그 뒤에도 키는 세션으로 가야 한다.
    fn fix_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        let nothing = window.focused(cx).is_none();
        if self.model.launcher_open() {
            if nothing || (self.typed.is_none() && !self.focus.is_focused(window)) {
                window.focus(&self.focus);
            }
        } else if nothing || self.focus.is_focused(window) {
            window.focus(&self.term.focus);
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 데몬의 일(새 세션에 붙기)로 입력 상자가 닫히면 그때는 창이 없어 치우지 못한다. 그릴 때 치운다.
        if !self.model.typing {
            self.close_typed(window, cx);
        }
        self.watch_activation(window, cx);
        self.fix_focus(window, cx);
        frame()
            .track_focus(&self.focus)
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_new_task))
            .on_action(cx.listener(Self::on_close))
            .on_action(cx.listener(Self::on_select_tab))
            .on_action(
                cx.listener(|ws, _: &PrevSession, window, cx| ws.step_session(-1, window, cx)),
            )
            .on_action(
                cx.listener(|ws, _: &NextSession, window, cx| ws.step_session(1, window, cx)),
            )
            .on_action(cx.listener(|ws, _: &PrevProject, _, cx| ws.step_project(-1, cx)))
            .on_action(cx.listener(|ws, _: &NextProject, _, cx| ws.step_project(1, cx)))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_key_down(cx.listener(Self::on_key))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .when(self.model.sidebar_open, |this| {
                        this.child(sidebar::render(self, cx))
                    })
                    .child(center(self, window, cx))
                    // D3의 Changes 칸 자리. D1에서는 접혀 있다.
                    .child(div().w(px(0.)).h_full().flex_shrink_0()),
            )
    }
}

/// 닫기 확인 창의 단추. macOS(NSAlert)는 첫 단추에 Return을 건다. ⌘W 뒤 Return이 세션을 끝내지 않도록 첫 단추를
/// 취소로 둔다(Ruling D-R9).
///
/// 취소를 `PromptButton::cancel`로 만들면 GPUI가 그 단추의 키를 Esc로 바꿔 Return이 어느 단추에도 걸리지 않는다.
/// 그래서 보통 단추로 둔다. 대신 Esc는 이 창을 닫지 않는다.
fn close_buttons() -> [PromptButton; 2] {
    [PromptButton::new("취소"), PromptButton::new("닫기")]
}

/// 확인 창의 답(단추 자리)이 닫기(둘째 단추)인가. 창이 닫혀 답이 없으면 취소로 본다.
fn close_confirmed(answer: Option<usize>) -> bool {
    answer == Some(1)
}

/// 가운데 칸: 위에 탭, 아래에 실행 화면이나 붙은 세션의 터미널 칸, 맨 아래에 알림 줄.
fn center(ws: &Workspace, window: &Window, cx: &mut Context<Workspace>) -> impl IntoElement {
    let m = &ws.model;
    v_flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .bg(rgb(WINDOW_BG))
        .child(tabs::render(ws, cx))
        .map(|this| {
            if m.launcher_open() {
                this.child(launcher::render(ws, cx))
            } else {
                this.child(terminal::render(ws, window, cx))
            }
        })
        .when_some(m.message.clone(), |this, message| {
            this.child(
                div()
                    .flex_shrink_0()
                    .px_3()
                    .py_1p5()
                    .bg(rgb(BAR_BG))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .text_size(px(12.))
                    .text_color(rgb(WAITING))
                    .child(message),
            )
        })
}

/// 창 전체 틀: 목업 색의 제목줄과 그 아래 내용.
fn frame() -> gpui::Div {
    v_flex()
        .size_full()
        .bg(rgb(WINDOW_BG))
        .text_color(rgb(TEXT))
        .child(
            TitleBar::new().child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(MUTED))
                    .child("mine"),
            ),
        )
}

/// 앱을 쓸 수 없을 때(데몬을 띄울 `mine`이 없을 때 등) 창에 띄우는 뷰.
pub struct Problem(pub String);

impl Render for Problem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        frame().child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_8()
                .text_size(px(14.))
                .text_color(rgb(TEXT))
                .child(self.0.clone()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_prompt_defaults_to_cancel() {
        // macOS는 첫 단추에 Return을 건다. ⌘W 뒤 Return이 세션을 끝내지 않도록 첫 단추는 취소다(Ruling D-R9).
        let buttons = close_buttons();
        let labels: Vec<_> = buttons.iter().map(|b| b.label().to_string()).collect();
        assert_eq!(labels, ["취소", "닫기"]);
        // 취소를 Cancel 단추로 만들면 GPUI가 그 키를 Esc로 바꿔 Return이 아무 단추에도 걸리지 않는다.
        assert!(matches!(buttons[0], PromptButton::Other(_)));
        assert!(!close_confirmed(Some(0)));
        assert!(close_confirmed(Some(1)));
        assert!(!close_confirmed(None));
    }
}
