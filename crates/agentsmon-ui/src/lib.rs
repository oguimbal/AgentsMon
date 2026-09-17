//! GPUI views for AgentsMon.

mod diagnostic_report;
mod text_input;
pub use diagnostic_report::DiagnosticReportView;

use std::collections::{HashMap, HashSet};
use std::f32::consts::PI;
use std::sync::{Arc, Mutex, OnceLock};

use agentsmon_core::{
    AgentId, AgentSnapshot, AgentState, GroupKey, ProviderStatus, ProviderUsage, SessionGroup,
    Slot, UsageLimit,
};
use gpui::prelude::*;
use gpui::{
    canvas, div, fill, img, point, px, rgb, rgba, size, AnyElement, Bounds, ContentMask, Context,
    Corners, DragMoveEvent, Entity, EventEmitter, Focusable, Hsla, Image, ImageFormat, IntoElement,
    MouseButton, MouseDownEvent, PathBuilder, Render, SharedString, Subscription, Window,
};

pub use text_input::key_bindings as rename_key_bindings;
use text_input::{TextInput, TextInputEvent};

/// What a rename prompt edits.
#[derive(Debug, Clone)]
pub enum RenameTarget {
    Session(Box<AgentSnapshot>),
    Group(SessionGroup),
}

impl RenameTarget {
    fn label(&self) -> &str {
        match self {
            RenameTarget::Session(agent) => agent.label(),
            RenameTarget::Group(group) => group.label(),
        }
    }

    fn key(&self) -> RenameKey {
        match self {
            RenameTarget::Session(agent) => RenameKey::Session(agent.id.clone()),
            RenameTarget::Group(group) => RenameKey::Group(group.key.clone()),
        }
    }
}

/// Where a submitted name goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameKey {
    Session(AgentId),
    Group(GroupKey),
}

#[derive(Debug, Clone)]
pub enum MonitorEvent {
    Focus(AgentId),
    Hide(AgentId),
    HideGroup(GroupKey),
    /// Move the dragged card (all its sessions, kept together) before `before`.
    Reorder {
        dragged: Vec<AgentId>,
        before: Option<AgentId>,
    },
    MoveBarBy(f32),
    ShowDetails {
        agent: Box<AgentSnapshot>,
        anchor_x: f32,
    },
    ShowProviderDetails {
        provider: Box<ProviderStatus>,
        anchor_x: f32,
    },
    HideDetails,
    /// Pointer entered a group card: open (or keep) the menu listing its members.
    ShowGroup {
        key: GroupKey,
        anchor_x: f32,
    },
    /// Pointer left a group card. The menu stays while the pointer reaches it.
    LeaveGroup,
    /// Right click on a card: open the rename prompt.
    Rename {
        target: Box<RenameTarget>,
        anchor_x: f32,
    },
}

/// How a `MonitorView` lays its cards out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// The horizontal bar at the bottom of a display.
    Bar,
    /// The vertical list of a group's members, shown above the bar.
    Menu,
}

pub struct MonitorView {
    slots: Vec<Slot>,
    providers: Vec<ProviderStatus>,
    unread: HashSet<AgentId>,
    layout: Layout,
    bar_drag_last_x: Option<gpui::Pixels>,
}

/// Drag payload: the sessions of the dragged card, in bar order.
#[derive(Clone)]
struct DragSlot(Vec<AgentId>);

impl MonitorView {
    pub fn new(
        slots: Vec<Slot>,
        providers: Vec<ProviderStatus>,
        unread: HashSet<AgentId>,
        layout: Layout,
    ) -> Self {
        Self {
            slots,
            providers,
            unread,
            layout,
            bar_drag_last_x: None,
        }
    }

    pub fn set_data(
        &mut self,
        slots: Vec<Slot>,
        providers: Vec<ProviderStatus>,
        unread: HashSet<AgentId>,
        cx: &mut Context<Self>,
    ) {
        self.slots = slots;
        self.providers = providers;
        self.unread = unread;
        cx.notify();
    }

    fn accent_for(&self, provider_id: &str) -> Option<u32> {
        self.providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .and_then(|provider| provider.accent_rgb)
    }

