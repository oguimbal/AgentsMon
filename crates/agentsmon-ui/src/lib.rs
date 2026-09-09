//! GPUI views for AgentsMon.

use std::collections::HashSet;
use std::f32::consts::PI;
use std::sync::{Arc, OnceLock};

use agentsmon_core::{
    AgentId, AgentSnapshot, AgentState, ProviderStatus, ProviderUsage, UsageLimit,
};
use gpui::prelude::*;
use gpui::{
    canvas, div, img, point, px, rgb, rgba, Context, DragMoveEvent, EventEmitter, Hsla, Image,
    ImageFormat, IntoElement, MouseButton, MouseDownEvent, PathBuilder, Render, SharedString,
    Window,
};

#[derive(Debug, Clone)]
pub enum MonitorEvent {
    Focus(AgentId),
    Hide(AgentId),
    Reorder {
        dragged: AgentId,
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
}

pub struct MonitorView {
    agents: Vec<AgentSnapshot>,
    providers: Vec<ProviderStatus>,
    unread: HashSet<AgentId>,
    bar_drag_last_x: Option<gpui::Pixels>,
}

impl MonitorView {
    pub fn new(
        agents: Vec<AgentSnapshot>,
        providers: Vec<ProviderStatus>,
        unread: HashSet<AgentId>,
    ) -> Self {
        Self {
            agents,
            providers,
            unread,
            bar_drag_last_x: None,
        }
    }

    pub fn set_data(
        &mut self,
        agents: Vec<AgentSnapshot>,
        providers: Vec<ProviderStatus>,
        unread: HashSet<AgentId>,
        cx: &mut Context<Self>,
    ) {
        self.agents = agents;
        self.providers = providers;
        self.unread = unread;
        cx.notify();
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
            .child(provider_ring(provider.id, usage))
    }

    fn render_card(&self, agent: AgentSnapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let id = agent.id.clone();
        let focus_id = id.clone();
        let hide_id = id.clone();
        let live_target = id.clone();
        let drag_label = agent.project_name.clone();
        let context = agent.context_usage.map(|usage| usage.used_percentage());
        let hover_details = agent.clone();
        let is_unread = self.unread.contains(&id);
        let state_background = state_color(agent.state);

        div()
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
            .on_click(cx.listener(move |_, _, _event, cx| {
                cx.emit(MonitorEvent::Focus(focus_id.clone()));
            }))
            .on_drag(id.clone(), move |_, _, _, cx| {
                cx.new(|_| DragPreview {
                    label: drag_label.clone(),
                })
            })
            .drag_over::<AgentId>(|style, _, _, _| style.bg(rgb(0x3a3c45)))
            .on_drag_move::<AgentId>(cx.listener(
                move |this, event: &DragMoveEvent<AgentId>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let dragged = event.drag(cx);
                    let after = event.event.position.x
                        >= event.bounds.origin.x + event.bounds.size.width / 2.0;
                    if let Some(before) =
                        live_reorder_destination(&this.agents, dragged, Some(&live_target), after)
                    {
                        cx.emit(MonitorEvent::Reorder {
                            dragged: dragged.clone(),
                            before,
                        });
                    }
                },
            ))
            .when(is_unread, |card| {
                card.child(
                    div()
                        .absolute()
                        .top(px(-1.0))
                        .left(px(-1.0))
                        .size(px(5.0))
                        .rounded_full()
                        .bg(rgb(0xf2b84b)),
                )
            })
            .child(
                div()
                    .id(SharedString::from(format!("context-{}", id.session_id)))
                    .size(px(12.0))
                    .child(context_pie(context)),
            )
            .child(
                div()
                    .max_w(px(122.0))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(10.0))
                    .text_color(rgb(0xe8e8ea))
                    .child(SharedString::from(agent.project_name)),
            )
            .child(
                div()
                    .id(SharedString::from(format!("hide-{}", hide_id.session_id)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(12.0))
                    .rounded_full()
                    .text_size(px(10.0))
                    .text_color(rgb(0x8d8f98))
                    .hover(|style| style.bg(rgb(0x484a53)).text_color(rgb(0xffffff)))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(MonitorEvent::Hide(hide_id.clone()));
                    }))
                    .child("×"),
            )
    }
}

impl EventEmitter<MonitorEvent> for MonitorView {}

