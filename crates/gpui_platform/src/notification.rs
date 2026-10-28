#[cfg(target_os = "windows")]
use std::sync::{Mutex, OnceLock};

use thiserror::Error;

/// Optional action attached to a successful queue notification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationAction {
    pub label: String,
    thread_id: Option<String>,
    codex_path: Option<String>,
}

impl NotificationAction {
    #[must_use]
    pub fn open_thread(thread_id: impl Into<String>, codex_path: impl Into<String>) -> Self {
        let thread_id = thread_id.into();
        Self {
            label: "查看".into(),
            thread_id: Some(thread_id),
            codex_path: Some(codex_path.into()),
        }
    }

    fn open(&self) -> Result<(), crate::OpenThreadError> {
        if let (Some(thread_id), Some(codex_path)) =
            (self.thread_id.as_deref(), self.codex_path.as_deref())
        {
            crate::open_thread(thread_id, codex_path)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Error)]
pub enum NotificationError {
    #[error("native notification failed: {0}")]
    Native(String),
}

/// Display a native success notification.
///
/// # Errors
///
/// Returns an error when the desktop notification service rejects the toast.
pub fn notify_success(
    title: &str,
    body: &str,
    action: Option<&NotificationAction>,
    audio_enabled: bool,
) -> Result<(), NotificationError> {
    platform_notify(title, body, action, audio_enabled)
}

#[cfg(target_os = "windows")]
fn platform_notify(
    title: &str,
    body: &str,
    action: Option<&NotificationAction>,
    audio_enabled: bool,
) -> Result<(), NotificationError> {
    let result = show_windows_toast(title, body, action, audio_enabled);
    if result.is_err() && audio_enabled {
        crate::completion_sound::play_completion_sound();
    }
    result
}

#[cfg(target_os = "windows")]
fn show_windows_toast(
    title: &str,
    body: &str,
    action: Option<&NotificationAction>,
    audio_enabled: bool,
) -> Result<(), NotificationError> {
    use windows::{
        Data::Xml::Dom::XmlDocument,
        Foundation::TypedEventHandler,
        UI::Notifications::{
            ToastDismissedEventArgs, ToastFailedEventArgs, ToastNotification,
            ToastNotificationManager,
        },
        core::{HSTRING, IInspectable},
    };

    let icon_path = crate::identity::notification_icon_path().ok_or_else(|| {
        NotificationError::Native("failed to prepare the Windows notification icon".into())
    })?;
    let xml = windows_toast_xml(title, body, action, audio_enabled);
    let document =
        XmlDocument::new().map_err(|error| NotificationError::Native(error.to_string()))?;
    document
        .LoadXml(&HSTRING::from(xml))
        .map_err(|error| NotificationError::Native(error.to_string()))?;
    let toast = ToastNotification::CreateToastNotification(&document)
        .map_err(|error| NotificationError::Native(error.to_string()))?;
    let action = action.cloned();
    let activated = TypedEventHandler::<ToastNotification, IInspectable>::new(move |_, _| {
        crate::stop_completion_overlay();
        if let Some(action) = action.as_ref()
            && let Err(error) = action.open()
        {
            crate::show_error_dialog("无法打开 Codex 结果", &error.to_string());
        }
        Ok(())
    });
    toast
        .Activated(&activated)
        .map_err(|error| NotificationError::Native(error.to_string()))?;
    let dismissed = TypedEventHandler::<ToastNotification, ToastDismissedEventArgs>::new(|_, _| {
        crate::stop_completion_overlay();
        Ok(())
    });
    toast
        .Dismissed(&dismissed)
        .map_err(|error| NotificationError::Native(error.to_string()))?;
    let failed = TypedEventHandler::<ToastNotification, ToastFailedEventArgs>::new(move |_, _| {
        crate::stop_completion_overlay();
        if audio_enabled {
            crate::completion_sound::play_completion_sound();
        }
        Ok(())
    });
    toast
        .Failed(&failed)
        .map_err(|error| NotificationError::Native(error.to_string()))?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(
        crate::identity::APP_USER_MODEL_ID,
    ))
    .map_err(|error| NotificationError::Native(error.to_string()))?;
    show_toast_with_icon_context(&notifier, &toast, Some(&icon_path))?;
    retain_windows_toast(toast);
    Ok(())
}