    fn render_provider(
        &self,
        provider: ProviderStatus,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let details = provider.clone();
        let usage = provider.usage.clone();
        div()
            .id(SharedString::from(format!("provider-{}", provider.id)))
            .flex()
            .items_center()
            .justify_center()
            .size(px(17.0))
            .rounded_full()
            .hover(|style| style.bg(rgb(0x303138)))
            .on_mouse_move(|_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_hover(cx.listener(move |_, hovered: &bool, window, cx| {
                if *hovered {
                    cx.emit(MonitorEvent::ShowProviderDetails {
                        provider: Box::new(details.clone()),
                        anchor_x: window.mouse_position().x.as_f32(),
                    });
                } else {
                    cx.emit(MonitorEvent::HideDetails);
                }
            }))
            .child(provider_ring(provider.icon_png, usage))
    }

    fn render_slot(&self, slot: Slot, cx: &mut Context<Self>) -> AnyElement {
        match slot {
            Slot::Single(agent) => self.render_card(*agent, cx).into_any_element(),
            Slot::Group(group) => self.render_group(group, cx).into_any_element(),
        }
    }

    /// Drag-and-drop reordering, only meaningful in the bar.
    fn draggable(
        &self,
        card: gpui::Stateful<gpui::Div>,
        ids: Vec<AgentId>,
        label: String,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        if self.layout != Layout::Bar {
            return card;
        }
        let live_target = ids[0].clone();
        card.on_drag(DragSlot(ids), move |_, _, _, cx| {
            cx.new(|_| DragPreview {
                label: label.clone(),
            })
        })
        .drag_over::<DragSlot>(|style, _, _, _| style.bg(rgb(0x3a3c45)))
        .on_drag_move::<DragSlot>(cx.listener(
            move |this, event: &DragMoveEvent<DragSlot>, _, cx| {
                if !event.bounds.contains(&event.event.position) {
                    return;
                }
                let dragged = &event.drag(cx).0;
                let after =
                    event.event.position.x >= event.bounds.origin.x + event.bounds.size.width / 2.0;
                if let Some(before) =
                    live_reorder_destination(&this.slots, dragged, Some(&live_target), after)
                {
                    cx.emit(MonitorEvent::Reorder {
                        dragged: dragged.clone(),
                        before,
                    });
                }
            },
        ))
    }

    fn render_card(&self, agent: AgentSnapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let id = agent.id.clone();
        let focus_id = id.clone();
        let hide_id = id.clone();
        let in_menu = self.layout == Layout::Menu;
        // The menu lists sessions that share a project name, so it shows the
        // conversation title instead (see `AgentSnapshot::menu_label`).
        let label = if in_menu {
            agent.menu_label().to_owned()
        } else {
            agent.label().to_owned()
        };
        let context = agent.context_usage.map(|usage| usage.used_percentage());
        let accent = self.accent_for(&id.provider);
        let hover_details = agent.clone();
        let rename_target = RenameTarget::Session(Box::new(agent.clone()));
        let is_unread = self.unread.contains(&id);
        let state_background = state_color(agent.state);

        let card = div()
            .id(SharedString::from(format!(
                "agent-{}-{}",
                id.provider, id.session_id
            )))
            .flex()
            .items_center()
            .gap(px(3.0))
            .h(px(17.0))
            .px(px(5.0))
            .rounded_full()
            .bg(state_background)
            .relative()
            .cursor_pointer()
            .when(in_menu, |card| card.w_full())
            .on_mouse_move(|_, _, cx| cx.stop_propagation())
            .on_hover(cx.listener(move |_, hovered: &bool, window, cx| {
                if *hovered {
                    cx.emit(MonitorEvent::ShowDetails {
                        agent: Box::new(hover_details.clone()),
                        anchor_x: window.mouse_position().x.as_f32(),
                    });
                } else {
                    cx.emit(MonitorEvent::HideDetails);
                }
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |_, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    cx.emit(MonitorEvent::Rename {
                        target: Box::new(rename_target.clone()),
                        anchor_x: window.mouse_position().x.as_f32(),
                    });
                }),
            )
            .on_click(cx.listener(move |_, _, _event, cx| {
                cx.emit(MonitorEvent::Focus(focus_id.clone()));
            }));
        self.draggable(card, vec![id.clone()], label.clone(), cx)
            .when(is_unread, |card| card.child(unread_badge()))
            .child(
                div()
                    .id(SharedString::from(format!("context-{}", id.session_id)))
                    .size(px(CONTEXT_INDICATOR_SIZE))
                    .child(context_pie(context, accent)),
            )
            .child(card_label(label, in_menu))
            .child(close_button(
                format!("hide-{}", hide_id.session_id),
                cx.listener(move |_, _, _, cx| {
                    cx.stop_propagation();
                    cx.emit(MonitorEvent::Hide(hide_id.clone()));
                }),
            ))
    }

    /// One card for several sessions: the background is split into equal
    /// stripes, one per member state, and the indicator shows the member count
    /// ringed by their provider colours.
    fn render_group(&self, group: SessionGroup, cx: &mut Context<Self>) -> impl IntoElement {
        let key = group.key.clone();
        let label = group.label().to_owned();
        let ids = group.ids();
        let focus_id = group
            .most_recent()
            .map(|member| member.id.clone())
            .unwrap_or_else(|| ids[0].clone());
        let stripes: Vec<Hsla> = group
            .members
            .iter()
            .map(|member| state_color(member.state))
            .collect();
        let accents: Vec<Option<u32>> = group
            .members
            .iter()
            .map(|member| self.accent_for(&member.id.provider))
            .collect();
        let is_unread = group
            .members
            .iter()
            .any(|member| self.unread.contains(&member.id));
        let hover_key = key.clone();
        let hide_key = key.clone();
        let rename_target = RenameTarget::Group(group.clone());
        let element_id = match &key {
            GroupKey::Directory(cwd) => format!("group-{}", cwd.display()),
        };

        let card = div()
            .id(SharedString::from(element_id.clone()))
            .flex()
            .items_center()
            .gap(px(3.0))
            .h(px(17.0))
            .px(px(5.0))
            .rounded_full()
            .relative()
            .cursor_pointer()
            .child(group_stripes(stripes))
            .on_mouse_move(|_, _, cx| cx.stop_propagation())
            .on_hover(cx.listener(move |_, hovered: &bool, window, cx| {
                if *hovered {
                    cx.emit(MonitorEvent::ShowGroup {
                        key: hover_key.clone(),
                        anchor_x: window.mouse_position().x.as_f32(),
                    });
                } else {
                    cx.emit(MonitorEvent::LeaveGroup);
                }
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |_, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    cx.emit(MonitorEvent::Rename {
                        target: Box::new(rename_target.clone()),
                        anchor_x: window.mouse_position().x.as_f32(),
                    });
                }),
            )
            .on_click(cx.listener(move |_, _, _event, cx| {
                cx.emit(MonitorEvent::Focus(focus_id.clone()));
            }));
        self.draggable(card, ids, label.clone(), cx)
            .when(is_unread, |card| card.child(unread_badge()))
            .child(
                div()
                    .id(SharedString::from(format!("count-{element_id}")))
                    .size(px(CONTEXT_INDICATOR_SIZE))
                    .child(group_indicator(group.members.len(), accents)),
            )
            .child(card_label(label, false))
            .child(close_button(
                format!("hide-{element_id}"),
                cx.listener(move |_, _, _, cx| {
                    cx.stop_propagation();
                    cx.emit(MonitorEvent::HideGroup(hide_key.clone()));
                }),
            ))
    }

    fn render_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .id("agentsmon-root")
            .flex()
            .items_center()
            .h_full()
            .w_full()
            .p(px(3.0))
            .gap(px(3.0))
            .overflow_x_scroll()
            .rounded_tl(px(5.0))
            .rounded_tr(px(5.0))
            .rounded_bl(px(0.0))
            .rounded_br(px(0.0))
            .shadow_sm()
            .bg(rgba(0x18191de8))
            .on_mouse_move(cx.listener(|_, _, _, cx| {
                cx.emit(MonitorEvent::HideDetails);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, _| {
                    this.bar_drag_last_x = Some(event.position.x);
                }),
            )
            .on_drag(BarDrag, |_, _, _, cx| cx.new(|_| BarDragPreview))
            .on_drag_move::<BarDrag>(cx.listener(|this, event: &DragMoveEvent<BarDrag>, _, cx| {
                let x = event.event.position.x;
                if let Some(last_x) = this.bar_drag_last_x.replace(x) {
                    let delta = (x - last_x).as_f32();
                    if delta.abs() >= 0.25 {
                        cx.emit(MonitorEvent::MoveBarBy(delta));
                    }
                }
            }));

        let root = root
            .children(
                self.providers
                    .clone()
                    .into_iter()
                    .map(|provider| self.render_provider(provider, cx)),
            )
            .when(!self.providers.is_empty(), |root| {
                root.child(div().w(px(1.0)).h(px(13.0)).bg(rgb(0x3b3d44)))
            });

        if self.slots.is_empty() {
            root.child(
                div()
                    .flex()
                    .items_center()
                    .h(px(17.0))
                    .px(px(5.0))
                    .text_size(px(10.0))
                    .text_color(rgb(0x92949d))
                    .child("Aucun agent"),
            )
        } else {
            root.children(
                self.slots
                    .clone()
                    .into_iter()
                    .map(|slot| self.render_slot(slot, cx)),
            )
            .child(
                div()
                    .id("reorder-at-end")
                    .h(px(17.0))
                    .min_w(px(8.0))
                    .drag_over::<DragSlot>(|style, _, _, _| style.bg(rgb(0x3a3c45)))
                    .on_drag_move::<DragSlot>(cx.listener(
                        |this, event: &DragMoveEvent<DragSlot>, _, cx| {
                            if !event.bounds.contains(&event.event.position) {
                                return;
                            }
                            let dragged = &event.drag(cx).0;
                            if let Some(before) =
                                live_reorder_destination(&this.slots, dragged, None, true)
                            {
                                cx.emit(MonitorEvent::Reorder {
                                    dragged: dragged.clone(),
                                    before,
                                });
                            }
                        },
                    )),
            )
        }
    }

    fn render_menu(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("agentsmon-menu")
            .flex()
            .flex_col()
            .size_full()
            .p(px(3.0))
            .gap(px(3.0))
            .rounded_md()
            .shadow_sm()
            .bg(rgba(0x18191df4))
            .on_mouse_move(cx.listener(|_, _, _, cx| {
                cx.emit(MonitorEvent::HideDetails);
            }))
            .children(
                self.slots
                    .clone()
                    .into_iter()
                    .map(|slot| self.render_slot(slot, cx)),
            )
    }
}

impl EventEmitter<MonitorEvent> for MonitorView {}

impl Render for MonitorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.layout {
            Layout::Bar => self.render_bar(cx).into_any_element(),
            Layout::Menu => self.render_menu(cx).into_any_element(),
        }
    }
}

