//! Longwatch desktop shell.
//!
//! The layout follows the Qoder workbench: a 40 px title bar and a sidebar on
//! a tinted shell, next to one bordered content card. The sidebar *is* the
//! queue — running, waiting and finished tasks — so the window answers "what
//! is queued and how far along is it?" at a glance. Everything else (queue
//! settings, diagnostics) lives on the settings page, except the model, which
//! is picked in the input box like the folder.

use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, App, Application, Bounds, BoxShadow, ClipboardItem, Context, Corner, Div,
    ElementId, Entity, EntityInputHandler, Focusable, FontWeight, Image, ImageFormat, KeyDownEvent,
    MouseDownEvent, PathPromptOptions, Pixels, Render, ScrollHandle, SharedString, Stateful,
    StyleRefinement, Timer, TitlebarOptions, Window, WindowAppearance, WindowBounds,
    WindowControlArea, WindowOptions, anchored, canvas, deferred, div, img, point, prelude::*, px,
    rgb, rgba, size,
};
use gpui_component::{
    Root as ComponentRoot,
    input::{Input, InputEvent, InputState},
    text::{TextView, TextViewStyle},
};
#[cfg(target_os = "windows")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tdesign_gpui::{
    Icon, IconName, TDesignAssetSource, TDesignConfig, TDesignRoot, ThemeMode, ThemeOverrides,
};
use tracing::warn;

use crate::{
    app_server::fetch_model_catalog,
    config::{
        ClientIdentity, ConfigStore, MAX_CONCURRENT_SESSIONS, MIN_CONCURRENT_SESSIONS, QueueConfig,
    },
    diagnostics::{self, CodexCliInfo},
    queue::{
        ConversationMessage, ConversationRole, PendingTaskSummary, QueuePhase, QueueSessionSummary,
        QueueSnapshot,
    },
    runtime::{RuntimeCommand, RuntimeHandle},
    transport::ModelCatalog,
};

const TITLEBAR_HEIGHT: f32 = 40.;
const SIDEBAR_WIDTH: f32 = 256.;
const CHAT_WIDTH: f32 = 720.;
const CONTENT_RADIUS: f32 = 10.;
const COMPOSER_RADIUS: f32 = 20.;
/// Text column of a sidebar row: sidebar padding, row padding, indicator and
/// gap removed. gpui only ellipsizes text whose width is known up front.
const ROW_TEXT_WIDTH: f32 = SIDEBAR_WIDTH - 16. - 16. - 14. - 9.;

/// Qoder's "light" and "forest-dark" tokens, flattened to the handful of
/// roles this window uses.
#[derive(Clone, Copy, Debug)]
struct Palette {
    dark: bool,
    shell: u32,
    surface: u32,
    elevated: u32,
    base: u32,
    fill_secondary: u32,
    text: u32,
    text_secondary: u32,
    text_tertiary: u32,
    text_quaternary: u32,
    border_secondary: u32,
    border_tertiary: u32,
    primary: u32,
    primary_hover: u32,
    primary_bg: u32,
    primary_border: u32,
    primary_text: u32,
    on_primary: u32,
    success: u32,
    warning: u32,
    error: u32,
    error_bg: u32,
    switch_off: u32,
    /// RGBA overlays for sidebar rows, drawn over the shell.
    hover: u32,
    active: u32,
    mask: u32,
}

