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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub id: String,
    pub usage: Option<ProviderUsage>,
}

impl ProviderStatus {
    pub fn new(id: impl Into<String>, usage: Option<ProviderUsage>) -> Self {
        Self {
            id: id.into(),
            usage,
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
    pub project_name: String,
    pub state: AgentState,
    pub context_usage: Option<ContextUsage>,
    pub provider_usage: Option<ProviderUsage>,
    pub focus_target: FocusTarget,
    pub last_activity: SystemTime,
}

impl AgentSnapshot {
    pub fn project_name(cwd: &Path) -> String {
        cwd.file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| cwd.display().to_string())
    }
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

    #[test]
    fn project_name_is_basename() {
        assert_eq!(
            AgentSnapshot::project_name(Path::new("/tmp/my-project")),
            "my-project"
        );
    }
}
