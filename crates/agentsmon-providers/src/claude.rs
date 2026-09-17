use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

#[cfg(target_os = "macos")]
use std::io::Write;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

use agentsmon_core::{
    AgentId, AgentSnapshot, AgentState, ContextUsage, FocusTarget, ProviderUsage, UsageLimit,
};
#[cfg(target_os = "macos")]
use anyhow::bail;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossbeam_channel::{bounded, Receiver};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, System};

use crate::process::{find_host, refresh_process_trees};
use crate::{claude_runtime, focus, AgentProvider, ProviderDiagnostic};

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
    pub event_timestamp: Option<String>,
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
    #[serde(default)]
    pub title: Option<String>,
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
        // Claude Status 2.x can classify a completed assistant response as a
        // question and leave it waiting indefinitely because it has no Stop
        // hook. Only explicit lifecycle events should represent actionable
        // input; AgentsMon's own hooks cover those events going forward.
        if self.event == "assistant" && self.state == "waiting" {
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
    runtime: claude_runtime::Registry,
    usage_cache: Arc<Mutex<UsageCache>>,
}

impl ClaudeProvider {
    pub fn new() -> Self {
        Self::with_root(default_projects_root())
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self {
            runtime: claude_runtime::Registry::new(root.with_file_name("sessions")),
            root,
            usage_cache: Arc::default(),
        }
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

    fn display_name(&self) -> &'static str {
        "Claude"
    }