fn unread_badge() -> impl IntoElement {
    div()
        .absolute()
        .top(px(-1.0))
        .left(px(-1.0))
        .size(px(5.0))
        .rounded_full()
        .bg(rgb(0xf2b84b))
}

/// Card text. In the menu the label stretches over the row so the close
/// button of every member lines up on the right edge.
fn card_label(label: String, in_menu: bool) -> impl IntoElement {
    div()
        .max_w(px(if in_menu {
            MENU_LABEL_MAX_WIDTH
        } else {
            BAR_LABEL_MAX_WIDTH
        }))
        .when(in_menu, |label| label.flex_1())
        .overflow_hidden()
        .text_ellipsis()
        .text_size(px(10.0))
        .text_color(rgb(0xe8e8ea))
        .child(SharedString::from(label))
}

fn close_button(
    id: String,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(SharedString::from(id))
        .flex()
        .items_center()
        .justify_center()
        .size(px(12.0))
        .rounded_full()
        .text_size(px(10.0))
        .text_color(rgb(0x8d8f98))
        .hover(|style| style.bg(rgb(0x484a53)).text_color(rgb(0xffffff)))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
        .child("×")
}

/// Pill background split into equal vertical stripes, each painted as the
/// full rounded pill clipped to its share so the ends stay round.
fn group_stripes(colors: Vec<Hsla>) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let count = colors.len().max(1);
            let stripe = bounds.size.width / count as f32;
            let radii = Corners::all(bounds.size.height / 2.0);
            for (index, color) in colors.iter().enumerate() {
                let clip = Bounds {
                    origin: point(bounds.origin.x + stripe * index as f32, bounds.origin.y),
                    size: size(stripe, bounds.size.height),
                };
                window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                    window.paint_quad(fill(bounds, *color).corner_radii(radii));
                });
            }
        },
    )
    .absolute()
    .top(px(0.0))
    .left(px(0.0))
    .size_full()
}

/// Member count on a dark disc, ringed by one sector per member in the
/// provider's colour (same order as the stripes).
fn group_indicator(count: usize, accents: Vec<Option<u32>>) -> impl IntoElement {
    div()
        .size(px(CONTEXT_INDICATOR_SIZE))
        .relative()
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let center = point(
                        bounds.origin.x + bounds.size.width / 2.0,
                        bounds.origin.y + bounds.size.height / 2.0,
                    );
                    let sectors = accents.len().max(1) as f32;
                    for (index, accent) in accents.iter().enumerate() {
                        let color = accent.unwrap_or(0x686b75);
                        draw_sector(
                            window,
                            center,
                            px(6.5),
                            index as f32 / sectors,
                            (index + 1) as f32 / sectors,
                            rgb(color).into(),
                        );
                    }
                    draw_filled_circle(window, center, px(5.0), rgb(0x2a2b31).into());
                },
            )
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size(px(CONTEXT_INDICATOR_SIZE)),
        )
        .child(
            div()
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size(px(CONTEXT_INDICATOR_SIZE))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(8.0))
                .line_height(px(8.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(0xe8e8ea))
                .child(count.to_string()),
        )
}

/// Where dropping the dragged card (identified by its sessions) over the card
/// starting with `target` would put it: `Some(before)` when the bar order
/// would change, `None` otherwise.
fn live_reorder_destination(
    slots: &[Slot],
    dragged: &[AgentId],
    target: Option<&AgentId>,
    after_target: bool,
) -> Option<Option<AgentId>> {
    let current: Vec<_> = slots.iter().map(|slot| slot.first_id().clone()).collect();
    let dragged = dragged.first()?;
    if !current.contains(dragged) || target == Some(dragged) {
        return None;
    }
    let mut reordered: Vec<_> = current
        .iter()
        .filter(|id| *id != dragged)
        .cloned()
        .collect();
    let insertion_index = match target {
        Some(target) => {
            let index = reordered.iter().position(|id| id == target)?;
            index + usize::from(after_target)
        }
        None => reordered.len(),
    };
    let before = reordered.get(insertion_index).cloned();
    reordered.insert(insertion_index, dragged.clone());
    (reordered != current).then_some(before)
}

fn state_color(state: AgentState) -> Hsla {
    match state {
        AgentState::Active => rgb(0x43c46b).into(),
        AgentState::Waiting => rgb(0xf0a33a).into(),
        AgentState::Idle => rgba(0x25262bf2).into(),
        AgentState::Compacting => rgb(0x4f8df7).into(),
    }
}

/// Side of the square holding the context pie and its provider ring.
const CONTEXT_INDICATOR_SIZE: f32 = 14.0;

/// Context pie (grey base, white/amber/red wedge) on a thin ring in the
/// provider's brand colour. The ring is a separate, constant-width shape so
/// it never reads as a context level, whatever hue the provider uses.
fn context_pie(percentage: Option<f32>, accent_rgb: Option<u32>) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let center = point(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            );
            let radius = px(5.0);
            if let Some(accent) = accent_rgb {
                draw_filled_circle(window, center, px(6.5), rgb(accent).into());
            }
            draw_filled_circle(window, center, radius, rgb(0x686b75).into());
            if let Some(progress) = ring_progress(percentage) {
                draw_pie(
                    window,
                    center,
                    radius,
                    progress,
                    rgb(context_ring_color(percentage.unwrap_or_default())).into(),
                );
            }
        },
    )
    .size(px(CONTEXT_INDICATOR_SIZE))
}

