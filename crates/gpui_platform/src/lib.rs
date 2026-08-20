#![allow(
    clippy::default_trait_access,
    clippy::missing_errors_doc,
    clippy::redundant_closure_for_method_calls,
    clippy::cast_possible_truncation,
    clippy::map_unwrap_or,
    clippy::items_after_statements
)]

//! Small platform boundary used by the GPUI application.
//!
//! GPUI 0.2.2 contains its windowing backends in the `gpui` crate.  This local
//! facade intentionally owns the non-window services that vary by desktop OS:
//! native notifications, thread opening, and explicitly opted-in GUI input.

mod completion_alert;
#[cfg(target_os = "windows")]
mod completion_sound;
mod error_dialog;
mod identity;
#[cfg(target_os = "macos")]
mod macos_icon;
mod notification;
mod open_directory;
mod open_thread;
#[cfg(target_os = "windows")]
mod process_job;
mod system_info;
#[cfg(target_os = "windows")]
mod tray;

#[cfg(feature = "gui-fallback")]
mod automation;

pub use completion_alert::{
    show_completion_overlay, show_retry_error_overlay, stop_completion_overlay,
};
pub use error_dialog::{confirm_exit_while_running, show_error_dialog};
pub use identity::{IdentityError, initialize_app_identity};
#[cfg(target_os = "macos")]
pub use macos_icon::sync_macos_app_icon;
pub use notification::{NotificationAction, NotificationError, notify_success};
pub use open_directory::{OpenDirectoryError, open_directory};
pub use open_thread::{OpenThreadError, open_thread};
#[cfg(target_os = "windows")]
pub use process_job::{ProcessJob, ProcessJobError};
pub use system_info::system_summary;
#[cfg(target_os = "windows")]
pub use tray::{
    TrayAction, TrayControl, TrayError, cancel_tray_exit, hide_window_to_tray, install_tray,
    request_existing_instance_show, set_tray_state, set_tray_tooltip, show_window_from_tray,
    shutdown_tray, take_tray_action, tray_exit_requested,
};

#[cfg(feature = "gui-fallback")]
pub use automation::{AutomationError, GuiAutomation, SystemGuiAutomation};

/// Returns true when the current Linux session is Wayland, including `XWayland`.
#[must_use]
pub fn is_wayland_session() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var("XDG_SESSION_TYPE")
                .is_ok_and(|session| session.eq_ignore_ascii_case("wayland"))
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}
