use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StatusRecord {
    session_id: String,
    pid: i32,
    ppid: i32,
    state: String,
    activity: String,
    timestamp: String,
    /// Last lifecycle hook, unaffected by status-line heartbeat writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    event_timestamp: Option<String>,
    cwd: String,
    event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_used_percentage: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rate_limits: Option<ClaudeRateLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tmux_pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tmux_socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    focused_app_pid: Option<i32>,
    /// Conversation title assigned by Claude Code or chosen with `/rename`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeRateLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<ClaudeRateLimitWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<ClaudeRateLimitWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeRateLimitWindow {
    pub used_percentage: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

pub struct RecordUpdate<'a> {
    pub session_id: &'a str,
    pub pid: i32,
    pub ppid: i32,
    pub state: &'a str,
    pub activity: String,
    pub cwd: &'a str,
    pub event: &'a str,
    pub context_percentage: Option<f32>,
    pub rate_limits: Option<ClaudeRateLimits>,
    pub title: Option<String>,
}

pub fn path_from_transcript(transcript: &str, session_id: &str) -> Result<PathBuf> {
    let parent = Path::new(transcript)
        .parent()
        .context("transcript has no parent")?;
    Ok(parent.join(format!("{session_id}.agentsmon.cstatus")))
}

pub fn merge_record(path: &Path, update: RecordUpdate<'_>) -> Result<()> {
    let parent = path.parent().context("cstatus path has no parent")?;
    fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("cstatus.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    lock.lock_exclusive()?;

    let previous = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<StatusRecord>(&bytes).ok());
    let environment = EnvironmentMetadata::current();
    let now = Utc::now().to_rfc3339();
    let record = StatusRecord {
        session_id: update.session_id.into(),
        pid: if update.pid > 0 {
            update.pid
        } else {
            previous.as_ref().map_or(0, |r| r.pid)
        },
        ppid: if update.ppid > 0 {
            update.ppid
        } else {
            previous.as_ref().map_or(0, |r| r.ppid)
        },
        state: if update.state.is_empty() {
            previous
                .as_ref()
                .map_or("idle", |r| r.state.as_str())
                .into()
        } else {
            update.state.into()
        },
        activity: if update.activity.is_empty() && update.event.is_empty() {
            previous.as_ref().map_or("", |r| r.activity.as_str()).into()
        } else {
            update.activity
        },
        timestamp: now.clone(),
        event_timestamp: if update.event.is_empty() {
            previous.as_ref().map(|r| {
                r.event_timestamp
                    .clone()
                    .unwrap_or_else(|| r.timestamp.clone())
            })
        } else {
            Some(now)
        },
        cwd: if update.cwd.is_empty() {
            previous.as_ref().map_or("", |r| r.cwd.as_str()).into()
        } else {
            update.cwd.into()
        },
        event: if update.event.is_empty() {
            previous.as_ref().map_or("", |r| r.event.as_str()).into()
        } else {
            update.event.into()
        },
        context_used_percentage: update
            .context_percentage
            .or_else(|| previous.as_ref().and_then(|r| r.context_used_percentage)),
        rate_limits: merge_rate_limits(
            update.rate_limits,
            previous.as_ref().and_then(|r| r.rate_limits.clone()),
        ),
        host_app: environment
            .host_app
            .or_else(|| previous.as_ref().and_then(|r| r.host_app.clone())),
        terminal_session_id: environment.terminal_session_id.or_else(|| {
            previous
                .as_ref()
                .and_then(|r| r.terminal_session_id.clone())
        }),
        tmux_pane: environment
            .tmux_pane
            .or_else(|| previous.as_ref().and_then(|r| r.tmux_pane.clone())),
        tmux_socket: environment
            .tmux_socket
            .or_else(|| previous.as_ref().and_then(|r| r.tmux_socket.clone())),
        focused_app_pid: if update.state == "active" {
            crate::foreground::application_pid()
                .or_else(|| previous.as_ref().and_then(|r| r.focused_app_pid))
        } else {
            previous.as_ref().and_then(|r| r.focused_app_pid)
        },
        title: update
            .title
            .or_else(|| previous.as_ref().and_then(|r| r.title.clone())),
    };

    let temp = path.with_extension(format!("cstatus.tmp.{}", std::process::id()));
    let mut file = File::create(&temp)?;
    serde_json::to_writer(&mut file, &record)?;
    file.sync_all()?;
    replace_atomically(&temp, path)?;
    FileExt::unlock(&lock)?;
    Ok(())
}

