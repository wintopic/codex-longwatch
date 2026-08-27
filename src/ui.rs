//! `TDesign` GPUI desktop shell.

use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::WindowAppearance;
use gpui::{
    Animation, AnimationExt as _, App, Application, Bounds, ClipboardItem, Context, Entity,
    EntityInputHandler, Focusable, FontWeight, Image, ImageFormat, Render, ScrollHandle, Stateful,
    StyleRefinement, Timer, TitlebarOptions, Transformation, Window, WindowBounds,
    WindowControlArea, WindowOptions, div, img, percentage, point, prelude::*, px, relative, rgb,
    rgba, size, svg,
};
use gpui_component::Root as ComponentRoot;
use gpui_component::input::{Input as ThemedInput, InputState};
use gpui_component::text::{TextView, TextViewStyle};
#[cfg(target_os = "windows")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tdesign_gpui::{
    Icon, IconName, Switch, TDesignAssetSource, TDesignConfig, TDesignRoot, Tag, TagTheme,
    ThemeMode, ThemeOverrides, ToggleState,
};
use tracing::warn;

use crate::{
    config::{ConfigStore, QueueConfig},
    diagnostics::{self, CodexCliInfo},
    queue::{ConversationMessage, ConversationRole, PendingMessage, QueuePhase, QueueSnapshot},
    runtime::{RuntimeCommand, RuntimeHandle},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CopyFeedback {
    #[default]
    Idle,
    Copied,
}

// Longwatch follows a small, deliberately boring visual language: an 8px
// spacing rhythm, quiet surfaces, and one shared panel radius. Keeping these
// values together makes the desktop shell feel like one product instead of a
// collection of individually styled widgets.
const PANEL_RADIUS: f32 = 10.;
const INNER_RADIUS: f32 = 8.;
const CANVAS_COLOR: u32 = 0xf5f6f5;
const PANEL_BORDER: u32 = 0xdfe5e2;
const SOFT_BORDER: u32 = 0xe7ebe9;
const TEXT_PRIMARY: u32 = 0x252a28;
const TEXT_SECONDARY: u32 = 0x68716d;
const TEXT_MUTED: u32 = 0x929a96;
const BRAND_COLOR: u32 = 0x16a673;

/// Colors owned by the application shell. `TDesign` and `gpui-component` already
/// have system-aware palettes, but the dashboard is intentionally built from
/// plain GPUI primitives. Keeping its colors in one small palette prevents a
/// Windows dark-mode switch from leaving light cards and a light composer
/// behind a dark code block.
#[derive(Clone, Copy, Debug)]
struct UiPalette {
    dark: bool,
    canvas: u32,
    surface: u32,
    surface_alt: u32,
    surface_subtle: u32,
    border: u32,
    border_soft: u32,
    divider: u32,
    text: u32,
    text_secondary: u32,
    text_muted: u32,
    user_bubble: u32,
    user_text: u32,
    hover: u32,
    action: u32,
    action_text: u32,
    brand: u32,
    brand_soft: u32,
    error: u32,
    error_soft: u32,
    warning_soft: u32,
    success_soft: u32,
    inactive: u32,
    inactive_border: u32,
    overlay: u32,
}

impl UiPalette {
    fn for_window(window: &Window) -> Self {
        Self::for_dark(matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        ))
    }

    fn for_dark(dark: bool) -> Self {
        if dark {
            Self {
                dark,
                canvas: 0x171b19,
                surface: 0x202622,
                surface_alt: 0x252d28,
                surface_subtle: 0x1c221f,
                border: 0x3b4841,
                border_soft: 0x303b35,
                divider: 0x344039,
                text: 0xe9f0ec,
                text_secondary: 0xaab8b0,
                text_muted: 0x829089,
                user_bubble: 0x304038,
                user_text: 0xe7efea,
                hover: 0x2d3932,
                action: 0x3b4741,
                action_text: 0xf5faf7,
                brand: 0x43c997,
                brand_soft: 0x1f4c3b,
                error: 0xff958a,
                error_soft: 0x4b2927,
                warning_soft: 0x4a3523,
                success_soft: 0x214a3a,
                inactive: 0x2a332e,
                inactive_border: 0x435049,
                overlay: 0x00000099,
            }
        } else {
            Self {
                dark,
                canvas: CANVAS_COLOR,
                surface: 0xffffff,
                surface_alt: 0xf9fbfa,
                surface_subtle: 0xf8faf9,
                border: PANEL_BORDER,
                border_soft: SOFT_BORDER,
                divider: 0xe7e7e7,
                text: TEXT_PRIMARY,
                text_secondary: TEXT_SECONDARY,
                text_muted: TEXT_MUTED,
                user_bubble: 0xf4f4f4,
                user_text: 0x2f2f2f,
                hover: 0xf0f4f2,
                action: 0x2f3432,
                action_text: 0xffffff,
                brand: BRAND_COLOR,
                brand_soft: 0xe8f8f0,
                error: 0xc23b33,
                error_soft: 0xfff5f3,
                warning_soft: 0xfff7f0,
                success_soft: 0xf4faf7,
                inactive: 0xe9eeeb,
                inactive_border: 0xdce2df,
                overlay: 0x0000004d,
            }
        }
    }
}

struct QueueView {
    config: QueueConfig,
    snapshot: QueueSnapshot,
    commands: tokio::sync::mpsc::Sender<RuntimeCommand>,
    prompt_input: Entity<InputState>,
    default_prompt_input: Entity<InputState>,
    cwd_input: Entity<InputState>,
    phrases_input: Entity<InputState>,
    auto_send_toggle: Entity<ToggleState>,
    full_screen_toggle: Entity<ToggleState>,
    audio_toggle: Entity<ToggleState>,
    logo_image: Arc<Image>,
    codex_cli: CodexCliInfo,
    advanced_open: bool,
    route_motion_frame: u8,
    celebration_burst_id: u64,
    prompt_error: bool,
    next_request_id: u64,
    pending_clear_request: Option<u64>,
    pending_clear_text: Option<String>,
    conversation_scroll: ScrollHandle,
    copy_feedback: CopyFeedback,
    diagnostic_copy_feedback: CopyFeedback,
    config_directory: PathBuf,
    logs_directory: PathBuf,
    #[allow(dead_code)] // Stored for its lifetime; dropping it stops theme-change callbacks.
    appearance_subscription: Option<gpui::Subscription>,
}

impl QueueView {
    fn send(&self, command: RuntimeCommand) -> bool {
        // UI callbacks run on GPUI's executor, not necessarily inside a
        // Tokio reactor.  A synchronous bounded send avoids spawning a Tokio
        // future during window teardown (which used to trigger "there is no
        // reactor running" on the final tray callback).
        self.commands.try_send(command).is_ok()
    }

    fn configure(&self) {
        let _ = self.send(RuntimeCommand::Configure(self.config.clone()));
    }

    fn start_now(&mut self, prompt: String, cwd: String, phrases: String, _cx: &mut Context<Self>) {
        if self.pending_clear_request.is_some() {
            return;
        }
        if prompt.trim().is_empty() {
            self.prompt_error = true;
            return;
        }
        self.prompt_error = false;
        gpui_platform::stop_completion_overlay();
        let submitted_prompt = prompt.clone();
        self.config.prompt = prompt;
        self.config.working_directory = (!cwd.trim().is_empty()).then(|| PathBuf::from(cwd.trim()));
        self.config.failure_phrases = phrases
            .split('|')
            .map(str::trim)
            .filter(|phrase| !phrase.is_empty())
            .map(str::to_owned)
            .collect();
        let request_id = self.allocate_request_id();
        if self.send(RuntimeCommand::SubmitConfigured {
            config: self.config.clone(),
            request_id,
        }) {
            self.pending_clear_request = Some(request_id);
            self.pending_clear_text = Some(submitted_prompt);
        }
    }

    fn queue_message_now(
        &mut self,
        prompt: String,
        cwd: String,
        phrases: String,
        cx: &mut Context<Self>,
    ) {
        if self.pending_clear_request.is_some() {
            return;
        }
        if prompt.trim().is_empty() {
            self.prompt_error = true;
            return;
        }
        self.prompt_error = false;
        let message = PendingMessage {
            prompt,
            working_directory: (!cwd.trim().is_empty()).then(|| PathBuf::from(cwd.trim())),
            failure_phrases: phrases
                .split('|')
                .map(str::trim)
                .filter(|phrase| !phrase.is_empty())
                .map(str::to_owned)
                .collect(),
        };
        let request_id = self.allocate_request_id();
        if self.send(RuntimeCommand::QueueMessageWithAck {
            message,
            request_id,
        }) {
            self.pending_clear_request = Some(request_id);
            self.pending_clear_text = Some(self.prompt_input.read(cx).value().to_string());
        }
    }

    fn allocate_request_id(&mut self) -> u64 {
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        self.next_request_id
    }

    fn resume_now(&mut self, cx: &mut Context<Self>) {
        self.prompt_error = false;
        gpui_platform::stop_completion_overlay();
        let _ = self.send(RuntimeCommand::Resume);
        cx.notify();
    }

