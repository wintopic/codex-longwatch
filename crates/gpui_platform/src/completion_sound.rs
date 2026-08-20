//! Audible completion cue that does not depend on toast notification audio.

use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{OnceLock, mpsc},
    thread,
    time::Duration,
};

use windows::{
    Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_FILENAME, SND_NODEFAULT, SND_SYNC},
    core::{PCWSTR, w},
};

static SOUND_SENDER: OnceLock<mpsc::Sender<()>> = OnceLock::new();

/// Queues a noticeable Clock alarm fallback on a dedicated worker.
pub(crate) fn play_completion_sound() {
    let sender = SOUND_SENDER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        let _ = thread::Builder::new()
            .name("longwatch-completion-sound".into())
            .spawn(move || {
                while receiver.recv().is_ok() {
                    play_completion_sequence();
                }
            });
        sender
    });
    let _ = sender.send(());
}

fn play_completion_sequence() {
    let media_directory = windows_media_directory();
    let alarm = media_directory
        .as_ref()
        .map(|directory| directory.join("Alarm01.wav"));
    if alarm.as_deref().is_some_and(play_wave_file) {
        return;
    }

    // Stripped-down Windows images may omit the Clock alarm assets. Keep a
    // two-part native notification sequence as a reliable final fallback.
    let first = media_directory
        .as_ref()
        .map(|directory| directory.join("Windows Notify Calendar.wav"));
    let second = media_directory
        .as_ref()
        .map(|directory| directory.join("Windows Notify System Generic.wav"));

    if !first.as_deref().is_some_and(play_wave_file) {
        play_system_notification();
    }
    thread::sleep(Duration::from_millis(250));
    if !second.as_deref().is_some_and(play_wave_file) {
        play_system_notification();
    }
}

fn windows_media_directory() -> Option<PathBuf> {
    std::env::var_os("WINDIR")
        .or_else(|| std::env::var_os("SystemRoot"))
        .map(PathBuf::from)
        .map(|directory| directory.join("Media"))
}

#[allow(unsafe_code)]
fn play_wave_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let wide = wide_null(path.as_os_str());
    // SAFETY: the path buffer is NUL-terminated and remains alive for the
    // entire synchronous playback call.
    unsafe {
        PlaySoundW(
            PCWSTR(wide.as_ptr()),
            None,
            SND_FILENAME | SND_NODEFAULT | SND_SYNC,
        )
        .as_bool()
    }
}

#[allow(unsafe_code)]
fn play_system_notification() {
    // SAFETY: the alias is a static, NUL-terminated UTF-16 string and the
    // synchronous call does not retain it.
    let _ = unsafe { PlaySoundW(w!("SystemNotification"), None, SND_ALIAS | SND_SYNC) };
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_paths_are_nul_terminated() {
        let encoded = wide_null(OsStr::new("completion.wav"));
        assert_eq!(encoded.last(), Some(&0));
        assert_eq!(encoded.iter().filter(|&&unit| unit == 0).count(), 1);
    }
}
