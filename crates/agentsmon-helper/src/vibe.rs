use std::fs::{self, OpenOptions};
use std::path::Path;

use agentsmon_core::AgentState;
use agentsmon_providers::vibe::{default_runtime_root, is_vibe_command, VibeStatusRecord};
use anyhow::{bail, Result};
use chrono::Utc;
use fs2::FileExt;
use serde::Deserialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::record::{replace_atomically, EnvironmentMetadata};

// Deliberately exclude tool_input, tool_output*, tool_error and transcript_path.
// Serde skips those values without materializing conversation/tool content.
#[derive(Deserialize)]
struct HookInput {
    session_id: String,
    #[serde(default)]
    parent_session_id: Option<String>,
    cwd: String,
    hook_event_name: String,
    #[serde(default)]
    tool_name: Option<String>,
}

fn event_state(hook: &HookInput) -> Option<AgentState> {
    match hook.hook_event_name.as_str() {
        "post_agent" | "post_agent_turn" => Some(AgentState::Idle),
        "pre_tool" | "before_tool" if hook.tool_name.as_deref() == Some("ask_user_question") => {
            Some(AgentState::Waiting)
        }
        "pre_tool" | "post_tool" | "before_tool" | "after_tool" => Some(AgentState::Active),
        _ => None,
    }
}

pub fn process(input: &[u8]) -> Result<()> {
    let Some((pid, ppid)) = process_ids() else {
        return Ok(());
    };
    process_at(input, &default_runtime_root(), pid, ppid)
}

fn process_at(input: &[u8], root: &Path, pid: i32, ppid: i32) -> Result<()> {
    let hook: HookInput = serde_json::from_slice(input)?;
    // Subagents share the parent's process and have no independently focusable
    // terminal. Their completions must never reset the parent to idle.
    if hook.parent_session_id.is_some() {
        return Ok(());
    }
    let Some(state) = event_state(&hook) else {
        return Ok(());
    };
    if hook.session_id.is_empty()
        || hook.session_id.len() > 128
        || !hook
            .session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid Vibe session ID");
    }
    fs::create_dir_all(root)?;
    // One interactive conversation per process. /clear and /resume must remove
    // the previous card even though Vibe offers no session-end hook.
    let path = root.join(format!("{pid}.json"));
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(root.join(format!("{pid}.lock")))?;
    lock.lock_exclusive()?;
    let env = EnvironmentMetadata::current();
    let record = VibeStatusRecord {
        session_id: hook.session_id,
        pid,
        ppid,
        state,
        timestamp: Utc::now().to_rfc3339(),
        cwd: hook.cwd,
        event: hook.hook_event_name,
        source: None,
        context_tokens: None,
        context_window: None,
        host_app: env.host_app,
        terminal_session_id: env.terminal_session_id,
        tmux_pane: env.tmux_pane,
        tmux_socket: env.tmux_socket,
    };
    let temp = root.join(format!("{pid}.tmp.{}", std::process::id()));
    fs::write(&temp, serde_json::to_vec(&record)?)?;
    replace_atomically(&temp, &path)?;
    FileExt::unlock(&lock)?;
    Ok(())
}

fn process_ids() -> Option<(i32, i32)> {
    let mut system = System::new();
    let mut current = Pid::from_u32(std::process::id());
    for _ in 0..10 {
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[current]),
            false,
            ProcessRefreshKind::nothing()
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .with_exe(UpdateKind::OnlyIfNotSet),
        );
        let process = system.process(current)?;
        if is_vibe_command(process.name(), process.exe(), process.cmd()) {
            return Some((current.as_u32() as i32, process.parent()?.as_u32() as i32));
        }
        current = process.parent().filter(|pid| pid.as_u32() > 1)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tracks_tools_questions_completion_and_session_switches_without_content() {
        let dir = tempfile::tempdir().unwrap();
        let mut hook = json!({
            "session_id": "first", "cwd": "/tmp/demo", "hook_event_name": "pre_tool",
            "tool_name": "ask_user_question", "tool_input": {"question": "PRIVATE INPUT"},
            "tool_output_text": "PRIVATE OUTPUT", "tool_error": "PRIVATE ERROR",
            "transcript_path": "/must/not/be/opened/messages.jsonl"
        });
        for (event, expected) in [
            ("pre_tool", AgentState::Waiting),
            ("post_tool", AgentState::Active),
            ("post_agent", AgentState::Idle),
        ] {
            hook["hook_event_name"] = json!(event);
            process_at(&serde_json::to_vec(&hook).unwrap(), dir.path(), 42, 4).unwrap();
            let bytes = fs::read(dir.path().join("42.json")).unwrap();
            let record: VibeStatusRecord = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(record.state, expected);
            let text = String::from_utf8(bytes).unwrap();
            assert!(!text.contains("PRIVATE"));
            assert!(!text.contains("transcript"));
        }
        hook["session_id"] = json!("second");
        hook["hook_event_name"] = json!("pre_tool");
        hook["tool_name"] = json!("bash");
        process_at(&serde_json::to_vec(&hook).unwrap(), dir.path(), 42, 4).unwrap();
        let record =
            agentsmon_providers::VibeProvider::parse_file(&dir.path().join("42.json")).unwrap();
        assert_eq!(record.session_id, "second");
        assert_eq!(record.state, AgentState::Active);
        hook["parent_session_id"] = json!("second");
        hook["session_id"] = json!("child");
        hook["hook_event_name"] = json!("post_agent");
        process_at(&serde_json::to_vec(&hook).unwrap(), dir.path(), 42, 4).unwrap();
        assert_eq!(
            agentsmon_providers::VibeProvider::parse_file(&dir.path().join("42.json"))
                .unwrap()
                .session_id,
            "second"
        );
    }

    #[test]
    fn rejects_path_traversal_and_ignores_unknown_events() {
        let dir = tempfile::tempdir().unwrap();
        let hook = json!({"session_id":"../escape", "cwd":"/tmp", "hook_event_name":"post_agent"});
        assert!(process_at(&serde_json::to_vec(&hook).unwrap(), dir.path(), 42, 4).is_err());
        let hook = json!({"session_id":"s", "cwd":"/tmp", "hook_event_name":"future_event"});
        process_at(&serde_json::to_vec(&hook).unwrap(), dir.path(), 42, 4).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
