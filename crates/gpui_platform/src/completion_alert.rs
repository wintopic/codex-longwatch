//! Strong, non-activating full-desktop overlays for success and retry errors.

use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[derive(Clone, Copy)]
enum OverlayKind {
    Success,
    RetryError,
}

static SUCCESS_OVERLAY_SENDER: OnceLock<mpsc::Sender<OverlayKind>> = OnceLock::new();
static RETRY_OVERLAY_SENDER: OnceLock<mpsc::Sender<OverlayKind>> = OnceLock::new();
static SUCCESS_OVERLAY_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Covers the full virtual desktop with a click-through success pulse until acknowledged.
pub fn show_completion_overlay() {
    if !SUCCESS_OVERLAY_ACTIVE.swap(true, Ordering::SeqCst) {
        enqueue_overlay(OverlayKind::Success);
    }
}

/// Stops the persistent success overlay after the completion toast is acknowledged.
pub fn stop_completion_overlay() {
    SUCCESS_OVERLAY_ACTIVE.store(false, Ordering::SeqCst);
}

/// Shows one strong red pulse sequence for each retry-producing error.
///
/// Events are queued instead of coalesced so rapid Codex reconnect attempts
/// still produce one visible full-screen warning per failed attempt.
pub fn show_retry_error_overlay() {
    enqueue_overlay(OverlayKind::RetryError);
}