    fn icon_png(&self) -> Option<&'static [u8]> {
        Some(include_bytes!("../assets/claude-16.png"))
    }

    fn accent_rgb(&self) -> Option<u32> {
        // Claude terracotta, the fill of the official symbol.
        Some(0xd97757)
    }

    fn discover(&self) -> Result<Vec<AgentSnapshot>> {
        let mut records = prefer_agentsmon_records(
            self.scan_files()?
                .into_iter()
                .filter_map(|file| match Self::parse_file(&file) {
                    Ok(record) => {
                        let last_activity = record.last_activity(&file);
                        Some((file, record, last_activity))
                    }
                    Err(error) => {
                        tracing::debug!(%error, "ignore incomplete cstatus write");
                        None
                    }
                })
                .collect(),
        );
        // Stable order also makes a persistent diagnostic stable when several
        // sessions are affected. The application limits notifications globally.
        records.sort_by(|a, b| a.0.session_id.cmp(&b.0.session_id));
        let mut system = System::new();
        let seed_pids: Vec<_> = records
            .iter()
            .flat_map(|(record, _)| [Some(record.pid), Some(record.ppid), record.focused_app_pid])
            .flatten()
            .collect();
        refresh_process_trees(&mut system, &seed_pids);
        let mut snapshots = Vec::new();
        let mut runtime_issue = None;
        for (record, mut last_activity) in records {
            if !is_claude_process(&system, record.pid, last_activity) {
                continue;
            }
            let cwd = PathBuf::from(&record.cwd);
            let (host_pid, detected_host_app) =
                find_host(&system, record.pid, record.ppid, record.focused_app_pid);
            let mut state = record.state();
            let hook_time = record
                .event_timestamp
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(SystemTime::from)
                .unwrap_or(last_activity);
            let process_started = system
                .process(Pid::from_u32(record.pid as u32))
                .and_then(|p| {
                    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(p.start_time()))
                })
                .unwrap_or(last_activity);
            match self.runtime.read(
                record.pid,
                &record.session_id,
                process_started,
                state,
                hook_time,
                SystemTime::now(),
            ) {
                Ok(Some((runtime_state, timestamp))) => {
                    state = runtime_state;
                    last_activity = last_activity.max(timestamp);
                }
                Ok(None) => {}
                Err(issue) => {
                    runtime_issue.get_or_insert(issue);
                }
            }
            let uri = focus_uri(
                record.host_app.as_deref(),
                detected_host_app.as_deref(),
                &cwd,
                &record.session_id,
            );
            let host_app = record.host_app.or(detected_host_app);
            snapshots.push(AgentSnapshot {
                id: AgentId::new(self.id(), record.session_id),
                pid: record.pid,
                project_name: AgentSnapshot::project_name(&cwd),
                title: record.title,
                custom_name: None,
                cwd,
                state,
                context_usage: record.context_used_percentage.and_then(ContextUsage::new),
                // Provider-wide usage is fetched once from Claude's OAuth
                // endpoint and attached by the application refresh.
                provider_usage: None,
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
        self.runtime.finish_scan(runtime_issue);
        snapshots.sort_by_key(|snapshot| snapshot.last_activity);
        Ok(snapshots)
    }

    fn usage(&self) -> Result<Option<ProviderUsage>> {
        // Runs on the UI thread inside a refresh: never block on the network
        // here. Serve the cache and refetch on a background thread when stale.
        let mut cache = self
            .usage_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Claude usage cache poisoned"))?;
        let fresh = cache
            .fetched_at
            .is_some_and(|instant| instant.elapsed() < Duration::from_secs(60));
        if !fresh && !cache.fetching {
            cache.fetching = true;
            let shared = Arc::clone(&self.usage_cache);
            if let Err(error) = std::thread::Builder::new()
                .name("agentsmon-claude-usage".into())
                .spawn(move || {
                    let fetched = fetch_claude_usage();
                    let Ok(mut cache) = shared.lock() else {
                        return;
                    };
                    cache.fetching = false;
                    cache.fetched_at = Some(Instant::now());
                    match fetched {
                        Ok(usage) => cache.usage = usage,
                        Err(error) => {
                            tracing::debug!(%error, "keep stale Claude usage after refresh failure");
                            if let Some(usage) = cache.usage.as_mut() {
                                usage.stale = true;
                            }
                        }
                    }
                })
            {
                cache.fetching = false;
                tracing::warn!(%error, "spawn Claude usage refresh");
            }
        }
        Ok(cache.usage.clone())
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
        // Registry may not exist on older Claude installations. The regular
        // ten-second refresh still covers its later creation or a lost watch.
        if self.runtime.root.is_dir() {
            if let Err(error) = watcher.watch(&self.runtime.root, RecursiveMode::NonRecursive) {
                tracing::warn!(%error, "Claude session registry watcher unavailable; polling remains enabled");
            }
        }
        Ok(ProviderWatcher::new(watcher, changes))
    }

    fn runtime_root(&self) -> &Path {
        &self.root
    }

    fn diagnostic(&self) -> Option<ProviderDiagnostic> {
        self.runtime.diagnostic()
    }
}

#[derive(Default)]
struct UsageCache {
    fetched_at: Option<Instant>,
    fetching: bool,
    usage: Option<ProviderUsage>,
}

fn prefer_agentsmon_records(
    records: Vec<(PathBuf, CStatusRecord, SystemTime)>,
) -> Vec<(CStatusRecord, SystemTime)> {
    let mut selected = HashMap::<String, (bool, CStatusRecord, SystemTime)>::new();
    for (path, record, last_activity) in records {
        let owned = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".agentsmon.cstatus"));
        let replace =
            selected
                .get(&record.session_id)
                .is_none_or(|(current_owned, _, current_activity)| {
                    (owned && !current_owned)
                        || (owned == *current_owned && last_activity > *current_activity)
                });
        if replace {
            selected.insert(record.session_id.clone(), (owned, record, last_activity));
        }
    }
    selected
        .into_values()
        .map(|(_, record, last_activity)| (record, last_activity))
        .collect()
}

#[derive(Debug, Deserialize)]
struct ClaudeUsageResponse {
    #[serde(default)]
    limits: Vec<ClaudeUsageLimit>,
}

#[derive(Debug, Deserialize)]
struct ClaudeUsageLimit {
    kind: String,
    group: String,
    percent: f32,
    #[serde(default)]
    resets_at: Option<String>,
    #[serde(default)]
    scope: Option<ClaudeUsageScope>,
}

#[derive(Debug, Deserialize)]
struct ClaudeUsageScope {
    #[serde(default)]
    model: Option<ClaudeUsageModel>,
}

#[derive(Debug, Deserialize)]
struct ClaudeUsageModel {
    #[serde(default)]
    display_name: Option<String>,
}

