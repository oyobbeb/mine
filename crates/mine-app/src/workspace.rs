//! 뿌리 뷰: 제목줄 아래에 왼쪽 목록 | 가운데 | 접힌 Changes 자리를 둔다.
//!
//! 이 뷰는 창보다 오래 산다. 창을 닫아도 앱 전역(`main::MainView`)이 쥐고 있다가 Dock으로 다시 연 창에 그린다.

use std::path::PathBuf;

use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use gpui::prelude::FluentBuilder;
use gpui::{Context, IntoElement, ParentElement, Render, Styled, Task, Window, div, px, rgb};
use gpui_component::{TitleBar, h_flex, v_flex};
use mine_core::link::{DaemonLink, LinkEvent};
use mine_core::proto::ToDaemon;

use crate::model::{Effect, Model};
use crate::sidebar;
use crate::theme::*;

pub struct Workspace {
    pub model: Model,
    tx: UnboundedSender<ToDaemon>,
    /// 프로젝트 고르기를 펼쳤다.
    pub picker_open: bool,
    /// 연결 스레드의 일을 모델에 먹이는 작업. 뷰가 버려지면 함께 멈춘다.
    _pump: Task<()>,
}

impl Workspace {
    /// 연결 스레드의 일을 GPUI 전경 실행기에서 받아 모델에 먹인다.
    pub fn new(model: Model, link: DaemonLink, cx: &mut Context<Self>) -> Self {
        let DaemonLink { tx, mut rx } = link;
        let pump = cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                if this.update(cx, |ws, cx| ws.on_link(ev, cx)).is_err() {
                    break;
                }
            }
        });
        Workspace {
            model,
            tx,
            picker_open: false,
            _pump: pump,
        }
    }

    fn on_link(&mut self, ev: LinkEvent, cx: &mut Context<Self>) {
        let effects = self.model.on_link(ev);
        self.apply(effects);
        cx.notify();
    }

    /// 모델이 시킨 일을 한다. 연결이 없을 때 보낸 메시지는 연결 스레드가 버린다.
    fn apply(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Send(msg) => {
                    let _ = self.tx.unbounded_send(msg);
                }
            }
        }
    }

    pub fn toggle_group(&mut self, i: usize, cx: &mut Context<Self>) {
        self.model.toggle_group(i);
        cx.notify();
    }

    pub fn toggle_picker(&mut self, cx: &mut Context<Self>) {
        self.picker_open = !self.picker_open;
        cx.notify();
    }

    pub fn pick_project(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        self.model.filter = root;
        self.picker_open = false;
        cx.notify();
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        frame().child(
            h_flex()
                .flex_1()
                .min_h_0()
                .when(self.model.sidebar_open, |this| {
                    this.child(sidebar::render(self, cx))
                })
                .child(center())
                // D3의 Changes 칸 자리. D1에서는 접혀 있다.
                .child(div().w(px(0.)).h_full().flex_shrink_0()),
        )
    }
}

/// 가운데 칸. Task 3·4가 탭, 실행 화면, 터미널 칸으로 채운다.
fn center() -> impl IntoElement {
    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgb(WINDOW_BG))
        .text_size(px(14.))
        .text_color(rgb(FAINT))
        .child("세션을 고르세요")
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
