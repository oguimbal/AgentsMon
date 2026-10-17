use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId {
    pub provider: String,
    pub session_id: String,
}

impl AgentId {
    pub fn new(provider: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            session_id: session_id.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Active,
    Waiting,
    Idle,
    Compacting,
    /// Turn ended, but the agent armed something that will resume it (a
    /// monitor, a scheduled wakeup). A background shell alone is not this.
    Awaiting,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContextUsage {
    used_percentage: f32,
}

impl ContextUsage {
    pub fn new(value: f32) -> Option<Self> {
        value.is_finite().then(|| Self {
            used_percentage: value.clamp(0.0, 100.0),
        })
    }

    pub fn used_percentage(self) -> f32 {
        self.used_percentage
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageLimit {
    pub label: String,
    used_percentage: f32,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<SystemTime>,
}

impl UsageLimit {
    pub fn new(
        label: impl Into<String>,
        used_percentage: f32,
        window_minutes: Option<u64>,
        resets_at: Option<SystemTime>,
    ) -> Option<Self> {
        used_percentage.is_finite().then(|| Self {
            label: label.into(),
            used_percentage: used_percentage.clamp(0.0, 100.0),
            window_minutes,
            resets_at,
        })
    }

    pub fn used_percentage(&self) -> f32 {
        self.used_percentage
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProviderUsage {
    pub limits: Vec<UsageLimit>,
    pub plan: Option<String>,
    /// The last refresh failed (expired login, network…); values may be outdated.
    #[serde(default)]
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub icon_png: Option<Vec<u8>>,
    pub usage: Option<ProviderUsage>,
    /// Brand colour (0xRRGGBB) used to tag this provider's sessions in the bar.
    #[serde(default)]
    pub accent_rgb: Option<u32>,
}

impl ProviderStatus {
    pub fn new(id: impl Into<String>, usage: Option<ProviderUsage>) -> Self {
        Self {
            id: id.into(),
            display_name: String::new(),
            icon_png: None,
            usage,
            accent_rgb: None,
        }
    }
    pub fn label(&self) -> &str {
        if self.display_name.is_empty() {
            &self.id
        } else {
            &self.display_name
        }
    }
}

impl ProviderUsage {
    pub fn highest_used_percentage(&self) -> Option<f32> {
        self.limits
            .iter()
            .map(UsageLimit::used_percentage)
            .max_by(f32::total_cmp)
    }

    pub fn is_empty(&self) -> bool {
        self.limits.is_empty() && self.plan.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FocusTarget {
    pub host_pid: i32,
    pub host_app: Option<String>,
    pub terminal_session_id: Option<String>,
    pub tmux_pane: Option<String>,
    pub tmux_socket: Option<String>,
    pub uri: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub id: AgentId,
    pub pid: i32,
    pub cwd: PathBuf,
    /// Directory the session groups under when it differs from `cwd`: the
    /// main checkout of a git worktree. Resolved by the application, not providers.
    #[serde(default)]
    pub group_dir: Option<PathBuf>,
    pub project_name: String,
    /// Conversation title chosen by the agent (or renamed by the user).
    /// Metadata only: never derived from message text by AgentsMon.
    #[serde(default)]
    pub title: Option<String>,
    /// Label typed by the user in AgentsMon to replace the project name in the bar.
    #[serde(default)]
    pub custom_name: Option<String>,
    pub state: AgentState,
    pub context_usage: Option<ContextUsage>,
    pub provider_usage: Option<ProviderUsage>,
    pub focus_target: FocusTarget,
    pub last_activity: SystemTime,
}

impl AgentSnapshot {
    /// Text shown on the session card: the user's custom name, else the project name.
    pub fn label(&self) -> &str {
        self.custom_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.project_name)
    }

    /// Text shown on the session card inside a group menu: the user's custom
    /// name, else the conversation title, else the project name.
    ///
    /// The bar keeps the project name because agent-chosen titles are usually
    /// long and several of them side by side would eat the screen width when
    /// many sessions run. A vertical menu has room, and the members of a group
    /// all share the same project name, so the title is what tells them apart.
    pub fn menu_label(&self) -> &str {
        self.custom_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .or(self
                .title
                .as_deref()
                .filter(|title| !title.trim().is_empty()))
            .unwrap_or(&self.project_name)
    }

    pub fn project_name(cwd: &Path) -> String {
        cwd.file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| cwd.display().to_string())
    }
}

/// Why sessions share one card. Today only the working directory groups them;
/// a `Manual(id)` variant (sessions dropped onto each other) can be added
/// without touching the UI, which only ever handles opaque keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GroupKey {
    Directory(PathBuf),
}

/// Several visible sessions shown as one card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionGroup {
    pub key: GroupKey,
    /// Project name of the first member, the default label.
    pub project_name: String,
    /// Label typed by the user for the group card.
    pub custom_name: Option<String>,
    /// At least two members, in bar order.
    pub members: Vec<AgentSnapshot>,
}

impl SessionGroup {
    pub fn label(&self) -> &str {
        self.custom_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.project_name)
    }

    pub fn ids(&self) -> Vec<AgentId> {
        self.members
            .iter()
            .map(|member| member.id.clone())
            .collect()
    }

    /// The member that acted last, the natural target of a click on the group.
    pub fn most_recent(&self) -> Option<&AgentSnapshot> {
        self.members
            .iter()
            .max_by_key(|member| member.last_activity)
    }

    /// The directory shared by every member, if there is one.
    pub fn common_cwd(&self) -> Option<&Path> {
        let first = self.members.first()?;
        self.members
            .iter()
            .all(|member| member.cwd == first.cwd)
            .then_some(first.cwd.as_path())
    }
}

/// One card of the bar: a lone session or a group sharing a directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Slot {
    Single(Box<AgentSnapshot>),
    Group(SessionGroup),
}

impl Slot {
    pub fn label(&self) -> &str {
        match self {
            Slot::Single(agent) => agent.label(),
            Slot::Group(group) => group.label(),
        }
    }

    /// Session ids carried by the card, in bar order.
    pub fn ids(&self) -> Vec<AgentId> {
        match self {
            Slot::Single(agent) => vec![agent.id.clone()],
            Slot::Group(group) => group.ids(),
        }
    }

    /// Identity of the card for drag and drop: its first session.
    pub fn first_id(&self) -> &AgentId {
        match self {
            Slot::Single(agent) => &agent.id,
            Slot::Group(group) => &group.members[0].id,
        }
    }
}

/// Fold visible sessions into slots, grouping those with the same key.
/// A group sits where its first member was; `group_name` supplies custom labels.
pub fn group_sessions(
    agents: Vec<AgentSnapshot>,
    key_of: impl Fn(&AgentSnapshot) -> GroupKey,
    group_name: impl Fn(&GroupKey) -> Option<String>,
) -> Vec<Slot> {
    let mut slots: Vec<Slot> = Vec::new();
    let mut index_by_key: std::collections::HashMap<GroupKey, usize> =
        std::collections::HashMap::new();
    for agent in agents {
        let key = key_of(&agent);
        match index_by_key.get(&key) {
            Some(&index) => {
                let placeholder = Slot::Single(Box::new(agent.clone()));
                slots[index] = match std::mem::replace(&mut slots[index], placeholder) {
                    Slot::Single(first) => Slot::Group(SessionGroup {
                        custom_name: group_name(&key),
                        project_name: match &first.group_dir {
                            // A worktree member first: name the group after its main checkout.
                            Some(dir) => AgentSnapshot::project_name(dir),
                            None => first.project_name.clone(),
                        },
                        key,
                        members: vec![*first, agent],
                    }),
                    Slot::Group(mut group) => {
                        group.members.push(agent);
                        Slot::Group(group)
                    }
                };
            }
            None => {
                index_by_key.insert(key, slots.len());
                slots.push(Slot::Single(Box::new(agent)));
            }
        }
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_usage_is_clamped_and_rejects_nan() {
        assert_eq!(ContextUsage::new(-4.0).unwrap().used_percentage(), 0.0);
        assert_eq!(ContextUsage::new(140.0).unwrap().used_percentage(), 100.0);
        assert!(ContextUsage::new(f32::NAN).is_none());
    }

    #[test]
    fn provider_usage_reports_the_most_constrained_window() {
        let usage = ProviderUsage {
            limits: vec![
                UsageLimit::new("5 h", 42.0, Some(300), None).unwrap(),
                UsageLimit::new("7 j", 71.5, Some(10_080), None).unwrap(),
            ],
            plan: Some("Plus".into()),
            stale: false,
        };
        assert_eq!(usage.highest_used_percentage(), Some(71.5));
        assert_eq!(
            UsageLimit::new("x", 140.0, None, None)
                .unwrap()
                .used_percentage(),
            100.0
        );
        assert!(UsageLimit::new("x", f32::NAN, None, None).is_none());
    }

    fn agent(id: &str, cwd: &str) -> AgentSnapshot {
        AgentSnapshot {
            id: AgentId::new("claude", id),
            pid: 1,
            cwd: cwd.into(),
            group_dir: None,
            project_name: AgentSnapshot::project_name(Path::new(cwd)),
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
    fn sessions_sharing_a_directory_fold_into_one_slot() {
        let slots = group_sessions(
            vec![
                agent("a", "/repo/x"),
                agent("b", "/repo/y"),
                agent("c", "/repo/x"),
                agent("d", "/repo/x"),
            ],
            |agent| GroupKey::Directory(agent.cwd.clone()),
            |key| (*key == GroupKey::Directory("/repo/x".into())).then(|| "Nom perso".to_owned()),
        );
        assert_eq!(slots.len(), 2);
        let Slot::Group(group) = &slots[0] else {
            panic!("first slot should be the group");
        };
        assert_eq!(group.label(), "Nom perso");
        assert_eq!(group.project_name, "x");
        assert_eq!(group.common_cwd(), Some(Path::new("/repo/x")));
        assert_eq!(
            group
                .ids()
                .iter()
                .map(|id| id.session_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "c", "d"]
        );
        assert_eq!(slots[0].first_id().session_id, "a");
        assert!(matches!(&slots[1], Slot::Single(agent) if agent.id.session_id == "b"));
        assert_eq!(slots[1].label(), "y");
    }

    #[test]
    fn worktree_sessions_join_their_main_checkout_group() {
        let mut worktree = agent("w", "/repo/edf/.claude/worktrees/edf-oct");
        worktree.group_dir = Some("/repo/edf".into());
        let slots = group_sessions(
            vec![worktree, agent("m", "/repo/edf")],
            crate::AgentStore::group_key,
            |_| None,
        );
        let [Slot::Group(group)] = slots.as_slice() else {
            panic!("worktree and main checkout should share one card");
        };
        assert_eq!(group.label(), "edf");
        assert_eq!(group.key, GroupKey::Directory("/repo/edf".into()));
        assert_eq!(group.common_cwd(), None);
    }

    #[test]
    fn project_name_is_basename() {
        assert_eq!(
            AgentSnapshot::project_name(Path::new("/tmp/my-project")),
            "my-project"
        );
    }
}