fn ring_progress(percentage: Option<f32>) -> Option<f32> {
    percentage.map(|value| (value / 100.0).clamp(0.0, 1.0))
}

fn context_ring_color(percentage: f32) -> u32 {
    if percentage < 100.0 / 3.0 {
        0xf1f2f4
    } else if percentage < 200.0 / 3.0 {
        0xf0a33a
    } else {
        0xe45353
    }
}

fn provider_ring(icon_png: Option<Vec<u8>>, usage: Option<ProviderUsage>) -> impl IntoElement {
    let logo = icon_png.map(provider_icon);
    let has_logo = logo.is_some();
    let percentages: Vec<_> = usage
        .as_ref()
        .map(|usage| {
            usage
                .limits
                .iter()
                .take(3)
                .map(UsageLimit::used_percentage)
                .collect()
        })
        .unwrap_or_default();
    let stale = usage.as_ref().is_some_and(|usage| usage.stale);
    let three_rings = percentages.len() >= 3;
    div()
        .relative()
        .size(px(14.0))
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let center = point(
                        bounds.origin.x + bounds.size.width / 2.0,
                        bounds.origin.y + bounds.size.height / 2.0,
                    );
                    for (index, percentage) in percentages.iter().copied().enumerate() {
                        let radius = px([6.0, 4.3, 2.6][index]);
                        draw_circle(window, center, radius, px(1.0), rgb(0x555861).into());
                        draw_arc(
                            window,
                            center,
                            radius,
                            px(1.0),
                            ring_progress(Some(percentage)).unwrap_or_default(),
                            rgb(context_ring_color(percentage)).into(),
                        );
                    }
                    if !has_logo {
                        draw_circle(window, center, px(2.2), px(1.0), rgb(0xd5d6da).into());
                    }
                    if stale {
                        draw_lock_badge(
                            window,
                            point(
                                bounds.origin.x + bounds.size.width - px(3.5),
                                bounds.origin.y + bounds.size.height - px(3.5),
                            ),
                        );
                    }
                },
            )
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .size(px(14.0)),
        )
        .when_some(logo, |ring, logo| {
            let (offset, size) = if three_rings {
                (px(5.5), px(3.0))
            } else {
                (px(4.5), px(5.0))
            };
            ring.child(img(logo).absolute().top(offset).left(offset).size(size))
        })
}

