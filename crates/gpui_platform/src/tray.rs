#![allow(unsafe_code)]

use std::{
    ffi::c_void,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU32, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

use thiserror::Error;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, DispatchMessageW, FindWindowExW, GetCursorPos, GetMessageW,
                HWND_MESSAGE, IDI_APPLICATION, LoadIconW, MF_DISABLED, MF_GRAYED, MF_SEPARATOR,
                MF_STRING, MSG, PostMessageW, PostThreadMessageW, RegisterClassW, SW_HIDE,
                SW_RESTORE, SetForegroundWindow, ShowWindow, TPM_NONOTIFY, TPM_RETURNCMD,
                TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE,
                WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_QUIT,
                WM_RBUTTONUP, WNDCLASSW,
            },
        },
    },
    core::{PCWSTR, w},
};

const TRAY_ICON_ID: u32 = 1;
const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 41;
const TRAY_SHOW_MESSAGE: u32 = WM_APP + 42;
const MENU_STATUS: usize = 1000;
const MENU_SHOW: usize = 1001;
const MENU_CONTROL: usize = 1002;
const MENU_OPEN_RESULT: usize = 1003;
const MENU_EXIT: usize = 1004;
const TRAY_CLASS_NAME: PCWSTR = w!("Longwatch::TrayWindow");

const ACTION_NONE: u8 = 0;
const ACTION_PAUSE: u8 = 1;
const ACTION_RESUME: u8 = 2;
const ACTION_OPEN_RESULT: u8 = 3;

static MAIN_WINDOW: AtomicIsize = AtomicIsize::new(0);
static TRAY_WINDOW: AtomicIsize = AtomicIsize::new(0);
static TRAY_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static ACTION_REQUESTED: AtomicU8 = AtomicU8::new(ACTION_NONE);
static TRAY_THREAD: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();
static TRAY_MENU_STATE: OnceLock<Mutex<TrayMenuState>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TrayControl {
    #[default]
    Disabled,
    Pause,
    Resume,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    Pause,
    Resume,
    OpenResult,
}

#[derive(Clone, Debug)]
struct TrayMenuState {
    status: String,
    control: TrayControl,
    can_open_result: bool,
}