impl Palette {
    fn for_window(window: &Window) -> Self {
        // Debug builds can preview either palette without changing the OS
        // theme, matching the LONGWATCH_CONFIG_DIR override for previews.
        #[cfg(debug_assertions)]
        match std::env::var("LONGWATCH_FORCE_THEME").as_deref() {
            Ok("dark") => return Self::dark(),
            Ok("light") => return Self::light(),
            _ => {}
        }
        if matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ) {
            Self::dark()
        } else {
            Self::light()
        }
    }

    const fn light() -> Self {
        Self {
            dark: false,
            shell: 0xf6f6f6,
            surface: 0xffffff,
            elevated: 0xf9f9f9,
            base: 0xffffff,
            fill_secondary: 0xefefef,
            text: 0x141414,
            text_secondary: 0x636261,
            text_tertiary: 0x838280,
            text_quaternary: 0xaaa9a8,
            border_secondary: 0xdddddd,
            border_tertiary: 0xe6e6e6,
            primary: 0x4b6f5a,
            primary_hover: 0x436651,
            primary_bg: 0xf2f4f2,
            primary_border: 0xd5dbd8,
            primary_text: 0x4b6f5a,
            on_primary: 0xfdfdfd,
            success: 0x579b6e,
            warning: 0xefb041,
            error: 0xec5b56,
            error_bg: 0xfdf2f0,
            switch_off: 0xdfdfdf,
            hover: 0x0000_000a,
            active: 0x0000_0012,
            mask: 0x9595_9466,
        }
    }

    const fn dark() -> Self {
        Self {
            dark: true,
            shell: 0x242624,
            surface: 0x191b1a,
            elevated: 0x252825,
            base: 0x222523,
            fill_secondary: 0x2d332e,
            text: 0xd9dcd9,
            text_secondary: 0xb3bab4,
            text_tertiary: 0x949d96,
            text_quaternary: 0x8a948c,
            border_secondary: 0x404842,
            border_tertiary: 0x363c37,
            primary: 0x4d9c61,
            primary_hover: 0x62ad78,
            primary_bg: 0x14261c,
            primary_border: 0x335942,
            primary_text: 0x8ee5a1,
            on_primary: 0x080807,
            success: 0x73cd94,
            warning: 0xfaad14,
            error: 0xff4d4f,
            error_bg: 0x4a1a1b,
            switch_off: 0x6e736f,
            hover: 0x2c31_2dff,
            active: 0x343b_35ff,
            mask: 0x0000_0099,
        }
    }

    /// Qoder's `natural-shadow-sm`; dark surfaces separate by tone alone.
    fn soft_shadow(self) -> Vec<BoxShadow> {
        if self.dark {
            return Vec::new();
        }
        vec![
            BoxShadow {
                color: rgba(0x0000_000a).into(),
                offset: point(px(0.), px(1.)),
                blur_radius: px(1.),
                spread_radius: px(0.),
            },
            BoxShadow {
                color: rgba(0x0000_0014).into(),
                offset: point(px(0.), px(4.)),
                blur_radius: px(12.),
                spread_radius: px(-6.),
            },
        ]
    }

    fn overlay_shadow(self) -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: rgba(if self.dark { 0x0000_0066 } else { 0x0000_0024 }).into(),
            offset: point(px(0.), px(12.)),
            blur_radius: px(32.),
            spread_radius: px(-8.),
        }]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Selection {
    New,
    Session(u64),
    Pending(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Page {
    Queue,
    Settings,
}

/// What the composer does with its text for the current selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ComposerTarget {
    NewTask,
    FollowUp { session_id: u64, running: bool },
    Blocked(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ButtonStyle {
    Ghost,
    Outline,
    Primary,
    DangerGhost,
    Danger,
}

#[derive(Clone, Debug)]
struct Notice {
    id: u64,
    text: String,
    error: bool,
}

/// The model list is optional: without it the picker still offers "follow
/// Codex settings", and the chip falls back to a generic label.
enum ModelCatalogState {
    Loading,
    Ready(ModelCatalog),
    Failed,
}

// Independent view flags (copy feedback, dialogs, validation) rather than a
// state machine.
#[allow(clippy::struct_excessive_bools)]
struct QueueView {
    config: QueueConfig,
    snapshot: QueueSnapshot,
    commands: tokio::sync::mpsc::Sender<RuntimeCommand>,
    selection: Selection,
    /// The first snapshots after launch may arrive before the scheduler has
    /// restored anything; the initial selection waits for real sessions.
    initial_selection_pending: bool,
    requested_session: Option<u64>,
    page: Page,
    prompt_input: Entity<InputState>,
    default_prompt_input: Entity<InputState>,
    phrases_input: Entity<InputState>,
    placeholder: &'static str,
    /// `None` while `codex --version` is still running in the background.
    codex_cli: Option<CodexCliInfo>,
    /// Models Codex offers, read in the background once the CLI is found.
    model_catalog: ModelCatalogState,
    model_menu_open: bool,
    /// Where the model chip was last painted; its menu opens above it.
    model_chip_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Runs the app-server that lists models; the UI thread is not in Tokio.
    tokio: tokio::runtime::Handle,
    logo: Arc<Image>,
    next_request_id: u64,
    /// Submission whose text was cleared optimistically, restored if rejected.
    pending_clear: Option<(u64, String)>,
    /// New task whose placement (session or waiting slot) is still unknown.
    awaiting_placement: Option<u64>,
    prompt_error: bool,
    timeline_scroll: ScrollHandle,
    reply_copied: bool,
    diagnostics_copied: bool,
    notice: Option<Notice>,
    notice_counter: u64,
    exit_confirm_open: bool,
    closing: Arc<AtomicBool>,
    runtime_stopped: bool,
    last_slow_refresh: Instant,
    /// Width available to a content header, refreshed on every render.
    header_space: f32,
    #[cfg(target_os = "windows")]
    tray_status: String,
    config_directory: PathBuf,
    logs_directory: PathBuf,
    _subscriptions: Vec<gpui::Subscription>,
    #[allow(dead_code)] // Stored for its lifetime; dropping it stops theme-change callbacks.
    appearance_subscription: Option<gpui::Subscription>,
}

impl QueueView {
    fn send(&self, command: RuntimeCommand) -> bool {
        // UI callbacks run on GPUI's executor, not necessarily inside a Tokio
        // reactor. A synchronous bounded send avoids spawning a Tokio future
        // during window teardown.
        self.commands.try_send(command).is_ok()
    }

    fn configure(&self) {
        let _ = self.send(RuntimeCommand::Configure(self.config.clone()));
    }

    fn allocate_request_id(&mut self) -> u64 {
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        self.next_request_id
    }

    fn summary(&self, id: u64) -> Option<&QueueSessionSummary> {
        self.snapshot
            .sessions
            .iter()
            .find(|session| session.id == id && session.has_history)
    }

    /// Full details are only published for the session the runtime has
    /// selected; anything else shows its summary until the switch lands.
    fn detail(&self, id: u64) -> Option<&QueueSnapshot> {
        (self.snapshot.session_id == Some(id)).then_some(&self.snapshot)
    }

    fn pending_follow_up_for(&self, session_id: u64) -> Option<&PendingTaskSummary> {
        self.snapshot
            .pending_tasks
            .iter()
            .find(|task| task.target_session_id == Some(session_id))
    }

    /// Width of the conversation column. It is set explicitly: replies can
    /// contain long unbreakable runs whose min-content width would otherwise
    /// stretch the composer past the card.
    fn chat_width(&self) -> f32 {
        (self.header_space + 32. - 56.).clamp(320., CHAT_WIDTH)
    }

    fn has_active_work(&self) -> bool {
        self.snapshot.running_session_count > 0 || !self.snapshot.pending_tasks.is_empty()
    }

    fn select(&mut self, selection: Selection, window: &mut Window, cx: &mut Context<Self>) {
        self.model_menu_open = false;
        if self.page == Page::Settings {
            self.save_text_settings(cx);
            self.page = Page::Queue;
        }
        if self.selection != selection {
            self.selection = selection;
            self.reply_copied = false;
            self.prompt_error = false;
            self.timeline_scroll.scroll_to_bottom();
        }
        self.initial_selection_pending = false;
        if let Selection::Session(id) = selection {
            self.request_runtime_selection(id);
        }
        self.sync_placeholder(window, cx);
        cx.notify();
    }

    fn start_new_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select(Selection::New, window, cx);
        self.prompt_input
            .update(cx, |state, cx| state.focus(window, cx));
    }

    fn request_runtime_selection(&mut self, id: u64) {
        if self.snapshot.session_id != Some(id)
            && self.requested_session != Some(id)
            && self.send(RuntimeCommand::SelectSession(id))
        {
            self.requested_session = Some(id);
        }
    }

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.page = Page::Settings;
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.save_text_settings(cx);
        self.page = Page::Queue;
        cx.notify();
    }

    fn composer_target(&self) -> ComposerTarget {
        match self.selection {
            Selection::New | Selection::Pending(_) => ComposerTarget::NewTask,
            Selection::Session(id) => {
                let Some(detail) = self.detail(id) else {
                    return ComposerTarget::Blocked("正在载入任务…");
                };
                let running = self.summary(id).is_some_and(|summary| summary.active);
                if detail.phase == QueuePhase::Paused {
                    ComposerTarget::Blocked("任务已暂停，继续或停止后才能追加消息")
                } else if detail.phase == QueuePhase::FatalError {
                    ComposerTarget::Blocked("请先重试或停止这个任务")
                } else if detail.queued_message.is_some()
                    || self.pending_follow_up_for(id).is_some()
                {
                    ComposerTarget::Blocked("已有一条追加消息在等待发送")
                } else {
                    ComposerTarget::FollowUp {
                        session_id: id,
                        running,
                    }
                }
            }
        }
    }

    fn sync_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = match self.composer_target() {
            ComposerTarget::NewTask => "描述要交给 Codex 的任务，按 Enter 加入队列",
            ComposerTarget::FollowUp { running: true, .. } => {
                "补充一条消息，当前回合结束后自动发送"
            }
            ComposerTarget::FollowUp { running: false, .. } => "继续这个会话，按 Enter 发送",
            ComposerTarget::Blocked(reason) => reason,
        };
        if placeholder != self.placeholder {
            self.placeholder = placeholder;
            self.prompt_input.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx)
            });
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = self.prompt_input.read(cx).value().to_string();
        let target = self.composer_target();
        if matches!(target, ComposerTarget::Blocked(_)) {
            return;
        }
        if prompt.trim().is_empty() {
            self.prompt_error = true;
            cx.notify();
            return;
        }
        self.prompt_error = false;
        let request_id = self.allocate_request_id();
        let sent = match target {
            ComposerTarget::NewTask => {
                let config = QueueConfig {
                    prompt: prompt.clone(),
                    working_directory: self.config.default_working_directory.clone(),
                    model: self.config.default_model.clone(),
                    reasoning_effort: self.config.default_reasoning_effort.clone(),
                    ..self.config.clone()
                };
                let sent = self.send(RuntimeCommand::StartNewConfigured { config, request_id });
                if sent {
                    self.awaiting_placement = Some(request_id);
                }
                sent
            }
            ComposerTarget::FollowUp { session_id, .. } => {
                // Follow-ups stay in the conversation's own folder and model.
                let config = QueueConfig {
                    prompt: prompt.clone(),
                    working_directory: self.snapshot.session_working_directory.clone(),
                    model: self.snapshot.session_model.clone(),
                    reasoning_effort: self.snapshot.session_reasoning_effort.clone(),
                    ..self.config.clone()
                };
                self.request_runtime_selection(session_id);
                self.send(RuntimeCommand::SubmitConfigured { config, request_id })
            }
            ComposerTarget::Blocked(_) => false,
        };
        if sent {
            self.pending_clear = Some((request_id, prompt));
            self.prompt_input
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.timeline_scroll.scroll_to_bottom();
        } else {
            self.show_notice("后台没有响应，请稍后再试", true, window, cx);
        }
        cx.notify();
    }

    fn run_selected(&mut self, command: RuntimeCommand, cx: &mut Context<Self>) {
        if let Selection::Session(id) = self.selection {
            self.request_runtime_selection(id);
            let _ = self.send(command);
            cx.notify();
        }
    }

    fn dismiss_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Selection::Session(id) = self.selection {
            let _ = self.send(RuntimeCommand::DismissSession(id));
            self.select(Selection::New, window, cx);
        }
    }

    fn cancel_pending(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.send(RuntimeCommand::CancelPending(id));
        if self.selection == Selection::Pending(id) {
            self.select(Selection::New, window, cx);
        }
        cx.notify();
    }

    fn clear_finished(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.send(RuntimeCommand::ClearFinished);
        if let Selection::Session(id) = self.selection
            && self.summary(id).is_some_and(session_is_finished)
        {
            self.select(Selection::New, window, cx);
        }
        cx.notify();
    }

    fn selected_thread_id(&self) -> Option<String> {
        match self.selection {
            Selection::Session(id) => self.summary(id)?.active_thread_id.clone(),
            _ => None,
        }
    }

    fn open_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_id) = self.selected_thread_id() else {
            return;
        };
        if let Err(error) =
            gpui_platform::open_thread(&thread_id, &self.config.codex_path.to_string_lossy())
        {
            warn!(%error, "无法打开 Codex 会话");
            self.show_notice(format!("无法打开 Codex：{error}"), true, window, cx);
        }
    }

    fn copy_reply(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.reply_copied = true;
        cx.spawn_in(window, async move |view, cx| {
            Timer::after(Duration::from_millis(1400)).await;
            let _ = view.update(cx, |view, cx| {
                view.reply_copied = false;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn copy_diagnostics(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cli = self.codex_cli.clone().unwrap_or_else(|| CodexCliInfo {
            requested_path: self.config.codex_path.clone(),
            resolved_path: None,
            version: None,
            error: Some("仍在检测".into()),
        });
        let report =
            diagnostics::build_support_report(&self.config_directory, &cli, &self.snapshot);
        cx.write_to_clipboard(ClipboardItem::new_string(report));
        self.diagnostics_copied = true;
        cx.spawn_in(window, async move |view, cx| {
            Timer::after(Duration::from_millis(1600)).await;
            let _ = view.update(cx, |view, cx| {
                view.diagnostics_copied = false;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_logs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = gpui_platform::open_directory(&self.logs_directory) {
            self.show_notice(format!("无法打开日志目录：{error}"), true, window, cx);
        }
    }

    fn pick_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择任务的工作目录".into()),
        });
        cx.spawn_in(window, async move |view, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = view.update(cx, |view, cx| {
                view.config.default_working_directory = Some(path);
                view.configure();
                cx.notify();
            });
        })
        .detach();
    }

    fn clear_folder(&mut self, cx: &mut Context<Self>) {
        self.config.default_working_directory = None;
        self.configure();
        cx.notify();
    }

    /// Read the model list with a short-lived app-server, the same request
    /// the official app makes for its picker.
    fn load_models(&mut self, cx: &mut Context<Self>) {
        self.model_catalog = ModelCatalogState::Loading;
        let fetch = self.tokio.spawn(fetch_model_catalog(
            self.config.codex_path.clone(),
            self.config.client_identity,
        ));
        cx.spawn(async move |view, cx| {
            let catalog = match fetch.await {
                Ok(Ok(catalog)) => ModelCatalogState::Ready(catalog),
                Ok(Err(error)) => {
                    warn!(%error, "无法读取 Codex 模型列表");
                    ModelCatalogState::Failed
                }
                Err(error) => {
                    warn!(%error, "读取 Codex 模型列表的后台任务异常结束");
                    ModelCatalogState::Failed
                }
            };
            let _ = view.update(cx, |view, cx| {
                view.model_catalog = catalog;
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_model_menu(&mut self, cx: &mut Context<Self>) {
        self.model_menu_open = !self.model_menu_open;
        if self.model_menu_open && matches!(self.model_catalog, ModelCatalogState::Failed) {
            self.load_models(cx);
        }
        cx.notify();
    }

    /// Pick the model for new tasks. A model keeps the menu open so its
    /// reasoning effort can be chosen next; the effort carries over when the
    /// new model supports it and otherwise goes back to 默认 (Codex decides).
    fn choose_model(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        let effort = match (&self.model_catalog, model.as_deref()) {
            (ModelCatalogState::Ready(catalog), Some(model)) => {
                catalog.find(model).and_then(|info| {
                    self.config
                        .default_reasoning_effort
                        .clone()
                        .filter(|current| info.efforts.contains(current))
                })
            }
            _ => None,
        };
        if model.is_none() {
            self.model_menu_open = false;
        }
        self.config.default_model = model;
        self.config.default_reasoning_effort = effort;
        self.configure();
        cx.notify();
    }

    /// `None` is 默认: Codex uses the effort from its config, or the model's
    /// own default.
    fn choose_effort(&mut self, effort: Option<String>, cx: &mut Context<Self>) {
        self.config.default_reasoning_effort = effort;
        self.model_menu_open = false;
        self.configure();
        cx.notify();
    }

    fn catalog(&self) -> Option<&ModelCatalog> {
        match &self.model_catalog {
            ModelCatalogState::Ready(catalog) => Some(catalog),
            ModelCatalogState::Loading | ModelCatalogState::Failed => None,
        }
    }

    fn model_name(&self, model: &str) -> String {
        self.catalog()
            .and_then(|catalog| catalog.find(model))
            .map_or_else(|| model.to_owned(), |info| info.display_name.clone())
    }

    /// Chip text: the chosen model and effort, or the model Codex falls back
    /// to when Longwatch sends none.
    fn model_label(&self, model: Option<&str>, effort: Option<&str>) -> String {
        match (model, effort) {
            (Some(model), Some(effort)) => {
                format!("{} · {}", self.model_name(model), effort_label(effort))
            }
            (Some(model), None) => self.model_name(model),
            (None, _) => self
                .catalog()
                .and_then(ModelCatalog::default_model)
                .map_or_else(
                    || "默认模型".to_owned(),
                    |model| format!("默认 · {}", self.model_name(model)),
                ),
        }
    }

    fn render_model_menu(
        &self,
        chip: Bounds<Pixels>,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chosen_model = self.config.default_model.as_deref();
        let chosen_effort = self.config.default_reasoning_effort.as_deref();
        let catalog = self.catalog();
        let default_detail = catalog.and_then(ModelCatalog::default_model).map(|model| {
            let name = self.model_name(model);
            match catalog.and_then(|catalog| catalog.configured_effort.as_deref()) {
                Some(effort) => format!("当前为 {name} · {}", effort_label(effort)),
                None => format!("当前为 {name}"),
            }
        });
        let room_above = f32::from(chip.top()) - TITLEBAR_HEIGHT - 20.;
        let mut menu = div()
            .id("model-menu")
            .occlude()
            .w(px(MODEL_MENU_WIDTH))
            .max_h(px(room_above.clamp(160., 480.)))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .p(px(4.))
            .rounded(px(12.))
            .border_1()
            .border_color(rgb(palette.border_tertiary))
            .bg(rgb(palette.base))
            .shadow(palette.overlay_shadow())
            .on_mouse_down_out(cx.listener(move |view, event: &MouseDownEvent, _, cx| {
                // A press on the chip itself is its own toggle.
                if !chip.contains(&event.position) {
                    view.model_menu_open = false;
                    cx.notify();
                }
            }))
            .child(menu_section("模型", palette))
            .child(
                model_menu_item(
                    "model-default",
                    "跟随 Codex 设置",
                    default_detail,
                    chosen_model.is_none(),
                    palette,
                )
                .on_click(cx.listener(|view, _, _, cx| view.choose_model(None, cx))),
            );
        match &self.model_catalog {
            ModelCatalogState::Loading => {
                menu = menu.child(menu_note("正在读取 Codex 的模型列表…", palette));
            }
            ModelCatalogState::Failed => {
                menu = menu.child(menu_note("无法读取模型列表", palette)).child(
                    model_menu_item("model-reload", "重新读取", None, false, palette)
                        .on_click(cx.listener(|view, _, _, cx| view.load_models(cx))),
                );
            }
            ModelCatalogState::Ready(catalog) => {
                for (index, info) in catalog.models.iter().enumerate() {
                    let model = info.model.clone();
                    menu = menu.child(
                        model_menu_item(
                            ElementId::Name(format!("model-option-{index}").into()),
                            &info.display_name,
                            None,
                            chosen_model == Some(info.model.as_str()),
                            palette,
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.choose_model(Some(model.clone()), cx);
                        })),
                    );
                }
            }
        }
        if let Some(info) = chosen_model
            .and_then(|model| catalog?.find(model))
            .filter(|info| !info.efforts.is_empty())
        {
            let mut efforts = div()
                .flex_none()
                .flex()
                .flex_wrap()
                .gap(px(4.))
                .px(px(6.))
                .pb(px(6.))
                .child(
                    effort_pill("effort-default", "默认", chosen_effort.is_none(), palette)
                        .on_click(cx.listener(|view, _, _, cx| view.choose_effort(None, cx))),
                );
            for (index, effort) in info.efforts.iter().enumerate() {
                let value = effort.clone();
                efforts = efforts.child(
                    effort_pill(
                        ElementId::Name(format!("effort-option-{index}").into()),
                        effort_label(effort),
                        chosen_effort == Some(effort.as_str()),
                        palette,
                    )
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.choose_effort(Some(value.clone()), cx);
                    })),
                );
            }
            menu = menu
                .child(
                    div()
                        .flex_none()
                        .h(px(1.))
                        .mx(px(4.))
                        .my(px(4.))
                        .bg(rgb(palette.border_tertiary)),
                )
                .child(menu_section("推理强度", palette))
                .child(efforts);
        }
        deferred(
            anchored()
                .snap_to_window_with_margin(px(8.))
                .anchor(Corner::BottomRight)
                .position(point(chip.right(), chip.top() - px(6.)))
                .child(menu),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn adjust_concurrency(&mut self, delta: i16, cx: &mut Context<Self>) {
        let next = (i16::from(self.config.max_concurrent_sessions) + delta).clamp(
            i16::from(MIN_CONCURRENT_SESSIONS),
            i16::from(MAX_CONCURRENT_SESSIONS),
        );
        let next = u8::try_from(next).unwrap_or(MIN_CONCURRENT_SESSIONS);
        if next != self.config.max_concurrent_sessions {
            self.config.max_concurrent_sessions = next;
            self.configure();
            cx.notify();
        }
    }

    fn set_identity(&mut self, identity: ClientIdentity, cx: &mut Context<Self>) {
        if self.config.client_identity != identity {
            self.config.client_identity = identity;
            self.configure();
            cx.notify();
        }
    }

    fn toggle_audio(&mut self, cx: &mut Context<Self>) {
        self.config.audio_alert_enabled = !self.config.audio_alert_enabled;
        self.configure();
        cx.notify();
    }

    fn toggle_auto_send(&mut self, cx: &mut Context<Self>) {
        self.save_text_settings(cx);
        self.config.auto_send_default_prompt = !self.config.auto_send_default_prompt;
        self.configure();
        cx.notify();
    }

    fn save_text_settings(&mut self, cx: &mut Context<Self>) {
        let default_prompt = self.default_prompt_input.read(cx).value().to_string();
        let failure_phrases = parse_phrases(&self.phrases_input.read(cx).value());
        if default_prompt != self.config.default_prompt
            || failure_phrases != self.config.failure_phrases
        {
            self.config.default_prompt = default_prompt;
            self.config.failure_phrases = failure_phrases;
            self.configure();
        }
    }

    fn confirm_exit(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        self.closing.store(true, Ordering::Release);
        window.remove_window();
    }

    fn show_notice(
        &mut self,
        text: impl Into<String>,
        error: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notice_counter = self.notice_counter.wrapping_add(1);
        let id = self.notice_counter;
        self.notice = Some(Notice {
            id,
            text: text.into(),
            error,
        });
        cx.spawn_in(window, async move |view, cx| {
            Timer::after(Duration::from_millis(3600)).await;
            let _ = view.update(cx, |view, cx| {
                if view.notice.as_ref().is_some_and(|notice| notice.id == id) {
                    view.notice = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = &event.keystroke.modifiers;
        if key == "escape" {
            if self.model_menu_open {
                self.model_menu_open = false;
                cx.notify();
                cx.stop_propagation();
            } else if self.exit_confirm_open {
                self.exit_confirm_open = false;
                cx.notify();
                cx.stop_propagation();
            } else if self.page == Page::Settings {
                self.close_settings(cx);
                cx.stop_propagation();
            }
            return;
        }
        if self.exit_confirm_open {
            return;
        }
        if key == "n" && modifiers.secondary() && !modifiers.shift && !modifiers.alt {
            self.start_new_task(window, cx);
            cx.stop_propagation();
            return;
        }
        if !matches!(key, "enter" | "return") || self.page != Page::Queue {
            return;
        }
        if !self.prompt_input.focus_handle(cx).is_focused(window) {
            return;
        }
        // Enter that confirms an IME candidate must never submit the task.
        let composing = self.prompt_input.update(cx, |state, cx| {
            <InputState as EntityInputHandler>::marked_text_range(state, window, cx).is_some()
        });
        if composing {
            return;
        }
        let plain = !modifiers.modified();
        let shortcut = modifiers.secondary() && !modifiers.shift;
        if plain || shortcut {
            self.submit(window, cx);
            cx.stop_propagation();
        }
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "windows")]
        if let Some(action) = gpui_platform::take_tray_action() {
            match action {
                gpui_platform::TrayAction::Pause => {
                    let _ = self.send(RuntimeCommand::Pause);
                }
                gpui_platform::TrayAction::Resume => {
                    let _ = self.send(RuntimeCommand::Resume);
                }
                gpui_platform::TrayAction::OpenResult => self.open_thread(window, cx),
            }
            cx.notify();
        }
        #[cfg(not(target_os = "windows"))]
        let _ = window;
        // Countdowns change every second; elapsed-time labels per minute.
        let countdown = self.snapshot.sessions.iter().any(|session| {
            session.next_attempt_at.is_some()
                && matches!(session.phase, QueuePhase::Backoff | QueuePhase::Connecting)
        });
        if countdown || self.last_slow_refresh.elapsed() >= Duration::from_secs(20) {
            self.last_slow_refresh = Instant::now();
            cx.notify();
        }
    }

    fn update_snapshot(
        &mut self,
        snapshot: QueueSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((request_id, text)) = self.pending_clear.clone() {
            if snapshot.accepted_request_id >= request_id {
                self.pending_clear = None;
            } else if snapshot.processed_request_id >= request_id {
                // Rejected: put the text back unless the user typed again.
                self.pending_clear = None;
                if self.prompt_input.read(cx).value().is_empty() {
                    self.prompt_input
                        .update(cx, |state, cx| state.set_value(text, window, cx));
                }
                let reason = snapshot.status_message.trim();
                if !reason.is_empty() {
                    self.show_notice(reason.to_owned(), true, window, cx);
                }
            }
        }
        if let Some(request_id) = self.awaiting_placement {
            match snapshot.last_submission {
                Some(placement) if placement.request_id == request_id => {
                    self.awaiting_placement = None;
                    if let Some(session_id) = placement.session_id {
                        self.selection = Selection::Session(session_id);
                    } else if let Some(pending_id) = placement.pending_id {
                        self.selection = Selection::Pending(pending_id);
                    }
                }
                _ if snapshot.processed_request_id >= request_id
                    && snapshot.accepted_request_id < request_id =>
                {
                    self.awaiting_placement = None;
                }
                _ => {}
            }
        }

        let max_scroll = self.timeline_scroll.max_offset().height;
        let distance_from_bottom = max_scroll + self.timeline_scroll.offset().y;
        let should_follow = max_scroll <= px(1.) || distance_from_bottom <= px(32.);
        let content_changed = self.snapshot.session_id != snapshot.session_id
            || self.snapshot.conversation.len() != snapshot.conversation.len()
            || self.snapshot.reply_preview != snapshot.reply_preview
            || self.snapshot.phase != snapshot.phase;
        if self.snapshot.phase != snapshot.phase || self.snapshot.session_id != snapshot.session_id
        {
            self.reply_copied = false;
        }
        self.snapshot = snapshot;
        if self.snapshot.session_id == self.requested_session {
            self.requested_session = None;
        }
        self.reconcile_selection();
        if let Selection::Session(id) = self.selection {
            self.request_runtime_selection(id);
        }
        if should_follow && content_changed {
            self.timeline_scroll.scroll_to_bottom();
        }
        #[cfg(target_os = "windows")]
        self.update_tray();
        self.sync_placeholder(window, cx);
        cx.notify();
    }

    /// Keep the selection pointing at something that still exists, following a
    /// waiting task into the session it was started in.
    fn reconcile_selection(&mut self) {
        if self.initial_selection_pending && !self.snapshot.sessions.is_empty() {
            self.initial_selection_pending = false;
            if self.selection == Selection::New
                && let Some(session) = self
                    .snapshot
                    .sessions
                    .iter()
                    .filter(|session| session.has_history)
                    .max_by_key(|session| session.active)
            {
                self.selection = Selection::Session(session.id);
            }
        }
        match self.selection {
            Selection::New => {}
            Selection::Session(id) => {
                if self.summary(id).is_none() {
                    self.selection = Selection::New;
                }
            }
            Selection::Pending(pending_id) => {
                if !self
                    .snapshot
                    .pending_tasks
                    .iter()
                    .any(|task| task.id == pending_id)
                {
                    self.selection = self
                        .snapshot
                        .sessions
                        .iter()
                        .find(|session| session.origin_pending_id == Some(pending_id))
                        .map_or(Selection::New, |session| Selection::Session(session.id));
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn update_tray(&mut self) {
        let status = tray_status(&self.snapshot);
        if status != self.tray_status {
            gpui_platform::set_tray_state(&status, gpui_platform::TrayControl::Disabled, false);
            self.tray_status = status;
        }
    }
}

impl Render for QueueView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_window(window);
        let now = Utc::now();
        // Card border and header padding come off the space beside the sidebar.
        self.header_space = f32::from(window.viewport_size().width) - SIDEBAR_WIDTH - 4. - 2. - 32.;
        let card = self.render_content_card(palette, now, window, cx);
        let content = div()
            .id("longwatch-root")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(palette.shell))
            .text_color(rgb(palette.text))
            .when(cfg!(target_os = "windows"), |this| {
                this.child(self.render_titlebar(palette, window))
            })
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(self.render_sidebar(palette, now, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .h_full()
                            .when(!cfg!(target_os = "windows"), |this| this.pt(px(4.)))
                            .pr(px(4.))
                            .pb(px(4.))
                            .child(card),
                    ),
            )
            .when(self.exit_confirm_open, |this| {
                this.child(self.render_exit_dialog(palette, cx))
            })
            .capture_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                view.handle_key(event, window, cx);
            }));
        let config = TDesignConfig::new()
            .theme_mode(ThemeMode::System)
            .theme_overrides(ThemeOverrides::new().brand(rgb(palette.primary)));
        TDesignRoot::with_config(config).child(content)
    }
}

// ---------------------------------------------------------------------------
// Shell: title bar and queue sidebar
// ---------------------------------------------------------------------------

impl QueueView {
    fn render_titlebar(&self, palette: Palette, window: &Window) -> impl IntoElement {
        let maximized = window.is_maximized();
        div()
            .h(px(TITLEBAR_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .child(
                div()
                    .h_full()
                    .flex_1()
                    .min_w(px(0.))
                    .pl(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .window_control_area(WindowControlArea::Drag)
                    .child(
                        div()
                            .size(px(16.))
                            .rounded(px(4.))
                            .overflow_hidden()
                            .child(img(self.logo.clone()).size_full()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(palette.text_secondary))
                            .child("Longwatch"),
                    ),
            )
            .child(window_control(WindowControlArea::Min, palette, false))
            .child(window_control(WindowControlArea::Max, palette, maximized))
            .child(window_control(WindowControlArea::Close, palette, false))
    }

    fn render_sidebar(
        &self,
        palette: Palette,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let lists = QueueLists::from_snapshot(&self.snapshot);
        let new_selected = self.page == Page::Queue && self.selection == Selection::New;
        let new_task = div()
            .id("new-task")
            .h(px(34.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .rounded(px(10.))
            .border_1()
            .border_color(rgb(if new_selected {
                palette.primary_border
            } else {
                palette.border_tertiary
            }))
            .bg(rgb(if new_selected {
                palette.primary_bg
            } else {
                palette.base
            }))
            .shadow(palette.soft_shadow())
            .cursor_pointer()
            .hover(move |this| this.border_color(rgb(palette.border_secondary)))
            .child(
                Icon::new(IconName::Add)
                    .size(px(15.))
                    .color(rgb(if new_selected {
                        palette.primary_text
                    } else {
                        palette.text
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(if new_selected {
                        palette.primary_text
                    } else {
                        palette.text
                    }))
                    .child("新任务"),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(palette.text_quaternary))
                    .child("Ctrl N"),
            )
            .on_click(cx.listener(|view, _, window, cx| view.start_new_task(window, cx)));

        let mut list = div().flex().flex_col().gap(px(2.)).pb(px(8.));
        if lists.is_empty() {
            list = list.child(
                div()
                    .px(px(10.))
                    .pt(px(18.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(palette.text_tertiary))
                            .child("队列是空的"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .text_color(rgb(palette.text_quaternary))
                            .child("输入任务后按 Enter，Longwatch 会一直排队到 Codex 完成。"),
                    ),
            );
        }
        if !lists.running.is_empty() {
            list = list.child(section_heading(
                "进行中",
                Some(format!(
                    "{}/{}",
                    self.snapshot.running_session_count, self.config.max_concurrent_sessions
                )),
                None,
                palette,
            ));
            for session in &lists.running {
                list = list.child(self.session_row(session, palette, now, cx));
            }
        }
        if !lists.waiting.is_empty() {
            list = list.child(section_heading(
                "排队中",
                Some(lists.waiting.len().to_string()),
                None,
                palette,
            ));
            for (position, task) in &lists.waiting {
                list = list.child(self.pending_row(*position, task, palette, cx));
            }
        }
        if !lists.paused.is_empty() {
            list = list.child(section_heading("已暂停", None, None, palette));
            for session in &lists.paused {
                list = list.child(self.session_row(session, palette, now, cx));
            }
        }
        if !lists.finished.is_empty() {
            let clear = text_button("clear-finished", None, "清除", ButtonStyle::Ghost, palette)
                .h(px(20.))
                .px(px(6.))
                .text_size(px(11.))
                .on_click(cx.listener(|view, _, window, cx| view.clear_finished(window, cx)));
            list = list.child(section_heading(
                "已完成",
                None,
                Some(clear.into_any_element()),
                palette,
            ));
            for session in &lists.finished {
                list = list.child(self.session_row(session, palette, now, cx));
            }
        }

        let settings_selected = self.page == Page::Settings;
        let cli_tone = match &self.codex_cli {
            None => palette.text_quaternary,
            Some(cli) if cli.available() => palette.success,
            Some(_) => palette.error,
        };
        let footer = div()
            .flex_none()
            .pt(px(6.))
            .border_t_1()
            .border_color(rgb(palette.border_tertiary))
            .child(
                div()
                    .id("open-settings")
                    .h(px(30.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .rounded(px(8.))
                    .cursor_pointer()
                    .when(settings_selected, |this| this.bg(rgba(palette.active)))
                    .when(!settings_selected, |this| {
                        this.hover(move |style| style.bg(rgba(palette.hover)))
                    })
                    .child(
                        Icon::new(IconName::Setting)
                            .size(px(15.))
                            .color(rgb(palette.text_tertiary)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .text_color(rgb(if settings_selected {
                                palette.text
                            } else {
                                palette.text_secondary
                            }))
                            .child("设置"),
                    )
                    .child(div().size(px(6.)).rounded_full().bg(rgb(cli_tone)))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(palette.text_quaternary))
                            .child(
                                self.codex_cli
                                    .as_ref()
                                    .map_or_else(|| "检测中".to_owned(), cli_short_label),
                            ),
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.open_settings(cx))),
            );

        div()
            .w(px(SIDEBAR_WIDTH))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .px(px(8.))
            .pb(px(8.))
            .when(!cfg!(target_os = "windows"), |this| this.pt(px(8.)))
            .child(new_task)
            .child(
                div()
                    .id("queue-list")
                    .flex_1()
                    .min_h(px(0.))
                    .mt(px(6.))
                    .overflow_y_scroll()
                    .child(list),
            )
            .child(footer)
    }

    fn session_row(
        &self,
        session: &QueueSessionSummary,
        palette: Palette,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = session.id;
        let selected = self.page == Page::Queue && self.selection == Selection::Session(id);
        let meta_color = match session.phase {
            QueuePhase::FatalError => palette.error,
            _ => palette.text_tertiary,
        };
        task_row(
            ElementId::Name(format!("session-row-{id}").into()),
            selected,
            session_indicator(session, palette),
            session.title.clone(),
            session_meta(session, now),
            meta_color,
            palette,
        )
        .on_click(cx.listener(move |view, _, window, cx| {
            view.select(Selection::Session(id), window, cx);
        }))
    }

    fn pending_row(
        &self,
        position: usize,
        task: &PendingTaskSummary,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = task.id;
        let selected = self.page == Page::Queue && self.selection == Selection::Pending(id);
        let meta = if task.target_session_id.is_some() {
            format!("第 {position} 位 · 追加到原会话")
        } else {
            format!("第 {position} 位 · 等待空闲通道")
        };
        task_row(
            ElementId::Name(format!("pending-row-{id}").into()),
            selected,
            queued_ring(palette),
            task.title.clone(),
            meta,
            palette.text_tertiary,
            palette,
        )
        .on_click(cx.listener(move |view, _, window, cx| {
            view.select(Selection::Pending(id), window, cx);
        }))
    }
}

// ---------------------------------------------------------------------------
// Content card: new task, task detail, waiting task, settings
// ---------------------------------------------------------------------------

impl QueueView {
    fn render_content_card(
        &mut self,
        palette: Palette,
        now: DateTime<Utc>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = match self.page {
            Page::Settings => self.render_settings(palette, cx),
            Page::Queue => match self.selection {
                Selection::New => self.render_new_task(palette, window, cx),
                Selection::Session(id) => self.render_session(id, palette, now, window, cx),
                Selection::Pending(id) => self.render_pending(id, palette, cx),
            },
        };
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(CONTENT_RADIUS))
            .border_1()
            .border_color(rgb(palette.border_tertiary))
            .bg(rgb(palette.surface))
            .when(self.runtime_stopped, |this| {
                this.child(
                    div()
                        .flex_none()
                        .px(px(16.))
                        .py(px(8.))
                        .bg(rgb(palette.error_bg))
                        .text_size(px(12.))
                        .text_color(rgb(palette.error))
                        .child("后台任务意外停止，请重新启动 Longwatch。详细原因已写入日志目录。"),
                )
            })
            .child(div().flex_1().min_h(px(0.)).flex().flex_col().child(body))
            .when_some(self.notice.clone(), |this, notice| {
                this.child(
                    div()
                        .absolute()
                        .bottom(px(16.))
                        .right(px(16.))
                        .max_w(px(420.))
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(10.))
                        .border_1()
                        .border_color(rgb(if notice.error {
                            palette.error
                        } else {
                            palette.border_tertiary
                        }))
                        .bg(rgb(if notice.error {
                            palette.error_bg
                        } else {
                            palette.surface
                        }))
                        .shadow(palette.overlay_shadow())
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .text_color(rgb(if notice.error {
                            palette.error
                        } else {
                            palette.text
                        }))
                        .child(notice.text),
                )
            })
            .into_any_element()
    }

    fn render_new_task(
        &mut self,
        palette: Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let running = self.snapshot.running_session_count;
        let max = usize::from(self.config.max_concurrent_sessions);
        let waiting = self.snapshot.pending_tasks.len();
        let capacity = if running < max && waiting == 0 {
            if running == 0 {
                "通道空闲，新任务会立即开始".to_owned()
            } else {
                format!("{running}/{max} 个通道在运行，新任务会立即开始")
            }
        } else {
            format!(
                "{running}/{max} 个通道在运行，新任务将排在第 {} 位",
                waiting + 1
            )
        };
        div()
            .id("new-task-page")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px(px(32.))
            .py(px(32.))
            .child(
                div()
                    .w(px(self.chat_width()))
                    .flex()
                    .flex_col()
                    .gap(px(20.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .size(px(36.))
                                    .rounded(px(9.))
                                    .overflow_hidden()
                                    .child(img(self.logo.clone()).size_full()),
                            )
                            .child(
                                div()
                                    .text_size(px(20.))
                                    .line_height(px(32.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(palette.text))
                                    .child("今天要让 Codex 完成什么？"),
                            )
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .line_height(px(20.))
                                    .text_color(rgb(palette.text_tertiary))
                                    .child("交给 Longwatch 后，它会一直排队：遇到高峰自动重试，直到拿到结果。"),
                            ),
                    )
                    .child(self.render_composer(ComposerTarget::NewTask, true, palette, window, cx))
                    .child(
                        div()
                            .flex()
                            .justify_center()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(12.))
                            .text_color(rgb(palette.text_tertiary))
                            .child(div().size(px(6.)).rounded_full().bg(rgb(
                                if running < max {
                                    palette.success
                                } else {
                                    palette.warning
                                },
                            )))
                            .child(capacity),
                    ),
            )
            .into_any_element()
    }

    fn render_session(
        &mut self,
        id: u64,
        palette: Palette,
        now: DateTime<Utc>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(summary) = self.summary(id).cloned() else {
            return empty_state("这个任务已经移除", palette);
        };
        let header = self.render_session_header(&summary, palette, cx);
        let Some(detail) = self.detail(id).cloned() else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(header)
                .child(empty_state("正在载入任务…", palette))
                .into_any_element();
        };

        let mut column = div()
            .w(px(self.chat_width()))
            .flex()
            .flex_col()
            .gap(px(18.));
        let entries = timeline_entries(&detail);
        let entry_count = entries.len();
        for (index, message) in entries.into_iter().enumerate() {
            column = column.child(render_message(id, index, &message, palette, window, cx));
        }
        if let Some(card) = self.render_status_card(&summary, &detail, palette, now, cx) {
            column = column.child(card);
        } else if let Some(reply) = final_reply(&detail) {
            column = column.child(self.render_reply_actions(&summary, &detail, reply, palette, cx));
        }
        if entry_count == 0 && self.render_status_placeholder(&detail) {
            column = column.child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(palette.text_tertiary))
                    .child("等待任务开始…"),
            );
        }

        let target = self.composer_target();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id(ElementId::Name(format!("timeline-{id}").into()))
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.timeline_scroll)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .px(px(28.))
                            .pt(px(22.))
                            .pb(px(16.))
                            .child(column),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.))
                    .px(px(28.))
                    .pb(px(16.))
                    .child(self.render_staged_message(id, &detail, palette, cx))
                    .child(
                        div()
                            .w(px(self.chat_width()))
                            .child(self.render_composer(target, false, palette, window, cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_status_placeholder(&self, detail: &QueueSnapshot) -> bool {
        detail.phase == QueuePhase::Idle && detail.attempt_count == 0
    }

    fn render_session_header(
        &self,
        summary: &QueueSessionSummary,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut actions = div().flex_none().flex().items_center().gap(px(4.));
        if summary.active {
            actions = actions
                .child(
                    text_button(
                        "pause-task",
                        Some(IconName::Pause),
                        "暂停",
                        ButtonStyle::Ghost,
                        palette,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.run_selected(RuntimeCommand::Pause, cx);
                    })),
                )
                .child(
                    text_button(
                        "stop-task",
                        Some(IconName::Stop),
                        "停止",
                        ButtonStyle::Ghost,
                        palette,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.run_selected(RuntimeCommand::Stop, cx);
                    })),
                );
        } else {
            match summary.phase {
                QueuePhase::Paused => {
                    actions = actions
                        .child(
                            text_button(
                                "resume-task",
                                Some(IconName::Play),
                                "继续",
                                ButtonStyle::Primary,
                                palette,
                            )
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.run_selected(RuntimeCommand::Resume, cx);
                            })),
                        )
                        .child(
                            text_button(
                                "end-task",
                                Some(IconName::Stop),
                                "停止",
                                ButtonStyle::Ghost,
                                palette,
                            )
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.run_selected(RuntimeCommand::Stop, cx);
                            })),
                        );
                }
                QueuePhase::FatalError => {
                    actions = actions.child(
                        text_button(
                            "retry-task",
                            Some(IconName::Refresh),
                            "重试",
                            ButtonStyle::Primary,
                            palette,
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.run_selected(RuntimeCommand::Resume, cx);
                        })),
                    );
                }
                _ => {}
            }
            if summary.active_thread_id.is_some() {
                actions = actions.child(
                    text_button(
                        "open-in-codex",
                        Some(IconName::Jump),
                        "在 Codex 中打开",
                        ButtonStyle::Ghost,
                        palette,
                    )
                    .on_click(cx.listener(|view, _, window, cx| view.open_thread(window, cx))),
                );
            }
            actions = actions.child(
                text_button(
                    "remove-task",
                    Some(IconName::Delete),
                    "移除",
                    ButtonStyle::DangerGhost,
                    palette,
                )
                .on_click(cx.listener(|view, _, window, cx| view.dismiss_selected(window, cx))),
            );
        }
        content_header(
            summary.title.clone(),
            self.header_space - 470.,
            Some(status_pill(
                session_state_label(summary),
                session_tone(summary, palette),
                palette,
            )),
            actions.into_any_element(),
            palette,
        )
    }

    fn render_status_card(
        &self,
        summary: &QueueSessionSummary,
        detail: &QueueSnapshot,
        palette: Palette,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // While waiting to retry, whatever the preview holds (an error or a
        // "high demand" reply) is the reason for the wait.
        let notice = (detail.reply_preview_is_notice
            || !summary.active
            || detail.phase == QueuePhase::Backoff)
            .then(|| detail.reply_preview.trim())
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        let attempts = detail.attempt_count.max(summary.attempt_count);
        let mut facts = Vec::new();
        if detail.phase == QueuePhase::Backoff {
            if attempts > 0 {
                facts.push(format!("已尝试 {attempts} 次"));
            }
        } else if attempts > 1 {
            facts.push(format!("第 {attempts} 次尝试"));
        }
        if let Some(waited) = detail
            .first_attempt_at
            .and_then(|since| elapsed_label(since, now))
        {
            facts.push(format!("已排队 {waited}"));
        }
        let (tone, headline, detail_text, action): (
            u32,
            String,
            Option<String>,
            Option<AnyElement>,
        ) = match detail.phase {
            QueuePhase::Backoff => {
                let eta = detail
                    .next_attempt_at
                    .map_or_else(|| "稍后".to_owned(), |next| compact_eta(next, now));
                (
                    palette.warning,
                    format!("Codex 暂时繁忙，{eta}重新排队"),
                    notice.clone(),
                    Some(
                        text_button(
                            "retry-now",
                            Some(IconName::Refresh),
                            "立即重试",
                            ButtonStyle::Outline,
                            palette,
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.run_selected(RuntimeCommand::RetryNow, cx);
                        }))
                        .into_any_element(),
                    ),
                )
            }
            QueuePhase::Connecting => {
                let reconnecting =
                    detail.next_attempt_at.is_some() && is_retry_status(&detail.status_message);
                let headline = match detail.next_attempt_at {
                    Some(next) if reconnecting => {
                        format!("连接中断，{}重新连接", compact_eta(next, now))
                    }
                    _ => "正在连接 Codex…".to_owned(),
                };
                (
                    if reconnecting {
                        palette.warning
                    } else {
                        palette.primary
                    },
                    headline,
                    reconnecting.then(|| detail.status_message.clone()),
                    None,
                )
            }
            QueuePhase::Sending => (palette.primary, "正在发送任务…".to_owned(), None, None),
            QueuePhase::Waiting => {
                if is_retry_status(&detail.status_message) {
                    (
                        palette.warning,
                        "Codex 正在自动重试".to_owned(),
                        Some(strip_status_prefix(&detail.status_message)),
                        None,
                    )
                } else if detail.reply_preview.trim().is_empty() {
                    (palette.primary, "Codex 正在处理…".to_owned(), None, None)
                } else {
                    // Streaming text is already on screen; keep a quiet
                    // progress line under it.
                    (palette.primary, "Codex 正在输出…".to_owned(), None, None)
                }
            }
            QueuePhase::Paused => (
                palette.text_tertiary,
                "已暂停".to_owned(),
                Some(detail.status_message.clone()).filter(|text| !text.trim().is_empty()),
                None,
            ),
            QueuePhase::FatalError => (
                palette.error,
                "需要处理".to_owned(),
                Some(detail.status_message.clone()),
                None,
            ),
            QueuePhase::Success | QueuePhase::Idle => return None,
        };
        let warning = detail.runtime_warning.clone();
        Some(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(6.))
                .px(px(14.))
                .py(px(12.))
                .rounded(px(12.))
                .border_1()
                .border_color(rgb(palette.border_tertiary))
                .bg(rgb(palette.elevated))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().size(px(7.)).flex_none().rounded_full().bg(rgb(tone)))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_size(px(13.))
                                .line_height(px(20.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(palette.text))
                                .child(headline),
                        )
                        .when_some(action, |this, action| this.child(action)),
                )
                .when(!facts.is_empty(), |this| {
                    this.child(
                        div()
                            .pl(px(15.))
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .text_color(rgb(palette.text_tertiary))
                            .child(facts.join(" · ")),
                    )
                })
                .when_some(detail_text, |this, text| {
                    this.child(
                        div()
                            .pl(px(15.))
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .text_color(rgb(palette.text_secondary))
                            .line_clamp(4)
                            .child(text),
                    )
                })
                .when_some(warning, |this, warning| {
                    this.child(
                        div()
                            .pl(px(15.))
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .text_color(rgb(palette.error))
                            .child(warning),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_reply_actions(
        &self,
        summary: &QueueSessionSummary,
        detail: &QueueSnapshot,
        reply: String,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copied = self.reply_copied;
        let mut meta = Vec::new();
        if let Some(completed) = detail.reply_completed_at {
            meta.push(clock_label(completed));
        }
        if summary.phase == QueuePhase::Idle {
            meta.push("已停止".to_owned());
        }
        if detail.attempt_count > 1 {
            meta.push(format!("经过 {} 次尝试", detail.attempt_count));
        }
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(
                text_button(
                    "copy-reply",
                    Some(if copied {
                        IconName::Check
                    } else {
                        IconName::Copy
                    }),
                    if copied { "已复制" } else { "复制" },
                    ButtonStyle::Ghost,
                    palette,
                )
                .h(px(24.))
                .px(px(6.))
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.copy_reply(reply.clone(), window, cx);
                })),
            )
            .when(!meta.is_empty(), |this| {
                this.child(
                    div()
                        .pl(px(6.))
                        .text_size(px(12.))
                        .text_color(rgb(palette.text_quaternary))
                        .child(meta.join(" · ")),
                )
            })
            .into_any_element()
    }

    fn render_staged_message(
        &self,
        session_id: u64,
        detail: &QueueSnapshot,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (text, waiting_for_lane, pending_id) = if let Some(message) = &detail.queued_message {
            (message.prompt.clone(), false, None)
        } else if let Some(task) = self.pending_follow_up_for(session_id) {
            (task.prompt.clone(), true, Some(task.id))
        } else {
            return div().into_any_element();
        };
        let label = if waiting_for_lane {
            "追加消息正在等待空闲通道"
        } else {
            "当前回合结束后发送"
        };
        div()
            .w(px(self.chat_width()))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .py(px(7.))
            .rounded(px(10.))
            .bg(rgb(palette.primary_bg))
            .border_1()
            .border_color(rgb(palette.primary_border))
            .child(
                Icon::new(IconName::Time)
                    .size(px(13.))
                    .color(rgb(palette.primary_text)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(12.))
                    .text_color(rgb(palette.primary_text))
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(12.))
                    .text_color(rgb(palette.text_secondary))
                    .child(ellipsize(&first_line(&text), 360., 12.)),
            )
            .child(
                text_button(
                    "withdraw-message",
                    None,
                    "撤回",
                    ButtonStyle::Ghost,
                    palette,
                )
                .h(px(22.))
                .px(px(6.))
                .on_click(cx.listener(
                    move |view, _, window, cx| match pending_id {
                        Some(pending_id) => view.cancel_pending(pending_id, window, cx),
                        None => view.run_selected(RuntimeCommand::ClearQueuedMessage, cx),
                    },
                )),
            )
            .into_any_element()
    }

    fn render_pending(&self, id: u64, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some((index, task)) = self
            .snapshot
            .pending_tasks
            .iter()
            .enumerate()
            .find(|(_, task)| task.id == id)
        else {
            return empty_state("任务已开始", palette);
        };
        let position = index + 1;
        let running = self.snapshot.running_session_count;
        let max = self.config.max_concurrent_sessions;
        let header = content_header(
            task.title.clone(),
            self.header_space - 220.,
            Some(status_pill("排队中", palette.text_tertiary, palette)),
            text_button(
                "cancel-pending",
                Some(IconName::Close),
                "取消排队",
                ButtonStyle::DangerGhost,
                palette,
            )
            .on_click(cx.listener(move |view, _, window, cx| view.cancel_pending(id, window, cx)))
            .into_any_element(),
            palette,
        );
        let headline = if position == 1 {
            "排在第 1 位，有通道空闲时立即开始".to_owned()
        } else {
            format!("排在第 {position} 位，前面还有 {} 个任务", position - 1)
        };
        let mut facts = vec![format!("{running}/{max} 个通道正在运行")];
        if let Some(directory) = &task.working_directory {
            facts.push(format!("工作目录：{}", folder_name(directory)));
        }
        let column = div()
            .w(px(self.chat_width()))
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(user_bubble(&task.prompt, palette))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .px(px(14.))
                    .py(px(12.))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(rgb(palette.border_tertiary))
                    .bg(rgb(palette.elevated))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .w(px(14.))
                                    .flex()
                                    .justify_center()
                                    .child(queued_ring(palette)),
                            )
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(palette.text))
                                    .child(headline),
                            ),
                    )
                    .child(
                        div()
                            .pl(px(22.))
                            .text_size(px(12.))
                            .text_color(rgb(palette.text_tertiary))
                            .child(facts.join(" · ")),
                    )
                    .child(
                        div().pl(px(22.)).pt(px(2.)).flex().child(
                            text_button(
                                "raise-concurrency",
                                None,
                                "调整同时运行的数量",
                                ButtonStyle::Ghost,
                                palette,
                            )
                            .h(px(22.))
                            .px(px(6.))
                            .text_color(rgb(palette.primary_text))
                            .on_click(cx.listener(|view, _, _, cx| view.open_settings(cx))),
                        ),
                    ),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id(ElementId::Name(format!("pending-{id}").into()))
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .px(px(28.))
                            .pt(px(22.))
                            .pb(px(16.))
                            .child(column),
                    ),
            )
            .into_any_element()
    }

    fn render_composer(
        &self,
        target: ComposerTarget,
        large: bool,
        palette: Palette,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = self.prompt_input.focus_handle(cx).is_focused(window);
        let blocked = matches!(target, ComposerTarget::Blocked(_));
        let has_text = !self.prompt_input.read(cx).value().trim().is_empty();
        let folder = match target {
            ComposerTarget::NewTask => self.config.default_working_directory.clone(),
            ComposerTarget::FollowUp { .. } => self.snapshot.session_working_directory.clone(),
            ComposerTarget::Blocked(_) => None,
        }
        .filter(|path| !path.as_os_str().is_empty());
        let folder_editable = target == ComposerTarget::NewTask;
        let folder_label = folder
            .as_deref()
            .map_or_else(|| "默认目录".to_owned(), folder_name);
        let mut folder_chip = div()
            .id("folder-chip")
            .h(px(28.))
            .max_w(px(260.))
            .flex()
            .items_center()
            .gap(px(5.))
            .px(px(8.))
            .rounded(px(8.))
            .child(
                Icon::new(IconName::FolderOpen)
                    .size(px(14.))
                    .color(rgb(palette.text_tertiary)),
            )
            .child(
                div()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(12.))
                    .text_color(rgb(if folder.is_some() {
                        palette.text_secondary
                    } else {
                        palette.text_tertiary
                    }))
                    .child(ellipsize(&folder_label, 200., 12.)),
            );
        if folder_editable {
            folder_chip = folder_chip
                .cursor_pointer()
                .hover(move |style| style.bg(rgb(palette.fill_secondary)))
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size(px(12.))
                        .color(rgb(palette.text_quaternary)),
                )
                .on_click(cx.listener(|view, _, window, cx| view.pick_folder(window, cx)));
        }
        let clear_folder = (folder_editable && folder.is_some()).then(|| {
            div()
                .id("clear-folder")
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(move |style| style.bg(rgb(palette.fill_secondary)))
                .child(
                    Icon::new(IconName::Close)
                        .size(px(12.))
                        .color(rgb(palette.text_tertiary)),
                )
                .on_click(cx.listener(|view, _, _, cx| view.clear_folder(cx)))
        });
        // Follow-ups keep their conversation's model, like its folder.
        let (model, effort) = if folder_editable {
            (
                self.config.default_model.as_deref(),
                self.config.default_reasoning_effort.as_deref(),
            )
        } else {
            (
                self.snapshot.session_model.as_deref(),
                self.snapshot.session_reasoning_effort.as_deref(),
            )
        };
        let chip_bounds = Rc::clone(&self.model_chip_bounds);
        let mut model_chip = div()
            .id("model-chip")
            .relative()
            .h(px(28.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(6.))
            .rounded_full()
            .child(model_orb(palette))
            .child(
                div()
                    .whitespace_nowrap()
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(palette.text_secondary))
                    .child(ellipsize(&self.model_label(model, effort), 170., 12.)),
            )
            .child(
                canvas(
                    move |bounds, _, _| chip_bounds.set(Some(bounds)),
                    |_, (), _, _| {},
                )
                .absolute()
                .size_full(),
            );
        if folder_editable {
            model_chip = model_chip
                .cursor_pointer()
                .when(self.model_menu_open, |this| {
                    this.bg(rgb(palette.fill_secondary))
                })
                .hover(move |style| style.bg(rgb(palette.fill_secondary)))
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size(px(12.))
                        .color(rgb(palette.text_quaternary)),
                )
                .on_click(cx.listener(|view, _, _, cx| view.toggle_model_menu(cx)));
        }
        let model_menu = (folder_editable && self.model_menu_open)
            .then(|| self.model_chip_bounds.get())
            .flatten()
            .map(|chip| self.render_model_menu(chip, palette, cx));
        let hint = if self.prompt_error {
            Some(("请先输入任务内容", palette.error))
        } else if large && self.chat_width() >= 680. {
            Some(("Enter 发送 · Shift+Enter 换行", palette.text_quaternary))
        } else {
            None
        };
        let can_send = !blocked && has_text;
        let send_button = div()
            .id("send-task")
            .size(px(32.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(10.))
            .bg(rgb(palette.primary))
            .when(!can_send, |this| this.opacity(0.38))
            .when(can_send, |this| {
                this.cursor_pointer()
                    .hover(move |style| style.bg(rgb(palette.primary_hover)))
            })
            .child(
                Icon::new(IconName::ArrowUp)
                    .size(px(16.))
                    .color(rgb(palette.on_primary)),
            )
            .on_click(cx.listener(|view, _, window, cx| view.submit(window, cx)));
        div()
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(COMPOSER_RADIUS))
            .border_1()
            .border_color(rgb(if focused && !blocked {
                palette.border_secondary
            } else {
                palette.border_tertiary
            }))
            .bg(rgb(if blocked {
                palette.elevated
            } else {
                palette.base
            }))
            .shadow(palette.soft_shadow())
            .child(
                div()
                    .h(px(if large { 120. } else { 76. }))
                    .px(px(8.))
                    .pt(px(6.))
                    .text_size(px(14.))
                    .child(
                        Input::new(&self.prompt_input)
                            .appearance(false)
                            .bordered(false)
                            .focus_bordered(false)
                            .disabled(blocked)
                            .h_full(),
                    ),
            )
            .child(
                div()
                    .min_h(px(44.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .px(px(8.))
                    .pt(px(6.))
                    .pb(px(8.))
                    .border_t_1()
                    .border_dashed()
                    .border_color(rgb(palette.border_tertiary))
                    .child(folder_chip)
                    .when_some(clear_folder, |this, button| this.child(button))
                    .child(div().flex_1())
                    .when_some(hint, |this, (hint, color)| {
                        this.child(
                            div()
                                .mr(px(8.))
                                .text_size(px(12.))
                                .text_color(rgb(color))
                                .child(hint),
                        )
                    })
                    .child(model_chip)
                    .when_some(model_menu, |this, menu| this.child(menu))
                    .child(send_button),
            )
            .into_any_element()
    }

    fn render_settings(&mut self, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        let concurrency = div()
            .h(px(30.))
            .flex()
            .items_center()
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(palette.border_tertiary))
            .bg(rgb(palette.base))
            .child(
                stepper_button(
                    "concurrency-down",
                    IconName::Remove,
                    self.config.max_concurrent_sessions > MIN_CONCURRENT_SESSIONS,
                    palette,
                )
                .on_click(cx.listener(|view, _, _, cx| view.adjust_concurrency(-1, cx))),
            )
            .child(
                div()
                    .w(px(32.))
                    .text_center()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(self.config.max_concurrent_sessions.to_string()),
            )
            .child(
                stepper_button(
                    "concurrency-up",
                    IconName::Add,
                    self.config.max_concurrent_sessions < MAX_CONCURRENT_SESSIONS,
                    palette,
                )
                .on_click(cx.listener(|view, _, _, cx| view.adjust_concurrency(1, cx))),
            );
        let queue_group = settings_group(
            vec![
                settings_row(
                    "同时运行",
                    "最多同时进行的任务数，其余任务按顺序排队等待",
                    concurrency.into_any_element(),
                    palette,
                ),
                settings_block(
                    "繁忙提示短语",
                    "Codex 的回复以这些内容开头时视为高峰繁忙并重新排队，用 | 分隔",
                    text_field(&self.phrases_input, palette),
                    palette,
                ),
            ],
            palette,
        );

        let auto_send = self.config.auto_send_default_prompt;
        let startup_group = settings_group(
            vec![
                settings_row(
                    "启动时自动发送",
                    "打开 Longwatch 后把下面的消息作为新任务加入队列；关闭时它只用来预填输入框",
                    switch("auto-send", auto_send, palette)
                        .on_click(cx.listener(|view, _, _, cx| view.toggle_auto_send(cx)))
                        .into_any_element(),
                    palette,
                ),
                settings_block(
                    "默认消息",
                    "留空则每次启动显示空白输入框",
                    text_field(&self.default_prompt_input, palette),
                    palette,
                ),
            ],
            palette,
        );

        let identity = self.config.client_identity;
        let mut segmented = div()
            .flex()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(10.))
            .bg(rgb(palette.fill_secondary));
        for option in ClientIdentity::ALL {
            let selected = option == identity;
            segmented = segmented.child(
                div()
                    .id(ElementId::Name(format!("identity-{option:?}").into()))
                    .h(px(26.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .rounded(px(8.))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(selected, |this| {
                        this.bg(rgb(palette.base))
                            .text_color(rgb(palette.text))
                            .font_weight(FontWeight::MEDIUM)
                            .shadow(palette.soft_shadow())
                    })
                    .when(!selected, |this| {
                        this.text_color(rgb(palette.text_secondary))
                            .hover(move |style| style.text_color(rgb(palette.text)))
                    })
                    .child(option.label())
                    .on_click(cx.listener(move |view, _, _, cx| view.set_identity(option, cx))),
            );
        }
        let (cli_description, cli_pill) = match &self.codex_cli {
            None => (
                "正在检测 Codex CLI…".to_owned(),
                status_pill("检测中", palette.text_quaternary, palette),
            ),
            Some(cli) if cli.available() => (
                format!("{} · {}", cli.version_label(), cli.path_label()),
                status_pill("可用", palette.success, palette),
            ),
            Some(cli) => (
                format!("未找到可用的 Codex CLI：{}", cli.path_label()),
                status_pill("需要检查", palette.error, palette),
            ),
        };
        let connection_group = settings_group(
            vec![
                settings_block(
                    "请求身份",
                    identity_description(identity),
                    div().flex().child(segmented).into_any_element(),
                    palette,
                ),
                settings_row("Codex 程序", cli_description, cli_pill, palette),
            ],
            palette,
        );

        let alert_group = settings_group(
            vec![settings_row(
                "完成提示音",
                "任务完成时除系统通知外再播放提示音",
                switch("audio-alert", self.config.audio_alert_enabled, palette)
                    .on_click(cx.listener(|view, _, _, cx| view.toggle_audio(cx)))
                    .into_any_element(),
                palette,
            )],
            palette,
        );

        let diagnostics_group = settings_group(
            vec![settings_row(
                "诊断信息",
                transport_status_text(&self.snapshot),
                div()
                    .flex()
                    .gap(px(4.))
                    .child(
                        text_button(
                            "copy-diagnostics",
                            Some(if self.diagnostics_copied {
                                IconName::Check
                            } else {
                                IconName::Copy
                            }),
                            if self.diagnostics_copied {
                                "已复制"
                            } else {
                                "复制诊断"
                            },
                            ButtonStyle::Outline,
                            palette,
                        )
                        .on_click(
                            cx.listener(|view, _, window, cx| view.copy_diagnostics(window, cx)),
                        ),
                    )
                    .child(
                        text_button(
                            "open-logs",
                            Some(IconName::FolderOpen),
                            "打开日志",
                            ButtonStyle::Outline,
                            palette,
                        )
                        .on_click(cx.listener(|view, _, window, cx| view.open_logs(window, cx))),
                    )
                    .into_any_element(),
                palette,
            )],
            palette,
        );

        let back = text_button(
            "close-settings",
            Some(IconName::ChevronLeft),
            "返回队列",
            ButtonStyle::Ghost,
            palette,
        )
        .on_click(cx.listener(|view, _, _, cx| view.close_settings(cx)));
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(content_header(
                "设置".into(),
                self.header_space - 140.,
                None,
                back.into_any_element(),
                palette,
            ))
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .px(px(28.))
                            .pb(px(40.))
                            .child(
                                div()
                                    .w_full()
                                    .max_w(px(680.))
                                    .flex()
                                    .flex_col()
                                    .child(settings_section("队列", queue_group, palette))
                                    .child(settings_section("启动", startup_group, palette))
                                    .child(settings_section("连接", connection_group, palette))
                                    .child(settings_section("提醒", alert_group, palette))
                                    .child(settings_section("诊断", diagnostics_group, palette)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_exit_dialog(&self, palette: Palette, cx: &mut Context<Self>) -> AnyElement {
        let running = self.snapshot.running_session_count;
        let waiting = self.snapshot.pending_tasks.len();
        let work = match (running, waiting) {
            (0, waiting) => format!("还有 {waiting} 个任务在排队"),
            (running, 0) => format!("还有 {running} 个任务正在进行"),
            (running, waiting) => format!("还有 {running} 个任务正在进行、{waiting} 个在排队"),
        };
        div()
            .id("exit-dialog")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .bg(rgba(palette.mask))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(400.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(24.))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(rgb(palette.border_tertiary))
                    .bg(rgb(palette.surface))
                    .shadow(palette.overlay_shadow())
                    .child(
                        div()
                            .text_size(px(16.))
                            .line_height(px(24.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("退出 Longwatch？"),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .line_height(px(20.))
                            .text_color(rgb(palette.text_secondary))
                            .child(format!("{work}。退出后排队会停止，这些任务不会自动恢复。")),
                    )
                    .child(
                        div()
                            .pt(px(8.))
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(
                                text_button(
                                    "cancel-exit",
                                    None,
                                    "取消",
                                    ButtonStyle::Outline,
                                    palette,
                                )
                                .on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.exit_confirm_open = false;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                text_button(
                                    "confirm-exit",
                                    None,
                                    "退出",
                                    ButtonStyle::Danger,
                                    palette,
                                )
                                .on_click(cx.listener(
                                    |view, _, window, cx| {
                                        view.confirm_exit(window, cx);
                                    },
                                )),
                            ),
                    ),
            )
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Building blocks
// ---------------------------------------------------------------------------

/// Sidebar groups, in the order they are shown.
struct QueueLists<'a> {
    running: Vec<&'a QueueSessionSummary>,
    waiting: Vec<(usize, &'a PendingTaskSummary)>,
    paused: Vec<&'a QueueSessionSummary>,
    finished: Vec<&'a QueueSessionSummary>,
}

impl<'a> QueueLists<'a> {
    fn from_snapshot(snapshot: &'a QueueSnapshot) -> Self {
        let visible = snapshot
            .sessions
            .iter()
            .filter(|session| session.has_history);
        let running = visible.clone().filter(|session| session.active).collect();
        let paused = visible
            .clone()
            .filter(|session| {
                !session.active
                    && matches!(session.phase, QueuePhase::Paused | QueuePhase::FatalError)
            })
            .collect();
        let mut finished = visible
            .filter(|session| session_is_finished(session))
            .collect::<Vec<_>>();
        finished.sort_by(|left, right| {
            right
                .reply_completed_at
                .cmp(&left.reply_completed_at)
                .then(right.id.cmp(&left.id))
        });
        Self {
            running,
            waiting: snapshot
                .pending_tasks
                .iter()
                .enumerate()
                .map(|(index, task)| (index + 1, task))
                .collect(),
            paused,
            finished,
        }
    }

    fn is_empty(&self) -> bool {
        self.running.is_empty()
            && self.waiting.is_empty()
            && self.paused.is_empty()
            && self.finished.is_empty()
    }
}

fn session_is_finished(session: &QueueSessionSummary) -> bool {
    session.has_history
        && !session.active
        && matches!(session.phase, QueuePhase::Success | QueuePhase::Idle)
}

fn window_control(area: WindowControlArea, palette: Palette, maximized: bool) -> impl IntoElement {
    let close = area == WindowControlArea::Close;
    let glyph: AnyElement = match area {
        WindowControlArea::Min => div()
            .w(px(10.))
            .h(px(1.))
            .bg(rgb(palette.text_secondary))
            .into_any_element(),
        WindowControlArea::Max if maximized => div()
            .size(px(10.))
            .relative()
            .child(
                div()
                    .absolute()
                    .top(px(0.))
                    .left(px(2.))
                    .size(px(8.))
                    .border_1()
                    .border_color(rgb(palette.text_secondary)),
            )
            .child(
                div()
                    .absolute()
                    .top(px(2.))
                    .left(px(0.))
                    .size(px(8.))
                    .border_1()
                    .border_color(rgb(palette.text_secondary))
                    .bg(rgb(palette.shell)),
            )
            .into_any_element(),
        WindowControlArea::Max => div()
            .size(px(10.))
            .border_1()
            .border_color(rgb(palette.text_secondary))
            .into_any_element(),
        _ => Icon::new(IconName::Close)
            .size(px(14.))
            .color(rgb(palette.text_secondary))
            .into_any_element(),
    };
    div()
        .w(px(46.))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .window_control_area(area)
        .hover(move |style| {
            if close {
                style.bg(rgb(0xc42b1c))
            } else {
                style.bg(rgba(palette.hover))
            }
        })
        .child(glyph)
}

fn section_heading(
    label: &'static str,
    count: Option<String>,
    action: Option<AnyElement>,
    palette: Palette,
) -> impl IntoElement {
    div()
        .h(px(28.))
        .mt(px(8.))
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(8.))
        .child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(palette.text_tertiary))
                .child(label),
        )
        .when_some(count, |this, count| {
            this.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(palette.text_quaternary))
                    .child(count),
            )
        })
        .child(div().flex_1())
        .when_some(action, |this, action| this.child(action))
}

fn task_row(
    id: ElementId,
    selected: bool,
    indicator: AnyElement,
    title: String,
    meta: String,
    meta_color: u32,
    palette: Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_start()
        .gap(px(9.))
        .px(px(8.))
        .py(px(6.))
        .rounded(px(8.))
        .cursor_pointer()
        .when(selected, |this| this.bg(rgba(palette.active)))
        .when(!selected, |this| {
            this.hover(move |style| style.bg(rgba(palette.hover)))
        })
        .child(
            div()
                .w(px(14.))
                .h(px(18.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(indicator),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(
                    div()
                        .w(px(ROW_TEXT_WIDTH))
                        .text_size(px(13.))
                        .line_height(px(18.))
                        .text_color(rgb(if selected {
                            palette.text
                        } else {
                            palette.text_secondary
                        }))
                        .truncate()
                        .child(title),
                )
                .child(
                    div()
                        .w(px(ROW_TEXT_WIDTH))
                        .text_size(px(11.))
                        .line_height(px(15.))
                        .text_color(rgb(meta_color))
                        .truncate()
                        .child(meta),
                ),
        )
}

fn session_indicator(session: &QueueSessionSummary, palette: Palette) -> AnyElement {
    let dot = |color: u32| {
        div()
            .size(px(8.))
            .rounded_full()
            .bg(rgb(color))
            .into_any_element()
    };
    let icon = |name: IconName, color: u32| {
        Icon::new(name)
            .size(px(13.))
            .color(rgb(color))
            .into_any_element()
    };
    match session.phase {
        QueuePhase::Backoff => dot(palette.warning),
        _ if session.active => dot(palette.primary),
        QueuePhase::Success => icon(IconName::Check, palette.success),
        QueuePhase::Paused => icon(IconName::Pause, palette.text_tertiary),
        QueuePhase::FatalError => icon(IconName::ErrorCircle, palette.error),
        _ => dot(palette.text_quaternary),
    }
}

/// Hollow ring: queued, not started yet.
fn queued_ring(palette: Palette) -> AnyElement {
    div()
        .size(px(9.))
        .rounded_full()
        .border_2()
        .border_color(rgb(palette.text_quaternary))
        .into_any_element()
}

fn session_tone(session: &QueueSessionSummary, palette: Palette) -> u32 {
    match session.phase {
        QueuePhase::Backoff => palette.warning,
        _ if session.active => palette.primary,
        QueuePhase::Success => palette.success,
        QueuePhase::FatalError => palette.error,
        _ => palette.text_tertiary,
    }
}

const MODEL_MENU_WIDTH: f32 = 280.;

/// Chinese label for a Codex reasoning effort; unknown values show as-is.
fn effort_label(effort: &str) -> &str {
    match effort {
        "none" => "关闭",
        "minimal" => "最低",
        "low" => "低",
        "medium" => "中",
        "high" => "高",
        "xhigh" => "超高",
        "max" => "最大",
        "ultra" => "极限",
        other => other,
    }
}

/// The kit's `q-model-orb`: a small tinted disc in front of the model name.
fn model_orb(palette: Palette) -> Div {
    div()
        .size(px(16.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgb(palette.primary_bg))
        .child(
            Icon::new(IconName::Ai)
                .size(px(12.))
                .color(rgb(palette.primary_text)),
        )
}

fn menu_section(title: &'static str, palette: Palette) -> Div {
    div()
        .flex_none()
        .px(px(8.))
        .pt(px(6.))
        .pb(px(4.))
        .text_size(px(11.))
        .line_height(px(14.))
        .text_color(rgb(palette.text_tertiary))
        .child(title)
}

fn menu_note(text: &'static str, palette: Palette) -> Div {
    div()
        .flex_none()
        .px(px(8.))
        .py(px(8.))
        .text_size(px(12.))
        .line_height(px(16.))
        .text_color(rgb(palette.text_tertiary))
        .child(text)
}

fn model_menu_item(
    id: impl Into<ElementId>,
    title: &str,
    detail: Option<String>,
    selected: bool,
    palette: Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .w_full()
        .min_h(px(32.))
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(8.))
        .py(px(6.))
        .rounded(px(6.))
        .cursor_pointer()
        .hover(move |style| style.bg(rgb(palette.fill_secondary)))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(12.))
                        .line_height(px(16.))
                        .text_color(rgb(palette.text))
                        .when(selected, |this| this.font_weight(FontWeight::MEDIUM))
                        .child(ellipsize(title, MODEL_MENU_WIDTH - 56., 12.)),
                )
                .when_some(detail, |this, detail| {
                    this.child(
                        div()
                            .text_size(px(11.))
                            .line_height(px(14.))
                            .text_color(rgb(palette.text_tertiary))
                            .child(ellipsize(&detail, MODEL_MENU_WIDTH - 56., 11.)),
                    )
                }),
        )
        .when(selected, |this| {
            this.child(
                Icon::new(IconName::Check)
                    .size(px(14.))
                    .color(rgb(palette.primary)),
            )
        })
}

