use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use crate::{AgentId, AgentSnapshot, AgentState};

#[derive(Debug, Default)]
pub struct AgentStore {
    snapshots: HashMap<AgentId, AgentSnapshot>,
    order: Vec<AgentId>,
    hidden: HashMap<AgentId, SystemTime>,
    names: HashMap<AgentId, String>,
}

impl AgentStore {
    pub fn apply_snapshot(&mut self, incoming: Vec<AgentSnapshot>) {
        let live: HashSet<_> = incoming.iter().map(|item| item.id.clone()).collect();
        self.snapshots.retain(|id, _| live.contains(id));
        self.order.retain(|id| live.contains(id));
        self.hidden.retain(|id, _| live.contains(id));
        self.names.retain(|id, _| live.contains(id));

        for mut snapshot in incoming {
            snapshot.custom_name = self.names.get(&snapshot.id).cloned();
            let has_new_activity = self.hidden.get(&snapshot.id).is_some_and(|hidden_at| {
                snapshot.state == AgentState::Active && snapshot.last_activity > *hidden_at
            });
            if has_new_activity {
                self.hidden.remove(&snapshot.id);
            }
            if !self.snapshots.contains_key(&snapshot.id) {
                self.order.push(snapshot.id.clone());
            }
            self.snapshots.insert(snapshot.id.clone(), snapshot);
        }
    }

    pub fn visible(&self) -> Vec<AgentSnapshot> {
        self.order
            .iter()
            .filter(|id| !self.hidden.contains_key(*id))
            .filter_map(|id| self.snapshots.get(id).cloned())
            .collect()
    }

    pub fn hide(&mut self, id: &AgentId) {
        if let Some(snapshot) = self.snapshots.get(id) {
            self.hidden.insert(id.clone(), snapshot.last_activity);
        }
    }

    pub fn reorder(&mut self, dragged: &AgentId, before: Option<&AgentId>) {
        if before.is_some_and(|before| dragged == before)
            || !self.order.contains(dragged)
            || before.is_some_and(|before| !self.order.contains(before))
        {
            return;
        }
        self.order.retain(|id| id != dragged);
        if let Some(index) = before.and_then(|before| self.order.iter().position(|id| id == before))
        {
            self.order.insert(index, dragged.clone());
        } else {
            self.order.push(dragged.clone());
        }
    }

    /// Set the card label for a session. An empty (or blank) name restores the project name.
    pub fn rename(&mut self, id: &AgentId, name: &str) {
        let Some(snapshot) = self.snapshots.get_mut(id) else {
            return;
        };
        let name = name.trim();
        if name.is_empty() {
            self.names.remove(id);
            snapshot.custom_name = None;
        } else {
            self.names.insert(id.clone(), name.to_owned());
            snapshot.custom_name = Some(name.to_owned());
        }
    }

    pub fn get(&self, id: &AgentId) -> Option<&AgentSnapshot> {
        self.snapshots.get(id)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use super::*;
    use crate::{AgentState, FocusTarget};

    fn item(id: &str) -> AgentSnapshot {
        AgentSnapshot {
            id: AgentId::new("claude", id),
            pid: 10,
            cwd: PathBuf::from(format!("/tmp/{id}")),
            project_name: id.into(),
            title: None,
            custom_name: None,
            state: AgentState::Idle,
            context_usage: None,
            provider_usage: None,
            focus_target: FocusTarget::default(),
            last_activity: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn keeps_order_hides_and_releases_finished_ids() {
        let mut store = AgentStore::default();
        store.apply_snapshot(vec![item("a"), item("b")]);
        store.reorder(
            &AgentId::new("claude", "b"),
            Some(&AgentId::new("claude", "a")),
        );
        assert_eq!(store.visible()[0].id.session_id, "b");

        store.reorder(&AgentId::new("claude", "b"), None);
        assert_eq!(store.visible()[1].id.session_id, "b");

        store.hide(&AgentId::new("claude", "b"));
        assert_eq!(store.visible().len(), 1);
        store.apply_snapshot(vec![item("a")]);
        store.apply_snapshot(vec![item("a"), item("b")]);
        assert_eq!(store.visible().len(), 2);
        assert_eq!(store.visible()[1].id.session_id, "b");
    }

    #[test]
    fn custom_names_survive_refreshes_and_clear_on_blank() {
        let mut store = AgentStore::default();
        store.apply_snapshot(vec![item("a"), item("b")]);
        let a = AgentId::new("claude", "a");
        store.rename(&a, "  Refonte auth  ");
        assert_eq!(store.visible()[0].label(), "Refonte auth");
        assert_eq!(store.visible()[1].label(), "b");

        store.apply_snapshot(vec![item("a"), item("b")]);
        assert_eq!(
            store.get(&a).unwrap().custom_name.as_deref(),
            Some("Refonte auth")
        );

        store.rename(&a, "   ");
        assert_eq!(store.visible()[0].label(), "a");
        assert!(store.get(&a).unwrap().custom_name.is_none());

        store.rename(&AgentId::new("claude", "zz"), "ghost");
        store.apply_snapshot(vec![item("b")]);
        store.apply_snapshot(vec![item("a"), item("b")]);
        assert!(store.get(&a).unwrap().custom_name.is_none());
    }

    #[test]
    fn hidden_session_returns_on_new_active_activity() {
        let mut store = AgentStore::default();
        let mut session = item("a");
        session.state = AgentState::Active;
        store.apply_snapshot(vec![session.clone()]);
        store.hide(&session.id);

        store.apply_snapshot(vec![session.clone()]);
        assert!(store.visible().is_empty());

        session.state = AgentState::Waiting;
        session.last_activity = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        store.apply_snapshot(vec![session.clone()]);
        assert!(store.visible().is_empty());

        session.state = AgentState::Active;
        session.last_activity = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2);
        store.apply_snapshot(vec![session]);
        assert_eq!(store.visible().len(), 1);
    }
}
