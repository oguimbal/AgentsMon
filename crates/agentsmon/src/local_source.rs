//! Local IPC adapter; all actions return to the existing application focus path.
use crate::AppState;
use agentsmon_core::{AgentId, AgentState, Slot};
use local_source_ipc::{Entry, SessionState, Snapshot, MAX_ENTRIES};
#[cfg(any(unix, test))]
use local_source_ipc::{Envelope, Focus, AGENTSMON};
use std::sync::{Arc, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub struct FocusCommand {
    pub id: AgentId,
    pub deadline: Instant,
    pub reply: crossbeam_channel::Sender<Result<(), &'static str>>,
}

pub struct LocalSource {
    snapshot: Arc<RwLock<Snapshot>>,
    pub commands: crossbeam_channel::Receiver<FocusCommand>,
    #[cfg(unix)]
    _server: Option<local_source_ipc::unix::Server>,
}

impl LocalSource {
    pub fn new() -> Self {
        let snapshot = Arc::new(RwLock::new(Snapshot {
            instance: format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            capabilities: vec!["list".into(), "focus".into()],
            entries: Vec::new(),
        }));
        let (sender, commands) = crossbeam_channel::bounded::<FocusCommand>(8);
        #[cfg(unix)]
        let server = {
            let data = snapshot.clone();
            let result = local_source_ipc::unix::endpoint(AGENTSMON).and_then(|path| {
                local_source_ipc::unix::Server::start(path, AGENTSMON, move |request, deadline| {
                    handle_request(request, deadline, &data, &sender)
                })
            });
            match result {
                Ok(server) => Some(server),
                Err(error) => {
                    tracing::warn!(%error, "local source IPC unavailable");
                    None
                }
            }
        };
        #[cfg(not(unix))]
        let _ = sender;
        Self {
            snapshot,
            commands,
            #[cfg(unix)]
            _server: server,
        }
    }

    pub fn publish(&self, state: &AppState) {
        let providers = state.0.provider_statuses.borrow();
        let unread = state.0.unread.borrow();
        let slots = state.0.store.borrow().visible_slots();
        let entries = export_entries(slots, &providers, &unread);
        if let Ok(mut snapshot) = self.snapshot.write() {
            snapshot.entries = entries;
        }
    }
}

fn export_entries(
    slots: Vec<Slot>,
    providers: &[agentsmon_core::ProviderStatus],
    unread: &std::collections::HashSet<AgentId>,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    for slot in slots {
        let (members, group_name) = match slot {
            Slot::Single(agent) => (vec![*agent], None),
            Slot::Group(group) => (group.members, group.custom_name),
        };
        for agent in members {
            let provider = providers.iter().find(|p| p.id == agent.id.provider);
            let Ok(id) = serde_json::to_string(&agent.id) else {
                continue;
            };
            let entry = Entry {
                id,
                label: agent.label().into(),
                project_name: agent.project_name.clone(),
                repository_path: agent.cwd.to_string_lossy().into_owned(),
                title: agent.title,
                custom_name: agent.custom_name,
                group_name: group_name.clone(),
                provider_name: provider
                    .map(|p| p.label())
                    .unwrap_or(&agent.id.provider)
                    .into(),
                accent_rgb: provider.and_then(|p| p.accent_rgb),
                state: match agent.state {
                    AgentState::Active => SessionState::Active,
                    AgentState::Waiting => SessionState::Waiting,
                    AgentState::Idle => SessionState::Idle,
                    AgentState::Compacting => SessionState::Compacting,
                },
                unread: unread.contains(&agent.id),
                context_percent: agent
                    .context_usage
                    .map(|c| c.used_percentage().round() as u8),
                last_activity_ms: agent
                    .last_activity
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u64::MAX as u128) as u64,
            };
            if entry.validate() {
                entries.push(entry);
            }
        }
    }
    entries.truncate(MAX_ENTRIES);
    entries
}

