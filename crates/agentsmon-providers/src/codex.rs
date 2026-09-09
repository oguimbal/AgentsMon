use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use agentsmon_core::{
    AgentId, AgentSnapshot, AgentState, ContextUsage, FocusTarget, ProviderUsage, UsageLimit,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::{focus, AgentProvider, ProviderWatcher};

const MAX_RECORD_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const INTERRUPTED_ACTIVITY_GRACE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexStatusRecord {
    pub session_id: String,
    pub pid: i32,
    #[serde(default)]
    pub ppid: i32,
    pub state: String,
    #[serde(default)]
    pub activity: String,
    pub timestamp: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub context_used_percentage: Option<f32>,
    #[serde(default)]
    pub rate_limits: Option<CodexRateLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexRateLimits {
    #[serde(default)]
    pub primary: Option<CodexRateLimitWindow>,
    #[serde(default)]
    pub secondary: Option<CodexRateLimitWindow>,
    #[serde(default, alias = "planType")]
    pub plan_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexRateLimitWindow {
    #[serde(alias = "usedPercent")]
    pub used_percent: f32,
    #[serde(default, alias = "windowDurationMins", alias = "windowMinutes")]
    pub window_minutes: Option<u64>,
    #[serde(default, alias = "resetsAt")]
    pub resets_at: Option<i64>,
}

impl CodexStatusRecord {
    fn state(&self) -> AgentState {
        // Helpers released before this mapping was corrected wrote Stop as
        // waiting. A completed turn is resting, not awaiting actionable input.
        if self.event == "Stop" && self.state == "waiting" {
            return AgentState::Idle;
        }
        match self.state.as_str() {
            "active" => AgentState::Active,
            "waiting" => AgentState::Waiting,
            "compacting" => AgentState::Compacting,
            _ => AgentState::Idle,
        }
    }

    fn effective_state(&self, last_activity: SystemTime, now: SystemTime) -> AgentState {
        settle_interrupted_activity(self.state(), &self.event, last_activity, now)
    }

    fn last_activity(&self, path: &Path) -> SystemTime {
        DateTime::parse_from_rfc3339(&self.timestamp)
            .map(|date| date.with_timezone(&Utc).into())
            .or_else(|_| fs::metadata(path).and_then(|metadata| metadata.modified()))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

pub struct CodexProvider {
    root: PathBuf,
    archived_sessions_root: Option<PathBuf>,
}

impl CodexProvider {
    pub fn new() -> Self {
        Self {
            root: default_runtime_root(),
            archived_sessions_root: default_archived_sessions_root(),
        }
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self {
            root,
            archived_sessions_root: None,
        }
    }

    #[cfg(test)]
    fn with_roots(root: PathBuf, archived_sessions_root: PathBuf) -> Self {
        Self {
            root,
            archived_sessions_root: Some(archived_sessions_root),
        }
    }

    fn scan_files(&self) -> Result<Vec<PathBuf>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("read Codex runtime directory"),
        };
        Ok(entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect())
    }

    pub fn parse_file(path: &Path) -> Result<CodexStatusRecord> {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
    }

    fn archived_session_ids(&self) -> HashSet<String> {
        self.archived_sessions_root
            .as_deref()
            .and_then(|root| fs::read_dir(root).ok())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| archived_session_id_from_path(&entry.path()))
            .collect()
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentProvider for CodexProvider {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn discover(&self) -> Result<Vec<AgentSnapshot>> {
        let records: Vec<_> = self
            .scan_files()?
            .into_iter()
            .filter_map(|path| match Self::parse_file(&path) {
                Ok(record) => {
                    let last_activity = record.last_activity(&path);
                    Some((record, last_activity))
                }
                Err(error) => {
                    tracing::debug!(%error, "ignore incomplete Codex status write");
                    None
                }
            })
            .collect();
        let mut system = System::new();
        let pids: Vec<_> = records
            .iter()
            .filter(|(record, _)| record.pid > 0)
            .map(|(record, _)| Pid::from_u32(record.pid as u32))
            .collect();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            false,
            ProcessRefreshKind::nothing()
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .with_exe(UpdateKind::OnlyIfNotSet),
        );
        let now = SystemTime::now();
        let archived_session_ids = self.archived_session_ids();
        let mut snapshots = Vec::new();
        for (record, last_activity) in records {
            if archived_session_ids.contains(&record.session_id) {
                continue;
            }
            if now.duration_since(last_activity).unwrap_or_default() > MAX_RECORD_AGE
                || !is_codex_process(&system, record.pid, last_activity)
            {
                continue;
            }
            let cwd = PathBuf::from(&record.cwd);
            snapshots.push(AgentSnapshot {
                id: AgentId::new(self.id(), &record.session_id),
                pid: record.pid,
                project_name: AgentSnapshot::project_name(&cwd),
                cwd,
                state: record.effective_state(last_activity, now),
                context_usage: record.context_used_percentage.and_then(ContextUsage::new),
                provider_usage: record.rate_limits.as_ref().and_then(provider_usage),
                focus_target: FocusTarget {
                    host_pid: record.ppid,
                    uri: Some(format!("codex://threads/{}", record.session_id)),
                    ..FocusTarget::default()
                },
                last_activity,
            });
        }
        snapshots.sort_by_key(|snapshot| snapshot.last_activity);
        Ok(snapshots)
    }

    fn usage(&self) -> Result<Option<ProviderUsage>> {
        Ok(self
            .scan_files()?
            .into_iter()
            .filter_map(|path| {
                let record = Self::parse_file(&path).ok()?;
                let usage = record.rate_limits.as_ref().and_then(provider_usage)?;
                Some((record.last_activity(&path), usage))
            })
            .max_by_key(|(last_activity, _)| *last_activity)
            .map(|(_, usage)| usage))
    }

    fn focus(&self, target: &FocusTarget) -> Result<()> {
        focus::focus(target)
    }

    fn watch(&self) -> Result<ProviderWatcher> {
        fs::create_dir_all(&self.root)?;
        // Coalesce filesystem bursts: the receiver only needs to know that at
        // least one change happened since the previous refresh.
        let (tx, changes) = crossbeam_channel::bounded(1);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok() {
                    let _ = tx.try_send(());
                }
            })?;
        watcher.watch(&self.root, RecursiveMode::NonRecursive)?;
        if let Some(root) = self
            .archived_sessions_root
            .as_deref()
            .filter(|root| root.is_dir())
        {
            watcher.watch(root, RecursiveMode::NonRecursive)?;
        }
        Ok(ProviderWatcher::new(watcher, changes))
    }

    fn runtime_root(&self) -> &Path {
        &self.root
    }
}

fn default_archived_sessions_root() -> Option<PathBuf> {
    directories::UserDirs::new().map(|dirs| dirs.home_dir().join(".codex/archived_sessions"))
}

fn archived_session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let session_id = stem.get(stem.len().checked_sub(36)?..)?;
    (session_id.len() == 36
        && session_id
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-'))
    .then(|| session_id.to_owned())
}

fn provider_usage(rate_limits: &CodexRateLimits) -> Option<ProviderUsage> {
    let limits = [
        ("Fenêtre principale", rate_limits.primary.as_ref()),
        ("Fenêtre secondaire", rate_limits.secondary.as_ref()),
    ]
    .into_iter()
    .filter_map(|(label, window)| {
        let window = window?;
        let resets_at = window
            .resets_at
            .and_then(|timestamp| u64::try_from(timestamp).ok())
            .and_then(|timestamp| {
                SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(timestamp))
            });
        UsageLimit::new(label, window.used_percent, window.window_minutes, resets_at)
    })
    .collect();
    let usage = ProviderUsage {
        limits,
        plan: rate_limits.plan_type.clone(),
        stale: false,
    };
    (!usage.is_empty()).then_some(usage)
}

