use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::record::{self, ClaudeRateLimitWindow, ClaudeRateLimits, RecordUpdate};

#[derive(Deserialize)]
struct StatusLineInput {
    session_id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    workspace: Option<Workspace>,
    #[serde(default)]
    context_window: Option<ContextWindow>,
    #[serde(default)]
    rate_limits: Option<StatusLineRateLimits>,
}

#[derive(Deserialize)]
struct Workspace {
    #[serde(default)]
    project_dir: String,
}

impl StatusLineInput {
    /// Launch directory: top-level `cwd` follows the Bash tool's `cd`.
    fn session_dir(&self) -> &str {
        self.workspace
            .as_ref()
            .map(|workspace| workspace.project_dir.as_str())
            .filter(|dir| !dir.is_empty())
            .unwrap_or(&self.cwd)
    }
}

#[derive(Deserialize)]
struct ContextWindow {
    used_percentage: Option<f32>,
}

#[derive(Deserialize)]
struct StatusLineRateLimits {
    #[serde(default)]
    five_hour: Option<StatusLineRateLimitWindow>,
    #[serde(default)]
    seven_day: Option<StatusLineRateLimitWindow>,
}

#[derive(Deserialize)]
struct StatusLineRateLimitWindow {
    used_percentage: f32,
    #[serde(default)]
    resets_at: Option<i64>,
}

#[derive(Deserialize)]
struct OriginalStatusLine {
    command: String,
}

pub fn proxy(input: &[u8]) -> Result<()> {
    let status: StatusLineInput = serde_json::from_slice(input)?;
    let cwd = status.session_dir().to_owned();
    let percentage = normalize_percentage(
        status
            .context_window
            .and_then(|window| window.used_percentage),
    );
    let rate_limits = status.rate_limits.and_then(normalize_rate_limits);
    for path in record::find_session_records(&status.session_id) {
        record::merge_record(
            &path,
            RecordUpdate {
                session_id: &status.session_id,
                pid: 0,
                ppid: 0,
                state: "",
                activity: String::new(),
                cwd: &cwd,
                // A status-line refresh is a heartbeat, not a lifecycle event.
                // Keep the last hook event so the provider can distinguish an
                // in-flight tool from model activity that may be interrupted
                // without Claude emitting Stop.
                event: "",
                context_percentage: percentage,
                rate_limits: rate_limits.clone(),
                title: None,
                subagent: false,
                started_monitor: None,
                stop_tasks: None,
            },
        )?;
    }
    run_original(input)
}

fn run_original(input: &[u8]) -> Result<()> {
    let Some(config) = original_config() else {
        return Ok(());
    };
    // No saved statusline means the user had none before AgentsMon wrapped
    // the setting; a proxy failure here would make Claude Code report the
    // whole statusline command as broken.
    let bytes = match std::fs::read(config) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let original: OriginalStatusLine = serde_json::from_slice(&bytes)?;
    if original.command.trim().is_empty() {
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    let mut child = {
        let mut command = if let Ok(shell) = std::env::var("SHELL") {
            let mut command = Command::new(shell);
            command.args(["-lc", &original.command]);
            command
        } else {
            use std::os::windows::process::CommandExt;
            let mut command =
                Command::new(std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()));
            // cmd.exe does not understand the \" escaping of Command::arg. /S strips
            // the outer quotes and leaves the command untouched.
            command
                .args(["/D", "/S", "/C"])
                .raw_arg(format!("\"{}\"", original.command));
            command
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?
    };
    #[cfg(not(target_os = "windows"))]
    let mut child = Command::new(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()))
        .args(["-lc", &original.command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    child
        .stdin
        .take()
        .context("statusline stdin")?
        .write_all(input)?;
    let output = child.wait_with_output()?;
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    if !output.status.success() {
        std::process::exit(output.status.code().unwrap_or(1));
    }
    Ok(())
}

fn original_config() -> Option<std::path::PathBuf> {
    if let Some(directory) = std::env::var_os("AGENTSMON_CONFIG_DIR") {
        return Some(std::path::PathBuf::from(directory).join("statusline-original.json"));
    }
    directories::ProjectDirs::from("com", "oliv", "AgentsMon")
        .map(|dirs| dirs.config_dir().join("statusline-original.json"))
}

fn normalize_percentage(value: Option<f32>) -> Option<f32> {
    value
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 100.0))
}

fn normalize_rate_limits(value: StatusLineRateLimits) -> Option<ClaudeRateLimits> {
    let normalize_window = |window: StatusLineRateLimitWindow| {
        normalize_percentage(Some(window.used_percentage)).map(|used_percentage| {
            ClaudeRateLimitWindow {
                used_percentage,
                resets_at: window.resets_at,
            }
        })
    };
    let limits = ClaudeRateLimits {
        five_hour: value.five_hour.and_then(normalize_window),
        seven_day: value.seven_day.and_then(normalize_window),
    };
    (limits.five_hour.is_some() || limits.seven_day.is_some()).then_some(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_null_and_fractional_usage() {
        let null: StatusLineInput =
            serde_json::from_str(r#"{"session_id":"x","context_window":{"used_percentage":null}}"#)
                .unwrap();
        assert!(null.context_window.unwrap().used_percentage.is_none());
        let value: StatusLineInput =
            serde_json::from_str(r#"{"session_id":"x","context_window":{"used_percentage":68.5}}"#)
                .unwrap();
        assert_eq!(value.context_window.unwrap().used_percentage, Some(68.5));
    }

    #[test]
    fn prefers_the_project_directory() {
        let moved: StatusLineInput = serde_json::from_str(
            r#"{"session_id":"x","cwd":"/repo/ui","workspace":{"current_dir":"/repo/ui","project_dir":"/repo"}}"#,
        )
        .unwrap();
        assert_eq!(moved.session_dir(), "/repo");
        let old: StatusLineInput =
            serde_json::from_str(r#"{"session_id":"x","cwd":"/repo/ui"}"#).unwrap();
        assert_eq!(old.session_dir(), "/repo/ui");
    }

    #[test]
    fn normalizes_invalid_and_out_of_range_usage() {
        assert_eq!(normalize_percentage(Some(-1.0)), Some(0.0));
        assert_eq!(normalize_percentage(Some(1.0)), Some(1.0));
        assert_eq!(normalize_percentage(Some(50.0)), Some(50.0));
        assert_eq!(normalize_percentage(Some(99.0)), Some(99.0));
        assert_eq!(normalize_percentage(Some(101.0)), Some(100.0));
        assert_eq!(normalize_percentage(Some(f32::NAN)), None);
        assert_eq!(normalize_percentage(None), None);
    }

    #[test]
    fn normalizes_subscription_rate_limits() {
        let input: StatusLineInput = serde_json::from_str(
            r#"{"session_id":"x","rate_limits":{"five_hour":{"used_percentage":37.5,"resets_at":1784200000},"seven_day":{"used_percentage":104}}}"#,
        )
        .unwrap();
        let limits = normalize_rate_limits(input.rate_limits.unwrap()).unwrap();
        assert_eq!(limits.five_hour.unwrap().used_percentage, 37.5);
        assert_eq!(limits.seven_day.unwrap().used_percentage, 100.0);
    }
}
