//! Provider-agnostic setup views: the small actions menu opened by a right
//! click on the provider icons, and a connect form with a status area. The
//! application supplies every label; nothing here knows about a provider.

use gpui::prelude::*;
use gpui::{
    div, px, rgb, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    SharedString, Subscription, Window,
};

use crate::text_input::{TextInput, TextInputEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuAction {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub enum ActionMenuEvent {
    Select(String),
    Dismiss,
}

const MENU_ROW: f32 = 22.0;
const MENU_PADDING: f32 = 4.0;

pub struct ActionMenuView {
    actions: Vec<MenuAction>,
    focus_handle: FocusHandle,
    was_active: bool,
    _subscription: Subscription,
}

impl ActionMenuView {
    pub fn new(actions: Vec<MenuAction>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Clicking anywhere else dismisses the menu, like a native context menu.
        let subscription = cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            if this.was_active && !active {
                cx.emit(ActionMenuEvent::Dismiss);
            }
            this.was_active = active;
        });
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            actions,
            focus_handle,
            was_active: false,
            _subscription: subscription,
        }
    }
}

/// Height of the actions menu for `count` entries.
pub fn action_menu_height(count: usize) -> f32 {
    count as f32 * MENU_ROW + 2.0 * MENU_PADDING
}

impl EventEmitter<ActionMenuEvent> for ActionMenuView {}

impl Focusable for ActionMenuView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ActionMenuView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|_, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.emit(ActionMenuEvent::Dismiss);
                }
            }))
            .flex()
            .flex_col()
            .size_full()
            .p(px(MENU_PADDING))
            .rounded_md()
            .bg(rgb(0x111216))
            .text_size(px(12.0))
            .text_color(rgb(0xf4f4f5))
            .children(self.actions.iter().map(|action| {
                let id = action.id.clone();
                div()
                    .id(SharedString::from(format!("action-{}", action.id)))
                    .flex()
                    .items_center()
                    .h(px(MENU_ROW))
                    .px(px(8.0))
                    .rounded_sm()
                    .hover(|style| style.bg(rgb(0x2f5fb8)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.was_active = false;
                        cx.emit(ActionMenuEvent::Select(id.clone()));
                    }))
                    .child(action.label.clone())
            }))
    }
}

/// One text field of a connect form.
#[derive(Debug, Clone)]
pub struct ConnectField {
    pub label: String,
    pub initial: String,
    /// Secrets: drawn as bullets, never copied.
    pub masked: bool,
}

/// What the status area of a connect form shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectStatus {
    #[default]
    Idle,
    Busy(String),
    /// Waiting for an action elsewhere; `command` is shown with a copy
    /// button, and a retry button is offered.
    Waiting {
        message: String,
        command: Option<String>,
    },
    Done(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub enum ConnectEvent {
    /// Field values, in the order the fields were given.
    Submit(Vec<String>),
    Retry,
    Cancel,
}

pub struct ConnectView {
    title: String,
    intro: String,
    submit_label: String,
    fields: Vec<(String, Entity<TextInput>)>,
    status: ConnectStatus,
    _subscriptions: Vec<Subscription>,
}

impl ConnectView {
    pub fn new(
        title: String,
        intro: String,
        fields: Vec<ConnectField>,
        submit_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let inputs: Vec<(String, Entity<TextInput>)> = fields
            .into_iter()
            .map(|field| {
                let input = cx.new(|cx| {
                    if field.masked {
                        TextInput::masked(cx)
                    } else {
                        TextInput::new(field.initial, cx)
                    }
                });
                (field.label, input)
            })
            .collect();
        let subscriptions = inputs
            .iter()
            .enumerate()
            .map(|(index, (_, input))| {
                cx.subscribe_in(
                    input,
                    window,
                    move |this, _, event, window, cx| match event {
                        TextInputEvent::Confirm => this.submit(cx),
                        TextInputEvent::Cancel => cx.emit(ConnectEvent::Cancel),
                        TextInputEvent::Tab => {
                            let next = (index + 1) % this.fields.len();
                            window.focus(&this.fields[next].1.focus_handle(cx), cx);
                        }
                    },
                )
            })
            .collect();
        // Start where typing is needed: the first empty field.
        if let Some((_, input)) = inputs
            .iter()
            .find(|(_, input)| input.read(cx).text().is_empty())
            .or(inputs.first())
        {
            window.focus(&input.focus_handle(cx), cx);
        }
        Self {
            title,
            intro,
            submit_label,
            fields: inputs,
            status: ConnectStatus::Idle,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_status(&mut self, status: ConnectStatus, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.notify();
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let values = self
            .fields
            .iter()
            .map(|(_, input)| input.read(cx).text().to_owned())
            .collect();
        cx.emit(ConnectEvent::Submit(values));
    }

    fn render_status(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (message, color, command) = match &self.status {
            ConnectStatus::Idle => return None,
            ConnectStatus::Busy(message) => (message.clone(), 0xb9bbc3, None),
            ConnectStatus::Waiting { message, command } => {
                (message.clone(), 0xf5c451, command.clone())
            }
            ConnectStatus::Done(message) => (message.clone(), 0x4ade80, None),
            ConnectStatus::Error(message) => (message.clone(), 0xf87171, None),
        };
        let waiting = matches!(self.status, ConnectStatus::Waiting { .. });
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(div().text_color(rgb(color)).child(message))
                .when_some(command, |status, command| {
                    let copied = command.clone();
                    status.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex_1()
                                    .px(px(7.0))
                                    .py(px(4.0))
                                    .rounded_sm()
                                    .bg(rgb(0x1e1f24))
                                    .font_family("Menlo")
                                    .text_size(px(11.0))
                                    .child(command),
                            )
                            .child(button("copy-command", "Copy", false).on_click(
                                move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copied.clone(),
                                    ));
                                },
                            )),
                    )
                })
                .when(waiting, |status| {
                    status.child(
                        div().flex().child(
                            button("retry", "Retry now", false)
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(ConnectEvent::Retry))),
                        ),
                    )
                }),
        )
    }
}

impl EventEmitter<ConnectEvent> for ConnectView {}

impl Render for ConnectView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.render_status(cx);
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .size_full()
            .p(px(16.0))
            .bg(rgb(0x111216))
            .text_size(px(12.0))
            .text_color(rgb(0xf4f4f5))
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.title.clone()),
            )
            .child(div().text_color(rgb(0xb9bbc3)).child(self.intro.clone()))
            .children(self.fields.iter().map(|(label, input)| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(div().text_color(rgb(0x8f929c)).child(label.clone()))
                    .child(input.clone())
            }))
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        button("submit", &self.submit_label, true)
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    )
                    .child(
                        button("cancel", "Cancel", false)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ConnectEvent::Cancel))),
                    ),
            )
            .children(status)
    }
}

fn button(id: &'static str, label: &str, primary: bool) -> gpui::Stateful<gpui::Div> {
    let (background, hover) = if primary {
        (0x2f5fb8, 0x3b6fd0)
    } else {
        (0x2a2b31, 0x36373e)
    };
    div()
        .id(id)
        .px(px(10.0))
        .py(px(4.0))
        .rounded_sm()
        .bg(rgb(background))
        .hover(move |style| style.bg(rgb(hover)))
        .cursor_pointer()
        .child(label.to_owned())
}
