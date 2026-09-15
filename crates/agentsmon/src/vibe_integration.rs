use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

#[derive(Clone, Copy)]
enum HookFormat {
    Experimental,
    Stable,
}

pub fn install() -> Result<()> {
    let Some(executable) = vibe_executable() else {
        return Ok(());
    };
    let output = Command::new(&executable)
        .arg("--version")
        .output()
        .context("get Vibe version")?;
    if !output.status.success() {
        bail!("cannot determine Mistral Vibe version")
    }
    let format = hook_format(&String::from_utf8_lossy(&output.stdout))?;
    let home = agentsmon_providers::vibe::home_dir();
    let dirs = directories::ProjectDirs::from("com", "oliv", "AgentsMon")
        .context("cannot resolve AgentsMon data directory")?;
    let helper = dirs
        .data_dir()
        .join("bin")
        .join(super::integration::helper_filename());

    // Validate every edit before touching the user's files. Existing hooks and
    // TOML comments survive installation and version migrations.
    let hooks_path = home.join("hooks.toml");
    let hooks_before = read_optional(&hooks_path)?;
    let hooks_after = merged_hooks(&hooks_before, &helper, format)?;
    let config_path = home.join("config.toml");
    let config_update = if matches!(format, HookFormat::Experimental) {
        let before = read_optional(&config_path)?;
        let after = enable_experimental_hooks(&before)?;
        Some((before, after))
    } else {
        None
    };

    let source = super::integration::helper_source()?;
    fs::create_dir_all(helper.parent().context("helper has no parent")?)?;
    fs::copy(&source, &helper).context("install Vibe hook helper")?;
    super::integration::make_executable(&helper)?;
    if let Some((before, after)) = config_update {
        write_if_changed(&config_path, &before, &after)?;
    }
    write_if_changed(&hooks_path, &hooks_before, &hooks_after)?;
    install_observer(&executable)
}

const OBSERVER: &str = include_str!("vibe_observer.py");
const OBSERVER_PTH: &str = "import _agentsmon_vibe; _agentsmon_vibe.install()\n";

fn install_observer(executable: &Path) -> Result<()> {
    let executable = fs::canonicalize(executable)?;
    let bin = executable
        .parent()
        .context("Vibe executable has no parent")?;
    let python = [
        bin.join("python3"),
        bin.join("python"),
        bin.join("python.exe"),
        bin.parent().unwrap_or(bin).join("python.exe"),
    ]
    .into_iter()
    .find(|path| path.is_file());
    let Some(python) = python else {
        tracing::warn!(
            "Vibe has no isolated Python environment; only tool/completion hooks are available"
        );
        return Ok(());
    };
    // Do not install startup code into a global/system Python. Probe only
    // package metadata and the two adapter boundaries, without loading a session.
    let probe = r#"
import inspect, sys, sysconfig
from importlib.metadata import version
from pathlib import Path
if sys.prefix == sys.base_prefix:
    sys.exit(2)
parts = version('mistral-vibe').split('.')
if tuple(map(int, parts[:2])) < (2, 25):
    sys.exit(3)
from vibe.app_server.server import AppServer
assert inspect.iscoroutinefunction(AppServer._route_notification)
assert inspect.iscoroutinefunction(AppServer._activate_backend)
site = Path(sysconfig.get_path('purelib')).resolve()
assert site.is_relative_to(Path(sys.prefix).resolve())
print(site)
"#;
    let output = Command::new(python).args(["-c", probe]).output()?;
    if matches!(output.status.code(), Some(2 | 3)) {
        tracing::warn!("Full Vibe activity/context monitoring requires Vibe 2.25+ in an isolated Python environment");
        return Ok(());
    }
    if !output.status.success() {
        bail!("Vibe's app-server adapter is incompatible; tool/completion hooks remain installed")
    }
    let site = PathBuf::from(String::from_utf8(output.stdout)?.trim());
    if !site.is_absolute() || !site.is_dir() {
        bail!("invalid Vibe Python package directory")
    }
    install_observer_at(&site)
}

