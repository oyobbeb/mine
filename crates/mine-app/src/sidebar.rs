//! The left list: New task, the project picker, sessions grouped by state, and Add project. All state lives in `Model`;
//! this module only draws and wires clicks through to `Workspace`.
//!
//! Sizes follow the mockup. Its text uses the browser's normal line height, so the line heights here are what that renders in the
//! system font (a fallback glyph such as "＋" or "◆" makes the line a pixel taller). GPUI's own default would be 1.618 em.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, Axis, Context, Div, ElementId, FontWeight, Hsla, InteractiveElement,
    IntoElement, MouseButton, ParentElement, StatefulInteractiveElement, Styled, div, px, rgb,
};
use gpui_component::{StyledExt, Theme as UiTheme, h_flex, v_flex};
use mine_core::launch;
use mine_core::profiles::ProfileInfo;
use mine_core::session::{SessionInfo, SessionState};
use mine_core::view::{Group, folder_name};

use crate::glass;
use crate::model::{self, Model, ProjectItem};
use crate::theme::{self, Theme};
use crate::workspace::Workspace;

/// List width. The mockup's panel is `width: 286px` with 12 px of padding at each side in CSS's content-box sizing, so it renders
/// 310 wide and its blocks 286.
pub const WIDTH: f32 = 310.;

/// The gap between the list's blocks, and between a group's header and rows.
const BLOCK_GAP: f32 = 12.;
const GROUP_GAP: f32 = 6.;

pub fn render(ws: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let m = &ws.model;
    let t = theme::current(cx).clone();
    let groups = m.visible_groups();
    let mut list: Vec<AnyElement> = Vec::new();
    if groups.is_empty() && m.connected {
        list.push(
            div()
                .px(px(6.))
                .text_size(px(12.))
                .text_color(t.text_tertiary.hsla())
                .child("No sessions")
                .into_any_element(),
        );
    }
    for (gi, g) in groups.iter().enumerate() {
        let mut block = v_flex().gap(px(GROUP_GAP)).child(group_head(gi, g, &t, cx));
        if !g.collapsed {
            block = block.children(g.rows.iter().map(|&i| session_row(m, i, &t, cx)));
        }
        list.push(block.into_any_element());
    }
    // The panel carries its own shadow outside it, so the clipping is on the content inside.
    glass::panel(&t)
        .w(px(WIDTH))
        .flex_shrink_0()
        .h_full()
        .child(
            v_flex()
                .id("sidebar")
                .size_full()
                .overflow_hidden()
                // The mockup's panel: padding 14 × 12 and 12 between its blocks.
                .py(glass::pad(14.))
                .px(glass::pad(12.))
                .gap(px(BLOCK_GAP))
                .when_some(m.offline.as_deref(), |this, reason| {
                    this.child(offline_banner(reason, &t))
                })
                .child(new_task(&t, cx))
                .child(project_picker(m, ws.picker_open, &t, cx))
                // The groups take the room left above Add project, the mockup's spacer.
                .child(
                    v_flex().id("groups").flex_1().min_h_0().child(
                        v_flex()
                            .gap(px(BLOCK_GAP))
                            .children(list)
                            .scrollable(Axis::Vertical),
                    ),
                )
                .child(add_project(&t, cx)),
        )
}

/// The strip above the list while the connection is down. The reason it dropped (if the version differs, how to restart the daemon) is written small below.
fn offline_banner(reason: &str, t: &Theme) -> impl IntoElement {
    v_flex()
        .px_3()
        .py_2()
        .gap_0p5()
        .border_b_1()
        .border_color(t.edge.hsla())
        .child(
            div()
                .text_size(px(12.))
                .text_color(t.accent_text.hsla())
                .child("Disconnected from the daemon · reconnecting…"),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(t.text_tertiary.hsla())
                .child(reason.to_string()),
        )
}

/// "＋ New task ⌘N", a strong capsule. Clicking opens the launcher (same as ⌘N).
fn new_task(t: &Theme, cx: &mut Context<Workspace>) -> impl IntoElement {
    let capsule = glass::capsule(t, true)
        .id("new-task")
        .flex()
        .items_center()
        .justify_between()
        .py(glass::pad(10.))
        .px(glass::pad(14.))
        .line_height(px(18.))
        .text_size(px(14.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text.hsla())
        .child("＋ New task")
        .child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::NORMAL)
                .text_color(t.text_secondary.hsla())
                .child("⌘N"),
        )
        .on_click(cx.listener(|ws, _, window, cx| ws.new_task(window, cx)));
    glass::hover_edge(capsule, t.highlight)
}

