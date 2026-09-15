//! Process ancestry shared by terminal-based providers.

use std::collections::HashSet;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

pub(crate) fn refresh_process_trees(system: &mut System, seeds: &[i32]) {
    let refresh_kind = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .with_exe(UpdateKind::OnlyIfNotSet);
    let mut seen = HashSet::new();
    let mut frontier: Vec<_> = seeds
        .iter()
        .copied()
        .filter(|pid| *pid > 1)
        .map(|pid| Pid::from_u32(pid as u32))
        .filter(|pid| seen.insert(*pid))
        .collect();
    for _ in 0..10 {
        if frontier.is_empty() {
            break;
        }
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&frontier), false, refresh_kind);
        frontier = frontier
            .iter()
            .filter_map(|pid| system.process(*pid)?.parent())
            .filter(|pid| pid.as_u32() > 1 && seen.insert(*pid))
            .collect();
    }
}

pub(crate) fn find_host(
    system: &System,
    pid: i32,
    reported_parent: i32,
    focused_app_pid: Option<i32>,
) -> (i32, Option<String>) {
    let start = (reported_parent > 1)
        .then_some(reported_parent)
        .filter(|parent| system.process(Pid::from_u32(*parent as u32)).is_some())
        .unwrap_or(pid.max(0));
    let mut current = Pid::from_u32(start as u32);
    let mut candidate = pid;
    for _ in 0..10 {
        let Some(process) = system.process(current) else {
            break;
        };
        candidate = current.as_u32() as i32;
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        let executable = process
            .exe()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let searchable = format!("{name} {}", executable.to_ascii_lowercase());
        if [
            "terminal.app",
            "iterm2",
            "warp.app",
            "ghostty",
            "wezterm",
            "alacritty",
            "kitty",
            "visual studio code.app",
            "zed.app",
            "cursor.app",
            "windowsterminal",
            "claude.app/contents/macos/claude",
        ]
        .iter()
        .any(|host| searchable.contains(host))
        {
            return (candidate, Some(executable));
        }
        let Some(parent) = process.parent() else {
            break;
        };
        current = parent;
    }
    if let Some(focused) = focused_app_pid.filter(|pid| *pid > 1) {
        if let Some(process) = system.process(Pid::from_u32(focused as u32)) {
            let executable = process
                .exe()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|| process.name().to_string_lossy().into_owned());
            return (focused, Some(executable));
        }
    }
    (candidate, None)
}
