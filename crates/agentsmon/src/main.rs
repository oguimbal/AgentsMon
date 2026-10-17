use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentsmon_core::{AgentStore, GroupKey, ProviderStatus, Slot};
use agentsmon_providers::{
    AgentProvider, ClaudeProvider, CodexProvider, OpenClawProvider, ProviderWatcher, VibeProvider,
};
use agentsmon_ui::{
    action_menu_height, agent_details_height, menu_size, natural_width, provider_details_height,
    rename_height, rename_key_bindings, ActionMenuEvent, ActionMenuView, AgentDetailsView, Layout,
    MenuAction, MonitorEvent, MonitorView, ProviderDetailsView, RenameEvent, RenameKey,
    RenameTarget, RenameView,
};
use gpui::{
    actions, point, px, size, AnyWindowHandle, App, AppContext, AsyncApp, Bounds, DisplayId,
    Entity, Menu, MenuItem, Pixels, Subscription, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions,
};
use gpui_platform::application;

actions!(
    agentsmon,
    [Quit, ShowBars, Diagnostics, ConnectionDiagnostics]
);

const BAR_HEIGHT: f32 = 23.0;
const SCREEN_INSET: f32 = 0.0;
const DETAILS_WIDTH: f32 = 400.0;
const DETAILS_GAP: f32 = 6.0;
const RENAME_WINDOW_TITLE: &str = "Renommer la conversation";
const MENU_WINDOW_TITLE: &str = "Conversations du groupe";
const ACTIONS_WINDOW_TITLE: &str = "Actions des providers";
const ACTIONS_MENU_WIDTH: f32 = 200.0;
/// How long the pointer may be outside both a group card and its menu before
/// the menu closes; covers the travel between the two.
const MENU_GRACE: Duration = Duration::from_millis(300);
const WATCHED_POLL_INTERVAL: Duration = Duration::from_secs(10);
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(3);
const CHANGE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

mod connection_diagnostics;
mod custom_names;
mod git_worktree;
mod hidden_sessions;
mod local_source;
mod notification;

struct Panel {
    display_id: DisplayId,
    display_bounds: Bounds<Pixels>,
    max_width: f32,
    handle: AnyWindowHandle,
    view: Entity<MonitorView>,
    _subscription: Subscription,
}

#[derive(Clone)]
struct AppState(Rc<AppStateInner>);

/// The popup listing the members of a group card, with the bookkeeping that
/// keeps it open while the pointer travels from the card to the menu.
struct GroupMenu {
    handle: AnyWindowHandle,
    view: Entity<MonitorView>,
    key: GroupKey,
    member_count: usize,
    display_id: DisplayId,
    display_bounds: Bounds<Pixels>,
    bounds: Bounds<Pixels>,
    pill_hovered: bool,
    outside_since: Option<Instant>,
    _subscription: Subscription,
}

struct AppStateInner {
    local_source: local_source::LocalSource,
    store: RefCell<AgentStore>,
    worktrees: RefCell<git_worktree::WorktreeResolver>,
    provider_statuses: RefCell<Vec<ProviderStatus>>,
    providers: Vec<Arc<dyn AgentProvider>>,
    /// Also in `providers`; kept typed for its connect flow.
    openclaw: Arc<OpenClawProvider>,
    watchers: RefCell<Vec<ProviderWatcher>>,
    panels: RefCell<Vec<Panel>>,
    x_offset: RefCell<f32>,
    details_window: RefCell<Option<AnyWindowHandle>>,
    rename_window: RefCell<Option<AnyWindowHandle>>,
    actions_menu: RefCell<Option<AnyWindowHandle>>,
    openclaw_window: RefCell<Option<AnyWindowHandle>>,
    group_menu: RefCell<Option<GroupMenu>>,
    unread: RefCell<HashSet<agentsmon_core::AgentId>>,
    notifications: notification::NativeNotifications,
    connection_diagnostics: RefCell<connection_diagnostics::ConnectionDiagnostics>,
    connection_window: RefCell<Option<AnyWindowHandle>>,
    /// Last hidden-session list written to disk, to skip redundant writes.
    persisted_hidden: RefCell<Vec<agentsmon_core::HiddenSession>>,
    custom_names: RefCell<custom_names::Persistence>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    application().run(|cx| {
        let openclaw = Arc::new(OpenClawProvider::new());
        let providers: Vec<Arc<dyn AgentProvider>> = vec![
            Arc::new(ClaudeProvider::new()),
            Arc::new(CodexProvider::new()),
            Arc::new(VibeProvider::new()),
            openclaw.clone(),
        ];
        let watchers: Vec<_> = providers
            .iter()
            .filter_map(|provider| {
                provider
                    .watch()
                    .map_err(|error| {
                        tracing::warn!(provider = provider.id(), %error, "filesystem watcher unavailable; polling remains active");
                        error
                    })
                    .ok()
            })
            .collect();
        let poll_interval = if watchers.len() == providers.len() {
            WATCHED_POLL_INTERVAL
        } else {
            FALLBACK_POLL_INTERVAL
        };
        let (mut store, persisted_hidden) = restored_store();
        let custom_names = custom_names::Persistence::new();
        store.restore_custom_names(custom_names.restored());
        let state = AppState(Rc::new(AppStateInner {
            local_source: local_source::LocalSource::new(),
            store: RefCell::new(store),
            worktrees: RefCell::default(),
            provider_statuses: RefCell::new(Vec::new()),
            providers,
            openclaw,
            watchers: RefCell::new(watchers),
            panels: RefCell::new(Vec::new()),
            x_offset: RefCell::new(0.0),
            details_window: RefCell::new(None),
            rename_window: RefCell::new(None),
            actions_menu: RefCell::new(None),
            openclaw_window: RefCell::new(None),
            group_menu: RefCell::new(None),
            unread: RefCell::new(HashSet::new()),
            notifications: notification::NativeNotifications::new(),
            connection_diagnostics: RefCell::new(connection_diagnostics::ConnectionDiagnostics::new()),
            connection_window: RefCell::new(None),
            persisted_hidden: RefCell::new(persisted_hidden),
            custom_names: RefCell::new(custom_names),
        }));

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys(rename_key_bindings());
        let show_state = state.clone();
        cx.on_action(move |_: &ShowBars, cx| reconcile_displays(cx, &show_state, true));
        let diagnostics_state = state.clone();
        cx.on_action(move |_: &Diagnostics, cx| open_diagnostics(cx, &diagnostics_state));
        let connection_state = state.clone();
        cx.on_action(move |_: &ConnectionDiagnostics, cx| {
            let report = connection_state.0.connection_diagnostics.borrow().current_prompt();
            open_connection_report(cx, &connection_state, "Diagnostic des connexions".into(), report);
        });
        cx.set_menus([Menu::new("AgentsMon").items([
            MenuItem::action("Afficher les barres", ShowBars),
            MenuItem::action("Diagnostic Claude…", Diagnostics),
            MenuItem::action("Diagnostic des connexions…", ConnectionDiagnostics),
            MenuItem::separator(),
            MenuItem::action("Quitter AgentsMon", Quit),
        ])]);

        refresh(&state, cx);
        reconcile_displays(cx, &state, true);
        ensure_integrations(cx);
        spawn_refresh_loop(cx, state, poll_interval);
    });
}

