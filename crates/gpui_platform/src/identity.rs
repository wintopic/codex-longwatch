#![allow(unsafe_code)]

use thiserror::Error;

/// Neutral, product-owned Windows identity used for notifications and taskbar grouping.
///
/// Keep this independent from a personal GitHub handle so Windows never exposes
/// a maintainer name when it falls back to displaying the raw application ID.
#[cfg(target_os = "windows")]
pub(crate) const APP_USER_MODEL_ID: &str = "Longwatch";

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
            System::Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE,
                RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
            },
            UI::Shell::SetCurrentProcessExplicitAppUserModelID,
        },
        core::{HSTRING, PCWSTR, w},
    };

    // Remove the pre-release identity so Windows does not keep surfacing the
    // former personal prefix after an upgrade.
    let old_status = unsafe {
        RegDeleteTreeW(
            HKEY_CURRENT_USER,
            w!("Software\\Classes\\AppUserModelId\\wintopic.Longwatch"),
        )
    };
    if old_status != ERROR_SUCCESS && old_status != ERROR_FILE_NOT_FOUND {
        return Err(IdentityError::Native(format!(
            "RegDeleteTreeW returned {}",
            old_status.0
        )));
    }

    unsafe {
        SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(APP_USER_MODEL_ID))
            .map_err(|error| IdentityError::Native(error.to_string()))?;
    }

    let mut key = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Classes\\AppUserModelId\\Longwatch"),
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
        let executable =
            std::env::current_exe().map_err(|error| IdentityError::Native(error.to_string()))?;
        set_registry_string(key, w!("IconUri"), executable.as_os_str())?;
        let visible = 1_u32.to_ne_bytes();
        let status =
            unsafe { RegSetValueExW(key, w!("ShowInSettings"), None, REG_DWORD, Some(&visible)) };
        if status != ERROR_SUCCESS {
            return Err(IdentityError::Native(format!(
                "RegSetValueExW(ShowInSettings) returned {}",
                status.0
            )));
        }
        Ok(())
    })();

    unsafe {
        let _ = RegCloseKey(key);
    }
    result
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
