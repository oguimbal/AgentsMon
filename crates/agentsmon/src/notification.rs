use std::cell::RefCell;
use std::collections::HashMap;

use agentsmon_core::{AgentId, AgentSnapshot};
use crossbeam_channel::{Receiver, Sender};
use notify_rust::{Notification, NotificationId, NotificationResponse, Timeout};

pub struct NativeNotifications {
    clicked_tx: Sender<AgentId>,
    clicked_rx: Receiver<AgentId>,
    delivered: RefCell<HashMap<AgentId, NotificationId>>,
}

impl NativeNotifications {
    pub fn new() -> Self {
        let (clicked_tx, clicked_rx) = crossbeam_channel::unbounded();
        Self {
            clicked_tx,
            clicked_rx,
            delivered: RefCell::new(HashMap::new()),
        }
    }

    pub fn show(&self, snapshot: &AgentSnapshot) {
        self.close(&snapshot.id);
        let mut notification = Notification::new();
        notification
            .summary("Agent terminé")
            .body(&format!("{} attend votre attention", snapshot.project_name))
            .timeout(Timeout::Milliseconds(8_000));

        match notification.show() {
            Ok(handle) => {
                self.delivered
                    .borrow_mut()
                    .insert(snapshot.id.clone(), handle.id());
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
                                let _ = clicked.send(id.clone());
                            }
                        });
                    });
            }
            Err(error) => tracing::warn!(%error, "native notification failed"),
        }
    }

    pub fn try_clicked(&self) -> Option<AgentId> {
        self.clicked_rx.try_recv().ok()
    }

    pub fn close(&self, id: &AgentId) {
        let Some(notification_id) = self.delivered.borrow_mut().remove(id) else {
            return;
        };
        close_native(notification_id);
    }
}

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
