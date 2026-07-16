use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};

const TRANSCRIPT_TAIL_BYTES: u64 = 512 * 1024;

#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub cwd: String,
    pub hook_event_name: String,
    #[serde(default)]
    pub tool_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexStatusRecord {
    session_id: String,
    pid: i32,
    ppid: i32,
    state: String,
    activity: String,
    timestamp: String,
    cwd: String,
    event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_used_percentage: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rate_limits: Option<CodexRateLimits>,
}

#[derive(Debug, Deserialize)]
struct TranscriptEnvelope {
    #[serde(rename = "type")]
    kind: String,
    payload: TokenCountPayload,
}

#[derive(Debug, Deserialize)]
struct TokenCountPayload {
    #[serde(rename = "type")]
    kind: String,
    info: Option<TokenCountInfo>,
    #[serde(default, alias = "rateLimits")]
    rate_limits: Option<CodexRateLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexRateLimits {
    #[serde(default)]
    primary: Option<CodexRateLimitWindow>,
    #[serde(default)]
    secondary: Option<CodexRateLimitWindow>,
    #[serde(default, alias = "planType")]
    plan_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexRateLimitWindow {
    #[serde(alias = "usedPercent")]
    used_percent: f32,
    #[serde(default, alias = "windowDurationMins", alias = "windowMinutes")]
    window_minutes: Option<u64>,
    #[serde(default, alias = "resetsAt")]
    resets_at: Option<i64>,
}

#[derive(Default)]
struct UsageMetrics {
    context_percentage: Option<f32>,
    rate_limits: Option<CodexRateLimits>,
}

#[derive(Debug, Deserialize)]
struct TokenCountInfo {
    last_token_usage: Option<TokenUsage>,
    model_context_window: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenUsage {
    total_tokens: u64,
}

pub fn process(input: &[u8]) -> Result<()> {
    let hook: HookInput = serde_json::from_slice(input)?;
    validate_session_id(&hook.session_id)?;
    let path = runtime_root()?.join(format!("{}.json", hook.session_id));
    let (state, activity) = event_state(&hook);
    let metrics = hook
        .transcript_path
        .as_deref()
        .and_then(|path| extract_usage_metrics(Path::new(path)).ok())
        .unwrap_or_default();
    let (pid, ppid) = codex_process_ids();
    merge_record(&path, &hook, pid, ppid, state, activity, metrics)?;

    // Stop hooks require valid JSON on stdout. An empty object leaves Codex behavior unchanged.
    if hook.hook_event_name == "Stop" {
        println!("{{}}");
    }
    Ok(())
}

fn event_state(input: &HookInput) -> (&'static str, String) {
    match input.hook_event_name.as_str() {
        "SessionStart" => ("idle", String::new()),
        "PermissionRequest" => ("waiting", "input".into()),
        "PreCompact" => ("compacting", "compacting".into()),
        "Stop" => ("idle", String::new()),
        "PostCompact" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => {
            ("active", input.tool_name.clone().unwrap_or_default())
        }
        _ => ("active", String::new()),
    }
}

fn runtime_root() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "oliv", "AgentsMon")
        .context("cannot resolve AgentsMon data directory")?;
    Ok(dirs.data_dir().join("codex"))
}

fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("invalid Codex session id");
    }
    Ok(())
}

fn merge_record(
    path: &Path,
    hook: &HookInput,
    pid: i32,
    ppid: i32,
    state: &str,
    activity: String,
    metrics: UsageMetrics,
) -> Result<()> {
    let parent = path.parent().context("Codex status path has no parent")?;
    fs::create_dir_all(parent)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    lock.lock_exclusive()?;

    let previous = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<CodexStatusRecord>(&bytes).ok());
    let record = CodexStatusRecord {
        session_id: hook.session_id.clone(),
        pid: (pid > 0)
            .then_some(pid)
            .or_else(|| previous.as_ref().map(|record| record.pid))
            .unwrap_or_default(),
        ppid: (ppid > 0)
            .then_some(ppid)
            .or_else(|| previous.as_ref().map(|record| record.ppid))
            .unwrap_or_default(),
        state: state.into(),
        activity,
        timestamp: Utc::now().to_rfc3339(),
        cwd: if hook.cwd.is_empty() {
            previous
                .as_ref()
                .map(|record| record.cwd.clone())
                .unwrap_or_default()
        } else {
            hook.cwd.clone()
        },
        event: hook.hook_event_name.clone(),
        context_used_percentage: merged_context_percentage(
            hook.hook_event_name.as_str(),
            metrics.context_percentage,
            previous
                .as_ref()
                .and_then(|record| record.context_used_percentage),
        ),
        rate_limits: metrics.rate_limits.or_else(|| {
            previous
                .as_ref()
                .and_then(|record| record.rate_limits.clone())
        }),
    };

    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = File::create(&temporary)?;
    serde_json::to_writer(&mut file, &record)?;
    file.sync_all()?;
    crate::record::replace_atomically(&temporary, path)?;
    FileExt::unlock(&lock)?;
    Ok(())
}

