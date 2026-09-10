use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
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
const LIFECYCLE_TAIL_BYTES: u64 = 512 * 1024;
// A permission hook alone does not establish a persistent need for attention.
// Suppress brief requests before they reach either the panels or notifications.
const PERMISSION_ATTENTION_GRACE: Duration = Duration::from_secs(3);

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
    pub transcript_path: Option<PathBuf>,
    #[serde(default)]
    pub context_used_percentage: Option<f32>,
    #[serde(default)]
    pub rate_limits: Option<CodexRateLimits>,
    #[serde(default)]
    pub title: Option<String>,
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
        // Silence is normal while the model reasons. Only an explicit lifecycle
        // event newer than the hook can supersede it (including waiting/tools).
        self.transcript_path
            .as_deref()
            .and_then(|path| latest_lifecycle(path).ok().flatten())
            .filter(|event| event.timestamp > last_activity)
            .map_or_else(
                || {
                    if self.state() == AgentState::Waiting
                        && self.event == "PermissionRequest"
                        && now.duration_since(last_activity).unwrap_or_default()
                            < PERMISSION_ATTENTION_GRACE
                    {
                        AgentState::Active
                    } else {
                        self.state()
                    }
                },
                |event| event.state,
            )
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
                title: record.title.clone(),
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

/// Decode only the envelope timestamp and event discriminator. Serde skips all
/// other fields, including final answers carried by task_complete. Never use a
/// serde_json::Value here: it would deserialize conversation content.
#[derive(Deserialize)]
struct LifecycleEnvelope {
    timestamp: String,
    #[serde(rename = "type")]
    kind: String,
    payload: LifecyclePayload,
}

#[derive(Deserialize)]
struct LifecyclePayload {
    #[serde(rename = "type")]
    kind: String,
}

struct LifecycleEvent {
    timestamp: SystemTime,
    state: AgentState,
}

