use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::{group_sessions, AgentId, AgentSnapshot, AgentState, GroupKey, Slot};

/// A session the user closed with the × button, and when. The application
/// persists these so a restart does not bring every dormant conversation back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HiddenSession {
    pub id: AgentId,
    pub hidden_at: SystemTime,
}

#[derive(Debug, Default)]
pub struct AgentStore {
    snapshots: HashMap<AgentId, AgentSnapshot>,
    order: Vec<AgentId>,
    hidden: HashMap<AgentId, SystemTime>,
    names: HashMap<AgentId, String>,
    group_names: HashMap<GroupKey, String>,
}

impl AgentStore {
    pub fn apply_snapshot(&mut self, incoming: Vec<AgentSnapshot>) {
        let live: HashSet<_> = incoming.iter().map(|item| item.id.clone()).collect();
        self.snapshots.retain(|id, _| live.contains(id));
        self.order.retain(|id| live.contains(id));
        self.hidden.retain(|id, _| live.contains(id));
        self.names.retain(|id, _| live.contains(id));
        let live_groups: HashSet<_> = incoming.iter().map(Self::group_key).collect();
        self.group_names.retain(|key, _| live_groups.contains(key));

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

    /// The card a session belongs to. Sessions group by working directory;
    /// a manual assignment map consulted here first is all a drag-to-group
    /// feature would need.
    pub fn group_key(snapshot: &AgentSnapshot) -> GroupKey {
        GroupKey::Directory(snapshot.cwd.clone())
    }

    /// Visible sessions folded into bar cards.
    pub fn visible_slots(&self) -> Vec<Slot> {
        group_sessions(self.visible(), Self::group_key, |key| {
            self.group_names.get(key).cloned()
        })
    }

    /// Visible members of a group, in bar order.
    pub fn group_members(&self, key: &GroupKey) -> Vec<AgentSnapshot> {
        self.visible()
            .into_iter()
            .filter(|snapshot| Self::group_key(snapshot) == *key)
            .collect()
    }

    pub fn hide(&mut self, id: &AgentId) {
        if let Some(snapshot) = self.snapshots.get(id) {
            self.hidden.insert(id.clone(), snapshot.last_activity);
        }
    }

    /// Hidden sessions, sorted by ID so the result is stable across refreshes.
    pub fn hidden_sessions(&self) -> Vec<HiddenSession> {
        let mut sessions: Vec<_> = self
            .hidden
            .iter()
            .map(|(id, hidden_at)| HiddenSession {
                id: id.clone(),
                hidden_at: *hidden_at,
            })
            .collect();
        sessions.sort_by(|a, b| {
            (&a.id.provider, &a.id.session_id).cmp(&(&b.id.provider, &b.id.session_id))
        });
        sessions
    }

    /// Seed hidden sessions saved by a previous run. Call before the first
    /// snapshot: sessions that are no longer alive are dropped on that snapshot,
    /// and a session that became active again after `hidden_at` reappears.
    pub fn restore_hidden(&mut self, sessions: impl IntoIterator<Item = HiddenSession>) {
        for session in sessions {
            self.hidden.entry(session.id).or_insert(session.hidden_at);
        }
    }

    pub fn hide_group(&mut self, key: &GroupKey) {
        let ids: Vec<_> = self.group_members(key).into_iter().map(|s| s.id).collect();
        for id in &ids {
            self.hide(id);
        }
    }

    /// Move the `dragged` sessions, kept together in the given order, so that
    /// they sit just before `before` (or at the end).
    pub fn reorder(&mut self, dragged: &[AgentId], before: Option<&AgentId>) {
        if dragged.is_empty()
            || before.is_some_and(|before| dragged.contains(before))
            || dragged.iter().any(|id| !self.order.contains(id))
            || before.is_some_and(|before| !self.order.contains(before))
        {
            return;
        }
        self.order.retain(|id| !dragged.contains(id));
        let index = before
            .and_then(|before| self.order.iter().position(|id| id == before))
            .unwrap_or(self.order.len());
        for (offset, id) in dragged.iter().enumerate() {
            self.order.insert(index + offset, id.clone());
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

    /// Set the label of a group card. Blank restores the project name.
    pub fn rename_group(&mut self, key: &GroupKey, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.group_names.remove(key);
        } else if self
            .snapshots
            .values()
            .any(|snapshot| Self::group_key(snapshot) == *key)
        {
            self.group_names.insert(key.clone(), name.to_owned());
        }
    }

    pub fn get(&self, id: &AgentId) -> Option<&AgentSnapshot> {
        self.snapshots.get(id)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
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
            &[AgentId::new("claude", "b")],
            Some(&AgentId::new("claude", "a")),
        );
        assert_eq!(store.visible()[0].id.session_id, "b");

        store.reorder(&[AgentId::new("claude", "b")], None);
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

    fn item_in(id: &str, cwd: &str) -> AgentSnapshot {
        let mut snapshot = item(id);
        snapshot.cwd = PathBuf::from(cwd);
        snapshot.project_name = AgentSnapshot::project_name(Path::new(cwd));
        snapshot
    }

    #[test]
    fn groups_move_together_hide_together_and_keep_their_name() {
        let mut store = AgentStore::default();
        store.apply_snapshot(vec![
            item_in("a", "/r/x"),
            item_in("b", "/r/y"),
            item_in("c", "/r/x"),
        ]);
        let slots = store.visible_slots();
        assert_eq!(slots.len(), 2);
        assert!(matches!(&slots[0], Slot::Group(group) if group.members.len() == 2));

        // Drag the group after the single card: both members follow, contiguous.
        store.reorder(&slots[0].ids(), None);
        let ids: Vec<_> = store
            .visible()
            .iter()
            .map(|s| s.id.session_id.clone())
            .collect();
        assert_eq!(ids, ["b", "a", "c"]);
        assert!(matches!(&store.visible_slots()[1], Slot::Group(_)));

        // Dropping a card onto one of its own members is a no-op.
        store.reorder(&slots[0].ids(), Some(&AgentId::new("claude", "c")));
        assert_eq!(store.visible().len(), 3);

        let x = GroupKey::Directory("/r/x".into());
        store.rename_group(&x, " Projet X ");
        assert_eq!(store.visible_slots()[1].label(), "Projet X");
        store.rename_group(&GroupKey::Directory("/nowhere".into()), "ignored");
        store.apply_snapshot(vec![item_in("a", "/r/x"), item_in("c", "/r/x")]);
        assert_eq!(store.visible_slots()[0].label(), "Projet X");
        assert_eq!(store.group_members(&x).len(), 2);

        store.hide_group(&x);
        assert!(store.visible().is_empty());

        // A single leftover session is a plain card again, and the name is dropped
        // once nothing runs in that directory.
        store.apply_snapshot(vec![item_in("z", "/r/x")]);
        assert!(matches!(&store.visible_slots()[0], Slot::Single(_)));
        store.apply_snapshot(vec![]);
        store.apply_snapshot(vec![item_in("a", "/r/x"), item_in("c", "/r/x")]);
        assert_eq!(store.visible_slots()[0].label(), "x");
    }

    #[test]
    fn restored_hidden_sessions_stay_hidden_until_new_activity() {
        let mut store = AgentStore::default();
        let hidden_at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        store.restore_hidden(vec![
            HiddenSession {
                id: AgentId::new("claude", "a"),
                hidden_at,
            },
            HiddenSession {
                id: AgentId::new("claude", "dead"),
                hidden_at,
            },
        ]);

        let mut a = item("a");
        a.state = AgentState::Idle;
        a.last_activity = hidden_at;
        store.apply_snapshot(vec![a.clone(), item("b")]);
        assert_eq!(store.visible().len(), 1);
        assert_eq!(store.visible()[0].id.session_id, "b");
        // Dead sessions are forgotten on the first snapshot.
        assert_eq!(store.hidden_sessions().len(), 1);
        assert_eq!(store.hidden_sessions()[0].id.session_id, "a");
        assert_eq!(store.hidden_sessions()[0].hidden_at, hidden_at);

        a.state = AgentState::Active;
        a.last_activity = hidden_at + std::time::Duration::from_secs(1);
        store.apply_snapshot(vec![a, item("b")]);
        assert_eq!(store.visible().len(), 2);
        assert!(store.hidden_sessions().is_empty());
    }

    #[test]
    fn restore_does_not_override_a_live_hide() {
        let mut store = AgentStore::default();
        let mut a = item("a");
        a.last_activity = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(5);
        store.apply_snapshot(vec![a]);
        store.hide(&AgentId::new("claude", "a"));
        store.restore_hidden(vec![HiddenSession {
            id: AgentId::new("claude", "a"),
            hidden_at: SystemTime::UNIX_EPOCH,
        }]);
        assert_eq!(
            store.hidden_sessions()[0].hidden_at,
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(5)
        );
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
