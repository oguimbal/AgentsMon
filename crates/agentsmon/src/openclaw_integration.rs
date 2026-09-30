//! "Connecter OpenClaw…": the provider-menu action and its window. The shared
//! gateway token typed here only lives in memory until pairing succeeds; the
//! provider then keeps the device token the gateway mints for AgentsMon.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use agentsmon_providers::openclaw::{OpenClawProvider, PairingState};
use agentsmon_ui::{ConnectEvent, ConnectField, ConnectStatus, ConnectView, MenuAction};
use gpui::{
    point, px, size, AnyWindowHandle, App, AppContext, AsyncApp, Bounds, WindowBounds, WindowKind,
    WindowOptions,
};

pub(crate) const CONNECT_ACTION: &str = "openclaw.connect";
const STATUS_INTERVAL: Duration = Duration::from_millis(250);
/// How long "Connecté" stays visible before the window closes.
const CLOSE_DELAY: Duration = Duration::from_millis(1500);

pub(crate) fn menu_actions(openclaw: &OpenClawProvider) -> Vec<MenuAction> {
    if openclaw.needs_connection() {
        vec![MenuAction {
            id: CONNECT_ACTION.into(),
            label: "Connecter OpenClaw…".into(),
        }]
    } else {
        Vec::new()
    }
}

/// Opens the connect window; `on_connected` runs once pairing succeeds.
pub(crate) fn open_connect_window(
    cx: &mut App,
    openclaw: Arc<OpenClawProvider>,
    on_connected: Rc<dyn Fn(&mut App)>,
) -> Option<AnyWindowHandle> {
    let options = WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("Connecter OpenClaw".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(140.0), px(140.0)),
            size: size(px(500.0), px(420.0)),
        })),
        kind: WindowKind::Normal,
        focus: true,
        show: true,
        is_movable: true,
        is_resizable: false,
        ..Default::default()
    };
    openclaw.clear_pairing_status();
    let initial_url = openclaw.configured_url().unwrap_or_default();
    let view_slot = Rc::new(std::cell::RefCell::new(None));
    let slot = view_slot.clone();
    let handle = match cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| {
            ConnectView::new(
                "Connecter OpenClaw".into(),
                "AgentsMon suit vos conversations OpenClaw en lecture seule. Indiquez \
                 l'adresse de l'interface web de la passerelle et son token : il sert \
                 uniquement à l'appairage et n'est pas enregistré."
                    .into(),
                vec![
                    ConnectField {
                        label: "Adresse de la passerelle".into(),
                        initial: initial_url,
                        masked: false,
                    },
                    ConnectField {
                        label: "Token de la passerelle".into(),
                        initial: String::new(),
                        masked: true,
                    },
                ],
                "Connecter".into(),
                window,
                cx,
            )
        });
        *slot.borrow_mut() = Some(view.clone());
        view
    }) {
        Ok(handle) => handle,
        Err(error) => {
            tracing::warn!(%error, "cannot open the OpenClaw connect window");
            return None;
        }
    };
    let view = view_slot.borrow().clone()?;
    cx.activate(true);

    let events_provider = openclaw.clone();
    let window: AnyWindowHandle = handle.into();
    cx.subscribe(&view, move |view, event, cx| match event {
        ConnectEvent::Submit(values) => {
            let url = values.first().map(String::as_str).unwrap_or_default();
            let token = values.get(1).map(String::as_str).unwrap_or_default();
            let status = match events_provider.pair(url, token) {
                Ok(()) => status_for(&PairingState::Connecting),
                Err(code) => ConnectStatus::Error(message_for(code)),
            };
            view.update(cx, |view, cx| view.set_status(status, cx));
        }
        ConnectEvent::Retry => events_provider.retry_pairing(),
        ConnectEvent::Cancel => {
            let _ = cx.update_window(window, |_, window, _| window.remove_window());
        }
    })
    .detach();

    // Follow the worker's progress; closing the window abandons a pairing.
    cx.spawn(async move |cx: &mut AsyncApp| loop {
        cx.background_executor().timer(STATUS_INTERVAL).await;
        let state = openclaw.pairing_state();
        let status = status_for(&state);
        let open = cx.update(|cx| {
            cx.update_window(window, |_, _, cx| {
                view.update(cx, |view, cx| view.set_status(status, cx));
            })
            .is_ok()
        });
        if !open {
            if state != PairingState::Connected {
                openclaw.cancel_pairing();
            }
            return;
        }
        if state == PairingState::Connected {
            cx.update(|cx| on_connected(cx));
            cx.background_executor().timer(CLOSE_DELAY).await;
            cx.update(|cx| {
                let _ = cx.update_window(window, |_, window, _| window.remove_window());
            });
            return;
        }
    })
    .detach();
    Some(window)
}

