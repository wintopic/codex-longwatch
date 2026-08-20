use thiserror::Error;

#[derive(Debug, Error)]
pub enum AutomationError {
    #[error("GUI fallback is unavailable: {0}")]
    Unavailable(String),
    #[error("GUI fallback failed: {0}")]
    Failed(String),
}

pub trait GuiAutomation: Send + Sync {
    /// Submit the exact user-provided prompt to the visible Codex input.
    ///
    /// # Errors
    ///
    /// Returns an error when accessibility permission, a Codex input element,
    /// or the platform input facility is unavailable.
    fn submit_prompt(&self, prompt: &str) -> Result<(), AutomationError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemGuiAutomation;

impl GuiAutomation for SystemGuiAutomation {
    fn submit_prompt(&self, prompt: &str) -> Result<(), AutomationError> {
        platform_submit_prompt(prompt)
    }
}

#[cfg(target_os = "linux")]
fn platform_submit_prompt(prompt: &str) -> Result<(), AutomationError> {
    if crate::is_wayland_session() {
        return Err(AutomationError::Unavailable(
            "Wayland global input injection is intentionally disabled; use app-server".into(),
        ));
    }
    if std::env::var_os("DISPLAY").is_none() {
        return Err(AutomationError::Unavailable(
            "X11 DISPLAY is unavailable; use app-server".into(),
        ));
    }

    linux_automation::submit(prompt)
}

#[cfg(target_os = "linux")]
mod linux_automation {
    use std::{
        collections::{HashSet, VecDeque},
        thread,
        time::Duration,
    };

    use atspi::{
        AccessibilityConnection, Interface, ObjectRefOwned, Role, State,
        proxy::{accessible::ObjectRefExt, proxy_ext::ProxyExt},
    };
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};

    use super::AutomationError;

    const MAX_TREE_NODES: usize = 4_096;
    const MAX_TREE_DEPTH: usize = 64;
    const APPLICATION_HINTS: &[&str] = &["codex", "chatgpt"];
    const COMPOSER_HINTS: &[&str] = &[
        "message", "prompt", "composer", "ask", "codex", "task", "write", "input", "任务", "消息",
        "提示", "输入",
    ];

    pub fn submit(prompt: &str) -> Result<(), AutomationError> {
        let mut enigo = Enigo::new(&Settings::default())
            .map_err(|error| AutomationError::Failed(format!("X11 input unavailable: {error}")))?;

        async_io::block_on(focus_and_fill(prompt))?;
        thread::sleep(Duration::from_millis(120));
        enigo
            .key(Key::Return, Direction::Click)
            .map_err(|error| AutomationError::Failed(format!("failed to submit with X11: {error}")))
    }

