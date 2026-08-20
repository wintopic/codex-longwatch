#![allow(unsafe_code)]

use thiserror::Error;

/// Neutral, product-owned Windows identity used for notifications and taskbar grouping.
///
/// Keep this independent from a personal GitHub handle so Windows never exposes
/// a maintainer name when it falls back to displaying the raw application ID.
#[cfg(target_os = "windows")]
pub(crate) const APP_USER_MODEL_ID: &str = "Longwatch.Codex";

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("failed to register the Longwatch application identity: {0}")]
    Native(String),
}

/// Register the process identity used by Windows notifications and taskbar grouping.
///
/// # Errors
///
/// Returns an error when Windows rejects either the process AUMID or its per-user
/// registration. Other platforms do not require explicit registration.
pub fn initialize_app_identity() -> Result<(), IdentityError> {
    platform_initialize_app_identity()
}

#[cfg(target_os = "windows")]
fn platform_initialize_app_identity() -> Result<(), IdentityError> {
    use windows::{
        Win32::{
            Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
            System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW},
            UI::Shell::SetCurrentProcessExplicitAppUserModelID,
        },
        core::{HSTRING, w},
    };

    // Remove identities used by pre-release builds. Besides keeping the
    // product independent from a maintainer handle, moving away from the
    // original generic AUMID invalidates Windows' cached blank header icon.
    for legacy_identity in [
        w!("Software\\Classes\\AppUserModelId\\wintopic.Longwatch"),
        w!("Software\\Classes\\AppUserModelId\\Longwatch"),
    ] {
        let old_status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, legacy_identity) };
        if old_status != ERROR_SUCCESS && old_status != ERROR_FILE_NOT_FOUND {
            return Err(IdentityError::Native(format!(
                "RegDeleteTreeW returned {}",
                old_status.0
            )));
        }
    }

    unsafe {
        SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_USER_MODEL_ID))
            .map_err(|error| IdentityError::Native(error.to_string()))?;
    }

    register_notification_identity().map(|_| ())
}

#[cfg(target_os = "windows")]
fn register_notification_identity() -> Result<std::path::PathBuf, IdentityError> {
    use windows::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE,
            RegCloseKey, RegCreateKeyExW, RegSetValueExW,
        },
    };
    use windows::core::{PCWSTR, w};

    let mut key = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Classes\\AppUserModelId\\Longwatch.Codex"),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &raw mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(IdentityError::Native(format!(
            "RegCreateKeyExW returned {}",
            status.0
        )));
    }

    let result = (|| {
        set_registry_string(key, w!("DisplayName"), std::ffi::OsStr::new("Longwatch"))?;
        let notification_icon = install_notification_icon()?;
        set_registry_string(key, w!("IconUri"), notification_icon.as_os_str())?;
        // Windows' unpackaged-app notification identity expects this value
        // alongside IconUri. A transparent background lets the theme-specific
        // black/white mark render without a colored tile behind it.
        set_registry_string(key, w!("IconBackgroundColor"), std::ffi::OsStr::new("0"))?;
        let visible = 1_u32.to_ne_bytes();
        let status =
            unsafe { RegSetValueExW(key, w!("ShowInSettings"), None, REG_DWORD, Some(&visible)) };
        if status != ERROR_SUCCESS {
            return Err(IdentityError::Native(format!(
                "RegSetValueExW(ShowInSettings) returned {}",
                status.0
            )));
        }
        Ok(notification_icon)
    })();

    unsafe {
        let _ = RegCloseKey(key);
    }
    if result.is_ok() {
        // The notification host caches unpackaged-app identity metadata. Ask
        // the shell to invalidate that cache after changing IconUri so an
        // upgraded build does not keep showing the old/blank mark.
        use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
        unsafe {
            SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
        }
    }
    result
}

#[cfg(target_os = "windows")]
fn install_notification_icon() -> Result<std::path::PathBuf, IdentityError> {
    use windows::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    };
    use windows::core::w;

    let read_theme_value = |name| {
        let mut use_light_theme = 1_u32;
        let mut data_size = size_of::<u32>() as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
                name,
                RRF_RT_REG_DWORD,
                None,
                Some((&raw mut use_light_theme).cast()),
                Some(&raw mut data_size),
            )
        };
        (status == ERROR_SUCCESS).then_some(use_light_theme)
    };
    // Toasts are drawn by the Windows shell, so SystemUsesLightTheme is the
    // relevant preference. Fall back to the app preference on older systems.
    let dark = read_theme_value(w!("SystemUsesLightTheme"))
        .or_else(|| read_theme_value(w!("AppsUseLightTheme")))
        .is_some_and(|use_light_theme| use_light_theme == 0);
    let file_name = if dark {
        "toast-dark.png"
    } else {
        "toast-light.png"
    };
    let bytes: &[u8] = if dark {
        include_bytes!("../../../packaging/windows/toast-dark.png")
    } else {
        include_bytes!("../../../packaging/windows/toast-light.png")
    };
    let directory = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Longwatch")
        .join("assets");
    std::fs::create_dir_all(&directory).map_err(|error| {
        IdentityError::Native(format!(
            "failed to create notification icon directory: {error}"
        ))
    })?;
    let destination = directory.join(file_name);
    if std::fs::read(&destination).ok().as_deref() != Some(bytes) {
        std::fs::write(&destination, bytes).map_err(|error| {
            IdentityError::Native(format!("failed to write notification icon: {error}"))
        })?;
    }
    Ok(destination)
}

#[cfg(target_os = "windows")]
pub(crate) fn notification_icon_path() -> Option<std::path::PathBuf> {
    // Refresh the registry immediately before every toast. Besides repairing
    // upgrades from older builds, this switches the header icon when Windows'
    // app theme changes while Longwatch remains running.
    register_notification_identity().ok()
}

#[cfg(target_os = "windows")]
fn set_registry_string(
    key: windows::Win32::System::Registry::HKEY,
    name: windows::core::PCWSTR,
    value: &std::ffi::OsStr,
) -> Result<(), IdentityError> {
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{REG_SZ, RegSetValueExW},
    };

    let wide = value.encode_wide().chain(Some(0)).collect::<Vec<_>>();
    let bytes = unsafe {
        std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * size_of::<u16>())
    };
    let status = unsafe { RegSetValueExW(key, name, None, REG_SZ, Some(bytes)) };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(IdentityError::Native(format!(
            "RegSetValueExW returned {}",
            status.0
        )))
    }
}

#[cfg(not(target_os = "windows"))]
#[allow(clippy::unnecessary_wraps)]
fn platform_initialize_app_identity() -> Result<(), IdentityError> {
    // Keep the public API identical across platforms even though only Windows
    // needs an explicit process identity registration step.
    Ok(())
}
