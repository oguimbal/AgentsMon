use agentsmon_core::FocusTarget;
#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::{bail, Result};

pub fn focus(target: &FocusTarget) -> Result<()> {
    if let Some(uri) = target.uri.as_deref() {
        return open_uri(uri);
    }
    if target.host_pid <= 0 {
        bail!("invalid host pid {}", target.host_pid);
    }
    if let Some(pane) = &target.tmux_pane {
        let mut select_window = std::process::Command::new("tmux");
        if let Some(socket) = &target.tmux_socket {
            select_window.args(["-S", socket]);
        }
        let _ = select_window.args(["select-window", "-t", pane]).status();

        let mut select_pane = std::process::Command::new("tmux");
        if let Some(socket) = &target.tmux_socket {
            select_pane.args(["-S", socket]);
        }
        let status = select_pane.args(["select-pane", "-t", pane]).status();
        if status.is_ok_and(|status| status.success()) {
            return focus_host(target);
        }
    }
    focus_host(target)
}

#[cfg(target_os = "macos")]
fn open_uri(uri: &str) -> Result<()> {
    let status = std::process::Command::new("open").arg(uri).status()?;
    if !status.success() {
        bail!("macOS could not open {uri}");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_uri(uri: &str) -> Result<()> {
    let status = std::process::Command::new("cmd")
        .args(["/C", "start", "", uri])
        .status()?;
    if !status.success() {
        bail!("Windows could not open {uri}");
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn open_uri(uri: &str) -> Result<()> {
    let status = std::process::Command::new("xdg-open").arg(uri).status()?;
    if !status.success() {
        bail!("could not open {uri}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn focus_host(target: &FocusTarget) -> Result<()> {
    let is_iterm = target
        .host_app
        .as_deref()
        .is_some_and(|app| app.to_ascii_lowercase().contains("iterm"));
    if is_iterm {
        if let Some(session_id) = target.terminal_session_id.as_deref() {
            if focus_iterm_session(session_id).is_ok() {
                return Ok(());
            }
        }
    }
    focus_pid(target.host_pid)
}

#[cfg(target_os = "macos")]
fn focus_iterm_session(session_id: &str) -> Result<()> {
    let unique_id = session_id.split_once(':').map_or(session_id, |(_, id)| id);
    if unique_id.is_empty()
        || !unique_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        bail!("invalid iTerm session id");
    }
    let script = r#"
on run argv
    set targetId to item 1 of argv
    tell application "iTerm2"
        repeat with aWindow in windows
            repeat with aTab in tabs of aWindow
                repeat with aSession in sessions of aTab
                    if unique ID of aSession is targetId then
                        select aTab
                        tell aWindow to select
                        activate
                        return
                    end if
                end repeat
            end repeat
        end repeat
    end tell
    error "iTerm session not found"
end run
"#;
    let status = std::process::Command::new("osascript")
        .args(["-e", script, unique_id])
        .status()?;
    if !status.success() {
        bail!("iTerm did not select session {unique_id}");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn focus_host(target: &FocusTarget) -> Result<()> {
    focus_pid(target.host_pid)
}

#[cfg(target_os = "macos")]
fn focus_pid(pid: i32) -> Result<()> {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .with_context(|| format!("no running application for pid {pid}"))?;
    if app.isHidden() {
        let _ = app.unhide();
    }
    #[allow(deprecated)]
    let activated = app.activateWithOptions(
        NSApplicationActivationOptions::ActivateAllWindows
            | NSApplicationActivationOptions::ActivateIgnoringOtherApps,
    );
    if !activated {
        bail!("macOS refused to activate pid {pid}");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn focus_pid(pid: i32) -> Result<()> {
    use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible, SetForegroundWindow, ShowWindow,
        SW_RESTORE,
    };
    static TARGET: AtomicU32 = AtomicU32::new(0);
    static FOUND: AtomicIsize = AtomicIsize::new(0);
    unsafe extern "system" fn each(hwnd: HWND, _: LPARAM) -> BOOL {
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
        if owner == TARGET.load(Ordering::Relaxed) && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            FOUND.store(hwnd.0 as isize, Ordering::Relaxed);
            return BOOL(0);
        }
        BOOL(1)
    }
    TARGET.store(pid as u32, Ordering::Relaxed);
    FOUND.store(0, Ordering::Relaxed);
    unsafe { EnumWindows(Some(each), LPARAM(0))? };
    let raw = FOUND.load(Ordering::Relaxed);
    if raw == 0 {
        bail!("no visible host window for pid {pid}");
    }
    let hwnd = HWND(raw as *mut _);
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        if !SetForegroundWindow(hwnd).as_bool() {
            bail!("Windows refused to activate pid {pid}");
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn focus_pid(_pid: i32) -> Result<()> {
    bail!("focusing is currently supported on macOS and Windows")
}