fn effort_pill(
    id: impl Into<ElementId>,
    label: &str,
    selected: bool,
    palette: Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(26.))
        .px(px(6.))
        .flex()
        .items_center()
        .rounded(px(8.))
        .text_size(px(12.))
        .cursor_pointer()
        .when(selected, |this| {
            this.bg(rgb(palette.primary_bg))
                .text_color(rgb(palette.primary_text))
                .font_weight(FontWeight::MEDIUM)
        })
        .when(!selected, |this| {
            this.text_color(rgb(palette.text_secondary))
                .hover(move |style| {
                    style
                        .bg(rgb(palette.fill_secondary))
                        .text_color(rgb(palette.text))
                })
        })
        .child(label.to_owned())
}

fn status_pill(label: &str, tone: u32, palette: Palette) -> AnyElement {
    div()
        .flex_none()
        .h(px(22.))
        .flex()
        .items_center()
        .gap(px(5.))
        .px(px(8.))
        .rounded_full()
        .border_1()
        .border_color(rgb(palette.border_tertiary))
        .bg(rgb(palette.elevated))
        .child(div().size(px(6.)).rounded_full().bg(rgb(tone)))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(palette.text_secondary))
                .child(label.to_owned()),
        )
        .into_any_element()
}

fn content_header(
    title: String,
    title_budget: f32,
    pill: Option<AnyElement>,
    actions: AnyElement,
    palette: Palette,
) -> AnyElement {
    let title = ellipsize(&title, title_budget, 13.);
    div()
        .h(px(48.))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(16.))
        .border_b_1()
        .border_color(rgb(palette.border_tertiary))
        .child(
            div()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(13.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(palette.text))
                .child(title),
        )
        .when_some(pill, |this, pill| this.child(pill))
        .child(div().flex_1())
        .child(actions)
        .into_any_element()
}