    fn stop_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        gpui_platform::stop_completion_overlay();
        let prompt = self.config.prompt.clone();
        self.prompt_input.update(cx, |state, cx| {
            state.set_value(prompt, window, cx);
        });
        self.prompt_error = false;
        self.pending_clear_request = None;
        self.pending_clear_text = None;
        let _ = self.send(RuntimeCommand::Stop);
    }

    fn set_auto_send_default(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.save_default_prompt_from_input(cx);
        self.config.auto_send_default_prompt = enabled;
        self.configure();
        cx.notify();
    }

    fn set_full_screen(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.config.full_screen_flash_enabled = enabled;
        if !enabled {
            gpui_platform::stop_completion_overlay();
        }
        self.configure();
        cx.notify();
    }

    fn set_audio_alert(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.config.audio_alert_enabled = enabled;
        self.configure();
        cx.notify();
    }

    fn trigger_success_burst(&mut self, cx: &mut Context<Self>) {
        if self.snapshot.phase == QueuePhase::Success {
            self.celebration_burst_id = self.celebration_burst_id.wrapping_add(1);
            cx.notify();
        }
    }

    fn toggle_advanced_now(&mut self, cx: &mut Context<Self>) {
        if self.advanced_open {
            self.save_default_prompt_from_input(cx);
        }
        self.advanced_open = !self.advanced_open;
        cx.notify();
    }

    fn save_default_prompt_from_input(&mut self, cx: &mut Context<Self>) {
        let default_prompt = self.default_prompt_input.read(cx).value().to_string();
        if default_prompt != self.config.default_prompt {
            self.config.default_prompt = default_prompt;
            self.configure();
        }
    }

    fn open_thread_now(&mut self) {
        gpui_platform::stop_completion_overlay();
        if let Some(thread_id) = self.snapshot.active_thread_id.as_deref() {
            if let Err(error) =
                gpui_platform::open_thread(thread_id, &self.config.codex_path.to_string_lossy())
            {
                gpui_platform::show_error_dialog("无法打开 Codex 结果", &error.to_string());
            }
        }
    }

    fn copy_reply(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copy_feedback = CopyFeedback::Copied;
        let view = cx.entity();
        cx.spawn_in(window, async move |_, cx| {
            Timer::after(Duration::from_millis(1400)).await;
            let _ = view.update(cx, |view, cx| {
                view.copy_feedback = CopyFeedback::Idle;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_logs_now(&self) {
        if let Err(error) = gpui_platform::open_directory(&self.logs_directory) {
            gpui_platform::show_error_dialog("无法打开日志目录", &error.to_string());
        }
    }

    fn copy_diagnostics(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let report = diagnostics::build_support_report(
            &self.config_directory,
            &self.codex_cli,
            &self.snapshot,
        );
        cx.write_to_clipboard(ClipboardItem::new_string(report));
        self.diagnostic_copy_feedback = CopyFeedback::Copied;
        let view = cx.entity();
        cx.spawn_in(window, async move |_, cx| {
            Timer::after(Duration::from_millis(1600)).await;
            let _ = view.update(cx, |view, cx| {
                view.diagnostic_copy_feedback = CopyFeedback::Idle;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn update_snapshot(
        &mut self,
        snapshot: QueueSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .pending_clear_request
            .is_some_and(|request_id| snapshot.accepted_request_id == request_id)
        {
            let submitted_text = self.pending_clear_text.take();
            let current_text = self.prompt_input.read(cx).value().to_string();
            if submitted_text.as_deref() == Some(current_text.as_str()) {
                self.prompt_input.update(cx, |state, cx| {
                    state.set_value("", window, cx);
                });
            }
            self.pending_clear_request = None;
            self.prompt_error = false;
        }
        let entered_success =
            self.snapshot.phase != QueuePhase::Success && snapshot.phase == QueuePhase::Success;
        let conversation_changed = self.snapshot.reply_preview != snapshot.reply_preview
            || self.snapshot.status_message != snapshot.status_message
            || self.snapshot.phase != snapshot.phase;
        let max_scroll = self.conversation_scroll.max_offset().height;
        let distance_from_bottom = max_scroll + self.conversation_scroll.offset().y;
        let should_follow = max_scroll <= px(1.) || distance_from_bottom <= px(24.);
        let retry_alerts = snapshot
            .retry_alert_count
            .saturating_sub(self.snapshot.retry_alert_count)
            .min(16);
        if conversation_changed {
            self.copy_feedback = CopyFeedback::Idle;
        }
        if entered_success {
            self.celebration_burst_id = self.celebration_burst_id.wrapping_add(1);
        }
        self.snapshot = snapshot;
        if should_follow && conversation_changed {
            self.conversation_scroll.scroll_to_bottom();
        }
        #[cfg(target_os = "windows")]
        update_tray_state(&self.snapshot);
        if retry_alerts > 0 && self.config.full_screen_flash_enabled {
            for _ in 0..retry_alerts {
                gpui_platform::show_retry_error_overlay();
            }
        }
        cx.notify();
    }
}

impl Render for QueueView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = UiPalette::for_window(window);
        let phase = friendly_phase(self.snapshot.phase);
        let next = next_attempt_label(self.snapshot.next_attempt_at);
        let primary_label = primary_action_label(self.snapshot.phase);
        let phase_accent = phase_color(self.snapshot.phase, palette);
        let has_runtime_warning = self.snapshot.runtime_warning.is_some();
        let phase_detail = self.snapshot.runtime_warning.clone().unwrap_or_else(|| {
            phase_detail(
                self.snapshot.phase,
                next.as_deref(),
                self.snapshot.consecutive_retries,
            )
        });
        let diagnostic_ok = self.codex_cli.available();
        let cli_status = if diagnostic_ok {
            format!("{} · 可用", self.codex_cli.version_label())
        } else {
            "Codex CLI 不可用".into()
        };
        let transport_status = transport_status_text(&self.snapshot);
        let viewport = window.viewport_size();
        let compact = viewport.height < px(640.);
        let status_padding = if compact { 12. } else { 16. };
        // Keep the composer inset (8px top + 12px bottom) and the three left
        // cards on one shared 147px track at the normal desktop size. The
        // shared track is what makes the attempt card line up with the input
        // surface instead of relying on content-driven flex sizing.
        let composer_panel_height = if compact { 153. } else { 167. };
        // The composer is inset 8px from the top of its reserved panel. Reserve
        // exactly the same height for the attempt card so both top and bottom
        // borders line up across the two columns.
        let attempt_panel_height = composer_panel_height - 20.;
        let side_by_side = viewport.width >= px(780.);
        let has_conversation_task = self.snapshot.attempt_count > 0
            || self.snapshot.active_thread_id.is_some()
            || self.snapshot.phase != QueuePhase::Idle;
        let assistant_is_error = matches!(
            self.snapshot.phase,
            QueuePhase::Backoff | QueuePhase::FatalError
        ) || (matches!(
            self.snapshot.phase,
            QueuePhase::Waiting | QueuePhase::Paused
        ) && is_retry_status(&self.snapshot.status_message));
        let view_entity = cx.entity();
        let start_handler = view_entity.clone();
        let settings_handler = view_entity.clone();
        let status_icon_element = if self.snapshot.phase == QueuePhase::Success {
            let celebration_handler = view_entity.clone();
            let celebration_id = self.celebration_burst_id;
            div()
                .id("success-status-icon")
                .relative()
                .w(px(58.))
                .h(px(58.))
                .flex_none()
                .cursor_pointer()
                .hover(|this| this.opacity(0.82))
                .on_click(move |_, _, app| {
                    celebration_handler.update(app, |view, cx| {
                        view.trigger_success_burst(cx);
                    });
                })
                .when(celebration_id > 0, |this| {
                    this.child(success_feedback_overlay(celebration_id))
                })
                .child(status_icon(&self.snapshot, phase_accent, palette))
                .into_any_element()
        } else {
            div()
                .child(status_icon(&self.snapshot, phase_accent, palette))
                .into_any_element()
        };
        let primary_is_stop = matches!(
            self.snapshot.phase,
            QueuePhase::Connecting
                | QueuePhase::Sending
                | QueuePhase::Waiting
                | QueuePhase::Backoff
        );
        let primary_button = div()
            .id(primary_label)
            .focusable()
            .w(px(34.))
            .h(px(34.))
            .flex_none()
            .rounded_full()
            .bg(rgb(palette.action))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|this| this.opacity(0.84))
            .child(
                Icon::new(if primary_is_stop {
                    IconName::Stop
                } else {
                    IconName::ArrowUp
                })
                .size(px(if primary_is_stop { 24. } else { 18. }))
                .color(rgb(palette.action_text)),
            )
            .on_click(move |_, window, app| {
                let prompt = start_handler
                    .read(app)
                    .prompt_input
                    .read(app)
                    .value()
                    .to_string();
                let cwd = start_handler
                    .read(app)
                    .cwd_input
                    .read(app)
                    .value()
                    .to_string();
                let phrases = start_handler
                    .read(app)
                    .phrases_input
                    .read(app)
                    .value()
                    .to_string();
                start_handler.update(app, |view, cx| {
                    if matches!(
                        view.snapshot.phase,
                        QueuePhase::Connecting
                            | QueuePhase::Sending
                            | QueuePhase::Waiting
                            | QueuePhase::Backoff
                    ) {
                        view.stop_now(window, cx);
                    } else if view.snapshot.phase == QueuePhase::Paused
                        && view.snapshot.active_thread_id.is_some()
                    {
                        view.resume_now(cx);
                    } else {
                        view.start_now(prompt, cwd, phrases, cx);
                    }
                    cx.notify();
                });
            });
        let settings_button = div()
            .id("advanced-settings")
            .focusable()
            .h(px(34.))
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(palette.border))
            .bg(rgb(palette.surface_subtle))
            .text_sm()
            .text_color(rgb(palette.text_secondary))
            .cursor_pointer()
            .hover({
                let hover = palette.hover;
                move |this| this.bg(rgb(hover))
            })
            .child(
                Icon::new(IconName::Setting)
                    .size(px(16.))
                    .color(rgb(palette.text_secondary)),
            )
            .child("设置")
            .on_click(move |_, window, app| {
                settings_handler.update(app, |view, cx| {
                    let _ = window;
                    view.toggle_advanced_now(cx);
                });
            });

        let full_screen_handler = view_entity.clone();
        let audio_handler = view_entity.clone();
        let full_screen_switch =
            Switch::new(self.full_screen_toggle.clone()).on_change(move |change, _, app| {
                full_screen_handler
                    .update(app, |view, cx| view.set_full_screen(change.current, cx));
            });
        let audio_switch =
            Switch::new(self.audio_toggle.clone()).on_change(move |change, _, app| {
                audio_handler.update(app, |view, cx| view.set_audio_alert(change.current, cx));
            });

        let logs_handler = view_entity.clone();
        let open_logs_button = settings_action_button(
            "open-log-directory",
            "打开日志",
            IconName::FolderOpen,
            rgb(palette.text_secondary),
            palette,
        )
        .on_click(move |_, _, app| {
            logs_handler.update(app, |view, _| view.open_logs_now());
        });
        let diagnostics_handler = view_entity.clone();
        let copy_diagnostics_button = settings_action_button(
            "copy-diagnostics",
            if self.diagnostic_copy_feedback == CopyFeedback::Copied {
                "已复制"
            } else {
                "复制诊断"
            },
            if self.diagnostic_copy_feedback == CopyFeedback::Copied {
                IconName::Check
            } else {
                IconName::Copy
            },
            if self.diagnostic_copy_feedback == CopyFeedback::Copied {
                rgb(palette.brand)
            } else {
                rgb(palette.text_secondary)
            },
            palette,
        )
        .on_click(move |_, window, app| {
            diagnostics_handler.update(app, |view, cx| {
                view.copy_diagnostics(window, cx);
            });
        });
        let diagnostic_actions = div()
            .flex()
            .items_center()
            .gap_1()
            .child(copy_diagnostics_button)
            .child(open_logs_button);

        let conversation_history = if has_conversation_task {
            let mut history = self.snapshot.conversation.clone();
            // State files written by older versions only had one preview. Keep
            // that result visible while the new local history is being built.
            if history.is_empty() {
                history.push(ConversationMessage {
                    role: ConversationRole::User,
                    content: self.config.prompt.clone(),
                    automatic: false,
                });
                if !self.snapshot.reply_preview.trim().is_empty()
                    || self.snapshot.phase != QueuePhase::Success
                {
                    history.push(ConversationMessage {
                        role: ConversationRole::Assistant,
                        content: conversation_text(&self.snapshot),
                        automatic: true,
                    });
                }
            }
            let transient_assistant = conversation_text(&self.snapshot);
            let last_is_same_reply = history.last().is_some_and(|message| {
                message.role == ConversationRole::Assistant
                    && message.content == transient_assistant
            });
            if !last_is_same_reply
                && (!transient_assistant.trim().is_empty())
                && matches!(
                    self.snapshot.phase,
                    QueuePhase::Connecting
                        | QueuePhase::Sending
                        | QueuePhase::Waiting
                        | QueuePhase::Backoff
                        | QueuePhase::Paused
                        | QueuePhase::FatalError
                )
            {
                history.push(ConversationMessage {
                    role: ConversationRole::Assistant,
                    content: transient_assistant,
                    automatic: true,
                });
            }
            let history_len = history.len();
            let history_elements = history
                .iter()
                .enumerate()
                .map(|(index, message)| match message.role {
                    ConversationRole::User => div()
                        .w_full()
                        .min_w(px(0.))
                        .flex_none()
                        .flex()
                        .justify_end()
                        .child(
                            div()
                                .flex_none()
                                .max_w(px(600.))
                                .min_w(px(72.))
                                .px_3()
                                .py_2()
                                .rounded(px(18.))
                                .bg(rgb(palette.user_bubble))
                                .text_base()
                                .text_color(rgb(palette.user_text))
                                .whitespace_normal()
                                .child(message.content.clone()),
                        )
                        .into_any_element(),
                    ConversationRole::Assistant | ConversationRole::System => {
                        let markdown = chat_markdown_source(&message.content);
                        let text_color = if assistant_is_error
                            && message.automatic
                            && index + 1 == history_len
                        {
                            rgb(palette.error)
                        } else if message.role == ConversationRole::System {
                            rgb(palette.text_secondary)
                        } else {
                            rgb(palette.text)
                        };
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .flex_none()
                            .flex()
                            .justify_start()
                            .child(
                                div()
                                    .w_full()
                                    .min_w(px(0.))
                                    .flex_none()
                                    .max_w(px(640.))
                                    .text_base()
                                    .text_color(text_color)
                                    .child(
                                        TextView::markdown(
                                            (
                                                gpui::ElementId::from((
                                                    "assistant-markdown",
                                                    index,
                                                )),
                                                if palette.dark { "dark" } else { "light" },
                                            ),
                                            markdown,
                                            window,
                                            cx,
                                        )
                                        .style(
                                            chat_text_view_style(palette)
                                                .paragraph_gap(gpui::rems(0.45))
                                                .heading_font_size(|level, base| match level {
                                                    1..=2 => base * 1.15,
                                                    3 => base * 1.05,
                                                    _ => base * 0.95,
                                                })
                                                .code_block(
                                                    StyleRefinement::default().mt(gpui::rems(0.55)),
                                                ),
                                        ),
                                    ),
                            )
                            .into_any_element()
                    }
                })
                .collect::<Vec<_>>();
            let assistant_text = conversation_text(&self.snapshot);
            let copy_text = assistant_text.clone();
            let copy_handler = view_entity.clone();
            let copy_button = icon_action_button(
                "复制回复",
                if self.copy_feedback == CopyFeedback::Copied {
                    IconName::Check
                } else {
                    IconName::Copy
                },
                if self.copy_feedback == CopyFeedback::Copied {
                    rgb(palette.brand)
                } else {
                    rgb(palette.text_secondary)
                },
                palette,
            )
            .on_click(move |_, window, app| {
                let text = copy_text.clone();
                copy_handler.update(app, |view, cx| view.copy_reply(text, window, cx));
            });
            let can_open_result = self.snapshot.phase == QueuePhase::Success
                && self.snapshot.active_thread_id.is_some();
            let open_result_handler = view_entity.clone();
            let open_result_button = icon_action_button(
                "打开结果",
                IconName::Fullscreen,
                rgb(palette.text_secondary),
                palette,
            )
            .on_click(move |_, window, app| {
                open_result_handler.update(app, |view, _| {
                    let _ = window;
                    view.open_thread_now();
                });
            });
            let assistant_actions = div()
                .flex()
                .items_center()
                .gap_1()
                .child(copy_button)
                .when(can_open_result, |this| this.child(open_result_button))
                .when_some(
                    self.snapshot.reply_completed_at.map(reply_time_label),
                    |this, time| {
                        this.child(
                            div()
                                .ml_1()
                                .text_xs()
                                .text_color(rgb(palette.text_muted))
                                .child(time),
                        )
                    },
                );
            let assistant_content = div()
                .w_full()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_4()
                .children(history_elements)
                .child(assistant_actions);
            div()
                .w_full()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_4()
                .child(assistant_content)
                .into_any_element()
        } else {
            div()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .px_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(palette.text_muted))
                        .child("等待任务"),
                )
                .into_any_element()
        };

        let conversation_card = div()
            .h_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(PANEL_RADIUS))
            .border_1()
            .border_color(rgb(palette.border))
            .bg(rgb(palette.surface))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(palette.divider))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(palette.text))
                            .child("Codex 对话"),
                    )
                    .child(
                        Tag::new(reply_tag_label(self.snapshot.phase))
                            .theme(tag_theme(self.snapshot.phase)),
                    ),
            )
            .child(
                div()
                    .id("conversation-history")
                    .flex_1()
                    .min_h(px(64.))
                    .overflow_y_scroll()
                    .track_scroll(&self.conversation_scroll)
                    .px_4()
                    .py_3()
                    .bg(rgb(palette.surface))
                    .child(conversation_history),
            )
            .child(
                div()
                    .h(px(composer_panel_height))
                    .flex_none()
                    .px_3()
                    .pt_2()
                    .pb_3()
                    .child(
                        div()
                            .size_full()
                            .relative()
                            .overflow_hidden()
                            .rounded(px(INNER_RADIUS))
                            .border_1()
                            .border_color(rgb(palette.border))
                            .bg(rgb(palette.surface))
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .right_0()
                                    .bottom_0()
                                    .child(composer_field(self.prompt_input.clone(), palette)),
                            )
                            .child(
                                div()
                                    // Keep the editor full-height and place the controls over
                                    // its lower edge. The stock Textarea border now lands at the
                                    // composer's outer edge instead of between text and controls.
                                    .absolute()
                                    .left_0()
                                    .right_0()
                                    .bottom_0()
                                    .h(px(40.))
                                    .bg(rgb(palette.surface))
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .px_2()
                                    .pb_2()
                                    .when(self.prompt_error, |this| {
                                        this.child(
                                            div()
                                                .text_sm()
                                                .text_color(rgb(palette.error))
                                                .child("请输入任务内容后再开始"),
                                        )
                                    })
                                    .when(self.snapshot.queued_message.is_some(), |this| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(palette.brand))
                                                .child("已排队 1 条，当前任务结束后自动发送"),
                                        )
                                    })
                                    .child(div().flex_1())
                                    .child(primary_button),
                            ),
                    ),
            )
            .into_any_element();

        let status_card = div()
            .when(side_by_side, |this| this.flex_1().min_h(px(0.)))
            .when(!side_by_side, |this| this.flex_none())
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(PANEL_RADIUS))
            .border_1()
            .border_color(rgb(palette.border))
            .bg(rgb(palette.surface))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(palette.divider))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(palette.text))
                            .child("状态"),
                    )
                    .child(Tag::new(phase).theme(tag_theme(self.snapshot.phase))),
            )
            .child(
                div()
                    .when(side_by_side, |this| this.flex_1().min_h(px(0.)))
                    .when(!side_by_side, |this| this.flex_none())
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .child(
                        div()
                            .h(px(attempt_panel_height))
                            .flex_none()
                            .relative()
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .p(px(status_padding))
                            .rounded(px(INNER_RADIUS))
                            .bg(status_surface(self.snapshot.phase, palette))
                            .border_1()
                            .border_color(status_surface_border(self.snapshot.phase, palette))
                            .when(self.snapshot.phase == QueuePhase::Success, |this| {
                                this.child(success_completion_background())
                            })
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .gap_3()
                                    .when(self.snapshot.phase == QueuePhase::Success, |this| {
                                        this.flex_col()
                                    })
                                    .child(status_icon_element)
                                    .child(
                                        div()
                                            .text_size(
                                                if self.snapshot.phase == QueuePhase::Success {
                                                    px(22.)
                                                } else {
                                                    px(18.)
                                                },
                                            )
                                            .font_weight(FontWeight::BOLD)
                                            .child(simple_status_title(&self.snapshot)),
                                    ),
                            )
                            .when(self.snapshot.phase != QueuePhase::Success, |this| {
                                this.child(
                                    div()
                                        .w_full()
                                        .px_2()
                                        .text_sm()
                                        .text_center()
                                        .text_color(if has_runtime_warning {
                                            rgb(palette.error)
                                        } else {
                                            rgb(palette.text_secondary)
                                        })
                                        .child(phase_detail),
                                )
                            }),
                    )
                    .when(side_by_side, |this| {
                        this.child(
                            div()
                                .h(px(attempt_panel_height))
                                .flex_none()
                                .flex()
                                .flex_col()
                                .rounded(px(INNER_RADIUS))
                                .border_1()
                                .border_color(rgb(palette.border_soft))
                                .bg(rgb(palette.surface_alt))
                                .child(animated_attempt_route(
                                    self.snapshot.attempt_count,
                                    self.snapshot.consecutive_retries,
                                    self.snapshot.phase,
                                    phase_accent,
                                    self.route_motion_frame,
                                    palette,
                                )),
                        )
                        .child(
                            div()
                                .h(px(attempt_panel_height))
                                .flex_none()
                                .flex()
                                .flex_col()
                                .rounded(px(INNER_RADIUS))
                                .border_1()
                                .border_color(rgb(palette.border_soft))
                                .bg(rgb(palette.surface_alt))
                                .child(attempt_grid(
                                    self.snapshot.attempt_count,
                                    phase_accent,
                                    palette,
                                )),
                        )
                    }),
            );

        let dashboard = if side_by_side {
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .h_full()
                        .flex()
                        .flex_col()
                        .child(status_card),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .h_full()
                        .flex()
                        .flex_col()
                        .child(conversation_card),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_col()
                .gap_3()
                .child(status_card)
                .child(conversation_card)
                .into_any_element()
        };

        let advanced_view = if self.advanced_open {
            let auto_send_handler = view_entity.clone();
            let auto_send_switch =
                Switch::new(self.auto_send_toggle.clone()).on_change(move |change, _, app| {
                    auto_send_handler.update(app, |view, cx| {
                        view.set_auto_send_default(change.current, cx)
                    });
                });
            let close_handler = view_entity.clone();
            let close_button = icon_action_button(
                "close-settings",
                IconName::Close,
                rgb(palette.text_secondary),
                palette,
            )
            .on_click(move |_, window, app| {
                close_handler.update(app, |view, cx| {
                    let _ = window;
                    view.toggle_advanced_now(cx);
                });
            });
            let default_prompt_field = div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(label("启动默认消息", palette))
                        .child(div().flex_1())
                        .child(
                            div()
                                .mr_2()
                                .text_xs()
                                .text_color(rgb(palette.text_muted))
                                .child("启动后自动发送"),
                        )
                        .child(auto_send_switch),
                )
                .child(rounded_field(self.default_prompt_input.clone(), palette));
            let secondary_fields = if viewport.width >= px(720.) {
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(label("工作目录（可选）", palette))
                            .child(rounded_field(self.cwd_input.clone(), palette)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(label("繁忙提示短语（用 | 分隔）", palette))
                            .child(rounded_field(self.phrases_input.clone(), palette)),
                    )
            } else {
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(label("工作目录（可选）", palette))
                    .child(rounded_field(self.cwd_input.clone(), palette))
                    .child(label("繁忙提示短语（用 | 分隔）", palette))
                    .child(rounded_field(self.phrases_input.clone(), palette))
            };
            let fields = div()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .rounded(px(INNER_RADIUS))
                .border_1()
                .border_color(rgb(palette.border_soft))
                .bg(rgb(palette.surface_alt))
                .child(default_prompt_field)
                .child(secondary_fields);
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .bg(rgba(palette.overlay))
                .flex()
                .items_center()
                .justify_center()
                .p_3()
                .child(
                    div()
                        .w_full()
                        .max_w(px(820.))
                        .rounded(px(PANEL_RADIUS))
                        .overflow_hidden()
                        .border_1()
                        .border_color(rgb(palette.border))
                        .bg(rgb(palette.surface))
                        .child(
                            div()
                                .h(px(64.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .px_4()
                                .border_b_1()
                                .border_color(rgb(palette.border_soft))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_base()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(palette.text))
                                                .child("设置"),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(palette.text_muted))
                                                .child("启动、提醒与运行诊断"),
                                        ),
                                )
                                .child(close_button),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .p_3()
                                .child(fields)
                                .child(if viewport.width >= px(720.) {
                                    div()
                                        .flex()
                                        .gap_2()
                                        .child(setting_tile(
                                            "全屏强提示",
                                            "成功或错误时闪屏",
                                            IconName::Brightness1,
                                            full_screen_switch,
                                            self.config.full_screen_flash_enabled,
                                            None,
                                            palette,
                                        ))
                                        .child(setting_tile(
                                            "成功音效提示",
                                            "完成后循环响铃",
                                            IconName::SoundHighFilled,
                                            audio_switch,
                                            self.config.audio_alert_enabled,
                                            None,
                                            palette,
                                        ))
                                        .into_any_element()
                                } else {
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(setting_row(
                                            "全屏强提示",
                                            "成功或错误时闪屏",
                                            IconName::Brightness1,
                                            full_screen_switch,
                                            self.config.full_screen_flash_enabled,
                                            None,
                                            palette,
                                        ))
                                        .child(setting_row(
                                            "成功音效提示",
                                            "完成后循环响铃",
                                            IconName::SoundHighFilled,
                                            audio_switch,
                                            self.config.audio_alert_enabled,
                                            None,
                                            palette,
                                        ))
                                        .into_any_element()
                                })
                                .child(diagnostic_card(
                                    &self.codex_cli.path_label(),
                                    diagnostic_ok,
                                    &cli_status,
                                    &transport_status,
                                    diagnostic_actions,
                                    palette,
                                )),
                        ),
                )
        } else {
            div()
        };

        let titlebar = titlebar(self.logo_image.clone(), palette);
        let keyboard_handler = view_entity.clone();
        let content = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(palette.canvas))
            .when(cfg!(target_os = "windows"), |this| this.child(titlebar))
            .child(
                div()
                    .id("longwatch-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .w_full()
                            .px_5()
                            .pt_3()
                            .pb_3()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .pb_2()
                                    .border_b_1()
                                    .border_color(rgb(palette.border_soft))
                                    .child(
                                        div()
                                            .text_xl()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(rgb(palette.text))
                                            .child("STAY WITH THE TASK."),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .h(px(34.))
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .px_3()
                                            .rounded_lg()
                                            .border_1()
                                            .border_color(rgb(palette.border))
                                            .bg(rgb(palette.surface))
                                            .child(div().w(px(7.)).h(px(7.)).rounded_full().bg(
                                                if diagnostic_ok {
                                                    rgb(palette.brand)
                                                } else {
                                                    rgb(palette.error)
                                                },
                                            ))
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(rgb(palette.text_secondary))
                                                    .child(cli_status),
                                            )
                                            .child(
                                                div().w(px(1.)).h(px(14.)).bg(rgb(palette.divider)),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(palette.text_muted))
                                                    .child(format!(
                                                        "v{}",
                                                        env!("CARGO_PKG_VERSION")
                                                    )),
                                            ),
                                    )
                                    .child(settings_button),
                            )
                            .child(dashboard),
                    ),
            )
            .child(advanced_view)
            .capture_key_down(move |event, window, app| {
                let key = event.keystroke.key.as_str();
                let is_enter = matches!(key, "enter" | "return");
                let prompt_input = keyboard_handler.read(app).prompt_input.clone();
                let prompt_focused = prompt_input.focus_handle(app).is_focused(window);
                let prompt_composing = prompt_input.update(app, |state, cx| {
                    <InputState as EntityInputHandler>::marked_text_range(state, window, cx)
                        .is_some()
                });
                let advanced_open = keyboard_handler.read(app).advanced_open;
                let plain_prompt_enter = is_enter
                    && prompt_focused
                    && !prompt_composing
                    && !event.keystroke.modifiers.modified();
                let shortcut_enter = is_enter
                    && event.keystroke.modifiers.secondary()
                    && !event.keystroke.modifiers.shift
                    && !(prompt_focused && prompt_composing)
                    && !advanced_open;
                if plain_prompt_enter || shortcut_enter {
                    let active = matches!(
                        keyboard_handler.read(app).snapshot.phase,
                        QueuePhase::Connecting
                            | QueuePhase::Sending
                            | QueuePhase::Waiting
                            | QueuePhase::Backoff
                    );
                    let prompt = keyboard_handler
                        .read(app)
                        .prompt_input
                        .read(app)
                        .value()
                        .to_string();
                    let cwd = keyboard_handler
                        .read(app)
                        .cwd_input
                        .read(app)
                        .value()
                        .to_string();
                    let phrases = keyboard_handler
                        .read(app)
                        .phrases_input
                        .read(app)
                        .value()
                        .to_string();
                    keyboard_handler.update(app, |view, cx| {
                        if active {
                            view.queue_message_now(prompt, cwd, phrases, cx);
                        } else {
                            view.start_now(prompt, cwd, phrases, cx);
                        }
                        cx.notify();
                    });
                    app.stop_propagation();
                } else if key == "escape" && keyboard_handler.read(app).advanced_open {
                    keyboard_handler.update(app, |view, cx| {
                        view.toggle_advanced_now(cx);
                    });
                    app.stop_propagation();
                }
            });

        let config = TDesignConfig::new()
            .theme_mode(ThemeMode::System)
            .theme_overrides(ThemeOverrides::new().brand(rgb(palette.brand)));
        TDesignRoot::with_config(config).child(content)
    }
}