fn ensure_integrations(cx: &mut App) {
    let (sender, receiver) = crossbeam_channel::bounded(1);
    let _ = std::thread::Builder::new()
        .name("agentsmon-integration-setup".into())
        .spawn(move || {
            match integration::install() {
                Ok(()) => tracing::info!("Claude integration installed or repaired automatically"),
                Err(error) => {
                    tracing::warn!(%error, "automatic Claude integration setup failed; use the application diagnostic to retry")
                }
            }
            match vibe_integration::install() {
                Ok(()) => tracing::info!("Mistral Vibe integration installed automatically"),
                Err(error) => tracing::warn!(%error, "automatic Mistral Vibe integration setup failed"),
            }
            match codex_integration::install() {
                Ok(()) => {
                    tracing::info!("Codex integration installed automatically");
                    match codex_integration::hooks_need_approval() {
                        Ok(needs_approval) => {
                            let _ = sender.send(needs_approval);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "cannot inspect Codex hook approval status")
                        }
                    }
                }
                Err(error) => tracing::warn!(%error, "automatic Codex integration setup failed"),
            }
        });

    cx.spawn(async move |cx: &mut AsyncApp| loop {
        match receiver.try_recv() {
            Ok(true) => {
                cx.update(open_codex_approval);
                break;
            }
            Ok(false) | Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            Err(crossbeam_channel::TryRecvError::Empty) => {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        }
    })
    .detach();
}

fn open_codex_approval(cx: &mut App) {
    let options = WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("Autoriser Codex".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(120.0), px(120.0)),
            size: size(px(480.0), px(230.0)),
        })),
        kind: WindowKind::Normal,
        focus: true,
        show: true,
        is_movable: true,
        is_resizable: false,
        ..Default::default()
    };
    let entity_slot = Rc::new(RefCell::new(None));
    let slot = entity_slot.clone();
    if let Ok(handle) = cx.open_window(options, move |_, cx| {
        let entity = cx.new(|_| agentsmon_ui::CodexApprovalView);
        *slot.borrow_mut() = Some(entity.clone());
        entity
    }) {
        if let Some(entity) = entity_slot.borrow().clone() {
            cx.subscribe(&entity, move |_, _, cx| {
                let _ = cx.update_window(handle.into(), |_, window, _| window.remove_window());
            })
            .detach();
        }
    }
}

fn refresh(state: &AppState, cx: &mut App) {
    let mut snapshots = Vec::new();
    let mut provider_statuses = Vec::new();
    for provider in &state.0.providers {
        if !provider.is_enabled() {
            continue;
        }
        let usage = match provider.usage() {
            Ok(usage) => usage,
            Err(error) => {
                tracing::warn!(provider = provider.id(), %error, "provider usage discovery failed");
                None
            }
        };
        let mut status = ProviderStatus::new(provider.id(), usage.clone());
        status.display_name = provider.display_name().into();
        status.icon_png = provider.icon_png().map(<[u8]>::to_vec);
        status.accent_rgb = provider.accent_rgb();
        provider_statuses.push(status);
        match provider.discover() {
            Ok(mut provider_snapshots) => {
                for snapshot in &mut provider_snapshots {
                    snapshot.provider_usage = usage.clone();
                }
                snapshots.append(&mut provider_snapshots)
            }
            Err(error) => {
                tracing::warn!(provider = provider.id(), %error, "agent discovery failed")
            }
        }
        let prompt = state.0.connection_diagnostics.borrow_mut().observe(
            provider.id(),
            provider.display_name(),
            provider.diagnostic(),
        );
        if let Some(prompt) = prompt {
            state
                .0
                .notifications
                .show_diagnostic(provider.display_name(), prompt);
        }
    }
    snapshots.sort_by_key(|snapshot| snapshot.last_activity);
    state.0.worktrees.borrow_mut().resolve(&mut snapshots);
    let previous: HashMap<_, _> = state
        .0
        .store
        .borrow()
        .visible()
        .into_iter()
        .map(|snapshot| (snapshot.id, snapshot.state))
        .collect();
    let completed: Vec<_> = snapshots
        .iter()
        .filter(|snapshot| {
            // Awaiting a monitor or a wakeup is not a completed turn yet.
            let working = |state: &agentsmon_core::AgentState| {
                matches!(
                    state,
                    agentsmon_core::AgentState::Active | agentsmon_core::AgentState::Awaiting
                )
            };
            previous.get(&snapshot.id).is_some_and(working) && !working(&snapshot.state)
        })
        .map(|snapshot| snapshot.id.clone())
        .collect();
    let active_ids: Vec<_> = snapshots
        .iter()
        .filter(|snapshot| snapshot.state == agentsmon_core::AgentState::Active)
        .map(|snapshot| snapshot.id.clone())
        .collect();
    state.0.store.borrow_mut().apply_snapshot(snapshots);
    *state.0.provider_statuses.borrow_mut() = provider_statuses;
    for id in active_ids {
        state.0.unread.borrow_mut().remove(&id);
        state.0.notifications.close(&id);
    }
    for id in completed {
        // Read back from the store so the notification carries the user's custom name.
        let snapshot = state.0.store.borrow().get(&id).cloned();
        if let Some(snapshot) = snapshot {
            state.0.unread.borrow_mut().insert(id);
            state.0.notifications.show(&snapshot);
        }
    }
    broadcast(state, cx);
}

fn spawn_refresh_loop(cx: &mut App, state: AppState, poll_interval: Duration) {
    let mut last_refresh = Instant::now();
    let mut refresh_pending = false;
    cx.spawn(async move |cx: &mut AsyncApp| loop {
        while let Ok(command) = state.0.local_source.commands.try_recv() {
            if Instant::now() < command.deadline {
                let result = cx.update(|cx| focus_agent_result(&command.id, &state, cx));
                let _ = command.reply.try_send(result);
            }
        }
        while let Some(click) = state.0.notifications.try_clicked() {
            cx.update(|cx| match click {
                notification::NotificationClick::Agent(id) => focus_agent(&id, &state, cx),
                notification::NotificationClick::Diagnostic { title, prompt } => {
                    open_connection_report(cx, &state, title, prompt)
                }
            });
        }
        let should_close_details = state.0.details_window.borrow().is_some()
            && !pointer_is_over_native_panel()
            && !pointer_is_over_menu();
        if should_close_details {
            cx.update(|cx| close_details(&state, cx));
        }
        let menu_expired = state
            .0
            .group_menu
            .borrow_mut()
            .as_mut()
            .is_some_and(|menu| {
                if menu.pill_hovered || pointer_is_over_menu() {
                    menu.outside_since = None;
                    false
                } else {
                    menu.outside_since
                        .get_or_insert_with(Instant::now)
                        .elapsed()
                        >= MENU_GRACE
                }
            });
        if menu_expired {
            cx.update(|cx| close_group_menu(&state, cx));
        }
        let changed = state
            .0
            .watchers
            .borrow()
            .iter()
            // Always drain one signal from every bounded watcher. `any`
            // short-circuits and leaves the other provider queued, which
            // needlessly schedules another full refresh on the next tick.
            .filter(|watcher| watcher.changes.try_recv().is_ok())
            .count()
            > 0;
        refresh_pending |= changed;
        // Drain every provider's worker signal, even if a filesystem event has
        // already requested a refresh. No provider-specific UI conditionals.
        refresh_pending |= state
            .0
            .providers
            .iter()
            .filter(|provider| provider.take_changed())
            .count()
            > 0;
        let elapsed = last_refresh.elapsed();
        let periodic_refresh_due = elapsed >= poll_interval;
        let changed_refresh_due = refresh_pending && elapsed >= CHANGE_REFRESH_INTERVAL;
        if periodic_refresh_due || changed_refresh_due {
            cx.update(|cx| {
                refresh(&state, cx);
                reconcile_displays(cx, &state, false);
            });
            refresh_pending = false;
            last_refresh = Instant::now();
        }
        cx.background_executor()
            .timer(Duration::from_millis(75))
            .await;
    })
    .detach();
}

