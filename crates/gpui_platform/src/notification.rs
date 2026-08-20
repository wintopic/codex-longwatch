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
    notifier
        .Show(&toast)
        .map_err(|error| NotificationError::Native(error.to_string()))?;
    retain_windows_toast(toast);
    Ok(())
}

#[cfg(target_os = "windows")]
fn retain_windows_toast(toast: windows::UI::Notifications::ToastNotification) {
    static ACTIVE_TOAST: OnceLock<Mutex<Option<windows::UI::Notifications::ToastNotification>>> =
        OnceLock::new();
    if let Ok(mut active) = ACTIVE_TOAST.get_or_init(|| Mutex::new(None)).lock() {
        *active = Some(toast);
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
    if audio_enabled {
        let controls = action.map_or_else(
            || {
                "<commands scenario=\"alarm\"><command id=\"dismiss\"/></commands>".to_owned()
            },
            |action| {
                format!(
                    "<actions><action content=\"{}\" arguments=\"open-result\" activationType=\"foreground\"/></actions>",
                    escape_xml(&action.label)
                )
            },
        );
        return format!(
            "<toast scenario=\"alarm\" duration=\"long\"><visual><binding template=\"ToastGeneric\"><text>{title}</text><text>{body}</text></binding></visual><audio src=\"ms-winsoundevent:Notification.Looping.Alarm\" loop=\"true\"/>{controls}</toast>"
        );
    }

    let actions = action.map_or_else(String::new, |action| {
        format!(
            "<actions><action content=\"{}\" arguments=\"open-result\" activationType=\"foreground\"/></actions>",
            escape_xml(&action.label),
        )
    });
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{title}</text><text>{body}</text></binding></visual><audio silent=\"true\"/>{actions}</toast>"
    )
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

    #[test]
    fn enabled_audio_uses_a_persistent_looping_alarm_toast() {
        let action = NotificationAction::open_thread("thread-1", "codex");
        let xml = windows_toast_xml("完成", "任务已完成", Some(&action), true);
        assert!(xml.contains("scenario=\"alarm\""));
        assert!(xml.contains("duration=\"long\""));
        assert!(xml.contains("Notification.Looping.Alarm"));
        assert!(xml.contains("loop=\"true\""));
        assert!(xml.contains("content=\"查看\""));
        assert!(xml.contains("activationType=\"foreground\""));
        assert!(xml.contains("arguments=\"open-result\""));
    }

    #[test]
    fn alarm_without_a_thread_keeps_the_system_dismiss_button() {
        let xml = windows_toast_xml("完成", "任务已完成", None, true);
        assert!(xml.contains("<command id=\"dismiss\"/>"));
    }

    #[test]
    fn disabled_audio_keeps_the_regular_silent_notification_action() {
        let action = NotificationAction::open_thread("thread-1", "codex");
        let xml = windows_toast_xml("完成", "任务已完成", Some(&action), false);
        assert!(!xml.contains("scenario=\"alarm\""));
        assert!(xml.contains("audio silent=\"true\""));
        assert!(xml.contains("activationType=\"foreground\""));
        assert!(xml.contains("arguments=\"open-result\""));
    }

    #[test]
    #[ignore = "shows a native looping Windows alarm toast for manual verification"]
    fn preview_windows_alarm_toast() {
        notify_success("任务完成", "已完成", None, true).unwrap();
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
                        action.open();
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