fn titlebar(logo_image: Arc<Image>, palette: UiPalette) -> gpui::Div {
    div()
        .h(px(32.))
        .flex_none()
        .flex()
        .items_center()
        .border_b_1()
        .border_color(rgb(palette.divider))
        .bg(rgb(palette.surface))
        .child(
            div()
                .h_full()
                .flex_1()
                .px_3()
                .flex()
                .items_center()
                .window_control_area(WindowControlArea::Drag)
                .child(
                    div()
                        .w(px(18.))
                        .h(px(18.))
                        .rounded(px(4.))
                        .overflow_hidden()
                        .child(img(logo_image).size_full()),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(palette.text_secondary))
                        .child("Longwatch"),
                ),
        )
        .child(titlebar_control(
            "−",
            WindowControlArea::Min,
            false,
            palette,
        ))
        .child(titlebar_control(
            "×",
            WindowControlArea::Close,
            true,
            palette,
        ))
}

fn titlebar_control(
    symbol: &'static str,
    area: WindowControlArea,
    close: bool,
    palette: UiPalette,
) -> gpui::Div {
    div()
        .w(px(44.))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .window_control_area(area)
        .text_size(px(14.))
        .text_color(rgb(palette.text_secondary))
        .hover(move |element| {
            if close {
                element.bg(rgb(0xd54941)).text_color(rgb(0xffffff))
            } else {
                element.bg(rgb(palette.hover)).text_color(rgb(palette.text))
            }
        })
        .child(symbol)
}