    async fn focus_and_fill(prompt: &str) -> Result<(), AutomationError> {
        let connection = AccessibilityConnection::new().await.map_err(|error| {
            AutomationError::Unavailable(format!(
                "AT-SPI accessibility bus is unavailable: {error}"
            ))
        })?;
        let roots = find_codex_roots(&connection).await?;
        let mut candidates = find_editable_candidates(&connection, roots).await;
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));

        for (_, object) in candidates {
            let Ok(accessible) = object.as_accessible_proxy(connection.connection()).await else {
                continue;
            };
            let Ok(proxies) = accessible.proxies().await else {
                continue;
            };
            let Ok(component) = proxies.component().await else {
                continue;
            };
            if !component.grab_focus().await.unwrap_or(false) {
                continue;
            }
            let Ok(editable) = proxies.editable_text().await else {
                continue;
            };
            if editable.set_text_contents(prompt).await.unwrap_or(false) {
                return Ok(());
            }
        }

        Err(AutomationError::Unavailable(
            "未找到可聚焦的 Codex 输入框；请确认 Codex 窗口已打开且 AT-SPI 可用".into(),
        ))
    }

    async fn find_codex_roots(
        connection: &AccessibilityConnection,
    ) -> Result<Vec<ObjectRefOwned>, AutomationError> {
        let registry = connection
            .root_accessible_on_registry()
            .await
            .map_err(|error| AutomationError::Failed(error.to_string()))?;
        let applications = registry
            .get_children()
            .await
            .map_err(|error| AutomationError::Failed(error.to_string()))?;
        let mut roots = Vec::new();

        for application_ref in applications {
            let Ok(application) = application_ref
                .as_accessible_proxy(connection.connection())
                .await
            else {
                continue;
            };
            if is_codex_application(&accessible_identity(&application).await) {
                roots.push(application_ref);
                continue;
            }

            // Electron application roots can have a generic name.  In that case,
            // identify Codex by the title/metadata of one of its top-level windows.
            let Ok(windows) = application.get_children().await else {
                continue;
            };
            for window_ref in windows {
                let Ok(window) = window_ref
                    .as_accessible_proxy(connection.connection())
                    .await
                else {
                    continue;
                };
                if is_codex_application(&accessible_identity(&window).await) {
                    roots.push(window_ref);
                }
            }
        }

        if roots.is_empty() {
            Err(AutomationError::Unavailable(
                "未在 AT-SPI 可访问性树中找到 Codex 应用；请先打开 Codex 窗口".into(),
            ))
        } else {
            Ok(roots)
        }
    }

    async fn find_editable_candidates(
        connection: &AccessibilityConnection,
        roots: Vec<ObjectRefOwned>,
    ) -> Vec<(i32, ObjectRefOwned)> {
        let mut queue = VecDeque::new();
        let mut visited = HashSet::new();
        let mut candidates = Vec::new();
        for root in roots {
            queue.push_back((root, 0_usize));
        }

        while let Some((object, depth)) = queue.pop_front() {
            if visited.len() >= MAX_TREE_NODES || !visited.insert(object.clone()) {
                continue;
            }
            let Ok(accessible) = object.as_accessible_proxy(connection.connection()).await else {
                continue;
            };
            if let Some(score) = candidate_score(&accessible).await {
                candidates.push((score, object.clone()));
            }
            if depth >= MAX_TREE_DEPTH {
                continue;
            }
            if let Ok(children) = accessible.get_children().await {
                queue.extend(
                    children
                        .into_iter()
                        .filter(|child| !child.is_null())
                        .map(|child| (child, depth + 1)),
                );
            }
        }

        candidates
    }

    async fn candidate_score(
        accessible: &atspi::proxy::accessible::AccessibleProxy<'_>,
    ) -> Option<i32> {
        let interfaces = accessible.get_interfaces().await.ok()?;
        if !interfaces.contains(Interface::EditableText)
            || !interfaces.contains(Interface::Component)
        {
            return None;
        }

        let role = accessible.get_role().await.ok()?;
        if role == Role::PasswordText {
            return None;
        }
        let states = accessible.get_state().await.unwrap_or_default();
        if states.contains(State::Defunct) || states.contains(State::ReadOnly) {
            return None;
        }

        let mut score = match role {
            Role::Entry => 80,
            Role::Text | Role::DocumentText => 55,
            Role::Paragraph => 35,
            _ => 20,
        };
        if states.contains(State::Editable) {
            score += 30;
        }
        if states.contains(State::MultiLine) {
            score += 25;
        }
        if states.contains(State::Focused) {
            score += 20;
        }
        if states.contains(State::Focusable) {
            score += 10;
        }
        if states.contains(State::Visible) && states.contains(State::Showing) {
            score += 10;
        }
        if contains_hint(&accessible_identity(accessible).await, COMPOSER_HINTS) {
            score += 60;
        }
        Some(score)
    }

    async fn accessible_identity(
        accessible: &atspi::proxy::accessible::AccessibleProxy<'_>,
    ) -> String {
        let mut fields = Vec::new();
        if let Ok(value) = accessible.name().await {
            fields.push(value);
        }
        if let Ok(value) = accessible.description().await {
            fields.push(value);
        }
        if let Ok(value) = accessible.accessible_id().await {
            fields.push(value);
        }
        if let Ok(attributes) = accessible.get_attributes().await {
            for (key, value) in attributes {
                fields.push(key);
                fields.push(value);
            }
        }
        fields.join(" ").to_lowercase()
    }

    fn contains_hint(identity: &str, hints: &[&str]) -> bool {
        hints.iter().any(|hint| identity.contains(hint))
    }

    fn is_codex_application(identity: &str) -> bool {
        !identity.contains("codexqueue")
            && !identity.contains("longwatch")
            && !identity.contains("codex queue")
            && contains_hint(identity, APPLICATION_HINTS)
    }
}

