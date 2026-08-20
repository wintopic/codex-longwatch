#![allow(unsafe_code)]

/// Return a compact operating-system description suitable for diagnostics.
#[must_use]
pub fn system_summary() -> String {
    platform_system_summary()
}

#[cfg(target_os = "windows")]
fn platform_system_summary() -> String {
    let product = read_windows_version_value(windows::core::w!("ProductName"))
        .unwrap_or_else(|| "Windows".into());
    let display = read_windows_version_value(windows::core::w!("DisplayVersion"))
        .or_else(|| read_windows_version_value(windows::core::w!("ReleaseId")));
    let build = read_windows_version_value(windows::core::w!("CurrentBuildNumber"));
    let build_revision = read_windows_version_value(windows::core::w!("UBR"));

    let mut summary = product;
    if let Some(display) = display {
        summary.push(' ');
        summary.push_str(&display);
    }
    if let Some(build) = build {
        summary.push_str(" · build ");
        summary.push_str(&build);
        if let Some(revision) = build_revision {
            summary.push('.');
            summary.push_str(&revision);
        }
    }
    format!("{summary} ({})", std::env::consts::ARCH)
}

#[cfg(target_os = "windows")]
fn read_windows_version_value(name: windows::core::PCWSTR) -> Option<String> {
    use windows::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
    };

    let subkey = windows::core::w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let mut byte_count = 0_u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            name,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&raw mut byte_count),
        )
    };
    if status != ERROR_SUCCESS || byte_count < 2 {
        return None;
    }
    let mut words = vec![0_u16; (byte_count as usize).div_ceil(size_of::<u16>())];
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            name,
            RRF_RT_REG_SZ,
            None,
            Some(words.as_mut_ptr().cast()),
            Some(&raw mut byte_count),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    words.truncate(byte_count as usize / size_of::<u16>());
    Some(
        String::from_utf16_lossy(&words)
            .trim_end_matches('\0')
            .to_owned(),
    )
}

#[cfg(target_os = "linux")]
fn platform_system_summary() -> String {
    let pretty = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|contents| {
            contents.lines().find_map(|line| {
                line.strip_prefix("PRETTY_NAME=")
                    .map(|value| value.trim_matches('"').to_owned())
            })
        })
        .unwrap_or_else(|| "Linux".into());
    format!("{pretty} ({})", std::env::consts::ARCH)
}

#[cfg(target_os = "macos")]
fn platform_system_summary() -> String {
    format!("macOS ({})", std::env::consts::ARCH)
}
