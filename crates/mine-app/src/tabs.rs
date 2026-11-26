//! The tab bar, a glass capsule centered in the title-bar row: the selected project's sessions in name order, with a "+" at the end.
//! Each tab is a capsule with a status symbol, a name, and a chip; the active one is a strong capsule.

use gpui::prelude::FluentBuilder;
use gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    div, px,
};
use gpui_component::h_flex;
use mine_core::view::folder_name;

use crate::glass;
use crate::model;
use crate::sidebar::{account_chip, answer_buttons, state_color};
use crate::theme;
use crate::workspace::Workspace;

/// The mockup's tab bar: pills with 6 × 14 px padding around a 16 px line (its 13 px text). A chip or the compact Allow/Deny (18 px)
/// makes a pill 30 tall, like the round "+"; a pill without one is 28, centered. 4 px of padding and 4 px gaps in the bar, so the bar
/// is 38 px, centered in the 52 px title-bar row.
pub const HEIGHT: f32 = 30.;
const LINE: f32 = 16.;

pub fn render(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let m = &ws.model;
    let t = theme::current(cx).clone();
    let launcher = m.launcher_open();
    // With all projects shown, every project has a t1, so the name alone can't tell the tabs apart. In that case the folder name is put in front, dimmed.
    let all = m.filter.is_none();
    let tabs = m.tabs().into_iter().enumerate().map(|(k, i)| {
        let s = &m.sessions[i];
        // When the launcher covers the view, "+" is the selected tab.
        let selected = !launcher && m.attached.as_ref() == Some(&s.meta.id);
        let id = s.meta.id.clone();
        let tab = glass::pill(&t, selected)
            .id(("tab", k))
            .flex()
            .items_center()
            .flex_shrink_0()
            .py(glass::pad(6.))
            .px(glass::pad(14.))
            .line_height(px(LINE))
            .gap_1p5()
            .text_size(px(13.))
            .text_color(if selected { t.text } else { t.text_secondary }.hsla())
            .child(
                div()
                    .text_color(state_color(&t, &s.state))
                    .child(model::glyph(&s.state)),
            )
            .when(all, |this| {
                this.child(
                    div()
                        .text_color(t.text_tertiary.hsla())
                        .child(format!("{} ·", folder_name(&s.meta.project))),
                )
            })
            .child(s.meta.name.clone())
            .children(account_chip(s.meta.account.as_deref(), &m.profiles))
            .children(
                m.answerable(s)
                    .map(|request| answer_buttons(&s.meta.id, request, true, &t, cx)),
            )
            .on_click(cx.listener(move |ws, _, window, cx| ws.attach(&id, window, cx)));
        if selected {
            glass::hover_edge(tab, t.highlight)
        } else {
            glass::hover_fill(tab, &t)
        }
    });
    let plus = glass::pill(&t, launcher)
        .id("new-tab")
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .size(px(HEIGHT))
        .text_size(px(16.))
        .text_color(if launcher { t.text } else { t.text_secondary }.hsla())
        .child("+")
        .on_click(cx.listener(|ws, _, window, cx| ws.new_task(window, cx)));
    let plus = if launcher {
        glass::hover_edge(plus, t.highlight)
    } else {
        glass::hover_fill(plus, &t)
    };
    // The bar occludes the title-bar row under it, so a double click on a tab doesn't also zoom the window. That also hides the frame
    // from its clicks, so the bar reports them as user input itself (presence), on the way down like the frame does. It scrolls
    // inside, because the capsule paints its shadow outside itself.
    glass::capsule(&t, false)
        .occlude()
        .capture_any_mouse_down(cx.listener(|ws, _, _, _| ws.user_input()))
        .flex()
        .items_center()
        .min_w_0()
        .p(glass::pad(4.))
        .child(
            h_flex()
                .id("tabs")
                .min_w_0()
                .gap_1()
                .overflow_x_scroll()
                .children(tabs)
                .child(plus),
        )
}