fn provider_icon(bytes: Vec<u8>) -> Arc<Image> {
    // Reuse GPUI image identities across refreshes and displays for any provider.
    static ICONS: OnceLock<Mutex<HashMap<Vec<u8>, Arc<Image>>>> = OnceLock::new();
    let mut icons = ICONS
        .get_or_init(Mutex::default)
        .lock()
        .expect("provider icon cache poisoned");
    icons
        .entry(bytes.clone())
        .or_insert_with(|| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
        .clone()
}

fn format_percentage(value: f32) -> String {
    let formatted = format!("{value:.2}");
    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

fn draw_circle(
    window: &mut Window,
    center: gpui::Point<gpui::Pixels>,
    radius: gpui::Pixels,
    stroke: gpui::Pixels,
    color: Hsla,
) {
    let mut builder = PathBuilder::stroke(stroke);
    builder.move_to(point(center.x + radius, center.y));
    builder.arc_to(
        point(radius, radius),
        px(0.0),
        false,
        true,
        point(center.x - radius, center.y),
    );
    builder.arc_to(
        point(radius, radius),
        px(0.0),
        false,
        true,
        point(center.x + radius, center.y),
    );
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn draw_filled_circle(
    window: &mut Window,
    center: gpui::Point<gpui::Pixels>,
    radius: gpui::Pixels,
    color: Hsla,
) {
    let mut builder = PathBuilder::fill();
    builder.move_to(point(center.x + radius, center.y));
    builder.arc_to(
        point(radius, radius),
        px(0.0),
        false,
        true,
        point(center.x - radius, center.y),
    );
    builder.arc_to(
        point(radius, radius),
        px(0.0),
        false,
        true,
        point(center.x + radius, center.y),
    );
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// Small amber padlock on a dark disc, signalling stale (logged-out) quota data.
fn draw_lock_badge(window: &mut Window, center: gpui::Point<gpui::Pixels>) {
    draw_filled_circle(window, center, px(3.5), rgb(0x1e1f24).into());
    let lock_color: Hsla = rgb(0xf0a33a).into();

    let shackle_radius = px(1.1);
    let shackle_base = center.y - px(0.5);
    let mut builder = PathBuilder::stroke(px(0.8));
    builder.move_to(point(center.x - shackle_radius, shackle_base));
    builder.arc_to(
        point(shackle_radius, shackle_radius),
        px(0.0),
        false,
        true,
        point(center.x + shackle_radius, shackle_base),
    );
    if let Ok(path) = builder.build() {
        window.paint_path(path, lock_color);
    }

    let half_width = px(1.8);
    let top = center.y - px(0.5);
    let bottom = center.y + px(2.0);
    let mut builder = PathBuilder::fill();
    builder.move_to(point(center.x - half_width, top));
    builder.line_to(point(center.x + half_width, top));
    builder.line_to(point(center.x + half_width, bottom));
    builder.line_to(point(center.x - half_width, bottom));
    builder.line_to(point(center.x - half_width, top));
    if let Ok(path) = builder.build() {
        window.paint_path(path, lock_color);
    }
}

fn draw_pie(
    window: &mut Window,
    center: gpui::Point<gpui::Pixels>,
    radius: gpui::Pixels,
    progress: f32,
    color: Hsla,
) {
    draw_sector(window, center, radius, 0.0, progress, color);
}

/// Filled sector from `from` to `to`, fractions of a turn clockwise from the top.
fn draw_sector(
    window: &mut Window,
    center: gpui::Point<gpui::Pixels>,
    radius: gpui::Pixels,
    from: f32,
    to: f32,
    color: Hsla,
) {
    let from = from.clamp(0.0, 1.0);
    let to = to.clamp(0.0, 1.0);
    let span = to - from;
    if span <= 0.0 {
        return;
    }
    if span >= 0.999 {
        draw_filled_circle(window, center, radius, color);
        return;
    }
    let angle = |fraction: f32| -PI / 2.0 + fraction * 2.0 * PI;
    let start = point(
        center.x + radius * angle(from).cos(),
        center.y + radius * angle(from).sin(),
    );
    let end = point(
        center.x + radius * angle(to).cos(),
        center.y + radius * angle(to).sin(),
    );
    let mut builder = PathBuilder::fill();
    builder.move_to(center);
    builder.line_to(start);
    builder.arc_to(point(radius, radius), px(0.0), span > 0.5, true, end);
    builder.line_to(center);
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn draw_arc(
    window: &mut Window,
    center: gpui::Point<gpui::Pixels>,
    radius: gpui::Pixels,
    stroke: gpui::Pixels,
    progress: f32,
    color: Hsla,
) {
    let progress = progress.clamp(0.0, 1.0);
    if progress <= 0.0 {
        return;
    }
    if progress >= 0.999 {
        draw_circle(window, center, radius, stroke, color);
        return;
    }
    let mut builder = PathBuilder::stroke(stroke);
    builder.move_to(point(center.x, center.y - radius));
    let angle = -PI / 2.0 + progress * 2.0 * PI;
    builder.arc_to(
        point(radius, radius),
        px(0.0),
        progress > 0.5,
        true,
        point(
            center.x + radius * angle.cos(),
            center.y + radius * angle.sin(),
        ),
    );
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

pub struct AgentDetailsView(AgentSnapshot, String);

impl AgentDetailsView {
    pub fn new(agent: AgentSnapshot, provider_name: String) -> Self {
        Self(agent, provider_name)
    }
}

pub struct ProviderDetailsView(ProviderStatus);

impl ProviderDetailsView {
    pub fn new(provider: ProviderStatus) -> Self {
        Self(provider)
    }
}

impl Render for ProviderDetailsView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let provider = &self.0;
        let mut rows = Vec::new();
        if let Some(usage) = &provider.usage {
            if let Some(plan) = &usage.plan {
                rows.push(("Offre".to_string(), plan.clone()));
            }
            rows.extend(usage_rows(usage));
        } else {
            rows.push(("Quota".into(), "Indisponible".into()));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .size_full()
            .px(px(9.0))
            .py(px(7.0))
            .rounded_md()
            .bg(rgb(0x111216))
            .text_size(px(11.0))
            .text_color(rgb(0xf4f4f5))
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(provider.label().to_string()),
            )
            .child(
                div()
                    .text_color(rgb(0xb9bbc3))
                    .child("Utilisation du provider"),
            )
            .children(rows.into_iter().map(|(label, value)| {
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w(px(108.0))
                            .text_color(rgb(0x8f929c))
                            .child(label),
                    )
                    .child(div().child(value))
            }))
    }
}

impl Render for AgentDetailsView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let agent = &self.0;
        let provider = &self.1;
        let state = state_label(agent.state);
        let path = if !agent.cwd.as_os_str().is_empty() {
            agent.cwd.display().to_string()
        } else {
            "Indisponible".into()
        };
        let context = agent
            .context_usage
            .map(|usage| format!("{} %", format_percentage(usage.used_percentage())))
            .unwrap_or_else(|| "Indisponible".into());
        let mut rows = vec![
            ("Provider".to_string(), provider.to_string()),
            ("État".to_string(), state.to_string()),
            ("Contexte".to_string(), context),
        ];
        if let Some(usage) = &agent.provider_usage {
            if let Some(plan) = &usage.plan {
                rows.push(("Offre".into(), plan.clone()));
            }
            rows.extend(usage_rows(usage));
        } else {
            rows.push(("Quota".into(), "Indisponible".into()));
        }

        div()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .size_full()
            .px(px(9.0))
            .py(px(7.0))
            .rounded_md()
            .bg(rgb(0x111216))
            .text_size(px(11.0))
            .text_color(rgb(0xf4f4f5))
            .child(
                // The card under the pointer already shows the project name;
                // the conversation title is the information the popup adds.
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(
                        agent
                            .title
                            .clone()
                            .unwrap_or_else(|| agent.label().to_owned()),
                    ),
            )
            .child(div().text_color(rgb(0xb9bbc3)).child(path))
            .children(rows.into_iter().map(|(label, value)| {
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w(px(108.0))
                            .text_color(rgb(0x8f929c))
                            .child(label),
                    )
                    .child(div().child(value))
            }))
    }
}

/// Height of the agent details popup: one line per rendered row plus the
/// container padding, so a title or an extra quota window is never clipped.
pub fn agent_details_height(agent: &AgentSnapshot) -> f32 {
    let usage_lines = match &agent.provider_usage {
        Some(usage) => {
            usage.limits.len() + usize::from(usage.plan.is_some()) + usize::from(usage.stale)
        }
        None => 1,
    };
    // Title, path, provider, state, context.
    let lines = 5 + usage_lines;
    details_height(lines)
}

/// Height of the provider details popup for the same layout.
pub fn provider_details_height(provider: &ProviderStatus) -> f32 {
    // Provider name and the "Utilisation du provider" line, then one row per
    // quota window (or the placeholder row).
    let lines = 2 + match &provider.usage {
        Some(usage) => {
            usage.limits.len() + usize::from(usage.plan.is_some()) + usize::from(usage.stale)
        }
        None => 1,
    };
    details_height(lines)
}

fn details_height(lines: usize) -> f32 {
    // Vertical padding of the container, GPUI's default line height (phi times
    // the 11 px text size) and the column gap, plus a little slack for rounding.
    const PADDING: f32 = 14.0;
    const LINE: f32 = 11.0 * 1.618;
    const GAP: f32 = 5.0;
    const SLACK: f32 = 4.0;
    PADDING + lines as f32 * LINE + lines.saturating_sub(1) as f32 * GAP + SLACK
}

fn usage_rows(usage: &ProviderUsage) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = usage
        .limits
        .iter()
        .map(|limit| {
            let mut value = format!("{} %", format_percentage(limit.used_percentage()));
            if let Some(minutes) = limit.window_minutes {
                value.push_str(&format!(" sur {}", format_window(minutes)));
            }
            if let Some(reset) = limit.resets_at {
                value.push_str(&format!(" · reset {}", format_reset(reset)));
            }
            (limit.label.clone(), value)
        })
        .collect();
    if usage.stale {
        rows.push((
            "Session".into(),
            "expirée · données périmées (relancez /login)".into(),
        ));
    }
    rows
}

fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Active => "Actif",
        AgentState::Waiting => "En attente d’une action",
        AgentState::Idle => "Au repos",
        AgentState::Compacting => "Compaction",
    }
}

fn format_window(minutes: u64) -> String {
    if minutes.is_multiple_of(10_080) {
        format!("{} j", minutes / 1_440)
    } else if minutes.is_multiple_of(60) {
        format!("{} h", minutes / 60)
    } else {
        format!("{minutes} min")
    }
}