#[cfg(target_os = "windows")]
fn platform_submit_prompt(prompt: &str) -> Result<(), AutomationError> {
    windows_automation::submit(prompt)
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod windows_automation {
    use std::{mem::size_of, thread, time::Duration};

    use arboard::Clipboard;
    use windows::{
        Win32::{
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            UI::{
                Accessibility::{
                    CUIAutomation, IUIAutomation, IUIAutomationElement, TreeScope_Children,
                    TreeScope_Descendants, UIA_DocumentControlTypeId, UIA_EditControlTypeId,
                    UIA_WindowControlTypeId,
                },
                Input::KeyboardAndMouse::{
                    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
                    SendInput, VIRTUAL_KEY, VK_A, VK_CONTROL, VK_RETURN, VK_V,
                },
            },
        },
        core::BOOL,
    };

    use super::AutomationError;

    struct ComGuard(bool);

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: this thread successfully called CoInitializeEx.
                unsafe { CoUninitialize() };
            }
        }
    }

    pub fn submit(prompt: &str) -> Result<(), AutomationError> {
        // SAFETY: initialization is balanced by ComGuard. RPC_E_CHANGED_MODE
        // means COM was already initialized and must not be uninitialized here.
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let _guard = ComGuard(com);
        // SAFETY: CUIAutomation is the documented in-process implementation.
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| AutomationError::Failed(error.to_string()))?;
        let input = find_codex_input(&automation)?;
        // SAFETY: input is a live UI Automation element from this apartment.
        unsafe { input.SetFocus() }.map_err(|error| AutomationError::Failed(error.to_string()))?;

        let mut clipboard =
            Clipboard::new().map_err(|error| AutomationError::Failed(error.to_string()))?;
        let previous_text = clipboard.get_text().ok();
        clipboard
            .set_text(prompt.to_owned())
            .map_err(|error| AutomationError::Failed(error.to_string()))?;
        thread::sleep(Duration::from_millis(100));

        let keys = [
            key(VK_CONTROL, false),
            key(VK_A, false),
            key(VK_A, true),
            key(VK_V, false),
            key(VK_V, true),
            key(VK_CONTROL, true),
            key(VK_RETURN, false),
            key(VK_RETURN, true),
        ];
        // SAFETY: keys is a valid INPUT slice and cbSize matches INPUT.
        let sent =
            unsafe { SendInput(&keys, i32::try_from(size_of::<INPUT>()).unwrap_or(i32::MAX)) };
        thread::sleep(Duration::from_millis(250));
        if let Some(previous_text) = previous_text {
            let _ = clipboard.set_text(previous_text);
        } else {
            let _ = clipboard.clear();
        }
        let expected = u32::try_from(keys.len()).unwrap_or(u32::MAX);
        if sent != expected {
            return Err(AutomationError::Failed(format!(
                "Windows accepted only {sent}/{} keyboard events",
                keys.len()
            )));
        }
        Ok(())
    }

    fn key(code: VIRTUAL_KEY, key_up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: code,
                    dwFlags: if key_up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS::default()
                    },
                    ..Default::default()
                },
            },
        }
    }

    fn find_codex_input(
        automation: &IUIAutomation,
    ) -> Result<IUIAutomationElement, AutomationError> {
        // SAFETY: interfaces and conditions are returned by UI Automation;
        // array bounds are checked before GetElement.
        unsafe {
            let root = automation
                .GetRootElement()
                .map_err(|error| AutomationError::Failed(error.to_string()))?;
            let condition = automation
                .CreateTrueCondition()
                .map_err(|error| AutomationError::Failed(error.to_string()))?;
            let windows = root
                .FindAll(TreeScope_Children, &condition)
                .map_err(|error| AutomationError::Failed(error.to_string()))?;
            let window_count = windows
                .Length()
                .map_err(|error| AutomationError::Failed(error.to_string()))?;
            for window_index in 0..window_count {
                let window = windows
                    .GetElement(window_index)
                    .map_err(|error| AutomationError::Failed(error.to_string()))?;
                if window.CurrentControlType().ok() != Some(UIA_WindowControlTypeId) {
                    continue;
                }
                let name = window
                    .CurrentName()
                    .map(|name| name.to_string().to_lowercase())
                    .unwrap_or_default();
                if !name.contains("codex")
                    || name.contains("codexqueue")
                    || name.contains("longwatch")
                    || name.contains("codex queue")
                {
                    continue;
                }
                let descendants = window
                    .FindAll(TreeScope_Descendants, &condition)
                    .map_err(|error| AutomationError::Failed(error.to_string()))?;
                let count = descendants
                    .Length()
                    .map_err(|error| AutomationError::Failed(error.to_string()))?;
                let mut edit = None;
                let mut document = None;
                for index in 0..count {
                    let element = descendants
                        .GetElement(index)
                        .map_err(|error| AutomationError::Failed(error.to_string()))?;
                    if !element.CurrentIsEnabled().is_ok_and(BOOL::as_bool)
                        || !element
                            .CurrentIsKeyboardFocusable()
                            .is_ok_and(BOOL::as_bool)
                        || element.CurrentIsOffscreen().map_or(true, BOOL::as_bool)
                    {
                        continue;
                    }
                    match element.CurrentControlType().ok() {
                        Some(kind) if kind == UIA_EditControlTypeId => {
                            let identity = format!(
                                "{} {}",
                                element
                                    .CurrentName()
                                    .map(|value| value.to_string())
                                    .unwrap_or_default(),
                                element
                                    .CurrentAutomationId()
                                    .map(|value| value.to_string())
                                    .unwrap_or_default()
                            )
                            .to_lowercase();
                            if ["message", "prompt", "ask", "codex", "任务", "消息", "输入"]
                                .iter()
                                .any(|hint| identity.contains(hint))
                            {
                                return Ok(element);
                            }
                            // The composer is commonly the last editable
                            // control in Chromium's accessibility order.
                            edit = Some(element);
                        }
                        Some(kind) if kind == UIA_DocumentControlTypeId => document = Some(element),
                        _ => {}
                    }
                }
                if let Some(edit) = edit {
                    return Ok(edit);
                }
                if let Some(document) = document {
                    return Ok(document);
                }
            }
        }
        Err(AutomationError::Unavailable(
            "未找到可聚焦的 Codex 输入框；请先打开 Codex 窗口".into(),
        ))
    }
}

