mod codex;
mod foreground;
mod record;
mod statusline;
mod transcript;
mod vibe;

use std::io::{self, Read};

use anyhow::{bail, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: String,
    transcript_path: String,
    #[serde(default)]
    cwd: String,
    hook_event_name: String,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    notification_type: Option<String>,
    #[serde(default)]
    is_interrupt: bool,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    background_tasks: Vec<BackgroundTask>,
    #[serde(default)]
    session_crons: Vec<SessionCron>,
}

/// One running background task listed by the Stop hook. Only its opaque id
/// and public type are decoded: `command` and `description` carry
/// conversation text. Claude Code 2.1.302 labels every `local_bash` task
/// `shell`, Monitor tool watches included; only the id tells them apart.
#[derive(Debug, Deserialize)]
struct BackgroundTask {
    #[serde(default)]
    id: String,
    #[serde(rename = "type", default)]
    kind: String,
}

/// Session cron listed by the Stop hook (CronCreate, ScheduleWakeup, /loop).
/// `prompt` is conversation text and is not decoded.
#[derive(Debug, Deserialize)]
struct SessionCron {
    #[serde(default)]
    recurring: bool,
}

/// `PostToolUse` of the Monitor tool, decoded in a second pass only for that
/// tool: other tools' responses carry conversation content.
#[derive(Debug, Deserialize)]
struct MonitorHook {
    tool_response: MonitorResponse,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MonitorResponse {
    task_id: String,
    #[serde(default)]
    persistent: bool,
}

impl BackgroundTask {
    /// Agent work that continues after the turn and resumes the session.
    /// Shells (dev servers…), MCP tasks and internal housekeeping (`dream`,
    /// `auto-mode scan`) run without the model waiting on them. Monitor tool
    /// watches are recognised by id instead (see `record::StopTasks`).
    fn is_agent_work(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "subagent" | "workflow" | "teammate" | "cloud session"
        )
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("agentsmon-helper: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "hook".into());
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input)?;
    match mode.as_str() {
        "hook" => process_hook(&input),
        "codex-hook" => codex::process(&input),
        "vibe-hook" => vibe::process(&input),
        "statusline-proxy" => statusline::proxy(&input),
        _ => bail!("unknown mode {mode}"),
    }
}

fn process_hook(input: &[u8]) -> Result<()> {
    let hook: HookInput = serde_json::from_slice(input)?;
    let path = record::path_from_transcript(&hook.transcript_path, &hook.session_id)?;
    if hook.hook_event_name == "SessionEnd" {
        if let Err(error) = std::fs::remove_file(&path) {
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error.into());
            }
        }
        return Ok(());
    }

    let (state, activity) = event_state(&hook);
    let started_monitor = started_monitor(&hook, input);
    let stop_tasks = stop_tasks(&hook);
    // Informational hooks only refresh the record, like a status-line beat.
    let event = if state.is_empty() {
        ""
    } else {
        hook.hook_event_name.as_str()
    };
    let (pid, ppid) = record::claude_process_ids();
    // Sessions without a status line (VS Code, claude.ai) only learn their
    // context usage from the transcript tail, which also carries the title.
    let transcript = transcript::inspect(&hook.transcript_path);
    let project_dir = std::env::var("CLAUDE_PROJECT_DIR").ok();
    record::merge_record(
        &path,
        record::RecordUpdate {
            session_id: &hook.session_id,
            pid,
            ppid,
            cwd: session_dir(project_dir.as_deref(), &hook.cwd),
            event,
            state,
            activity,
            context_percentage: transcript.context_percentage,
            rate_limits: None,
            title: transcript.title,
            // Claude Code runs internal forked agents (away summary, memory
            // extraction...) after the main turn ended; their hooks carry an
            // agent_id and must not turn an idle or waiting session green.
            subagent: hook.agent_id.is_some(),
            started_monitor,
            stop_tasks,
        },
    )
}

/// Directory the session belongs to. Hook `cwd` follows the Bash tool's
/// working directory, so a `cd ui` (often from a subagent) would move the
/// card to a `ui` project; Claude exports the launch directory to hooks.
fn session_dir<'a>(project_dir: Option<&'a str>, cwd: &'a str) -> &'a str {
    project_dir.filter(|dir| !dir.is_empty()).unwrap_or(cwd)
}

/// Task id of a bounded watch armed by the main agent with the Monitor tool.
/// Persistent monitors (log tails…) watch indefinitely without the agent
/// waiting on an outcome; a subagent's monitor does not resume the session.
fn started_monitor(hook: &HookInput, input: &[u8]) -> Option<String> {
    if hook.hook_event_name != "PostToolUse"
        || hook.tool_name.as_deref() != Some("Monitor")
        || hook.agent_id.is_some()
    {
        return None;
    }
    let monitor: MonitorHook = serde_json::from_slice(input).ok()?;
    (!monitor.tool_response.persistent && !monitor.tool_response.task_id.is_empty())
        .then_some(monitor.tool_response.task_id)
}

