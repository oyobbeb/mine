//! The root view: the tab bar centered in the title-bar row, and below it the left list and the center pane (the launcher or a
//! session's terminal), each a glass panel floating over the blurred window.
//!
//! This view outlives the window. When the window is closed, the app-wide `main::MainView` keeps hold of it and draws it
//! into the window reopened from the Dock. All state lives in `Model`; this view wires GPUI's work (shortcuts, clicks,
//! the confirmation dialog, the folder picker, the input box) to the model.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, PathPromptOptions, PromptButton, PromptLevel, Render, Styled,
    Subscription, Task, Window, WindowAppearance, WindowId, div, px,
};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::{InteractiveElementExt, h_flex, v_flex};
use mine_core::launch::Choice;
use mine_core::link::{DaemonLink, LinkEvent};
use mine_core::paths::State;
use mine_core::project;
use mine_core::proto::{PRESENCE_HEARTBEAT, ToDaemon};
use mine_core::session::SessionState;
use mine_core::view;

use crate::actions::{
    CONTEXT, CloseSession, NewTask, NextProject, NextSession, PrevProject, PrevSession, SelectTab,
    ToggleSidebar,
};
use crate::glass::{self, INSET};
use crate::model::{Close, Effect, Model, PENDING_TIMEOUT};
use crate::terminal::{self, Pane};
use crate::theme::{self, Theme};
use crate::{appearance, launcher, sidebar, tabs};

