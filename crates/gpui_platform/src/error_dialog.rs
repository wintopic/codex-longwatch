/// Show a fatal startup/runtime error without requiring a terminal window.
pub fn show_error_dialog(title: &str, message: &str) {
    platform_show_error_dialog(title, message);
}

/// Ask the user to confirm closing while a queue task is active.
#[must_use]
pub fn confirm_exit_while_running() -> bool {
    platform_confirm_exit_while_running()
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn platform_show_error_dialog(title: &str, message: &str) {
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TASKMODAL, MessageBoxW,
        },
        core::HSTRING,
    };

    let title = HSTRING::from(title);
    let message = HSTRING::from(message);
    // SAFETY: both HSTRING values remain alive for the duration of the modal
    // call, and a null owner is valid for a process-level startup error.
    unsafe {
        MessageBoxW(
            None,
            &message,
            &title,
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TASKMODAL,
        );
    }
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn platform_confirm_exit_while_running() -> bool {
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TASKMODAL, MB_YESNO,
            MessageBoxW,
        },
        core::w,
    };

    // SAFETY: static UTF-16 strings remain valid for the modal call and a
    // null owner is valid for this process-level confirmation.
    unsafe {
        MessageBoxW(
            None,
            w!("任务仍在进行中。关闭 Longwatch 将停止后续重试，确定退出吗？"),
            w!("确认退出 Longwatch"),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SETFOREGROUND | MB_TASKMODAL,
        ) == IDYES
    }
}

#[cfg(not(target_os = "windows"))]
fn platform_show_error_dialog(title: &str, message: &str) {
    eprintln!("{title}: {message}");
}

#[cfg(not(target_os = "windows"))]
fn platform_confirm_exit_while_running() -> bool {
    true
}