impl Default for TrayMenuState {
    fn default() -> Self {
        Self {
            status: "准备就绪".into(),
            control: TrayControl::Disabled,
            can_open_result: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum TrayError {
    #[error("failed to initialize the Windows tray icon: {0}")]
    Native(String),
}

/// Install the Longwatch tray icon for a GPUI window represented by its HWND.
///
/// # Errors
///
/// Returns an error when the tray thread or notification icon cannot be created.
pub fn install_tray(main_window: isize) -> Result<(), TrayError> {
    MAIN_WINDOW.store(main_window, Ordering::Release);
    if TRAY_THREAD_ID.load(Ordering::Acquire) != 0 {
        return Ok(());
    }

    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("longwatch-tray".into())
        .spawn(move || run_tray(&ready_sender))
        .map_err(|error| TrayError::Native(error.to_string()))?;
    let ready = ready_receiver
        .recv_timeout(Duration::from_secs(5))
        .map_err(|error| TrayError::Native(error.to_string()))?;
    if let Err(error) = ready {
        let _ = thread.join();
        return Err(error);
    }
    *TRAY_THREAD
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|error| TrayError::Native(error.to_string()))? = Some(thread);
    Ok(())
}

/// Hide the main window while leaving the process and queue running.
pub fn hide_window_to_tray(main_window: isize) {
    if main_window != 0 {
        unsafe {
            let _ = ShowWindow(HWND(main_window as *mut c_void), SW_HIDE);
        }
    }
}

/// Restore and focus the main window.
pub fn show_window_from_tray(main_window: isize) {
    restore_main_window(main_window);
}

/// Ask a previously running instance to restore its hidden main window.
#[must_use]
pub fn request_existing_instance_show() -> bool {
    for _ in 0..30 {
        let window =
            unsafe { FindWindowExW(Some(HWND_MESSAGE), None, TRAY_CLASS_NAME, PCWSTR::null()) };
        if let Ok(window) = window
            && unsafe { PostMessageW(Some(window), TRAY_SHOW_MESSAGE, WPARAM(0), LPARAM(0)) }
                .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[must_use]
pub fn tray_exit_requested() -> bool {
    EXIT_REQUESTED.load(Ordering::Acquire)
}

pub fn cancel_tray_exit() {
    EXIT_REQUESTED.store(false, Ordering::Release);
}

/// Update the status and enabled actions shown by the native tray menu.
pub fn set_tray_state(status: &str, control: TrayControl, can_open_result: bool) {
    if let Ok(mut state) = TRAY_MENU_STATE
        .get_or_init(|| Mutex::new(TrayMenuState::default()))
        .lock()
    {
        status.clone_into(&mut state.status);
        state.control = control;
        state.can_open_result = can_open_result;
    }
    set_tray_tooltip(&format!("Longwatch · {status}"));
}

/// Take the next user action requested through the tray menu.
#[must_use]
pub fn take_tray_action() -> Option<TrayAction> {
    match ACTION_REQUESTED.swap(ACTION_NONE, Ordering::AcqRel) {
        ACTION_PAUSE => Some(TrayAction::Pause),
        ACTION_RESUME => Some(TrayAction::Resume),
        ACTION_OPEN_RESULT => Some(TrayAction::OpenResult),
        _ => None,
    }
}

/// Update the tray tooltip with the current queue status.
pub fn set_tray_tooltip(status: &str) {
    let tray_window = TRAY_WINDOW.load(Ordering::Acquire);
    if tray_window == 0 {
        return;
    }
    let mut data = tray_data(HWND(tray_window as *mut c_void), status);
    data.uFlags = NIF_TIP;
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &raw const data);
    }
}

/// Remove the tray icon and stop its native message thread.
pub fn shutdown_tray() {
    let thread_id = TRAY_THREAD_ID.load(Ordering::Acquire);
    if thread_id != 0 {
        unsafe {
            let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
    if let Ok(mut slot) = TRAY_THREAD.get_or_init(|| Mutex::new(None)).lock()
        && let Some(thread) = slot.take()
    {
        let _ = thread.join();
    }
    MAIN_WINDOW.store(0, Ordering::Release);
    EXIT_REQUESTED.store(false, Ordering::Release);
    ACTION_REQUESTED.store(ACTION_NONE, Ordering::Release);
}

fn run_tray(ready: &mpsc::SyncSender<Result<(), TrayError>>) {
    let result = unsafe { create_tray_window() };
    let (window, icon_data) = match result {
        Ok(value) => value,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    TRAY_WINDOW.store(window.0 as isize, Ordering::Release);
    TRAY_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    let _ = ready.send(Ok(()));

    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&raw mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }

    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &raw const icon_data);
        let _ = DestroyWindow(window);
    }
    TRAY_WINDOW.store(0, Ordering::Release);
    TRAY_THREAD_ID.store(0, Ordering::Release);
}

#[allow(clippy::manual_dangling_ptr)]
unsafe fn create_tray_window() -> Result<(HWND, NOTIFYICONDATAW), TrayError> {
    let module =
        unsafe { GetModuleHandleW(None) }.map_err(|error| TrayError::Native(error.to_string()))?;
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(tray_window_procedure),
        hInstance: module.into(),
        lpszClassName: TRAY_CLASS_NAME,
        ..Default::default()
    };
    unsafe {
        let _ = RegisterClassW(&raw const window_class);
    }
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            TRAY_CLASS_NAME,
            w!("Longwatch Tray"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(module.into()),
            None,
        )
    }
    .map_err(|error| TrayError::Native(error.to_string()))?;

    // Win32 encodes integer resource identifiers as low-valued pointers.
    let icon_resource = PCWSTR(1_usize as *const u16);
    let icon = unsafe { LoadIconW(Some(module.into()), icon_resource) }
        .or_else(|_| unsafe { LoadIconW(None, IDI_APPLICATION) })
        .map_err(|error| TrayError::Native(error.to_string()))?;
    let mut data = tray_data(window, "Longwatch · 准备就绪");
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = TRAY_CALLBACK_MESSAGE;
    data.hIcon = icon;
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &raw const data) }.as_bool() {
        unsafe {
            let _ = DestroyWindow(window);
        }
        return Err(TrayError::Native(
            "Shell_NotifyIconW(NIM_ADD) failed".into(),
        ));
    }
    Ok((window, data))
}