fn text_button(
    id: &'static str,
    icon: Option<IconName>,
    label: &'static str,
    style: ButtonStyle,
    palette: Palette,
) -> Stateful<Div> {
    let (background, foreground, hover, border) = match style {
        ButtonStyle::Ghost => (None, palette.text_secondary, palette.fill_secondary, None),
        ButtonStyle::Outline => (
            Some(palette.base),
            palette.text,
            palette.fill_secondary,
            Some(palette.border_tertiary),
        ),
        ButtonStyle::Primary => (
            Some(palette.primary),
            palette.on_primary,
            palette.primary_hover,
            None,
        ),
        ButtonStyle::DangerGhost => (None, palette.error, palette.error_bg, None),
        ButtonStyle::Danger => (Some(palette.error), 0xffffff, palette.error, None),
    };
    div()
        .id(id)
        .h(px(28.))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.))
        .px(px(10.))
        .rounded(px(8.))
        .when_some(background, |this, color| this.bg(rgb(color)))
        .when_some(border, |this, color| {
            this.border_1().border_color(rgb(color))
        })
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(foreground))
        .cursor_pointer()
        .hover(move |this| {
            if style == ButtonStyle::Danger {
                this.opacity(0.9)
            } else {
                this.bg(rgb(hover))
            }
        })
        .when_some(icon, |this, icon| {
            this.child(Icon::new(icon).size(px(14.)).color(rgb(foreground)))
        })
        .child(label)
}