/// Project picker, a clear capsule. Clicking expands it downward to show "All projects" and the projects; choosing one collapses it.
fn project_picker(
    m: &Model,
    open: bool,
    t: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let items = m.projects();
    let current = items
        .iter()
        .find(|p| p.root == m.filter)
        .cloned()
        .unwrap_or(ProjectItem {
            root: m.filter.clone(),
            approval: false,
        });
    // The mockup's "◆ ▾" is one run in the approval color.
    let caret = if open { "▴" } else { "▾" };
    let (mark, mark_color) = if current.approval {
        (format!("◆ {caret}"), t.approval)
    } else {
        (caret.to_string(), t.text_secondary)
    };
    let head = glass::clear_capsule(t)
        .id("picker")
        .flex()
        .items_center()
        .gap_1p5()
        .py(glass::pad(8.))
        .px(glass::pad(14.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(project_name(&current)),
        )
        .child(div().text_color(mark_color.hsla()).child(mark))
        .on_click(cx.listener(|ws, _, _, cx| ws.toggle_picker(cx)));
    let head = glass::hover_fill(head, t);
    let rows = open.then(|| {
        items.into_iter().enumerate().map(|(k, item)| {
            let selected = item.root == m.filter;
            let name = project_name(&item);
            let approval = item.approval;
            let row = glass::row(t, selected)
                .id(("project", k))
                .flex()
                .items_center()
                .gap_1p5()
                .py(glass::pad(6.))
                .px(glass::pad(14.))
                .child(div().flex_1().min_w_0().truncate().child(name))
                .when(approval, |this| this.child(approval_mark(t)))
                .on_click(cx.listener(move |ws, _, _, cx| ws.pick_project(item.root.clone(), cx)));
            if selected {
                glass::hover_edge(row, t.highlight)
            } else {
                glass::hover_fill(row, t)
            }
        })
    });
    v_flex()
        .gap_1()
        .text_size(px(13.))
        .line_height(px(17.))
        .text_color(t.text.hsla())
        .child(head)
        .children(rows.into_iter().flatten())
}

fn project_name(item: &ProjectItem) -> String {
    item.root
        .as_deref()
        .map_or_else(|| "All projects".to_string(), folder_name)
}

fn approval_mark(t: &Theme) -> impl IntoElement {
    div().text_color(t.approval.hsla()).child("◆")
}

/// The group headers' letter spacing, the mockup's 0.06 em at 11 px.
const HEAD_SIZE: f32 = 11.;
const TRACKING: f32 = 0.06 * HEAD_SIZE;

/// `text` at [`HEAD_SIZE`] with [`TRACKING`] after each letter, as CSS letter spacing adds it.
///
/// GPUI 0.2.2 has no letter spacing, so each letter is its own text in a box as wide as the letter's advance plus the tracking. (A
/// text sizes itself to its width rounded up to a whole pixel, which would add about half a pixel per letter.)
// ponytail: this loses kerning between the letters. Use GPUI's own letter spacing if it gets one.
fn tracked(text: &str, cx: &App) -> Vec<Div> {
    let system = cx.text_system();
    let font = system.resolve_font(&gpui::font(UiTheme::global(cx).font_family.clone()));
    text.chars()
        .map(|c| {
            // A lone space would be an empty line; a no-break space keeps its width.
            let c = if c == ' ' { '\u{a0}' } else { c };
            let letter = div().flex_shrink_0().child(c.to_string());
            match system.advance(font, px(HEAD_SIZE), c) {
                Ok(advance) => letter.w(advance.width + px(TRACKING)),
                Err(_) => letter.mr(px(TRACKING)),
            }
        })
        .collect()
}

/// Group header: `◆ NEEDS APPROVAL` with the count at the right, small uppercase secondary text with letter spacing. Clicking
/// collapses or expands it.
fn group_head(gi: usize, g: &Group, t: &Theme, cx: &mut Context<Workspace>) -> impl IntoElement {
    let label = format!(" {}", model::group_label(&g.state).to_uppercase());
    // The mockup's line is 14 px where the symbol comes from a fallback font (◆ ◐), 13 px otherwise.
    let line = match g.state {
        SessionState::Approval | SessionState::Working => 14.,
        _ => 13.,
    };
    h_flex()
        .id(("group", gi))
        .justify_between()
        .px(px(6.))
        .line_height(px(line))
        .text_size(px(HEAD_SIZE))
        .text_color(t.text_secondary.hsla())
        .cursor_pointer()
        .child(
            h_flex()
                .child(
                    div()
                        .mr(px(TRACKING))
                        .text_color(state_color(t, &g.state))
                        .child(model::glyph(&g.state)),
                )
                .children(tracked(&label, cx)),
        )
        .child(h_flex().children(tracked(&g.rows.len().to_string(), cx)))
        .on_click({
            // Holds the state, not the position. Even if a new list arrives after drawing and the groups shift, the clicked group is the one that changes.
            let state = g.state.clone();
            cx.listener(move |ws, _, _, cx| ws.toggle_group(&state, cx))
        })
}

/// The color of a status symbol. Tabs use the same color.
pub fn state_color(t: &Theme, state: &SessionState) -> Hsla {
    match state {
        SessionState::Approval => t.approval,
        SessionState::Working => t.working,
        SessionState::Done | SessionState::Ended => t.done,
        SessionState::Idle => t.idle,
    }
    .hsla()
}

/// A session row. The first line is the name and chip, the second is `project · waiting text or status`. Clicking attaches to that
/// session. A row with a held request is a pending row with Allow and Deny under its lines.
fn session_row(m: &Model, i: usize, t: &Theme, cx: &Context<Workspace>) -> impl IntoElement {
    let s: &SessionInfo = &m.sessions[i];
    // Holds the id, not the position. Even if a new list arrives after drawing and the rows shift, it attaches to the clicked session.
    let id = s.meta.id.clone();
    let selected = m.attached.as_ref() == Some(&s.meta.id);
    let request = m.answerable(s);
    let (status, waiting) = model::status_text(s);
    let row = match request {
        Some(_) => glass::pending_row(t),
        None => glass::row(t, selected),
    }
    .id(("row", i))
    .flex()
    .flex_col()
    .py(glass::pad(10.))
    .px(glass::pad(12.))
    .gap(px(if request.is_some() { 8. } else { 4. }))
    .on_click(cx.listener(move |ws, _, window, cx| ws.attach(&id, window, cx)))
    .child(
        h_flex()
            .justify_between()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(14.))
                    .line_height(px(17.))
                    .text_color(t.text.hsla())
                    .when(selected || request.is_some(), |this| {
                        this.font_weight(FontWeight::SEMIBOLD)
                    })
                    .child(s.meta.name.clone()),
            )
            .children(account_chip(s.meta.account.as_deref(), &m.profiles)),
    )
    .child(
        h_flex()
            .min_w_0()
            .text_size(px(12.))
            .line_height(px(15.))
            .text_color(t.text_secondary.hsla())
            // A no-break space, so the space before the status keeps its width.
            .child(
                div()
                    .flex_shrink_0()
                    .child(format!("{} ·\u{a0}", folder_name(&s.meta.project))),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .when(waiting, |this| this.text_color(t.accent_text.hsla()))
                    .child(status.to_string()),
            ),
    )
    .children(request.map(|request| answer_buttons(&s.meta.id, request, false, t, cx)));
    match (request, selected) {
        (Some(_), _) => glass::hover_edge(row, t.accent_text),
        (None, true) => glass::hover_edge(row, t.highlight),
        (None, false) => glass::hover_fill(row, t),
    }
}