#[cfg(target_os = "windows")]
fn show_toast_with_icon_context(
    notifier: &windows::UI::Notifications::ToastNotifier,
    toast: &windows::UI::Notifications::ToastNotification,
    icon_path: Option<&std::path::Path>,
) -> Result<(), NotificationError> {
    notifier.Show(toast).map_err(|first_error| {
        let detail = icon_path.map_or_else(
            || "notification icon path unavailable".to_owned(),
            |path| format!("notification icon path: {}", path.display()),
        );
        NotificationError::Native(format!("{first_error}; {detail}"))
    })
}

/// Keeps recent toasts alive: a toast's `Activated` handler only fires while
/// the toast object exists, and several sessions can finish while an earlier
/// alarm toast is still waiting on screen.
#[cfg(target_os = "windows")]
fn retain_windows_toast(toast: windows::UI::Notifications::ToastNotification) {
    const MAX_RETAINED_TOASTS: usize = 16;
    static ACTIVE_TOASTS: OnceLock<
        Mutex<std::collections::VecDeque<windows::UI::Notifications::ToastNotification>>,
    > = OnceLock::new();
    if let Ok(mut active) = ACTIVE_TOASTS.get_or_init(Mutex::default).lock() {
        if active.len() == MAX_RETAINED_TOASTS {
            active.pop_front();
        }
        active.push_back(toast);
    }
}

#[cfg(target_os = "windows")]
fn windows_toast_xml(
    title: &str,
    body: &str,
    action: Option<&NotificationAction>,
    audio_enabled: bool,
) -> String {
    let title = escape_xml(title);
    let body = escape_xml(body);
    let visual = format!(
        "<visual><binding template=\"ToastGeneric\"><text hint-wrap=\"true\">{title}</text><text hint-wrap=\"true\">{body}</text></binding></visual>"
    );
    if audio_enabled {
        // An alarm toast needs a button to stay on screen. When there is a
        // thread to open, that button is 查看; clicking it also stops the
        // alarm. Without one, only the system dismiss button makes sense.
        let audio = "<audio src=\"ms-winsoundevent:Notification.Looping.Alarm\" loop=\"true\"/>";
        return match action {
            Some(action) => format!(
                "<toast scenario=\"alarm\" duration=\"long\">{visual}<actions><action content=\"{label}\" arguments=\"open\" activationType=\"foreground\"/></actions>{audio}</toast>",
                label = escape_xml(&action.label),
            ),
            None => format!(
                "<toast scenario=\"alarm\" duration=\"long\">{visual}{audio}<commands scenario=\"alarm\"><command id=\"dismiss\"/></commands></toast>"
            ),
        };
    }

    format!("<toast>{visual}<audio silent=\"true\"/></toast>")
}

#[cfg(target_os = "windows")]
fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(all(test, target_os = "windows"))]
mod windows_tests {
    use super::*;

    /// Parses the XML the same way `show_windows_toast` does, without showing
    /// anything on screen.
    fn assert_windows_accepts(xml: &str) {
        use windows::{
            Data::Xml::Dom::XmlDocument, UI::Notifications::ToastNotification, core::HSTRING,
        };

        let document = XmlDocument::new().unwrap();
        document.LoadXml(&HSTRING::from(xml)).unwrap();
        ToastNotification::CreateToastNotification(&document).unwrap();
    }

    #[test]
    fn enabled_audio_uses_a_persistent_looping_alarm_toast() {
        let action = NotificationAction::open_thread("thread-1", "codex");
        let xml = windows_toast_xml("完成", "任务已完成", Some(&action), true);
        assert!(xml.contains("scenario=\"alarm\""));
        assert!(xml.contains("duration=\"long\""));
        assert!(xml.contains("Notification.Looping.Alarm"));
        assert!(xml.contains("loop=\"true\""));
        assert!(!xml.contains("appLogoOverride"));
        assert_windows_accepts(&xml);
    }

    #[test]
    fn alarm_with_a_thread_offers_view_instead_of_dismiss() {
        let action = NotificationAction::open_thread("thread-1", "codex");
        let xml = windows_toast_xml("完成", "任务已完成", Some(&action), true);
        assert!(xml.contains(
            "<actions><action content=\"查看\" arguments=\"open\" activationType=\"foreground\"/></actions>"
        ));
        assert!(!xml.contains("<command id=\"dismiss\"/>"));
        assert!(!xml.contains("<commands"));
    }