fn stepper_button(
    id: &'static str,
    icon: IconName,
    enabled: bool,
    palette: Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(move |style| style.bg(rgb(palette.fill_secondary)))
        })
        .child(Icon::new(icon).size(px(14.)).color(rgb(if enabled {
            palette.text_secondary
        } else {
            palette.text_quaternary
        })))
}

/// Qoder's MD switch: 32×20 track, 16 px thumb.
fn switch(id: &'static str, on: bool, palette: Palette) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(32.))
        .h(px(20.))
        .flex_none()
        .flex()
        .items_center()
        .px(px(2.))
        .rounded_full()
        .bg(rgb(if on {
            palette.primary
        } else {
            palette.switch_off
        }))
        .cursor_pointer()
        .when(on, |this| this.justify_end())
        .child(
            div()
                .size(px(16.))
                .rounded_full()
                .bg(rgb(if on && palette.dark {
                    palette.on_primary
                } else {
                    0xffffff
                })),
        )
}

fn text_field(state: &Entity<InputState>, palette: Palette) -> AnyElement {
    div()
        .h(px(32.))
        .px(px(4.))
        .flex()
        .items_center()
        .rounded(px(8.))
        .border_1()
        .border_color(rgb(palette.border_tertiary))
        .bg(rgb(palette.base))
        .text_size(px(13.))
        .child(
            Input::new(state)
                .appearance(false)
                .bordered(false)
                .focus_bordered(false),
        )
        .into_any_element()
}