impl Render for MonitorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        if self.agents.is_empty() {
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
                self.agents
                    .clone()
                    .into_iter()
                    .map(|agent| self.render_card(agent, cx)),
            )
            .child(
                div()
                    .id("reorder-at-end")
                    .h(px(17.0))
                    .min_w(px(8.0))
                    .drag_over::<AgentId>(|style, _, _, _| style.bg(rgb(0x3a3c45)))
                    .on_drag_move::<AgentId>(cx.listener(
                        |this, event: &DragMoveEvent<AgentId>, _, cx| {
                            if !event.bounds.contains(&event.event.position) {
                                return;
                            }
                            let dragged = event.drag(cx);
                            if let Some(before) =
                                live_reorder_destination(&this.agents, dragged, None, true)
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
}

fn live_reorder_destination(
    agents: &[AgentSnapshot],
    dragged: &AgentId,
    target: Option<&AgentId>,
    after_target: bool,
) -> Option<Option<AgentId>> {
    let current: Vec<_> = agents.iter().map(|agent| agent.id.clone()).collect();
    if !current.contains(dragged) {
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

fn context_pie(percentage: Option<f32>) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let center = point(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            );
            let radius = px(5.0);
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
    .size(px(12.0))
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

fn provider_ring(provider: String, usage: Option<ProviderUsage>) -> impl IntoElement {
    let logo = provider_logo(&provider);
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

fn provider_logo(provider: &str) -> Option<Arc<Image>> {
    static OPENAI: OnceLock<Arc<Image>> = OnceLock::new();
    static CLAUDE: OnceLock<Arc<Image>> = OnceLock::new();
    match provider {
        "codex" => Some(
            OPENAI
                .get_or_init(|| {
                    Arc::new(Image::from_bytes(
                        ImageFormat::Png,
                        include_bytes!("../assets/openai-16.png").to_vec(),
                    ))
                })
                .clone(),
        ),
        "claude" => Some(
            CLAUDE
                .get_or_init(|| {
                    Arc::new(Image::from_bytes(
                        ImageFormat::Png,
                        include_bytes!("../assets/claude-16.png").to_vec(),
                    ))
                })
                .clone(),
        ),
        _ => None,
    }
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
    let progress = progress.clamp(0.0, 1.0);
    if progress <= 0.0 {
        return;
    }
    if progress >= 0.999 {
        draw_filled_circle(window, center, radius, color);
        return;
    }

    let start = point(center.x, center.y - radius);
    let angle = -PI / 2.0 + progress * 2.0 * PI;
    let end = point(
        center.x + radius * angle.cos(),
        center.y + radius * angle.sin(),
    );
    let mut builder = PathBuilder::fill();
    builder.move_to(center);
    builder.line_to(start);
    builder.arc_to(point(radius, radius), px(0.0), progress > 0.5, true, end);
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

pub struct AgentDetailsView(AgentSnapshot);

impl AgentDetailsView {
    pub fn new(agent: AgentSnapshot) -> Self {
        Self(agent)
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
                    .child(provider_label(&provider.id).to_string()),
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
        let provider = provider_label(&agent.id.provider);
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
                            .unwrap_or_else(|| agent.project_name.clone()),
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
    let lines = match &provider.usage {
        Some(usage) => {
            1 + usage.limits.len() + usize::from(usage.plan.is_some()) + usize::from(usage.stale)
        }
        None => 2,
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

fn provider_label(provider: &str) -> &str {
    match provider {
        "claude" => "Claude",
        "codex" => "Codex",
        value => value,
    }
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

pub fn natural_width(
    agents: &[AgentSnapshot],
    providers: &[ProviderStatus],
    max_width: f32,
) -> f32 {
    // Root padding + the trailing drop target, then one gap per card (including
    // the gap before that target). Keeping these outside the per-card estimate
    // prevents the last close button from being clipped.
    let provider_width = providers.len() as f32 * 20.0 + f32::from(!providers.is_empty()) * 4.0;
    let chrome = 14.0 + agents.len() as f32 * 3.0 + provider_width;
    let content = chrome
        + agents
            .iter()
            .map(|agent| 42.0 + agent.project_name.chars().count().min(27) as f32 * 5.5)
            .sum::<f32>()
        + if agents.is_empty() { 55.0 } else { 0.0 };
    content.clamp(76.0, max_width.max(76.0))
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
            state: AgentState::Active,
            context_usage: None,
            provider_usage: None,
            focus_target: Default::default(),
            last_activity: std::time::SystemTime::UNIX_EPOCH,
        }
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
            state: AgentState::Active,
            context_usage: None,
            provider_usage: None,
            focus_target: Default::default(),
            last_activity: std::time::SystemTime::UNIX_EPOCH,
        };
        assert_eq!(
            natural_width(&vec![snapshot.clone(); 20], &[], 500.0),
            500.0
        );
        snapshot.project_name = "a".into();
        assert!(natural_width(&[snapshot], &[], 500.0) >= 76.0);
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
        let agents = vec![snapshot("a"), snapshot("b"), snapshot("c")];
        let a = AgentId::new("claude", "a");
        let b = AgentId::new("claude", "b");
        let c = AgentId::new("claude", "c");

        assert_eq!(
            live_reorder_destination(&agents, &a, Some(&b), true),
            Some(Some(c.clone()))
        );
        assert_eq!(
            live_reorder_destination(&agents, &c, Some(&b), false),
            Some(Some(b.clone()))
        );
        assert_eq!(live_reorder_destination(&agents, &a, Some(&b), false), None);
        assert_eq!(
            live_reorder_destination(&agents, &a, None, true),
            Some(None)
        );
        assert_eq!(live_reorder_destination(&agents, &c, None, true), None);
    }
}