    #[test]
    fn alarm_without_a_thread_keeps_the_system_dismiss_button() {
        let xml = windows_toast_xml("完成", "任务已完成", None, true);
        assert!(xml.contains("<command id=\"dismiss\"/>"));
        assert!(!xml.contains("<actions>"));
        assert_windows_accepts(&xml);
    }

    #[test]
    fn toast_text_is_escaped() {
        let xml = windows_toast_xml("a < b & \"c\"", "<script>", None, false);
        assert!(xml.contains("a &lt; b &amp; &quot;c&quot;"));
        assert!(xml.contains("&lt;script&gt;"));
        assert_windows_accepts(&xml);
    }

    #[test]
    fn disabled_audio_keeps_the_regular_silent_notification_action() {
        let action = NotificationAction::open_thread("thread-1", "codex");
        let xml = windows_toast_xml("完成", "任务已完成", Some(&action), false);
        assert!(!xml.contains("scenario=\"alarm\""));
        assert!(xml.contains("audio silent=\"true\""));
        assert!(!xml.contains("<actions>"));
        assert!(!xml.contains("appLogoOverride"));
        assert_windows_accepts(&xml);
    }

    #[test]
    #[ignore = "shows a native looping Windows alarm toast for manual verification"]
    fn preview_windows_alarm_toast() {
        let action = NotificationAction::open_thread("preview-thread", "codex");
        notify_success("任务完成", "已完成", Some(&action), true).unwrap();
    }

    #[test]
    #[ignore = "shows a native silent Windows toast for manual header-icon verification"]
    fn preview_windows_toast_header_icon() {
        notify_success(
            "整理项目文件并发布最新版本",
            "已完成界面调整、测试与多平台发布配置。",
            None,
            false,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

#[cfg(target_os = "macos")]
fn platform_notify(
    title: &str,
    body: &str,
    action: Option<&NotificationAction>,
    audio_enabled: bool,
) -> Result<(), NotificationError> {
    use mac_notification_sys::{MainButton, Notification, NotificationResponse};

    let Some(action) = action.cloned() else {
        let mut options = Notification::new();
        if audio_enabled {
            options.default_sound();
        }
        return mac_notification_sys::send_notification(title, None, body, Some(&options))
            .map(|_| ())
            .map_err(|error| NotificationError::Native(error.to_string()));
    };
    let title = title.to_owned();
    let body = body.to_owned();
    std::thread::Builder::new()
        .name("longwatch-macos-notification".into())
        .spawn(move || {
            let mut options = Notification::new();
            options.main_button(MainButton::SingleAction(&action.label));
            if audio_enabled {
                options.default_sound();
            }
            if matches!(
                mac_notification_sys::send_notification(&title, None, &body, Some(&options)),
                Ok(NotificationResponse::ActionButton(_) | NotificationResponse::Click)
            ) {
                let _ = action.open();
            }
        })
        .map(|_| ())
        .map_err(|error| NotificationError::Native(error.to_string()))
}

#[cfg(target_os = "linux")]
fn platform_notify(
    title: &str,
    body: &str,
    action: Option<&NotificationAction>,
    audio_enabled: bool,
) -> Result<(), NotificationError> {
    let mut notification = notify_rust::Notification::new();
    notification
        .summary(title)
        .body(body)
        .appname("Longwatch for Codex");
    if audio_enabled {
        notification.sound_name("message-new-instant");
    }
    if let Some(action) = action {
        notification.action("open", &action.label);
    }
    let handle = notification
        .show()
        .map_err(|error| NotificationError::Native(error.to_string()))?;
    if let Some(action) = action.cloned() {
        std::thread::Builder::new()
            .name("longwatch-linux-notification".into())
            .spawn(move || {
                handle.wait_for_action(|selected| {
                    if selected == "open" || selected == "default" {
                        let _ = action.open();
                    }
                });
            })
            .map_err(|error| NotificationError::Native(error.to_string()))?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn platform_notify(
    _title: &str,
    _body: &str,
    _action: Option<&NotificationAction>,
    _audio_enabled: bool,
) -> Result<(), NotificationError> {
    Err(NotificationError::Native(
        "notifications are unsupported on this platform".into(),
    ))
}
