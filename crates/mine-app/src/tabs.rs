//! The tab strip at the top of the center: the selected project's sessions in name order, with a "＋" at the end.
//! Each tab carries a status symbol, a name, and a chip.

use gpui::prelude::FluentBuilder;
use gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    div, px, rgb,
};
use gpui_component::h_flex;
use mine_core::view::folder_name;

use crate::model;
use crate::sidebar::{account_chip, answer_buttons, state_color};
use crate::theme::*;
use crate::workspace::Workspace;

/// Height of the tab strip.
pub const HEIGHT: f32 = 36.;

pub fn render(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let m = &ws.model;
    let launcher = m.launcher_open();
    // With all projects shown, every project has a t1, so the name alone can't tell the tabs apart. In that case the folder name is put in front, dimmed.
    let all = m.filter.is_none();
    let tabs = m.tabs().into_iter().enumerate().map(|(k, i)| {
        let s = &m.sessions[i];
        // When the launcher covers the view, "＋" is the selected tab.
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
            .children(account_chip(s.meta.account.as_deref(), &m.profiles))
            .children(
                m.answerable(s)
                    .map(|request| answer_buttons(&s.meta.id, request, cx)),
            )
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
