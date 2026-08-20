//! `TDesign` GPUI desktop shell.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[cfg(target_os = "macos")]
use gpui::WindowAppearance;
use gpui::{
    App, Application, Bounds, ClipboardItem, Context, Entity, FontWeight, Image, ImageFormat,
    Render, ScrollHandle, Timer, TitlebarOptions, Window, WindowBounds, WindowControlArea,
    WindowOptions, div, img, prelude::*, px, rgb, rgba, size,
};
#[cfg(target_os = "windows")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tdesign_gpui::{
    Button, ButtonVariant, Card, ComponentSize, Icon, IconName, Input, InputState, Sizable, Switch,
    TDesignAssetSource, TDesignConfig, TDesignRoot, Tag, TagTheme, Textarea, ThemeMode,
    ThemeOverrides, ToggleState,
};
use tracing::warn;

use crate::{
    config::{ConfigStore, QueueConfig},
    diagnostics::{self, CodexCliInfo},
    queue::{QueuePhase, QueueSnapshot},
    runtime::{RuntimeCommand, RuntimeHandle},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CopyFeedback {
    #[default]
    Idle,
    Copied,
}

struct QueueView {
    config: QueueConfig,
    snapshot: QueueSnapshot,
    commands: tokio::sync::mpsc::Sender<RuntimeCommand>,
    prompt_input: Entity<InputState>,
    cwd_input: Entity<InputState>,
    phrases_input: Entity<InputState>,
    gui_fallback_toggle: Entity<ToggleState>,
    full_screen_toggle: Entity<ToggleState>,
    audio_toggle: Entity<ToggleState>,
    logo_image: Arc<Image>,
    codex_cli: CodexCliInfo,
    advanced_open: bool,
    pulsing: bool,
    route_motion_frame: u8,
    prompt_error: bool,
    conversation_scroll: ScrollHandle,
    copy_feedback: CopyFeedback,
    diagnostic_copy_feedback: CopyFeedback,
    config_directory: PathBuf,
    logs_directory: PathBuf,
    #[cfg(target_os = "macos")]
    _appearance_subscription: Option<gpui::Subscription>,
}

impl QueueView {
    fn send(&self, command: RuntimeCommand) {
        // UI callbacks run on GPUI's executor, not necessarily inside a
        // Tokio reactor.  A synchronous bounded send avoids spawning a Tokio
        // future during window teardown (which used to trigger "there is no
        // reactor running" on the final tray callback).
        let _ = self.commands.try_send(command);
    }

    fn configure(&self) {
        self.send(RuntimeCommand::Configure(self.config.clone()));
    }

    fn start_now(&mut self, prompt: String, cwd: String, phrases: String) {
        if prompt.trim().is_empty() {
            self.prompt_error = true;
            return;
        }
        self.prompt_error = false;
        gpui_platform::stop_completion_overlay();
        self.config.prompt = prompt;
        self.config.working_directory = (!cwd.trim().is_empty()).then(|| PathBuf::from(cwd.trim()));
        self.config.failure_phrases = phrases
            .split('|')
            .map(str::trim)
            .filter(|phrase| !phrase.is_empty())
            .map(str::to_owned)
            .collect();
        self.send(RuntimeCommand::StartConfigured(self.config.clone()));
    }

    fn pause_now(&mut self) {
        self.send(RuntimeCommand::Pause);
    }

    fn primary_action_now(&mut self, prompt: String, cwd: String, phrases: String) {
        match self.snapshot.phase {
            QueuePhase::Connecting
            | QueuePhase::Sending
            | QueuePhase::Waiting
            | QueuePhase::Backoff => self.pause_now(),
            _ => self.start_now(prompt, cwd, phrases),
        }
    }

    fn secondary_action_now(&mut self) {
        if self.snapshot.phase == QueuePhase::Success && self.snapshot.active_thread_id.is_some() {
            self.open_thread_now();
        } else {
            self.stop_now();
        }
    }

    fn stop_now(&mut self) {
        gpui_platform::stop_completion_overlay();
        self.send(RuntimeCommand::Stop);
    }

    fn set_gui_fallback(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.config.gui_fallback_enabled = enabled;
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

    fn toggle_advanced_now(&mut self, cx: &mut Context<Self>) {
        self.advanced_open = !self.advanced_open;
        cx.notify();
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
        let became_success =
            self.snapshot.phase != QueuePhase::Success && snapshot.phase == QueuePhase::Success;
        if conversation_changed {
            self.copy_feedback = CopyFeedback::Idle;
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
        if became_success && !self.pulsing {
            self.pulsing = true;
            cx.spawn_in(window, async move |this, cx| {
                for _ in 0..10 {
                    Timer::after(Duration::from_millis(220)).await;
                    if this
                        .update_in(cx, |view, _, cx| {
                            view.pulsing = !view.pulsing;
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                let _ = this.update_in(cx, |view, _, cx| {
                    view.pulsing = false;
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }
}

impl Render for QueueView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let phase = friendly_phase(self.snapshot.phase);
        let next = next_attempt_label(self.snapshot.next_attempt_at);
        let primary_label = primary_action_label(self.snapshot.phase);
        let secondary_label = if self.snapshot.phase == QueuePhase::Success {
            "打开结果"
        } else {
            "停止"
        };
        let primary_variant = match self.snapshot.phase {
            QueuePhase::Connecting
            | QueuePhase::Sending
            | QueuePhase::Waiting
            | QueuePhase::Backoff => ButtonVariant::Warning,
            QueuePhase::FatalError => ButtonVariant::Danger,
            _ => ButtonVariant::Primary,
        };
        let phase_accent = if self.pulsing && self.snapshot.phase == QueuePhase::Success {
            rgb(0x0b7f58)
        } else {
            phase_color(self.snapshot.phase)
        };
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
        let status_panel_height = if self.snapshot.phase == QueuePhase::Success {
            if compact { 156. } else { 178. }
        } else if compact {
            128.
        } else {
            146.
        };
        let metrics_panel_height = 116.;
        let bottom_panel_height = if compact { 104. } else { 138. };
        let side_by_side = viewport.width >= px(780.);
        let primary_button_width = if side_by_side { 104. } else { 116. };
        let stop_button_width = if side_by_side { 88. } else { 92. };
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
        let secondary_handler = view_entity.clone();
        let settings_handler = view_entity.clone();
        let primary_button = Button::new(primary_label)
            .variant(primary_variant)
            .size(ComponentSize::Large)
            .on_click(move |_, window, app| {
                let prompt = start_handler
                    .read(app)
                    .prompt_input
                    .read(app)
                    .value
                    .to_string();
                let cwd = start_handler
                    .read(app)
                    .cwd_input
                    .read(app)
                    .value
                    .to_string();
                let phrases = start_handler
                    .read(app)
                    .phrases_input
                    .read(app)
                    .value
                    .to_string();
                start_handler.update(app, |view, cx| {
                    let _ = window;
                    view.primary_action_now(prompt, cwd, phrases);
                    cx.notify();
                });
            });
        let secondary_button = Button::new(secondary_label)
            .variant(if self.snapshot.phase == QueuePhase::Success {
                ButtonVariant::Outline
            } else {
                ButtonVariant::Danger
            })
            .size(ComponentSize::Large)
            .on_click(move |_, window, app| {
                secondary_handler.update(app, |view, cx| {
                    let _ = window;
                    view.secondary_action_now();
                    cx.notify();
                });
            });
        let settings_button = Button::new("高级设置")
            .variant(ButtonVariant::Outline)
            .size(ComponentSize::Medium)
            .icon(Icon::new(IconName::Setting))
            .on_click(move |_, window, app| {
                settings_handler.update(app, |view, cx| {
                    let _ = window;
                    view.toggle_advanced_now(cx);
                });
            });

        let gui_handler = view_entity.clone();
        let full_screen_handler = view_entity.clone();
        let audio_handler = view_entity.clone();
        let gui_switch =
            Switch::new(self.gui_fallback_toggle.clone()).on_change(move |change, _, app| {
                gui_handler.update(app, |view, cx| view.set_gui_fallback(change.current, cx));
            });
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
        let open_logs_button = Button::new("打开日志目录")
            .variant(ButtonVariant::Text)
            .size(ComponentSize::Small)
            .on_click(move |_, _, app| {
                logs_handler.update(app, |view, _| view.open_logs_now());
            });
        let diagnostics_handler = view_entity.clone();
        let copy_diagnostics_button =
            Button::new(if self.diagnostic_copy_feedback == CopyFeedback::Copied {
                "已复制"
            } else {
                "复制诊断"
            })
            .variant(ButtonVariant::Text)
            .size(ComponentSize::Small)
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
            let user_message = div()
                .w_full()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .items_end()
                .gap_1()
                .child(div().text_xs().text_color(rgb(0x888888)).child("你"))
                .child(
                    div()
                        .max_w(px(600.))
                        .min_w(px(0.))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(rgb(0xf0f0f0))
                        .border_1()
                        .border_color(rgb(0xe0e0e0))
                        .text_base()
                        .text_color(rgb(0x2b2b2b))
                        .whitespace_normal()
                        .child(self.config.prompt.clone()),
                );
            let assistant_text = conversation_text(&self.snapshot);
            let copy_text = assistant_text.clone();
            let copy_handler = view_entity.clone();
            let copy_button = Button::new(if self.copy_feedback == CopyFeedback::Copied {
                "已复制"
            } else {
                "复制"
            })
            .variant(ButtonVariant::Text)
            .size(ComponentSize::Small)
            .on_click(move |_, window, app| {
                let text = copy_text.clone();
                copy_handler.update(app, |view, cx| view.copy_reply(text, window, cx));
            });
            let assistant_message = div().w_full().min_w(px(0.)).flex().items_start().child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_xs()
                            .text_color(if assistant_is_error {
                                rgb(0xc23b33)
                            } else {
                                rgb(0x888888)
                            })
                            .child(div().flex_1().child(if assistant_is_error {
                                "Codex · 重试信息"
                            } else {
                                "Codex"
                            }))
                            .child(copy_button),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .max_w(px(640.))
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .border_1()
                            .border_color(if assistant_is_error {
                                rgb(0xf0c9c5)
                            } else {
                                rgb(0xe7e7e7)
                            })
                            .bg(if assistant_is_error {
                                rgb(0xfff6f5)
                            } else {
                                rgb(0xffffff)
                            })
                            .text_base()
                            .text_color(if assistant_is_error {
                                rgb(0x8f2f29)
                            } else {
                                rgb(0x333333)
                            })
                            .whitespace_normal()
                            .child(assistant_text),
                    ),
            );
            div()
                .w_full()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_2()
                .child(user_message)
                .child(assistant_message)
                .into_any_element()
        } else {
            div()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .px_3()
                .child(div().text_sm().text_color(rgb(0x777777)).child("等待任务"))
                .into_any_element()
        };

        let conversation_card = div()
            .h_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_xl()
            .border_1()
            .border_color(rgb(0xe2e6e4))
            .bg(rgb(0xffffff))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(0xe7e7e7))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(0x222222))
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
                    .p_2()
                    .bg(rgb(0xf7f7f7))
                    .child(conversation_history),
            )
            .child(
                div()
                    .h(px(bottom_panel_height))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .border_t_1()
                    .border_color(rgb(0xe7e7e7))
                    .child(Textarea::new(self.prompt_input.clone()).rows(if compact {
                        1
                    } else {
                        2
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(if self.prompt_error {
                                div()
                                    .text_sm()
                                    .text_color(rgb(0xd54941))
                                    .child("请输入任务内容后再开始")
                                    .into_any_element()
                            } else {
                                div().into_any_element()
                            })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .w(px(primary_button_width))
                                    .flex_none()
                                    .child(primary_button),
                            )
                            .child(
                                div()
                                    .w(px(stop_button_width))
                                    .flex_none()
                                    .child(secondary_button),
                            ),
                    ),
            )
            .into_any_element();

        let status_card = div()
            .when(side_by_side, |this| this.h_full().flex_1().min_h(px(0.)))
            .when(!side_by_side, |this| this.flex_none())
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_xl()
            .border_1()
            .border_color(rgb(0xe2e6e4))
            .bg(rgb(0xffffff))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(0xe7e7e7))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(0x222222))
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
                    .gap_3()
                    .p_3()
                    .justify_center()
                    .child(
                        div()
                            .h(px(status_panel_height))
                            .flex_none()
                            .relative()
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap_3()
                            .p(px(if self.snapshot.phase == QueuePhase::Success {
                                status_padding + 4.
                            } else {
                                status_padding
                            }))
                            .rounded_xl()
                            .bg(status_surface(self.snapshot.phase))
                            .border_1()
                            .border_color(status_surface_border(self.snapshot.phase))
                            .when(self.snapshot.phase == QueuePhase::Success, |this| {
                                this.child(success_status_texture(self.route_motion_frame))
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
                                    .child(status_icon(self.snapshot.phase, phase_accent))
                                    .child(
                                        div()
                                            .text_size(
                                                if self.snapshot.phase == QueuePhase::Success {
                                                    px(24.)
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
                                            rgb(0xc23b33)
                                        } else {
                                            rgb(0x666666)
                                        })
                                        .child(phase_detail),
                                )
                            }),
                    )
                    .when(side_by_side, |this| {
                        this.child(
                            div()
                                .h(px(metrics_panel_height))
                                .flex_none()
                                .flex()
                                .flex_col()
                                .rounded_xl()
                                .border_1()
                                .border_color(rgb(0xe7e9e8))
                                .bg(rgb(0xf8faf9))
                                .child(animated_attempt_route(
                                    self.snapshot.attempt_count,
                                    self.snapshot.consecutive_retries,
                                    self.snapshot.phase,
                                    phase_accent,
                                    self.route_motion_frame,
                                )),
                        )
                    }),
            )
            .when(side_by_side, |this| {
                this.child(
                    div()
                        .h(px(bottom_panel_height))
                        .flex_none()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(rgb(0xe7e7e7))
                        .bg(rgb(0xf8faf9))
                        .child(attempt_grid(self.snapshot.attempt_count, phase_accent)),
                )
            });

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
            let close_handler = view_entity.clone();
            let close_button = Button::new("关闭")
                .variant(ButtonVariant::Text)
                .size(ComponentSize::Medium)
                .on_click(move |_, window, app| {
                    close_handler.update(app, |view, cx| {
                        let _ = window;
                        view.toggle_advanced_now(cx);
                    });
                });
            let fields = if viewport.width >= px(720.) {
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(label("工作目录（可选）"))
                            .child(Input::new(self.cwd_input.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(label("繁忙提示短语（用 | 分隔）"))
                            .child(Input::new(self.phrases_input.clone())),
                    )
            } else {
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(label("工作目录（可选）"))
                    .child(Input::new(self.cwd_input.clone()))
                    .child(label("繁忙提示短语（用 | 分隔）"))
                    .child(Input::new(self.phrases_input.clone()))
            };
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .bg(rgba(0x00000066))
                .flex()
                .items_center()
                .justify_center()
                .p_3()
                .child(
                    div()
                        .w_full()
                        .max_w(px(720.))
                        .rounded_xl()
                        .overflow_hidden()
                        .child(
                            Card::new()
                                .title("高级设置")
                                .subtitle("按需调整兼容、提醒与重试行为")
                                .extra(close_button)
                                .body(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_3()
                                        .child(fields)
                                        .child(info_hint(
                                            "回复中命中任意短语即判定为繁忙，并继续低频重试。",
                                        ))
                                        .child(if viewport.width >= px(720.) {
                                            div()
                                                .flex()
                                                .gap_3()
                                                .child(setting_tile(
                                                    "兼容输入回退",
                                                    "通过辅助功能注入输入；提交时请勿操作键鼠",
                                                    "↩",
                                                    gui_switch,
                                                    self.config.gui_fallback_enabled,
                                                    Some("推荐关闭"),
                                                ))
                                                .child(setting_tile(
                                                    "全屏强提示",
                                                    "成功持续闪烁至点击开始，重试错误显示红色",
                                                    "✦",
                                                    full_screen_switch,
                                                    self.config.full_screen_flash_enabled,
                                                    None,
                                                ))
                                                .child(setting_tile(
                                                    "成功音效提示",
                                                    "循环播放时钟铃声，点击通知后停止",
                                                    "♪",
                                                    audio_switch,
                                                    self.config.audio_alert_enabled,
                                                    None,
                                                ))
                                                .into_any_element()
                                        } else {
                                            div()
                                                .flex()
                                                .flex_col()
                                                .gap_2()
                                                .child(setting_row(
                                                    "兼容输入回退",
                                                    "通过辅助功能注入输入；提交时请勿操作键鼠",
                                                    "↩",
                                                    gui_switch,
                                                    self.config.gui_fallback_enabled,
                                                    Some("推荐关闭"),
                                                ))
                                                .child(setting_row(
                                                    "全屏强提示",
                                                    "成功持续闪烁至点击开始，覆盖所有显示器",
                                                    "✦",
                                                    full_screen_switch,
                                                    self.config.full_screen_flash_enabled,
                                                    None,
                                                ))
                                                .child(setting_row(
                                                    "成功音效提示",
                                                    "循环播放时钟铃声，点击通知后停止",
                                                    "♪",
                                                    audio_switch,
                                                    self.config.audio_alert_enabled,
                                                    None,
                                                ))
                                                .into_any_element()
                                        })
                                        .child(diagnostic_card(
                                            &self.codex_cli.path_label(),
                                            diagnostic_ok,
                                            &cli_status,
                                            &transport_status,
                                            diagnostic_actions,
                                        )),
                                ),
                        ),
                )
        } else {
            div()
        };

        let titlebar = titlebar(self.logo_image.clone());
        let keyboard_handler = view_entity.clone();
        let content = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0xf3f3f3))
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
                                    .border_color(rgb(0xe7e7e7))
                                    .child(
                                        div()
                                            .text_xl()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(rgb(0x1f1f1f))
                                            .child("STAY WITH THE TASK."),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .px_2()
                                            .py_1()
                                            .rounded_lg()
                                            .border_1()
                                            .border_color(rgb(0xdfe5e2))
                                            .bg(rgb(0xf8faf9))
                                            .child(div().w(px(7.)).h(px(7.)).rounded_full().bg(
                                                if diagnostic_ok {
                                                    rgb(0x16a673)
                                                } else {
                                                    rgb(0xd54941)
                                                },
                                            ))
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(rgb(0x59635f))
                                                    .child(cli_status),
                                            )
                                            .child(div().w(px(1.)).h(px(14.)).bg(rgb(0xdfe5e2)))
                                            .child(
                                                div().text_xs().text_color(rgb(0x777f7c)).child(
                                                    format!("v{}", env!("CARGO_PKG_VERSION")),
                                                ),
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
                if event.keystroke.modifiers.secondary() && matches!(key, "enter" | "return") {
                    let active = matches!(
                        keyboard_handler.read(app).snapshot.phase,
                        QueuePhase::Connecting
                            | QueuePhase::Sending
                            | QueuePhase::Waiting
                            | QueuePhase::Backoff
                    );
                    let advanced_open = keyboard_handler.read(app).advanced_open;
                    if !active && !advanced_open {
                        let prompt = keyboard_handler
                            .read(app)
                            .prompt_input
                            .read(app)
                            .value
                            .to_string();
                        let cwd = keyboard_handler
                            .read(app)
                            .cwd_input
                            .read(app)
                            .value
                            .to_string();
                        let phrases = keyboard_handler
                            .read(app)
                            .phrases_input
                            .read(app)
                            .value
                            .to_string();
                        keyboard_handler.update(app, |view, cx| {
                            view.start_now(prompt, cwd, phrases);
                            cx.notify();
                        });
                    }
                    let _ = window;
                    app.stop_propagation();
                } else if key == "escape" && keyboard_handler.read(app).advanced_open {
                    keyboard_handler.update(app, |view, cx| {
                        view.advanced_open = false;
                        cx.notify();
                    });
                    app.stop_propagation();
                }
            });

        let config = TDesignConfig::new()
            .theme_mode(ThemeMode::Light)
            .theme_overrides(ThemeOverrides::new().brand(rgb(0x16a673)));
        TDesignRoot::with_config(config).child(content)
    }
}