fn merge_rate_limits(
    update: Option<ClaudeRateLimits>,
    previous: Option<ClaudeRateLimits>,
) -> Option<ClaudeRateLimits> {
    match (update, previous) {
        (Some(update), Some(previous)) => Some(ClaudeRateLimits {
            five_hour: update.five_hour.or(previous.five_hour),
            seven_day: update.seven_day.or(previous.seven_day),
        }),
        (update, previous) => update.or(previous),
    }
}

pub(crate) struct EnvironmentMetadata {
    pub host_app: Option<String>,
    pub terminal_session_id: Option<String>,
    pub tmux_pane: Option<String>,
    pub tmux_socket: Option<String>,
}

impl EnvironmentMetadata {
    pub(crate) fn current() -> Self {
        let terminal_session_id = std::env::var("ITERM_SESSION_ID")
            .ok()
            .or_else(|| std::env::var("WT_SESSION").ok());
        let terminal_name = std::env::var("LC_TERMINAL")
            .ok()
            .or_else(|| std::env::var("TERM_PROGRAM").ok());
        let host_app = if std::env::var_os("ITERM_SESSION_ID").is_some()
            || terminal_name
                .as_deref()
                .is_some_and(|name| name.to_ascii_lowercase().contains("iterm"))
        {
            Some("iTerm2".into())
        } else if std::env::var_os("WT_SESSION").is_some() {
            Some("WindowsTerminal".into())
        } else if std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some() {
            Some("Ghostty".into())
        } else {
            terminal_name
        };
        Self {
            host_app,
            terminal_session_id,
            tmux_pane: std::env::var("TMUX_PANE").ok(),
            tmux_socket: std::env::var("TMUX")
                .ok()
                .and_then(|value| tmux_socket(&value)),
        }
    }
}

fn tmux_socket(value: &str) -> Option<String> {
    let mut separators = value.rmatch_indices(',');
    let (_, _) = separators.next()?;
    let (socket_end, _) = separators.next()?;
    Some(value[..socket_end].to_owned()).filter(|value| !value.is_empty())
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn replace_atomically(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn replace_atomically(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )?;
    }
    Ok(())
}

pub fn find_session_records(session_id: &str) -> Vec<PathBuf> {
    let Some(home) = directories::UserDirs::new() else {
        return vec![];
    };
    let root = home.home_dir().join(".claude/projects");
    let project_directories: Vec<_> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    let owned: Vec<_> = project_directories
        .iter()
        .map(|directory| directory.join(format!("{session_id}.agentsmon.cstatus")))
        .filter(|path| path.exists())
        .collect();
    if !owned.is_empty() {
        return owned;
    }
    project_directories
        .into_iter()
        .filter_map(|directory| {
            directory
                .join(format!("{session_id}.cstatus"))
                .exists()
                .then(|| directory.join(format!("{session_id}.agentsmon.cstatus")))
        })
        .collect()
}