fn format_reset(reset: std::time::SystemTime) -> String {
    let remaining = reset
        .duration_since(std::time::SystemTime::now())
        .unwrap_or_default()
        .as_secs();
    if remaining == 0 {
        return "maintenant".into();
    }
    let days = remaining / 86_400;
    let hours = (remaining % 86_400) / 3_600;
    let minutes = (remaining % 3_600) / 60;
    if days > 0 {
        format!("dans {days} j {hours} h")
    } else if hours > 0 {
        format!("dans {hours} h {minutes} min")
    } else {
        format!("dans {} min", minutes.max(1))
    }
}

#[derive(Debug, Clone)]
pub enum RenameEvent {
    Submit { target: RenameKey, name: String },
    Cancel,
}

/// Prompt opened by a right click on a card: the hover details (conversation
/// title, full path, provider and state) above a text field pre-filled with
/// the current label, fully selected so typing replaces it.
pub struct RenameView {
    target: RenameTarget,
    provider_names: Vec<String>,
    input: Entity<TextInput>,
    was_active: bool,
    _subscriptions: Vec<Subscription>,
}

impl RenameView {
    /// `provider_names` holds the display name of each session's provider
    /// (one entry for a session, one per member for a group).
    pub fn new(
        target: RenameTarget,
        provider_names: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new(target.label().to_owned(), cx));
        let key = target.key();
        let input_events = cx.subscribe(&input, move |this, input, event, cx| match event {
            TextInputEvent::Confirm => {
                let name = input.read(cx).text().to_owned();
                this.was_active = false;
                cx.emit(RenameEvent::Submit {
                    target: key.clone(),
                    name,
                });
            }
            TextInputEvent::Cancel => {
                this.was_active = false;
                cx.emit(RenameEvent::Cancel);
            }
        });
        // Clicking anywhere else dismisses the prompt, like a native rename field.
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            if this.was_active && !active {
                cx.emit(RenameEvent::Cancel);
            }
            this.was_active = active;
        });
        window.focus(&input.focus_handle(cx), cx);
        Self {
            target,
            provider_names,
            input,
            was_active: false,
            _subscriptions: vec![input_events, activation],
        }
    }

    fn heading(&self) -> String {
        match &self.target {
            RenameTarget::Session(agent) => agent
                .title
                .clone()
                .unwrap_or_else(|| agent.project_name.clone()),
            RenameTarget::Group(group) => group.project_name.clone(),
        }
    }

    fn path(&self) -> String {
        let cwd = match &self.target {
            RenameTarget::Session(agent) => Some(agent.cwd.as_path()),
            RenameTarget::Group(group) => group.common_cwd(),
        };
        match cwd {
            Some(cwd) if !cwd.as_os_str().is_empty() => cwd.display().to_string(),
            Some(_) => "Chemin indisponible".into(),
            None => "Plusieurs dossiers".into(),
        }
    }

    fn meta(&self) -> String {
        match &self.target {
            RenameTarget::Session(agent) => {
                let provider = self.provider_names.first().cloned().unwrap_or_default();
                let mut meta = format!("{provider} · {}", state_label(agent.state));
                if let Some(usage) = agent.context_usage {
                    meta.push_str(&format!(
                        " · contexte {} %",
                        format_percentage(usage.used_percentage())
                    ));
                }
                meta
            }
            RenameTarget::Group(group) => {
                let mut providers: Vec<&str> =
                    self.provider_names.iter().map(String::as_str).collect();
                providers.sort_unstable();
                providers.dedup();
                format!(
                    "{} conversations · {}",
                    group.members.len(),
                    providers.join(", ")
                )
            }
        }
    }
}

impl EventEmitter<RenameEvent> for RenameView {}

impl Render for RenameView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(5.0))
            .size_full()
            .px(px(9.0))
            .py(px(7.0))
            .rounded_md()
            .bg(rgb(0x111216))
            .text_size(px(11.0))
            .text_color(rgb(0xf4f4f5))
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(self.heading()),
            )
            .child(
                div()
                    .text_color(rgb(0xb9bbc3))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(self.path()),
            )
            .child(div().text_color(rgb(0x8f929c)).child(self.meta()))
            .child(self.input.clone())
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(0x8f929c))
                    .child("Entrée : renommer · Échap : annuler · vide : nom du dépôt"),
            )
    }
}

/// Height of the rename prompt: three text rows, the field and the hint line.
pub fn rename_height() -> f32 {
    // Field: 16 px line, 4 px vertical padding and 1 px border on both sides.
    const FIELD: f32 = 16.0 + 2.0 * 4.0 + 2.0;
    details_height(4) + FIELD + 5.0
}

struct DragPreview {
    label: String,
}

#[derive(Clone)]
struct BarDrag;

struct BarDragPreview;

impl Render for BarDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size(px(1.0)).opacity(0.0)
    }
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_full()
            .bg(rgb(0x3a3c45))
            .text_xs()
            .text_color(rgb(0xffffff))
            .child(self.label.clone())
    }
}

pub fn natural_width(slots: &[Slot], providers: &[ProviderStatus], max_width: f32) -> f32 {
    // Root padding + the trailing drop target, then one gap per card (including
    // the gap before that target). Keeping these outside the per-card estimate
    // prevents the last close button from being clipped.
    let provider_width = providers.len() as f32 * 20.0 + f32::from(!providers.is_empty()) * 4.0;
    let chrome = 14.0 + slots.len() as f32 * 3.0 + provider_width;
    let content = chrome
        + slots
            .iter()
            .map(|slot| card_width(slot.label()))
            .sum::<f32>()
        + if slots.is_empty() { 55.0 } else { 0.0 };
    content.clamp(76.0, max_width.max(76.0))
}

const BAR_LABEL_MAX_WIDTH: f32 = 122.0;
const MENU_LABEL_MAX_WIDTH: f32 = 240.0;

/// Estimated width of one card for the given label.
fn card_width(label: &str) -> f32 {
    estimated_card_width(label, BAR_LABEL_MAX_WIDTH)
}

fn estimated_card_width(label: &str, label_max_width: f32) -> f32 {
    let text = (label.chars().count() as f32 * 5.5).min(label_max_width);
    44.0 + text
}

/// Size of the menu window listing a group's members as vertical cards.
pub fn menu_size(members: &[AgentSnapshot]) -> (f32, f32) {
    let widest = members
        .iter()
        .map(|member| estimated_card_width(member.menu_label(), MENU_LABEL_MAX_WIDTH))
        .fold(0.0, f32::max);
    let width = 6.0 + widest + 4.0;
    let height = 6.0 + members.len() as f32 * 17.0 + members.len().saturating_sub(1) as f32 * 3.0;
    (width.max(76.0), height)
}

