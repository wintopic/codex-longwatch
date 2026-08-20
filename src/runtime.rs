//! Runtime controller joining the queue state machine to a transport.

use std::time::{Duration, Instant};

use chrono::Utc;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde_json::Value;
use thiserror::Error;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{self, Instant as TokioInstant},
};
use tracing::{debug, info, warn};

use crate::{
    backoff::WakeDetector,
    classifier::{
        CompletedStatus, TurnDecision, TurnError, classify_error_notification,
        classify_turn_completion,
    },
    config::{ConfigStore, QueueConfig, prompt_digest},
    queue::{QueueError, QueueMachine, QueuePhase, QueueSnapshot},
    transport::{
        CodexTransport, RestoredTurn, StartedTurn, ThreadSession, TransportError, TransportEvent,
        TurnStatus,
    },
};

#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeCommand {
    Configure(QueueConfig),
    StartConfigured(QueueConfig),
    Start,
    Pause,
    Stop,
    Wake,
    Shutdown,
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("configuration is invalid: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("transport failed: {0}")]
    Transport(#[from] TransportError),
    #[error("queue state error: {0}")]
    Queue(#[from] QueueError),
    #[error("后台命令通道已关闭")]
    ChannelClosed,
    #[error("后台任务异常结束：{0}")]
    TaskJoin(String),
    #[error("内部状态异常：{0}")]
    Invariant(&'static str),
}

/// Handle returned by [`spawn_runtime`].  The UI can clone the command sender
/// and subscribe to the watch receiver without exposing mutable runtime state.
pub struct RuntimeHandle {
    commands: mpsc::Sender<RuntimeCommand>,
    snapshot: watch::Receiver<QueueSnapshot>,
    join: JoinHandle<Result<(), RuntimeError>>,
}

impl std::fmt::Debug for RuntimeHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeHandle")
            .finish_non_exhaustive()
    }
}

impl RuntimeHandle {
    pub async fn send(&self, command: RuntimeCommand) -> Result<(), RuntimeError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| RuntimeError::ChannelClosed)
    }

    #[must_use]
    pub fn snapshot(&self) -> watch::Receiver<QueueSnapshot> {
        self.snapshot.clone()
    }

    #[must_use]
    pub fn command_sender(&self) -> mpsc::Sender<RuntimeCommand> {
        self.commands.clone()
    }

    pub async fn join(self) -> Result<(), RuntimeError> {
        self.join
            .await
            .map_err(|error| RuntimeError::TaskJoin(error.to_string()))?
    }
}

/// Spawn a runtime on the current Tokio runtime.
pub fn spawn_runtime<T>(
    transport: T,
    config: QueueConfig,
    state: crate::config::PersistedQueueState,
    store: Option<ConfigStore>,
) -> RuntimeHandle
where
    T: CodexTransport + 'static,
{
    let active_phase = matches!(
        state.phase,
        QueuePhase::Connecting | QueuePhase::Sending | QueuePhase::Waiting | QueuePhase::Backoff
    );
    let prompt_matches = state
        .prompt_digest
        .as_deref()
        .is_none_or(|digest| digest == prompt_digest(&config.prompt));
    let mut machine = QueueMachine::restore(state, config.codex_path.clone());
    if active_phase && !prompt_matches {
        machine.pause("保存的任务与当前提示不一致；请确认后再开始");
        machine.clear_submission_uncertain();
    } else if active_phase && machine.snapshot().phase == QueuePhase::Sending {
        // Older state files did not have the uncertainty bit.  Treat a
        // persisted Sending phase conservatively and reconcile it first.
        machine.mark_submission_uncertain();
    }
    let initial = machine.snapshot().clone();
    let (commands, command_receiver) = mpsc::channel(32);
    let (snapshot_sender, snapshot_receiver) = watch::channel(initial);
    let mut runtime = QueueRuntime::new(transport, config, machine, store, snapshot_sender);
    // A persisted active task is resumed automatically.  Paused, successful,
    // and fatal states remain user-controlled and never start on their own.
    runtime.started = active_phase && prompt_matches;
    let join = tokio::spawn(async move { runtime.run(command_receiver).await });
    RuntimeHandle {
        commands,
        snapshot: snapshot_receiver,
        join,
    }
}

/// The deterministic controller.  It is public so integration tests can use a
/// fake app-server without opening a desktop window.
pub struct QueueRuntime<T> {
    transport: T,
    config: QueueConfig,
    machine: QueueMachine,
    store: Option<ConfigStore>,
    snapshot_sender: watch::Sender<QueueSnapshot>,
    rng: ChaCha8Rng,
    wake_detector: WakeDetector,
    started: bool,
    connected: bool,
    connection_failures: u32,
    turn_deadline: Option<Instant>,
    success_notified: bool,
    persistence: PersistenceHealth,
}

#[derive(Debug, Default)]
struct PersistenceHealth {
    state_failures: u32,
    config_failed: bool,
}

impl<T> std::fmt::Debug for QueueRuntime<T>
where
    T: std::fmt::Debug,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = self.machine.snapshot();
        formatter
            .debug_struct("QueueRuntime")
            .field("config_version", &self.config.version)
            .field("codex_path", &self.config.codex_path)
            .field("phase", &snapshot.phase)
            .field("active_thread_id", &snapshot.active_thread_id)
            .field("active_turn_id", &snapshot.active_turn_id)
            .field("attempt_count", &snapshot.attempt_count)
            .field("started", &self.started)
            .field("connected", &self.connected)
            .finish_non_exhaustive()
    }
}