/// Build the store with the sessions hidden during the previous run. Dead
/// sessions drop out on the first refresh, which then rewrites the file.
fn restored_store() -> (AgentStore, Vec<agentsmon_core::HiddenSession>) {
    let hidden = match hidden_sessions::path() {
        Ok(path) => hidden_sessions::load_from(&path),
        Err(error) => {
            tracing::warn!(%error, "hidden sessions are not persisted");
            Vec::new()
        }
    };
    let mut store = AgentStore::default();
    store.restore_hidden(hidden.iter().cloned());
    (store, hidden)
}

/// Write the hidden-session list when it changed since the last write.
fn persist_hidden(state: &AppState) {
    let hidden = state.0.store.borrow().hidden_sessions();
    if *state.0.persisted_hidden.borrow() == hidden {
        return;
    }
    let result = hidden_sessions::path().and_then(|path| hidden_sessions::save_to(&path, &hidden));
    match result {
        Ok(()) => *state.0.persisted_hidden.borrow_mut() = hidden,
        Err(error) => tracing::warn!(%error, "cannot save hidden sessions"),
    }
}

fn broadcast(state: &AppState, cx: &mut App) {
    state.0.local_source.publish(state);
    persist_hidden(state);
    state
        .0
        .custom_names
        .borrow_mut()
        .save_if_changed(state.0.store.borrow().custom_names());
    let slots = state.0.store.borrow().visible_slots();
    let providers = state.0.provider_statuses.borrow().clone();
    let unread = state.0.unread.borrow().clone();
    for panel in state.0.panels.borrow().iter() {
        panel.view.update(cx, |view, cx| {
            view.set_data(slots.clone(), providers.clone(), unread.clone(), cx)
        });
        let width = natural_width(&slots, &providers, panel.max_width);
        let _ = cx.update_window(panel.handle, |_, window, _| {
            window.resize(size(px(width), px(BAR_HEIGHT)));
        });
    }
    refresh_group_menu(state, &providers, &unread, cx);
    let max_offset = maximum_x_offset(state, &slots);
    let mut offset = state.0.x_offset.borrow_mut();
    *offset = (*offset).clamp(0.0, max_offset);
    move_native_panels(*offset);
}

/// Keep an open group menu in step with the store. A change in membership
/// would change the menu's size, so the menu closes instead of resizing.
fn refresh_group_menu(
    state: &AppState,
    providers: &[ProviderStatus],
    unread: &HashSet<agentsmon_core::AgentId>,
    cx: &mut App,
) {
    let open = state
        .0
        .group_menu
        .borrow()
        .as_ref()
        .map(|menu| (menu.key.clone(), menu.member_count, menu.view.clone()));
    let Some((key, member_count, view)) = open else {
        return;
    };
    let members = state.0.store.borrow().group_members(&key);
    if members.len() != member_count {
        close_group_menu(state, cx);
        return;
    }
    let slots: Vec<Slot> = members
        .into_iter()
        .map(|member| Slot::Single(Box::new(member)))
        .collect();
    view.update(cx, |view, cx| {
        view.set_data(slots, providers.to_vec(), unread.clone(), cx)
    });
}

fn maximum_x_offset(state: &AppState, slots: &[Slot]) -> f32 {
    let providers = state.0.provider_statuses.borrow();
    state
        .0
        .panels
        .borrow()
        .iter()
        .map(|panel| {
            panel.display_bounds.size.width.as_f32()
                - natural_width(slots, &providers, panel.max_width)
        })
        .fold(f32::INFINITY, f32::min)
        .max(0.0)
}

fn handle_event(
    event: &MonitorEvent,
    state: &AppState,
    display_id: DisplayId,
    display_bounds: Bounds<Pixels>,
    cx: &mut App,
) {
    match event {
        MonitorEvent::Focus(id) => {
            focus_agent(id, state, cx);
        }
        MonitorEvent::Hide(id) => {
            state.0.store.borrow_mut().hide(id);
            broadcast(state, cx);
        }
        MonitorEvent::HideGroup(key) => {
            close_group_menu(state, cx);
            state.0.store.borrow_mut().hide_group(key);
            broadcast(state, cx);
        }
        MonitorEvent::Reorder { dragged, before } => {
            state.0.store.borrow_mut().reorder(dragged, before.as_ref());
            broadcast(state, cx);
        }
        MonitorEvent::MoveBarBy(delta) => {
            close_details(state, cx);
            close_group_menu(state, cx);
            let slots = state.0.store.borrow().visible_slots();
            let max_offset = maximum_x_offset(state, &slots);
            let mut offset = state.0.x_offset.borrow_mut();
            *offset = (*offset + delta).clamp(0.0, max_offset);
            move_native_panels(*offset);
        }
        MonitorEvent::ShowGroup { key, anchor_x } => {
            open_group_menu(
                state,
                display_id,
                display_bounds,
                *anchor_x,
                key.clone(),
                cx,
            );
        }
        MonitorEvent::LeaveGroup => {
            if let Some(menu) = state.0.group_menu.borrow_mut().as_mut() {
                menu.pill_hovered = false;
            }
        }
        MonitorEvent::ShowDetails { agent, anchor_x } => {
            close_group_menu(state, cx);
            open_details(
                state,
                display_id,
                display_bounds,
                *anchor_x,
                agent.as_ref().clone(),
                cx,
            );
        }
        MonitorEvent::ShowProviderDetails { provider, anchor_x } => {
            close_group_menu(state, cx);
            open_provider_details(
                state,
                display_id,
                display_bounds,
                *anchor_x,
                provider.as_ref().clone(),
                cx,
            );
        }
        MonitorEvent::HideDetails => close_details(state, cx),
        MonitorEvent::Rename { target, anchor_x } => {
            close_group_menu(state, cx);
            open_rename(
                state,
                display_id,
                display_bounds,
                *anchor_x,
                target.as_ref().clone(),
                cx,
            );
        }
        MonitorEvent::ProvidersMenu { anchor_x } => {
            close_group_menu(state, cx);
            let actions = openclaw_integration::menu_actions(&state.0.openclaw);
            open_actions_menu(state, display_id, display_bounds, *anchor_x, actions, cx);
        }
    }
}

