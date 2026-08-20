#![allow(unsafe_code)]

use std::path::Path;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OpenDirectoryError {
    #[error("failed to open directory: {0}")]
    Launch(String),
}

/// Open a directory in the platform file manager.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or the platform shell
/// refuses to open it.
pub fn open_directory(path: &Path) -> Result<(), OpenDirectoryError> {
    std::fs::create_dir_all(path).map_err(|error| OpenDirectoryError::Launch(error.to_string()))?;
    platform_open_directory(path)
}

#[cfg(target_os = "windows")]
fn platform_open_directory(path: &Path) -> Result<(), OpenDirectoryError> {
    use windows::{
        Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        core::{HSTRING, PCWSTR, w},
    };

    let path = HSTRING::from(path.to_string_lossy().as_ref());
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &path,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(OpenDirectoryError::Launch(format!(
            "ShellExecuteW returned {code}"
        )))
    }
}

#[cfg(target_os = "macos")]
fn platform_open_directory(path: &Path) -> Result<(), OpenDirectoryError> {
    std::process::Command::new("open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| OpenDirectoryError::Launch(error.to_string()))
}

#[cfg(target_os = "linux")]
fn platform_open_directory(path: &Path) -> Result<(), OpenDirectoryError> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| OpenDirectoryError::Launch(error.to_string()))
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn platform_open_directory(_path: &Path) -> Result<(), OpenDirectoryError> {
    Err(OpenDirectoryError::Launch(
        "opening directories is unsupported on this platform".into(),
    ))
}
