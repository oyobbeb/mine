//! 가운데 위 탭 줄: 고른 프로젝트의 세션을 이름순으로 놓고 끝에 "＋"를 둔다. 탭에는 상태 기호, 이름, 칩이 붙는다.

use gpui::prelude::FluentBuilder;
use gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    div, px, rgb,
};
use gpui_component::h_flex;
use mine_core::view::folder_name;

use crate::model;
use crate::sidebar::{account_chip, state_color};
use crate::theme::*;
use crate::workspace::Workspace;

/// 탭 줄 높이.
pub const HEIGHT: f32 = 36.;

pub fn render(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let m = &ws.model;
    let launcher = m.launcher_open();
    // 전체를 보면 프로젝트마다 t1이 있어 이름만으로는 탭을 가릴 수 없다. 이때는 폴더 이름을 앞에 흐리게 붙인다.
    let all = m.filter.is_none();
    let tabs = m.tabs().into_iter().enumerate().map(|(k, i)| {
        let s = &m.sessions[i];
        // 실행 화면이 덮고 있으면 "＋"가 고른 탭이다.
        let selected = !launcher && m.attached.as_ref() == Some(&s.meta.id);
        let id = s.meta.id.clone();
        h_flex()
            .id(("tab", k))
            .flex_shrink_0()
            .h(px(26.))
            .px_2p5()
            .gap_1p5()
            .rounded(px(6.))
            .cursor_pointer()
            .text_size(px(13.))
            .text_color(rgb(if selected { TITLE } else { MUTED }))
            .when(selected, |this| this.bg(rgb(SELECTED_BG)))
            .hover(|style| style.bg(rgb(SELECTED_BG)))
            .child(
                div()
                    .text_color(rgb(state_color(&s.state)))
                    .child(model::glyph(&s.state)),
            )
            .when(all, |this| {
                this.child(
                    div()
                        .text_color(rgb(FAINT))
                        .child(format!("{} ·", folder_name(&s.meta.project))),
                )
            })
            .child(s.meta.name.clone())
            .children(account_chip(s.meta.account.as_deref()))
            .on_click(cx.listener(move |ws, _, window, cx| ws.attach(&id, window, cx)))
    });
    let plus = div()
        .id("new-tab")
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .size(px(26.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_size(px(15.))
        .text_color(rgb(if launcher { TITLE } else { FAINT }))
        .when(launcher, |this| this.bg(rgb(SELECTED_BG)))
        .hover(|style| style.bg(rgb(SELECTED_BG)))
        .child("＋")
        .on_click(cx.listener(|ws, _, window, cx| ws.new_task(window, cx)));
    h_flex()
        .id("tabs")
        .flex_shrink_0()
        .h(px(HEIGHT))
        .px_2()
        .gap_1()
        .overflow_x_scroll()
        .bg(rgb(BAR_BG))
        .border_b_1()
        .border_color(rgb(BORDER))
        .children(tabs)
        .child(plus)
}