fn settings_section(title: &'static str, group: AnyElement, palette: Palette) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(44.))
                .flex()
                .items_end()
                .px(px(4.))
                .pb(px(8.))
                .text_size(px(12.))
                .text_color(rgb(palette.text_tertiary))
                .child(title),
        )
        .child(group)
}

fn settings_group(rows: Vec<AnyElement>, palette: Palette) -> AnyElement {
    let count = rows.len();
    let mut group = div()
        .w_full()
        .flex()
        .flex_col()
        .px(px(16.))
        .rounded(px(12.))
        .border_1()
        .border_color(rgb(palette.border_tertiary))
        .bg(rgb(palette.surface))
        .shadow(palette.soft_shadow());
    for (index, row) in rows.into_iter().enumerate() {
        group = group.child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .when(index + 1 < count, |this| {
                    this.border_b_1()
                        .border_dashed()
                        .border_color(rgb(palette.border_tertiary))
                })
                .child(row),
        );
    }
    group.into_any_element()
}

fn settings_text(
    title: &'static str,
    description: impl Into<SharedString>,
    palette: Palette,
) -> impl IntoElement {
    div()
        .flex_1()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(
            div()
                .text_size(px(13.))
                .line_height(px(20.))
                .text_color(rgb(palette.text))
                .child(title),
        )
        .child(
            div()
                .text_size(px(12.))
                .line_height(px(18.))
                .text_color(rgb(palette.text_tertiary))
                .child(description.into()),
        )
}