#[derive(Debug, Clone, Default)]
pub struct DiagnosticsStatus {
    pub claude_found: bool,
    pub hook_found: bool,
    pub plugin_found: bool,
    pub statusline_wrapped: bool,
    pub existing_statusline: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub enum DiagnosticsEvent {
    Install,
    Uninstall,
}

pub struct DiagnosticsView {
    status: DiagnosticsStatus,
}

impl DiagnosticsView {
    pub fn new(status: DiagnosticsStatus) -> Self {
        Self { status }
    }

    pub fn set_message(&mut self, message: String, cx: &mut Context<Self>) {
        self.status.message = Some(message);
        cx.notify();
    }
}

impl EventEmitter<DiagnosticsEvent> for DiagnosticsView {}

impl Render for DiagnosticsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fully_integrated = self.status.hook_found && self.status.statusline_wrapped;
        let (headline, detail, button_label) = if fully_integrated && self.status.plugin_found {
            (
                "Claude Code est connecté",
                "AgentsMon affiche l’état des sessions et leur utilisation du contexte."
                    .to_string(),
                "Désactiver l’intégration",
            )
        } else if fully_integrated {
            (
                "Indicateur de contexte actif",
                "Le suivi des sessions reste assuré par votre intégration existante.".to_string(),
                "Désactiver l’indicateur de contexte",
            )
        } else if self.status.hook_found {
            (
                "Afficher l’utilisation du contexte",
                "Le suivi des sessions est déjà actif. AgentsMon peut aussi afficher le pourcentage de contexte utilisé.".to_string(),
                "Activer l’indicateur de contexte",
            )
        } else if !self.status.claude_found {
            (
                "Claude Code est introuvable",
                "AgentsMon ne trouve pas la commande Claude Code et ne peut pas installer l’intégration.".to_string(),
                "Installer l’intégration Claude Code",
            )
        } else {
            (
                "Connecter Claude Code",
                "AgentsMon installera son plugin Claude Code pour votre compte utilisateur. Aucun contenu de conversation ne sera lu.".to_string(),
                "Installer l’intégration Claude Code",
            )
        };
        let statusline = match (
            &self.status.existing_statusline,
            self.status.statusline_wrapped,
        ) {
            (_, true) => {
                "Seuls l’identifiant de session et le pourcentage de contexte sont transmis à AgentsMon.".to_string()
            }
            (Some(command), false) => {
                format!("Votre status line actuelle sera conservée et continuera de fonctionner normalement. Commande détectée : {command}")
            }
            (None, false) => {
                "AgentsMon ajoutera une intégration invisible à Claude Code. Seuls l’identifiant de session et le pourcentage de contexte seront transmis.".to_string()
            }
        };
        let installed = fully_integrated;

        div()
            .flex()
            .flex_col()
            .gap_4()
            .p_5()
            .size_full()
            .text_sm()
            .bg(rgb(0x1c1d21))
            .text_color(rgb(0xe8e8ea))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(headline),
                    )
                    .child(div().text_color(rgb(0xa8aab2)).child(detail)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pt_3()
                    .border_t_1()
                    .border_color(rgb(0x303137))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child("Utilisation du contexte"),
                    )
                    .child(div().text_color(rgb(0xa8aab2)).child(statusline)),
            )
            .when_some(self.status.message.clone(), |root, message| {
                root.child(div().text_color(rgb(0xe1a34b)).child(message))
            })
            .child(
                div().flex().justify_end().child(
                    div()
                        .id("integration-action")
                        .px_3()
                        .py_1()
                        .rounded_sm()
                        .bg(if installed {
                            rgb(0x34353b)
                        } else {
                            rgb(0x315f8c)
                        })
                        .hover(|style| style.opacity(0.88))
                        .cursor_pointer()
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(if installed {
                                DiagnosticsEvent::Uninstall
                            } else {
                                DiagnosticsEvent::Install
                            });
                        }))
                        .child(button_label),
                ),
            )
    }
}

#[derive(Debug, Clone)]
pub enum CodexApprovalEvent {
    Close,
}

pub struct CodexApprovalView;

impl EventEmitter<CodexApprovalEvent> for CodexApprovalView {}

