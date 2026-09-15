use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "PreCompact",
    "PostCompact",
    "Stop",
];

pub fn install() -> Result<()> {
    if !codex_available() {
        return Ok(());
    }
    let helper_source = super::integration::helper_source()?;
    let dirs = directories::ProjectDirs::from("com", "oliv", "AgentsMon")
        .context("cannot resolve AgentsMon data directory")?;
    let stable_helper = dirs
        .data_dir()
        .join("bin")
        .join(super::integration::helper_filename());
    fs::create_dir_all(stable_helper.parent().expect("helper has parent"))?;
    fs::copy(&helper_source, &stable_helper)
        .with_context(|| format!("copy helper from {}", helper_source.display()))?;
    super::integration::make_executable(&stable_helper)?;
    install_hooks(&stable_helper)
}

/// Ask Codex itself whether the AgentsMon hooks are allowed to run.
///
/// Trust is intentionally never granted here: Codex command hooks can execute
/// outside its sandbox, so only the user may approve them in ChatGPT settings.
pub fn hooks_need_approval() -> Result<bool> {
    let executable = codex_executable().context("Codex executable not found")?;
    let mut child = Command::new(executable)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start Codex app-server")?;

    let stdout = child
        .stdout
        .take()
        .context("capture Codex app-server stdout")?;
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let result = line
                .context("read Codex app-server response")
                .and_then(|line| {
                    serde_json::from_str::<Value>(&line).context("parse Codex app-server response")
                });
            if sender.send(result).is_err() {
                break;
            }
        }
    });

    let cwd = std::env::current_dir().unwrap_or_else(|_| home_dir());
    let stdin = child
        .stdin
        .as_mut()
        .context("open Codex app-server stdin")?;
    writeln!(
        stdin,
        "{}",
        json!({
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": {
                    "name": "agentsmon",
                    "title": "AgentsMon",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        })
    )?;
    writeln!(
        stdin,
        "{}",
        json!({ "method": "initialized", "params": {} })
    )?;
    writeln!(
        stdin,
        "{}",
        json!({
            "method": "hooks/list",
            "id": 2,
            "params": { "cwds": [cwd] }
        })
    )?;
    stdin.flush()?;

    let deadline = Instant::now() + Duration::from_secs(5);
    let result = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break Err(anyhow::anyhow!("Codex hook status check timed out"));
        }
        match receiver.recv_timeout(remaining) {
            Ok(Ok(response)) if response.get("id").and_then(Value::as_u64) == Some(2) => {
                break response_needs_approval(&response);
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => break Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                break Err(anyhow::anyhow!("Codex hook status check timed out"));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(anyhow::anyhow!("Codex app-server closed unexpectedly"));
            }
        }
    };

    let _ = child.kill();
    let _ = child.wait();
    drop(receiver);
    let _ = reader.join();
    result
}

fn response_needs_approval(response: &Value) -> Result<bool> {
    let entries = response
        .pointer("/result/data")
        .and_then(Value::as_array)
        .context("Codex hooks/list response has no data")?;
    let mut found = false;
    for hook in entries
        .iter()
        .filter_map(|entry| entry.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter(|hook| contains_owned_command(hook))
    {
        found = true;
        if !matches!(
            hook.get("trustStatus").and_then(Value::as_str),
            Some("trusted" | "managed")
        ) {
            return Ok(true);
        }
    }
    Ok(!found)
}

fn install_hooks(helper: &Path) -> Result<()> {
    let path = hooks_path();
    let mut root = if path.exists() {
        serde_json::from_slice::<Value>(&fs::read(&path)?).context("parse Codex hooks.json")?
    } else {
        json!({})
    };
    let Some(root_object) = root.as_object_mut() else {
        bail!("Codex hooks.json must contain a JSON object; no changes were made");
    };
    let hooks = root_object.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("Codex hooks.json field `hooks` must be an object; no changes were made");
    };
    let command = format!("\"{}\" codex-hook", helper.display());

    for event in EVENTS {
        let groups = hooks.entry(*event).or_insert_with(|| json!([]));
        let Some(groups) = groups.as_array_mut() else {
            bail!("Codex hook event `{event}` must be an array; no changes were made");
        };
        groups.retain(|group| !contains_owned_command(group));
        groups.push(json!({
            "hooks": [{
                "type": "command",
                "command": command,
                "timeout": 5
            }]
        }));
    }
    write_atomic(&path, &serde_json::to_vec_pretty(&root)?)
}