/// What the ending turn left armed, for a Stop that would otherwise rest.
fn stop_tasks(hook: &HookInput) -> Option<record::StopTasks> {
    (hook.hook_event_name == "Stop").then(|| record::StopTasks {
        running: hook
            .background_tasks
            .iter()
            .map(|task| task.id.clone())
            .collect(),
        // One-shot crons: ScheduleWakeup and one-time CronCreate reminders.
        // Recurring crons (/loop 5m…) can stay armed for days.
        wakeup: hook.session_crons.iter().any(|cron| !cron.recurring),
    })
}

fn event_state(input: &HookInput) -> (&'static str, String) {
    match input.hook_event_name.as_str() {
        "SessionStart" => ("idle", String::new()),
        "PermissionRequest" | "Elicitation" | "ElicitationResult" => ("waiting", "input".into()),
        // Claude's VS Code extension can show the plan approval UI without
        // emitting PermissionRequest. PreToolUse is delivered before that UI,
        // so treat ExitPlanMode itself as the actionable wait boundary.
        "PreToolUse" if input.tool_name.as_deref() == Some("ExitPlanMode") => {
            ("waiting", "input".into())
        }
        "Notification" => notification_state(input.notification_type.as_deref()),
        "PreCompact" => ("compacting", "compacting".into()),
        // Stop is explicitly NOT emitted for user interrupts. An interrupted
        // tool is a terminal event, unlike an ordinary tool failure. An agent
        // interruption alone does not establish that the parent turn ended.
        "PostToolUseFailure" if input.is_interrupt && input.agent_id.is_none() => {
            ("idle", String::new())
        }
        "Stop"
            if input
                .background_tasks
                .iter()
                .any(BackgroundTask::is_agent_work) =>
        {
            ("active", "background task".into())
        }
        "Stop" => ("idle", String::new()),
        "PostCompact" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse"
        | "PostToolUseFailure" | "SubagentStart" | "SubagentStop" => {
            ("active", input.tool_name.clone().unwrap_or_default())
        }
        _ => ("active", String::new()),
    }
}

