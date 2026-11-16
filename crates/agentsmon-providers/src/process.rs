//! Process ancestry shared by terminal-based providers.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};
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

/// Every process, with the fields `shell_child_started` reads.
pub(crate) fn all_processes() -> System {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        false,
        ProcessRefreshKind::nothing(),
    );
    system
}

/// Whether `parent` has a live shell child started after `after` and no
/// later than `until`. Start times are whole seconds: a shell started within
/// the second of `after` does not count. Only the name, the parent and the
/// start time of a process are read, never its command line.
pub(crate) fn shell_child_started(
    system: &System,
    parent: i32,
    after: SystemTime,
    until: SystemTime,
) -> bool {
    let seconds = |time: SystemTime| time.duration_since(UNIX_EPOCH).map(|since| since.as_secs());
    let (Ok(after), Ok(until)) = (seconds(after), seconds(until)) else {
        return false;
    };
    let parent = Pid::from_u32(parent as u32);
    system.processes().values().any(|process| {
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        process.parent() == Some(parent)
            && process.thread_kind().is_none()
            && matches!(
                name.trim_start_matches('-').trim_end_matches(".exe"),
                "zsh" | "bash" | "sh"
            )
            && process.start_time() > after
            && process.start_time() <= until
    })
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Child, Command};
    use std::time::Duration;

    struct Killed(Child);

    impl Drop for Killed {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn finds_only_shell_children_started_in_the_window() {
        let me = std::process::id() as i32;
        let before = SystemTime::now() - Duration::from_secs(2);
        let after = SystemTime::now() + Duration::from_secs(5);
        let started = || shell_child_started(&all_processes(), me, before, after);

        let _other = Killed(Command::new("sleep").arg("30").spawn().unwrap());
        assert!(!started());

        // The trailing command keeps the shell from exec'ing `sleep`.
        let _shell = Killed(
            Command::new("sh")
                .args(["-c", "sleep 30; :"])
                .spawn()
                .unwrap(),
        );
        assert!(started());
        let system = all_processes();
        // Started before the window opens, or after it closed.
        assert!(!shell_child_started(&system, me, after, after));
        assert!(!shell_child_started(
            &system,
            me,
            before - Duration::from_secs(60),
            before
        ));
        // Another process's children are not ours.
        assert!(!shell_child_started(&system, 1, before, after));
    }
}
