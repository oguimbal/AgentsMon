use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentsmon_core::{AgentStore, ProviderStatus};
use agentsmon_providers::{AgentProvider, ClaudeProvider, CodexProvider, ProviderWatcher};
use agentsmon_ui::{
    natural_width, AgentDetailsView, MonitorEvent, MonitorView, ProviderDetailsView,
};
use gpui::{
    actions, point, px, size, AnyWindowHandle, App, AppContext, AsyncApp, Bounds, DisplayId,
    Entity, Menu, MenuItem, Pixels, Subscription, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions,
};
use gpui_platform::application;

actions!(agentsmon, [Quit, ShowBars, Diagnostics]);

const BAR_HEIGHT: f32 = 23.0;
const SCREEN_INSET: f32 = 0.0;
const DETAILS_WIDTH: f32 = 400.0;
const DETAILS_HEIGHT: f32 = 176.0;
const DETAILS_GAP: f32 = 6.0;

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

struct AppStateInner {
    store: RefCell<AgentStore>,
    provider_statuses: RefCell<Vec<ProviderStatus>>,
    providers: Vec<Arc<dyn AgentProvider>>,
    watchers: RefCell<Vec<ProviderWatcher>>,
    panels: RefCell<Vec<Panel>>,
    x_offset: RefCell<f32>,
    details_window: RefCell<Option<AnyWindowHandle>>,
    unread: RefCell<HashSet<agentsmon_core::AgentId>>,
    notifications: notification::NativeNotifications,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    application().run(|cx| {
        let providers: Vec<Arc<dyn AgentProvider>> = vec![
            Arc::new(ClaudeProvider::new()),
            Arc::new(CodexProvider::new()),
        ];
        let watchers = providers
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
        let state = AppState(Rc::new(AppStateInner {
            store: RefCell::new(AgentStore::default()),
            provider_statuses: RefCell::new(Vec::new()),
            providers,
            watchers: RefCell::new(watchers),
            panels: RefCell::new(Vec::new()),
            x_offset: RefCell::new(0.0),
            details_window: RefCell::new(None),
            unread: RefCell::new(HashSet::new()),
            notifications: notification::NativeNotifications::new(),
        }));

        cx.on_action(|_: &Quit, cx| cx.quit());
        let show_state = state.clone();
        cx.on_action(move |_: &ShowBars, cx| reconcile_displays(cx, &show_state, true));
        let diagnostics_state = state.clone();
        cx.on_action(move |_: &Diagnostics, cx| open_diagnostics(cx, &diagnostics_state));
        cx.set_menus([Menu::new("AgentsMon").items([
            MenuItem::action("Afficher les barres", ShowBars),
            MenuItem::action("Diagnostic Claude…", Diagnostics),
            MenuItem::separator(),
            MenuItem::action("Quitter AgentsMon", Quit),
        ])]);

        refresh(&state, cx);
        reconcile_displays(cx, &state, true);
        ensure_integrations(cx);
        spawn_refresh_loop(cx, state);
    });
}

fn ensure_integrations(cx: &mut App) {
    let (sender, receiver) = crossbeam_channel::bounded(1);
    let _ = std::thread::Builder::new()
        .name("agentsmon-integration-setup".into())
        .spawn(move || {
            let status = integration::inspect();
            if !(status.hook_found && status.statusline_wrapped) {
                match integration::install() {
                    Ok(()) => tracing::info!("Claude integration installed automatically"),
                    Err(error) => {
                        tracing::warn!(%error, "automatic Claude integration setup failed; use the application diagnostic to retry")
                    }
                }
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
        let usage = match provider.usage() {
            Ok(usage) => usage,
            Err(error) => {
                tracing::warn!(provider = provider.id(), %error, "provider usage discovery failed");
                None
            }
        };
        provider_statuses.push(ProviderStatus::new(provider.id(), usage.clone()));
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
    }
    snapshots.sort_by_key(|snapshot| snapshot.last_activity);
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
            previous.get(&snapshot.id) == Some(&agentsmon_core::AgentState::Active)
                && snapshot.state != agentsmon_core::AgentState::Active
        })
        .cloned()
        .collect();
    state.0.store.borrow_mut().apply_snapshot(snapshots);
    *state.0.provider_statuses.borrow_mut() = provider_statuses;
    for snapshot in completed {
        state.0.unread.borrow_mut().insert(snapshot.id.clone());
        state.0.notifications.show(&snapshot);
    }
    broadcast(state, cx);
}

fn spawn_refresh_loop(cx: &mut App, state: AppState) {
    let mut last_poll = Instant::now() - Duration::from_secs(3);
    cx.spawn(async move |cx: &mut AsyncApp| loop {
        while let Some(id) = state.0.notifications.try_clicked() {
            cx.update(|cx| focus_agent(&id, &state, cx));
        }
        let should_close_details =
            state.0.details_window.borrow().is_some() && !pointer_is_over_native_panel();
        if should_close_details {
            cx.update(|cx| close_details(&state, cx));
        }
        let changed = state
            .0
            .watchers
            .borrow()
            .iter()
            .any(|watcher| watcher.changes.try_recv().is_ok());
        if changed || last_poll.elapsed() >= Duration::from_secs(3) {
            cx.update(|cx| {
                refresh(&state, cx);
                reconcile_displays(cx, &state, false);
            });
            last_poll = Instant::now();
        }
        cx.background_executor()
            .timer(Duration::from_millis(75))
            .await;
    })
    .detach();
}

