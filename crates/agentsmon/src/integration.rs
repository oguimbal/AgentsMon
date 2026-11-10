use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentsmon_ui::DiagnosticsStatus;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const MARKETPLACE: &str = include_str!("../../../.claude-plugin/marketplace.json");
const PLUGIN_MANIFEST: &str = include_str!("../../../claude-plugin/.claude-plugin/plugin.json");
const HOOKS: &str = include_str!("../../../claude-plugin/hooks/hooks.json");

pub fn inspect() -> DiagnosticsStatus {
    let claude_found = claude_executable().is_some_and(|executable| {
        Command::new(executable)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    });
    let plugins = installed_plugin_names();
    let plugin_found = plugins.iter().any(|name| name.starts_with("agentsmon@"));
    let hook_found = plugin_found
        || plugins
            .iter()
            .any(|name| name.starts_with("claude-status@"));
    let statusline = read_settings()
        .ok()
        .and_then(|settings| settings.get("statusLine").cloned());
    let existing_statusline = statusline
        .as_ref()
        .and_then(|value| value.get("command"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let statusline_wrapped = statusline.as_ref().is_some_and(owns_statusline);
    DiagnosticsStatus {
        claude_found,
        hook_found,
        plugin_found,
        statusline_wrapped,
        existing_statusline,
        message: None,
    }
}

fn installed_plugin_names() -> Vec<String> {
    let registry = home_dir().join(".claude/plugins/installed_plugins.json");
    fs::read(registry)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|json| json.get("plugins").and_then(Value::as_object).cloned())
        .map(|plugins| plugins.into_iter().map(|(name, _)| name).collect())
        .unwrap_or_default()
}

pub fn install() -> Result<()> {
    let dirs = project_dirs()?;
    let data = dirs.data_dir();
    let helper_source = helper_source()?;
    let helper_name = helper_filename();
    let stable_helper = data.join("bin").join(helper_name);
    fs::create_dir_all(stable_helper.parent().unwrap())?;
    fs::copy(&helper_source, &stable_helper)
        .with_context(|| format!("copy helper from {}", helper_source.display()))?;
    make_executable(&stable_helper)?;
    sync_plugin_bundle(data, helper_name, &stable_helper)?;

    // Compatible status plugins can provide initial discovery, but some do not
    // subscribe to Stop or tool lifecycle events. Install our hooks alongside
    // them so waiting states are always cleared authoritatively.
    if !inspect().plugin_found {
        install_plugin(data)?;
    } else {
        repair_installed_plugins(helper_name, &stable_helper)?;
    }

    install_statusline_proxy(&stable_helper, dirs.config_dir())
}

fn repair_installed_plugins(helper_name: &str, stable_helper: &Path) -> Result<()> {
    for plugin in installed_agentsmon_paths() {
        let helper = plugin.join("bin").join(helper_name);
        fs::create_dir_all(helper.parent().unwrap())?;
        fs::copy(stable_helper, &helper)?;
        make_executable(&helper)?;
        write_text(&plugin.join("hooks/hooks.json"), HOOKS)?;
    }
    Ok(())
}

fn sync_plugin_bundle(data: &Path, helper_name: &str, stable_helper: &Path) -> Result<()> {
    let marketplace = data.join("marketplace");
    write_text(
        &marketplace.join(".claude-plugin/marketplace.json"),
        MARKETPLACE,
    )?;
    write_text(
        &marketplace.join("claude-plugin/.claude-plugin/plugin.json"),
        PLUGIN_MANIFEST,
    )?;
    write_text(&marketplace.join("claude-plugin/hooks/hooks.json"), HOOKS)?;
    let plugin_helper = marketplace.join("claude-plugin/bin").join(helper_name);
    fs::create_dir_all(plugin_helper.parent().unwrap())?;
    fs::copy(stable_helper, &plugin_helper)?;
    make_executable(&plugin_helper)
}

fn installed_agentsmon_paths() -> Vec<PathBuf> {
    let registry = home_dir().join(".claude/plugins/installed_plugins.json");
    fs::read(registry)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|json| {
            json.get("plugins")?
                .get("agentsmon@agentsmon-local")?
                .as_array()
                .cloned()
        })
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .get("installPath")
                .and_then(Value::as_str)
                .map(PathBuf::from)
        })
        .collect()
}

fn install_plugin(data: &Path) -> Result<()> {
    let claude = claude_executable()
        .context("Claude Code not found. Check its installation, then restart AgentsMon.")?;
    let marketplace = data.join("marketplace");

    let _ = Command::new(&claude)
        .args(["plugin", "marketplace", "add"])
        .arg(&marketplace)
        .status();
    let installed = Command::new(claude)
        .args([
            "plugin",
            "install",
            "agentsmon@agentsmon-local",
            "--scope",
            "user",
        ])
        .status()
        .context("run claude plugin install")?;
    if !installed.success() {
        bail!("claude plugin install failed with {installed}");
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    if let Some(claude) = claude_executable() {
        let _ = Command::new(claude)
            .args(["plugin", "uninstall", "agentsmon@agentsmon-local"])
            .status();
    }
    restore_statusline()
}

fn claude_executable() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(target_os = "windows") {
        &["claude.exe", "claude.cmd", "claude"]
    } else {
        &["claude"]
    };
    let mut candidates = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .collect::<Vec<_>>();

    let home = home_dir();
    for name in names {
        candidates.push(home.join(".local/bin").join(name));
        candidates.push(home.join(".claude/local").join(name));
    }
    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from("/opt/homebrew/bin/claude"));
        candidates.push(PathBuf::from("/usr/local/bin/claude"));
    }
    #[cfg(target_os = "windows")]
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        for name in names {
            candidates.push(
                PathBuf::from(&local_app_data)
                    .join("Programs/Claude")
                    .join(name),
            );
        }
    }

    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn install_statusline_proxy(helper: &Path, config_dir: &Path) -> Result<()> {
    let mut settings = read_settings()?;
    let original = settings.get("statusLine").cloned();
    let command = format!("\"{}\" statusline-proxy", helper.display());
    if original.as_ref().is_some_and(owns_statusline) {
        // Ours already: the saved original stays. Only a proxy whose helper no
        // longer exists (data directory moved) is pointed at the current one.
        let missing = original
            .as_ref()
            .and_then(|value| value.get("command")?.as_str())
            .and_then(proxy_helper_path)
            .is_some_and(|path| !Path::new(path).is_file());
        if missing {
            settings["statusLine"]["command"] = json!(command);
            return write_settings(&settings);
        }
        return Ok(());
    }
    fs::create_dir_all(config_dir)?;
    if let Some(original) = original.as_ref() {
        write_atomic(
            config_dir.join("statusline-original.json"),
            &serde_json::to_vec_pretty(original)?,
        )?;
    } else {
        let marker = config_dir.join("statusline-original.json");
        if marker.exists() {
            fs::remove_file(marker)?;
        }
    }
    settings["statusLine"] = json!({
        "type": "command",
        "command": command,
        "refreshInterval": 3
    });
    write_settings(&settings)
}