fn setting_row(
    title: &'static str,
    description: &'static str,
    symbol: IconName,
    switch: impl IntoElement,
    enabled: bool,
    badge: Option<&'static str>,
    palette: UiPalette,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .p_3()
        .rounded(px(INNER_RADIUS))
        .border_1()
        .border_color(rgb(palette.border_soft))
        .bg(rgb(palette.surface))
        .child(
            div()
                .w(px(34.))
                .h(px(34.))
                .flex_none()
                .rounded(px(6.))
                .bg(if enabled {
                    rgb(palette.brand_soft)
                } else {
                    rgb(palette.surface_alt)
                })
                .flex()
                .items_center()
                .justify_center()
                .text_color(if enabled {
                    rgb(palette.brand)
                } else {
                    rgb(palette.text_muted)
                })
                .child(Icon::new(symbol).size(px(17.)).color(if enabled {
                    rgb(palette.brand)
                } else {
                    rgb(palette.text_muted)
                })),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title)
                        .when_some(badge, |this, badge| {
                            this.child(setting_badge(badge, palette))
                        }),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(palette.text_muted))
                        .child(description),
                ),
        )
        .child(switch)
}

fn setting_tile(
    title: &'static str,
    description: &'static str,
    symbol: IconName,
    switch: impl IntoElement,
    enabled: bool,
    badge: Option<&'static str>,
    palette: UiPalette,
) -> impl IntoElement {
    div()
        .flex_1()
        .min_w(px(0.))
        .flex()
        .items_center()
        .gap_2()
        .p_2()
        .rounded(px(INNER_RADIUS))
        .border_1()
        .border_color(rgb(palette.border_soft))
        .bg(rgb(palette.surface))
        .child(
            div()
                .w(px(30.))
                .h(px(30.))
                .flex_none()
                .rounded(px(6.))
                .bg(if enabled {
                    rgb(palette.brand_soft)
                } else {
                    rgb(palette.surface_alt)
                })
                .flex()
                .items_center()
                .justify_center()
                .text_color(if enabled {
                    rgb(palette.brand)
                } else {
                    rgb(palette.text_muted)
                })
                .child(Icon::new(symbol).size(px(16.)).color(if enabled {
                    rgb(palette.brand)
                } else {
                    rgb(palette.text_muted)
                })),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title)
                        .when_some(badge, |this, badge| {
                            this.child(setting_badge(badge, palette))
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(palette.text_muted))
                        .child(description),
                ),
        )
        .child(switch)
}

