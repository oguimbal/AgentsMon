use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use agentsmon_core::{
    AgentId, AgentSnapshot, AgentState, ContextUsage, FocusTarget, ProviderUsage, UsageLimit,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossbeam_channel::{bounded, Receiver};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::{focus, AgentProvider};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CStatusRecord {
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
    pub rate_limits: Option<ClaudeRateLimits>,
    #[serde(default)]
    pub host_app: Option<String>,
    #[serde(default)]
    pub terminal_session_id: Option<String>,
    #[serde(default)]
    pub tmux_pane: Option<String>,
    #[serde(default)]
    pub tmux_socket: Option<String>,
    #[serde(default)]
    pub focused_app_pid: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeRateLimits {
    #[serde(default)]
    pub five_hour: Option<ClaudeRateLimitWindow>,
    #[serde(default)]
    pub seven_day: Option<ClaudeRateLimitWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeRateLimitWindow {
    pub used_percentage: f32,
    #[serde(default)]
    pub resets_at: Option<i64>,
}

impl CStatusRecord {
    pub fn state(&self) -> AgentState {
        // Preserve compatibility with records written before Stop represented
        // a resting session. Background work is stored as active and unaffected.
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

    fn last_activity(&self, file: &Path) -> SystemTime {
        DateTime::parse_from_rfc3339(&self.timestamp)
            .map(|date| date.with_timezone(&Utc).into())
            .or_else(|_| fs::metadata(file).and_then(|metadata| metadata.modified()))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

pub struct ProviderWatcher {
    _watcher: RecommendedWatcher,
    pub changes: Receiver<()>,
}

impl ProviderWatcher {
    pub(crate) fn new(watcher: RecommendedWatcher, changes: Receiver<()>) -> Self {
        Self {
            _watcher: watcher,
            changes,
        }
    }
}

pub struct ClaudeProvider {
    root: PathBuf,
}

impl ClaudeProvider {
    pub fn new() -> Self {
        Self {
            root: default_projects_root(),
        }
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    fn scan_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        let projects = match fs::read_dir(&self.root) {
            Ok(projects) => projects,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(files),
            Err(error) => return Err(error).context("read Claude projects directory"),
        };
        for project in projects.flatten() {
            if !project.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            for entry in fs::read_dir(project.path()).into_iter().flatten().flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "cstatus")
                {
                    files.push(path);
                }
            }
        }
        Ok(files)
    }

    pub fn parse_file(path: &Path) -> Result<CStatusRecord> {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
    }
}

impl Default for ClaudeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentProvider for ClaudeProvider {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn discover(&self) -> Result<Vec<AgentSnapshot>> {
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .with_exe(UpdateKind::OnlyIfNotSet),
        );
        let mut snapshots = Vec::new();
        for file in self.scan_files()? {
            let record = match Self::parse_file(&file) {
                Ok(record) => record,
                Err(error) => {
                    tracing::debug!(%error, "ignore incomplete cstatus write");
                    continue;
                }
            };
            let last_activity = record.last_activity(&file);
            if !is_claude_process(&system, record.pid, last_activity) {
                continue;
            }
            let cwd = PathBuf::from(&record.cwd);
            let (host_pid, detected_host_app) =
                find_host(&system, record.pid, record.ppid, record.focused_app_pid);
            let state = record.state();
            let host_app = record.host_app.or(detected_host_app);
            let uri = focus_uri(&host_app, &cwd, &record.session_id);
            snapshots.push(AgentSnapshot {
                id: AgentId::new(self.id(), record.session_id),
                pid: record.pid,
                project_name: AgentSnapshot::project_name(&cwd),
                cwd,
                state,
                context_usage: record.context_used_percentage.and_then(ContextUsage::new),
                provider_usage: record.rate_limits.as_ref().and_then(provider_usage),
                focus_target: FocusTarget {
                    host_pid,
                    host_app,
                    terminal_session_id: record.terminal_session_id,
                    tmux_pane: record.tmux_pane,
                    tmux_socket: record.tmux_socket,
                    uri,
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
        // A watcher notification only means "refresh needed". Keep at most one
        // pending signal so a burst of writes cannot turn into a long series of
        // expensive process-table scans.
        let (tx, changes) = bounded(1);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok() {
                    let _ = tx.try_send(());
                }
            })?;
        watcher.watch(&self.root, RecursiveMode::Recursive)?;
        Ok(ProviderWatcher::new(watcher, changes))
    }

    fn runtime_root(&self) -> &Path {
        &self.root
    }
}

fn provider_usage(rate_limits: &ClaudeRateLimits) -> Option<ProviderUsage> {
    let limits = [
        ("Quota 5 heures", Some(300), rate_limits.five_hour.as_ref()),
        (
            "Quota 7 jours",
            Some(10_080),
            rate_limits.seven_day.as_ref(),
        ),
    ]
    .into_iter()
    .filter_map(|(label, window_minutes, window)| {
        let window = window?;
        let resets_at = window
            .resets_at
            .and_then(|timestamp| u64::try_from(timestamp).ok())
            .and_then(|timestamp| {
                SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(timestamp))
            });
        UsageLimit::new(label, window.used_percentage, window_minutes, resets_at)
    })
    .collect();
    let usage = ProviderUsage { limits, plan: None };
    (!usage.is_empty()).then_some(usage)
}

pub fn default_projects_root() -> PathBuf {
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().join(".claude/projects"))
        .unwrap_or_else(|| PathBuf::from(".claude/projects"))
}

fn is_claude_process(system: &System, pid: i32, last_activity: SystemTime) -> bool {
    if pid <= 0 {
        return false;
    }
    let Some(process) = system.process(Pid::from_u32(pid as u32)) else {
        return false;
    };
    let process_started = SystemTime::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(process.start_time()))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    if last_activity < process_started {
        return false;
    }
    let name = process.name().to_string_lossy().to_ascii_lowercase();
    let executable = process
        .exe()
        .map(|path| path.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains("claude") || executable.contains("claude") {
        return true;
    }
    (name == "node" || name == "bun")
        && process.cmd().iter().any(|argument| {
            argument
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("claude")
        })
}

fn find_host(
    system: &System,
    pid: i32,
    reported_parent: i32,
    focused_app_pid: Option<i32>,
) -> (i32, Option<String>) {
    let start = (reported_parent > 1)
        .then_some(reported_parent)
        .filter(|parent| system.process(Pid::from_u32(*parent as u32)).is_some())
        .unwrap_or(pid.max(0));
    let mut current = Pid::from_u32(start as u32);
    let mut candidate = pid;
    for _ in 0..10 {
        let Some(process) = system.process(current) else {
            break;
        };
        candidate = current.as_u32() as i32;
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        let executable = process
            .exe()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let searchable = format!("{name} {}", executable.to_ascii_lowercase());
        if [
            "terminal.app",
            "iterm2",
            "warp.app",
            "ghostty",
            "visual studio code.app",
            "zed.app",
            "cursor.app",
            "windowsterminal",
            "claude.app/contents/macos/claude",
        ]
        .iter()
        .any(|host| searchable.contains(host))
        {
            return (candidate, Some(executable));
        }
        let Some(parent) = process.parent() else {
            break;
        };
        current = parent;
    }
    if let Some(focused) = focused_app_pid.filter(|pid| *pid > 1) {
        if let Some(process) = system.process(Pid::from_u32(focused as u32)) {
            let executable = process
                .exe()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|| process.name().to_string_lossy().into_owned());
            return (focused, Some(executable));
        }
    }
    (candidate, None)
}

fn focus_uri(host_app: &Option<String>, cwd: &Path, session_id: &str) -> Option<String> {
    let host = host_app.as_deref()?.to_ascii_lowercase();
    if host.contains("visual studio code") {
        let file_url = url::Url::from_directory_path(cwd).ok()?.to_string();
        return Some(file_url.replacen("file://", "vscode://file", 1));
    }
    if host.contains("claude.app") {
        return Some(format!("claude://resume?session={session_id}"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compatible_record_and_maps_states() {
        let record: CStatusRecord = serde_json::from_str(
            r#"{"session_id":"s","pid":42,"state":"compacting","timestamp":"2026-07-16T10:00:00Z","cwd":"/tmp/demo","context_used_percentage":68.5}"#,
        ).unwrap();
        assert_eq!(record.state(), AgentState::Compacting);
        assert_eq!(record.context_used_percentage, Some(68.5));
    }

    #[test]
    fn maps_subscription_rate_limits() {
        let limits = ClaudeRateLimits {
            five_hour: Some(ClaudeRateLimitWindow {
                used_percentage: 35.0,
                resets_at: Some(1_784_200_000),
            }),
            seven_day: Some(ClaudeRateLimitWindow {
                used_percentage: 74.0,
                resets_at: None,
            }),
        };
        let usage = provider_usage(&limits).unwrap();
        assert_eq!(usage.highest_used_percentage(), Some(74.0));
        assert_eq!(usage.limits[0].window_minutes, Some(300));
        assert_eq!(usage.limits[1].window_minutes, Some(10_080));
    }

    #[test]
    fn exposes_the_latest_quota_without_a_live_session() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        fs::create_dir(&project).unwrap();
        fs::write(
            project.join("stopped.cstatus"),
            r#"{"session_id":"stopped","pid":999999,"state":"idle","timestamp":"2026-07-16T10:00:00Z","rate_limits":{"five_hour":{"used_percentage":37.0}}}"#,
        )
        .unwrap();

        let provider = ClaudeProvider::with_root(directory.path().to_path_buf());
        assert!(provider.discover().unwrap().is_empty());
        assert_eq!(
            provider.usage().unwrap().unwrap().highest_used_percentage(),
            Some(37.0)
        );
    }

    #[test]
    fn unknown_state_is_idle() {
        let mut record: CStatusRecord = serde_json::from_str(
            r#"{"session_id":"s","pid":42,"state":"future","timestamp":"2026-07-16T10:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(record.state(), AgentState::Idle);
        record.state = "waiting".into();
        assert_eq!(record.state(), AgentState::Waiting);
    }

    #[test]
    fn active_state_requires_an_explicit_lifecycle_event_to_end() {
        let record: CStatusRecord = serde_json::from_str(
            r#"{"session_id":"s","pid":42,"state":"active","event":"UserPromptSubmit","timestamp":"2026-07-16T10:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(record.state(), AgentState::Active);
    }

    #[test]
    fn rejects_partial_and_corrupted_files_without_panicking() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.cstatus");
        fs::write(&path, b"{\"session_id\":").unwrap();
        assert!(ClaudeProvider::parse_file(&path).is_err());
        fs::write(&path, b"not-json").unwrap();
        assert!(ClaudeProvider::parse_file(&path).is_err());
    }

    #[test]
    fn builds_supported_desktop_focus_uris() {
        let cwd = Path::new("/tmp/project with spaces");
        let vscode = focus_uri(
            &Some("/Applications/Visual Studio Code.app/Contents/MacOS/Electron".into()),
            cwd,
            "session",
        )
        .unwrap();
        assert_eq!(vscode, "vscode://file/tmp/project%20with%20spaces/");
        assert_eq!(
            focus_uri(
                &Some("/Applications/Claude.app/Contents/MacOS/Claude".into()),
                cwd,
                "session-id"
            )
            .as_deref(),
            Some("claude://resume?session=session-id")
        );
        assert!(focus_uri(&Some("Warp".into()), cwd, "session").is_none());
    }
}