fn install_observer_at(site: &Path) -> Result<()> {
    let module = site.join("_agentsmon_vibe.py");
    let pth = site.join("_agentsmon_vibe.pth");
    let module_before = read_optional(&module)?;
    let pth_before = read_optional(&pth)?;
    if !module_before.is_empty() && !module_before.starts_with("\"\"\"AgentsMon's local Vibe") {
        bail!("Vibe observer module name is already in use; preserving the existing file")
    }
    if !pth_before.is_empty() && pth_before != OBSERVER_PTH {
        bail!("Vibe observer startup file was edited; preserving the existing file")
    }
    write_if_changed(&module, &module_before, OBSERVER)?;
    write_if_changed(&pth, &pth_before, OBSERVER_PTH)
}

fn hook_format(version: &str) -> Result<HookFormat> {
    let version = version
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))
        .context("unrecognized Mistral Vibe version")?;
    let mut parts = version.split('.');
    let major: u32 = parts
        .next()
        .context("missing Vibe major version")?
        .parse()?;
    let minor: u32 = parts
        .next()
        .context("missing Vibe minor version")?
        .parse()?;
    match (major, minor) {
        (2, 18..=20) => Ok(HookFormat::Experimental),
        (2, 21..) | (3.., _) => Ok(HookFormat::Stable),
        _ => bail!("AgentsMon requires Mistral Vibe 2.18 or newer"),
    }
}

