/// Show a fatal startup/runtime error without requiring a terminal window.
///
/// On Windows the message box runs on its own thread and this call blocks
/// until it is dismissed. A modal box on the UI thread would pump the GPUI
/// window's messages from inside whatever callback is running and re-enter
/// the app while it is borrowed (`RefCell already borrowed`).
pub fn show_error_dialog(title: &str, message: &str) {
    platform_show_error_dialog(title, message);
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

    let title = title.to_owned();
    let message = message.to_owned();
    let dialog = std::thread::Builder::new()
        .name("longwatch-error-dialog".into())
        .spawn(move || {
            let title = HSTRING::from(title);
            let message = HSTRING::from(message);
            // SAFETY: both HSTRING values remain alive for the duration of
            // the modal call, and a null owner is valid for a process-level
            // error.
            unsafe {
                MessageBoxW(
                    None,
                    &message,
                    &title,
                    MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TASKMODAL,
                );
            }
        });
    if let Ok(dialog) = dialog {
        let _ = dialog.join();
    }
}

#[cfg(not(target_os = "windows"))]
fn platform_show_error_dialog(title: &str, message: &str) {
    eprintln!("{title}: {message}");
}