fn contains_owned_command(value: &Value) -> bool {
    match value {
        Value::String(value) => value.contains("agentsmon-helper") && value.contains("codex-hook"),
        Value::Array(values) => values.iter().any(contains_owned_command),
        Value::Object(values) => values.values().any(contains_owned_command),
        _ => false,
    }
}

fn codex_available() -> bool {
    codex_executable().is_some_and(|executable| {
        Command::new(executable)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

fn codex_executable() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(target_os = "windows") {
        &["codex.exe", "codex.cmd", "codex"]
    } else {
        &["codex"]
    };
    let mut candidates = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .collect::<Vec<_>>();
    let home = home_dir();
    for name in names {
        candidates.push(home.join(".local/bin").join(name));
    }
    #[cfg(target_os = "macos")]
    {
        // Since the July 2026 desktop merge, Codex ships inside ChatGPT.app while
        // keeping the `com.openai.codex` bundle identifier and `codex` URL scheme.
        // Keep the legacy location for machines that have not migrated yet.
        candidates.push(PathBuf::from(
            "/Applications/ChatGPT.app/Contents/Resources/codex",
        ));
        candidates.push(PathBuf::from(
            "/Applications/Codex.app/Contents/Resources/codex",
        ));
        candidates.push(PathBuf::from("/opt/homebrew/bin/codex"));
        candidates.push(PathBuf::from("/usr/local/bin/codex"));
    }
    #[cfg(target_os = "windows")]
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        for name in names {
            candidates.push(
                PathBuf::from(&local_app_data)
                    .join("Programs/ChatGPT/resources")
                    .join(name),
            );
            candidates.push(
                PathBuf::from(&local_app_data)
                    .join("Programs/Codex/resources")
                    .join(name),
            );
        }
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn hooks_path() -> PathBuf {
    home_dir().join(".codex/hooks.json")
}

fn home_dir() -> PathBuf {
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().expect("hooks path has parent"))?;
    let temporary = path.with_extension("agentsmon.tmp");
    fs::write(&temporary, contents)?;
    replace_atomically(&temporary, path)
}

#[cfg(not(target_os = "windows"))]
fn replace_atomically(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn replace_atomically(source: &Path, destination: &Path) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_agentsmon_codex_hooks() {
        assert!(contains_owned_command(&json!({
            "hooks": [{ "command": "agentsmon-helper codex-hook" }]
        })));
        assert!(!contains_owned_command(&json!({
            "hooks": [{ "command": "my-hook" }]
        })));
    }

    #[test]
    fn detects_untrusted_or_missing_agentsmon_hooks() {
        let trusted = json!({
            "result": { "data": [{ "hooks": [{
                "command": "agentsmon-helper codex-hook",
                "trustStatus": "trusted"
            }]}]}
        });
        let untrusted = json!({
            "result": { "data": [{ "hooks": [{
                "command": "agentsmon-helper codex-hook",
                "trustStatus": "untrusted"
            }]}]}
        });
        let unrelated = json!({
            "result": { "data": [{ "hooks": [{
                "command": "another-helper",
                "trustStatus": "trusted"
            }]}]}
        });

        assert!(!response_needs_approval(&trusted).unwrap());
        assert!(response_needs_approval(&untrusted).unwrap());
        assert!(response_needs_approval(&unrelated).unwrap());
    }
}