fn settle_interrupted_activity(
    state: AgentState,
    event: &str,
    last_activity: SystemTime,
    now: SystemTime,
) -> AgentState {
    if state != AgentState::Active || event == "PreToolUse" {
        return state;
    }

    if now.duration_since(last_activity).unwrap_or_default() >= INTERRUPTED_ACTIVITY_GRACE {
        AgentState::Idle
    } else {
        state
    }
}

pub fn default_runtime_root() -> PathBuf {
    directories::ProjectDirs::from("com", "oliv", "AgentsMon")
        .map(|dirs| dirs.data_dir().join("codex"))
        .unwrap_or_else(|| PathBuf::from(".agentsmon/codex"))
}

fn is_codex_process(system: &System, pid: i32, last_activity: SystemTime) -> bool {
    if pid <= 0 {
        return false;
    }
    let Some(process) = system.process(Pid::from_u32(pid as u32)) else {
        return false;
    };
    let process_started = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(process.start_time()))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    if last_activity < process_started {
        return false;
    }
    let name = process.name().to_string_lossy().to_ascii_lowercase();
    let executable = process
        .exe()
        .map(|path| path.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    name.contains("codex")
        || executable.contains("codex")
        || process.cmd().iter().any(|argument| {
            argument
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("codex")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_records_and_maps_states() {
        let record: CodexStatusRecord = serde_json::from_str(
            r#"{"session_id":"s","pid":42,"state":"waiting","timestamp":"2026-07-16T10:00:00Z","cwd":"/tmp/demo","context_used_percentage":56.25}"#,
        )
        .unwrap();
        assert_eq!(record.state(), AgentState::Waiting);
        assert_eq!(record.context_used_percentage, Some(56.25));

        let stopped: CodexStatusRecord = serde_json::from_str(
            r#"{"session_id":"s","pid":42,"state":"waiting","event":"Stop","timestamp":"2026-07-16T10:00:00Z","cwd":"/tmp/demo"}"#,
        )
        .unwrap();
        assert_eq!(stopped.state(), AgentState::Idle);
    }

    #[test]
    fn ignores_corrupt_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.json");
        fs::write(&path, b"{partial").unwrap();
        assert!(CodexProvider::parse_file(&path).is_err());
    }

    #[test]
    fn maps_rate_limit_windows_to_provider_usage() {
        let limits = CodexRateLimits {
            primary: Some(CodexRateLimitWindow {
                used_percent: 24.0,
                window_minutes: Some(300),
                resets_at: Some(1_784_200_000),
            }),
            secondary: Some(CodexRateLimitWindow {
                used_percent: 68.0,
                window_minutes: Some(10_080),
                resets_at: None,
            }),
            plan_type: Some("plus".into()),
        };
        let usage = provider_usage(&limits).unwrap();
        assert_eq!(usage.highest_used_percentage(), Some(68.0));
        assert_eq!(usage.limits[0].window_minutes, Some(300));
        assert_eq!(usage.plan.as_deref(), Some("plus"));
    }

    #[test]
    fn exposes_the_latest_quota_without_a_live_session() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("stopped.json"),
            r#"{"session_id":"stopped","pid":999999,"state":"idle","timestamp":"2026-07-16T10:00:00Z","rate_limits":{"primary":{"used_percent":41.0}}}"#,
        )
        .unwrap();

        let provider = CodexProvider::with_root(directory.path().to_path_buf());
        assert!(provider.discover().unwrap().is_empty());
        assert_eq!(
            provider.usage().unwrap().unwrap().highest_used_percentage(),
            Some(41.0)
        );
    }

    #[test]
    fn recognizes_archived_session_ids_without_reading_transcripts() {
        let path =
            Path::new("rollout-2026-07-16T11-34-13-019f6a46-cf27-7d53-9559-2228c6ab755f.jsonl");
        assert_eq!(
            archived_session_id_from_path(path).as_deref(),
            Some("019f6a46-cf27-7d53-9559-2228c6ab755f")
        );
        assert!(archived_session_id_from_path(Path::new("unrelated.jsonl")).is_none());
    }

    #[test]
    fn archived_sessions_are_discovered_from_filenames() {
        let runtime = tempfile::tempdir().unwrap();
        let archived = tempfile::tempdir().unwrap();
        let session_id = "019f6a46-cf27-7d53-9559-2228c6ab755f";
        fs::write(
            archived
                .path()
                .join(format!("rollout-2026-07-16T11-34-13-{session_id}.jsonl")),
            "content is deliberately never inspected",
        )
        .unwrap();

        let provider =
            CodexProvider::with_roots(runtime.path().to_path_buf(), archived.path().to_path_buf());
        assert!(provider.archived_session_ids().contains(session_id));
    }

    #[test]
    fn settles_active_model_work_when_interrupt_emits_no_stop() {
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        assert_eq!(
            settle_interrupted_activity(
                AgentState::Active,
                "PostToolUse",
                started,
                started + INTERRUPTED_ACTIVITY_GRACE - Duration::from_millis(1),
            ),
            AgentState::Active
        );
        assert_eq!(
            settle_interrupted_activity(
                AgentState::Active,
                "PostToolUse",
                started,
                started + INTERRUPTED_ACTIVITY_GRACE,
            ),
            AgentState::Idle
        );
    }

    #[test]
    fn does_not_expire_an_explicitly_running_tool() {
        let started = SystemTime::UNIX_EPOCH;
        assert_eq!(
            settle_interrupted_activity(
                AgentState::Active,
                "PreToolUse",
                started,
                started + Duration::from_secs(300),
            ),
            AgentState::Active
        );
    }
}