fn enqueue_overlay(kind: OverlayKind) {
    let (slot, thread_name) = match kind {
        OverlayKind::Success => (&SUCCESS_OVERLAY_SENDER, "longwatch-success-overlay"),
        OverlayKind::RetryError => (&RETRY_OVERLAY_SENDER, "longwatch-retry-overlay"),
    };
    let sender = slot.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<OverlayKind>();
        let _ = std::thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                while let Ok(kind) = receiver.recv() {
                    platform_show_overlay(kind);
                }
            });
        sender
    });
    let _ = sender.send(kind);
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn platform_show_overlay(kind: OverlayKind) {
    use std::{
        sync::OnceLock,
        thread,
        time::{Duration, Instant},
    };

    use windows::{
        Win32::{
            Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
            Graphics::Gdi::{CreateSolidBrush, UpdateWindow},
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{
                CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
                DispatchMessageW, GetSystemMetrics, HWND_TOPMOST, LWA_ALPHA, MSG, PM_REMOVE,
                PeekMessageW, RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
                SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOWNA, SWP_NOACTIVATE, SWP_SHOWWINDOW,
                SetLayeredWindowAttributes, SetWindowPos, ShowWindow, TranslateMessage,
                WM_DISPLAYCHANGE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
                WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
            },
        },
        core::w,
    };

    static SUCCESS_CLASS_READY: OnceLock<bool> = OnceLock::new();
    static ERROR_CLASS_READY: OnceLock<bool> = OnceLock::new();

    unsafe fn virtual_desktop_bounds() -> Option<(i32, i32, i32, i32)> {
        // SAFETY: system metrics are process-independent scalar values.
        let (x, y, width, height) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        (width > 0 && height > 0).then_some((x, y, width, height))
    }

    unsafe fn fit_virtual_desktop(window: HWND) {
        let Some((x, y, width, height)) = (unsafe { virtual_desktop_bounds() }) else {
            return;
        };
        // SAFETY: `window` is the live overlay HWND owned by this thread.
        let _ = unsafe {
            SetWindowPos(
                window,
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        };
    }

    unsafe extern "system" fn overlay_window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_DISPLAYCHANGE {
            // SAFETY: the callback receives the live HWND being resized.
            unsafe { fit_virtual_desktop(window) };
        }
        // SAFETY: this window has no custom messages or owned resources, so the
        // default procedure is the correct handler for every message.
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    let Ok(module) = (unsafe { GetModuleHandleW(None) }) else {
        return;
    };
    let instance = HINSTANCE(module.0);
    let (class_ready, class_name, title) = match kind {
        OverlayKind::Success => {
            let ready = *SUCCESS_CLASS_READY.get_or_init(|| {
                // COLORREF uses 0x00BBGGRR; this is product emerald #10B981.
                let brush = unsafe { CreateSolidBrush(COLORREF(0x0081_B910)) };
                let class = WNDCLASSW {
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(overlay_window_proc),
                    hInstance: instance,
                    hbrBackground: brush,
                    lpszClassName: w!("LongwatchCompletionOverlay"),
                    ..Default::default()
                };
                // SAFETY: the class and static UTF-16 name remain valid for
                // the process lifetime. The class owns its brush.
                unsafe { RegisterClassW(&raw const class) != 0 }
            });
            (
                ready,
                w!("LongwatchCompletionOverlay"),
                w!("Longwatch 任务完成"),
            )
        }
        OverlayKind::RetryError => {
            let ready = *ERROR_CLASS_READY.get_or_init(|| {
                // Strong retry red #E53935 in COLORREF's 0x00BBGGRR format.
                let brush = unsafe { CreateSolidBrush(COLORREF(0x0035_39E5)) };
                let class = WNDCLASSW {
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(overlay_window_proc),
                    hInstance: instance,
                    hbrBackground: brush,
                    lpszClassName: w!("LongwatchRetryErrorOverlay"),
                    ..Default::default()
                };
                // SAFETY: the class and static UTF-16 name remain valid for
                // the process lifetime. The class owns its brush.
                unsafe { RegisterClassW(&raw const class) != 0 }
            });
            (
                ready,
                w!("LongwatchRetryErrorOverlay"),
                w!("Longwatch 正在重试"),
            )
        }
    };
    if !class_ready {
        return;
    }

    let Some((x, y, width, height)) = (unsafe { virtual_desktop_bounds() }) else {
        return;
    };

    // SAFETY: the registered class is valid, the window has no owner or menu,
    // and its lifetime is confined to this worker thread.
    let Ok(window) = (unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name,
            title,
            WS_POPUP,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(instance),
            None,
        )
    }) else {
        return;
    };

    // SAFETY: `window` is a live layered popup created above. It is shown
    // without activation, remains click-through, and is destroyed below.
    unsafe {
        let _ = SetLayeredWindowAttributes(window, COLORREF(0), 0, LWA_ALPHA);
        let _ = SetWindowPos(
            window,
            Some(HWND_TOPMOST),
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = ShowWindow(window, SW_SHOWNA);
        let _ = UpdateWindow(window);
    }

    const SUCCESS_ALPHAS: [u8; 12] = [24, 55, 95, 140, 180, 215, 185, 145, 100, 60, 28, 0];
    const ERROR_ALPHAS: [u8; 8] = [35, 85, 150, 225, 175, 105, 45, 0];

    fn pump_messages() {
        let mut message = MSG::default();
        // SAFETY: this drains messages for the current overlay worker thread;
        // MSG remains valid for each dispatch call.
        unsafe {
            while PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
    }

    fn wait_with_messages(duration: Duration) {
        let deadline = Instant::now() + duration;
        loop {
            pump_messages();
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(remaining.min(Duration::from_millis(16)));
        }
    }

    match kind {
        OverlayKind::Success => {
            while SUCCESS_OVERLAY_ACTIVE.load(Ordering::SeqCst) {
                for &alpha in &SUCCESS_ALPHAS {
                    if !SUCCESS_OVERLAY_ACTIVE.load(Ordering::SeqCst) {
                        break;
                    }
                    // SAFETY: the window remains alive throughout the pulse loop.
                    let _ = unsafe {
                        SetLayeredWindowAttributes(window, COLORREF(0), alpha, LWA_ALPHA)
                    };
                    wait_with_messages(Duration::from_millis(115));
                }
                if SUCCESS_OVERLAY_ACTIVE.load(Ordering::SeqCst) {
                    wait_with_messages(Duration::from_millis(140));
                }
            }
        }
        OverlayKind::RetryError => {
            for pulse in 0..2 {
                for &alpha in &ERROR_ALPHAS {
                    // SAFETY: the window remains alive throughout the pulse loop.
                    let _ = unsafe {
                        SetLayeredWindowAttributes(window, COLORREF(0), alpha, LWA_ALPHA)
                    };
                    wait_with_messages(Duration::from_millis(75));
                }
                if pulse == 0 {
                    wait_with_messages(Duration::from_millis(90));
                }
            }
        }
    }

    // SAFETY: the window was created on this thread and is destroyed once.
    let _ = unsafe { DestroyWindow(window) };
}

#[cfg(not(target_os = "windows"))]
fn platform_show_overlay(_: OverlayKind) {}