fn merged_hooks(before: &str, helper: &Path, format: HookFormat) -> Result<String> {
    let mut doc = before
        .parse::<DocumentMut>()
        .context("parse Vibe hooks.toml")?;
    let command = hook_command(helper)?;
    if !doc.contains_key("hooks") {
        doc["hooks"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    // Vibe also accepts the equivalent inline hooks = [{...}] notation.
    if let Some(array) = doc["hooks"].as_array() {
        let mut tables = ArrayOfTables::new();
        for item in array {
            let table = item
                .as_inline_table()
                .context("Vibe hooks must be tables")?;
            tables.push(table.clone().into_table());
        }
        doc["hooks"] = Item::ArrayOfTables(tables);
    }
    let hooks = doc["hooks"]
        .as_array_of_tables_mut()
        .context("Vibe hooks must be an array of tables")?;
    let events = match format {
        HookFormat::Experimental => ["before_tool", "after_tool", "post_agent_turn"],
        HookFormat::Stable => ["pre_tool", "post_tool", "post_agent"],
    };
    for (name, event) in [
        "agentsmon-pre-tool",
        "agentsmon-post-tool",
        "agentsmon-post-agent",
    ]
    .into_iter()
    .zip(events)
    {
        let mut found = false;
        for hook in hooks
            .iter_mut()
            .filter(|hook| hook.get("name").and_then(Item::as_str) == Some(name))
        {
            if found {
                bail!("duplicate Vibe hook name {name}")
            }
            let existing = hook
                .get("command")
                .and_then(Item::as_str)
                .unwrap_or_default();
            if !existing.contains("agentsmon-helper") || !existing.ends_with(" vibe-hook") {
                bail!("Vibe hook {name} was edited; preserving it without changes");
            }
            hook["type"] = value(event);
            hook["command"] = value(&command);
            found = true;
        }
        if !found {
            let mut hook = Table::new();
            hook["name"] = value(name);
            hook["type"] = value(event);
            hook["command"] = value(&command);
            hook["timeout"] = value(5.0);
            hook["strict"] = value(false);
            hooks.push(hook);
        }
    }
    Ok(doc.to_string())
}

fn enable_experimental_hooks(before: &str) -> Result<String> {
    let mut doc = before
        .parse::<DocumentMut>()
        .context("parse Vibe config.toml")?;
    match doc.get("enable_experimental_hooks").map(Item::as_bool) {
        Some(Some(true)) => return Ok(before.into()),
        Some(_) => bail!("Vibe experimental hooks are explicitly disabled or invalid; enable enable_experimental_hooks in Vibe config.toml to connect AgentsMon"),
        None => doc["enable_experimental_hooks"] = value(true),
    }
    Ok(doc.to_string())
}

fn hook_command(helper: &Path) -> Result<String> {
    let path = helper.to_str().context("Vibe helper path is not UTF-8")?;
    if path.contains(['\n', '\r']) {
        bail!("Vibe helper path contains a newline")
    }
    #[cfg(target_os = "windows")]
    {
        // Vibe's command loader requires forward slashes on Windows as well.
        let path = path.replace('\\', "/");
        if path.contains(['"', '%']) {
            bail!("unsupported character in Vibe helper path")
        }
        Ok(format!("\"{path}\" vibe-hook"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        if path.contains('\\') {
            bail!("Vibe does not support backslashes in hook commands")
        }
        Ok(format!("'{}' vibe-hook", path.replace('\'', "'\"'\"'")))
    }
}

fn read_optional(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn write_if_changed(path: &Path, before: &str, after: &str) -> Result<()> {
    if before != after {
        super::codex_integration::write_atomic(path, after.as_bytes())?;
    }
    Ok(())
}

fn vibe_executable() -> Option<PathBuf> {
    let name = if cfg!(target_os = "windows") {
        "vibe.exe"
    } else {
        "vibe"
    };
    let mut candidates: Vec<_> = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .collect();
    if let Some(dirs) = directories::UserDirs::new() {
        candidates.push(dirs.home_dir().join(".local/bin").join(name));
    }
    #[cfg(target_os = "macos")]
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        candidates.push(Path::new(dir).join(name));
    }
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_install_is_idempotent_and_preserves_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        install_observer_at(dir.path()).unwrap();
        install_observer_at(dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("_agentsmon_vibe.py")).unwrap(),
            OBSERVER
        );
        let pth = dir.path().join("_agentsmon_vibe.pth");
        fs::write(&pth, "import my_custom_observer\n").unwrap();
        assert!(install_observer_at(dir.path()).is_err());
        assert_eq!(
            fs::read_to_string(pth).unwrap(),
            "import my_custom_observer\n"
        );
    }

    #[test]
    fn preserves_user_hooks_comments_and_is_idempotent_across_upgrade() {
        let before = "# My hook\n[[hooks]]\nname = 'guard'\ntype = 'before_tool'\ncommand = 'my-guard' # keep me\nstrict = true\n";
        let helper = Path::new("/Applications/AgentsMon App/agentsmon-helper");
        let legacy = merged_hooks(before, helper, HookFormat::Experimental).unwrap();
        assert!(legacy.starts_with(before));
        assert!(legacy.contains("post_agent_turn"));
        assert_eq!(
            legacy,
            merged_hooks(&legacy, helper, HookFormat::Experimental).unwrap()
        );
        let stable = merged_hooks(&legacy, helper, HookFormat::Stable).unwrap();
        assert!(stable.starts_with(before));
        assert!(!stable.contains("post_agent_turn"));
        assert_eq!(
            stable,
            merged_hooks(&stable, helper, HookFormat::Stable).unwrap()
        );
        assert_eq!(
            stable.parse::<DocumentMut>().unwrap()["hooks"]
                .as_array_of_tables()
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn accepts_inline_hooks_and_rejects_invalid_or_edited_config() {
        let helper = Path::new("/tmp/agentsmon-helper");
        for text in [
            "hooks = []",
            "hooks = [{name='other', type='pre_tool', command='guard'}]",
        ] {
            assert!(merged_hooks(text, helper, HookFormat::Stable).is_ok());
        }
        for text in [
            "hooks = 1",
            "invalid [",
            "[[hooks]]\nname='agentsmon-pre-tool'\ncommand='user-edit'",
        ] {
            assert!(merged_hooks(text, helper, HookFormat::Stable).is_err());
        }
        let config = "# Keep model\nactive_model = 'devstral'\n[tools.bash]\npermission='ask'\n";
        let enabled = enable_experimental_hooks(config).unwrap();
        assert!(enabled.contains("# Keep model"));
        assert_eq!(enabled, enable_experimental_hooks(&enabled).unwrap());
        assert!(enable_experimental_hooks("enable_experimental_hooks = false").is_err());
    }

    #[test]
    fn detects_hook_protocol_versions() {
        assert!(matches!(
            hook_format("vibe 2.18.0\n").unwrap(),
            HookFormat::Experimental
        ));
        assert!(matches!(
            hook_format("vibe 2.21.0").unwrap(),
            HookFormat::Stable
        ));
        assert!(matches!(
            hook_format("vibe 3.0.0").unwrap(),
            HookFormat::Stable
        ));
        assert!(hook_format("vibe 2.17.0").is_err());
        assert!(hook_format("unknown").is_err());
    }
}