fn settings_row(
    title: &'static str,
    description: impl Into<SharedString>,
    control: AnyElement,
    palette: Palette,
) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(16.))
        .py(px(14.))
        .child(settings_text(title, description, palette))
        .child(div().flex_none().child(control))
        .into_any_element()
}

fn settings_block(
    title: &'static str,
    description: impl Into<SharedString>,
    control: AnyElement,
    palette: Palette,
) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(10.))
        .py(px(14.))
        .child(settings_text(title, description, palette))
        // A column stretches the control to the full row width.
        .child(div().flex().flex_col().child(control))
        .into_any_element()
}

fn empty_state(text: &'static str, palette: Palette) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(13.))
        .text_color(rgb(palette.text_tertiary))
        .child(text)
        .into_any_element()
}

fn user_bubble(text: &str, palette: Palette) -> AnyElement {
    div()
        .w_full()
        .flex()
        .justify_end()
        .child(
            div()
                .max_w(px(560.))
                .px(px(14.))
                .py(px(10.))
                .rounded(px(16.))
                .bg(rgb(palette.primary_bg))
                .text_size(px(13.))
                .line_height(px(20.))
                .text_color(rgb(palette.text))
                .whitespace_normal()
                .child(text.trim().to_owned()),
        )
        .into_any_element()
}

fn render_message(
    session_id: u64,
    index: usize,
    message: &ConversationMessage,
    palette: Palette,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    match message.role {
        ConversationRole::User => user_bubble(&message.content, palette),
        // Automatic assistant entries are Longwatch's own terminal notices
        // (for example why a task was paused), not Codex prose.
        _ if message.automatic => div()
            .w_full()
            .flex()
            .gap(px(8.))
            .px(px(12.))
            .py(px(8.))
            .rounded(px(10.))
            .bg(rgb(palette.error_bg))
            .child(
                Icon::new(IconName::InfoCircle)
                    .size(px(14.))
                    .color(rgb(palette.error)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(rgb(palette.error))
                    .child(message.content.clone()),
            )
            .into_any_element(),
        _ => div()
            .w_full()
            .text_size(px(14.))
            .line_height(px(24.))
            .text_color(rgb(palette.text))
            .child(
                TextView::markdown(
                    ElementId::Name(
                        format!(
                            "reply-{session_id}-{index}-{}",
                            if palette.dark { "dark" } else { "light" }
                        )
                        .into(),
                    ),
                    chat_markdown_source(&message.content),
                    window,
                    cx,
                )
                .style(
                    chat_text_view_style(palette)
                        .paragraph_gap(gpui::rems(0.5))
                        .heading_font_size(|level, base| match level {
                            1..=2 => base * 1.15,
                            3 => base * 1.05,
                            _ => base,
                        })
                        .code_block(StyleRefinement::default().mt(gpui::rems(0.55))),
                ),
            )
            .into_any_element(),
    }
}

fn chat_text_view_style(palette: Palette) -> TextViewStyle {
    TextViewStyle {
        highlight_theme: if palette.dark {
            gpui_component::highlighter::HighlightTheme::default_dark().clone()
        } else {
            gpui_component::highlighter::HighlightTheme::default_light().clone()
        },
        is_dark: palette.dark,
        ..TextViewStyle::default()
    }
}

// ---------------------------------------------------------------------------
// Labels and text helpers
// ---------------------------------------------------------------------------

/// Messages shown for a session: its conversation plus the answer that is
/// still streaming.
fn timeline_entries(snapshot: &QueueSnapshot) -> Vec<ConversationMessage> {
    let mut entries = snapshot.conversation.clone();
    if !entries
        .iter()
        .any(|message| message.role == ConversationRole::User)
        && !snapshot.session_prompt.trim().is_empty()
    {
        entries.insert(
            0,
            ConversationMessage {
                role: ConversationRole::User,
                content: snapshot.session_prompt.clone(),
                automatic: false,
            },
        );
    }
    let streaming = matches!(snapshot.phase, QueuePhase::Sending | QueuePhase::Waiting)
        && !snapshot.reply_preview_is_notice
        && !snapshot.reply_preview.trim().is_empty();
    if streaming
        && !entries.last().is_some_and(|message| {
            message.role == ConversationRole::Assistant && message.content == snapshot.reply_preview
        })
    {
        entries.push(ConversationMessage {
            role: ConversationRole::Assistant,
            content: snapshot.reply_preview.clone(),
            automatic: false,
        });
    }
    entries
}

/// The final answer of a finished turn, for copying.
fn final_reply(snapshot: &QueueSnapshot) -> Option<String> {
    if !matches!(snapshot.phase, QueuePhase::Success | QueuePhase::Idle) {
        return None;
    }
    snapshot
        .conversation
        .iter()
        .rev()
        .find(|message| message.role == ConversationRole::Assistant && !message.automatic)
        .map(|message| message.content.clone())
}

fn session_state_label(session: &QueueSessionSummary) -> &'static str {
    match session.phase {
        QueuePhase::Connecting if session.status_message.contains("恢复") => "恢复连接中",
        QueuePhase::Connecting => "连接中",
        QueuePhase::Sending => "发送中",
        QueuePhase::Waiting if is_retry_status(&session.status_message) => "自动重试中",
        QueuePhase::Waiting => "处理中",
        QueuePhase::Backoff => "等待重试",
        QueuePhase::Success => "已完成",
        QueuePhase::Paused => "已暂停",
        QueuePhase::FatalError => "需要处理",
        QueuePhase::Idle if session.active => "准备中",
        QueuePhase::Idle => "已停止",
    }
}

fn session_meta(session: &QueueSessionSummary, now: DateTime<Utc>) -> String {
    let label = session_state_label(session);
    match session.phase {
        QueuePhase::Backoff => match session.next_attempt_at {
            Some(next) => format!(
                "{}重试 · 已尝试 {} 次",
                compact_eta(next, now),
                session.attempt_count.max(1)
            ),
            None => label.to_owned(),
        },
        QueuePhase::Connecting | QueuePhase::Sending | QueuePhase::Waiting => session
            .first_attempt_at
            .and_then(|since| elapsed_label(since, now))
            .map_or_else(
                || label.to_owned(),
                |elapsed| format!("{label} · {elapsed}"),
            ),
        QueuePhase::Success => session.reply_completed_at.map_or_else(
            || label.to_owned(),
            |done| format!("{label} · {}", clock_label(done)),
        ),
        _ => label.to_owned(),
    }
}

/// "12 秒后" / "3 分钟后" / "14:05 " — always followed by an action word.
fn compact_eta(next: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let remaining = (next - now).num_seconds();
    if remaining <= 0 {
        "即将".into()
    } else if remaining < 60 {
        format!("{remaining} 秒后")
    } else if remaining < 60 * 60 {
        format!("{} 分钟后", (remaining + 30) / 60)
    } else {
        format!("{} ", clock_label(next))
    }
}

fn elapsed_label(since: DateTime<Utc>, now: DateTime<Utc>) -> Option<String> {
    let seconds = (now - since).num_seconds();
    if seconds < 60 {
        None
    } else if seconds < 60 * 60 {
        Some(format!("{} 分钟", seconds / 60))
    } else {
        let minutes = (seconds % 3600) / 60;
        if minutes == 0 {
            Some(format!("{} 小时", seconds / 3600))
        } else {
            Some(format!("{} 小时 {minutes} 分", seconds / 3600))
        }
    }
}

fn clock_label(timestamp: DateTime<Utc>) -> String {
    let local = timestamp.with_timezone(&chrono::Local);
    if local.date_naive() == chrono::Local::now().date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%m月%d日 %H:%M").to_string()
    }
}

fn is_retry_status(status: &str) -> bool {
    let normalized = status.to_ascii_lowercase();
    status.contains("重试")
        || status.contains("恢复")
        || normalized.contains("reconnecting")
        || normalized.contains("disconnected")
        || normalized.contains("error sending request")
}

fn strip_status_prefix(status: &str) -> String {
    ["Codex 正在内部重试：", "Codex 正在静默重试："]
        .iter()
        .find_map(|prefix| status.strip_prefix(prefix))
        .unwrap_or(status)
        .to_owned()
}

/// Cut `text` to roughly `max_width` pixels with a trailing ellipsis. gpui
/// cannot ellipsize text whose width is only known after flex layout, so
/// variable-width labels are shortened up front using a glyph-width estimate.
fn ellipsize(text: &str, max_width: f32, font_size: f32) -> String {
    let glyph_width = |character: char| {
        if character.is_ascii() {
            font_size * 0.56
        } else {
            font_size
        }
    };
    let total: f32 = text.chars().map(glyph_width).sum();
    if total <= max_width.max(0.) {
        return text.to_owned();
    }
    let budget = (max_width - font_size).max(font_size * 2.);
    let mut used = 0.;
    let mut shortened = String::new();
    for character in text.chars() {
        used += glyph_width(character);
        if used > budget {
            break;
        }
        shortened.push(character);
    }
    shortened.truncate(shortened.trim_end().len());
    shortened.push('…');
    shortened
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn parse_phrases(input: &str) -> Vec<String> {
    input
        .split('|')
        .map(str::trim)
        .filter(|phrase| !phrase.is_empty())
        .map(str::to_owned)
        .collect()
}

fn cli_short_label(cli: &CodexCliInfo) -> String {
    match cli.version.as_deref() {
        Some(version) => version
            .trim()
            .trim_start_matches("codex-cli")
            .trim()
            .to_owned(),
        None => "未找到 Codex".into(),
    }
}

fn identity_description(identity: ClientIdentity) -> String {
    let detail = match identity {
        ClientIdentity::OfficialDesktop => "与 ChatGPT 桌面应用发出的请求一致",
        ClientIdentity::CodexCli => "与 Codex 命令行一致",
        ClientIdentity::Longwatch => "标明请求来自 Longwatch",
    };
    format!(
        "originator: {} · {detail}。修改后，从下一次连接开始生效",
        identity.originator()
    )
}

fn transport_status_text(snapshot: &QueueSnapshot) -> String {
    let status = &snapshot.transport_status;
    if status.connected {
        status.server_agent.as_ref().map_or_else(
            || format!("{} · 已连接", status.kind.label()),
            |agent| format!("已连接 · {agent}"),
        )
    } else {
        "任务开始时自动连接 Codex app-server；问题反馈可附上诊断信息（不含任务原文与凭据）".into()
    }
}

#[cfg(target_os = "windows")]
fn tray_status(snapshot: &QueueSnapshot) -> String {
    let running = snapshot.running_session_count;
    let waiting = snapshot.pending_tasks.len();
    match (running, waiting) {
        (0, 0) => "空闲".into(),
        (running, 0) => format!("{running} 个任务进行中"),
        (0, waiting) => format!("{waiting} 个任务排队中"),
        (running, waiting) => format!("{running} 个进行中 · {waiting} 个排队"),
    }
}

/// Keep the rich Markdown parser for headings, emphasis, links and code, but
/// flatten list containers into ordinary ChatGPT-style lines. gpui-component's
/// virtual list item uses a zero-height flex child when it is placed in this
/// auto-height conversation pane; flattening prevents list text from being
/// clipped while retaining inline Markdown marks on each line.
fn chat_markdown_source(markdown: &str) -> String {
    let mut in_fence = false;
    let mut normalized = String::with_capacity(markdown.len());

    for (index, line) in markdown.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }

        let transformed = if in_fence {
            line.to_owned()
        } else {
            normalize_markdown_list_line(line)
        };
        if index > 0 {
            normalized.push('\n');
        }
        normalized.push_str(&transformed);
    }

    normalized
}

fn normalize_markdown_list_line(line: &str) -> String {
    let indentation_len = line.len() - line.trim_start().len();
    let (indentation, content) = line.split_at(indentation_len);
    let content = content.trim_start();
    let number_len = content.bytes().take_while(u8::is_ascii_digit).count();
    if number_len > 0
        && content
            .get(number_len..)
            .is_some_and(|suffix| suffix.starts_with(". "))
    {
        let rest = &content[number_len + 2..];
        return format!("{indentation}{} · {rest}", &content[..number_len]);
    }
    let Some(rest) = content
        .strip_prefix('-')
        .or_else(|| content.strip_prefix('*'))
        .or_else(|| content.strip_prefix('+'))
    else {
        return line.to_owned();
    };
    let Some(rest) = rest.strip_prefix(' ') else {
        return line.to_owned();
    };

    let marker = if let Some(task) = rest.strip_prefix("[ ] ") {
        return format!("{indentation}☐ {task}");
    } else if let Some(task) = rest
        .strip_prefix("[x] ")
        .or_else(|| rest.strip_prefix("[X] "))
    {
        return format!("{indentation}☑ {task}");
    } else {
        "• "
    };
    format!("{indentation}{marker}{rest}")
}

// ---------------------------------------------------------------------------
// Application entry
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn windows_hwnd(window: &Window) -> Option<isize> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn sync_macos_icon_for_window(window: &Window) {
    let dark = matches!(
        window.appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    );
    gpui_platform::sync_macos_app_icon(
        dark,
        include_bytes!("../packaging/macos/Longwatch-Light.icns"),
        include_bytes!("../packaging/macos/Longwatch-Dark.icns"),
    );
}

