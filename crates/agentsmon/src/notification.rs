use std::cell::RefCell;
use std::collections::HashMap;

use agentsmon_core::{AgentId, AgentSnapshot};
use crossbeam_channel::{Receiver, Sender};
use notify_rust::{Notification, NotificationId, NotificationResponse, Timeout};

pub struct NativeNotifications {
    clicked_tx: Sender<NotificationClick>,
    clicked_rx: Receiver<NotificationClick>,
    delivered: RefCell<HashMap<AgentId, NotificationId>>,
    diagnostic: RefCell<Option<NotificationId>>,
}

#[derive(Clone)]
pub enum NotificationClick {
    Agent(AgentId),
    Diagnostic { title: String, prompt: String },
}

impl NativeNotifications {
    pub fn new() -> Self {
        let (clicked_tx, clicked_rx) = crossbeam_channel::bounded(32);
        request_authorization();
        Self {
            clicked_tx,
            clicked_rx,
            delivered: RefCell::new(HashMap::new()),
            diagnostic: RefCell::new(None),
        }
    }

    pub fn show(&self, snapshot: &AgentSnapshot) {
        self.close(&snapshot.id);
        let mut notification = Notification::new();
        notification
            .summary("Agent terminé")
            .body(&format!("{} attend votre attention", snapshot.label()))
            .timeout(Timeout::Milliseconds(8_000));

        match notification.show() {
            Ok(handle) => {
                if let Some(notification_id) = delivered_id(&handle) {
                    self.delivered
                        .borrow_mut()
                        .insert(snapshot.id.clone(), notification_id);
                }
                let id = snapshot.id.clone();
                let clicked = self.clicked_tx.clone();
                let _ = std::thread::Builder::new()
                    .name("agentsmon-notification".into())
                    .spawn(move || {
                        let _ = handle.wait_for_response(move |response: &NotificationResponse| {
                            if matches!(
                                response,
                                NotificationResponse::Default | NotificationResponse::Action(_)
                            ) {
                                let _ = clicked.try_send(NotificationClick::Agent(id.clone()));
                            }
                        });
                    });
            }
            Err(error) => tracing::warn!(%error, "native notification failed"),
        }
    }

    /// The caller owns the durable hourly limit. Capture the exact report in
    /// the callback so a later recovery does not erase the incident on click.
    pub fn show_diagnostic(&self, name: &str, prompt: String) {
        if let Some(id) = self.diagnostic.borrow_mut().take() {
            close_native(id);
        }
        let title = format!("Suivi {name} dégradé");
        match Notification::new()
            .summary(&title)
            .body("Le statut peut être incomplet. Cliquez pour ouvrir les détails et copier un prompt de diagnostic.")
            // Keep the report reachable from Notification Center after the
            // banner disappears. mac-usernotifications treats a numeric timeout
            // as deletion + cancellation of the click handler, not banner TTL.
            .timeout(Timeout::Never)
            .show()
        {
            Ok(handle) => {
                *self.diagnostic.borrow_mut() = delivered_id(&handle);
                let clicked = self.clicked_tx.clone();
                let result = std::thread::Builder::new().name("agentsmon-diagnostic-notification".into()).spawn(move || {
                    let _ = handle.wait_for_response(move |response: &NotificationResponse| {
                        if matches!(response, NotificationResponse::Default | NotificationResponse::Action(_)) {
                            let _ = clicked.try_send(NotificationClick::Diagnostic { title: title.clone(), prompt: prompt.clone() });
                        }
                    });
                });
                if let Err(error) = result { tracing::warn!(%error, "cannot listen for diagnostic notification click; report remains available in menu"); }
            }
            Err(error) => tracing::warn!(%error, "diagnostic notification failed; report remains available in menu"),
        }
    }

    pub fn try_clicked(&self) -> Option<NotificationClick> {
        self.clicked_rx.try_recv().ok()
    }

    pub fn close(&self, id: &AgentId) {
        let Some(notification_id) = self.delivered.borrow_mut().remove(id) else {
            return;
        };
        close_native(notification_id);
    }
}

#[cfg(not(target_os = "windows"))]
fn delivered_id(handle: &notify_rust::NotificationHandle) -> Option<NotificationId> {
    Some(handle.id())
}

/// Windows toasts have no identifier: notify-rust cannot remove them from history
/// and does not export their handle type.
#[cfg(target_os = "windows")]
fn delivered_id<Handle>(_handle: &Handle) -> Option<NotificationId> {
    None
}

#[cfg(target_os = "macos")]
fn request_authorization() {
    match mac_usernotifications::blocking::request_auth() {
        Ok(true) => {}
        Ok(false) => tracing::warn!("native notifications are disabled in macOS settings"),
        Err(error) => tracing::warn!(%error, "cannot request native notification permission"),
    }
}

#[cfg(not(target_os = "macos"))]
fn request_authorization() {}

#[cfg(target_os = "macos")]
fn close_native(id: NotificationId) {
    if let NotificationId::Mac(id) = id {
        mac_usernotifications::blocking::close_delivered(&id);
    }
}

#[cfg(not(target_os = "macos"))]
fn close_native(_id: NotificationId) {
    // notify-rust exposes activation on Windows, but not removal from notification history.
}