fn setting_badge(text: &'static str, palette: UiPalette) -> impl IntoElement {
    div()
        .h(px(16.))
        .flex_none()
        .px_1()
        .rounded(px(3.))
        .bg(rgb(palette.brand_soft))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(10.))
        .font_weight(FontWeight::NORMAL)
        .text_color(rgb(palette.brand))
        .child(text)
}

fn settings_action_button(
    id: &'static str,
    label: &'static str,
    icon: IconName,
    color: gpui::Rgba,
    palette: UiPalette,
) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .focusable()
        .h(px(30.))
        .flex_none()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .rounded(px(6.))
        .text_xs()
        .text_color(color)
        .cursor_pointer()
        .hover(move |this| this.bg(rgb(palette.hover)))
        .child(Icon::new(icon).size(px(14.)).color(color))
        .child(label)
}

fn diagnostic_card(
    diagnostic: &str,
    ok: bool,
    cli_status: &str,
    transport_status: &str,
    action: impl IntoElement,
    palette: UiPalette,
) -> impl IntoElement {
    let diagnostic = diagnostic.to_owned();
    let cli_status = cli_status.to_owned();
    let transport_status = transport_status.to_owned();
    div()
        .rounded(px(INNER_RADIUS))
        .border_1()
        .border_color(rgb(palette.border_soft))
        .bg(rgb(palette.surface))
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(rgb(palette.border_soft))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(palette.text))
                                .child("运行诊断"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(palette.text_muted))
                                .child(cli_status),
                        ),
                )
                .child(
                    Tag::new(if ok { "运行正常" } else { "需要检查" }).theme(if ok {
                        TagTheme::Success
                    } else {
                        TagTheme::Danger
                    }),
                )
                .child(action),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .text_xs()
                .text_color(rgb(palette.text_secondary))
                .child(format!("Codex CLI · {diagnostic}"))
                .child(format!("连接通道 · {transport_status}"))
                .child("后台窗口已隐藏，提醒行为按开关执行"),
        )
}

fn label(text: &'static str, palette: UiPalette) -> impl IntoElement {
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(palette.text_secondary))
        .child(text)
}

fn icon_action_button(
    id: &'static str,
    icon: IconName,
    icon_color: gpui::Rgba,
    palette: UiPalette,
) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .focusable()
        .w(px(28.))
        .h(px(28.))
        .flex_none()
        .rounded(px(7.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_color(icon_color)
        .hover(move |this| this.bg(rgb(palette.hover)))
        .child(Icon::new(icon).size(px(17.)).color(icon_color))
}

fn rounded_field(state: Entity<InputState>, palette: UiPalette) -> impl IntoElement {
    div()
        .id(("rounded-field", state.entity_id()))
        .w_full()
        .rounded(px(INNER_RADIUS))
        .overflow_hidden()
        .border_1()
        .border_color(rgb(palette.border))
        .bg(rgb(palette.surface))
        .child(
            ThemedInput::new(&state)
                .appearance(false)
                .bordered(false)
                .focus_bordered(false),
        )
}

fn composer_field(state: Entity<InputState>, palette: UiPalette) -> impl IntoElement {
    div()
        .id(("composer-field", state.entity_id()))
        .size_full()
        .overflow_hidden()
        .bg(rgb(palette.surface))
        .child(
            ThemedInput::new(&state)
                .appearance(false)
                .bordered(false)
                .focus_bordered(false)
                .h_full(),
        )
}

fn chat_text_view_style(palette: UiPalette) -> TextViewStyle {
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

fn status_icon_name(snapshot: &QueueSnapshot) -> IconName {
    match snapshot.phase {
        QueuePhase::Idle => IconName::Play,
        QueuePhase::Connecting => IconName::Link,
        QueuePhase::Sending => IconName::SendFilled,
        QueuePhase::Waiting if is_retry_status(&snapshot.status_message) => IconName::Refresh,
        QueuePhase::Waiting => IconName::Loading,
        QueuePhase::Backoff => IconName::HourglassFilled,
        QueuePhase::Success => IconName::CheckCircleFilled,
        QueuePhase::Paused => IconName::Pause,
        QueuePhase::FatalError => IconName::ErrorCircleFilled,
    }
}

fn status_icon(
    snapshot: &QueueSnapshot,
    accent: gpui::Rgba,
    palette: UiPalette,
) -> impl IntoElement {
    let phase = snapshot.phase;
    let icon = status_icon_name(snapshot);
    let icon_size = match phase {
        QueuePhase::Success => 35.,
        QueuePhase::FatalError => 31.,
        QueuePhase::Waiting => 29.,
        _ => 28.,
    };
    let icon_element = match phase {
        QueuePhase::Idle => svg()
            .path(icon.asset_path())
            .w(px(icon_size))
            .h(px(icon_size))
            .text_color(accent)
            .with_animation(
                "status-icon-idle",
                Animation::new(Duration::from_millis(2200)).repeat(),
                |icon, delta| {
                    let wave = 0.5 + 0.5 * (delta * std::f32::consts::TAU).sin();
                    let scale = 0.96 + 0.04 * wave;
                    icon.opacity(0.82 + 0.18 * wave)
                        .with_transformation(Transformation::scale(size(scale, scale)))
                },
            )
            .into_any_element(),
        QueuePhase::Connecting => svg()
            .path(icon.asset_path())
            .w(px(icon_size))
            .h(px(icon_size))
            .text_color(accent)
            .with_animation(
                "status-icon-connecting",
                Animation::new(Duration::from_millis(1500)).repeat(),
                |icon, delta| {
                    let wave = (delta * std::f32::consts::TAU).sin();
                    let scale = 0.98 + 0.025 * wave.abs();
                    icon.with_transformation(
                        Transformation::rotate(percentage(0.018 * wave))
                            .with_scaling(size(scale, scale)),
                    )
                },
            )
            .into_any_element(),
        QueuePhase::Sending => svg()
            .path(icon.asset_path())
            .w(px(icon_size))
            .h(px(icon_size))
            .text_color(accent)
            .with_animation(
                "status-icon-sending",
                Animation::new(Duration::from_millis(1200)).repeat(),
                |icon, delta| {
                    let lift = 0.5 - 0.5 * (delta * std::f32::consts::TAU).cos();
                    icon.opacity(0.82 + 0.18 * lift).with_transformation(
                        Transformation::translate(point(px(2.2 * lift), px(-2.2 * lift)))
                            .with_scaling(size(0.98 + 0.025 * lift, 0.98 + 0.025 * lift)),
                    )
                },
            )
            .into_any_element(),
        QueuePhase::Waiting => svg()
            .path(icon.asset_path())
            .w(px(icon_size))
            .h(px(icon_size))
            .text_color(accent)
            .with_animation(
                "status-icon-waiting",
                Animation::new(Duration::from_millis(900)).repeat(),
                |icon, delta| icon.with_transformation(Transformation::rotate(percentage(delta))),
            )
            .into_any_element(),
        QueuePhase::Backoff => svg()
            .path(icon.asset_path())
            .w(px(icon_size))
            .h(px(icon_size))
            .text_color(accent)
            .with_animation(
                "status-icon-backoff",
                Animation::new(Duration::from_millis(2600)).repeat(),
                |icon, delta| {
                    let flip = if delta < 0.42 {
                        0.
                    } else if delta < 0.68 {
                        let progress = ((delta - 0.42) / 0.26).clamp(0., 1.);
                        let eased = progress * progress * (3. - 2. * progress);
                        0.5 * eased
                    } else {
                        0.5
                    };
                    icon.with_transformation(Transformation::rotate(percentage(flip)))
                },
            )
            .into_any_element(),
        QueuePhase::Success => svg()
            .path(icon.asset_path())
            .w(px(icon_size))
            .h(px(icon_size))
            .text_color(accent)
            .into_any_element(),
        QueuePhase::Paused => svg()
            .path(icon.asset_path())
            .w(px(icon_size))
            .h(px(icon_size))
            .text_color(accent)
            .with_animation(
                "status-icon-paused",
                Animation::new(Duration::from_millis(2400)).repeat(),
                |icon, delta| {
                    let wave = 0.5 + 0.5 * (delta * std::f32::consts::TAU).sin();
                    let scale = 0.98 + 0.02 * wave;
                    icon.opacity(0.74 + 0.2 * wave)
                        .with_transformation(Transformation::scale(size(scale, scale)))
                },
            )
            .into_any_element(),
        QueuePhase::FatalError => svg()
            .path(icon.asset_path())
            .w(px(icon_size))
            .h(px(icon_size))
            .text_color(accent)
            .with_animation(
                "status-icon-error",
                Animation::new(Duration::from_millis(420)),
                |icon, delta| {
                    let shake = (delta * std::f32::consts::TAU * 3.).sin() * (1. - delta) * 2.4;
                    icon.with_transformation(Transformation::translate(point(px(shake), px(0.))))
                },
            )
            .into_any_element(),
    };
    let halo = div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .rounded_full()
        .bg(status_icon_halo(phase, palette));
    let halo = if matches!(
        phase,
        QueuePhase::Idle
            | QueuePhase::Connecting
            | QueuePhase::Sending
            | QueuePhase::Waiting
            | QueuePhase::Backoff
            | QueuePhase::Paused
    ) {
        halo.with_animation(
            "status-icon-halo",
            Animation::new(Duration::from_millis(1900)).repeat(),
            |halo, delta| {
                let wave = 0.5 + 0.5 * (delta * std::f32::consts::TAU).sin();
                halo.opacity(0.45 + 0.4 * wave)
            },
        )
        .into_any_element()
    } else {
        halo.into_any_element()
    };
    div()
        .relative()
        .w(px(58.))
        .h(px(58.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(halo)
        .child(
            div()
                .relative()
                .w(px(46.))
                .h(px(46.))
                .rounded_full()
                .border_1()
                .border_color(status_icon_border(phase, palette))
                .bg(status_icon_surface(phase, palette))
                .flex()
                .items_center()
                .justify_center()
                .child(icon_element),
        )
}

fn status_icon_surface(phase: QueuePhase, palette: UiPalette) -> gpui::Rgba {
    if palette.dark {
        return match phase {
            QueuePhase::Connecting
            | QueuePhase::Sending
            | QueuePhase::Waiting
            | QueuePhase::Backoff => rgb(palette.warning_soft),
            QueuePhase::Success => rgb(palette.success_soft),
            QueuePhase::FatalError => rgb(palette.error_soft),
            QueuePhase::Idle | QueuePhase::Paused => rgb(palette.surface),
        };
    }
    match phase {
        QueuePhase::Connecting
        | QueuePhase::Sending
        | QueuePhase::Waiting
        | QueuePhase::Backoff => rgba(0xfffcf9ff),
        QueuePhase::Idle | QueuePhase::Success | QueuePhase::Paused => rgba(0xffffffff),
        QueuePhase::FatalError => rgba(0xfffbfaff),
    }
}

fn status_icon_border(phase: QueuePhase, palette: UiPalette) -> gpui::Rgba {
    if palette.dark {
        return status_surface_border(phase, palette);
    }
    match phase {
        QueuePhase::Idle => rgb(0xd5dcda),
        QueuePhase::Connecting
        | QueuePhase::Sending
        | QueuePhase::Waiting
        | QueuePhase::Backoff => rgb(0xf0c7a7),
        QueuePhase::Success => rgb(0xb9dfcf),
        QueuePhase::Paused => rgb(0xd4d7d6),
        QueuePhase::FatalError => rgb(0xe9b7b0),
    }
}

fn status_icon_halo(phase: QueuePhase, palette: UiPalette) -> gpui::Rgba {
    if palette.dark {
        return match phase {
            QueuePhase::Idle => rgba(0xaab8b02e),
            QueuePhase::Connecting
            | QueuePhase::Sending
            | QueuePhase::Waiting
            | QueuePhase::Backoff => rgba(0xe9a23b35),
            QueuePhase::Success => rgba(0x43c99738),
            QueuePhase::Paused => rgba(0xaab8b029),
            QueuePhase::FatalError => rgba(0xff958a33),
        };
    }
    match phase {
        QueuePhase::Idle => rgba(0x66736f24),
        QueuePhase::Connecting
        | QueuePhase::Sending
        | QueuePhase::Waiting
        | QueuePhase::Backoff => rgba(0xe373182b),
        QueuePhase::Success => rgba(0x16a67340),
        QueuePhase::Paused => rgba(0x77777720),
        QueuePhase::FatalError => rgba(0xd5494126),
    }
}

fn success_completion_background() -> impl IntoElement {
    const COLUMNS: usize = 15;
    const ROWS: usize = 5;

    // Preserve the original subtle dot matrix. Celebration motion is layered
    // above it and never replaces or recolors this texture.
    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .children((0..COLUMNS * ROWS).map(move |index| {
            let column = index % COLUMNS;
            let row = index / COLUMNS;
            let pattern = (column * 7 + row * 11) % 17;
            let alpha = match pattern {
                0 | 1 => 0x3a,
                2..=5 => 0x2a,
                6..=10 => 0x20,
                _ => 0x15,
            };
            let x = 24. + column as f32 * 28. + if row % 2 == 0 { 0. } else { 4. };
            let y = 20. + row as f32 * 28.;
            div()
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(3.))
                .h(px(3.))
                .rounded_full()
                .bg(rgba(0x16a67300 | alpha))
                .into_any_element()
        }))
        .with_animation(
            "success-dot-shimmer",
            Animation::new(Duration::from_millis(2800)).repeat(),
            |this, delta| {
                let shimmer = 0.5 + 0.5 * (delta * std::f32::consts::TAU).sin();
                this.opacity(0.56 + 0.44 * shimmer)
            },
        )
}

fn success_feedback_overlay(burst_id: u64) -> impl IntoElement {
    const WIDTH: f32 = 420.;
    const HEIGHT: f32 = 180.;
    const SPARKS: [(f32, f32, f32, u32, f32); 24] = [
        (0.05, 18., 0.00, 0x2f9c78, 5.),
        (0.13, 76., 0.18, 0x58aeb0, 4.),
        (0.21, 132., 0.34, 0xd7a13d, 5.),
        (0.29, 42., 0.48, 0x2f9c78, 4.),
        (0.36, 108., 0.12, 0x58aeb0, 5.),
        (0.44, 22., 0.28, 0xd7a13d, 4.),
        (0.51, 146., 0.42, 0x2f9c78, 5.),
        (0.58, 34., 0.56, 0x58aeb0, 4.),
        (0.64, 118., 0.08, 0x2f9c78, 5.),
        (0.71, 58., 0.22, 0xd7a13d, 4.),
        (0.78, 142., 0.38, 0x58aeb0, 5.),
        (0.85, 28., 0.52, 0x2f9c78, 4.),
        (0.92, 96., 0.14, 0xd7a13d, 5.),
        (0.08, 126., 0.30, 0x58aeb0, 4.),
        (0.24, 12., 0.46, 0x2f9c78, 4.),
        (0.76, 14., 0.06, 0x58aeb0, 4.),
        (0.89, 134., 0.26, 0x2f9c78, 4.),
        (0.97, 66., 0.44, 0xd7a13d, 5.),
        (0.18, 162., 0.20, 0x2f9c78, 4.),
        (0.32, 92., 0.40, 0x58aeb0, 5.),
        (0.48, 154., 0.58, 0xd7a13d, 4.),
        (0.62, 8., 0.16, 0x2f9c78, 5.),
        (0.74, 52., 0.32, 0x58aeb0, 4.),
        (0.86, 116., 0.50, 0xd7a13d, 5.),
    ];

    div()
        .absolute()
        .left(px(-(WIDTH - 58.) / 2.))
        .top(px(-(HEIGHT - 58.) / 2.))
        .w(px(WIDTH))
        .h(px(HEIGHT))
        .children(
            SPARKS
                .into_iter()
                .enumerate()
                .map(move |(index, (x, y, delay, color, size))| {
                    let animation_key = burst_id.wrapping_mul(64).wrapping_add(index as u64);
                    let cross = index % 3 != 1;
                    let particle_size = size * 1.4;
                    let stroke = 1.6;
                    div()
                        .absolute()
                        .left(relative(x))
                        .top(px(y))
                        .w(px(particle_size))
                        .h(px(particle_size))
                        .when(cross, |this| {
                            this.child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .top(px((particle_size - stroke) / 2.))
                                    .w_full()
                                    .h(px(stroke))
                                    .rounded_full()
                                    .bg(rgb(color)),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .left(px((particle_size - stroke) / 2.))
                                    .top_0()
                                    .w(px(stroke))
                                    .h_full()
                                    .rounded_full()
                                    .bg(rgb(color)),
                            )
                        })
                        .when(!cross, |this| this.rounded_full().bg(rgb(color)))
                        .with_animation(
                            ("success-spark", animation_key),
                            Animation::new(Duration::from_millis(2400)).repeat(),
                            move |this, delta| {
                                let progress = (delta + delay).fract();
                                let pulse =
                                    (std::f32::consts::PI * progress).sin().max(0.).powf(0.72);
                                this.opacity(0.92 * pulse)
                            },
                        )
                        .into_any_element()
                }),
        )
}

fn metric_panel_heading(
    title: &'static str,
    summary: Option<String>,
    accent: gpui::Rgba,
    palette: UiPalette,
) -> gpui::Div {
    let heading = div()
        .h(px(36.))
        .flex_none()
        .flex()
        .items_start()
        .gap_2()
        .px_3()
        .pt_2()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(palette.text))
                .child(title),
        )
        .child(div().flex_1());
    if let Some(summary) = summary {
        heading.child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(accent)
                .child(summary),
        )
    } else {
        heading
    }
}