/// Events from the menu listing a group's members. Details open beside the
/// menu rather than above the bar, where they would cover the other members.
fn handle_menu_event(event: &MonitorEvent, state: &AppState, cx: &mut App) {
    match event {
        MonitorEvent::Focus(id) => {
            close_details(state, cx);
            close_group_menu(state, cx);
            focus_agent(id, state, cx);
        }
        MonitorEvent::Hide(id) => {
            state.0.store.borrow_mut().hide(id);
            broadcast(state, cx);
        }
        MonitorEvent::ShowDetails { agent, .. } => {
            open_details_beside_menu(state, agent.as_ref().clone(), cx);
        }
        MonitorEvent::HideDetails => close_details(state, cx),
        MonitorEvent::Rename { target, anchor_x } => {
            let placement = state.0.group_menu.borrow().as_ref().map(|menu| {
                (
                    menu.display_id,
                    menu.display_bounds,
                    menu.bounds.origin.x.as_f32() + anchor_x
                        - menu.display_bounds.origin.x.as_f32()
                        - *state.0.x_offset.borrow(),
                )
            });
            close_group_menu(state, cx);
            if let Some((display_id, display_bounds, anchor_x)) = placement {
                open_rename(
                    state,
                    display_id,
                    display_bounds,
                    anchor_x,
                    target.as_ref().clone(),
                    cx,
                );
            }
        }
        MonitorEvent::HideGroup(_)
        | MonitorEvent::Reorder { .. }
        | MonitorEvent::MoveBarBy(_)
        | MonitorEvent::ShowProviderDetails { .. }
        | MonitorEvent::ShowGroup { .. }
        | MonitorEvent::ProvidersMenu { .. }
        | MonitorEvent::LeaveGroup => {}
    }
}

fn close_group_menu(state: &AppState, cx: &mut App) {
    if let Some(menu) = state.0.group_menu.borrow_mut().take() {
        let _ = cx.update_window(menu.handle, |_, window, _| window.remove_window());
    }
}

fn open_group_menu(
    state: &AppState,
    display_id: DisplayId,
    display_bounds: Bounds<Pixels>,
    anchor_x: f32,
    key: GroupKey,
    cx: &mut App,
) {
    if let Some(menu) = state.0.group_menu.borrow_mut().as_mut() {
        if menu.key == key {
            menu.pill_hovered = true;
            menu.outside_since = None;
            return;
        }
    }
    close_group_menu(state, cx);
    close_details(state, cx);
    if state.0.rename_window.borrow().is_some() {
        return;
    }
    let members = state.0.store.borrow().group_members(&key);
    if members.len() < 2 {
        return;
    }
    let (width, height) = menu_size(&members);
    let panel_x = display_bounds.origin.x.as_f32() + *state.0.x_offset.borrow();
    let min_x = display_bounds.origin.x.as_f32() + 6.0;
    let max_x = (display_bounds.origin.x + display_bounds.size.width).as_f32() - width - 6.0;
    let x = (panel_x + anchor_x - width / 2.0).clamp(min_x, max_x.max(min_x));
    // The menu touches the bar so the pointer can reach it without leaving both.
    let y = (display_bounds.origin.y + display_bounds.size.height).as_f32() - BAR_HEIGHT - height;
    let bounds = Bounds {
        origin: point(px(x), px(y)),
        size: size(px(width), px(height)),
    };
    let options = WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        focus: false,
        show: true,
        display_id: Some(display_id),
        window_background: WindowBackgroundAppearance::Transparent,
        app_id: Some("com.oliv.AgentsMon.menu".into()),
        ..Default::default()
    };
    let member_count = members.len();
    let slots: Vec<Slot> = members
        .into_iter()
        .map(|member| Slot::Single(Box::new(member)))
        .collect();
    let providers = state.0.provider_statuses.borrow().clone();
    let unread = state.0.unread.borrow().clone();
    let view_slot: Rc<RefCell<Option<Entity<MonitorView>>>> = Rc::new(RefCell::new(None));
    let slot = view_slot.clone();
    match cx.open_window(options, move |window, cx| {
        window.set_window_title(MENU_WINDOW_TITLE);
        let view = cx.new(|_| MonitorView::new(slots, providers, unread, Layout::Menu));
        *slot.borrow_mut() = Some(view.clone());
        view
    }) {
        Ok(handle) => {
            let view = view_slot
                .borrow()
                .clone()
                .expect("window builder sets view");
            let event_state = state.clone();
            let subscription = cx.subscribe(&view, move |_, event, cx| {
                handle_menu_event(event, &event_state, cx);
            });
            *state.0.group_menu.borrow_mut() = Some(GroupMenu {
                handle: handle.into(),
                view,
                key,
                member_count,
                display_id,
                display_bounds,
                bounds,
                pill_hovered: true,
                outside_since: None,
                _subscription: subscription,
            });
            configure_native_menu();
        }
        Err(error) => tracing::warn!(%error, "cannot open group menu"),
    }
}

fn close_rename(state: &AppState, cx: &mut App) {
    if let Some(handle) = state.0.rename_window.borrow_mut().take() {
        let _ = cx.update_window(handle, |_, window, _| window.remove_window());
    }
}

fn open_rename(
    state: &AppState,
    display_id: DisplayId,
    display_bounds: Bounds<Pixels>,
    anchor_x: f32,
    target: RenameTarget,
    cx: &mut App,
) {
    close_details(state, cx);
    close_rename(state, cx);
    let panel_x = display_bounds.origin.x.as_f32() + *state.0.x_offset.borrow();
    let min_x = display_bounds.origin.x.as_f32() + 6.0;
    let max_x =
        (display_bounds.origin.x + display_bounds.size.width).as_f32() - DETAILS_WIDTH - 6.0;
    let x = (panel_x + anchor_x - DETAILS_WIDTH / 2.0).clamp(min_x, max_x.max(min_x));
    let height = rename_height();
    let y = (display_bounds.origin.y + display_bounds.size.height).as_f32()
        - BAR_HEIGHT
        - DETAILS_GAP
        - height;
    // A pop-up panel takes key events without activating AgentsMon, so the
    // terminal keeps its place once the prompt closes. The title keeps the
    // native panel/details passes from restyling or click-proofing it.
    let options = WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(x), px(y)),
            size: size(px(DETAILS_WIDTH), px(height)),
        })),
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        focus: true,
        show: true,
        display_id: Some(display_id),
        window_background: WindowBackgroundAppearance::Transparent,
        app_id: Some("com.oliv.AgentsMon.rename".into()),
        ..Default::default()
    };
    let provider_names: Vec<String> = match &target {
        RenameTarget::Session(agent) => vec![provider_display_name(state, &agent.id.provider)],
        RenameTarget::Group(group) => group
            .members
            .iter()
            .map(|member| provider_display_name(state, &member.id.provider))
            .collect(),
    };
    let entity_slot = Rc::new(RefCell::new(None));
    let slot = entity_slot.clone();
    match cx.open_window(options, move |window, cx| {
        window.set_window_title(RENAME_WINDOW_TITLE);
        let entity = cx.new(|cx| RenameView::new(target, provider_names, window, cx));
        *slot.borrow_mut() = Some(entity.clone());
        entity
    }) {
        Ok(handle) => {
            *state.0.rename_window.borrow_mut() = Some(handle.into());
            configure_native_popup(RENAME_WINDOW_TITLE);
            let Some(entity) = entity_slot.borrow().clone() else {
                return;
            };
            let event_state = state.clone();
            cx.subscribe(&entity, move |_, event, cx| {
                match event {
                    RenameEvent::Submit { target, name } => {
                        match target {
                            RenameKey::Session(id) => {
                                event_state.0.store.borrow_mut().rename(id, name)
                            }
                            RenameKey::Group(key) => {
                                event_state.0.store.borrow_mut().rename_group(key, name)
                            }
                        }
                        broadcast(&event_state, cx);
                    }
                    RenameEvent::Cancel => {}
                }
                close_rename(&event_state, cx);
            })
            .detach();
        }
        Err(error) => tracing::warn!(%error, "cannot open rename prompt"),
    }
}