fn status_for(state: &PairingState) -> ConnectStatus {
    match state {
        PairingState::Idle => ConnectStatus::Idle,
        PairingState::Connecting => ConnectStatus::Busy("Connexion à la passerelle…".into()),
        PairingState::AwaitingApproval { request_id, paused } => ConnectStatus::Waiting {
            message: if *paused {
                "AgentsMon attend son approbation. Sur le serveur OpenClaw, lancez cette \
                 commande puis cliquez sur « Réessayer maintenant »."
                    .into()
            } else {
                "AgentsMon attend son approbation. Sur le serveur OpenClaw, lancez cette \
                 commande : la connexion reprendra d'elle-même."
                    .into()
            },
            command: Some(match request_id {
                Some(id) => format!("openclaw devices approve {id}"),
                None => "openclaw devices list".into(),
            }),
        },
        PairingState::Connected => ConnectStatus::Done(
            "Connecté. Vos conversations OpenClaw vont apparaître dans la barre.".into(),
        ),
        PairingState::Failed { code } => ConnectStatus::Error(message_for(code)),
    }
}

/// French explanation of a content-free failure code, the code kept for support.
fn message_for(code: &str) -> String {
    let explanation = match code {
        "invalid-url" => {
            "Adresse invalide : utilisez l'URL de l'interface web, par exemple http://hote:18789/."
        }
        "missing-token" => "Indiquez le token de la passerelle.",
        "AUTH_TOKEN_MISMATCH" | "AUTH_UNAUTHORIZED" | "AUTH_PASSWORD_MISMATCH" => {
            "Token refusé par la passerelle."
        }
        "AUTH_RATE_LIMITED" => {
            "Trop de tentatives : la passerelle bloque temporairement. Réessayez dans quelques minutes."
        }
        "resolve" | "connect" | "timeout" | "websocket-handshake" | "closed" | "read" | "send" => {
            "Passerelle injoignable. Vérifiez l'adresse et le réseau."
        }
        "no-device-token" => {
            "La passerelle n'a pas délivré de jeton d'appareil : AgentsMon ne pourrait pas rester connecté."
        }
        "save-failed" => "Impossible d'enregistrer l'appairage dans le dossier de données d'AgentsMon.",
        "pairing-timeout" => "Aucune approbation reçue après 15 minutes.",
        "PROTOCOL_MISMATCH" | "CLIENT_VERSION_MISMATCH" | "protocol-version" => {
            "Version de passerelle incompatible avec AgentsMon (protocole 4)."
        }
        code if code.starts_with("DEVICE_AUTH_") => {
            "La passerelle a refusé la preuve d'identité d'AgentsMon."
        }
        _ => "Connexion refusée par la passerelle.",
    };
    format!("{explanation} ({code})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_status_shows_the_exact_command() {
        let status = status_for(&PairingState::AwaitingApproval {
            request_id: Some("req-42".into()),
            paused: false,
        });
        assert!(matches!(
            status,
            ConnectStatus::Waiting { command: Some(command), .. }
                if command == "openclaw devices approve req-42"
        ));
    }

    #[test]
    fn failures_keep_their_code() {
        assert!(message_for("DEVICE_AUTH_SIGNATURE_INVALID")
            .ends_with("(DEVICE_AUTH_SIGNATURE_INVALID)"));
        assert!(message_for("AUTH_TOKEN_MISMATCH").starts_with("Token refusé"));
    }
}
