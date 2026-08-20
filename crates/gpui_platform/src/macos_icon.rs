use std::sync::atomic::{AtomicU8, Ordering};

use objc2::{AllocAnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage};
use objc2_foundation::NSData;

const LIGHT_ICON: u8 = 1;
const DARK_ICON: u8 = 2;
static ACTIVE_ICON: AtomicU8 = AtomicU8::new(0);

/// Keeps the running app's Dock icon aligned with the current macOS appearance.
///
/// Finder uses the light ICNS declared in `Info.plist`; once the app is running,
/// GPUI's appearance observer calls this function whenever the system theme changes.
pub fn sync_macos_app_icon(dark: bool, light_icns: &[u8], dark_icns: &[u8]) {
    let requested = if dark { DARK_ICON } else { LIGHT_ICON };
    if ACTIVE_ICON.load(Ordering::Acquire) == requested {
        return;
    }

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let bytes = if dark { dark_icns } else { light_icns };
    let data = NSData::with_bytes(bytes);
    let Some(icon) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    let application = NSApplication::sharedApplication(main_thread);

    // SAFETY: AppKit is called on the main thread and `icon` is a valid NSImage.
    unsafe {
        application.setApplicationIconImage(Some(&icon));
    }
    ACTIVE_ICON.store(requested, Ordering::Release);
}