fn close_actions_menu(state: &AppState, cx: &mut App) {
    if let Some(handle) = state.0.actions_menu.borrow_mut().take() {
        let _ = cx.update_window(handle, |_, window, _| window.remove_window());
    }
}

/// The provider actions menu, opened above the provider icons. Nothing opens
/// when no provider has an action to offer.
fn open_actions_menu(
    state: &AppState,
    display_id: DisplayId,
    display_bounds: Bounds<Pixels>,
    anchor_x: f32,
    actions: Vec<MenuAction>,
    cx: &mut App,
) {
    close_details(state, cx);
    close_rename(state, cx);
    close_actions_menu(state, cx);
    if actions.is_empty() {
        return;
    }
    let panel_x = display_bounds.origin.x.as_f32() + *state.0.x_offset.borrow();
    let min_x = display_bounds.origin.x.as_f32() + 6.0;
    let max_x =
        (display_bounds.origin.x + display_bounds.size.width).as_f32() - ACTIONS_MENU_WIDTH - 6.0;
    let x = (panel_x + anchor_x - 12.0).clamp(min_x, max_x.max(min_x));
    let height = action_menu_height(actions.len());
    let y = (display_bounds.origin.y + display_bounds.size.height).as_f32()
        - BAR_HEIGHT
        - DETAILS_GAP
        - height;
    let options = WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(x), px(y)),
            size: size(px(ACTIONS_MENU_WIDTH), px(height)),
        })),
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        focus: true,
        show: true,
        display_id: Some(display_id),
        window_background: WindowBackgroundAppearance::Transparent,
        app_id: Some("com.oliv.AgentsMon.actions".into()),
        ..Default::default()
    };
    let entity_slot = Rc::new(RefCell::new(None));
    let slot = entity_slot.clone();
    match cx.open_window(options, move |window, cx| {
        window.set_window_title(ACTIONS_WINDOW_TITLE);
        let entity = cx.new(|cx| ActionMenuView::new(actions, window, cx));
        *slot.borrow_mut() = Some(entity.clone());
        entity
    }) {
        Ok(handle) => {
            *state.0.actions_menu.borrow_mut() = Some(handle.into());
            configure_native_popup(ACTIONS_WINDOW_TITLE);
            let Some(entity) = entity_slot.borrow().clone() else {
                return;
            };
            let event_state = state.clone();
            cx.subscribe(&entity, move |_, event, cx| {
                close_actions_menu(&event_state, cx);
                match event {
                    ActionMenuEvent::Select(id) if id == openclaw_integration::CONNECT_ACTION => {
                        open_openclaw_connect(&event_state, cx)
                    }
                    ActionMenuEvent::Select(_) | ActionMenuEvent::Dismiss => {}
                }
            })
            .detach();
        }
        Err(error) => tracing::warn!(%error, "cannot open provider actions menu"),
    }
}

fn open_openclaw_connect(state: &AppState, cx: &mut App) {
    if let Some(handle) = state.0.openclaw_window.borrow_mut().take() {
        let _ = cx.update_window(handle, |_, window, _| window.remove_window());
    }
    let refresh_state = state.clone();
    let handle = openclaw_integration::open_connect_window(
        cx,
        state.0.openclaw.clone(),
        Rc::new(move |cx| {
            refresh(&refresh_state, cx);
            reconcile_displays(cx, &refresh_state, false);
        }),
    );
    *state.0.openclaw_window.borrow_mut() = handle;
}

fn provider_display_name(state: &AppState, provider_id: &str) -> String {
    state
        .0
        .providers
        .iter()
        .find(|provider| provider.id() == provider_id)
        .map(|provider| provider.display_name().to_string())
        .unwrap_or_else(|| provider_id.to_owned())
}

/// A click on a card or a notification. The click itself acknowledges the
/// session, so its unread mark goes even when the host cannot be focused
/// (e.g. a Warp build without session URLs). IPC consumers keep the stricter `focus_agent_result`.
fn focus_agent(id: &agentsmon_core::AgentId, state: &AppState, cx: &mut App) {
    if let Err(code) = focus_agent_result(id, state, cx) {
        tracing::warn!(code, session = %id.session_id, "focus failed");
        mark_read(id, state, cx);
    }
}

fn mark_read(id: &agentsmon_core::AgentId, state: &AppState, cx: &mut App) {
    state.0.unread.borrow_mut().remove(id);
    state.0.notifications.close(id);
    broadcast(state, cx);
}

fn focus_agent_result(
    id: &agentsmon_core::AgentId,
    state: &AppState,
    cx: &mut App,
) -> Result<(), &'static str> {
    let target = state
        .0
        .store
        .borrow()
        .visible()
        .into_iter()
        .find(|snapshot| &snapshot.id == id)
        .map(|snapshot| snapshot.focus_target)
        .ok_or("stale_session")?;
    let provider = state
        .0
        .providers
        .iter()
        .find(|provider| provider.id() == id.provider)
        .ok_or("provider_unavailable")?;
    provider.focus(&target).map_err(|error| {
        tracing::warn!(%error, session = %id.session_id, provider = %id.provider, "focus failed");
        "focus_failed"
    })?;
    mark_read(id, state, cx);
    Ok(())
}

fn close_details(state: &AppState, cx: &mut App) {
    if let Some(handle) = state.0.details_window.borrow_mut().take() {
        let _ = cx.update_window(handle, |_, window, _| window.remove_window());
    }
}

fn open_details(
    state: &AppState,
    display_id: DisplayId,
    display_bounds: Bounds<Pixels>,
    anchor_x: f32,
    agent: agentsmon_core::AgentSnapshot,
    cx: &mut App,
) {
    let panel_x = display_bounds.origin.x.as_f32() + *state.0.x_offset.borrow();
    let min_x = display_bounds.origin.x.as_f32() + 6.0;
    let max_x =
        (display_bounds.origin.x + display_bounds.size.width).as_f32() - DETAILS_WIDTH - 6.0;
    let x = (panel_x + anchor_x - DETAILS_WIDTH / 2.0).clamp(min_x, max_x.max(min_x));
    open_details_at(state, display_id, display_bounds, x, agent, cx);
}

/// Details of a menu entry: to the right of the menu (left when the screen
/// edge is too close), bottom-aligned with the usual popups.
fn open_details_beside_menu(state: &AppState, agent: agentsmon_core::AgentSnapshot, cx: &mut App) {
    let placement = state
        .0
        .group_menu
        .borrow()
        .as_ref()
        .map(|menu| (menu.display_id, menu.display_bounds, menu.bounds));
    let Some((display_id, display_bounds, menu_bounds)) = placement else {
        return;
    };
    let min_x = display_bounds.origin.x.as_f32() + 6.0;
    let max_x =
        (display_bounds.origin.x + display_bounds.size.width).as_f32() - DETAILS_WIDTH - 6.0;
    let right = (menu_bounds.origin.x + menu_bounds.size.width).as_f32() + DETAILS_GAP;
    let x = if right <= max_x {
        right
    } else {
        (menu_bounds.origin.x.as_f32() - DETAILS_GAP - DETAILS_WIDTH).max(min_x)
    };
    open_details_at(state, display_id, display_bounds, x, agent, cx);
}