fn titlebar(logo_image: Arc<Image>) -> gpui::Div {
    div()
        .h(px(32.))
        .flex_none()
        .flex()
        .items_center()
        .border_b_1()
        .border_color(rgb(0xe7e7e7))
        .bg(rgb(0xffffff))
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
                        .text_color(rgb(0x666666))
                        .child("Longwatch"),
                ),
        )
        .child(titlebar_control("−", WindowControlArea::Min, false))
        .child(titlebar_control("□", WindowControlArea::Max, false))
        .child(titlebar_control("×", WindowControlArea::Close, true))
}

fn titlebar_control(symbol: &'static str, area: WindowControlArea, close: bool) -> gpui::Div {
    div()
        .w(px(44.))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .window_control_area(area)
        .text_size(px(14.))
        .text_color(rgb(0x666666))
        .hover(move |element| {
            if close {
                element.bg(rgb(0xd54941)).text_color(rgb(0xffffff))
            } else {
                element.bg(rgb(0xf3f3f3)).text_color(rgb(0x1f1f1f))
            }
        })
        .child(symbol)
}

fn setting_row(
    title: &'static str,
    description: &'static str,
    symbol: &'static str,
    switch: impl IntoElement,
    enabled: bool,
    badge: Option<&'static str>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .p_3()
        .rounded_xl()
        .border_1()
        .border_color(rgb(0xe7e7e7))
        .bg(rgb(0xffffff))
        .child(
            div()
                .w(px(34.))
                .h(px(34.))
                .flex_none()
                .rounded_lg()
                .bg(if enabled {
                    rgb(0xe8f8f0)
                } else {
                    rgb(0xf3f3f3)
                })
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(17.))
                .text_color(if enabled {
                    rgb(0x16a673)
                } else {
                    rgb(0x777777)
                })
                .child(symbol),
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
                        .when_some(badge, |this, badge| this.child(setting_badge(badge))),
                )
                .child(div().text_sm().text_color(rgb(0x777777)).child(description)),
        )
        .child(switch)
}

