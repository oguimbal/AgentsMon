#[cfg(target_os = "macos")]
pub fn application_pid() -> Option<i32> {
    use objc2_app_kit::NSWorkspace;

    let application = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    Some(application.processIdentifier())
}

#[cfg(target_os = "windows")]
pub fn application_pid() -> Option<i32> {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    let window = unsafe { GetForegroundWindow() };
    if window.is_invalid() {
        return None;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    (pid > 0).then_some(pid as i32)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn application_pid() -> Option<i32> {
    None
}
