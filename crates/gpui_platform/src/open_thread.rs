#![allow(unsafe_code)]

#[cfg(not(target_os = "windows"))]
use std::process::Command;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OpenThreadError {
    #[error("failed to launch thread target: {0}")]
    Launch(String),
}

/// Open or resume a persisted Codex thread using the platform convention.
///
/// # Errors
///
/// Returns an error when the deep-link or Codex CLI process cannot be launched.
pub fn open_thread(thread_id: &str, codex_path: &str) -> Result<(), OpenThreadError> {
    platform_open_thread(thread_id, codex_path)
}

#[cfg(target_os = "windows")]
fn platform_open_thread(thread_id: &str, codex_path: &str) -> Result<(), OpenThreadError> {
    use windows::{
        Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        core::{HSTRING, PCWSTR, w},
    };

    let target = format!("codex://threads/{thread_id}");
    let target = HSTRING::from(target);
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &target,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let protocol_code = result.0 as isize;
    if protocol_code > 32 {
        return Ok(());
    }

    let executable_path =
        which::which(codex_path).unwrap_or_else(|_| std::path::PathBuf::from(codex_path));
    let executable = HSTRING::from(executable_path.to_string_lossy().as_ref());
    let parameters = HSTRING::from(format!("resume {thread_id}"));
    let fallback = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &executable,
            &parameters,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let fallback_code = fallback.0 as isize;
    if fallback_code > 32 {
        Ok(())
    } else {
        Err(OpenThreadError::Launch(format!(
            "codex:// returned {protocol_code}; Codex CLI fallback returned {fallback_code}"
        )))
    }
}

#[cfg(target_os = "macos")]
fn platform_open_thread(thread_id: &str, _codex_path: &str) -> Result<(), OpenThreadError> {
    let target = format!("codex://threads/{thread_id}");

    let status = Command::new("open").arg(&target).status();

    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(OpenThreadError::Launch(format!(
            "launcher exited with {status}"
        ))),
        Err(error) => Err(OpenThreadError::Launch(error.to_string())),
    }
}

#[cfg(target_os = "linux")]
fn platform_open_thread(thread_id: &str, codex_path: &str) -> Result<(), OpenThreadError> {
    let terminal_commands: &[(&str, &[&str])] = &[
        ("x-terminal-emulator", &["-e"]),
        ("gnome-terminal", &["--"]),
        ("konsole", &["-e"]),
        ("kitty", &[]),
        ("alacritty", &["-e"]),
    ];
    for (terminal, prefix) in terminal_commands {
        if Command::new(terminal)
            .args(*prefix)
            .arg(codex_path)
            .args(["resume", thread_id])
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
    }

    let command = format!(
        "{} resume {}",
        quote_for_display(codex_path),
        quote_for_display(thread_id)
    );
    let mut clipboard = arboard::Clipboard::new().map_err(|error| {
        OpenThreadError::Launch(format!(
            "no terminal launcher was available and the resume command could not be copied: {error}"
        ))
    })?;
    clipboard.set_text(command).map_err(|error| {
        OpenThreadError::Launch(format!(
            "no terminal launcher was available and the resume command could not be copied: {error}"
        ))
    })
}

#[cfg(target_os = "linux")]
fn quote_for_display(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-._/".contains(character))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn platform_open_thread(_thread_id: &str, _codex_path: &str) -> Result<(), OpenThreadError> {
    Err(OpenThreadError::Launch(
        "thread opening is unsupported on this platform".into(),
    ))
}