fn setting_tile(
    title: &'static str,
    description: &'static str,
    symbol: &'static str,
    switch: impl IntoElement,
    enabled: bool,
    badge: Option<&'static str>,
) -> impl IntoElement {
    div()
        .flex_1()
        .min_w(px(0.))
        .flex()
        .items_center()
        .gap_2()
        .p_2()
        .rounded_xl()
        .border_1()
        .border_color(rgb(0xe7e7e7))
        .bg(rgb(0xffffff))
        .child(
            div()
                .w(px(30.))
                .h(px(30.))
                .flex_none()
                .rounded_lg()
                .bg(if enabled {
                    rgb(0xe8f8f0)
                } else {
                    rgb(0xf3f3f3)
                })
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(16.))
                .text_color(if enabled {
                    rgb(0x16a673)
                } else {
                    rgb(0x777777)
                })
                .child(symbol),
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
                        .when_some(badge, |this, badge| this.child(setting_badge(badge))),
                )
                .child(div().text_xs().text_color(rgb(0x777777)).child(description)),
        )
        .child(switch)
}

fn setting_badge(text: &'static str) -> impl IntoElement {
    div()
        .h(px(16.))
        .flex_none()
        .px_1()
        .rounded(px(3.))
        .bg(rgb(0xe8f8f0))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(10.))
        .font_weight(FontWeight::NORMAL)
        .text_color(rgb(0x11875d))
        .child(text)
}