impl<T> QueueRuntime<T>
where
    T: CodexTransport,
{
    #[must_use]
    pub fn new(
        mut transport: T,
        config: QueueConfig,
        machine: QueueMachine,
        store: Option<ConfigStore>,
        snapshot_sender: watch::Sender<QueueSnapshot>,
    ) -> Self {
        transport.set_gui_fallback_enabled(config.gui_fallback_enabled);
        transport.set_app_server_window_hidden(true);
        let now = Utc::now();
        let monotonic = Instant::now();
        Self {
            transport,
            config,
            machine,
            store,
            snapshot_sender,
            rng: ChaCha8Rng::from_os_rng(),
            wake_detector: WakeDetector::new(now, monotonic),
            started: false,
            connected: false,
            connection_failures: 0,
            turn_deadline: None,
            success_notified: false,
            persistence: PersistenceHealth::default(),
        }
    }

    #[must_use]
    pub fn machine(&self) -> &QueueMachine {
        &self.machine
    }

    pub async fn run(
        mut self,
        commands: mpsc::Receiver<RuntimeCommand>,
    ) -> Result<(), RuntimeError> {
        let result = self.run_loop(commands).await;
        self.shutdown_transport().await;
        result
    }

    async fn run_loop(
        &mut self,
        mut commands: mpsc::Receiver<RuntimeCommand>,
    ) -> Result<(), RuntimeError> {
        self.publish();
        if self.started {
            let due = self
                .machine
                .snapshot()
                .next_attempt_at
                .is_none_or(|next| next <= Utc::now());
            if due {
                self.ensure_connection_and_thread().await?;
            }
        }
        let mut wake_poll = time::interval(Duration::from_secs(10));
        wake_poll.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        loop {
            let deadline = self.next_deadline();
            tokio::select! {
                command = commands.recv() => {
                    match command {
                        Some(RuntimeCommand::Configure(config)) => {
                            let _ = self.handle_configure(config);
                        }
                        Some(RuntimeCommand::StartConfigured(config)) => {
                            if self.machine.snapshot().phase == QueuePhase::Success {
                                self.handle_stop().await?;
                            }
                            if self.handle_configure(config) {
                                self.handle_start().await?;
                            }
                        }
                        Some(RuntimeCommand::Start) => self.handle_start().await?,
                        Some(RuntimeCommand::Pause) => self.handle_pause().await?,
                        Some(RuntimeCommand::Stop) => self.handle_stop().await?,
                        Some(RuntimeCommand::Wake) => self.handle_explicit_wake(),
                        Some(RuntimeCommand::Shutdown) | None => {
                            break;
                        }
                    }
                }
                event = self.transport.next_event(), if self.started && self.connected => {
                    match event {
                        Some(event) => self.handle_event(event).await?,
                        None => self.handle_disconnect("app-server event stream closed").await?,
                    }
                }
                _ = wake_poll.tick() => {
                    self.observe_wake();
                }
                _ = sleep_until(deadline), if self.started && deadline.is_some() => {
                    if !self.observe_wake() {
                        self.handle_deadline().await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_start(&mut self) -> Result<(), RuntimeError> {
        if self.started {
            return Ok(());
        }
        if self.machine.snapshot().phase == QueuePhase::Success {
            return Ok(());
        }
        if let Err(error) = self.config.validate() {
            self.machine.pause(&format!("配置无效：{error}"));
            self.publish_and_persist();
            return Ok(());
        }
        self.started = true;
        self.success_notified = false;
        if matches!(
            self.machine.snapshot().phase,
            QueuePhase::Paused | QueuePhase::FatalError | QueuePhase::Idle
        ) {
            self.machine.reactivate();
        }
        self.publish_and_persist();
        self.ensure_connection_and_thread().await
    }

    fn handle_configure(&mut self, config: QueueConfig) -> bool {
        if config == self.config {
            return true;
        }
        let task_locked = self.started
            || self.machine.snapshot().active_turn_id.is_some()
            || self.machine.snapshot().submission_uncertain;
        if task_locked {
            let mut accepted = self.config.clone();
            accepted.gui_fallback_enabled = config.gui_fallback_enabled;
            accepted.full_screen_flash_enabled = config.full_screen_flash_enabled;
            accepted.audio_alert_enabled = config.audio_alert_enabled;
            let only_live_settings_changed = accepted == config;

            self.config.gui_fallback_enabled = config.gui_fallback_enabled;
            self.config.full_screen_flash_enabled = config.full_screen_flash_enabled;
            self.config.audio_alert_enabled = config.audio_alert_enabled;
            self.transport
                .set_gui_fallback_enabled(self.config.gui_fallback_enabled);
            self.persist_config();

            if !only_live_settings_changed {
                self.machine.set_runtime_warning(
                    "任务运行中：提醒与兼容开关已生效，任务内容和重试参数需停止后修改",
                );
            }
            self.publish_and_persist();
            return only_live_settings_changed;
        }
        let task_changed = config.prompt != self.config.prompt
            || config.working_directory != self.config.working_directory;
        if task_changed && self.machine.snapshot().active_thread_id.is_some() {
            self.machine
                .set_status("更换任务或工作目录前请先点击“停止”，以创建新的持久线程");
            self.publish_and_persist();
            return false;
        }
        if let Err(error) = config.validate() {
            self.machine.set_status(&format!("配置无效：{error}"));
            self.publish_and_persist();
            return false;
        }
        if config.codex_path != self.config.codex_path {
            self.machine
                .set_status("Codex 路径已保存；请重启 Longwatch 后再开始");
            self.config = config;
            self.persist_config();
            self.publish_and_persist();
            return false;
        }
        self.config = config;
        self.transport
            .set_gui_fallback_enabled(self.config.gui_fallback_enabled);
        self.transport.set_app_server_window_hidden(true);
        self.persist_config();
        self.publish_and_persist();
        true
    }

    async fn ensure_connection_and_thread(&mut self) -> Result<(), RuntimeError> {
        if !self.connected {
            self.machine.begin_connecting();
            self.publish_and_persist();
            info!("正在连接 Codex app-server");
            let connect_result = self.transport.connect().await;
            self.reset_wake_baseline();
            if let Err(error) = connect_result {
                return self.handle_transport_failure(error).await;
            }
            self.connected = true;
            self.refresh_transport_status();
            self.publish_and_persist();
            info!("Codex app-server 已连接");
        }

        let existing_thread = self.machine.snapshot().active_thread_id.clone();
        let session = if let Some(thread_id) = existing_thread.as_deref() {
            self.transport
                .resume_thread(thread_id, self.config.working_directory.as_deref())
                .await
        } else {
            self.transport
                .start_thread(self.config.working_directory.as_deref())
                .await
        };
        self.reset_wake_baseline();
        let session = match session {
            Ok(session) => session,
            Err(error) => return self.handle_transport_failure(error).await,
        };
        self.connection_failures = 0;
        self.apply_thread_session(session).await
    }

    async fn apply_thread_session(&mut self, session: ThreadSession) -> Result<(), RuntimeError> {
        self.machine.set_thread(session.id);
        self.publish_and_persist();
        if let Some(turn) = session.latest_turn {
            match turn.status {
                TurnStatus::InProgress => {
                    // A server-side resume is authoritative.  If the
                    // persisted id is stale (for example after a process
                    // crash), replace it before installing the resumed turn
                    // instead of failing with TurnAlreadyActive.
                    if self.machine.snapshot().active_turn_id.as_deref() != Some(turn.id.as_str()) {
                        self.machine.clear_active_turn();
                        self.machine.set_reply_preview("");
                    }
                    self.machine.clear_submission_uncertain();
                    self.machine.set_active_turn(turn.id.clone())?;
                    self.turn_deadline =
                        Some(Instant::now() + Duration::from_secs(self.config.turn_timeout_secs));
                    self.publish_and_persist();
                }
                TurnStatus::Completed | TurnStatus::Interrupted | TurnStatus::Failed => {
                    if self.machine.snapshot().active_turn_id.is_some()
                        || self.machine.snapshot().submission_uncertain
                        || matches!(
                            self.machine.snapshot().phase,
                            QueuePhase::Sending | QueuePhase::Waiting
                        )
                    {
                        self.handle_completed_turn(&turn).await?;
                    } else if self.started {
                        self.send_attempt().await?;
                    }
                }
            }
        } else if self.started {
            // If an earlier process died between `turn/start` and receiving its
            // response, do not immediately resend.  Enter a normal backoff and
            // let the user inspect the persisted state if the server cannot
            // reconcile the thread.
            if self.machine.snapshot().submission_uncertain
                || self.machine.snapshot().active_turn_id.is_some()
                || matches!(
                    self.machine.snapshot().phase,
                    QueuePhase::Sending | QueuePhase::Waiting
                )
            {
                self.schedule_retry("无法确认上一次回合是否已提交");
            } else {
                self.send_attempt().await?;
            }
        }
        Ok(())
    }

    async fn send_attempt(&mut self) -> Result<(), RuntimeError> {
        if !self.started || self.machine.snapshot().active_thread_id.is_none() {
            return Ok(());
        }
        if let Some(next) = self.machine.snapshot().next_attempt_at {
            if next > Utc::now() {
                return Ok(());
            }
        }
        let now = Utc::now();
        if let Err(error) = self.machine.begin_sending(now) {
            match error {
                QueueError::TurnAlreadyActive => return Ok(()),
            }
        }
        self.publish_and_persist();
        // Persist the uncertainty marker before crossing the process boundary
        // so a crash at any point during `turn/start` is reconciled by
        // `thread/resume` rather than immediately duplicated.
        self.machine.mark_submission_uncertain();
        if !self.publish_and_persist_critical() {
            return Ok(());
        }
        let thread_id = self
            .machine
            .snapshot()
            .active_thread_id
            .clone()
            .ok_or(RuntimeError::Invariant("缺少活动线程 ID"))?;
        info!(
            attempt = self.machine.snapshot().attempt_count,
            thread_id, "正在提交 Codex 回合"
        );
        let start_result = self
            .transport
            .start_turn(
                &thread_id,
                &self.config.prompt,
                self.config.working_directory.as_deref(),
            )
            .await;
        self.reset_wake_baseline();
        match start_result {
            Ok(StartedTurn { id }) => {
                self.machine.set_active_turn(id)?;
                self.turn_deadline =
                    Some(Instant::now() + Duration::from_secs(self.config.turn_timeout_secs));
                self.publish_and_persist();
            }
            Err(error @ (TransportError::Timeout(_) | TransportError::Process(_))) => {
                self.machine.set_reply_preview(&error.to_string());
                self.shutdown_transport().await;
                self.turn_deadline = None;
                self.schedule_connection_retry(&error.to_string());
                // Reconnect and reconcile the same persistent thread before
                // considering another request.
                debug!(%error, "turn submission outcome is uncertain; reconciling thread");
            }
            Err(error) => self.handle_transport_failure(error).await?,
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: TransportEvent) -> Result<(), RuntimeError> {
        match event {
            TransportEvent::AgentMessageDelta {
                thread_id,
                turn_id,
                delta,
            } => {
                if self.matches_active(&thread_id, &turn_id) {
                    self.machine.append_reply_delta(&delta);
                    // Streaming text is a volatile preview. Persisting every
                    // token would force an fsync storm; terminal events still
                    // persist the authoritative final reply.
                    self.publish();
                }
            }
            TransportEvent::TurnStarted { thread_id, turn_id } => {
                if self.machine.snapshot().active_thread_id.as_deref() != Some(thread_id.as_str())
                    || !matches!(
                        self.machine.snapshot().phase,
                        QueuePhase::Sending | QueuePhase::Waiting
                    )
                {
                    return Ok(());
                }
                if let Some(active_turn) = self.machine.snapshot().active_turn_id.as_deref()
                    && active_turn != turn_id
                {
                    // A queued notification from an older turn can arrive
                    // after a retry was scheduled.  It must not resurrect the
                    // old turn or terminate the runtime.
                    return Ok(());
                }
                self.machine.set_active_turn(turn_id)?;
                self.turn_deadline =
                    Some(Instant::now() + Duration::from_secs(self.config.turn_timeout_secs));
                self.publish_and_persist();
            }
            TransportEvent::ThreadResolved {
                previous_thread_id,
                thread_id,
                turn_id,
            } => {
                if self.machine.snapshot().active_thread_id.as_deref()
                    == Some(previous_thread_id.as_str())
                    && self.machine.snapshot().active_turn_id.as_deref() == Some(turn_id.as_str())
                {
                    self.machine.resolve_thread(thread_id);
                    self.publish_and_persist();
                }
            }
            TransportEvent::Error {
                thread_id,
                turn_id,
                error,
                will_retry,
            } => {
                if !self.matches_active(&thread_id, &turn_id) {
                    return Ok(());
                }
                let conversation_error = error_conversation_text(&error);
                if !conversation_error.is_empty() {
                    self.machine.set_reply_preview(&conversation_error);
                }
                let decision =
                    classify_error_notification(&error, will_retry, &self.config.failure_phrases);
                match decision {
                    TurnDecision::WaitForInternalRetry(reason) => {
                        self.machine.note_internal_retry(&reason);
                        self.publish_and_persist();
                    }
                    TurnDecision::WaitForInternalRetryQuiet(reason) => {
                        self.machine.note_internal_retry_quiet(&reason);
                        self.publish_and_persist();
                    }
                    TurnDecision::Retryable(reason) => self.schedule_retry(&reason),
                    TurnDecision::RetryImmediately(reason) => {
                        self.schedule_immediate_retry(&reason);
                    }
                    TurnDecision::RetryImmediatelyQuiet(reason) => {
                        self.schedule_quiet_immediate_retry(&reason);
                    }
                    TurnDecision::Pause(reason) => {
                        self.machine.pause(&reason);
                        self.started = false;
                        self.turn_deadline = None;
                        self.publish_and_persist();
                    }
                    TurnDecision::Success => {}
                }
            }
            TransportEvent::TurnCompleted { thread_id, turn } => {
                if self.machine.snapshot().active_thread_id.as_deref() == Some(thread_id.as_str())
                    && self.machine.snapshot().active_turn_id.as_deref() == Some(turn.id.as_str())
                {
                    self.handle_completed_turn(&turn).await?;
                }
            }
            TransportEvent::Disconnected { message } => {
                if self.started {
                    self.handle_disconnect(&message).await?;
                }
            }
            TransportEvent::Diagnostic { message } => {
                if self.machine.snapshot().phase == QueuePhase::Backoff {
                    debug!(message, "退避期间忽略非关键诊断，保留重试倒计时");
                } else {
                    self.machine.set_status(&message);
                    // Diagnostics can be noisy (for example a changing JSONL
                    // file), so keep them visible without writing every update.
                    self.publish();
                }
            }
        }
        Ok(())
    }

    async fn handle_completed_turn(&mut self, turn: &RestoredTurn) -> Result<(), RuntimeError> {
        let status = match turn.status {
            TurnStatus::Completed => CompletedStatus::Completed,
            TurnStatus::Interrupted => CompletedStatus::Interrupted,
            TurnStatus::Failed => CompletedStatus::Failed,
            TurnStatus::InProgress => CompletedStatus::InProgress,
        };
        let decision = classify_turn_completion(
            status,
            &turn.final_message,
            turn.error.as_ref(),
            &self.config.failure_phrases,
            self.machine.snapshot().empty_reply_count,
            self.config.retry_policy.max_empty_replies,
        );
        let conversation_text = if turn.final_message.trim().is_empty() {
            turn.error
                .as_ref()
                .map(error_conversation_text)
                .unwrap_or_default()
        } else {
            turn.final_message.clone()
        };
        self.machine.set_reply_preview(&conversation_text);
        if !matches!(turn.status, TurnStatus::InProgress) {
            self.machine.clear_submission_uncertain();
        }
        self.turn_deadline = None;
        match decision {
            TurnDecision::Success => {
                self.machine.succeed("任务成功完成");
                self.started = false;
                info!(
                    attempts = self.machine.snapshot().attempt_count,
                    "Longwatch 任务已成功完成"
                );
                self.notify_success();
            }
            TurnDecision::Retryable(reason) => {
                if turn.final_message.trim().is_empty() {
                    self.machine.mark_empty_reply();
                }
                self.schedule_retry(&reason);
            }
            TurnDecision::RetryImmediately(reason) => {
                self.schedule_immediate_retry(&reason);
            }
            TurnDecision::RetryImmediatelyQuiet(reason) => {
                self.schedule_quiet_immediate_retry(&reason);
            }
            TurnDecision::WaitForInternalRetry(reason) => self.machine.note_internal_retry(&reason),
            TurnDecision::WaitForInternalRetryQuiet(reason) => {
                self.machine.note_internal_retry_quiet(&reason)
            }
            TurnDecision::Pause(reason) => {
                self.machine.pause(&reason);
                self.started = false;
            }
        }
        self.publish_and_persist();
        Ok(())
    }

    async fn handle_disconnect(&mut self, message: &str) -> Result<(), RuntimeError> {
        warn!(message, "Codex app-server 连接已断开");
        let preserve_backoff = self.machine.snapshot().phase == QueuePhase::Backoff
            && self.machine.snapshot().next_attempt_at.is_some();
        self.shutdown_transport().await;
        self.turn_deadline = None;
        if !self.started {
            self.publish_and_persist();
            return Ok(());
        }
        if preserve_backoff {
            // The retry deadline already encodes the conservative backoff.
            // A stale event-stream close must not shorten it to five seconds.
            self.publish_and_persist();
            return Ok(());
        }
        let error_text = format!("app-server 已断开：{message}");
        self.machine.set_reply_preview(&error_text);
        self.schedule_connection_retry(&format!("{error_text}；正在恢复线程"));
        Ok(())
    }

    async fn handle_transport_failure(
        &mut self,
        error: TransportError,
    ) -> Result<(), RuntimeError> {
        let error_text = error.to_string();
        self.machine.set_reply_preview(&error_text);
        match error {
            error @ (TransportError::ExecutableNotFound(_)
            | TransportError::FallbackDisabled(_)
            | TransportError::Unavailable(_)) => {
                self.shutdown_transport().await;
                self.turn_deadline = None;
                self.machine.pause(&error.to_string());
                self.started = false;
                self.publish_and_persist();
            }
            error @ TransportError::Protocol(_) => {
                self.shutdown_transport().await;
                self.turn_deadline = None;
                self.machine.fatal(&error.to_string());
                self.started = false;
                self.publish_and_persist();
            }
            TransportError::Rpc {
                code,
                message,
                data,
            } => {
                let codex_error_info = data
                    .as_ref()
                    .and_then(|value| value.get("codexErrorInfo").cloned())
                    .or_else(|| data.clone());
                let error = TurnError {
                    message: message.clone(),
                    additional_details: data.as_ref().map(Value::to_string),
                    codex_error_info,
                };
                self.machine
                    .set_reply_preview(&error_conversation_text(&error));
                match classify_error_notification(&error, false, &self.config.failure_phrases) {
                    TurnDecision::Retryable(reason) => {
                        info!(code, reason, "JSON-RPC 错误已归类为可重试");
                        self.schedule_retry(&reason);
                    }
                    TurnDecision::RetryImmediately(reason) => {
                        info!(code, reason, "JSON-RPC 错误触发立即重试");
                        self.schedule_immediate_retry(&reason);
                    }
                    TurnDecision::RetryImmediatelyQuiet(reason) => {
                        info!(code, reason, "JSON-RPC 高需求提示触发静默立即重试");
                        self.schedule_quiet_immediate_retry(&reason);
                    }
                    TurnDecision::WaitForInternalRetry(reason) => {
                        self.machine.note_internal_retry(&reason);
                        self.publish_and_persist();
                    }
                    TurnDecision::WaitForInternalRetryQuiet(reason) => {
                        self.machine.note_internal_retry_quiet(&reason);
                        self.publish_and_persist();
                    }
                    TurnDecision::Pause(reason) => {
                        warn!(code, message, "JSON-RPC 错误需要用户处理");
                        self.shutdown_transport().await;
                        self.turn_deadline = None;
                        self.machine.pause(&reason);
                        self.started = false;
                        self.publish_and_persist();
                    }
                    TurnDecision::Success => {}
                }
            }
            error @ (TransportError::Timeout(_) | TransportError::Process(_)) => {
                self.shutdown_transport().await;
                self.turn_deadline = None;
                self.schedule_connection_retry(&error.to_string());
            }
            TransportError::NotConnected => {
                self.connected = false;
                self.refresh_transport_status();
                self.turn_deadline = None;
                self.schedule_connection_retry("Codex app-server 尚未连接");
            }
        }
        Ok(())
    }

    fn schedule_connection_retry(&mut self, reason: &str) {
        self.connection_failures = self.connection_failures.saturating_add(1);
        let delay_seconds = connection_retry_delay_seconds(self.connection_failures);
        self.machine.record_retry_alert();
        self.machine.begin_connecting();
        self.machine.set_status(&format!(
            "{reason}；第 {} 次连接恢复，{} 秒后重试",
            self.connection_failures, delay_seconds
        ));
        self.machine
            .set_next_attempt_at(Utc::now() + chrono::Duration::seconds(i64::from(delay_seconds)));
        self.publish_and_persist();
    }

    async fn handle_pause(&mut self) -> Result<(), RuntimeError> {
        self.started = false;
        let active_turn = (
            self.machine.snapshot().active_thread_id.clone(),
            self.machine.snapshot().active_turn_id.clone(),
        );
        self.machine.pause("已暂停；不会自动发送新的回合");
        self.turn_deadline = None;
        self.publish_and_persist();
        if let (Some(thread_id), Some(turn_id)) = active_turn {
            match time::timeout(
                Duration::from_secs(5),
                self.transport.interrupt_turn(&thread_id, &turn_id),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warn!(%error, "暂停时中断当前回合失败"),
                Err(_) => warn!("暂停时中断当前回合超时，界面已保持暂停状态"),
            }
            self.reset_wake_baseline();
        }
        Ok(())
    }

    async fn handle_stop(&mut self) -> Result<(), RuntimeError> {
        self.started = false;
        let active_turn = (
            self.machine.snapshot().active_thread_id.clone(),
            self.machine.snapshot().active_turn_id.clone(),
        );
        self.machine.stop();
        self.turn_deadline = None;
        self.publish_and_persist();
        if let (Some(thread_id), Some(turn_id)) = active_turn {
            let _ = time::timeout(
                Duration::from_secs(5),
                self.transport.interrupt_turn(&thread_id, &turn_id),
            )
            .await;
            self.reset_wake_baseline();
        }
        self.shutdown_transport().await;
        Ok(())
    }

    fn refresh_transport_status(&mut self) {
        self.machine.set_transport_status(self.transport.status());
    }

    async fn shutdown_transport(&mut self) {
        self.transport.shutdown().await;
        self.connected = false;
        self.refresh_transport_status();
    }

    fn handle_explicit_wake(&mut self) {
        self.apply_wake_delay(Utc::now());
        // Reset the detector baseline so the periodic poll does not report the
        // same resume a second time.
        let _ = self.wake_detector.observe(Utc::now(), Instant::now());
    }

    fn observe_wake(&mut self) -> bool {
        let now = Utc::now();
        if !self.wake_detector.observe(now, Instant::now()) {
            return false;
        }
        self.apply_wake_delay(now)
    }

    fn apply_wake_delay(&mut self, now: chrono::DateTime<Utc>) -> bool {
        let mut changed = self
            .machine
            .delay_after_wake(now, &self.config.retry_policy);
        if self.machine.snapshot().phase == QueuePhase::Waiting
            && self.machine.snapshot().active_turn_id.is_some()
        {
            let wake_floor = Instant::now() + self.config.retry_policy.wake_delay();
            self.turn_deadline = Some(
                self.turn_deadline
                    .map_or(wake_floor, |deadline| deadline.max(wake_floor)),
            );
            self.machine
                .set_status("检测到系统唤醒；继续等待当前回合，不会立即补发");
            changed = true;
        }
        if changed {
            self.publish_and_persist();
        }
        changed
    }

    async fn handle_deadline(&mut self) -> Result<(), RuntimeError> {
        if self.machine.snapshot().phase == QueuePhase::Backoff {
            self.ensure_connection_and_thread().await?;
            return Ok(());
        }
        if self.machine.snapshot().phase == QueuePhase::Connecting {
            self.ensure_connection_and_thread().await?;
            return Ok(());
        }
        if self.machine.snapshot().active_turn_id.is_some() {
            let thread_id = self.machine.snapshot().active_thread_id.clone();
            let turn_id = self.machine.snapshot().active_turn_id.clone();
            if let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) {
                match time::timeout(
                    Duration::from_secs(5),
                    self.transport.interrupt_turn(&thread_id, &turn_id),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => warn!(%error, "回合超时后的中断请求失败"),
                    Err(_) => warn!("回合超时后的中断请求再次超时"),
                }
                self.reset_wake_baseline();
                self.machine.clear_active_turn();
                self.turn_deadline = None;
                self.schedule_retry("回合超时，已发送中断请求");
                self.publish_and_persist();
            }
        } else if self.machine.snapshot().next_attempt_at.is_some() {
            self.ensure_connection_and_thread().await?;
        }
        Ok(())
    }

    fn schedule_retry(&mut self, reason: &str) {
        self.turn_deadline = None;
        if self.machine.snapshot().reply_preview.trim().is_empty() {
            self.machine.set_reply_preview(reason);
        }
        let next = self.machine.schedule_retry(
            Utc::now(),
            &self.config.retry_policy,
            reason,
            &mut self.rng,
        );
        self.machine
            .set_status(&format!("{reason}；已安排低频重试"));
        info!(
            retry_round = self.machine.snapshot().consecutive_retries,
            next_attempt_at = %next,
            reason,
            "已安排下一次重试"
        );
        self.publish_and_persist();
    }

    fn schedule_immediate_retry(&mut self, reason: &str) {
        const SETTLE_DELAY_SECS: i64 = 1;
        let now = Utc::now();
        self.turn_deadline = None;
        if self.machine.snapshot().reply_preview.trim().is_empty() {
            self.machine.set_reply_preview(reason);
        }
        self.machine
            .schedule_retry(now, &self.config.retry_policy, reason, &mut self.rng);
        self.machine
            .set_next_attempt_at(now + chrono::Duration::seconds(SETTLE_DELAY_SECS));
        self.machine
            .set_status(&format!("{reason}；正在开始新一轮尝试"));
        info!(reason, "Codex 内部重试耗尽，正在衔接新一轮尝试");
        self.publish_and_persist();
    }

    fn schedule_quiet_immediate_retry(&mut self, reason: &str) {
        let now = Utc::now();
        self.turn_deadline = None;
        if self.machine.snapshot().reply_preview.trim().is_empty() {
            self.machine.set_reply_preview(reason);
        }
        let next = self.machine.schedule_quiet_immediate_retry(now, reason);
        info!(
            retry_round = self.machine.snapshot().consecutive_retries,
            next_attempt_at = %next,
            reason,
            "高需求提示已安排静默立即重试"
        );
        self.publish_and_persist();
    }

    fn matches_active(&self, thread_id: &str, turn_id: &str) -> bool {
        self.machine.snapshot().active_thread_id.as_deref() == Some(thread_id)
            && self.machine.snapshot().active_turn_id.as_deref() == Some(turn_id)
    }

    fn next_deadline(&self) -> Option<TokioInstant> {
        if let Some(deadline) = self.turn_deadline {
            return Some(TokioInstant::from_std(deadline));
        }
        self.machine.snapshot().next_attempt_at.map(|when| {
            let now = Utc::now();
            let duration = (when - now).to_std().unwrap_or_default();
            TokioInstant::now() + duration
        })
    }

    fn publish(&self) {
        let _ = self.snapshot_sender.send(self.machine.snapshot().clone());
    }

    fn publish_and_persist(&mut self) {
        self.publish();
        let _ = self.persist_state();
    }

    fn publish_and_persist_critical(&mut self) -> bool {
        self.publish();
        if self.persist_state() {
            return true;
        }

        self.started = false;
        self.turn_deadline = None;
        self.machine.clear_submission_uncertain();
        self.machine
            .pause("无法安全保存提交状态，已暂停以避免重复发送任务");
        self.publish();
        false
    }

    fn persist_state(&mut self) -> bool {
        let Some(store) = self.store.as_ref() else {
            return true;
        };
        let state = self
            .machine
            .persisted(Some(prompt_digest(&self.config.prompt)));
        match store.save_state(&state) {
            Ok(()) => {
                if self.persistence.state_failures > 0 {
                    info!(
                        failures = self.persistence.state_failures,
                        "运行状态保存已恢复"
                    );
                    self.persistence.state_failures = 0;
                    self.clear_persistence_warning_if_recovered();
                }
                true
            }
            Err(error) => {
                self.persistence.state_failures = self.persistence.state_failures.saturating_add(1);
                warn!(
                    %error,
                    failures = self.persistence.state_failures,
                    "运行状态保存失败，后台将继续运行"
                );
                self.machine.set_runtime_warning(format!(
                    "状态保存失败（连续 {} 次），当前任务仍在内存中运行：{error}",
                    self.persistence.state_failures
                ));
                self.publish();
                false
            }
        }
    }

    fn persist_config(&mut self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        match store.save_config(&self.config) {
            Ok(()) => {
                if self.persistence.config_failed {
                    info!("配置保存已恢复");
                    self.persistence.config_failed = false;
                    self.clear_persistence_warning_if_recovered();
                }
            }
            Err(error) => {
                self.persistence.config_failed = true;
                warn!(%error, "配置保存失败，当前会话仍使用内存中的设置");
                self.machine.set_runtime_warning(format!(
                    "配置保存失败，本次设置仅在当前会话生效：{error}"
                ));
                self.publish();
            }
        }
    }

    fn clear_persistence_warning_if_recovered(&mut self) {
        if self.persistence.state_failures == 0
            && !self.persistence.config_failed
            && self
                .machine
                .snapshot()
                .runtime_warning
                .as_deref()
                .is_some_and(|warning| {
                    warning.starts_with("状态保存失败") || warning.starts_with("配置保存失败")
                })
        {
            self.machine.clear_runtime_warning();
            self.publish();
        }
    }

    fn reset_wake_baseline(&mut self) {
        let _ = self.wake_detector.observe(Utc::now(), Instant::now());
    }

    fn notify_success(&mut self) {
        if self.success_notified {
            return;
        }
        self.success_notified = true;
        #[cfg(test)]
        return;
        #[cfg(not(test))]
        {
            if self.config.full_screen_flash_enabled {
                gpui_platform::show_completion_overlay();
            }
            let snapshot = self.machine.snapshot();
            let action = snapshot.active_thread_id.as_deref().map(|thread_id| {
                gpui_platform::NotificationAction::open_thread(
                    thread_id,
                    self.config.codex_path.to_string_lossy(),
                )
            });
            let notification_title = notification_preview(&self.config.prompt, 96);
            let notification_body = notification_preview(&snapshot.reply_preview, 220);
            if let Err(error) = gpui_platform::notify_success(
                &notification_title,
                &notification_body,
                action.as_ref(),
                self.config.audio_alert_enabled,
            ) {
                debug!(%error, "native notification unavailable");
            }
        }
    }
}

const fn connection_retry_delay_seconds(failure_count: u32) -> u32 {
    match failure_count {
        0 | 1 => 5,
        2 => 15,
        _ => 60,
    }
}

fn error_conversation_text(error: &TurnError) -> String {
    let message = error.message.trim();
    let details = error
        .additional_details
        .as_deref()
        .map(str::trim)
        .unwrap_or("");
    match (message.is_empty(), details.is_empty()) {
        (false, false) if !message.contains(details) => format!("{message}\n\n{details}"),
        (false, _) => message.to_owned(),
        (true, false) => details.to_owned(),
        (true, true) => "Codex 返回了未说明原因的错误".into(),
    }
}

fn notification_preview(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return "任务已完成".into();
    }
    let mut preview = normalized.chars().take(max_chars).collect::<String>();
    if normalized.chars().count() > max_chars {
        preview.push('…');
    }
    preview
}

async fn sleep_until(deadline: Option<TokioInstant>) {
    if let Some(deadline) = deadline {
        time::sleep_until(deadline).await;
    } else {
        // A long, cancellable sleep keeps the select branch cheap when the
        // runtime is idle and still responds instantly to commands.
        time::sleep(Duration::from_secs(24 * 60 * 60)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use super::*;
    use crate::{
        config::PersistedQueueState,
        transport::{ThreadSession, TurnStatus},
    };
    use serde_json::json;

    #[test]
    fn notification_preview_matches_compact_chat_style() {
        assert_eq!(
            notification_preview("第一行\n\n第二行", 20),
            "第一行 第二行"
        );
        assert_eq!(notification_preview("abcdef", 4), "abcd…");
        assert_eq!(notification_preview("   ", 20), "任务已完成");
    }

    #[derive(Debug, Default)]
    struct NoopTransport;

    #[async_trait::async_trait]
    impl CodexTransport for NoopTransport {
        async fn connect(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn start_thread(
            &mut self,
            _: Option<&std::path::Path>,
        ) -> Result<ThreadSession, TransportError> {
            Ok(ThreadSession {
                id: "thread".into(),
                latest_turn: None,
            })
        }
        async fn resume_thread(
            &mut self,
            id: &str,
            _: Option<&std::path::Path>,
        ) -> Result<ThreadSession, TransportError> {
            Ok(ThreadSession {
                id: id.into(),
                latest_turn: None,
            })
        }
        async fn start_turn(
            &mut self,
            _: &str,
            _: &str,
            _: Option<&std::path::Path>,
        ) -> Result<StartedTurn, TransportError> {
            Ok(StartedTurn { id: "turn".into() })
        }
        async fn interrupt_turn(&mut self, _: &str, _: &str) -> Result<(), TransportError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<TransportEvent> {
            None
        }
        async fn shutdown(&mut self) {}
    }

    #[derive(Debug)]
    struct WindowVisibilityTransport {
        hidden: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl CodexTransport for WindowVisibilityTransport {
        fn set_app_server_window_hidden(&mut self, hidden: bool) {
            self.hidden.store(hidden, Ordering::SeqCst);
        }

        async fn connect(&mut self) -> Result<(), TransportError> {
            unreachable!()
        }

        async fn start_thread(
            &mut self,
            _: Option<&std::path::Path>,
        ) -> Result<ThreadSession, TransportError> {
            unreachable!()
        }

        async fn resume_thread(
            &mut self,
            _: &str,
            _: Option<&std::path::Path>,
        ) -> Result<ThreadSession, TransportError> {
            unreachable!()
        }

        async fn start_turn(
            &mut self,
            _: &str,
            _: &str,
            _: Option<&std::path::Path>,
        ) -> Result<StartedTurn, TransportError> {
            unreachable!()
        }

        async fn interrupt_turn(&mut self, _: &str, _: &str) -> Result<(), TransportError> {
            unreachable!()
        }

        async fn next_event(&mut self) -> Option<TransportEvent> {
            std::future::pending().await
        }

        async fn shutdown(&mut self) {}
    }

    #[test]
    fn runtime_always_hides_the_app_server_window() {
        let hidden = Arc::new(AtomicBool::new(false));
        let config = QueueConfig::default();
        let machine = QueueMachine::new(config.codex_path.clone());
        let (sender, _) = watch::channel(machine.snapshot().clone());

        let _runtime = QueueRuntime::new(
            WindowVisibilityTransport {
                hidden: Arc::clone(&hidden),
            },
            config,
            machine,
            None,
            sender,
        );

        assert!(hidden.load(Ordering::SeqCst));
    }

    #[test]
    fn repeated_connection_failures_back_off_without_growing_forever() {
        assert_eq!(connection_retry_delay_seconds(1), 5);
        assert_eq!(connection_retry_delay_seconds(2), 15);
        assert_eq!(connection_retry_delay_seconds(3), 60);
        assert_eq!(connection_retry_delay_seconds(99), 60);
    }

    #[test]
    fn exhausted_internal_retries_start_the_next_round_immediately() {
        let config = QueueConfig::default();
        let machine = QueueMachine::new(config.codex_path.clone());
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let mut runtime = QueueRuntime::new(NoopTransport, config, machine, None, sender);
        let before = Utc::now();

        runtime.schedule_immediate_retry("Codex 内部 5 次重试已耗尽");

        let snapshot = runtime.machine().snapshot();
        let next = snapshot.next_attempt_at.expect("retry deadline is set");
        assert!(next >= before);
        assert!(next <= Utc::now() + chrono::Duration::seconds(2));
        assert_eq!(snapshot.phase, QueuePhase::Backoff);
        assert!(snapshot.status_message.contains("正在开始新一轮尝试"));
    }

    #[tokio::test]
    async fn start_creates_one_thread_and_one_turn() {
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let machine =
            QueueMachine::restore(PersistedQueueState::default(), config.codex_path.clone());
        let (_sender, _receiver) = watch::channel(machine.snapshot().clone());
        let (commands, receiver) = mpsc::channel(4);
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let runtime = QueueRuntime::new(NoopTransport, config, machine, None, sender);
        let task = tokio::spawn(runtime.run(receiver));
        commands.send(RuntimeCommand::Start).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[derive(Clone, Copy, Debug)]
    enum Script {
        HighDemand,
        CurrentHighDemandThenSuccess,
        InternalRetryThenSuccess,
        StreamDisconnect,
        Success,
    }

    #[derive(Debug)]
    struct ScriptedTransport {
        script: Script,
        starts: Arc<AtomicUsize>,
        sender: mpsc::Sender<TransportEvent>,
        receiver: mpsc::Receiver<TransportEvent>,
    }

    impl ScriptedTransport {
        fn new(script: Script) -> (Self, Arc<AtomicUsize>) {
            let (sender, receiver) = mpsc::channel(16);
            let starts = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    script,
                    starts: Arc::clone(&starts),
                    sender,
                    receiver,
                },
                starts,
            )
        }
    }

    #[async_trait::async_trait]
    impl CodexTransport for ScriptedTransport {
        async fn connect(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn start_thread(
            &mut self,
            _: Option<&std::path::Path>,
        ) -> Result<ThreadSession, TransportError> {
            Ok(ThreadSession {
                id: "thread".into(),
                latest_turn: None,
            })
        }
        async fn resume_thread(
            &mut self,
            id: &str,
            _: Option<&std::path::Path>,
        ) -> Result<ThreadSession, TransportError> {
            Ok(ThreadSession {
                id: id.into(),
                latest_turn: None,
            })
        }
        async fn start_turn(
            &mut self,
            _: &str,
            _: &str,
            _: Option<&std::path::Path>,
        ) -> Result<StartedTurn, TransportError> {
            let number = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
            let id = format!("turn-{number}");
            match self.script {
                Script::HighDemand => {
                    self.sender.send(TransportEvent::TurnCompleted {
                        thread_id: "thread".into(),
                        turn: RestoredTurn {
                            id: id.clone(),
                            status: TurnStatus::Completed,
                            final_message: "We're experiencing high demand. Please try again later.".into(),
                            error: None,
                        },
                    }).await.unwrap();
                }
                Script::CurrentHighDemandThenSuccess => {
                    let (status, final_message) = if number == 1 {
                        (
                            TurnStatus::Completed,
                            crate::config::CURRENT_HIGH_DEMAND_PHRASE.to_owned(),
                        )
                    } else {
                        (TurnStatus::Completed, "success".to_owned())
                    };
                    self.sender
                        .send(TransportEvent::TurnCompleted {
                            thread_id: "thread".into(),
                            turn: RestoredTurn {
                                id: id.clone(),
                                status,
                                final_message,
                                error: None,
                            },
                        })
                        .await
                        .unwrap();
                }
                Script::InternalRetryThenSuccess => {
                    self.sender
                        .send(TransportEvent::Error {
                            thread_id: "thread".into(),
                            turn_id: id.clone(),
                            error: TurnError {
                                message: "temporary overload".into(),
                                codex_error_info: Some(json!("serverOverloaded")),
                                ..TurnError::default()
                            },
                            will_retry: true,
                        })
                        .await
                        .unwrap();
                    self.sender
                        .send(TransportEvent::TurnCompleted {
                            thread_id: "thread".into(),
                            turn: RestoredTurn {
                                id: id.clone(),
                                status: TurnStatus::Completed,
                                final_message: "success".into(),
                                error: None,
                            },
                        })
                        .await
                        .unwrap();
                }
                Script::StreamDisconnect => {
                    self.sender
                        .send(TransportEvent::Error {
                            thread_id: "thread".into(),
                            turn_id: id.clone(),
                            error: TurnError {
                                message: "stream disconnected before completion: error sending request for url (https://anyrouter.top/v1/responses)".into(),
                                ..TurnError::default()
                            },
                            will_retry: false,
                        })
                        .await
                        .unwrap();
                }
                Script::Success => {
                    self.sender
                        .send(TransportEvent::TurnCompleted {
                            thread_id: "thread".into(),
                            turn: RestoredTurn {
                                id: id.clone(),
                                status: TurnStatus::Completed,
                                final_message: "success".into(),
                                error: None,
                            },
                        })
                        .await
                        .unwrap();
                }
            }
            Ok(StartedTurn { id })
        }
        async fn interrupt_turn(&mut self, _: &str, _: &str) -> Result<(), TransportError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<TransportEvent> {
            self.receiver.recv().await
        }
        async fn shutdown(&mut self) {}
    }

    async fn wait_for_phase(
        snapshots: &mut watch::Receiver<QueueSnapshot>,
        phase: QueuePhase,
    ) -> QueueSnapshot {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = snapshots.borrow().clone();
                if snapshot.phase == phase {
                    return snapshot;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .expect("runtime reached expected phase")
    }

    #[tokio::test]
    async fn high_demand_schedules_one_future_request_without_an_immediate_duplicate() {
        let (transport, starts) = ScriptedTransport::new(Script::HighDemand);
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let machine =
            QueueMachine::restore(PersistedQueueState::default(), config.codex_path.clone());
        let (sender, mut snapshots) = watch::channel(machine.snapshot().clone());
        let (commands, receiver) = mpsc::channel(4);
        let runtime = QueueRuntime::new(transport, config, machine, None, sender);
        let task = tokio::spawn(runtime.run(receiver));
        commands.send(RuntimeCommand::Start).await.unwrap();
        let snapshot = wait_for_phase(&mut snapshots, QueuePhase::Backoff).await;
        assert!(snapshot.next_attempt_at.is_some());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn current_high_demand_notice_retries_immediately_without_alert_or_backoff() {
        let (transport, starts) = ScriptedTransport::new(Script::CurrentHighDemandThenSuccess);
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let machine =
            QueueMachine::restore(PersistedQueueState::default(), config.codex_path.clone());
        let (sender, mut snapshots) = watch::channel(machine.snapshot().clone());
        let (commands, receiver) = mpsc::channel(4);
        let runtime = QueueRuntime::new(transport, config, machine, None, sender);
        let task = tokio::spawn(runtime.run(receiver));

        commands.send(RuntimeCommand::Start).await.unwrap();
        let snapshot = wait_for_phase(&mut snapshots, QueuePhase::Success).await;

        assert_eq!(snapshot.retry_alert_count, 0);
        assert_eq!(snapshot.attempt_count, 2);
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert_eq!(snapshot.reply_preview, "success");

        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn internal_retry_and_success_never_start_a_second_turn() {
        let (transport, starts) = ScriptedTransport::new(Script::InternalRetryThenSuccess);
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let machine =
            QueueMachine::restore(PersistedQueueState::default(), config.codex_path.clone());
        let (sender, mut snapshots) = watch::channel(machine.snapshot().clone());
        let (commands, receiver) = mpsc::channel(4);
        let runtime = QueueRuntime::new(transport, config, machine, None, sender);
        let task = tokio::spawn(runtime.run(receiver));
        commands.send(RuntimeCommand::Start).await.unwrap();
        let snapshot = wait_for_phase(&mut snapshots, QueuePhase::Success).await;
        assert_eq!(snapshot.reply_preview, "success");
        assert_eq!(snapshot.retry_alert_count, 1);
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn textual_stream_disconnect_stays_in_the_retry_loop_and_is_shown_as_a_reply() {
        let (transport, starts) = ScriptedTransport::new(Script::StreamDisconnect);
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let machine =
            QueueMachine::restore(PersistedQueueState::default(), config.codex_path.clone());
        let (sender, mut snapshots) = watch::channel(machine.snapshot().clone());
        let (commands, receiver) = mpsc::channel(4);
        let runtime = QueueRuntime::new(transport, config, machine, None, sender);
        let task = tokio::spawn(runtime.run(receiver));

        commands.send(RuntimeCommand::Start).await.unwrap();
        let snapshot = wait_for_phase(&mut snapshots, QueuePhase::Backoff).await;
        assert_eq!(snapshot.attempt_count, 1);
        assert_eq!(snapshot.retry_alert_count, 1);
        assert!(snapshot.next_attempt_at.is_some());
        assert!(
            snapshot
                .reply_preview
                .contains("stream disconnected before completion")
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);

        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn normal_success_stops_all_future_sends() {
        let (transport, starts) = ScriptedTransport::new(Script::Success);
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let machine =
            QueueMachine::restore(PersistedQueueState::default(), config.codex_path.clone());
        let (sender, mut snapshots) = watch::channel(machine.snapshot().clone());
        let (commands, receiver) = mpsc::channel(4);
        let runtime = QueueRuntime::new(transport, config, machine, None, sender);
        let task = tokio::spawn(runtime.run(receiver));
        commands.send(RuntimeCommand::Start).await.unwrap();
        wait_for_phase(&mut snapshots, QueuePhase::Success).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        commands.send(RuntimeCommand::Start).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn success_can_start_a_new_configured_task() {
        let (transport, starts) = ScriptedTransport::new(Script::Success);
        let config = QueueConfig {
            prompt: "first task".into(),
            ..QueueConfig::default()
        };
        let machine =
            QueueMachine::restore(PersistedQueueState::default(), config.codex_path.clone());
        let (sender, mut snapshots) = watch::channel(machine.snapshot().clone());
        let (commands, receiver) = mpsc::channel(4);
        let runtime = QueueRuntime::new(transport, config.clone(), machine, None, sender);
        let task = tokio::spawn(runtime.run(receiver));

        commands.send(RuntimeCommand::Start).await.unwrap();
        wait_for_phase(&mut snapshots, QueuePhase::Success).await;
        let mut next_config = config;
        next_config.prompt = "second task".into();
        commands
            .send(RuntimeCommand::StartConfigured(next_config))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if starts.load(Ordering::SeqCst) == 2
                    && snapshots.borrow().phase == QueuePhase::Success
                {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .expect("second configured task completed");

        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn resume_replaces_a_stale_persisted_turn_id() {
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let state = PersistedQueueState {
            phase: QueuePhase::Waiting,
            active_thread_id: Some("thread".into()),
            active_turn_id: Some("stale-turn".into()),
            ..PersistedQueueState::default()
        };
        let machine = QueueMachine::restore(state, config.codex_path.clone());
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let mut runtime = QueueRuntime::new(NoopTransport, config, machine, None, sender);
        runtime.started = true;

        runtime
            .apply_thread_session(ThreadSession {
                id: "thread".into(),
                latest_turn: Some(RestoredTurn {
                    id: "resumed-turn".into(),
                    status: TurnStatus::InProgress,
                    final_message: String::new(),
                    error: None,
                }),
            })
            .await
            .unwrap();

        assert_eq!(
            runtime.machine().snapshot().active_turn_id.as_deref(),
            Some("resumed-turn")
        );
        assert_eq!(runtime.machine().snapshot().phase, QueuePhase::Waiting);
    }

    #[tokio::test]
    async fn stale_turn_started_notification_is_ignored() {
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let state = PersistedQueueState {
            phase: QueuePhase::Waiting,
            active_thread_id: Some("thread".into()),
            active_turn_id: Some("current-turn".into()),
            ..PersistedQueueState::default()
        };
        let machine = QueueMachine::restore(state, config.codex_path.clone());
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let mut runtime = QueueRuntime::new(NoopTransport, config, machine, None, sender);
        runtime.started = true;

        runtime
            .handle_event(TransportEvent::TurnStarted {
                thread_id: "thread".into(),
                turn_id: "old-turn".into(),
            })
            .await
            .unwrap();

        assert_eq!(
            runtime.machine().snapshot().active_turn_id.as_deref(),
            Some("current-turn")
        );
    }

    #[tokio::test]
    async fn late_completion_for_an_old_turn_is_ignored() {
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let state = PersistedQueueState {
            phase: QueuePhase::Waiting,
            active_thread_id: Some("thread".into()),
            active_turn_id: Some("current-turn".into()),
            reply_preview: "current preview".into(),
            ..PersistedQueueState::default()
        };
        let machine = QueueMachine::restore(state, config.codex_path.clone());
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let mut runtime = QueueRuntime::new(NoopTransport, config, machine, None, sender);
        runtime.started = true;

        runtime
            .handle_event(TransportEvent::TurnCompleted {
                thread_id: "thread".into(),
                turn: RestoredTurn {
                    id: "old-turn".into(),
                    status: TurnStatus::Completed,
                    final_message: "late duplicate".into(),
                    error: None,
                },
            })
            .await
            .unwrap();

        assert_eq!(runtime.machine().snapshot().phase, QueuePhase::Waiting);
        assert_eq!(
            runtime.machine().snapshot().active_turn_id.as_deref(),
            Some("current-turn")
        );
        assert_eq!(
            runtime.machine().snapshot().reply_preview,
            "current preview"
        );
    }

    #[tokio::test]
    async fn gui_session_resolution_replaces_only_the_thread_id() {
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let state = PersistedQueueState {
            phase: QueuePhase::Waiting,
            active_thread_id: Some("gui-pending".into()),
            active_turn_id: Some("logical-turn".into()),
            ..PersistedQueueState::default()
        };
        let machine = QueueMachine::restore(state, config.codex_path.clone());
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let mut runtime = QueueRuntime::new(NoopTransport, config, machine, None, sender);
        runtime.started = true;

        runtime
            .handle_event(TransportEvent::ThreadResolved {
                previous_thread_id: "gui-pending".into(),
                thread_id: "thread-real".into(),
                turn_id: "logical-turn".into(),
            })
            .await
            .unwrap();

        assert_eq!(
            runtime.machine().snapshot().active_thread_id.as_deref(),
            Some("thread-real")
        );
        assert_eq!(
            runtime.machine().snapshot().active_turn_id.as_deref(),
            Some("logical-turn")
        );
        assert_eq!(runtime.machine().snapshot().phase, QueuePhase::Waiting);
    }

    #[test]
    fn configuring_during_an_active_turn_does_not_pause_or_replace_it() {
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let state = PersistedQueueState {
            phase: QueuePhase::Waiting,
            active_thread_id: Some("thread".into()),
            active_turn_id: Some("current-turn".into()),
            ..PersistedQueueState::default()
        };
        let machine = QueueMachine::restore(state, config.codex_path.clone());
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let mut runtime = QueueRuntime::new(NoopTransport, config.clone(), machine, None, sender);
        runtime.started = true;
        let mut edited = config;
        edited.failure_phrases.push("another phrase".into());

        assert!(!runtime.handle_configure(edited));
        assert!(runtime.started);
        assert_eq!(runtime.machine().snapshot().phase, QueuePhase::Waiting);
        assert_eq!(
            runtime.machine().snapshot().active_turn_id.as_deref(),
            Some("current-turn")
        );
    }

    #[test]
    fn live_alert_settings_apply_during_an_active_turn() {
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let state = PersistedQueueState {
            phase: QueuePhase::Waiting,
            active_thread_id: Some("thread".into()),
            active_turn_id: Some("current-turn".into()),
            ..PersistedQueueState::default()
        };
        let machine = QueueMachine::restore(state, config.codex_path.clone());
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let mut runtime = QueueRuntime::new(NoopTransport, config.clone(), machine, None, sender);
        runtime.started = true;
        let mut edited = config;
        edited.full_screen_flash_enabled = false;
        edited.audio_alert_enabled = false;

        assert!(runtime.handle_configure(edited));
        assert!(!runtime.config.full_screen_flash_enabled);
        assert!(!runtime.config.audio_alert_enabled);
        assert_eq!(runtime.machine().snapshot().phase, QueuePhase::Waiting);
        assert_eq!(
            runtime.machine().snapshot().active_turn_id.as_deref(),
            Some("current-turn")
        );
    }

    #[tokio::test]
    async fn overloaded_json_rpc_error_enters_backoff_instead_of_stopping() {
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let mut machine = QueueMachine::new(config.codex_path.clone());
        machine.set_thread("thread".into());
        machine.begin_sending(Utc::now()).unwrap();
        machine.set_active_turn("turn".into()).unwrap();
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let mut runtime = QueueRuntime::new(NoopTransport, config, machine, None, sender);
        runtime.started = true;
        runtime.connected = true;

        runtime
            .handle_transport_failure(TransportError::Rpc {
                code: -32000,
                message: "Server overloaded; retry later.".into(),
                data: Some(json!({
                    "codexErrorInfo": "serverOverloaded",
                    "httpStatusCode": 503
                })),
            })
            .await
            .unwrap();

        assert!(runtime.started);
        assert_eq!(runtime.machine().snapshot().phase, QueuePhase::Backoff);
        assert!(runtime.machine().snapshot().next_attempt_at.is_some());
    }

    #[test]
    fn ordinary_state_persistence_failure_is_visible_but_nonfatal() {
        let directory = tempfile::tempdir().unwrap();
        let blocked_directory = directory.path().join("blocked");
        std::fs::write(&blocked_directory, b"not a directory").unwrap();
        let store = ConfigStore::in_directory(blocked_directory);
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let machine = QueueMachine::new(config.codex_path.clone());
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let mut runtime = QueueRuntime::new(NoopTransport, config, machine, Some(store), sender);

        runtime.publish_and_persist();

        assert_eq!(runtime.machine().snapshot().phase, QueuePhase::Idle);
        assert!(
            runtime
                .machine()
                .snapshot()
                .runtime_warning
                .as_deref()
                .is_some_and(|warning| warning.contains("状态保存失败"))
        );
    }

    #[test]
    fn critical_persistence_failure_pauses_before_crossing_process_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let blocked_directory = directory.path().join("blocked");
        std::fs::write(&blocked_directory, b"not a directory").unwrap();
        let store = ConfigStore::in_directory(blocked_directory);
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let mut machine = QueueMachine::new(config.codex_path.clone());
        machine.set_thread("thread".into());
        machine.begin_sending(Utc::now()).unwrap();
        machine.mark_submission_uncertain();
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let mut runtime = QueueRuntime::new(NoopTransport, config, machine, Some(store), sender);
        runtime.started = true;

        assert!(!runtime.publish_and_persist_critical());
        assert!(!runtime.started);
        assert_eq!(runtime.machine().snapshot().phase, QueuePhase::Paused);
        assert!(!runtime.machine().snapshot().submission_uncertain);
    }

    #[test]
    fn changing_the_task_requires_stopping_the_persisted_thread() {
        let config = QueueConfig {
            prompt: "old task".into(),
            ..QueueConfig::default()
        };
        let state = PersistedQueueState {
            phase: QueuePhase::Paused,
            active_thread_id: Some("thread".into()),
            ..PersistedQueueState::default()
        };
        let machine = QueueMachine::restore(state, config.codex_path.clone());
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let mut runtime = QueueRuntime::new(NoopTransport, config.clone(), machine, None, sender);
        assert!(runtime.handle_configure(config.clone()));
        let mut edited = config;
        edited.prompt = "new task".into();

        assert!(!runtime.handle_configure(edited));
        assert_eq!(runtime.config.prompt, "old task");
        assert_eq!(
            runtime.machine().snapshot().active_thread_id.as_deref(),
            Some("thread")
        );
    }

    #[tokio::test]
    async fn uncertain_submission_without_a_restored_turn_enters_backoff() {
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let state = PersistedQueueState {
            phase: QueuePhase::Connecting,
            active_thread_id: Some("thread".into()),
            submission_uncertain: true,
            attempt_count: 1,
            ..PersistedQueueState::default()
        };
        let machine = QueueMachine::restore(state, config.codex_path.clone());
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let mut runtime = QueueRuntime::new(NoopTransport, config, machine, None, sender);
        runtime.started = true;

        runtime
            .apply_thread_session(ThreadSession {
                id: "thread".into(),
                latest_turn: None,
            })
            .await
            .unwrap();

        assert_eq!(runtime.machine().snapshot().phase, QueuePhase::Backoff);
        assert_eq!(runtime.machine().snapshot().attempt_count, 1);
        assert!(!runtime.machine().snapshot().submission_uncertain);
    }

    #[derive(Debug)]
    struct ResumeCompletedTransport {
        resumes: Arc<AtomicUsize>,
        starts: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl CodexTransport for ResumeCompletedTransport {
        async fn connect(&mut self) -> Result<(), TransportError> {
            Ok(())
        }

        async fn start_thread(
            &mut self,
            _: Option<&std::path::Path>,
        ) -> Result<ThreadSession, TransportError> {
            unreachable!("a persisted thread must be resumed")
        }

        async fn resume_thread(
            &mut self,
            id: &str,
            _: Option<&std::path::Path>,
        ) -> Result<ThreadSession, TransportError> {
            self.resumes.fetch_add(1, Ordering::SeqCst);
            Ok(ThreadSession {
                id: id.into(),
                latest_turn: Some(RestoredTurn {
                    id: "persisted-turn".into(),
                    status: TurnStatus::Completed,
                    final_message: "completed while the app was closed".into(),
                    error: None,
                }),
            })
        }

        async fn start_turn(
            &mut self,
            _: &str,
            _: &str,
            _: Option<&std::path::Path>,
        ) -> Result<StartedTurn, TransportError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(StartedTurn {
                id: "unexpected-turn".into(),
            })
        }

        async fn interrupt_turn(&mut self, _: &str, _: &str) -> Result<(), TransportError> {
            Ok(())
        }

        async fn next_event(&mut self) -> Option<TransportEvent> {
            std::future::pending().await
        }

        async fn shutdown(&mut self) {}
    }

    #[tokio::test]
    async fn persisted_active_task_resumes_automatically_without_a_duplicate_turn() {
        let resumes = Arc::new(AtomicUsize::new(0));
        let starts = Arc::new(AtomicUsize::new(0));
        let transport = ResumeCompletedTransport {
            resumes: Arc::clone(&resumes),
            starts: Arc::clone(&starts),
        };
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let state = PersistedQueueState {
            phase: QueuePhase::Waiting,
            active_thread_id: Some("thread".into()),
            active_turn_id: Some("persisted-turn".into()),
            prompt_digest: Some(prompt_digest(&config.prompt)),
            ..PersistedQueueState::default()
        };
        let handle = spawn_runtime(transport, config, state, None);
        let mut snapshots = handle.snapshot();
        wait_for_phase(&mut snapshots, QueuePhase::Success).await;

        assert_eq!(resumes.load(Ordering::SeqCst), 1);
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }
}
