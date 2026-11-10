//! Isolated manual UI check: no providers, hooks, panels or real diagnostic
//! storage. `--notify` additionally tests the native click callback; use an app
//! bundle with notification authorization on macOS. See the maintenance doc.
use gpui::{
    actions, point, px, size, AnyWindowHandle, App, AppContext, AsyncApp, Bounds, Menu, MenuItem,
    WindowBounds, WindowOptions,
};
use std::{cell::RefCell, rc::Rc, time::Duration};

#[allow(dead_code)]
#[path = "../src/notification.rs"]
mod notification;

actions!(diagnostic_preview, [Quit]);

const REPORT: &str = "LOCAL TEST — synthetic example, no real failure detected.\n\nDiagnose and fix state tracking in AgentsMon for Codex.\nRead docs/codex-desktop-ipc.md first.\nKeep the hook fallback and decode no conversation content.\n\nAgentsMon: 0.1.0\nIncident: stream_version_changed\nCodex approval tracking is degraded.\nFirst seen: 2026-09-16T18:45:23+00:00\nLast recorded observation: 2026-09-16T19:19:00+00:00\n\nadapter=codex-desktop-ipc\nphase=snapshot\ncode=stream_version_changed\nexpected=11; observed=Some(12)\nexpected_versions: initialize=0; owner=1; follow=1; stream=11\nsocket=/example/path/.codex/ipc/ipc.sock\nsession=00000000-0000-0000-0000-000000000001\nframe_limit_bytes=16777216\nrequest_timeout_ms=3000\nfallback=hooks_and_lifecycle\n\nPersistent log: /example/path/diagnostics/provider-diagnostics.log\nPrevious log: /example/path/diagnostics/provider-diagnostics.log.1\n\nThese details contain technical metadata only.\nAgentsMon accepted or refused no permission.";

fn show(cx: &mut App, slot: &RefCell<Option<AnyWindowHandle>>, title: String, report: String) {
    if let Some(window) = slot.borrow_mut().take() {
        let _ = cx.update_window(window, |_, window, _| window.remove_window());
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
        is_resizable: true,
        ..Default::default()
    };
    let handle = cx
        .open_window(options, move |_, cx| {
            cx.new(|_| agentsmon_ui::DiagnosticReportView::new(title, report))
        })
        .expect("preview window");
    *slot.borrow_mut() = Some(handle.into());
    cx.activate(true);
}

fn main() {
    gpui_platform::application().run(|cx| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus([Menu::new("Diagnostic de test").items([MenuItem::action("Quit", Quit)])]);
        let slot = Rc::new(RefCell::new(None));
        show(
            cx,
            &slot,
            "Connection diagnostics — TEST".into(),
            REPORT.into(),
        );
        if std::env::args().any(|arg| arg == "--notify") {
            let notifications = notification::NativeNotifications::new();
            notifications.show_diagnostic("Codex — TEST", REPORT.into());
            cx.spawn(async move |cx: &mut AsyncApp| loop {
                while let Some(click) = notifications.try_clicked() {
                    if let notification::NotificationClick::Diagnostic { title, prompt } = click {
                        cx.update(|cx| show(cx, &slot, title, prompt));
                    }
                }
                cx.background_executor()
                    .timer(Duration::from_millis(75))
                    .await;
            })
            .detach();
        }
    });
}