#[cfg(target_os = "macos")]
fn platform_submit_prompt(prompt: &str) -> Result<(), AutomationError> {
    macos_automation::submit(prompt)
}

#[cfg(target_os = "macos")]
mod macos_automation {
    use std::{thread, time::Duration};

    use accessibility::{
        AXAttribute, AXUIElement, AXUIElementActions, AXUIElementAttributes, ElementFinder,
    };
    use arboard::Clipboard;
    use core_foundation::{
        base::{CFType, TCFType},
        boolean::CFBoolean,
        string::CFString,
    };
    use core_graphics::{
        event::{CGEvent, CGEventFlags, CGEventTapLocation, KeyCode},
        event_source::{CGEventSource, CGEventSourceStateID},
    };
    use macos_accessibility_client::accessibility::application_is_trusted_with_prompt;

    use super::AutomationError;

    const CODEX_BUNDLE_IDS: &[&str] =
        &["com.openai.codex", "com.openai.chat", "com.openai.chatgpt"];
    const KEY_A: u16 = 0x00;
    const KEY_V: u16 = 0x09;

    pub fn submit(prompt: &str) -> Result<(), AutomationError> {
        if !application_is_trusted_with_prompt() {
            return Err(AutomationError::Unavailable(
                "请在系统设置 → 隐私与安全性 → 辅助功能中授权 Longwatch，然后重试".into(),
            ));
        }

        let application = CODEX_BUNDLE_IDS
            .iter()
            .find_map(|bundle| AXUIElement::application_with_bundle(bundle).ok())
            .ok_or_else(|| {
                AutomationError::Unavailable("未找到正在运行的 Codex 应用；请先打开窗口".into())
            })?;
        application
            .set_frontmost(true)
            .map_err(|error| AutomationError::Failed(error.to_string()))?;
        if let Ok(window) = application.main_window() {
            let _ = window.raise();
        }

        let input = find_input(&application)?;
        let focused = AXAttribute::<CFType>::new(&CFString::from_static_string("AXFocused"));
        input
            .set_attribute(&focused, CFBoolean::true_value().as_CFType())
            .map_err(|error| AutomationError::Failed(error.to_string()))?;

        let mut clipboard = ClipboardRestore::install(prompt)?;
        thread::sleep(Duration::from_millis(120));
        post_key(KEY_A, CGEventFlags::CGEventFlagCommand)?;
        thread::sleep(Duration::from_millis(60));
        post_key(KEY_V, CGEventFlags::CGEventFlagCommand)?;
        thread::sleep(Duration::from_millis(120));
        post_key(KeyCode::RETURN, CGEventFlags::CGEventFlagNull)?;
        thread::sleep(Duration::from_millis(250));
        clipboard.restore();
        Ok(())
    }

