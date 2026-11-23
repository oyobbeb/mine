//! 왼쪽 목록: 새 작업, 프로젝트 고르기, 상태별 세션 묶음, 프로젝트 추가. 상태는 모두 `Model`에 있고 여기서는 그리고
//! 누름을 `Workspace`에 잇기만 한다.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Axis, Context, FontWeight, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px, rgb,
};
use gpui_component::{StyledExt, h_flex, v_flex};
use mine_core::session::{SessionInfo, SessionState};
use mine_core::view::{Group, folder_name};

use crate::model::{self, Model, ProjectItem};
use crate::theme::*;
use crate::workspace::Workspace;

/// 목록 폭.
pub const WIDTH: f32 = 260.;

pub fn render(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let m = &ws.model;
    let groups = m.visible_groups();
    let mut list: Vec<AnyElement> = Vec::new();
    if groups.is_empty() && m.connected {
        list.push(
            div()
                .px_3()
                .pt_3()
                .text_size(px(12.))
                .text_color(rgb(FAINT))
                .child("세션이 없어요")
                .into_any_element(),
        );
    }
    for (gi, g) in groups.iter().enumerate() {
        list.push(group_head(gi, g, cx).into_any_element());
        if !g.collapsed {
            list.extend(
                g.rows
                    .iter()
                    .map(|&i| session_row(m, i, cx).into_any_element()),
            );
        }
    }
    v_flex()
        .id("sidebar")
        .w(px(WIDTH))
        .flex_shrink_0()
        .h_full()
        .overflow_hidden()
        .bg(rgb(LIST_BG))
        .border_r_1()
        .border_color(rgb(BORDER))
        .when_some(m.offline.as_deref(), |this, reason| {
            this.child(offline_banner(reason))
        })
        .child(new_task(cx))
        .child(project_picker(m, ws.picker_open, cx))
        .child(
            v_flex()
                .id("groups")
                .flex_1()
                .min_h_0()
                .child(div().pb_2().children(list).scrollable(Axis::Vertical)),
        )
        .child(add_project(cx))
}

/// 연결이 끊긴 동안 목록 위에 두는 띠. 끊긴 사유(버전이 다르면 데몬을 다시 띄우는 방법)를 아래에 작게 적는다.
fn offline_banner(reason: &str) -> impl IntoElement {
    v_flex()
        .px_3()
        .py_2()
        .gap_0p5()
        .bg(rgb(BAR_BG))
        .border_b_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(WAITING))
                .child("데몬 연결이 끊겼어요 · 다시 연결하는 중이에요"),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(FAINT))
                .child(reason.to_string()),
        )
}

/// "＋ 새 작업". 누르면 실행 화면을 연다(⌘N과 같다).
fn new_task(cx: &mut Context<Workspace>) -> impl IntoElement {
    h_flex()
        .id("new-task")
        .mx_2()
        .mt_2()
        .px_2()
        .h(px(30.))
        .rounded(px(6.))
        .text_size(px(13.))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(SELECTED_BG)))
        .child(div().flex_1().text_color(rgb(TITLE)).child("＋ 새 작업"))
        .child(div().text_size(px(12.)).text_color(rgb(FAINT)).child("⌘N"))
        .on_click(cx.listener(|ws, _, window, cx| ws.new_task(window, cx)))
}

/// 프로젝트 고르기. 누르면 아래로 펼쳐 "전체"와 프로젝트들을 보이고, 고르면 접는다.
fn project_picker(m: &Model, open: bool, cx: &mut Context<Workspace>) -> impl IntoElement {
    let items = m.projects();
    let current = items
        .iter()
        .find(|p| p.root == m.filter)
        .cloned()
        .unwrap_or(ProjectItem {
            root: m.filter.clone(),
            approval: false,
        });
    let head = h_flex()
        .id("picker")
        .px_2()
        .h(px(28.))
        .gap_1p5()
        .cursor_pointer()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(rgb(TEXT))
                .child(project_name(&current)),
        )
        .when(current.approval, |this| this.child(approval_mark()))
        .child(
            div()
                .text_color(rgb(FAINT))
                .child(if open { "▴" } else { "▾" }),
        )
        .on_click(cx.listener(|ws, _, _, cx| ws.toggle_picker(cx)));
    let rows = open.then(|| {
        items.into_iter().enumerate().map(|(k, item)| {
            let selected = item.root == m.filter;
            let name = project_name(&item);
            let approval = item.approval;
            h_flex()
                .id(("project", k))
                .px_2()
                .h(px(26.))
                .gap_1p5()
                .cursor_pointer()
                .border_t_1()
                .border_color(rgb(BORDER))
                .when(selected, |this| this.bg(rgb(SELECTED_BG)))
                .hover(|s| s.bg(rgb(SELECTED_BG)))
                .child(div().flex_1().min_w_0().truncate().child(name))
                .when(approval, |this| this.child(approval_mark()))
                .on_click(cx.listener(move |ws, _, _, cx| ws.pick_project(item.root.clone(), cx)))
        })
    });
    v_flex()
        .mx_2()
        .mt_1()
        .mb_1()
        .rounded(px(6.))
        .border_1()
        .border_color(rgb(BORDER))
        .overflow_hidden()
        .text_size(px(13.))
        .child(head)
        .children(rows.into_iter().flatten())
}