/// Claude Code 2.1.285 notification types. Only requests for the user's
/// decision are a wait; `idle_prompt` means the finished turn sat unanswered.
/// An empty state leaves the record untouched (informational notification).
fn notification_state(kind: Option<&str>) -> (&'static str, String) {
    match kind {
        // Older versions send no type: their notifications asked for input.
        None => ("waiting", "notification".into()),
        Some(
            kind @ ("permission_prompt"
            | "worker_permission_prompt"
            | "elicitation_dialog"
            | "elicitation_url_dialog"
            | "agent_needs_input"),
        ) => ("waiting", kind.into()),
        Some("idle_prompt") => ("idle", String::new()),
        Some("auth_success") => ("active", "auth_success".into()),
        // agent_completed, push_notification, quota_auto_resume_*,
        // computer_use_*, model_refusal_fallback and future types.
        Some(_) => ("", String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(event: &str) -> HookInput {
        HookInput {
            session_id: "s".into(),
            transcript_path: "/tmp/s.jsonl".into(),
            cwd: "/tmp".into(),
            hook_event_name: event.into(),
            tool_name: None,
            notification_type: None,
            is_interrupt: false,
            agent_id: None,
            background_tasks: vec![],
            session_crons: vec![],
        }
    }

    #[test]
    fn keeps_the_launch_directory_after_cd() {
        assert_eq!(session_dir(Some("/repo"), "/repo/ui"), "/repo");
        assert_eq!(session_dir(Some(""), "/repo/ui"), "/repo/ui");
        assert_eq!(session_dir(None, "/repo/ui"), "/repo/ui");
    }

    #[test]
    fn maps_the_four_states() {
        assert_eq!(event_state(&hook("SessionStart")).0, "idle");
        assert_eq!(event_state(&hook("UserPromptSubmit")).0, "active");
        assert_eq!(event_state(&hook("PermissionRequest")).0, "waiting");
        assert_eq!(event_state(&hook("PreCompact")).0, "compacting");
        assert_eq!(event_state(&hook("PostCompact")).0, "active");
        assert_eq!(event_state(&hook("Stop")).0, "idle");
    }

    #[test]
    fn only_requests_for_a_decision_are_waits() {
        let notification = |kind: Option<&str>| {
            let mut input = hook("Notification");
            input.notification_type = kind.map(str::to_owned);
            event_state(&input).0
        };
        assert_eq!(notification(Some("permission_prompt")), "waiting");
        assert_eq!(notification(Some("elicitation_dialog")), "waiting");
        assert_eq!(notification(Some("agent_needs_input")), "waiting");
        assert_eq!(notification(None), "waiting");
        assert_eq!(notification(Some("idle_prompt")), "idle");
        assert_eq!(notification(Some("auth_success")), "active");
        assert_eq!(notification(Some("agent_completed")), "");
        assert_eq!(notification(Some("push_notification")), "");
    }

    #[test]
    fn background_shells_do_not_keep_a_finished_turn_active() {
        let stop = |tasks: &str| {
            let input: HookInput = serde_json::from_str(&format!(
                r#"{{"session_id":"s","transcript_path":"/tmp/s.jsonl","hook_event_name":"Stop","background_tasks":{tasks}}}"#
            ))
            .unwrap();
            event_state(&input).0
        };
        assert_eq!(
            stop(
                r#"[{"id":"b1","type":"shell","status":"running","command":"npm run dev","description":"dev server"}]"#
            ),
            "idle"
        );
        assert_eq!(
            stop(r#"[{"id":"m1","type":"monitor"},{"id":"d1","type":"dream"}]"#),
            "idle"
        );
        assert_eq!(
            stop(r#"[{"id":"b1","type":"shell"},{"id":"a1","type":"subagent","agent_type":"x"}]"#),
            "active"
        );
        assert_eq!(stop(r#"[{"id":"x"}]"#), "idle");
    }

    #[test]
    fn records_only_bounded_monitors_of_the_main_agent() {
        let started = |json: &str| {
            let input: HookInput = serde_json::from_str(json).unwrap();
            started_monitor(&input, json.as_bytes())
        };
        let monitor = |response: &str, agent: &str| {
            format!(
                r#"{{"session_id":"s","transcript_path":"/t/s.jsonl","hook_event_name":"PostToolUse","tool_name":"Monitor"{agent},"tool_input":{{"command":"x"}},"tool_response":{response}}}"#
            )
        };
        assert_eq!(
            started(&monitor(
                r#"{"taskId":"bfxllzues","timeoutMs":120000,"persistent":false}"#,
                ""
            ))
            .as_deref(),
            Some("bfxllzues")
        );
        assert_eq!(
            started(&monitor(
                r#"{"taskId":"b1","timeoutMs":0,"persistent":true}"#,
                ""
            )),
            None
        );
        assert_eq!(
            started(&monitor(r#"{"taskId":"b1"}"#, r#","agent_id":"child""#)),
            None
        );
        assert_eq!(started(&monitor(r#""unexpected""#, "")), None);
        assert_eq!(
            started(
                r#"{"session_id":"s","transcript_path":"/t/s.jsonl","hook_event_name":"PostToolUse","tool_name":"Bash","tool_response":{"stdout":"x"}}"#
            ),
            None
        );
    }

    #[test]
    fn stop_lists_running_task_ids_and_one_shot_wakeups() {
        let tasks = |json: &str| {
            let input: HookInput = serde_json::from_str(&format!(
                r#"{{"session_id":"s","transcript_path":"/t/s.jsonl","hook_event_name":"Stop",{json}}}"#
            ))
            .unwrap();
            stop_tasks(&input).map(|tasks| (tasks.running, tasks.wakeup))
        };
        assert_eq!(
            tasks(
                r#""background_tasks":[{"id":"b1","type":"shell","status":"running","command":"x"}],"session_crons":[{"id":"c1","schedule":"* * * * *","recurring":true,"prompt":"x"}]"#
            ),
            Some((vec!["b1".to_owned()], false))
        );
        assert_eq!(
            tasks(
                r#""session_crons":[{"id":"c1","schedule":"5 12 * * *","recurring":false,"prompt":"x"}]"#
            ),
            Some((vec![], true))
        );
        assert_eq!(stop_tasks(&hook("SubagentStop")).map(|t| t.wakeup), None);
    }

    #[test]
    fn waits_while_a_proposed_plan_needs_a_decision() {
        let mut input = hook("PreToolUse");
        input.tool_name = Some("ExitPlanMode".into());
        assert_eq!(event_state(&input), ("waiting", "input".into()));

        input.hook_event_name = "PostToolUse".into();
        assert_eq!(event_state(&input), ("active", "ExitPlanMode".into()));
    }

    #[test]
    fn interrupted_tool_ends_the_turn_but_an_ordinary_failure_does_not() {
        let mut input: HookInput = serde_json::from_str(
            r#"{"session_id":"s","transcript_path":"/tmp/s.jsonl","hook_event_name":"PostToolUseFailure","tool_name":"Bash","is_interrupt":true,"error":"ignored","tool_input":{"command":"ignored"}}"#,
        ).unwrap();
        assert_eq!(event_state(&input), ("idle", String::new()));
        input.agent_id = Some("child".into());
        assert_eq!(event_state(&input).0, "active");
        input.agent_id = None;
        input.is_interrupt = false;
        assert_eq!(event_state(&input).0, "active");
        assert_eq!(event_state(&hook("PostToolUseFailure")).0, "active");
    }
}
