//! The left list: New task, the project picker, sessions grouped by state, and Add project. All state lives in `Model`;
//! this module only draws and wires clicks through to `Workspace`.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Axis, Context, FontWeight, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px, rgb,
};
use gpui_component::{StyledExt, h_flex, v_flex};
use mine_core::launch;
use mine_core::profiles::ProfileInfo;
use mine_core::session::{SessionInfo, SessionState};
use mine_core::view::{Group, folder_name};

use crate::model::{self, Model, ProjectItem};
use crate::theme::*;
use crate::workspace::Workspace;

/// List width.
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
                .child("No sessions")
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

/// The strip above the list while the connection is down. The reason it dropped (if the version differs, how to restart the daemon) is written small below.
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
                .child("Disconnected from the daemon · reconnecting…"),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(FAINT))
                .child(reason.to_string()),
        )
}

/// "＋ New task". Clicking opens the launcher (same as ⌘N).
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
        .child(div().flex_1().text_color(rgb(TITLE)).child("＋ New task"))
        .child(div().text_size(px(12.)).text_color(rgb(FAINT)).child("⌘N"))
        .on_click(cx.listener(|ws, _, window, cx| ws.new_task(window, cx)))
}

/// Project picker. Clicking expands it downward to show "All projects" and the projects; choosing one collapses it.
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
        .map_or_else(|| "All projects".to_string(), folder_name)
}

fn approval_mark() -> impl IntoElement {
    div().text_color(rgb(APPROVAL)).child("◆")
}

/// Group header: `▾ ◆ Needs approval  1`. Clicking collapses or expands it.
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
            // Holds the state, not the position. Even if a new list arrives after drawing and the groups shift, the clicked group is the one that changes.
            let state = g.state.clone();
            cx.listener(move |ws, _, _, cx| ws.toggle_group(&state, cx))
        })
}

/// The color of a status symbol. Tabs use the same color.
pub fn state_color(state: &SessionState) -> u32 {
    match state {
        SessionState::Approval => APPROVAL,
        SessionState::Working => WORKING,
        SessionState::Done | SessionState::Ended => DONE,
        SessionState::Idle => FAINT,
    }
}

/// A session row. The first line is the name and chip, the second is `project · waiting text or status`. Clicking attaches to that session.
fn session_row(m: &Model, i: usize, cx: &mut Context<Workspace>) -> impl IntoElement {
    let s: &SessionInfo = &m.sessions[i];
    // Holds the id, not the position. Even if a new list arrives after drawing and the rows shift, it attaches to the clicked session.
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
                .children(account_chip(s.meta.account.as_deref(), &m.profiles)),
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

/// The chip of a session's profile: its letter on its color. A shell, or a profile that isn't in the list, has none. Tabs use it too.
pub fn account_chip(
    account: Option<&str>,
    profiles: &[ProfileInfo],
) -> Option<impl IntoElement + use<>> {
    launch::profile(profiles, account?).map(|p| chip(p.chip.clone(), p.rgb()))
}

/// A chip: a profile's letter on its color (`0xrrggbb`). The launcher rows use it too.
pub fn chip(letter: String, color: u32) -> impl IntoElement {
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
        .child(letter)
}

/// "＋ Add project". Clicking picks a folder and registers it as a project.
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
        .child("＋ Add project")
        .on_click(cx.listener(|ws, _, _, cx| ws.add_project(cx)))
}
