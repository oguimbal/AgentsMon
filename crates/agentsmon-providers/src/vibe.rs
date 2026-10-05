//! Mistral Vibe's metadata-only hooks. Never open its session transcripts.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use agentsmon_core::{
    AgentId, AgentSnapshot, AgentState, ContextUsage, FocusTarget, ProviderUsage,
};
use anyhow::{Context, Result};
use chrono::DateTime;
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, System};

use crate::process::{find_host, refresh_process_trees};
use crate::{focus, AgentProvider, ProviderWatcher};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VibeStatusRecord {
    pub session_id: String,
    pub pid: i32,
    pub ppid: i32,
    pub state: AgentState,
    pub timestamp: String,
    pub cwd: String,
    pub event: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub context_tokens: Option<u64>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub host_app: Option<String>,
    #[serde(default)]
    pub terminal_session_id: Option<String>,
    #[serde(default)]
    pub tmux_pane: Option<String>,
    #[serde(default)]
    pub tmux_socket: Option<String>,
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("VIBE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            directories::UserDirs::new()
                .map(|dirs| dirs.home_dir().join(".vibe"))
                .unwrap_or_else(|| PathBuf::from(".vibe"))
        })
}

pub fn default_runtime_root() -> PathBuf {
    home_dir().join("agentsmon/sessions")
}

/// Vibe's console scripts may run as Python or as a standalone executable.
/// Only inspect the executable/script position, never arbitrary prompt arguments.
pub fn is_vibe_command(name: &OsStr, exe: Option<&Path>, cmd: &[impl AsRef<OsStr>]) -> bool {
    fn entrypoint(value: &OsStr) -> bool {
        let name = Path::new(value).file_name().unwrap_or(value);
        matches!(
            name.to_str(),
            Some(
                "vibe"
                    | "vibe.exe"
                    | "vibe-acp"
                    | "vibe-acp.exe"
                    | "vibe-app-server"
                    | "vibe-app-server.exe"
            )
        )
    }
    // Vibe 2.25+ uses setproctitle, replacing both the Python name and argv.
    if name == "Vibe CLI"
        || entrypoint(name)
        || exe.is_some_and(|path| entrypoint(path.as_os_str()))
    {
        return true;
    }
    if !name
        .to_string_lossy()
        .to_ascii_lowercase()
        .starts_with("python")
    {
        return false;
    }
    // On macOS sysinfo still reports "Python" after setproctitle, while argv[0]
    // carries Vibe's new title and the original script arguments are gone.
    cmd.first().is_some_and(|arg| arg.as_ref() == "Vibe CLI")
        || cmd.get(1).is_some_and(|arg| entrypoint(arg.as_ref()))
        || (cmd.get(1).is_some_and(|arg| arg.as_ref() == "-m")
            && cmd.get(2).is_some_and(|arg| {
                matches!(
                    arg.as_ref().to_str(),
                    Some("vibe" | "vibe.cli" | "vibe.acp")
                )
            }))
}

pub struct VibeProvider {
    root: PathBuf,
}

impl VibeProvider {
    pub fn new() -> Self {
        Self::with_root(default_runtime_root())
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn parse_file(path: &Path) -> Result<VibeStatusRecord> {
        serde_json::from_slice(&fs::read(path)?).context("parse Vibe status metadata")
    }
}

impl Default for VibeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentProvider for VibeProvider {
    fn id(&self) -> &'static str {
        "vibe"
    }

