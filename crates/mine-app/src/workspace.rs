//! The root view: below the title bar, the left list | the center (tabs, and the launcher or a session pane) | a collapsed Changes slot.
//!
//! This view outlives the window. When the window is closed, the app-wide `main::MainView` keeps hold of it and draws it
//! into the window reopened from the Dock. All state lives in `Model`; this view wires GPUI's work (shortcuts, clicks,
//! the confirmation dialog, the folder picker, the input box) to the model.

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

/// At most this many events from the link thread are gathered and fed to the model at once.
const LINK_BATCH: usize = 256;

pub struct Workspace {
    pub model: Model,
    tx: UnboundedSender<ToDaemon>,
    /// The state folder that "＋ Add project" registers into.
    state: State,
    /// The focus that receives shortcuts and launcher keys while the launcher is open. While the input box is open, it
    /// is on the box. When a session is attached and the launcher is closed, it is on the terminal pane (`term.focus`).
    /// That pane is inside this view, so ⌘ shortcuts still arrive.
    focus: FocusHandle,
    /// The terminal pane's view state (focus, pane position, wheel remainder, link press, sync timer).
    pub(crate) term: Pane,
    /// Whether the project picker is expanded.
    pub picker_open: bool,
    /// The launcher's `n` input box and its Enter subscription. Created and dropped to follow `model.typing`.
    typed: Option<(Entity<InputState>, Subscription)>,
    /// The timer set on a `New` that is waiting for an answer (its `since`). It is not set twice for the same `New`.
    pending_timer: Option<Instant>,
    /// The subscription set to re-read account names when the window is activated, and that window. This view outlives the window, so it is set anew for each window.
    activation: Option<(WindowId, Subscription)>,
    /// The task that feeds the link thread's events to the model. It stops with the view when the view is dropped.
    _pump: Task<()>,
}

impl Workspace {
    /// Receives the link thread's events on GPUI's foreground executor and feeds them to the model. Events already waiting are gathered and fed at once.
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

    /// Feeds the link thread's events to the model. It redraws on every output (`Output`), but GPUI coalesces `notify`
    /// into one per next frame (a dirty window is drawn once at the display refresh).
    fn on_link(&mut self, batch: Vec<LinkEvent>, cx: &mut Context<Self>) {
        for ev in batch {
            let effects = self.model.on_link(ev);
            self.apply(effects);
        }
        self.watch_sync(cx);
        cx.notify();
    }