impl Render for CodexApprovalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .justify_between()
            .gap_4()
            .p_5()
            .size_full()
            .text_sm()
            .bg(rgb(0x1c1d21))
            .text_color(rgb(0xe8e8ea))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Autoriser le suivi de Codex"),
                    )
                    .child(
                        div()
                            .text_color(rgb(0xa8aab2))
                            .child("ChatGPT n’exécute pas encore les hooks AgentsMon."),
                    )
                    .child(
                        div()
                            .pt_2()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child("Dans ChatGPT : Réglages → Hooks → User config"),
                    )
                    .child(
                        div()
                            .text_color(rgb(0xa8aab2))
                            .child("Approuvez ensuite tous les hooks AgentsMon."),
                    ),
            )
            .child(
                div().flex().justify_end().child(
                    div()
                        .id("codex-approval-close")
                        .px_3()
                        .py_1()
                        .rounded_sm()
                        .bg(rgb(0x315f8c))
                        .hover(|style| style.opacity(0.88))
                        .cursor_pointer()
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(CodexApprovalEvent::Close);
                        }))
                        .child("J’ai compris"),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(id: &str) -> AgentSnapshot {
        AgentSnapshot {
            id: AgentId::new("claude", id),
            pid: 1,
            cwd: format!("/tmp/{id}").into(),
            project_name: id.into(),
            title: None,
            custom_name: None,
            state: AgentState::Active,
            context_usage: None,
            provider_usage: None,
            focus_target: Default::default(),
            last_activity: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn single(id: &str) -> Slot {
        Slot::Single(Box::new(snapshot(id)))
    }

    #[test]
    fn empty_and_overflow_widths_are_bounded() {
        assert_eq!(natural_width(&[], &[], 900.0), 76.0);
        let mut snapshot = AgentSnapshot {
            id: AgentId::new("claude", "s"),
            pid: 1,
            cwd: "/tmp/very-long-project".into(),
            project_name: "x".repeat(100),
            title: None,
            custom_name: None,
            state: AgentState::Active,
            context_usage: None,
            provider_usage: None,
            focus_target: Default::default(),
            last_activity: std::time::SystemTime::UNIX_EPOCH,
        };
        assert_eq!(
            natural_width(
                &vec![Slot::Single(Box::new(snapshot.clone())); 20],
                &[],
                500.0
            ),
            500.0
        );
        snapshot.project_name = "a".into();
        assert!(natural_width(&[Slot::Single(Box::new(snapshot.clone()))], &[], 500.0) >= 76.0);
        // The bar is sized on the label the card shows, not the project name.
        let narrow = natural_width(&[Slot::Single(Box::new(snapshot.clone()))], &[], 900.0);
        snapshot.custom_name = Some("un nom nettement plus long".into());
        assert!(natural_width(&[Slot::Single(Box::new(snapshot))], &[], 900.0) > narrow);
    }

    #[test]
    fn menu_grows_with_members_and_their_labels() {
        let mut long = snapshot("b");
        long.custom_name = Some("une conversation au nom long".into());
        let (width, height) = menu_size(&[snapshot("a"), long.clone()]);
        assert_eq!(height, 6.0 + 2.0 * 17.0 + 3.0);
        assert!(width > menu_size(&[snapshot("a"), snapshot("c")]).0);
        assert_eq!(
            menu_size(&[snapshot("a"), long, snapshot("c")]).1,
            6.0 + 3.0 * 17.0 + 6.0
        );
    }

    #[test]
    fn menu_is_sized_on_conversation_titles() {
        // The menu shows the agent's title while the bar keeps the project
        // name, so only the menu grows with a long title.
        let mut titled = snapshot("a");
        let plain_menu = menu_size(&[titled.clone()]).0;
        let plain_bar = natural_width(&[Slot::Single(Box::new(titled.clone()))], &[], 900.0);
        titled.title = Some("Investigate the flaky integration test suite".into());
        assert!(menu_size(&[titled.clone()]).0 > plain_menu);
        assert_eq!(
            natural_width(&[Slot::Single(Box::new(titled.clone()))], &[], 900.0),
            plain_bar
        );
        // A custom name still wins over the title everywhere.
        titled.custom_name = Some("x".into());
        assert_eq!(menu_size(&[titled]).0, menu_size(&[snapshot("x")]).0);
    }

    #[test]
    fn details_height_grows_with_quota_rows() {
        let mut agent = snapshot("a");
        let bare = agent_details_height(&agent);
        agent.title = Some("Fix the build".into());
        let titled = agent_details_height(&agent);
        assert_eq!(titled, bare);
        agent.provider_usage = Some(ProviderUsage {
            limits: vec![
                UsageLimit::new("5 h", 1.0, None, None).unwrap(),
                UsageLimit::new("7 j", 1.0, None, None).unwrap(),
                UsageLimit::new("7 j · Fable", 1.0, None, None).unwrap(),
            ],
            plan: None,
            stale: false,
        });
        // Seven rows with the placeholder quota row, nine with three windows.
        assert!((agent_details_height(&agent) - titled - 2.0 * (11.0 * 1.618 + 5.0)).abs() < 0.01);
    }

    #[test]
    fn provider_details_height_counts_header_lines() {
        let usage = ProviderUsage {
            limits: vec![
                UsageLimit::new("5 h", 1.0, None, None).unwrap(),
                UsageLimit::new("7 j", 1.0, None, None).unwrap(),
                UsageLimit::new("7 j · Fable", 1.0, None, None).unwrap(),
            ],
            plan: None,
            stale: false,
        };
        let three = provider_details_height(&ProviderStatus::new("claude", Some(usage)));
        let none = provider_details_height(&ProviderStatus::new("claude", None));
        // Five lines with three windows, three with the placeholder row.
        assert!((three - none - 2.0 * (11.0 * 1.618 + 5.0)).abs() < 0.01);
        assert!(none > 3.0 * 11.0 * 1.618);
    }

    #[test]
    fn context_ring_handles_acceptance_boundaries() {
        assert_eq!(ring_progress(Some(0.0)), Some(0.0));
        assert_eq!(ring_progress(Some(1.0)), Some(0.01));
        assert_eq!(ring_progress(Some(50.0)), Some(0.5));
        assert_eq!(ring_progress(Some(99.0)), Some(0.99));
        assert_eq!(ring_progress(Some(100.0)), Some(1.0));
        assert_eq!(ring_progress(None), None);
        assert_eq!(context_ring_color(0.0), 0xf1f2f4);
        assert_eq!(context_ring_color(33.32), 0xf1f2f4);
        assert_eq!(context_ring_color(33.34), 0xf0a33a);
        assert_eq!(context_ring_color(66.65), 0xf0a33a);
        assert_eq!(context_ring_color(66.67), 0xe45353);
        assert_eq!(context_ring_color(100.0), 0xe45353);
        assert_eq!(format_percentage(68.5), "68.5");
        assert_eq!(format_window(300), "5 h");
        assert_eq!(format_window(10_080), "7 j");
    }

    #[test]
    fn live_reorder_uses_the_target_half_and_avoids_duplicate_updates() {
        let slots = vec![single("a"), single("b"), single("c")];
        let a = AgentId::new("claude", "a");
        let b = AgentId::new("claude", "b");
        let c = AgentId::new("claude", "c");

        assert_eq!(
            live_reorder_destination(&slots, std::slice::from_ref(&a), Some(&b), true),
            Some(Some(c.clone()))
        );
        assert_eq!(
            live_reorder_destination(&slots, std::slice::from_ref(&c), Some(&b), false),
            Some(Some(b.clone()))
        );
        assert_eq!(
            live_reorder_destination(&slots, std::slice::from_ref(&a), Some(&b), false),
            None
        );
        assert_eq!(
            live_reorder_destination(&slots, std::slice::from_ref(&a), None, true),
            Some(None)
        );
        assert_eq!(
            live_reorder_destination(&slots, std::slice::from_ref(&c), None, true),
            None
        );
        assert_eq!(
            live_reorder_destination(&slots, std::slice::from_ref(&a), Some(&a), true),
            None
        );
        assert_eq!(live_reorder_destination(&slots, &[], Some(&a), true), None);
    }

    #[test]
    fn live_reorder_moves_groups_as_one_card() {
        let mut d = snapshot("d");
        d.cwd = "/tmp/a".into();
        let group = Slot::Group(SessionGroup {
            key: GroupKey::Directory("/tmp/a".into()),
            project_name: "a".into(),
            custom_name: None,
            members: vec![snapshot("a"), d],
        });
        let slots = vec![group.clone(), single("b"), single("c")];
        let a = AgentId::new("claude", "a");
        let b = AgentId::new("claude", "b");
        let c = AgentId::new("claude", "c");

        // Dragging the group after "b" lands it before "c".
        assert_eq!(
            live_reorder_destination(&slots, &group.ids(), Some(&b), true),
            Some(Some(c.clone()))
        );
        // Dropping "c" onto the first half of the group puts it before the group.
        assert_eq!(
            live_reorder_destination(&slots, std::slice::from_ref(&c), Some(&a), false),
            Some(Some(a.clone()))
        );
        assert_eq!(
            live_reorder_destination(&slots, &group.ids(), Some(&b), false),
            None
        );
    }
}