    fn find_input(application: &AXUIElement) -> Result<AXUIElement, AutomationError> {
        let hinted_input = ElementFinder::new(
            application,
            |element| {
                (role_is(element, "AXTextArea") || role_is(element, "AXTextField"))
                    && is_enabled(element)
                    && is_editable(element)
                    && has_composer_hint(element)
            },
            Some(Duration::from_secs(2)),
        )
        .find();
        if let Ok(element) = hinted_input {
            return Ok(element);
        }

        ElementFinder::new(
            application,
            |element| role_is(element, "AXTextArea") && is_enabled(element) && is_editable(element),
            Some(Duration::from_secs(1)),
        )
        .find()
        .map_err(|_| {
            AutomationError::Unavailable(
                "未找到 Codex 输入框；请确认对话窗口已打开且输入框可见".into(),
            )
        })
    }

    fn role_is(element: &AXUIElement, expected: &str) -> bool {
        element.role().is_ok_and(|role| role == expected)
    }

    fn is_enabled(element: &AXUIElement) -> bool {
        element.enabled().map(bool::from).unwrap_or(true)
    }

    fn is_editable(element: &AXUIElement) -> bool {
        element
            .is_settable(&AXAttribute::<()>::value())
            .unwrap_or(false)
    }

    fn has_composer_hint(element: &AXUIElement) -> bool {
        [
            element.placeholder_value().ok(),
            element.description().ok(),
            element.title().ok(),
        ]
        .into_iter()
        .flatten()
        .map(|value| value.to_string().to_lowercase())
        .any(|value| {
            ["message", "prompt", "ask", "codex", "任务", "消息"]
                .iter()
                .any(|hint| value.contains(hint))
        })
    }

    fn post_key(keycode: u16, flags: CGEventFlags) -> Result<(), AutomationError> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| AutomationError::Failed("无法创建 CGEvent 输入源".into()))?;
        let down = CGEvent::new_keyboard_event(source.clone(), keycode, true)
            .map_err(|()| AutomationError::Failed("无法创建 CGEvent 按键按下事件".into()))?;
        down.set_flags(flags);
        down.post(CGEventTapLocation::HID);
        let up = CGEvent::new_keyboard_event(source, keycode, false)
            .map_err(|()| AutomationError::Failed("无法创建 CGEvent 按键释放事件".into()))?;
        up.set_flags(flags);
        up.post(CGEventTapLocation::HID);
        Ok(())
    }

    struct ClipboardRestore {
        clipboard: Clipboard,
        previous_text: Option<String>,
        restored: bool,
    }

    impl ClipboardRestore {
        fn install(prompt: &str) -> Result<Self, AutomationError> {
            let mut clipboard =
                Clipboard::new().map_err(|error| AutomationError::Failed(error.to_string()))?;
            let previous_text = clipboard.get_text().ok();
            clipboard
                .set_text(prompt.to_owned())
                .map_err(|error| AutomationError::Failed(error.to_string()))?;
            Ok(Self {
                clipboard,
                previous_text,
                restored: false,
            })
        }

        fn restore(&mut self) {
            if self.restored {
                return;
            }
            if let Some(previous_text) = self.previous_text.take() {
                let _ = self.clipboard.set_text(previous_text);
            } else {
                let _ = self.clipboard.clear();
            }
            self.restored = true;
        }
    }

    impl Drop for ClipboardRestore {
        fn drop(&mut self) {
            self.restore();
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn platform_submit_prompt(_prompt: &str) -> Result<(), AutomationError> {
    Err(AutomationError::Unavailable(
        "unsupported desktop platform".into(),
    ))
}