pub fn claude_process_ids() -> (i32, i32) {
    let own = Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let mut current = own;
    let mut previous = own;
    for _ in 0..6 {
        let Some(process) = system.process(current) else {
            break;
        };
        let Some(parent) = process.parent() else {
            break;
        };
        if let Some(parent_process) = system.process(parent) {
            let name = parent_process.name().to_string_lossy().to_ascii_lowercase();
            let executable = parent_process
                .exe()
                .map(|p| p.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if name.contains("claude") || executable.contains("claude") {
                return (
                    parent.as_u32() as i32,
                    parent_process.parent().unwrap_or(previous).as_u32() as i32,
                );
            }
        }
        previous = current;
        current = parent;
    }
    (current.as_u32() as i32, previous.as_u32() as i32)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn update(percentage: Option<f32>) -> RecordUpdate<'static> {
        RecordUpdate {
            session_id: "session",
            pid: 123,
            ppid: 12,
            state: "active",
            activity: "tool".into(),
            cwd: "/tmp/project",
            event: "test",
            context_percentage: percentage,
            rate_limits: None,
            title: None,
        }
    }

    #[test]
    fn recovers_from_corruption_and_preserves_context() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.cstatus");
        fs::write(&path, b"{partial").unwrap();
        merge_record(
            &path,
            RecordUpdate {
                title: Some("Fix the build".into()),
                ..update(Some(68.5))
            },
        )
        .unwrap();
        merge_record(&path, update(None)).unwrap();

        let record: StatusRecord = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(record.context_used_percentage, Some(68.5));
        assert_eq!(record.cwd, "/tmp/project");
        assert_eq!(record.title.as_deref(), Some("Fix the build"));
    }

    #[test]
    fn partial_rate_limit_updates_preserve_both_windows() {
        let five_hour = ClaudeRateLimitWindow {
            used_percentage: 21.0,
            resets_at: None,
        };
        let seven_day = ClaudeRateLimitWindow {
            used_percentage: 63.0,
            resets_at: None,
        };
        let merged = merge_rate_limits(
            Some(ClaudeRateLimits {
                five_hour: None,
                seven_day: Some(seven_day),
            }),
            Some(ClaudeRateLimits {
                five_hour: Some(five_hour),
                seven_day: None,
            }),
        )
        .unwrap();

        assert_eq!(merged.five_hour.unwrap().used_percentage, 21.0);
        assert_eq!(merged.seven_day.unwrap().used_percentage, 63.0);
    }

    #[test]
    fn concurrent_updates_leave_a_complete_record() {
        let directory = tempfile::tempdir().unwrap();
        let path = Arc::new(directory.path().join("session.cstatus"));
        let threads: Vec<_> = (0..8)
            .map(|value| {
                let path = path.clone();
                std::thread::spawn(move || {
                    merge_record(&path, update(Some(value as f32))).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }

        let record: StatusRecord =
            serde_json::from_slice(&fs::read(path.as_ref()).unwrap()).unwrap();
        assert_eq!(record.session_id, "session");
        assert!(record.context_used_percentage.unwrap() <= 7.0);
    }

    #[test]
    fn parses_tmux_socket_paths_containing_commas() {
        assert_eq!(
            tmux_socket("/tmp/a,b/default,123,0").as_deref(),
            Some("/tmp/a,b/default")
        );
        assert_eq!(tmux_socket("invalid"), None);
    }

    #[test]
    fn lifecycle_records_have_an_agentsmon_owned_name() {
        assert_eq!(
            path_from_transcript("/tmp/project/session.jsonl", "session").unwrap(),
            PathBuf::from("/tmp/project/session.agentsmon.cstatus")
        );
    }

    #[test]
    fn heartbeats_preserve_lifecycle_time_and_terminal_hooks_clear_activity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.cstatus");
        merge_record(&path, update(None)).unwrap();
        let mut initial: StatusRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        initial.timestamp = "2026-09-16T10:00:00Z".into();
        initial.event_timestamp = None; // Upgrade an existing record too.
        fs::write(&path, serde_json::to_vec(&initial).unwrap()).unwrap();
        merge_record(
            &path,
            RecordUpdate {
                state: "",
                event: "",
                activity: String::new(),
                ..update(Some(20.0))
            },
        )
        .unwrap();
        let heartbeat: StatusRecord = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            heartbeat.event_timestamp.as_deref(),
            Some("2026-09-16T10:00:00Z")
        );
        assert_eq!(heartbeat.state, "active");
        assert_eq!(heartbeat.activity, "tool");
        assert_ne!(heartbeat.timestamp, initial.timestamp);
        merge_record(
            &path,
            RecordUpdate {
                state: "idle",
                event: "PostToolUseFailure",
                activity: String::new(),
                ..update(None)
            },
        )
        .unwrap();
        let stopped: StatusRecord = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(stopped.state, "idle");
        assert!(stopped.activity.is_empty());
        assert_eq!(
            stopped.event_timestamp.as_deref(),
            Some(stopped.timestamp.as_str())
        );
    }
}