#[cfg(target_os = "macos")]
#[derive(Deserialize)]
struct ClaudeCredentialEnvelope {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: ClaudeOAuthCredential,
}

#[cfg(target_os = "macos")]
#[derive(Deserialize)]
struct ClaudeOAuthCredential {
    #[serde(rename = "accessToken")]
    access_token: String,
}

#[cfg(target_os = "macos")]
fn fetch_claude_usage() -> Result<Option<ProviderUsage>> {
    let credential = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .context("read Claude OAuth credential from macOS Keychain")?;
    if !credential.status.success() {
        bail!("Claude OAuth credential is unavailable");
    }
    let credential: ClaudeCredentialEnvelope =
        serde_json::from_slice(&credential.stdout).context("parse Claude OAuth credential")?;
    if credential.claude_ai_oauth.access_token.is_empty() {
        bail!("Claude OAuth credential has no access token");
    }

    let mut child = Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--max-time",
            "3",
            "--header",
            "@-",
            "https://api.anthropic.com/api/oauth/usage",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start Claude usage request")?;
    {
        let mut stdin = child.stdin.take().context("open Claude usage request")?;
        writeln!(
            stdin,
            "Authorization: Bearer {}",
            credential.claude_ai_oauth.access_token
        )?;
        writeln!(stdin, "anthropic-beta: oauth-2025-04-20")?;
        writeln!(stdin, "accept: application/json")?;
    }
    let output = child.wait_with_output().context("wait for Claude usage")?;
    if !output.status.success() {
        bail!("Claude usage endpoint returned {}", output.status);
    }
    let response: ClaudeUsageResponse =
        serde_json::from_slice(&output.stdout).context("parse Claude usage response")?;
    Ok(provider_usage_from_api(response))
}

#[cfg(not(target_os = "macos"))]
fn fetch_claude_usage() -> Result<Option<ProviderUsage>> {
    Ok(None)
}

fn provider_usage_from_api(response: ClaudeUsageResponse) -> Option<ProviderUsage> {
    let limits = response
        .limits
        .into_iter()
        .filter_map(|limit| {
            let (label, window_minutes) = match (limit.kind.as_str(), limit.group.as_str()) {
                ("session", _) => ("Quota 5 heures".to_string(), Some(300)),
                ("weekly_all", _) => ("Quota 7 jours".to_string(), Some(10_080)),
                ("weekly_scoped", _) => {
                    let scope = limit
                        .scope
                        .and_then(|scope| scope.model)
                        .and_then(|model| model.display_name)
                        .unwrap_or_else(|| "modèle".into());
                    (format!("Quota 7 jours · {scope}"), Some(10_080))
                }
                _ => return None,
            };
            let resets_at = limit
                .resets_at
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc).into());
            UsageLimit::new(label, limit.percent, window_minutes, resets_at)
        })
        .collect();
    let usage = ProviderUsage {
        limits,
        plan: None,
        stale: false,
    };
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