fn restore_statusline() -> Result<()> {
    let dirs = project_dirs()?;
    let mut settings = read_settings()?;
    if !settings.get("statusLine").is_some_and(owns_statusline) {
        return Ok(());
    }
    let backup = dirs.config_dir().join("statusline-original.json");
    let original = backup
        .exists()
        .then(|| fs::read(&backup))
        .transpose()?
        .map(|bytes| serde_json::from_slice(&bytes))
        .transpose()?;
    restore_statusline_value(&mut settings, original);
    write_settings(&settings)
}

fn owns_statusline(value: &Value) -> bool {
    value
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| {
            command.contains("agentsmon-helper") && command.contains("statusline-proxy")
        })
}

/// The helper path of a command written by `install_statusline_proxy`.
fn proxy_helper_path(command: &str) -> Option<&str> {
    command
        .strip_prefix('"')?
        .strip_suffix("\" statusline-proxy")
}

fn restore_statusline_value(settings: &mut Value, original: Option<Value>) {
    if let Some(original) = original {
        settings["statusLine"] = original;
    } else if let Some(object) = settings.as_object_mut() {
        object.remove("statusLine");
    }
}

fn read_settings() -> Result<Value> {
    let path = settings_path();
    if !path.exists() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&fs::read(path)?).context("parse Claude settings.json")
}

fn write_settings(settings: &Value) -> Result<()> {
    let path = settings_path();
    fs::create_dir_all(path.parent().unwrap())?;
    write_atomic(path, &serde_json::to_vec_pretty(settings)?)
}

fn write_atomic(path: impl AsRef<Path>, contents: &[u8]) -> Result<()> {
    let path = path.as_ref();
    let temp = path.with_extension("agentsmon.tmp");
    fs::write(&temp, contents)?;
    replace_atomically(&temp, path)
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

fn settings_path() -> PathBuf {
    home_dir().join(".claude/settings.json")
}

fn home_dir() -> PathBuf {
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn project_dirs() -> Result<directories::ProjectDirs> {
    agentsmon_providers::app_dirs::project_dirs().context("cannot resolve AgentsMon data directory")
}

pub(crate) fn helper_source() -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let sibling = executable.with_file_name(helper_filename());
    if sibling.exists() {
        return Ok(sibling);
    }
    if let Some(contents) = executable.parent().and_then(Path::parent) {
        let bundled = contents.join("Resources/bin").join(helper_filename());
        if bundled.exists() {
            return Ok(bundled);
        }
    }
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for profile in ["debug", "release"] {
        let candidate = workspace
            .join("target")
            .join(profile)
            .join(helper_filename());
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!("agentsmon-helper is not built; run cargo build -p agentsmon-helper")
}

pub(crate) fn helper_filename() -> &'static str {
    if cfg!(target_os = "windows") {
        "agentsmon-helper.exe"
    } else {
        "agentsmon-helper"
    }
}

fn write_text(path: &Path, contents: &str) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, contents)?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn detects_our_proxy_without_matching_arbitrary_helpers() {
        assert!(super::owns_statusline(&json!({
            "command": "\"/tmp/agentsmon-helper\" statusline-proxy"
        })));
        assert!(!super::owns_statusline(&json!({
            "command": "my-statusline.sh"
        })));
    }

    #[test]
    fn reads_the_helper_path_of_our_proxy_only() {
        assert_eq!(
            super::proxy_helper_path("\"/data dir/bin/agentsmon-helper\" statusline-proxy"),
            Some("/data dir/bin/agentsmon-helper")
        );
        assert_eq!(
            super::proxy_helper_path("agentsmon-helper statusline-proxy --mine"),
            None
        );
    }

    #[test]
    fn restoration_preserves_later_user_edits() {
        let mut edited = json!({ "statusLine": { "command": "user-new-command" } });
        if edited.get("statusLine").is_some_and(super::owns_statusline) {
            super::restore_statusline_value(&mut edited, Some(json!({ "command": "old" })));
        }
        assert_eq!(edited["statusLine"]["command"], "user-new-command");

        let mut ours = json!({
            "statusLine": { "command": "agentsmon-helper statusline-proxy" }
        });
        super::restore_statusline_value(&mut ours, Some(json!({ "command": "original" })));
        assert_eq!(ours["statusLine"]["command"], "original");
    }
}
