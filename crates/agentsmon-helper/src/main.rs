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
    background_tasks: Vec<serde_json::Value>,
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
    let (pid, ppid) = record::claude_process_ids();
    // Sessions without a status line (VS Code, claude.ai) only learn their
    // context usage from the transcript tail, which also carries the title.
    let transcript = transcript::inspect(&hook.transcript_path);
    record::merge_record(
        &path,
        record::RecordUpdate {
            session_id: &hook.session_id,
            pid,
            ppid,
            cwd: &hook.cwd,
            event: &hook.hook_event_name,
            state,
            activity,
            context_percentage: transcript.context_percentage,
            rate_limits: None,
            title: transcript.title,
            // Claude Code runs internal forked agents (away summary, memory
            // extraction...) after the main turn ended; their hooks carry an
            // agent_id and must not turn an idle session green again.
            keeps_idle: hook.agent_id.is_some() && state == "active",
        },
    )
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
        "Notification" if input.notification_type.as_deref() == Some("auth_success") => {
            ("active", "auth_success".into())
        }
        "Notification" => (
            "waiting",
            input
                .notification_type
                .clone()
                .unwrap_or_else(|| "notification".into()),
        ),
        "PreCompact" => ("compacting", "compacting".into()),
        // Stop is explicitly NOT emitted for user interrupts. An interrupted
        // tool is a terminal event, unlike an ordinary tool failure. An agent
        // interruption alone does not establish that the parent turn ended.
        "PostToolUseFailure" if input.is_interrupt && input.agent_id.is_none() => {
            ("idle", String::new())
        }
        "Stop" if input.background_tasks.is_empty() => ("idle", String::new()),
        "Stop" => ("active", "background task".into()),
        "PostCompact" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse"
        | "PostToolUseFailure" | "SubagentStart" | "SubagentStop" => {
            ("active", input.tool_name.clone().unwrap_or_default())
        }
        _ => ("active", String::new()),
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
        }
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