/// The element id of the `label` button (Allow or Deny) for held request `request` of session `session`.
///
/// It must never be a position. gpui remembers a press in progress by the element's id path across redraws, and fires the click from
/// the frame drawn at the release. With an id that is only the label under a positional row, a list update between press and release
/// that moves another held row into this slot would let a press begun on one request finish on the other. Named by session, request
/// and action, a press can only complete on the same request it began on.
///
/// ponytail: the id construction is unit-tested; the click path itself (gpui press state across redraws) is not, since it needs gpui's
/// test context. Add that if the buttons grow more behavior.
fn button_id(label: &str, session: &str, request: u64) -> ElementId {
    ElementId::NamedInteger(format!("{label}:{session}").into(), request)
}

/// Allow and Deny for held request `request` of session `id`. Each answers exactly that request, the one the row or tab showed when it
/// was drawn. A press on them stays on them: it doesn't also select the row or tab underneath. Tabs use it too, `compact`.
///
/// Allow is an accent capsule, Deny a clear one. In a row they share its width; in a tab they are as small as its chip.
pub fn answer_buttons(
    id: &str,
    request: u64,
    compact: bool,
    t: &Theme,
    cx: &Context<Workspace>,
) -> impl IntoElement {
    let button = |label: &'static str, allow: bool| {
        let session = id.to_string();
        let capsule = if allow {
            glass::accent_capsule(t)
                .text_color(t.on_accent.hsla())
                .font_weight(FontWeight::BOLD)
        } else {
            glass::clear_capsule(t)
                .text_color(t.text.hsla())
                .font_weight(FontWeight::SEMIBOLD)
        };
        let button = capsule
            .id(button_id(label, id, request))
            .flex()
            .items_center()
            .justify_center()
            .map(|this| {
                if compact {
                    this.flex_shrink_0()
                        .h(px(18.))
                        .px(glass::pad(8.))
                        .text_size(px(11.))
                } else {
                    this.flex_1()
                        .py(glass::pad(7.))
                        .text_size(px(13.))
                        .line_height(px(16.))
                }
            })
            // The parent's click starts from its own mouse-down, so stopping this one keeps the row from selecting itself.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |ws, _, _, _| ws.answer(&session, request, allow)))
            .child(label);
        // Both have their own edge to light up, so neither adds a fill over the row or tab under it.
        glass::hover_edge(button, t.highlight)
    };
    h_flex()
        .gap(px(if compact { 6. } else { 8. }))
        .child(button("Allow", true))
        .child(button("Deny", false))
}