    fn display_name(&self) -> &'static str {
        "Mistral Vibe"
    }

    fn icon_png(&self) -> Option<&'static [u8]> {
        Some(include_bytes!("../assets/mistral-vibe-16.png"))
    }

    fn accent_rgb(&self) -> Option<u32> {
        // Mistral orange, matching the tinted Vibe mark above.
        Some(0xff7000)
    }

    fn discover(&self) -> Result<Vec<AgentSnapshot>> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("read Vibe runtime directory"),
        };
        let records: Vec<_> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|entry| Self::parse_file(&entry.path()).ok())
            .collect();
        let mut system = System::new();
        let pids: Vec<_> = records
            .iter()
            .flat_map(|record| [record.pid, record.ppid])
            .collect();
        refresh_process_trees(&mut system, &pids);
        let mut live_records: HashMap<i32, (bool, AgentSnapshot)> = HashMap::new();
        for record in records {
            let Some(last_activity) = DateTime::parse_from_rfc3339(&record.timestamp)
                .ok()
                .map(SystemTime::from)
            else {
                continue;
            };
            let Some(process) = (record.pid > 1)
                .then(|| system.process(Pid::from_u32(record.pid as u32)))
                .flatten()
            else {
                continue;
            };
            let started = SystemTime::UNIX_EPOCH + Duration::from_secs(process.start_time());
            if last_activity < started
                || !is_vibe_command(process.name(), process.exe(), process.cmd())
            {
                continue;
            }
            let (host_pid, detected_host) = find_host(&system, record.pid, record.ppid, None);
            let is_live = record.source.as_deref() == Some("app_server");
            let context_usage = context_usage(record.context_tokens, record.context_window);
            let cwd = PathBuf::from(record.cwd);
            let snapshot = AgentSnapshot {
                id: AgentId::new(self.id(), record.session_id),
                pid: record.pid,
                group_dir: None,
                project_name: AgentSnapshot::project_name(&cwd),
                cwd,
                title: None,
                custom_name: None,
                state: record.state,
                context_usage,
                provider_usage: None,
                focus_target: FocusTarget {
                    host_pid,
                    host_app: record.host_app.or(detected_host),
                    terminal_session_id: record.terminal_session_id,
                    tmux_pane: record.tmux_pane,
                    tmux_socket: record.tmux_socket,
                    uri: None,
                },
                last_activity,
            };
            // Validate process identity BEFORE choosing precedence. A live
            // record left by a previous owner of this PID must not hide hooks.
            let replace = live_records
                .get(&record.pid)
                .is_none_or(|(previous_live, previous)| {
                    (is_live && !previous_live)
                        || (is_live == *previous_live && last_activity > previous.last_activity)
                });
            if replace {
                live_records.insert(record.pid, (is_live, snapshot));
            }
        }
        let mut snapshots: Vec<_> = live_records
            .into_values()
            .map(|(_, snapshot)| snapshot)
            .collect();
        snapshots.sort_by_key(|snapshot| snapshot.last_activity);
        Ok(snapshots)
    }

    fn usage(&self) -> Result<Option<ProviderUsage>> {
        Ok(None)
    }

    fn focus(&self, target: &FocusTarget) -> Result<()> {
        focus::focus(target)
    }

    fn watch(&self) -> Result<ProviderWatcher> {
        fs::create_dir_all(&self.root)?;
        let (tx, changes) = crossbeam_channel::bounded(1);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok() {
                    let _ = tx.try_send(());
                }
            })?;
        // FSEvents reports canonical paths, including for a symlinked VIBE_HOME.
        watcher.watch(&fs::canonicalize(&self.root)?, RecursiveMode::NonRecursive)?;
        Ok(ProviderWatcher::new(watcher, changes))
    }

    fn runtime_root(&self) -> &Path {
        &self.root
    }
}

fn context_usage(tokens: Option<u64>, window: Option<u64>) -> Option<ContextUsage> {
    let window = window.filter(|window| *window > 0)?;
    ContextUsage::new((tokens? as f64 / window as f64 * 100.0) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_uses_vibes_compaction_budget_and_preserves_unavailable() {
        for percentage in [0, 1, 50, 99, 100] {
            assert_eq!(
                context_usage(Some(percentage * 1000), Some(100_000))
                    .unwrap()
                    .used_percentage(),
                percentage as f32
            );
        }
        assert_eq!(
            context_usage(Some(200), Some(100))
                .unwrap()
                .used_percentage(),
            100.0
        );
        assert!(context_usage(None, Some(100)).is_none());
        assert!(context_usage(Some(10), None).is_none());
        assert!(context_usage(Some(10), Some(0)).is_none());
    }

    #[test]
    fn recognizes_entrypoints_without_matching_prompt_arguments() {
        for name in [
            "vibe",
            "vibe.exe",
            "vibe-acp",
            "vibe-app-server",
            "Vibe CLI",
        ] {
            assert!(is_vibe_command(OsStr::new(name), None, &[] as &[&str]));
        }
        assert!(is_vibe_command(OsStr::new("Python"), None, &["Vibe CLI"]));
        assert!(is_vibe_command(
            OsStr::new("python3.13"),
            None,
            &["python", "/home/me/.local/bin/vibe"]
        ));
        assert!(is_vibe_command(
            OsStr::new("python3"),
            None,
            &["python3", "-m", "vibe"]
        ));
        assert!(!is_vibe_command(
            OsStr::new("python3"),
            None,
            &["python3", "script.py", "vibe"]
        ));
        assert!(!is_vibe_command(
            OsStr::new("sh"),
            None,
            &["sh", "-c", "vibe"]
        ));
        assert!(!is_vibe_command(
            OsStr::new("agentsmon-helper"),
            None,
            &["agentsmon-helper", "vibe-hook"]
        ));
    }

    #[test]
    fn tolerates_missing_and_partial_records_and_rejects_dead_processes() {
        let dir = tempfile::tempdir().unwrap();
        let provider = VibeProvider::with_root(dir.path().join("sessions"));
        assert!(provider.discover().unwrap().is_empty());
        fs::create_dir_all(provider.runtime_root()).unwrap();
        fs::write(provider.runtime_root().join("broken.json"), "{").unwrap();
        fs::write(provider.runtime_root().join("dead.json"), r#"{"session_id":"s","pid":0,"ppid":0,"state":"active","cwd":"/tmp","event":"pre_tool","timestamp":"2026-09-15T10:00:00Z"}"#).unwrap();
        assert!(provider.discover().unwrap().is_empty());
        assert!(provider.usage().unwrap().is_none());
    }
}