fn project_name(item: &ProjectItem) -> String {
    item.root
        .as_deref()
        .map_or_else(|| "전체".to_string(), folder_name)
}

fn approval_mark() -> impl IntoElement {
    div().text_color(rgb(APPROVAL)).child("◆")
}

/// 묶음 머리: `▾ ◆ 승인 대기  1`. 누르면 접히거나 펼쳐진다.
fn group_head(gi: usize, g: &Group, cx: &mut Context<Workspace>) -> impl IntoElement {
    h_flex()
        .id(("group", gi))
        .px_3()
        .pt_3()
        .pb_1()
        .gap_1p5()
        .cursor_pointer()
        .text_size(px(12.))
        .child(
            div()
                .w(px(10.))
                .text_color(rgb(FAINT))
                .child(if g.collapsed { "▸" } else { "▾" }),
        )
        .child(
            div()
                .text_color(rgb(state_color(&g.state)))
                .child(model::glyph(&g.state)),
        )
        .child(
            div()
                .flex_1()
                .text_color(rgb(MUTED))
                .child(model::group_label(&g.state)),
        )
        .child(div().text_color(rgb(FAINT)).child(g.rows.len().to_string()))
        .on_click({
            // 자리가 아니라 상태를 쥔다. 그린 뒤 새 목록이 와 묶음이 밀려도 누른 묶음을 바꾼다.
            let state = g.state.clone();
            cx.listener(move |ws, _, _, cx| ws.toggle_group(&state, cx))
        })
}

/// 상태 기호의 색. 탭도 같은 색을 쓴다.
pub fn state_color(state: &SessionState) -> u32 {
    match state {
        SessionState::Approval => APPROVAL,
        SessionState::Working => WORKING,
        SessionState::Done | SessionState::Ended => DONE,
        SessionState::Idle => FAINT,
    }
}

/// 세션 줄. 첫 줄은 이름과 칩, 둘째 줄은 `프로젝트 · 대기 문구 또는 상태`다. 누르면 그 세션에 붙는다.
fn session_row(m: &Model, i: usize, cx: &mut Context<Workspace>) -> impl IntoElement {
    let s: &SessionInfo = &m.sessions[i];
    // 자리가 아니라 id를 쥔다. 그린 뒤 새 목록이 와 줄이 밀려도 누른 세션에 붙는다.
    let id = s.meta.id.clone();
    let selected = m.attached.as_ref() == Some(&s.meta.id);
    let (status, waiting) = model::status_text(s);
    v_flex()
        .id(("row", i))
        .mx_2()
        .px_2()
        .py_1p5()
        .gap_0p5()
        .rounded(px(6.))
        .cursor_pointer()
        .when(selected, |this| this.bg(rgb(SELECTED_BG)))
        .hover(|s| s.bg(rgb(SELECTED_BG)))
        .on_click(cx.listener(move |ws, _, window, cx| ws.attach(&id, window, cx)))
        .child(
            h_flex()
                .gap_2()
                .text_size(px(13.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(if selected { TITLE } else { TEXT }))
                        .child(s.meta.name.clone()),
                )
                .children(account_chip(s.meta.account.as_deref())),
        )
        .child(
            h_flex()
                .gap_1()
                .min_w_0()
                .text_size(px(12.))
                .text_color(rgb(MUTED))
                .child(div().flex_shrink_0().child(folder_name(&s.meta.project)))
                .child(div().flex_shrink_0().text_color(rgb(FAINT)).child("·"))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .when(waiting, |this| this.text_color(rgb(WAITING)))
                        .child(status.to_string()),
                ),
        )
}

/// 계정 칩(A·B·C). shell처럼 칩이 없는 계정은 `None`이다. 탭에서도 쓴다.
pub fn account_chip(account: Option<&str>) -> Option<impl IntoElement + use<>> {
    let (letter, color) = chip(account)?;
    Some(
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .size(px(16.))
            .rounded(px(3.))
            .bg(rgb(color))
            .text_color(rgb(CHIP_FG))
            .text_size(px(10.))
            .font_weight(FontWeight::BOLD)
            .child(letter),
    )
}

/// "＋ 프로젝트 추가". 누르면 폴더를 골라 프로젝트로 등록한다.
fn add_project(cx: &mut Context<Workspace>) -> impl IntoElement {
    h_flex()
        .id("add-project")
        .flex_shrink_0()
        .px_3()
        .h(px(36.))
        .border_t_1()
        .border_color(rgb(BORDER))
        .text_size(px(12.))
        .text_color(rgb(FAINT))
        .cursor_pointer()
        .hover(|s| s.text_color(rgb(TEXT)))
        .child("＋ 프로젝트 추가")
        .on_click(cx.listener(|ws, _, _, cx| ws.add_project(cx)))
}