#[allow(clippy::too_many_arguments)]
fn new_view(
    config: QueueConfig,
    snapshot: QueueSnapshot,
    commands: tokio::sync::mpsc::Sender<RuntimeCommand>,
    config_directory: PathBuf,
    logs_directory: PathBuf,
    closing: Arc<AtomicBool>,
    startup_auto_sent: bool,
    tokio: tokio::runtime::Handle,
    window: &mut Window,
    cx: &mut App,
) -> Entity<QueueView> {
    let logo = Arc::new(Image::from_bytes(
        ImageFormat::Png,
        include_bytes!("../packaging/ui-logo.png").to_vec(),
    ));
    let initial_prompt = if startup_auto_sent {
        String::new()
    } else {
        config.default_prompt.clone()
    };
    let prompt_input = cx.new(|cx| {
        InputState::new(window, cx)
            .multi_line(true)
            .default_value(initial_prompt)
            .placeholder("描述要交给 Codex 的任务，按 Enter 加入队列")
    });
    let default_prompt_input = cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(config.default_prompt.clone())
            .placeholder("例如：继续完成上次的任务")
    });
    let phrases_input = cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(config.failure_phrases.join(" | "))
            .placeholder("Server overloaded; retry later.")
    });
    cx.new(|cx| {
        let subscriptions = vec![
            cx.subscribe(
                &prompt_input,
                |view: &mut QueueView, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        view.prompt_error = false;
                        cx.notify();
                    }
                },
            ),
            cx.subscribe(
                &default_prompt_input,
                |view: &mut QueueView, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Blur) {
                        view.save_text_settings(cx);
                    }
                },
            ),
            cx.subscribe(
                &phrases_input,
                |view: &mut QueueView, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Blur) {
                        view.save_text_settings(cx);
                    }
                },
            ),
        ];
        QueueView {
            config,
            snapshot,
            commands,
            selection: Selection::New,
            initial_selection_pending: true,
            requested_session: None,
            page: Page::Queue,
            prompt_input,
            default_prompt_input,
            phrases_input,
            placeholder: "描述要交给 Codex 的任务，按 Enter 加入队列",
            codex_cli: None,
            model_catalog: ModelCatalogState::Loading,
            model_menu_open: false,
            model_chip_bounds: Rc::new(Cell::new(None)),
            tokio,
            logo,
            next_request_id: 0,
            pending_clear: None,
            awaiting_placement: None,
            prompt_error: false,
            timeline_scroll: ScrollHandle::new(),
            reply_copied: false,
            diagnostics_copied: false,
            notice: None,
            notice_counter: 0,
            exit_confirm_open: false,
            closing,
            runtime_stopped: false,
            last_slow_refresh: Instant::now(),
            header_space: 600.,
            #[cfg(target_os = "windows")]
            tray_status: String::new(),
            config_directory,
            logs_directory,
            _subscriptions: subscriptions,
            appearance_subscription: None,
        }
    })
}

pub fn run(
    config: QueueConfig,
    runtime: RuntimeHandle,
    tokio_runtime: Arc<tokio::runtime::Runtime>,
    startup_auto_sent: bool,
) {
    let shutdown_runtime = Arc::clone(&tokio_runtime);
    let tokio_handle = tokio_runtime.handle().clone();
    let commands = runtime.command_sender();
    let mut snapshots = runtime.snapshot();
    let config_directory = ConfigStore::discover()
        .map(|store| store.directory().to_path_buf())
        .unwrap_or_else(|_| PathBuf::from("."));
    let logs_directory = config_directory.join("logs");
    Application::new()
        .with_assets(TDesignAssetSource::new())
        .run(move |cx: &mut App| {
            tdesign_gpui::init(cx);
            // Markdown replies and text inputs come from gpui-component.
            gpui_component::init(cx);
            let closing = Arc::new(AtomicBool::new(false));
            let titlebar_inset = if cfg!(target_os = "windows") {
                TITLEBAR_HEIGHT
            } else {
                0.
            };
            let bounds = Bounds::centered(None, size(px(1120.), px(720. + titlebar_inset)), cx);
            let view_slot = Rc::new(RefCell::new(None));
            let view_slot_for_window = Rc::clone(&view_slot);
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        titlebar: Some(TitlebarOptions {
                            title: Some("Longwatch for Codex".into()),
                            appears_transparent: cfg!(target_os = "windows"),
                            ..Default::default()
                        }),
                        app_id: Some("Longwatch.Codex".into()),
                        is_resizable: true,
                        window_min_size: Some(size(px(880.), px(560. + titlebar_inset))),
                        ..Default::default()
                    },
                    {
                        let config = config.clone();
                        let commands = commands.clone();
                        let initial = snapshots.borrow().clone();
                        let config_directory = config_directory.clone();
                        let logs_directory = logs_directory.clone();
                        let closing = Arc::clone(&closing);
                        let tokio_handle = tokio_handle.clone();
                        move |app_window, cx| {
                            // `codex --version` goes through a node shim that can
                            // take seconds; never make the window wait for it.
                            let codex_path = config.codex_path.clone();
                            let view = new_view(
                                config,
                                initial,
                                commands,
                                config_directory,
                                logs_directory,
                                Arc::clone(&closing),
                                startup_auto_sent,
                                tokio_handle,
                                app_window,
                                cx,
                            );
                            // Countdowns, elapsed times and tray actions.
                            let ticking_view = view.clone();
                            let ticking_window = Window::window_handle(app_window);
                            cx.spawn(async move |cx| {
                                loop {
                                    Timer::after(Duration::from_millis(500)).await;
                                    if closing.load(Ordering::Acquire) {
                                        break;
                                    }
                                    if ticking_window
                                        .update(cx, |_root, window, cx| {
                                            ticking_view
                                                .update(cx, |view, cx| view.tick(window, cx));
                                        })
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            })
                            .detach();
                            let detection = cx
                                .background_executor()
                                .spawn(async move { diagnostics::inspect_codex_cli(&codex_path) });
                            let detected_view = view.clone();
                            cx.spawn(async move |cx| {
                                let info = detection.await;
                                let _ = detected_view.update(cx, |view, cx| {
                                    if info.available() {
                                        view.load_models(cx);
                                    } else {
                                        view.model_catalog = ModelCatalogState::Failed;
                                    }
                                    view.codex_cli = Some(info);
                                    cx.notify();
                                });
                            })
                            .detach();
                            *view_slot_for_window.borrow_mut() = Some(view.clone());
                            cx.new(|cx| ComponentRoot::new(view, app_window, cx))
                        }
                    },
                )
                .expect("failed to open Longwatch window");
            let queue_view = view_slot
                .borrow_mut()
                .take()
                .expect("Longwatch queue view was not initialized");
            let queue_view_for_setup = queue_view.clone();
            let _ = window.update(cx, |_root, app_window, cx| {
                let view = queue_view_for_setup.clone();
                gpui_component::Theme::sync_system_appearance(Some(app_window), cx);
                #[cfg(target_os = "macos")]
                sync_macos_icon_for_window(app_window);
                let appearance_subscription = app_window.observe_window_appearance(|window, cx| {
                    gpui_component::Theme::sync_system_appearance(Some(window), cx);
                    #[cfg(target_os = "macos")]
                    sync_macos_icon_for_window(window);
                    window.refresh();
                });
                view.update(cx, |queue_view, _| {
                    queue_view.appearance_subscription = Some(appearance_subscription);
                });
                #[cfg(target_os = "windows")]
                let native_window = windows_hwnd(app_window);
                #[cfg(target_os = "windows")]
                let tray_available = native_window.is_some_and(|native_window| {
                    match gpui_platform::install_tray(native_window) {
                        Ok(()) => true,
                        Err(error) => {
                            warn!(%error, "系统托盘初始化失败，将保留传统关闭行为");
                            false
                        }
                    }
                });
                let close_flag = Arc::clone(&closing);
                // Never block in here: a native modal box would pump this
                // window's messages while GPUI is mid-update. Confirmation is
                // an in-window dialog instead.
                app_window.on_window_should_close(cx, move |_, app| {
                    if close_flag.load(Ordering::Acquire) {
                        return true;
                    }
                    let active = view.read(app).has_active_work();
                    #[cfg(target_os = "windows")]
                    if tray_available && let Some(native_window) = native_window {
                        if !gpui_platform::tray_exit_requested() {
                            gpui_platform::hide_window_to_tray(native_window);
                            return false;
                        }
                        if !active {
                            close_flag.store(true, Ordering::Release);
                            return true;
                        }
                        gpui_platform::cancel_tray_exit();
                        gpui_platform::show_window_from_tray(native_window);
                    }
                    if !active {
                        close_flag.store(true, Ordering::Release);
                        return true;
                    }
                    view.update(app, |view, cx| {
                        view.exit_confirm_open = true;
                        cx.notify();
                    });
                    false
                });
            });
            let watcher_closing = Arc::clone(&closing);
            let queue_view_for_snapshots = queue_view;
            cx.spawn(async move |cx| {
                while snapshots.changed().await.is_ok() {
                    if watcher_closing.load(Ordering::Acquire) {
                        break;
                    }
                    let snapshot = snapshots.borrow().clone();
                    if window
                        .update(cx, |_root, window, cx| {
                            queue_view_for_snapshots.update(cx, |view, cx| {
                                view.update_snapshot(snapshot, window, cx);
                            });
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                if watcher_closing.load(Ordering::Acquire) {
                    return;
                }
                // The runtime ended on its own; say so inside the window.
                let _ = window.update(cx, |_root, _window, cx| {
                    queue_view_for_snapshots.update(cx, |view, cx| {
                        view.runtime_stopped = true;
                        cx.notify();
                    });
                });
            })
            .detach();
        });

    #[cfg(target_os = "windows")]
    gpui_platform::shutdown_tray();

    // Build timer-backed futures only after entering the Tokio runtime. Calling
    // `tokio::time::timeout` as an eager `block_on` argument panics during app
    // teardown because no reactor is active on GPUI's main thread.
    shutdown_runtime.block_on(async move {
        if let Err(error) = runtime.send(RuntimeCommand::Shutdown).await {
            warn!(%error, "发送后台关闭命令失败");
        }
        match tokio::time::timeout(Duration::from_secs(3), runtime.join()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => warn!(%error, "后台任务异常结束"),
            Err(_) => warn!("等待后台任务退出超时"),
        }
    });
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn summary(phase: QueuePhase) -> QueueSessionSummary {
        QueueSessionSummary {
            id: 7,
            phase,
            has_history: true,
            active: phase.is_running(),
            ..QueueSessionSummary::default()
        }
    }

    #[test]
    fn backoff_rows_show_a_live_countdown_and_attempts() {
        let now = Utc.with_ymd_and_hms(2026, 10, 28, 6, 0, 0).unwrap();
        let mut session = summary(QueuePhase::Backoff);
        session.next_attempt_at = Some(now + chrono::Duration::seconds(42));
        session.attempt_count = 3;

        assert_eq!(session_meta(&session, now), "42 秒后重试 · 已尝试 3 次");
        session.next_attempt_at = Some(now + chrono::Duration::seconds(190));
        assert_eq!(session_meta(&session, now), "3 分钟后重试 · 已尝试 3 次");
        session.next_attempt_at = Some(now - chrono::Duration::seconds(1));
        assert_eq!(session_meta(&session, now), "即将重试 · 已尝试 3 次");
    }

    #[test]
    fn running_rows_show_how_long_the_task_has_been_queued() {
        let now = Utc.with_ymd_and_hms(2026, 10, 28, 6, 0, 0).unwrap();
        let mut session = summary(QueuePhase::Waiting);
        session.first_attempt_at = Some(now - chrono::Duration::seconds(30));
        assert_eq!(session_meta(&session, now), "处理中");

        session.first_attempt_at = Some(now - chrono::Duration::minutes(75));
        assert_eq!(session_meta(&session, now), "处理中 · 1 小时 15 分");

        session.status_message = "Codex 正在内部重试：Reconnecting... 2/5".into();
        assert_eq!(session_state_label(&session), "自动重试中");
    }

    #[test]
    fn finished_and_stopped_tasks_are_grouped_apart_from_running_ones() {
        let snapshot = QueueSnapshot {
            sessions: vec![
                QueueSessionSummary {
                    id: 1,
                    ..summary(QueuePhase::Waiting)
                },
                QueueSessionSummary {
                    id: 2,
                    ..summary(QueuePhase::Success)
                },
                QueueSessionSummary {
                    id: 3,
                    ..summary(QueuePhase::Paused)
                },
                QueueSessionSummary {
                    id: 4,
                    has_history: false,
                    ..summary(QueuePhase::Idle)
                },
                QueueSessionSummary {
                    id: 5,
                    ..summary(QueuePhase::Idle)
                },
            ],
            pending_tasks: vec![PendingTaskSummary {
                id: 9,
                title: "later".into(),
                ..PendingTaskSummary::default()
            }],
            ..QueueSnapshot::default()
        };
        let lists = QueueLists::from_snapshot(&snapshot);

        assert_eq!(lists.running.iter().map(|s| s.id).collect::<Vec<_>>(), [1]);
        assert_eq!(lists.paused.iter().map(|s| s.id).collect::<Vec<_>>(), [3]);
        assert_eq!(
            lists.finished.iter().map(|s| s.id).collect::<Vec<_>>(),
            [5, 2]
        );
        assert_eq!(lists.waiting.len(), 1);
        assert_eq!(lists.waiting[0].0, 1);
    }

    #[test]
    fn streamed_text_is_shown_but_error_notices_are_not() {
        let mut snapshot = QueueSnapshot {
            phase: QueuePhase::Waiting,
            session_prompt: "write a poem".into(),
            reply_preview: "Roses are".into(),
            ..QueueSnapshot::default()
        };
        let entries = timeline_entries(&snapshot);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "write a poem");
        assert_eq!(entries[1].content, "Roses are");

        snapshot.reply_preview = "stream disconnected before completion".into();
        snapshot.reply_preview_is_notice = true;
        assert_eq!(timeline_entries(&snapshot).len(), 1);
    }

    #[test]
    fn identity_description_names_the_upstream_originator() {
        assert!(identity_description(ClientIdentity::OfficialDesktop).contains("Codex Desktop"));
        assert!(identity_description(ClientIdentity::CodexCli).contains("codex_cli_rs"));
    }

    #[test]
    fn long_labels_are_shortened_with_an_ellipsis() {
        assert_eq!(ellipsize("short", 200., 12.), "short");
        let shortened = ellipsize("一个很长很长很长很长很长的任务标题", 100., 12.);
        assert!(shortened.ends_with('…'));
        assert!(shortened.chars().count() < 18);
    }

    #[test]
    fn phrases_are_split_on_bars_and_trimmed() {
        assert_eq!(
            parse_phrases(" busy | | Server overloaded; retry later. "),
            ["busy", "Server overloaded; retry later."]
        );
    }

    #[test]
    fn chat_markdown_source_preserves_rich_markdown_and_flattens_lists() {
        let source = "# 标题\n\n- **第一项**\n- 第二项\n1. 第三项\n\n```\n- 保留原始代码\n```";

        assert_eq!(
            chat_markdown_source(source),
            "# 标题\n\n• **第一项**\n• 第二项\n1 · 第三项\n\n```\n- 保留原始代码\n```"
        );
    }
}