fn animated_attempt_route(
    attempt_count: u64,
    retry_count: u32,
    phase: QueuePhase,
    accent: gpui::Rgba,
    motion_frame: u8,
    palette: UiPalette,
) -> impl IntoElement {
    let stage: usize = match phase {
        QueuePhase::Connecting => 1,
        QueuePhase::Sending => 2,
        QueuePhase::Waiting | QueuePhase::Backoff => 3,
        QueuePhase::Success => 4,
        QueuePhase::Paused | QueuePhase::FatalError if attempt_count > 0 => 3,
        QueuePhase::Idle | QueuePhase::Paused | QueuePhase::FatalError => 0,
    };
    let running = matches!(
        phase,
        QueuePhase::Connecting | QueuePhase::Sending | QueuePhase::Waiting | QueuePhase::Backoff
    );
    let pulse_on = running && motion_frame % 2 == 0;
    let route_progress = stage as f32 / 4.;

    div()
        .size_full()
        .flex()
        .flex_col()
        .child(metric_panel_heading(
            "任务进度",
            Some(format!("尝试 {attempt_count} 次 · 重试 {retry_count} 轮")),
            accent,
            palette,
        ))
        .child(
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .items_center()
                .px_3()
                .pb_3()
                .child(
                    div()
                        .w_full()
                        .relative()
                        .flex()
                        .child(
                            div()
                                .absolute()
                                .left(relative(0.1))
                                .right(relative(0.1))
                                .top(px(7.5))
                                .h(px(3.))
                                .bg(rgb(palette.inactive_border))
                                .child(div().h_full().w(relative(route_progress)).bg(accent)),
                        )
                        .children(
                            ["开始", "连接", "发送", "处理", "完成"]
                                .into_iter()
                                .enumerate()
                                .map(move |(index, label)| {
                                    let completed = index < stage
                                        || (phase == QueuePhase::Success && index == 4);
                                    let current = index == stage;
                                    let active = completed || current;
                                    let dot_size = if current && pulse_on { 16. } else { 14. };
                                    div()
                                        .flex_1()
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w_full()
                                                .flex()
                                                .items_center()
                                                .child(div().flex_1().h(px(3.)))
                                                .child(
                                                    div()
                                                        .w(px(18.))
                                                        .h(px(18.))
                                                        .flex_none()
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .child(
                                                            div()
                                                                .w(px(dot_size))
                                                                .h(px(dot_size))
                                                                .rounded_full()
                                                                .when(!active, |this| {
                                                                    this.border_1()
                                                                        .border_color(rgb(
                                                                            palette.inactive_border,
                                                                        ))
                                                                })
                                                                .bg(if active {
                                                                    accent
                                                                } else {
                                                                    rgb(palette.surface)
                                                                }),
                                                        ),
                                                )
                                                .child(div().flex_1().h(px(3.))),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(if index <= stage {
                                                    accent
                                                } else {
                                                    rgb(palette.text_muted)
                                                })
                                                .child(label),
                                        )
                                }),
                        ),
                ),
        )
}

const ATTEMPT_GRID_ROWS: u64 = 5;
const ATTEMPT_GRID_COLUMNS: u64 = 18;

const fn attempt_grid_index(row: u64, column: u64) -> u64 {
    row * ATTEMPT_GRID_COLUMNS + column
}