fn merged_context_percentage(
    event: &str,
    current: Option<f32>,
    previous: Option<f32>,
) -> Option<f32> {
    if event == "PostCompact" {
        current
    } else {
        current.or(previous)
    }
}

fn extract_usage_metrics(path: &Path) -> Result<UsageMetrics> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;

    let mut lines = tail.split(|byte| *byte == b'\n');
    if start > 0 {
        let _ = lines.next();
    }
    let lines: Vec<_> = lines.collect();
    for line in lines.into_iter().rev() {
        if !contains(line, br#""type":"event_msg""#) || !contains(line, br#""type":"token_count""#)
        {
            continue;
        }
        let Ok(envelope) = serde_json::from_slice::<TranscriptEnvelope>(line) else {
            continue;
        };
        if envelope.kind != "event_msg" || envelope.payload.kind != "token_count" {
            continue;
        }
        let Some(info) = envelope.payload.info else {
            return Ok(UsageMetrics {
                rate_limits: envelope.payload.rate_limits,
                ..UsageMetrics::default()
            });
        };
        let Some(used) = info.last_token_usage.map(|usage| usage.total_tokens) else {
            return Ok(UsageMetrics {
                rate_limits: envelope.payload.rate_limits,
                ..UsageMetrics::default()
            });
        };
        let Some(window) = info.model_context_window.filter(|window| *window > 0) else {
            return Ok(UsageMetrics {
                rate_limits: envelope.payload.rate_limits,
                ..UsageMetrics::default()
            });
        };
        return Ok(UsageMetrics {
            context_percentage: Some(
                ((used as f64 / window as f64) * 100.0).clamp(0.0, 100.0) as f32
            ),
            rate_limits: envelope.payload.rate_limits,
        });
    }
    Ok(UsageMetrics::default())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn codex_process_ids() -> (i32, i32) {
    let own = Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let mut current = own;
    for _ in 0..10 {
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
                .map(|path| path.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if (name.contains("codex") || executable.contains("codex"))
                && !name.contains("agentsmon")
                && !executable.contains("agentsmon")
            {
                return (
                    parent.as_u32() as i32,
                    parent_process.parent().unwrap_or(current).as_u32() as i32,
                );
            }
        }
        current = parent;
    }
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_codex_lifecycle_states() {
        let input = |event: &str| HookInput {
            session_id: "session".into(),
            transcript_path: None,
            cwd: "/tmp/project".into(),
            hook_event_name: event.into(),
            tool_name: None,
        };
        assert_eq!(event_state(&input("SessionStart")).0, "idle");
        assert_eq!(event_state(&input("UserPromptSubmit")).0, "active");
        assert_eq!(event_state(&input("PermissionRequest")).0, "waiting");
        assert_eq!(event_state(&input("PreCompact")).0, "compacting");
        assert_eq!(event_state(&input("PostCompact")).0, "active");
        assert_eq!(event_state(&input("Stop")).0, "idle");
    }

    #[test]
    fn extracts_only_token_count_metrics() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"private\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":100},\"model_context_window\":200}}}\n"
            ),
        )
        .unwrap();
        assert_eq!(
            extract_usage_metrics(&path).unwrap().context_percentage,
            Some(50.0)
        );
    }

    #[test]
    fn clamps_invalid_context_ratios() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        fs::write(
            &path,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":250},\"model_context_window\":200}}}\n",
        )
        .unwrap();
        assert_eq!(
            extract_usage_metrics(&path).unwrap().context_percentage,
            Some(100.0)
        );
    }

    #[test]
    fn extracts_codex_rate_limit_metadata_without_conversation_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"private\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":null,\"rate_limits\":{\"primary\":{\"used_percent\":72.5,\"window_minutes\":300,\"resets_at\":1784200000},\"secondary\":{\"used_percent\":31,\"window_minutes\":10080,\"resets_at\":1784800000},\"plan_type\":\"plus\"}}}\n"
            ),
        )
        .unwrap();
        let limits = extract_usage_metrics(&path).unwrap().rate_limits.unwrap();
        assert_eq!(limits.primary.unwrap().used_percent, 72.5);
        assert_eq!(limits.secondary.unwrap().window_minutes, Some(10_080));
        assert_eq!(limits.plan_type.as_deref(), Some("plus"));
    }

    #[test]
    fn clears_stale_context_after_compaction() {
        assert_eq!(
            merged_context_percentage("PostCompact", None, Some(87.0)),
            None
        );
        assert_eq!(
            merged_context_percentage("PostCompact", Some(12.0), Some(87.0)),
            Some(12.0)
        );
        assert_eq!(
            merged_context_percentage("PostToolUse", None, Some(87.0)),
            Some(87.0)
        );
    }
}