fn open_details_at(
    state: &AppState,
    display_id: DisplayId,
    display_bounds: Bounds<Pixels>,
    x: f32,
    agent: agentsmon_core::AgentSnapshot,
    cx: &mut App,
) {
    if state.0.rename_window.borrow().is_some() {
        return;
    }
    close_details(state, cx);
    let height = agent_details_height(&agent);
    let y = (display_bounds.origin.y + display_bounds.size.height).as_f32()
        - BAR_HEIGHT
        - DETAILS_GAP
        - height;
    let options = WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(x), px(y)),
            size: size(px(DETAILS_WIDTH), px(height)),
        })),
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        focus: false,
        show: true,
        display_id: Some(display_id),
        window_background: WindowBackgroundAppearance::Transparent,
        app_id: Some("com.oliv.AgentsMon.details".into()),
        ..Default::default()
    };
    let provider_name = provider_display_name(state, &agent.id.provider);
    match cx.open_window(options, move |_, cx| {
        cx.new(|_| AgentDetailsView::new(agent, provider_name))
    }) {
        Ok(handle) => {
            *state.0.details_window.borrow_mut() = Some(handle.into());
            configure_native_details();
        }
        Err(error) => tracing::warn!(%error, "cannot open agent details"),
    }
}

fn open_provider_details(
    state: &AppState,
    display_id: DisplayId,
    display_bounds: Bounds<Pixels>,
    anchor_x: f32,
    provider: ProviderStatus,
    cx: &mut App,
) {
    if state.0.rename_window.borrow().is_some() {
        return;
    }
    close_details(state, cx);
    let panel_x = display_bounds.origin.x.as_f32() + *state.0.x_offset.borrow();
    let min_x = display_bounds.origin.x.as_f32() + 6.0;
    let max_x =
        (display_bounds.origin.x + display_bounds.size.width).as_f32() - DETAILS_WIDTH - 6.0;
    let x = (panel_x + anchor_x - DETAILS_WIDTH / 2.0).clamp(min_x, max_x.max(min_x));
    let height = provider_details_height(&provider);
    let y = (display_bounds.origin.y + display_bounds.size.height).as_f32()
        - BAR_HEIGHT
        - DETAILS_GAP
        - height;
    let options = WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(x), px(y)),
            size: size(px(DETAILS_WIDTH), px(height)),
        })),
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        focus: false,
        show: true,
        display_id: Some(display_id),
        window_background: WindowBackgroundAppearance::Transparent,
        app_id: Some("com.oliv.AgentsMon.provider-details".into()),
        ..Default::default()
    };
    match cx.open_window(options, move |_, cx| {
        cx.new(|_| ProviderDetailsView::new(provider))
    }) {
        Ok(handle) => {
            *state.0.details_window.borrow_mut() = Some(handle.into());
            configure_native_details();
        }
        Err(error) => tracing::warn!(%error, "cannot open provider details"),
    }
}

fn reconcile_displays(cx: &mut App, state: &AppState, force: bool) {
    let displays = cx.displays();
    let current: Vec<_> = state
        .0
        .panels
        .borrow()
        .iter()
        .map(|panel| (panel.display_id, panel.display_bounds))
        .collect();
    let next: Vec<_> = displays
        .iter()
        .map(|display| (display.id(), display.bounds()))
        .collect();
    if !force && current == next {
        return;
    }
    close_details(state, cx);
    close_rename(state, cx);
    close_group_menu(state, cx);

    for panel in state.0.panels.borrow_mut().drain(..) {
        let _ = cx.update_window(panel.handle, |_, window, _| window.remove_window());
    }

    let slots = state.0.store.borrow().visible_slots();
    let providers = state.0.provider_statuses.borrow().clone();
    let unread = state.0.unread.borrow().clone();
    for display in displays {
        let bounds = display.bounds();
        let max_width = (bounds.size.width.as_f32() - SCREEN_INSET * 2.0).max(76.0);
        let width = natural_width(&slots, &providers, max_width);
        let x_offset = (*state.0.x_offset.borrow()).clamp(0.0, (max_width - width).max(0.0));
        let window_bounds = Bounds {
            origin: point(
                bounds.origin.x + px(SCREEN_INSET + x_offset),
                bounds.origin.y + bounds.size.height - px(BAR_HEIGHT + SCREEN_INSET),
            ),
            size: size(px(width), px(BAR_HEIGHT)),
        };
        let options = WindowOptions {
            titlebar: None,
            window_bounds: Some(WindowBounds::Windowed(window_bounds)),
            kind: WindowKind::PopUp,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            focus: false,
            show: true,
            display_id: Some(display.id()),
            window_background: WindowBackgroundAppearance::Transparent,
            app_id: Some("com.oliv.AgentsMon".into()),
            ..Default::default()
        };

        let view_slot: Rc<RefCell<Option<Entity<MonitorView>>>> = Rc::new(RefCell::new(None));
        let slot = view_slot.clone();
        let initial = slots.clone();
        let initial_providers = providers.clone();
        let initial_unread = unread.clone();
        match cx.open_window(options, move |_, cx| {
            let view = cx
                .new(|_| MonitorView::new(initial, initial_providers, initial_unread, Layout::Bar));
            *slot.borrow_mut() = Some(view.clone());
            view
        }) {
            Ok(handle) => {
                let view = view_slot
                    .borrow()
                    .clone()
                    .expect("window builder sets view");
                let event_state = state.clone();
                let event_display_id = display.id();
                let event_display_bounds = bounds;
                let subscription = cx.subscribe(&view, move |_, event, cx| {
                    handle_event(
                        event,
                        &event_state,
                        event_display_id,
                        event_display_bounds,
                        cx,
                    );
                });
                state.0.panels.borrow_mut().push(Panel {
                    display_id: display.id(),
                    display_bounds: bounds,
                    max_width,
                    handle: handle.into(),
                    view,
                    _subscription: subscription,
                });
            }
            Err(error) => tracing::error!(%error, "cannot open monitor bar"),
        }
    }
    configure_native_panels(*state.0.x_offset.borrow());
}

fn open_connection_report(cx: &mut App, state: &AppState, title: String, report: String) {
    if let Some(handle) = state.0.connection_window.borrow_mut().take() {
        let _ = cx.update_window(handle, |_, window, _| window.remove_window());
    }
    let options = WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(title.clone().into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(100.0), px(100.0)),
            size: size(px(760.0), px(580.0)),
        })),
        kind: WindowKind::Normal,
        focus: true,
        show: true,
        is_movable: true,
        is_resizable: true,
        ..Default::default()
    };
    match cx.open_window(options, move |_, cx| {
        cx.new(|_| agentsmon_ui::DiagnosticReportView::new(title, report))
    }) {
        Ok(handle) => {
            *state.0.connection_window.borrow_mut() = Some(handle.into());
            cx.activate(true);
        }
        Err(error) => {
            tracing::warn!(%error, "cannot open connection diagnostic report; see persistent diagnostic log")
        }
    }
}