fn attempt_grid(attempt_count: u64, accent: gpui::Rgba, palette: UiPalette) -> impl IntoElement {
    const CELL_COUNT: u64 = ATTEMPT_GRID_ROWS * ATTEMPT_GRID_COLUMNS;
    let active_cells = attempt_count.min(CELL_COUNT);
    div()
        .w_full()
        .h_full()
        .flex()
        .flex_col()
        .child(metric_panel_heading("尝试轨迹", None, accent, palette))
        .child(
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .px_3()
                .pb_2()
                .child(div().w_full().flex().flex_col().gap_1().children(
                    (0..ATTEMPT_GRID_ROWS).map(move |row| {
                        div()
                            .w_full()
                            .flex()
                            .gap_1()
                            .children((0..ATTEMPT_GRID_COLUMNS).map(move |column| {
                                // Fill each row from left to right before moving down.
                                let index = attempt_grid_index(row, column);
                                let active = index < active_cells;
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .h(px(14.))
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(if active {
                                        accent
                                    } else {
                                        rgb(palette.inactive_border)
                                    })
                                    .bg(if active {
                                        accent
                                    } else {
                                        rgb(palette.inactive)
                                    })
                            }))
                    }),
                )),
        )
}

fn status_surface(phase: QueuePhase, palette: UiPalette) -> gpui::Rgba {
    if palette.dark {
        return match phase {
            QueuePhase::Success => rgb(palette.success_soft),
            QueuePhase::Idle | QueuePhase::Paused => rgb(palette.surface_alt),
            QueuePhase::Connecting
            | QueuePhase::Sending
            | QueuePhase::Waiting
            | QueuePhase::Backoff => rgb(palette.warning_soft),
            QueuePhase::FatalError => rgb(palette.error_soft),
        };
    }
    match phase {
        QueuePhase::Success => rgba(0xf4faf7ff),
        QueuePhase::Idle => rgba(0xf7f8f8ff),
        QueuePhase::Connecting
        | QueuePhase::Sending
        | QueuePhase::Waiting
        | QueuePhase::Backoff => rgba(0xfff7f0ff),
        QueuePhase::FatalError => rgba(0xfff5f3ff),
        QueuePhase::Paused => rgba(0xf7f7f7ff),
    }
}

fn status_surface_border(phase: QueuePhase, palette: UiPalette) -> gpui::Rgba {
    if palette.dark {
        return match phase {
            QueuePhase::Success => rgb(0x347359),
            QueuePhase::Idle | QueuePhase::Paused => rgb(palette.border),
            QueuePhase::Connecting
            | QueuePhase::Sending
            | QueuePhase::Waiting
            | QueuePhase::Backoff => rgb(0x795534),
            QueuePhase::FatalError => rgb(0x75413d),
        };
    }
    match phase {
        QueuePhase::Success => rgb(0xd3e9df),
        QueuePhase::Idle => rgb(0xdde2e0),
        QueuePhase::Connecting
        | QueuePhase::Sending
        | QueuePhase::Waiting
        | QueuePhase::Backoff => rgb(0xf2ceb2),
        QueuePhase::FatalError => rgb(0xf0c7c1),
        QueuePhase::Paused => rgb(0xdedede),
    }
}

fn tag_theme(phase: QueuePhase) -> TagTheme {
    match phase {
        QueuePhase::Success => TagTheme::Success,
        QueuePhase::Connecting
        | QueuePhase::Sending
        | QueuePhase::Waiting
        | QueuePhase::Backoff => TagTheme::Warning,
        QueuePhase::FatalError => TagTheme::Danger,
        QueuePhase::Paused | QueuePhase::Idle => TagTheme::Default,
    }
}

fn reply_tag_label(phase: QueuePhase) -> &'static str {
    match phase {
        QueuePhase::Success => "已完成",
        QueuePhase::Connecting
        | QueuePhase::Sending
        | QueuePhase::Waiting
        | QueuePhase::Backoff => "排队中",
        QueuePhase::FatalError => "需处理",
        QueuePhase::Idle | QueuePhase::Paused => "等待中",
    }
}

fn conversation_waiting_text(phase: QueuePhase) -> &'static str {
    match phase {
        QueuePhase::Connecting => "正在连接 Codex…",
        QueuePhase::Sending => "正在发送任务…",
        QueuePhase::Waiting => "正在处理，请稍候…",
        QueuePhase::Backoff => "当前服务繁忙，已安排下一次重试…",
        QueuePhase::Paused => "对话已暂停，继续后将恢复排队。",
        QueuePhase::FatalError => "当前任务需要处理，请检查状态后重新开始。",
        QueuePhase::Success => "任务已完成，正在整理最终回复。",
        QueuePhase::Idle => "等待发送任务…",
    }
}

fn conversation_text(snapshot: &QueueSnapshot) -> String {
    if !snapshot.reply_preview.trim().is_empty() {
        return snapshot.reply_preview.clone();
    }

    let status = snapshot.status_message.trim();
    let status_is_codex_output = matches!(
        snapshot.phase,
        QueuePhase::Backoff | QueuePhase::Paused | QueuePhase::FatalError
    ) || (snapshot.phase == QueuePhase::Waiting
        && is_retry_status(status));
    if status_is_codex_output && !status.is_empty() {
        status.to_owned()
    } else {
        conversation_waiting_text(snapshot.phase).to_owned()
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

fn simple_status_title(snapshot: &QueueSnapshot) -> &'static str {
    match snapshot.phase {
        QueuePhase::Idle => "等待开始",
        QueuePhase::Connecting if snapshot.status_message.contains("恢复") => "正在恢复连接",
        QueuePhase::Connecting => "正在连接 Codex",
        QueuePhase::Sending => "正在发送任务",
        QueuePhase::Waiting if is_retry_status(&snapshot.status_message) => "Codex 正在自动重试",
        QueuePhase::Waiting => "Codex 正在处理",
        QueuePhase::Backoff => "等待下一次重试",
        QueuePhase::Success => "任务已完成",
        QueuePhase::Paused => "队列已暂停",
        QueuePhase::FatalError => "运行需要处理",
    }
}

fn is_retry_status(status: &str) -> bool {
    let normalized = status.to_ascii_lowercase();
    status.contains("重试")
        || normalized.contains("reconnecting")
        || normalized.contains("disconnected")
        || normalized.contains("error sending request")
}

fn friendly_phase(phase: QueuePhase) -> &'static str {
    match phase {
        QueuePhase::Idle => "准备就绪",
        QueuePhase::Connecting => "正在连接",
        QueuePhase::Sending => "正在发送",
        QueuePhase::Waiting => "处理中",
        QueuePhase::Backoff => "稍后重试",
        QueuePhase::Success => "已完成",
        QueuePhase::Paused => "已暂停",
        QueuePhase::FatalError => "需要处理",
    }
}

fn transport_status_text(snapshot: &QueueSnapshot) -> String {
    let status = &snapshot.transport_status;
    if status.connected {
        status.server_agent.as_ref().map_or_else(
            || format!("{} · 已连接", status.kind.label()),
            |agent| format!("{} · 已连接 · {agent}", status.kind.label()),
        )
    } else if snapshot.phase == QueuePhase::Idle {
        "尚未连接 · 开始任务后自动启动 app-server".into()
    } else {
        format!("{} · 未连接，正在由队列恢复", status.kind.label())
    }
}

#[cfg(target_os = "windows")]
fn update_tray_state(snapshot: &QueueSnapshot) {
    let control = match snapshot.phase {
        QueuePhase::Connecting
        | QueuePhase::Sending
        | QueuePhase::Waiting
        | QueuePhase::Backoff => gpui_platform::TrayControl::Pause,
        QueuePhase::Paused => gpui_platform::TrayControl::Resume,
        QueuePhase::Idle | QueuePhase::Success | QueuePhase::FatalError => {
            gpui_platform::TrayControl::Disabled
        }
    };
    gpui_platform::set_tray_state(
        friendly_phase(snapshot.phase),
        control,
        snapshot.active_thread_id.is_some(),
    );
}

fn primary_action_label(phase: QueuePhase) -> &'static str {
    match phase {
        QueuePhase::Connecting
        | QueuePhase::Sending
        | QueuePhase::Waiting
        | QueuePhase::Backoff => "停止",
        QueuePhase::Paused => "继续当前会话",
        QueuePhase::Idle | QueuePhase::Success | QueuePhase::FatalError => "发送消息",
    }
}

fn phase_color(phase: QueuePhase, palette: UiPalette) -> gpui::Rgba {
    if palette.dark {
        return match phase {
            QueuePhase::Success => rgb(palette.brand),
            QueuePhase::Idle | QueuePhase::Paused => rgb(palette.text_secondary),
            QueuePhase::Connecting
            | QueuePhase::Sending
            | QueuePhase::Waiting
            | QueuePhase::Backoff => rgb(0xe9a23b),
            QueuePhase::FatalError => rgb(palette.error),
        };
    }
    match phase {
        QueuePhase::Success => rgb(0x16a673),
        QueuePhase::Idle => rgb(0x66736f),
        QueuePhase::Connecting
        | QueuePhase::Sending
        | QueuePhase::Waiting
        | QueuePhase::Backoff => rgb(0xe37318),
        QueuePhase::Paused => rgb(0x777777),
        QueuePhase::FatalError => rgb(0xd54941),
    }
}

fn phase_detail(phase: QueuePhase, next: Option<&str>, retry_round: u32) -> String {
    match phase {
        QueuePhase::Idle => "填写任务后即可开始排队".into(),
        QueuePhase::Connecting => "正在建立 Codex app-server 连接".into(),
        QueuePhase::Sending => "任务正在原样发送给 Codex".into(),
        QueuePhase::Waiting => "正在等待 Codex 完成当前回合".into(),
        QueuePhase::Backoff => next.map_or_else(
            || format!("第 {} 轮退避 · 已安排下一次低频重试", retry_round.max(1)),
            |next| format!("第 {} 轮退避 · {next}重试", retry_round.max(1)),
        ),
        QueuePhase::Success => String::new(),
        QueuePhase::Paused => "已暂停，不会自动发送新的回合".into(),
        QueuePhase::FatalError => "请检查配置或 Codex 运行状态后重新开始".into(),
    }
}

fn next_attempt_label(next: Option<chrono::DateTime<chrono::Utc>>) -> Option<String> {
    Some(next_attempt_label_at(next?, chrono::Utc::now()))
}

fn reply_time_label(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    timestamp
        .with_timezone(&chrono::Local)
        .format("%H:%M")
        .to_string()
}

fn next_attempt_label_at(
    next: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let remaining = (next - now).num_seconds().max(0);
    let local = next.with_timezone(&chrono::Local);
    let today = now.with_timezone(&chrono::Local).date_naive();
    if remaining == 0 {
        return "即将".into();
    }
    if remaining < 90 {
        return format!("约 {remaining} 秒后（{}）", local.format("%H:%M:%S"));
    }
    if remaining < 60 * 60 {
        let minutes = (remaining + 30) / 60;
        return format!("约 {minutes} 分钟后（{}）", local.format("%H:%M"));
    }
    if local.date_naive() == today {
        format!("今天 {}", local.format("%H:%M"))
    } else if local.date_naive() == today + chrono::Duration::days(1) {
        format!("明天 {}", local.format("%H:%M"))
    } else {
        local.format("%m月%d日 %H:%M").to_string()
    }
}

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