fn latest_lifecycle(path: &Path) -> std::io::Result<Option<LifecycleEvent>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(LIFECYCLE_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::new();
    // Bound the read even if Codex appends while we inspect the file.
    file.take(length - start).read_to_end(&mut tail)?;
    let mut lines = tail.split(|byte| *byte == b'\n');
    if start > 0 {
        let _ = lines.next(); // The first line may be truncated.
    }
    // A writer may be midway through a JSON record; wait for its newline.
    let _ = lines.next_back();
    for line in lines.rev() {
        let Ok(envelope) = serde_json::from_slice::<LifecycleEnvelope>(line) else {
            continue;
        };
        if envelope.kind != "event_msg" {
            continue;
        }
        let state = match envelope.payload.kind.as_str() {
            "task_started" => AgentState::Active,
            "task_complete" | "turn_aborted" => AgentState::Idle,
            _ => continue,
        };
        let Ok(timestamp) = DateTime::parse_from_rfc3339(&envelope.timestamp) else {
            continue;
        };
        return Ok(Some(LifecycleEvent {
            timestamp: timestamp.with_timezone(&Utc).into(),
            state,
        }));
    }
    Ok(None)
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
            r#"{"session_id":"s","pid":42,"state":"waiting","timestamp":"2026-07-16T10:00:00Z","cwd":"/tmp/demo","context_used_percentage":56.25,"title":"Automatiser la signature"}"#,
        )
        .unwrap();
        assert_eq!(record.state(), AgentState::Waiting);
        assert_eq!(record.context_used_percentage, Some(56.25));
        assert_eq!(record.title.as_deref(), Some("Automatiser la signature"));

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

    fn active_record(path: Option<PathBuf>) -> CodexStatusRecord {
        let mut record: CodexStatusRecord = serde_json::from_str(
            r#"{"session_id":"s","pid":42,"state":"active","event":"PostToolUse","timestamp":"2026-09-10T10:00:00Z"}"#,
        ).unwrap();
        record.transcript_path = path;
        record
    }

    fn lifecycle(kind: &str, timestamp: &str) -> String {
        format!(r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"{kind}"}}}}"#)
            + "\n"
    }

    #[test]
    fn permission_requests_need_to_persist_before_attracting_attention() {
        let mut record = active_record(None);
        record.state = "waiting".into();
        record.event = "PermissionRequest".into();
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        for elapsed in [
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(2),
            PERMISSION_ATTENTION_GRACE - Duration::from_millis(1),
        ] {
            assert_eq!(
                record.effective_state(started, started + elapsed),
                AgentState::Active
            );
        }
        // No second hook is needed: the periodic discovery reveals a lasting wait.
        for elapsed in [PERMISSION_ATTENTION_GRACE, Duration::from_secs(30)] {
            assert_eq!(
                record.effective_state(started, started + elapsed),
                AgentState::Waiting
            );
        }
        // A future timestamp should not bypass the grace period.
        assert_eq!(
            record.effective_state(started, started - Duration::from_secs(1)),
            AgentState::Active
        );
    }

    #[test]
    fn a_resolved_permission_request_never_becomes_a_delayed_alert() {
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut record = active_record(None);
        record.state = "waiting".into();
        record.event = "PermissionRequest".into();
        assert_eq!(
            record.effective_state(started, started + Duration::from_secs(1)),
            AgentState::Active
        );
        // Discovery reads the latest hook, so nothing is queued to turn orange later.
        for (event, state, expected) in [
            ("PreToolUse", "active", AgentState::Active),
            ("PostToolUse", "active", AgentState::Active),
            ("Stop", "idle", AgentState::Idle),
        ] {
            record.event = event.into();
            record.state = state.into();
            assert_eq!(
                record.effective_state(
                    started + Duration::from_secs(2),
                    started + Duration::from_secs(30)
                ),
                expected
            );
        }
        // A subsequent request gets its own full grace period.
        record.event = "PermissionRequest".into();
        record.state = "waiting".into();
        assert_eq!(
            record.effective_state(
                started + Duration::from_secs(30),
                started + Duration::from_secs(31)
            ),
            AgentState::Active
        );
    }

    #[test]
    fn attention_grace_does_not_delay_other_states_or_terminal_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let mut record = active_record(Some(path.clone()));
        let started = record.last_activity(&path);
        for (event, state, expected) in [
            ("OtherInputRequest", "waiting", AgentState::Waiting),
            ("PreCompact", "compacting", AgentState::Compacting),
            ("Stop", "waiting", AgentState::Idle),
        ] {
            record.event = event.into();
            record.state = state.into();
            assert_eq!(record.effective_state(started, started), expected);
        }
        record.event = "PermissionRequest".into();
        record.state = "waiting".into();
        for event in ["task_complete", "turn_aborted"] {
            fs::write(&path, lifecycle(event, "2026-09-10T10:00:01Z")).unwrap();
            assert_eq!(
                record.effective_state(started, started + Duration::from_secs(2)),
                AgentState::Idle
            );
        }
    }

    #[test]
    fn long_thinking_does_not_expire_without_a_terminal_event() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        fs::write(&path, lifecycle("task_started", "2026-09-10T09:59:59Z")).unwrap();
        let record = active_record(Some(path.clone()));
        let last_activity = record.last_activity(&path);
        assert_eq!(
            record.effective_state(last_activity, last_activity + Duration::from_secs(3600)),
            AgentState::Active
        );
        // Old helpers and unavailable transcripts also must not equate silence
        // with completion. Process liveness is checked separately by discover.
        assert_eq!(
            active_record(None)
                .effective_state(last_activity, last_activity + Duration::from_secs(3600)),
            AgentState::Active
        );
        fs::remove_file(&path).unwrap();
        assert_eq!(
            record.effective_state(last_activity, last_activity + Duration::from_secs(3600)),
            AgentState::Active
        );
    }

    #[test]
    fn terminal_events_recover_a_missing_stop_even_during_tools_or_compaction() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        for terminal in ["task_complete", "turn_aborted"] {
            fs::write(&path, lifecycle(terminal, "2026-09-10T10:30:00Z")).unwrap();
            for (state, event) in [
                ("active", "PostToolUse"),
                ("active", "PreToolUse"),
                ("waiting", "PermissionRequest"),
                ("compacting", "PreCompact"),
            ] {
                let mut record = active_record(Some(path.clone()));
                record.state = state.into();
                record.event = event.into();
                assert_eq!(
                    record.effective_state(
                        record.last_activity(&path),
                        record.last_activity(&path) + Duration::from_secs(3600)
                    ),
                    AgentState::Idle
                );
            }
        }
    }

    #[test]
    fn old_completion_cannot_override_a_new_prompt_or_permission_request() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        fs::write(&path, lifecycle("task_complete", "2026-09-10T09:59:59Z")).unwrap();
        let mut record = active_record(Some(path.clone()));
        assert_eq!(
            record.effective_state(
                record.last_activity(&path),
                record.last_activity(&path) + Duration::from_secs(3600)
            ),
            AgentState::Active
        );
        record.state = "waiting".into();
        fs::write(&path, lifecycle("task_started", "2026-09-10T09:59:59Z")).unwrap();
        assert_eq!(
            record.effective_state(
                record.last_activity(&path),
                record.last_activity(&path) + Duration::from_secs(3600)
            ),
            AgentState::Waiting
        );
        fs::write(&path, lifecycle("task_started", "2026-09-10T10:01:00Z")).unwrap();
        assert_eq!(
            record.effective_state(
                record.last_activity(&path),
                record.last_activity(&path) + Duration::from_secs(3600)
            ),
            AgentState::Active
        );
    }

    #[test]
    fn ignores_message_content_and_partial_records_and_uses_the_latest_turn() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let content = lifecycle("task_complete", "2026-09-10T10:01:00Z")
            + &lifecycle("task_started", "2026-09-10T10:02:00Z")
            + r#"{"timestamp":"2026-09-10T10:03:00Z","type":"response_item","payload":{"type":"task_complete","content":"private"}}"#
            + "\n"
            + r#"{"timestamp":"2026-09-10T10:04:00Z","type":"event_msg","payload":{"type":"agent_message","message":"task_complete"}}"#
            + "\n"
            + lifecycle("turn_aborted", "2026-09-10T10:05:00Z").trim_end();
        fs::write(&path, content).unwrap();
        assert_eq!(
            latest_lifecycle(&path).unwrap().unwrap().state,
            AgentState::Active
        );
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        assert_eq!(
            latest_lifecycle(&path).unwrap().unwrap().state,
            AgentState::Idle
        );
    }

    #[test]
    fn handles_a_tail_starting_in_the_middle_of_a_large_record() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        let content = "x".repeat(LIFECYCLE_TAIL_BYTES as usize + 100)
            + "\n"
            + &lifecycle("turn_aborted", "2026-09-10T10:01:00Z");
        fs::write(&path, content).unwrap();
        assert_eq!(
            latest_lifecycle(&path).unwrap().unwrap().state,
            AgentState::Idle
        );
    }
}