fn diagnostic_card(
    diagnostic: &str,
    ok: bool,
    cli_status: &str,
    transport_status: &str,
    action: impl IntoElement,
) -> impl IntoElement {
    let diagnostic = diagnostic.to_owned();
    let cli_status = cli_status.to_owned();
    let transport_status = transport_status.to_owned();
    div().rounded_xl().overflow_hidden().child(
        Card::new()
            .title("运行诊断")
            .subtitle(cli_status.clone())
            .extra(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Tag::new(if ok { "运行正常" } else { "需要检查" }).theme(if ok {
                            TagTheme::Success
                        } else {
                            TagTheme::Danger
                        }),
                    )
                    .child(action),
            )
            .body(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_sm()
                    .text_color(rgb(0x666666))
                    .child(format!("Codex CLI：{cli_status} · {diagnostic}"))
                    .child(format!("连接通道：{transport_status}"))
                    .child("后台窗口：永久隐藏 · 全屏与音效提醒按开关执行"),
            ),
    )
}

fn info_hint(text: &'static str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .text_color(rgb(0x777777))
        .child(
            div()
                .w(px(18.))
                .h(px(18.))
                .rounded_full()
                .bg(rgb(0xf3f3f3))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .child("i"),
        )
        .child(text)
}

fn label(text: &'static str) -> impl IntoElement {
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(0x555555))
        .child(text)
}