fn tray_data(window: HWND, tooltip: &str) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    write_wide_buffer(&mut data.szTip, tooltip);
    data
}

fn write_wide_buffer(buffer: &mut [u16], text: &str) {
    buffer.fill(0);
    let capacity = buffer.len().saturating_sub(1);
    for (slot, value) in buffer.iter_mut().take(capacity).zip(text.encode_utf16()) {
        *slot = value;
    }
}

unsafe extern "system" fn tray_window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == TRAY_CALLBACK_MESSAGE {
        let bytes = lparam.0.to_ne_bytes();
        let notification = u32::from(u16::from_ne_bytes([bytes[0], bytes[1]]));
        match notification {
            WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                restore_main_window(MAIN_WINDOW.load(Ordering::Acquire));
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => unsafe {
                show_tray_menu(window);
            },
            _ => {}
        }
        return LRESULT(0);
    }
    if message == TRAY_SHOW_MESSAGE {
        restore_main_window(MAIN_WINDOW.load(Ordering::Acquire));
        return LRESULT(0);
    }
    if message == WM_DESTROY {
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

unsafe fn show_tray_menu(window: HWND) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    let state = TRAY_MENU_STATE
        .get_or_init(|| Mutex::new(TrayMenuState::default()))
        .lock()
        .map_or_else(|_| TrayMenuState::default(), |state| state.clone());
    let status = wide_text(&format!("状态：{}", state.status));
    let control_label = match state.control {
        TrayControl::Disabled => "暂无可控制任务",
        TrayControl::Pause => "暂停排队",
        TrayControl::Resume => "继续排队",
    };
    let control = wide_text(control_label);
    let disabled = MF_STRING | MF_DISABLED | MF_GRAYED;
    unsafe {
        let _ = AppendMenuW(menu, disabled, MENU_STATUS, PCWSTR(status.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_SHOW, w!("打开 Longwatch"));
        let _ = AppendMenuW(
            menu,
            if state.control == TrayControl::Disabled {
                disabled
            } else {
                MF_STRING
            },
            MENU_CONTROL,
            PCWSTR(control.as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            if state.can_open_result {
                MF_STRING
            } else {
                disabled
            },
            MENU_OPEN_RESULT,
            w!("打开 Codex 结果"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT, w!("退出"));
    }
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&raw mut point) }.is_err() {
        unsafe {
            let _ = DestroyMenu(menu);
        }
        return;
    }
    unsafe {
        let _ = SetForegroundWindow(window);
    }
    let command = usize::try_from(
        unsafe {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY,
                point.x,
                point.y,
                None,
                window,
                None,
            )
        }
        .0,
    )
    .unwrap_or_default();
    unsafe {
        let _ = DestroyMenu(menu);
        let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
    }
    match command {
        MENU_SHOW => restore_main_window(MAIN_WINDOW.load(Ordering::Acquire)),
        MENU_CONTROL => match state.control {
            TrayControl::Pause => ACTION_REQUESTED.store(ACTION_PAUSE, Ordering::Release),
            TrayControl::Resume => ACTION_REQUESTED.store(ACTION_RESUME, Ordering::Release),
            TrayControl::Disabled => {}
        },
        MENU_OPEN_RESULT if state.can_open_result => {
            ACTION_REQUESTED.store(ACTION_OPEN_RESULT, Ordering::Release);
        }
        MENU_EXIT => {
            EXIT_REQUESTED.store(true, Ordering::Release);
            let main = MAIN_WINDOW.load(Ordering::Acquire);
            if main != 0 {
                unsafe {
                    let _ = PostMessageW(
                        Some(HWND(main as *mut c_void)),
                        WM_CLOSE,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
        }
        _ => {}
    }
}

fn wide_text(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn restore_main_window(main_window: isize) {
    if main_window == 0 {
        return;
    }
    let window = HWND(main_window as *mut c_void);
    unsafe {
        let _ = ShowWindow(window, SW_RESTORE);
        let _ = SetForegroundWindow(window);
    }
}