/// At most this many events from the link thread are gathered and fed to the model at once.
const LINK_BATCH: usize = 256;
/// The shortest wait of the presence heartbeat task.
const BEAT_MIN: Duration = Duration::from_secs(1);

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
    /// The subscriptions set to follow the window's activation (focus report, account names, the look) and its light or dark
    /// appearance, and that window. This view outlives the window, so they are set anew for each window.
    activation: Option<(WindowId, [Subscription; 2])>,
    /// The re-read of the look in flight ([`Workspace::refresh_look`]). A newer one replaces (drops) it.
    look: Option<Task<()>>,
    /// Reports the window as unfocused when it closes. A closing window's deactivation doesn't reach the activation subscription.
    _closed: Subscription,
    /// The task that feeds the link thread's events to the model. It stops with the view when the view is dropped.
    _pump: Task<()>,
    /// The presence heartbeat ([`Model::presence_deadline`]).
    _beat: Task<()>,
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
        let this = cx.weak_entity();
        let closed = cx.on_window_closed(move |cx| {
            let _ = this.update(cx, |ws, _| ws.set_focused(false));
        });
        // The presence heartbeat: sleeps until the next one is due, or a heartbeat's length while the window is inactive or there
        // is no connection. The wait is at least `BEAT_MIN`: GPUI's `timer` of zero is ready at once, so a deadline already past
        // would spin this loop on the main thread without yielding.
        let beat = cx.spawn(async move |this, cx| {
            while let Ok(due) = this.update(cx, |ws, _| {
                ws.presence_tick();
                ws.model.presence_deadline()
            }) {
                let wait = due.map_or(PRESENCE_HEARTBEAT, |d| {
                    d.saturating_duration_since(Instant::now())
                });
                cx.background_executor().timer(wait.max(BEAT_MIN)).await;
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
            look: None,
            _closed: closed,
            _pump: pump,
            _beat: beat,
        }
    }

    fn presence_tick(&mut self) {
        let effects = self.model.presence_tick(Instant::now());
        self.apply(effects);
    }

    /// A key or click anywhere in the window, seen on the way down (capture phase), so it counts whichever element handles it. An
    /// element that occludes the frame (the tab bar) reports its own clicks here too.
    pub(crate) fn user_input(&mut self) {
        let effects = self.model.user_input(Instant::now());
        self.apply(effects);
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

    /// Hooks this window to tell the daemon whether it is active, and to re-read the look when it becomes active (an edited
    /// `[appearance]`, Reduce transparency turned on or off) or when the system switches between light and dark. Safe to call on
    /// every draw: it hooks only once per window (a window reopened from the Dock is a new window, so it hooks again).
    fn watch_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = window.window_handle().window_id();
        if self
            .activation
            .as_ref()
            .is_some_and(|(seen, _)| *seen == id)
        {
            return;
        }
        let activation = cx.observe_window_activation(window, |ws, window, cx| {
            let active = window.is_window_active();
            ws.set_focused(active);
            if active {
                ws.refresh_look(window, cx);
            }
        });
        let appearance =
            cx.observe_window_appearance(window, |ws, window, cx| ws.refresh_look(window, cx));
        self.activation = Some((id, [activation, appearance]));
        // The window may have become active before this hook existed, or changed since the look was installed at startup.
        self.set_focused(window.is_window_active());
        self.refresh_look(window, cx);
    }

    fn refresh_look(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.look = Some(refresh_look(Some(self.state.clone()), window, cx));
    }

    fn set_focused(&mut self, focused: bool) {
        let effects = self.model.set_focused(focused);
        self.apply(effects);
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

    /// A row's or tab's Allow or Deny. The list changes when the daemon publishes the decision, so there is nothing to redraw here.
    pub fn answer(&mut self, id: &str, request: u64, allow: bool) {
        let effects = self.model.answer(id, request, allow);
        self.apply(effects);
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
            && let Some(choice) = launcher::choice_of_key(key, &self.model.profiles)
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
        self.close_stale_find(window);
        self.watch_activation(window, cx);
        self.fix_focus(window, cx);
        // Every state change that can change which terminal shows (attach, launcher, a session gone) redraws.
        self.presence_tick();
        let t = theme::current(cx).clone();
        let title = tabs::render(self, cx);
        frame(&t, title)
            .track_focus(&self.focus)
            .key_context(CONTEXT)
            .capture_key_down(cx.listener(|ws, _, _, _| ws.user_input()))
            .capture_any_mouse_down(cx.listener(|ws, _, _, _| ws.user_input()))
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
                // ponytail: D3's Changes panel gets its slot here when it lands; an empty one would add a gap.
                // The mockup's content area: padding 0 12 12 12 under the title-bar row, and 12 between the panels.
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .px(px(INSET))
                    .pb(px(INSET))
                    .gap(px(INSET))
                    .when(self.model.sidebar_open, |this| {
                        this.child(sidebar::render(self, cx))
                    })
                    .child(center(self, window, cx)),
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

/// The center: the glass pane with the launcher or the attached session's terminal, and the notice line below it.
fn center(ws: &Workspace, window: &Window, cx: &mut Context<Workspace>) -> impl IntoElement {
    let m = &ws.model;
    let t = theme::current(cx).clone();
    let pane = glass::panel(&t).flex_1().min_h_0().flex().flex_col();
    v_flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .gap(px(INSET))
        .child(if m.launcher_open() {
            // No live session sits under the launcher, so the pane is a plain panel there (Ruling G-R3).
            pane.child(launcher::render(ws, cx))
        } else {
            // The terminal fill covers the wallpaper, so session output reads as it always has (Ruling G-R2).
            pane.bg(t.terminal.hsla())
                .child(terminal::render(ws, window, cx))
        })
        // A capsule in the message's own row, so the terminal resizes for it as before and the text never sits on the bare tint.
        .when_some(m.message.clone(), |this, message| {
            this.child(
                glass::capsule(&t, false)
                    .flex_shrink_0()
                    .px(glass::pad(14.))
                    .py(glass::pad(6.))
                    .text_size(px(12.))
                    .line_height(px(15.))
                    .text_color(t.accent_text.hsla())
                    .child(message),
            )
        })
}

/// Height of the title-bar row. The traffic lights (`main::TRAFFIC_LIGHTS`) and the tab bar are centered in it.
pub const TITLE_ROW: f32 = 52.;
/// Room at each side of the title-bar row: 20 px padding, the traffic lights and the mockup's 8 px gap on the left, mirrored on the
/// right so `title` is centered on the window. macOS draws its own lights, 14 px with wider spacing than the mockup's 12 px and 8 px
/// gap, so they end at x 80 (measured), not 72: 80 + 8, rounded up. A full tab bar then stops short of the zoom button.
const TITLE_ROW_SIDE: f32 = 90.;

/// The whole-window frame: the clear title-bar row with `title` centered in it, and the content below. gpui-component's `Root`
/// paints the window tint under all of it, so the frame paints no background of its own.
///
/// The row is drawn here rather than with gpui-component's `TitleBar`, whose height is fixed at 34 px. Like that one, a double click
/// on its empty parts does the system's title-bar action (zoom by default); the tab bar occludes it, so a double click on a tab
/// doesn't. Dragging moves the window through macOS's own title-bar strip at the top of the row.
// ponytail: GPUI 0.2.2 can't start a window drag on macOS (`start_window_move` is a no-op there), so the part of the row below that
// strip doesn't drag. Add objc `performWindowDragWithEvent:` if that matters.
fn frame(t: &Theme, title: impl IntoElement) -> gpui::Div {
    v_flex().size_full().text_color(t.text.hsla()).child(
        h_flex()
            .id("title-row")
            .flex_shrink_0()
            .h(px(TITLE_ROW))
            .px(px(TITLE_ROW_SIDE))
            .justify_center()
            .on_double_click(|_, window, _| window.titlebar_double_click())
            .child(title),
    )
}

/// The view shown in the window when the app can't be used (such as when there is no `mine` to start the daemon). Like
/// [`Workspace`], it re-reads the look when its window becomes active or switches between light and dark.
pub struct Problem {
    message: String,
    /// Where `[appearance]` is read from. `None` when there is no state folder: the default look.
    state: Option<State>,
    /// The subscriptions set for the window it was last drawn in, and that window (see [`Workspace::watch_activation`]).
    hooks: Option<(WindowId, [Subscription; 2])>,
    /// The re-read of the look in flight.
    look: Option<Task<()>>,
}

impl Problem {
    pub fn new(message: String, state: Option<State>) -> Self {
        Problem {
            message,
            state,
            hooks: None,
            look: None,
        }
    }

    fn refresh_look(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.look = Some(refresh_look(self.state.clone(), window, cx));
    }
}

impl Render for Problem {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let id = window.window_handle().window_id();
        if self.hooks.as_ref().is_none_or(|(seen, _)| *seen != id) {
            let activation = cx.observe_window_activation(window, |problem, window, cx| {
                if window.is_window_active() {
                    problem.refresh_look(window, cx);
                }
            });
            let appearance = cx.observe_window_appearance(window, |problem, window, cx| {
                problem.refresh_look(window, cx)
            });
            self.hooks = Some((id, [activation, appearance]));
            self.refresh_look(window, cx);
        }
        // Text never sits on the bare window tint (the contrast test doesn't cover it), so the reason is in a panel.
        let t = theme::current(cx);
        frame(t, div()).child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p(px(INSET))
                .child(
                    glass::panel(t)
                        .px_8()
                        .py_6()
                        .text_size(px(14.))
                        .child(self.message.clone()),
                ),
        )
    }
}

/// Whether a window appearance is dark (`Dark` and `VibrantDark`).
pub fn is_dark(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

/// The theme the window should draw with now: `[appearance]` re-read from `<state>/config.toml`, macOS Reduce transparency, and the
/// window's light or dark appearance. It reads a file and runs `defaults`, so call it off the UI thread.
pub fn appearance_theme(system_dark: bool, state: Option<&State>) -> Theme {
    theme_with(system_dark, state, appearance::reduce_transparency)
}

/// [`appearance_theme`] with the Reduce transparency check passed in. The check only runs when the config asks for glass. With no
/// state folder (`None`) the config is the default.
pub fn theme_with(
    system_dark: bool,
    state: Option<&State>,
    reduced: impl FnOnce() -> bool,
) -> Theme {
    let (look, warnings) = state.map_or_else(Default::default, appearance::load);
    warn_once(warnings);
    let solid = look.glass == appearance::Glass::Solid;
    theme::resolve(look.mode, system_dark, look.glass, !solid && reduced())
}

/// Re-reads the look for the window's current appearance on a background thread (the config file and `defaults`), then applies it
/// to the window if it changed. Dropping the task cancels it.
fn refresh_look<V: 'static>(
    state: Option<State>,
    window: &Window,
    cx: &mut Context<V>,
) -> Task<()> {
    let dark = is_dark(window.appearance());
    let read = cx
        .background_executor()
        .spawn(async move { appearance_theme(dark, state.as_ref()) });
    cx.spawn_in(window, async move |_, cx| {
        let theme = read.await;
        let _ = cx.update(|window, cx| apply_theme(&theme, window, cx));
    })
}

/// Prints the config's warnings when they differ from the last ones printed, so a broken `[appearance]` is reported once, not at
/// every activation.
fn warn_once(warnings: Vec<String>) {
    static SHOWN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut shown = SHOWN.lock().unwrap_or_else(PoisonError::into_inner);
    if *shown != warnings {
        for warning in &warnings {
            eprintln!("{warning}");
        }
        *shown = warnings;
    }
}

/// Makes `theme` current and redraws the window with it, unless it already is.
fn apply_theme(theme: &Theme, window: &mut Window, cx: &mut App) {
    if theme::current(cx) == theme {
        return;
    }
    theme::install(cx, theme);
    window.set_background_appearance(theme.window_background());
    window.refresh();
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::WindowBackgroundAppearance;
    use std::cell::Cell;
    use std::fs;

    fn temp_state(name: &str) -> State {
        let root = std::env::temp_dir().join(format!("mine-look-{name}-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        State { root }
    }

    fn set_mode(state: &State, mode: &str) {
        let text = format!("[appearance]\nmode = \"{mode}\"\n");
        fs::write(state.root.join("config.toml"), text).unwrap();
    }

    #[test]
    fn appearance_theme_follows_system() {
        let state = temp_state("system");
        set_mode(&state, "system");
        assert_eq!(
            theme_with(false, Some(&state), || false),
            Theme::light_clear()
        );
        assert_eq!(
            theme_with(true, Some(&state), || false),
            Theme::dark_clear()
        );
        fs::remove_dir_all(&state.root).unwrap();
    }

    /// Each activation reads Reduce transparency and the config again: nothing is cached from the last one.
    #[test]
    fn activation_rereads_reduce_transparency_and_config() {
        let state = temp_state("activation");
        let reduce = Cell::new(false);
        let activate = || theme_with(true, Some(&state), || reduce.get());
        let first = activate();
        assert_eq!(first, Theme::dark_clear());
        assert_eq!(
            first.window_background(),
            WindowBackgroundAppearance::Blurred
        );
        reduce.set(true);
        let reduced = activate();
        assert_eq!(reduced, Theme::dark_glass().opaque());
        assert_eq!(
            reduced.window_background(),
            WindowBackgroundAppearance::Opaque
        );
        set_mode(&state, "light");
        assert_eq!(activate(), Theme::light_glass().opaque());
        reduce.set(false);
        assert_eq!(activate(), Theme::light_clear());
        fs::remove_dir_all(&state.root).unwrap();
    }

    /// Without a state folder (the problem view when `State::from_env` fails) the look is the default, and Reduce transparency still
    /// makes it opaque.
    #[test]
    fn no_state_folder_uses_default_look() {
        assert_eq!(theme_with(true, None, || false), Theme::dark_clear());
        assert_eq!(
            theme_with(false, None, || true),
            Theme::light_glass().opaque()
        );
    }

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