fn focus_uri(
    reported_host: Option<&str>,
    detected_host: Option<&str>,
    cwd: &Path,
    session_id: &str,
) -> Option<String> {
    let reported = reported_host.map(str::to_ascii_lowercase);
    let detected = detected_host.map(str::to_ascii_lowercase);
    let any_contains = |needle: &str| {
        reported
            .as_deref()
            .is_some_and(|host| host.contains(needle))
            || detected
                .as_deref()
                .is_some_and(|host| host.contains(needle))
    };
    let directory_uri = |scheme: &str| {
        let file_url = url::Url::from_directory_path(cwd).ok()?.to_string();
        Some(file_url.replacen("file://", scheme, 1))
    };
    // Cursor's integrated terminal also reports TERM_PROGRAM=vscode, so the
    // detected executable path must win before any VS Code match.
    if any_contains("cursor") {
        return directory_uri("cursor://file");
    }
    if any_contains("visual studio code") || any_contains("vscode") {
        return directory_uri("vscode://file");
    }
    if any_contains("claude.app") {
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
            r#"{"session_id":"s","pid":42,"state":"compacting","timestamp":"2026-07-16T10:00:00Z","cwd":"/tmp/demo","context_used_percentage":68.5,"title":"Fix the build"}"#,
        ).unwrap();
        assert_eq!(record.state(), AgentState::Compacting);
        assert_eq!(record.context_used_percentage, Some(68.5));
        assert_eq!(record.title.as_deref(), Some("Fix the build"));
    }

    #[test]
    fn maps_authoritative_oauth_usage_windows() {
        let response: ClaudeUsageResponse = serde_json::from_str(
            r#"{
                "limits": [
                    {"kind":"session","group":"session","percent":3,"resets_at":"2026-07-24T18:00:00Z","scope":null},
                    {"kind":"weekly_all","group":"weekly","percent":20,"resets_at":"2026-07-24T21:00:00Z","scope":null},
                    {"kind":"weekly_scoped","group":"weekly","percent":36,"resets_at":"2026-07-24T21:00:00Z","scope":{"model":{"display_name":"Fable"}}}
                ]
            }"#,
        )
        .unwrap();
        let usage = provider_usage_from_api(response).unwrap();

        assert_eq!(usage.limits.len(), 3);
        assert_eq!(usage.limits[0].label, "Quota 5 heures");
        assert_eq!(usage.limits[0].used_percentage(), 3.0);
        assert_eq!(usage.limits[1].label, "Quota 7 jours");
        assert_eq!(usage.limits[1].used_percentage(), 20.0);
        assert_eq!(usage.limits[2].label, "Quota 7 jours · Fable");
        assert_eq!(usage.limits[2].used_percentage(), 36.0);
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
    fn legacy_assistant_question_is_a_completed_turn() {
        let record: CStatusRecord = serde_json::from_str(
            r#"{"session_id":"s","pid":42,"state":"waiting","activity":"question","event":"assistant","timestamp":"2026-07-16T10:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(record.state(), AgentState::Idle);
    }

    #[test]
    fn agentsmon_record_wins_over_a_later_compatible_plugin_write() {
        let compatible: CStatusRecord = serde_json::from_str(
            r#"{"session_id":"s","pid":42,"state":"active","activity":"Bash","event":"assistant","timestamp":"2026-07-16T10:00:02Z"}"#,
        )
        .unwrap();
        let owned: CStatusRecord = serde_json::from_str(
            r#"{"session_id":"s","pid":42,"state":"waiting","activity":"input","event":"PermissionRequest","timestamp":"2026-07-16T10:00:01Z"}"#,
        )
        .unwrap();
        let selected = prefer_agentsmon_records(vec![
            (
                PathBuf::from("/tmp/s.cstatus"),
                compatible,
                SystemTime::UNIX_EPOCH + Duration::from_secs(2),
            ),
            (
                PathBuf::from("/tmp/s.agentsmon.cstatus"),
                owned,
                SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            ),
        ]);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].0.state(), AgentState::Waiting);
        assert_eq!(selected[0].0.event, "PermissionRequest");
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
            None,
            Some("/Applications/Visual Studio Code.app/Contents/MacOS/Electron"),
            cwd,
            "session",
        )
        .unwrap();
        assert_eq!(vscode, "vscode://file/tmp/project%20with%20spaces/");
        // VSCode's integrated terminal reports TERM_PROGRAM=vscode.
        assert_eq!(
            focus_uri(Some("vscode"), None, cwd, "session").as_deref(),
            Some("vscode://file/tmp/project%20with%20spaces/")
        );
        // Cursor reports TERM_PROGRAM=vscode too; the detected executable wins.
        assert_eq!(
            focus_uri(
                Some("vscode"),
                Some("/Applications/Cursor.app/Contents/MacOS/Cursor"),
                cwd,
                "session"
            )
            .as_deref(),
            Some("cursor://file/tmp/project%20with%20spaces/")
        );
        assert_eq!(
            focus_uri(
                None,
                Some("/Applications/Claude.app/Contents/MacOS/Claude"),
                cwd,
                "session-id"
            )
            .as_deref(),
            Some("claude://resume?session=session-id")
        );
        assert!(focus_uri(Some("Warp"), None, cwd, "session").is_none());
    }
}