    /// Carries out what the model asked for. The model emits no `New`, `Attach`, `Input` or `Resize` while there is no
    /// connection. Anything sent before the model learned the connection dropped is discarded by the link thread when it
    /// reconnects.
    pub(crate) fn apply(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Send(msg) => {
                    let _ = self.tx.unbounded_send(msg);
                }
            }
        }
    }

    /// Hooks this window to re-read account names when it is activated. Safe to call on every draw: it hooks only once
    /// per window (a window reopened from the Dock is a new window, so it hooks again).
    ///
    /// ponytail: no file watcher. An edited label or a changed account shows up when you return to the window.
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

    /// Re-reads the account names, and redraws if they changed.
    fn reload_labels(&mut self, cx: &mut Context<Self>) {
        let home = std::env::home_dir();
        if self.model.reload_labels(&self.state, home.as_deref()) {
            cx.notify();
        }
    }

    /// The launcher's input box. It exists only while open.
    pub fn typed_input(&self) -> Option<&Entity<InputState>> {
        self.typed.as_ref().map(|(input, _)| input)
    }

    /// Called after the model changes. Opens or closes the input box to match `model.typing`, and redraws.
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
                // When the launcher opens, take focus so its keys (↵ b x s n, Esc) and the shortcuts reach this view.
                // Focus left on the terminal pane can't receive keys if that pane isn't drawn.
                window.focus(&self.focus);
            }
        } else {
            // A session is attached and the launcher closed (list or tab click, ⌘1–9, ⌘[ ⌘], Esc). Hand focus to the
            // pane so keys go to the session.
            window.focus(&self.term.focus);
        }
        self.watch_pending(cx);
        cx.notify();
    }

    /// For each `New` sent, checks [`PENDING_TIMEOUT`] later whether an answer came. If not, the model drops it and reports it.
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

    /// Drops the input box. If focus was on the box, returns it to this view so shortcuts and launcher keys keep arriving.
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

    /// "＋ Add project": picks one folder, registers it, and selects that project.
    pub fn add_project(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add as project".into()),
        });
        cx.spawn(async move |this, cx| {
            // Cancelled, or the picker window couldn't be opened.
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

    /// Registers the chosen folder by its real path and re-reads the registered list. `project::register` does not
    /// register the home directory or `/`, so they are not in the list, and the model shows the reason.
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
            Err(e) => self.model.message = Some(format!("Couldn't add the project: {e:#}")),
        }
    }

    /// Launcher keys. Esc closes the input box, or a launcher opened over a session (Esc in the input box bubbles up
    /// here too). Row-picking keys are accepted only while the input box is closed.
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

    /// ⌘W. When closing a session, takes the answer of the confirmation dialog (`window.prompt`) and passes it to the model.
    fn on_close(&mut self, _: &CloseSession, window: &mut Window, cx: &mut Context<Self>) {
        match self.model.close() {
            Close::Launcher => self.changed(window, cx),
            Close::Window => window.remove_window(),
            Close::Nothing => {}
            Close::Confirm(name) => {
                let answer = window.prompt(
                    PromptLevel::Warning,
                    &format!("Close session {name}?"),
                    Some("This ends the session's process and removes it from the list."),
                    &close_buttons(),
                    cx,
                );
                cx.spawn(async move |this, cx| {
                    // Even if the window closes and no answer comes, it counts as cancel.
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
    /// Sets focus while drawing. Handles state that changed without the window (attaching to a session, or the attached
    /// session vanishing, because of daemon work) and a window opened for the first time (including one reopened from
    /// the Dock, which has no focus). Moving focus while drawing doesn't redraw; keys go there from the next key on.
    /// - Launcher open and no input box: this view. Launcher keys and shortcuts must arrive.
    /// - Session attached and the pane visible: hand focus to the pane when there is none or it is on this view itself.
    ///   Clicking a list group header or the project picker gives this view focus on mouse press, and keys must still
    ///   go to the session after that.
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
        // When the input box closes because of daemon work (attaching to a new session), there is no window at that point, so it can't be cleared. Clear it while drawing.
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
                    // The slot for D3's Changes panel. Collapsed in D1.
                    .child(div().w(px(0.)).h_full().flex_shrink_0()),
            )
    }
}

/// The buttons of the close confirmation dialog. On macOS (NSAlert), Return is bound to the first button. So that
/// Return after ⌘W doesn't end the session, the first button is Cancel (Ruling D-R9).
///
/// Making Cancel with `PromptButton::cancel` would make GPUI change that button's key to Esc, and Return would be bound
/// to no button. So it is a normal button. In exchange, Esc does not close this dialog.
fn close_buttons() -> [PromptButton; 2] {
    [PromptButton::new("Cancel"), PromptButton::new("Close")]
}

/// Is the dialog's answer (button slot) Close (the second button)? If the window closed and there is no answer, it counts as cancel.
fn close_confirmed(answer: Option<usize>) -> bool {
    answer == Some(1)
}

/// The center pane: tabs on top, the launcher or the attached session's terminal pane below, and the notice line at the very bottom.
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

/// The whole-window frame: a title bar in the mockup colors with the content below it.
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

/// The view shown in the window when the app can't be used (such as when there is no `mine` to start the daemon).
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
        // macOS binds Return to the first button. So that Return after ⌘W doesn't end the session, the first button is Cancel (Ruling D-R9).
        let buttons = close_buttons();
        let labels: Vec<_> = buttons.iter().map(|b| b.label().to_string()).collect();
        assert_eq!(labels, ["Cancel", "Close"]);
        // Making Cancel a Cancel button makes GPUI change that key to Esc, and Return is bound to no button.
        assert!(matches!(buttons[0], PromptButton::Other(_)));
        assert!(!close_confirmed(Some(0)));
        assert!(close_confirmed(Some(1)));
        assert!(!close_confirmed(None));
    }
}