/// The chip of a session's profile: its letter on its color. A shell, or a profile that isn't in the list, has none. Tabs use it too.
pub fn account_chip(
    account: Option<&str>,
    profiles: &[ProfileInfo],
) -> Option<impl IntoElement + use<>> {
    launch::profile(profiles, account?).map(|p| chip(p.chip.clone(), p.rgb()))
}

/// A chip: a profile's letter on its color (`0xrrggbb`), an 18 px circle. The letter is dark or white, whichever reads better on that
/// color. The launcher rows use it too.
pub fn chip(letter: String, color: u32) -> Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(px(18.))
        .rounded_full()
        .bg(rgb(color))
        .text_color(rgb(theme::chip_text(color)))
        .text_size(px(11.))
        .line_height(px(18.))
        .font_weight(FontWeight::BOLD)
        .child(letter)
}

/// "＋ Add project", a quiet text capsule. Clicking picks a folder and registers it as a project.
fn add_project(t: &Theme, cx: &mut Context<Workspace>) -> impl IntoElement {
    let capsule = div()
        .id("add-project")
        .flex_shrink_0()
        .flex()
        .items_center()
        .rounded_full()
        .py(px(8.))
        .px(px(12.))
        .text_size(px(13.))
        .line_height(px(17.))
        .text_color(t.text_secondary.hsla())
        .child("＋ Add project")
        .on_click(cx.listener(|ws, _, _, cx| ws.add_project(cx)));
    glass::hover_fill(capsule, t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Tint;

    /// The letter is dark or white by contrast with the chip (`theme::chip_text`), so a color a user picks stays readable.
    #[test]
    fn chip_letter_uses_chip_text() {
        let letter = |color: u32| {
            let mut chip = chip("N".into(), color);
            chip.style().text.as_ref().and_then(|text| text.color)
        };
        // Navy gets a white letter, gold a dark one.
        assert_eq!(letter(0x000080), Some(Tint::solid(0xffffff).hsla()));
        assert_eq!(letter(0xe5c07b), Some(Tint::solid(0x1b1d23).hsla()));
    }

    /// A button's id carries the session, the request and the action, so it can't be taken over by another row that moves into the same slot.
    #[test]
    fn answer_button_ids_name_the_session_request_and_action() {
        let allow = button_id("Allow", "k-t1", 5);
        // The same button keeps its id across redraws (a press in progress must still find its element).
        assert_eq!(allow, button_id("Allow", "k-t1", 5));
        // Anything that differs gives another element: the other action, another session, or a newer request of the same session.
        assert_ne!(allow, button_id("Deny", "k-t1", 5));
        assert_ne!(allow, button_id("Allow", "k-t2", 5));
        assert_ne!(allow, button_id("Allow", "k-t1", 6));
    }
}