fn status_icon(phase: QueuePhase, accent: gpui::Rgba) -> impl IntoElement {
    let (icon, size, frame_size) = match phase {
        QueuePhase::Success => (IconName::CheckCircleFilled, 64., 76.),
        QueuePhase::Connecting
        | QueuePhase::Sending
        | QueuePhase::Waiting
        | QueuePhase::Backoff => (IconName::Refresh, 25., 40.),
        QueuePhase::Paused => (IconName::PauseCircleFilled, 25., 40.),
        QueuePhase::FatalError => (IconName::ErrorCircleFilled, 25., 40.),
        QueuePhase::Idle => (IconName::ChevronRightCircle, 25., 40.),
    };
    div()
        .w(px(frame_size))
        .h(px(frame_size))
        .flex_none()
        .rounded_full()
        .bg(if phase == QueuePhase::Success {
            rgba(0xffffffd9)
        } else {
            rgba(0xffffffcc)
        })
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(icon).size(px(size)).color(accent))
}

fn success_status_texture(motion_frame: u8) -> impl IntoElement {
    const COLUMNS: usize = 15;
    const ROWS: usize = 5;

    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .children((0..COLUMNS * ROWS).map(move |index| {
            let column = index % COLUMNS;
            let row = index / COLUMNS;
            let wave = (usize::from(motion_frame) + column * 3 + row * 5) % 16;
            let alpha = match wave {
                0 | 1 => 0x42,
                2..=5 => 0x2e,
                6..=10 => 0x22,
                _ => 0x16,
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
        .child(success_confetti(motion_frame))
}

fn success_confetti(motion_frame: u8) -> impl IntoElement {
    const PIECES: [(bool, f32, f32, f32, f32, u32); 14] = [
        (false, 18., 5., 4., 13., 0x16a67300),
        (false, 44., 29., 12., 4., 0xe0a32900),
        (false, 76., 53., 5., 15., 0x39a99b00),
        (false, 28., 82., 13., 4., 0xe36b5d00),
        (false, 91., 105., 4., 12., 0x4f8dc900),
        (false, 55., 132., 11., 4., 0x8e72c700),
        (false, 112., 18., 4., 10., 0x16a67300),
        (true, 20., 9., 12., 4., 0xe0a32900),
        (true, 50., 35., 4., 14., 0x39a99b00),
        (true, 82., 60., 13., 4., 0xe36b5d00),
        (true, 31., 88., 5., 13., 0x4f8dc900),
        (true, 96., 111., 11., 4., 0x16a67300),
        (true, 60., 137., 4., 11., 0x8e72c700),
        (true, 118., 20., 10., 4., 0xe0a32900),
    ];
    let frame = usize::from(motion_frame);

    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .children(PIECES.into_iter().enumerate().map(
            move |(index, (from_right, side, top, width, height, color))| {
                let drift = ((frame * 2 + index * 5) % 15) as f32;
                let sway = match (frame + index * 3) % 8 {
                    0 | 1 => 0.,
                    2 | 3 | 7 => 2.,
                    4 | 5 => 4.,
                    _ => 3.,
                };
                let alpha = match (frame + index * 2) % 12 {
                    0..=2 => 0x92,
                    3..=6 => 0x78,
                    7..=9 => 0x60,
                    _ => 0x48,
                };

                div()
                    .absolute()
                    .top(px(top + drift))
                    .when(from_right, |this| this.right(px(side + sway)))
                    .when(!from_right, |this| this.left(px(side + sway)))
                    .w(px(width))
                    .h(px(height))
                    .rounded_full()
                    .bg(rgba(color | alpha))
                    .into_any_element()
            },
        ))
}

fn animated_attempt_route(
    attempt_count: u64,
    retry_count: u32,
    phase: QueuePhase,
    accent: gpui::Rgba,
    motion_frame: u8,
) -> impl IntoElement {
    let stage = match phase {
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

    div()
        .size_full()
        .flex()
        .flex_col()
        .justify_center()
        .gap_2()
        .px_4()
        .py_3()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(0x333333))
                        .child("任务进度"),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(accent)
                        .child(format!("尝试 {attempt_count} 次 · 重试 {retry_count} 轮")),
                ),
        )
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .children((0..9).map(move |slot| {
                    if slot % 2 == 1 {
                        let connector = slot / 2;
                        return div()
                            .flex_1()
                            .h(px(1.))
                            .rounded_full()
                            .bg(if connector < stage {
                                accent
                            } else {
                                rgb(0xdde3e0)
                            })
                            .into_any_element();
                    }

                    let index = slot / 2;
                    let completed = index < stage || (phase == QueuePhase::Success && index == 4);
                    let current = index == stage;
                    let dot_size = if current && pulse_on { 11. } else { 9. };
                    div()
                        .w(px(14.))
                        .h(px(14.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .w(px(dot_size))
                                .h(px(dot_size))
                                .rounded_full()
                                .border_1()
                                .border_color(if completed || current {
                                    accent
                                } else {
                                    rgb(0xcbd3cf)
                                })
                                .bg(if completed || current {
                                    accent
                                } else {
                                    rgb(0xffffff)
                                }),
                        )
                        .into_any_element()
                })),
        )
        .child(
            div().w_full().flex().children(
                ["开始", "连接", "发送", "处理", "完成"]
                    .into_iter()
                    .enumerate()
                    .map(move |(index, label)| {
                        div()
                            .flex_1()
                            .flex()
                            .justify_center()
                            .text_xs()
                            .text_color(if index <= stage {
                                accent
                            } else {
                                rgb(0x929a96)
                            })
                            .child(label)
                    }),
            ),
        )
}

fn attempt_grid(attempt_count: u64, accent: gpui::Rgba) -> impl IntoElement {
    const CELL_COUNT: u64 = 20;
    let active_cells = attempt_count.min(CELL_COUNT);
    let hidden_count = attempt_count.saturating_sub(CELL_COUNT);
    div()
        .w_full()
        .h_full()
        .flex()
        .flex_col()
        .justify_center()
        .gap_2()
        .px_1()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(0x333333))
                        .child("尝试轨迹"),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(0x777777))
                        .child(format!("{attempt_count} 次")),
                ),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_1()
                .children((0..CELL_COUNT).map(move |index| {
                    let active = index < active_cells;
                    div()
                        .flex_1()
                        .min_w(px(8.))
                        .h(px(6.))
                        .rounded_full()
                        .bg(if active { accent } else { rgb(0xe4e8e6) })
                })),
        )
        .when(hidden_count > 0, |this| {
            this.child(
                div()
                    .text_xs()
                    .text_color(rgb(0x777777))
                    .child(format!("另有 {hidden_count} 次未展开显示")),
            )
        })
}