fn open_diagnostics(cx: &mut App, state: &AppState) {
    let status = integration::inspect();
    let state_for_event = state.clone();
    let options = WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("Intégration Claude".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(100.0), px(100.0)),
            size: size(px(520.0), px(260.0)),
        })),
        kind: WindowKind::Normal,
        focus: true,
        show: true,
        is_movable: true,
        is_resizable: false,
        ..Default::default()
    };
    let entity_slot = Rc::new(RefCell::new(None));
    let slot = entity_slot.clone();
    if let Ok(handle) = cx.open_window(options, move |_, cx| {
        let entity = cx.new(|_| agentsmon_ui::DiagnosticsView::new(status));
        *slot.borrow_mut() = Some(entity.clone());
        entity
    }) {
        if let Some(entity) = entity_slot.borrow().clone() {
            let diagnostics = entity.clone();
            cx.subscribe(&entity, move |_, event, cx| {
                let result = match event {
                    agentsmon_ui::DiagnosticsEvent::Install => {
                        let result = integration::install();
                        if result.is_ok() {
                            refresh(&state_for_event, cx);
                        }
                        result
                    }
                    agentsmon_ui::DiagnosticsEvent::Uninstall => integration::uninstall(),
                };
                if let Err(error) = result {
                    tracing::error!(%error, "Claude integration action failed");
                    diagnostics.update(cx, |view, cx| {
                        view.set_message(format!("Échec : {error:#}"), cx);
                    });
                    return;
                }
                let _ = cx.update_window(handle.into(), |_, window, _| window.remove_window());
            })
            .detach();
        }
    }
}

#[cfg(target_os = "macos")]
fn configure_native_panels(x_offset: f32) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSFloatingWindowLevel, NSWindowCollectionBehavior, NSWindowStyleMask,
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    for index in 0..app.windows().count() {
        let window = app.windows().objectAtIndex(index);
        if window.title().is_empty() && is_bar_height(window.frame().size.height) {
            // Keep the bars above normal application windows, but below system UI such as
            // an auto-hidden Dock. GPUI popups otherwise use NSPopUpWindowLevel, which is
            // higher than the Dock and would cover it while it animates into view.
            window.setLevel(NSFloatingWindowLevel);
            // GPUI creates titleless panels with a titled/full-content style mask. AppKit
            // consequently clips all four native window corners. Make the panel genuinely
            // borderless; the two desired top radii are drawn explicitly by MonitorView.
            window.setStyleMask(NSWindowStyleMask::NonactivatingPanel);
            window.setHasShadow(false);
            window.setCollectionBehavior(
                window.collectionBehavior()
                    | NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
                    | NSWindowCollectionBehavior::IgnoresCycle,
            );
        }
    }
    // GPUI converts the requested top-left Y coordinate to AppKit's bottom-left
    // coordinate system without subtracting the window height. Pinning the native
    // frame here avoids leaving a gap exactly one panel-height above the screen edge.
    move_native_panels(x_offset);
}

#[cfg(target_os = "macos")]
fn move_native_panels(x_offset: f32) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    for index in 0..app.windows().count() {
        let window = app.windows().objectAtIndex(index);
        if !window.title().is_empty() || !is_bar_height(window.frame().size.height) {
            continue;
        }
        let Some(screen) = window.screen() else {
            continue;
        };
        let visible = screen.frame();
        let frame = window.frame();
        let available = (visible.size.width - frame.size.width).max(0.0);
        let mut origin = frame.origin;
        origin.x = visible.origin.x + f64::from(x_offset).clamp(0.0, available);
        origin.y = visible.origin.y;
        window.setFrameOrigin(origin);
    }
}

#[cfg(target_os = "macos")]
fn configure_native_details() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSFloatingWindowLevel, NSWindowCollectionBehavior, NSWindowStyleMask,
    };

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    for index in 0..app.windows().count() {
        let window = app.windows().objectAtIndex(index);
        let frame = window.frame();
        if !window.title().is_empty() || is_bar_height(frame.size.height) {
            continue;
        }
        let Some(screen) = window.screen() else {
            continue;
        };
        window.setLevel(NSFloatingWindowLevel);
        window.setStyleMask(NSWindowStyleMask::NonactivatingPanel);
        window.setHasShadow(true);
        window.setIgnoresMouseEvents(true);
        window.setCollectionBehavior(
            window.collectionBehavior()
                | NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        let mut origin = frame.origin;
        origin.y = screen.frame().origin.y + f64::from(BAR_HEIGHT + DETAILS_GAP);
        window.setFrameOrigin(origin);
    }
}

/// Floating, key-taking pop-up (rename prompt, actions menu) found by title.
#[cfg(target_os = "macos")]
fn configure_native_popup(title: &str) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSFloatingWindowLevel, NSWindowCollectionBehavior};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    for index in 0..app.windows().count() {
        let window = app.windows().objectAtIndex(index);
        if window.title().to_string() != title {
            continue;
        }
        let Some(screen) = window.screen() else {
            continue;
        };
        window.setLevel(NSFloatingWindowLevel);
        window.setHasShadow(true);
        window.setCollectionBehavior(
            window.collectionBehavior()
                | NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        let mut origin = window.frame().origin;
        origin.y = screen.frame().origin.y + f64::from(BAR_HEIGHT + DETAILS_GAP);
        window.setFrameOrigin(origin);
        window.makeKeyAndOrderFront(None);
    }
}

#[cfg(not(target_os = "macos"))]
fn configure_native_popup(_title: &str) {}

/// The group menu behaves like the bars: floating, on every space, and a
/// non-activating panel so clicking a member never brings AgentsMon forward.
#[cfg(target_os = "macos")]
fn configure_native_menu() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSFloatingWindowLevel, NSWindowCollectionBehavior, NSWindowStyleMask,
    };

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    for index in 0..app.windows().count() {
        let window = app.windows().objectAtIndex(index);
        if window.title().to_string() != MENU_WINDOW_TITLE {
            continue;
        }
        let Some(screen) = window.screen() else {
            continue;
        };
        window.setLevel(NSFloatingWindowLevel);
        window.setStyleMask(NSWindowStyleMask::NonactivatingPanel);
        window.setHasShadow(true);
        window.setCollectionBehavior(
            window.collectionBehavior()
                | NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        let mut origin = window.frame().origin;
        origin.y = screen.frame().origin.y + f64::from(BAR_HEIGHT);
        window.setFrameOrigin(origin);
    }
}

#[cfg(target_os = "macos")]
fn pointer_is_over_menu() -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSEvent};

    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let pointer = NSEvent::mouseLocation();
    let app = NSApplication::sharedApplication(mtm);
    for index in 0..app.windows().count() {
        let window = app.windows().objectAtIndex(index);
        if window.title().to_string() != MENU_WINDOW_TITLE {
            continue;
        }
        let frame = window.frame();
        if pointer.x >= frame.origin.x
            && pointer.x <= frame.origin.x + frame.size.width
            && pointer.y >= frame.origin.y
            && pointer.y <= frame.origin.y + frame.size.height
        {
            return true;
        }
    }
    false
}

#[cfg(target_os = "windows")]
fn window_title(hwnd: windows::Win32::Foundation::HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;
    let mut buffer = [0u16; 128];
    let length = unsafe { GetWindowTextW(hwnd, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..length.min(buffer.len())])
}

