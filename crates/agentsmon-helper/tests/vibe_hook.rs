//! Exercise the actual helper subprocess, process ancestry and provider together.
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use agentsmon_core::AgentState;
use agentsmon_providers::{AgentProvider, VibeProvider};
use serde_json::json;

struct Host(Child);
impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn discovers_live_sessions_and_removes_exited_processes() {
    let dir = tempfile::tempdir().unwrap();
    let executable = dir
        .path()
        .join(if cfg!(windows) { "vibe.exe" } else { "vibe" });
    fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let mut host = Host(
        Command::new(&executable)
            .args(["--ignored", "--exact", "fake_vibe_host", "--nocapture"])
            .env("VIBE_HOME", dir.path())
            .env("AGENTSMON_TEST_VIBE_HOST", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut output = BufReader::new(host.0.stdout.take().unwrap());
    let provider = VibeProvider::with_root(dir.path().join("agentsmon/sessions"));
    let watcher = provider.watch().unwrap();
    for (session, event, tool, expected) in [
        ("first", "before_tool", "bash", AgentState::Active),
        ("first", "after_tool", "bash", AgentState::Active),
        ("first", "post_agent_turn", "", AgentState::Idle),
        (
            "second",
            "pre_tool",
            "ask_user_question",
            AgentState::Waiting,
        ),
        (
            "second",
            "post_tool",
            "ask_user_question",
            AgentState::Active,
        ),
        ("second", "post_agent", "", AgentState::Idle),
    ] {
        let payload = json!({"session_id":session, "cwd":"/tmp/demo", "hook_event_name":event,
            "tool_name":tool, "tool_input":{"prompt":"PRIVATE"}, "tool_output_text":"PRIVATE",
            "transcript_path":"/must/not/be/read"});
        writeln!(host.0.stdin.as_mut().unwrap(), "{payload}").unwrap();
        loop {
            let mut line = String::new();
            assert!(
                output.read_line(&mut line).unwrap() > 0,
                "fixture host exited before helper completed"
            );
            if line.trim() == "UPDATED" {
                break;
            }
        }
        let agents = provider.discover().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id.provider, "vibe");
        assert_eq!(agents[0].id.session_id, session);
        assert_eq!(agents[0].pid, host.0.id() as i32);
        assert_eq!(agents[0].state, expected);
        assert_eq!(agents[0].project_name, "demo");
        assert!(agents[0].context_usage.is_none());
        assert!(agents[0].focus_target.host_pid > 1);
    }
    assert!(watcher
        .changes
        .recv_timeout(std::time::Duration::from_secs(3))
        .is_ok());
    let status_path = provider
        .runtime_root()
        .join(format!("{}.json", host.0.id()));
    let saved_status = fs::read(&status_path).unwrap();
    let live_path = provider
        .runtime_root()
        .join(format!("{}.live.json", host.0.id()));
    let mut live: serde_json::Value = serde_json::from_slice(&saved_status).unwrap();
    live["source"] = json!("app_server");
    live["state"] = json!("active");
    live["context_tokens"] = json!(50_000);
    live["context_window"] = json!(100_000);
    fs::write(&live_path, serde_json::to_vec(&live).unwrap()).unwrap();
    let agents = provider.discover().unwrap();
    assert_eq!(agents.len(), 1, "hooks and live events share one card");
    assert_eq!(
        agents[0].state,
        AgentState::Active,
        "completion hooks cannot overwrite live state"
    );
    assert_eq!(agents[0].context_usage.unwrap().used_percentage(), 50.0);
    live["timestamp"] = json!("1970-01-01T00:00:00Z");
    fs::write(&live_path, serde_json::to_vec(&live).unwrap()).unwrap();
    assert_eq!(
        provider.discover().unwrap()[0].state,
        AgentState::Idle,
        "stale live record must fall back to valid hooks"
    );
    fs::remove_file(&live_path).unwrap();
    let mut old: serde_json::Value = serde_json::from_slice(&saved_status).unwrap();
    old["timestamp"] = json!("1970-01-01T00:00:00Z");
    fs::write(&status_path, serde_json::to_vec(&old).unwrap()).unwrap();
    assert!(
        provider.discover().unwrap().is_empty(),
        "reused PIDs must not revive old sessions"
    );
    fs::write(&status_path, saved_status).unwrap();
    host.0.stdin.take();
    assert!(host.0.wait().unwrap().success());
    assert!(provider.discover().unwrap().is_empty());
}

#[test]
#[ignore = "subprocess fixture launched by the integration test"]
fn fake_vibe_host() {
    assert!(std::env::var_os("AGENTSMON_TEST_VIBE_HOST").is_some());
    for line in std::io::stdin().lock().lines() {
        let mut helper = Command::new(env!("CARGO_BIN_EXE_agentsmon-helper"))
            .arg("vibe-hook")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        helper
            .stdin
            .take()
            .unwrap()
            .write_all(line.unwrap().as_bytes())
            .unwrap();
        let result = helper.wait_with_output().unwrap();
        assert!(result.status.success());
        assert!(
            result.stdout.is_empty(),
            "the hook must not alter Vibe's execution"
        );
        println!("UPDATED");
        std::io::stdout().flush().unwrap();
    }
}