fn broadcast(state: &AppState, cx: &mut App) {
    let agents = state.0.store.borrow().visible();
    let providers = state.0.provider_statuses.borrow().clone();
    let unread = state.0.unread.borrow().clone();
    for panel in state.0.panels.borrow().iter() {
        panel.view.update(cx, |view, cx| {
            view.set_data(agents.clone(), providers.clone(), unread.clone(), cx)
        });
        let width = natural_width(&agents, &providers, panel.max_width);
        let _ = cx.update_window(panel.handle, |_, window, _| {
            window.resize(size(px(width), px(BAR_HEIGHT)));
        });
    }
    let max_offset = maximum_x_offset(state, &agents);
    let mut offset = state.0.x_offset.borrow_mut();
    *offset = (*offset).clamp(0.0, max_offset);
    move_native_panels(*offset);
}

fn maximum_x_offset(state: &AppState, agents: &[agentsmon_core::AgentSnapshot]) -> f32 {
    let providers = state.0.provider_statuses.borrow();
    state
        .0
        .panels
        .borrow()
        .iter()
        .map(|panel| {
            panel.display_bounds.size.width.as_f32()
                - natural_width(agents, &providers, panel.max_width)
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
        MonitorEvent::Reorder { dragged, before } => {
            state.0.store.borrow_mut().reorder(dragged, before.as_ref());
            broadcast(state, cx);
        }
        MonitorEvent::MoveBarBy(delta) => {
            close_details(state, cx);
            let agents = state.0.store.borrow().visible();
            let max_offset = maximum_x_offset(state, &agents);
            let mut offset = state.0.x_offset.borrow_mut();
            *offset = (*offset + delta).clamp(0.0, max_offset);
            move_native_panels(*offset);
        }
        MonitorEvent::ShowDetails { agent, anchor_x } => {
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
    }
}

fn focus_agent(id: &agentsmon_core::AgentId, state: &AppState, cx: &mut App) {
    state.0.unread.borrow_mut().remove(id);
    state.0.notifications.close(id);
    let target = state
        .0
        .store
        .borrow()
        .get(id)
        .map(|snapshot| snapshot.focus_target.clone());
    if let Some(target) = target {
        if let Some(provider) = state
            .0
            .providers
            .iter()
            .find(|provider| provider.id() == id.provider)
        {
            if let Err(error) = provider.focus(&target) {
                tracing::warn!(%error, session = %id.session_id, provider = %id.provider, "focus failed");
            }
        } else {
            tracing::warn!(provider = %id.provider, "focus provider is unavailable");
        }
    }
    broadcast(state, cx);
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
    close_details(state, cx);
    let panel_x = display_bounds.origin.x.as_f32() + *state.0.x_offset.borrow();
    let min_x = display_bounds.origin.x.as_f32() + 6.0;
    let max_x =
        (display_bounds.origin.x + display_bounds.size.width).as_f32() - DETAILS_WIDTH - 6.0;
    let x = (panel_x + anchor_x - DETAILS_WIDTH / 2.0).clamp(min_x, max_x.max(min_x));
    let y = (display_bounds.origin.y + display_bounds.size.height).as_f32()
        - BAR_HEIGHT
        - DETAILS_GAP
        - DETAILS_HEIGHT;
    let options = WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(x), px(y)),
            size: size(px(DETAILS_WIDTH), px(DETAILS_HEIGHT)),
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
    match cx.open_window(options, move |_, cx| {
        cx.new(|_| AgentDetailsView::new(agent))
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
    close_details(state, cx);
    let panel_x = display_bounds.origin.x.as_f32() + *state.0.x_offset.borrow();
    let min_x = display_bounds.origin.x.as_f32() + 6.0;
    let max_x =
        (display_bounds.origin.x + display_bounds.size.width).as_f32() - DETAILS_WIDTH - 6.0;
    let x = (panel_x + anchor_x - DETAILS_WIDTH / 2.0).clamp(min_x, max_x.max(min_x));
    let y = (display_bounds.origin.y + display_bounds.size.height).as_f32()
        - BAR_HEIGHT
        - DETAILS_GAP
        - DETAILS_HEIGHT;
    let options = WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(x), px(y)),
            size: size(px(DETAILS_WIDTH), px(DETAILS_HEIGHT)),
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

    for panel in state.0.panels.borrow_mut().drain(..) {
        let _ = cx.update_window(panel.handle, |_, window, _| window.remove_window());
    }

    let agents = state.0.store.borrow().visible();
    let providers = state.0.provider_statuses.borrow().clone();
    let unread = state.0.unread.borrow().clone();
    for display in displays {
        let bounds = display.bounds();
        let max_width = (bounds.size.width.as_f32() - SCREEN_INSET * 2.0).max(76.0);
        let width = natural_width(&agents, &providers, max_width);
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
        let initial = agents.clone();
        let initial_providers = providers.clone();
        let initial_unread = unread.clone();
        match cx.open_window(options, move |_, cx| {
            let view = cx.new(|_| MonitorView::new(initial, initial_providers, initial_unread));
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