#[cfg(target_os = "windows")]
fn configure_native_menu() {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongPtrW, GetWindowThreadProcessId, SetWindowLongPtrW, SetWindowPos,
        GWL_EXSTYLE, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW,
    };

    unsafe extern "system" fn each(hwnd: HWND, raw: LPARAM) -> BOOL {
        let pid = raw.0 as u32;
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
        if owner == pid && window_title(hwnd) == MENU_WINDOW_TITLE {
            let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
            let style = style | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0;
            unsafe {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        BOOL(1)
    }

    let pid = unsafe { GetCurrentProcessId() };
    let _ = unsafe { EnumWindows(Some(each), LPARAM(pid as isize)) };
}

#[cfg(target_os = "windows")]
fn pointer_is_over_menu() -> bool {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetCursorPos, GetWindowRect, GetWindowThreadProcessId,
    };

    #[repr(C)]
    struct Context {
        pid: u32,
        pointer: POINT,
        hovered: bool,
    }

    unsafe extern "system" fn each(hwnd: HWND, raw: LPARAM) -> BOOL {
        let context = unsafe { &mut *(raw.0 as *mut Context) };
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
        let mut rect = RECT::default();
        if owner == context.pid
            && window_title(hwnd) == MENU_WINDOW_TITLE
            && unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok()
            && context.pointer.x >= rect.left
            && context.pointer.x <= rect.right
            && context.pointer.y >= rect.top
            && context.pointer.y <= rect.bottom
        {
            context.hovered = true;
            return BOOL(0);
        }
        BOOL(1)
    }

    let mut pointer = POINT::default();
    if unsafe { GetCursorPos(&mut pointer) }.is_err() {
        return false;
    }
    let mut context = Context {
        pid: unsafe { GetCurrentProcessId() },
        pointer,
        hovered: false,
    };
    let _ = unsafe { EnumWindows(Some(each), LPARAM((&mut context as *mut Context) as isize)) };
    context.hovered
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn configure_native_menu() {}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn pointer_is_over_menu() -> bool {
    true
}

#[cfg(target_os = "windows")]
fn configure_native_details() {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
        GetWindowThreadProcessId, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TRANSPARENT,
    };

    unsafe extern "system" fn each(hwnd: HWND, raw: LPARAM) -> BOOL {
        let pid = raw.0 as u32;
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
        let mut rect = RECT::default();
        let is_details = unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok()
            && !is_bar_height(f64::from(rect.bottom - rect.top));
        if owner == pid && unsafe { GetWindowTextLengthW(hwnd) } == 0 && is_details {
            let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
            let style = style | WS_EX_TRANSPARENT.0 | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0;
            unsafe {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        BOOL(1)
    }

    let pid = unsafe { GetCurrentProcessId() };
    let _ = unsafe { EnumWindows(Some(each), LPARAM(pid as isize)) };
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn configure_native_details() {}

fn is_bar_height(height: f64) -> bool {
    (height - f64::from(BAR_HEIGHT)).abs() < 1.0
}

#[cfg(target_os = "macos")]
fn pointer_is_over_native_panel() -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSEvent};

    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let pointer = NSEvent::mouseLocation();
    let app = NSApplication::sharedApplication(mtm);
    for index in 0..app.windows().count() {
        let window = app.windows().objectAtIndex(index);
        let frame = window.frame();
        if !window.title().is_empty() || !is_bar_height(frame.size.height) {
            continue;
        }
        if pointer.x >= frame.origin.x
            && pointer.x <= frame.origin.x + frame.size.width
            && pointer.y >= frame.origin.y
            && pointer.y <= frame.origin.y + frame.size.height
        {
            return true;
        }
    }
    false
}

#[cfg(target_os = "windows")]
fn pointer_is_over_native_panel() -> bool {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetCursorPos, GetWindowRect, GetWindowTextLengthW, GetWindowThreadProcessId,
    };

    #[repr(C)]
    struct Context {
        pid: u32,
        pointer: POINT,
        hovered: bool,
    }

    unsafe extern "system" fn each(hwnd: HWND, raw: LPARAM) -> BOOL {
        let context = unsafe { &mut *(raw.0 as *mut Context) };
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
        let mut rect = RECT::default();
        if owner == context.pid
            && unsafe { GetWindowTextLengthW(hwnd) } == 0
            && unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok()
            && is_bar_height(f64::from(rect.bottom - rect.top))
            && context.pointer.x >= rect.left
            && context.pointer.x <= rect.right
            && context.pointer.y >= rect.top
            && context.pointer.y <= rect.bottom
        {
            context.hovered = true;
            return BOOL(0);
        }
        BOOL(1)
    }

    let mut pointer = POINT::default();
    if unsafe { GetCursorPos(&mut pointer) }.is_err() {
        return false;
    }
    let mut context = Context {
        pid: unsafe { GetCurrentProcessId() },
        pointer,
        hovered: false,
    };
    let _ = unsafe { EnumWindows(Some(each), LPARAM((&mut context as *mut Context) as isize)) };
    context.hovered
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn pointer_is_over_native_panel() -> bool {
    true
}

#[cfg(not(target_os = "macos"))]
#[cfg(not(target_os = "windows"))]
fn configure_native_panels(_x_offset: f32) {}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn move_native_panels(_x_offset: f32) {}

#[cfg(target_os = "windows")]
fn configure_native_panels(_x_offset: f32) {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
        GetWindowThreadProcessId, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };
    #[repr(C)]
    struct Context {
        pid: u32,
        seen: usize,
    }
    unsafe extern "system" fn each(hwnd: HWND, raw: LPARAM) -> BOOL {
        let context = unsafe { &mut *(raw.0 as *mut Context) };
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
        let mut rect = RECT::default();
        let is_bar = unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok()
            && is_bar_height(f64::from(rect.bottom - rect.top));
        if owner == context.pid && unsafe { GetWindowTextLengthW(hwnd) } == 0 && is_bar {
            let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
            let style = if context.seen == 0 {
                (style | WS_EX_APPWINDOW.0) & !WS_EX_TOOLWINDOW.0
            } else {
                (style | WS_EX_TOOLWINDOW.0) & !WS_EX_APPWINDOW.0
            };
            unsafe {
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            context.seen += 1;
        }
        BOOL(1)
    }
    let mut context = Context {
        pid: unsafe { GetCurrentProcessId() },
        seen: 0,
    };
    let _ = unsafe { EnumWindows(Some(each), LPARAM((&mut context as *mut Context) as isize)) };
}

#[cfg(target_os = "windows")]
fn move_native_panels(x_offset: f32) {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowRect, GetWindowTextLengthW, GetWindowThreadProcessId, SetWindowPos,
        HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE,
    };
    #[repr(C)]
    struct Context {
        pid: u32,
        offset: i32,
    }
    unsafe extern "system" fn each(hwnd: HWND, raw: LPARAM) -> BOOL {
        let context = unsafe { &mut *(raw.0 as *mut Context) };
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
        if owner == context.pid && unsafe { GetWindowTextLengthW(hwnd) } == 0 {
            let mut rect = RECT::default();
            if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok()
                && is_bar_height(f64::from(rect.bottom - rect.top))
            {
                let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
                    unsafe {
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            info.rcWork.left + context.offset,
                            rect.top,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOACTIVATE,
                        );
                    }
                }
            }
        }
        BOOL(1)
    }
    let mut context = Context {
        pid: unsafe { GetCurrentProcessId() },
        offset: x_offset.round() as i32,
    };
    let _ = unsafe { EnumWindows(Some(each), LPARAM((&mut context as *mut Context) as isize)) };
}

mod codex_integration;
mod integration;
mod openclaw_integration;
mod vibe_integration;