pub fn run(
    config: QueueConfig,
    runtime: RuntimeHandle,
    tokio_runtime: Arc<tokio::runtime::Runtime>,
    startup_auto_sent: bool,
) {
    let shutdown_runtime = Arc::clone(&tokio_runtime);
    let commands = runtime.command_sender();
    let mut snapshots = runtime.snapshot();
    let codex_cli = diagnostics::inspect_codex_cli(&config.codex_path);
    let config_directory = ConfigStore::discover()
        .map(|store| store.directory().to_path_buf())
        .unwrap_or_else(|_| PathBuf::from("."));
    let logs_directory = config_directory.join("logs");
    Application::new()
        .with_assets(TDesignAssetSource::new())
        .run(move |cx: &mut App| {
            tdesign_gpui::init(cx);
            // Markdown replies use gpui-component's TextView. Initialize its
            // key bindings/theme globals alongside the existing TDesign shell.
            gpui_component::init(cx);
            let closing = Arc::new(AtomicBool::new(false));
            let titlebar_inset = if cfg!(target_os = "windows") { 32. } else { 0. };
            let window_size = size(px(1000.), px(612. + titlebar_inset));
            let bounds = Bounds::centered(None, window_size, cx);
            #[cfg(target_os = "windows")]
            let initial_for_tray = snapshots.borrow().clone();
            let view_closing = Arc::clone(&closing);
            let queue_view_slot = Rc::new(RefCell::new(None));
            let queue_view_slot_for_window = Rc::clone(&queue_view_slot);
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
                        is_resizable: false,
                        window_min_size: Some(window_size),
                        ..Default::default()
                    },
                    {
                        let config = config.clone();
                        let commands = commands.clone();
                        let initial = snapshots.borrow().clone();
                        let initial_burst_id = u64::from(initial.phase == QueuePhase::Success);
                        let codex_cli = codex_cli.clone();
                        let config_directory = config_directory.clone();
                        let logs_directory = logs_directory.clone();
                        move |app_window, cx| {
                            let logo_image = Arc::new(Image::from_bytes(
                                ImageFormat::Png,
                                include_bytes!("../packaging/ui-logo.png").to_vec(),
                            ));
                            let prompt_value = if startup_auto_sent {
                                String::new()
                            } else {
                                config.default_prompt.clone()
                            };
                            let prompt_input = cx.new(|cx| {
                                InputState::new(app_window, cx)
                                    .multi_line(true)
                                    .default_value(prompt_value)
                                    .placeholder("输入一条真实、有意义的任务")
                            });
                            let default_prompt_input = cx.new(|cx| {
                                InputState::new(app_window, cx)
                                    .default_value(config.default_prompt.clone())
                                    .placeholder("留空则每次启动显示空输入框")
                            });
                            let cwd_value = config
                                .working_directory
                                .as_ref()
                                .map_or_else(String::new, |path| path.display().to_string());
                            let cwd_input = cx.new(|cx| {
                                InputState::new(app_window, cx)
                                    .default_value(cwd_value)
                                    .placeholder("留空则继承 Codex 默认工作目录")
                            });
                            let phrases_input = cx.new(|cx| {
                                InputState::new(app_window, cx)
                                    .default_value(config.failure_phrases.join(" | "))
                                    .placeholder("Server overloaded; retry later...")
                            });
                            let auto_send_toggle =
                                ToggleState::new(cx, config.auto_send_default_prompt);
                            let full_screen_toggle =
                                ToggleState::new(cx, config.full_screen_flash_enabled);
                            let audio_toggle = ToggleState::new(cx, config.audio_alert_enabled);
                            let view = cx.new(|_| QueueView {
                                celebration_burst_id: initial_burst_id,
                                config,
                                snapshot: initial,
                                commands,
                                prompt_input,
                                default_prompt_input,
                                cwd_input,
                                phrases_input,
                                auto_send_toggle,
                                full_screen_toggle,
                                audio_toggle,
                                logo_image,
                                codex_cli,
                                advanced_open: false,
                                route_motion_frame: 0,
                                prompt_error: false,
                                next_request_id: 0,
                                pending_clear_request: None,
                                pending_clear_text: None,
                                conversation_scroll: ScrollHandle::new(),
                                copy_feedback: CopyFeedback::Idle,
                                diagnostic_copy_feedback: CopyFeedback::Idle,
                                config_directory,
                                logs_directory,
                                appearance_subscription: None,
                            });
                            let animated_view = view.clone();
                            let animated_closing = Arc::clone(&view_closing);
                            cx.spawn(async move |cx| {
                                loop {
                                    Timer::after(Duration::from_millis(520)).await;
                                    if animated_closing.load(Ordering::Acquire) {
                                        break;
                                    }
                                    if animated_view
                                        .update(cx, |view, cx| {
                                            #[cfg(target_os = "windows")]
                                            if let Some(action) = gpui_platform::take_tray_action()
                                            {
                                                match action {
                                                    gpui_platform::TrayAction::Pause => {
                                                        let _ = view.send(RuntimeCommand::Pause);
                                                    }
                                                    gpui_platform::TrayAction::Resume => {
                                                        let _ = view.send(RuntimeCommand::Resume);
                                                    }
                                                    gpui_platform::TrayAction::OpenResult => {
                                                        view.open_thread_now();
                                                    }
                                                }
                                                cx.notify();
                                            }
                                            if matches!(
                                                view.snapshot.phase,
                                                QueuePhase::Connecting
                                                    | QueuePhase::Sending
                                                    | QueuePhase::Waiting
                                                    | QueuePhase::Backoff
                                            ) {
                                                view.route_motion_frame =
                                                    view.route_motion_frame.wrapping_add(1);
                                                cx.notify();
                                            }
                                        })
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            })
                            .detach();
                            *queue_view_slot_for_window.borrow_mut() = Some(view.clone());
                            cx.new(|cx| ComponentRoot::new(view, app_window, cx))
                        }
                    },
                )
                .expect("failed to open Longwatch window");
            let queue_view = queue_view_slot
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
                        Ok(()) => {
                            // `window.update` already holds a mutable lease for
                            // QueueView. Reading the entity here would re-enter
                            // that lease and trigger GPUI's double-borrow panic.
                            update_tray_state(&initial_for_tray);
                            true
                        }
                        Err(error) => {
                            warn!(%error, "系统托盘初始化失败，将保留传统关闭行为");
                            false
                        }
                    }
                });
                let close_flag = Arc::clone(&closing);
                app_window.on_window_should_close(cx, move |_, app| {
                    let active = matches!(
                        view.read(app).snapshot.phase,
                        QueuePhase::Connecting
                            | QueuePhase::Sending
                            | QueuePhase::Waiting
                            | QueuePhase::Backoff
                    );
                    #[cfg(target_os = "windows")]
                    if tray_available && let Some(native_window) = native_window {
                        if gpui_platform::tray_exit_requested() {
                            if active && !gpui_platform::confirm_exit_while_running() {
                                gpui_platform::cancel_tray_exit();
                                gpui_platform::show_window_from_tray(native_window);
                                return false;
                            }
                            close_flag.store(true, Ordering::Release);
                            return true;
                        }
                        gpui_platform::hide_window_to_tray(native_window);
                        return false;
                    }
                    let should_close = !active || gpui_platform::confirm_exit_while_running();
                    if should_close {
                        close_flag.store(true, Ordering::Release);
                    }
                    should_close
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
                if window.update(cx, |_, _, _| {}).is_ok() {
                    gpui_platform::show_error_dialog(
                        "Longwatch 后台已停止",
                        "后台任务异常结束，请重新启动 Longwatch。详细原因已写入日志目录。",
                    );
                }
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
    use super::*;

    #[test]
    fn retry_error_is_kept_in_the_conversation_and_simplified_on_the_left() {
        let error = "stream disconnected before completion: error sending request for url (https://anyrouter.top/v1/responses)";
        let snapshot = QueueSnapshot {
            phase: QueuePhase::Backoff,
            status_message: format!("{error}；已安排低频重试"),
            reply_preview: error.into(),
            ..QueueSnapshot::default()
        };

        assert_eq!(conversation_text(&snapshot), error);
        assert_eq!(simple_status_title(&snapshot), "等待下一次重试");
    }

    #[test]
    fn internal_retry_status_moves_to_the_codex_message_when_no_preview_exists() {
        let status = "Codex 正在内部重试：Reconnecting... 2/5";
        let snapshot = QueueSnapshot {
            phase: QueuePhase::Waiting,
            status_message: status.into(),
            ..QueueSnapshot::default()
        };

        assert_eq!(conversation_text(&snapshot), status);
        assert_eq!(simple_status_title(&snapshot), "Codex 正在自动重试");
    }

    #[test]
    fn status_icons_use_distinct_literal_metaphors() {
        let phases = [
            (QueuePhase::Idle, IconName::Play),
            (QueuePhase::Connecting, IconName::Link),
            (QueuePhase::Sending, IconName::SendFilled),
            (QueuePhase::Waiting, IconName::Loading),
            (QueuePhase::Backoff, IconName::HourglassFilled),
            (QueuePhase::Success, IconName::CheckCircleFilled),
            (QueuePhase::Paused, IconName::Pause),
            (QueuePhase::FatalError, IconName::ErrorCircleFilled),
        ];

        for (phase, icon) in phases {
            let snapshot = QueueSnapshot {
                phase,
                ..QueueSnapshot::default()
            };
            assert_eq!(status_icon_name(&snapshot), icon);
        }

        let retrying = QueueSnapshot {
            phase: QueuePhase::Waiting,
            status_message: "Reconnecting... 1/5".into(),
            ..QueueSnapshot::default()
        };
        assert_eq!(status_icon_name(&retrying), IconName::Refresh);
    }

    #[test]
    fn near_retry_deadline_uses_a_second_level_countdown() {
        let now = chrono::Utc::now();
        let label = next_attempt_label_at(now + chrono::Duration::seconds(42), now);

        assert!(label.starts_with("约 42 秒后"));
        assert!(label.contains(':'));
    }

    #[test]
    fn minute_scale_retry_deadline_is_rounded_for_readability() {
        let now = chrono::Utc::now();
        let label = next_attempt_label_at(now + chrono::Duration::seconds(190), now);

        assert!(label.starts_with("约 3 分钟后"));
    }

    #[test]
    fn attempt_grid_lights_one_cell_at_a_time_across_each_row() {
        assert_eq!(attempt_grid_index(0, 0), 0);
        assert_eq!(attempt_grid_index(0, 1), 1);
        assert_eq!(
            attempt_grid_index(0, ATTEMPT_GRID_COLUMNS - 1),
            ATTEMPT_GRID_COLUMNS - 1
        );
        assert_eq!(attempt_grid_index(1, 0), ATTEMPT_GRID_COLUMNS);
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