#[cfg(any(unix, test))]
fn handle_request(
    request: Envelope,
    deadline: Instant,
    data: &RwLock<Snapshot>,
    sender: &crossbeam_channel::Sender<FocusCommand>,
) -> Envelope {
    match request.method.as_str() {
        "list" => match data
            .read()
            .ok()
            .and_then(|s| serde_json::to_value(&*s).ok())
        {
            Some(snapshot) => Envelope::new(AGENTSMON, "list", snapshot),
            None => Envelope::error(AGENTSMON, "unavailable"),
        },
        "focus" => {
            let Ok(focus) = serde_json::from_value::<Focus>(request.payload) else {
                return Envelope::error(AGENTSMON, "invalid_request");
            };
            let valid = data.read().is_ok_and(|s| {
                s.instance == focus.instance && s.entries.iter().any(|e| e.id == focus.id)
            });
            if !valid {
                return Envelope::error(AGENTSMON, "stale_session");
            }
            let Ok(id) = serde_json::from_str::<AgentId>(&focus.id) else {
                return Envelope::error(AGENTSMON, "invalid_request");
            };
            let (reply, response) = crossbeam_channel::bounded(1);
            if sender
                .try_send(FocusCommand {
                    id,
                    deadline,
                    reply,
                })
                .is_err()
            {
                return Envelope::error(AGENTSMON, "busy");
            }
            match response.recv_deadline(deadline) {
                Ok(Ok(())) => {
                    Envelope::new(AGENTSMON, "focus", serde_json::json!({"focused":true}))
                }
                Ok(Err(code)) => Envelope::error(AGENTSMON, code),
                Err(_) => Envelope::error(AGENTSMON, "timeout"),
            }
        }
        _ => Envelope::error(AGENTSMON, "unsupported_method"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn focus_is_delegated_by_exact_id_and_only_acknowledged_after_completion() {
        let envelope = Envelope::decode(include_bytes!(
            "../../local-source-ipc/fixtures/agentsmon-v1.json"
        ))
        .unwrap();
        let snapshot: Snapshot = serde_json::from_value(envelope.payload).unwrap();
        let id = snapshot.entries[0].id.clone();
        let request = Envelope::new(
            AGENTSMON,
            "focus",
            serde_json::json!({"instance":snapshot.instance, "id":id}),
        );
        let (tx, rx) = crossbeam_channel::bounded(1);
        let thread = std::thread::spawn(move || {
            handle_request(
                request,
                Instant::now() + Duration::from_secs(1),
                &RwLock::new(snapshot),
                &tx,
            )
        });
        let command = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(command.id, AgentId::new("claude", "session-1"));
        command.reply.send(Ok(())).unwrap();
        assert_eq!(thread.join().unwrap().payload["focused"], true);
    }

    #[test]
    fn projection_uses_live_names_groups_visibility_and_unread_state() {
        use agentsmon_core::{AgentSnapshot, AgentStore, FocusTarget};
        let mut store = AgentStore::default();
        let make = |id: &str| AgentSnapshot {
            id: AgentId::new("claude", id),
            pid: 1,
            cwd: "/repo/api".into(),
            group_dir: None,
            project_name: "api".into(),
            title: Some("Agent title".into()),
            custom_name: None,
            state: AgentState::Waiting,
            context_usage: None,
            provider_usage: None,
            focus_target: FocusTarget::default(),
            last_activity: UNIX_EPOCH,
        };
        store.apply_snapshot(vec![make("a"), make("b"), make("hidden")]);
        store.rename(&AgentId::new("claude", "a"), "My task");
        store.rename_group(
            &agentsmon_core::GroupKey::Directory("/repo/api".into()),
            "Backend",
        );
        store.hide(&AgentId::new("claude", "hidden"));
        let unread = [AgentId::new("claude", "a")].into_iter().collect();
        let entries = export_entries(store.visible_slots(), &[], &unread);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].label, "My task");
        assert_eq!(entries[0].custom_name.as_deref(), Some("My task"));
        assert_eq!(entries[0].group_name.as_deref(), Some("Backend"));
        assert_eq!(entries[0].title.as_deref(), Some("Agent title"));
        assert!(entries[0].unread);
        assert!(!entries[1].unread);
        assert_eq!(entries[0].state, SessionState::Waiting);
    }

    #[test]
    fn stale_focus_and_unknown_commands_never_reach_the_ui() {
        let data = RwLock::new(Snapshot {
            instance: "new".into(),
            capabilities: vec!["focus".into()],
            entries: vec![],
        });
        let (tx, rx) = crossbeam_channel::bounded(1);
        for (method, payload, code) in [
            (
                "focus",
                serde_json::json!({"instance":"old", "id":"gone"}),
                "stale_session",
            ),
            ("focus", serde_json::Value::Null, "invalid_request"),
            (
                "future_command",
                serde_json::Value::Null,
                "unsupported_method",
            ),
        ] {
            let response = handle_request(
                Envelope::new(AGENTSMON, method, payload),
                Instant::now() + Duration::from_secs(1),
                &data,
                &tx,
            );
            assert_eq!(response.payload["code"], code);
            assert!(rx.is_empty());
        }
    }
}