fn status_surface(phase: QueuePhase) -> gpui::Rgba {
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

fn status_surface_border(phase: QueuePhase) -> gpui::Rgba {
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
        | QueuePhase::Backoff => "暂停排队",
        QueuePhase::Paused => "继续排队",
        QueuePhase::Success => "开始新任务",
        QueuePhase::FatalError => "重新开始",
        QueuePhase::Idle => "开始排队",
    }
}

fn phase_color(phase: QueuePhase) -> gpui::Rgba {
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
            let closing = Arc::new(AtomicBool::new(false));
            let titlebar_inset = if cfg!(target_os = "windows") { 32. } else { 0. };
            let bounds = Bounds::centered(None, size(px(1000.), px(612. + titlebar_inset)), cx);
            #[cfg(target_os = "windows")]
            let initial_for_tray = snapshots.borrow().clone();
            let view_closing = Arc::clone(&closing);
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        titlebar: Some(TitlebarOptions {
                            title: Some("Longwatch for Codex".into()),
                            appears_transparent: cfg!(target_os = "windows"),
                            ..Default::default()
                        }),
                        app_id: Some("Longwatch".into()),
                        window_min_size: Some(size(px(780.), px(560. + titlebar_inset))),
                        ..Default::default()
                    },
                    {
                        let config = config.clone();
                        let commands = commands.clone();
                        let initial = snapshots.borrow().clone();
                        let codex_cli = codex_cli.clone();
                        let config_directory = config_directory.clone();
                        let logs_directory = logs_directory.clone();
                        move |_, cx| {
                            let logo_image = Arc::new(Image::from_bytes(
                                ImageFormat::Png,
                                include_bytes!("../packaging/ui-logo.png").to_vec(),
                            ));
                            let prompt_input = InputState::new(cx, config.prompt.clone());
                            let cwd_value = config
                                .working_directory
                                .as_ref()
                                .map_or_else(String::new, |path| path.display().to_string());
                            let cwd_input = InputState::new(cx, cwd_value);
                            let phrases_input =
                                InputState::new(cx, config.failure_phrases.join(" | "));
                            prompt_input.update(cx, |state, cx| {
                                state.set_placeholder("输入一条真实、有意义的任务", cx);
                            });
                            cwd_input.update(cx, |state, cx| {
                                state.set_placeholder("留空则继承 Codex 默认工作目录", cx);
                            });
                            phrases_input.update(cx, |state, cx| {
                                state.set_placeholder("Server overloaded; retry later...", cx);
                            });
                            let gui_fallback_toggle =
                                ToggleState::new(cx, config.gui_fallback_enabled);
                            let full_screen_toggle =
                                ToggleState::new(cx, config.full_screen_flash_enabled);
                            let audio_toggle = ToggleState::new(cx, config.audio_alert_enabled);
                            let view = cx.new(|_| QueueView {
                                config,
                                snapshot: initial,
                                commands,
                                prompt_input,
                                cwd_input,
                                phrases_input,
                                gui_fallback_toggle,
                                full_screen_toggle,
                                audio_toggle,
                                logo_image,
                                codex_cli,
                                advanced_open: false,
                                pulsing: false,
                                route_motion_frame: 0,
                                prompt_error: false,
                                conversation_scroll: ScrollHandle::new(),
                                copy_feedback: CopyFeedback::Idle,
                                diagnostic_copy_feedback: CopyFeedback::Idle,
                                config_directory,
                                logs_directory,
                                #[cfg(target_os = "macos")]
                                _appearance_subscription: None,
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
                                                        view.send(RuntimeCommand::Pause);
                                                    }
                                                    gpui_platform::TrayAction::Resume => {
                                                        view.send(RuntimeCommand::Start);
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
                                                    | QueuePhase::Success
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
                            view
                        }
                    },
                )
                .expect("failed to open Longwatch window");
            let _ = window.update(cx, |queue_view, app_window, cx| {
                let view = cx.entity();
                #[cfg(target_os = "macos")]
                {
                    sync_macos_icon_for_window(app_window);
                    queue_view._appearance_subscription =
                        Some(app_window.observe_window_appearance(|window, _| {
                            sync_macos_icon_for_window(window);
                        }));
                }
                #[cfg(not(target_os = "macos"))]
                let _ = queue_view;
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
            cx.spawn(async move |cx| {
                while snapshots.changed().await.is_ok() {
                    if watcher_closing.load(Ordering::Acquire) {
                        break;
                    }
                    let snapshot = snapshots.borrow().clone();
                    if window
                        .update(cx, |view, window, cx| {
                            view.update_snapshot(snapshot, window, cx);
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

    let send_result = shutdown_runtime.block_on(runtime.send(RuntimeCommand::Shutdown));
    if let Err(error) = send_result {
        warn!(%error, "发送后台关闭命令失败");
    }
    match shutdown_runtime.block_on(tokio::time::timeout(Duration::from_secs(3), runtime.join())) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => warn!(%error, "后台任务异常结束"),
        Err(_) => warn!("等待后台任务退出超时"),
    }
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
}
