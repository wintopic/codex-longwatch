//! Runtime controller joining the queue state machine to a transport.

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

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
    config::{
        ConfigStore, PersistedPendingTask, PersistedQueueState, PersistedSchedulerState,
        QueueConfig, prompt_digest,
    },
    queue::{
        ConversationRole, PendingMessage, PendingTaskSummary, QueueError, QueueMachine, QueuePhase,
        QueueSessionSummary, QueueSnapshot, SubmissionPlacement,
    },
    transport::{
        CodexTransport, RestoredTurn, StartedTurn, ThreadSession, TransportError, TransportEvent,
        TurnStatus,
    },
};

#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeCommand {
    Configure(QueueConfig),
    StartConfigured(QueueConfig),
    /// Same as `StartConfigured`, with an editor acknowledgement token.
    SubmitConfigured {
        config: QueueConfig,
        request_id: u64,
    },
    Start,
    /// Resume the existing thread/task without treating the editor contents
    /// as a new user message.  This is used by the tray and paused-state UI.
    Resume,
    /// Keep one message ready for automatic delivery after the active turn.
    QueueMessage(PendingMessage),
    QueueMessageWithAck {
        message: PendingMessage,
        request_id: u64,
    },
    Pause,
    Stop,
    /// Start the editor contents in a fresh independent Codex conversation.
    /// In single-session mode this is equivalent to replacing the current
    /// session; the multi-session scheduler uses it to allocate another slot.
    StartNewConfigured {
        config: QueueConfig,
        request_id: u64,
    },
    /// Change which independent session is displayed. A single-session
    /// runtime accepts this as a harmless no-op; the session pool handles it.
    SelectSession(u64),
    /// Drop the selected session's staged follow-up message.
    ClearQueuedMessage,
    /// Remove a task that is still waiting for a free lane (scheduler only).
    CancelPending(u64),
    /// Remove a session that is not running (scheduler only).
    DismissSession(u64),
    /// Remove every finished session (scheduler only).
    ClearFinished,
    /// Forget the worker's task and return it to a pristine idle slot.
    Reset,
    /// Skip the remaining backoff of the selected task and try again now.
    RetryNow,
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
pub fn prepare_startup_config(
    config: &mut QueueConfig,
    state: &crate::config::PersistedQueueState,
) -> bool {
    let should_auto_send = !persisted_state_requires_recovery(state)
        && config.auto_send_default_prompt
        && !config.default_prompt.trim().is_empty();
    if should_auto_send {
        config.prompt = config.default_prompt.clone();
    }
    should_auto_send
}

pub fn spawn_runtime<T>(
    transport: T,
    mut config: QueueConfig,
    state: crate::config::PersistedQueueState,
    store: Option<ConfigStore>,
) -> RuntimeHandle
where
    T: CodexTransport + 'static,
{
    let auto_start_default = prepare_startup_config(&mut config, &state);
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
    let mut initial = machine.snapshot().clone();
    initial.session_prompt = config.prompt.clone();
    initial.session_working_directory = config.working_directory.clone();
    initial.session_failure_phrases = config.failure_phrases.clone();
    initial.running_session_count = usize::from(initial.phase.is_running());
    initial.pending_task_count = usize::from(initial.queued_message.is_some());
    initial.max_concurrent_sessions = config.max_concurrent_sessions;
    let (commands, command_receiver) = mpsc::channel(32);
    let (snapshot_sender, snapshot_receiver) = watch::channel(initial);
    let mut runtime = QueueRuntime::new(transport, config, machine, store, snapshot_sender);
    // A persisted active task is resumed automatically.  Paused, successful,
    // and fatal states remain user-controlled and never start on their own.
    if auto_start_default {
        runtime.persist_config();
    }
    runtime.started = (active_phase && prompt_matches) || auto_start_default;
    let join = tokio::spawn(async move { runtime.run(command_receiver).await });
    RuntimeHandle {
        commands,
        snapshot: snapshot_receiver,
        join,
    }
}

/// The deterministic controller.  It is public so integration tests can use a
/// fake app-server without opening a desktop window.
#[allow(clippy::struct_excessive_bools)]
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
    /// Whether the logical user message for `config.prompt` is already in the
    /// local conversation history.  Retries must not add duplicate bubbles.
    current_prompt_recorded: bool,
    /// Set by an explicit user resume: an interrupted turn found while
    /// reconnecting is continued right away instead of after a backoff.
    resume_requested: bool,
    persistence: PersistenceHealth,
}

#[derive(Debug, Default)]
struct PersistenceHealth {
    state_failures: u32,
    config_failed: bool,
}

/// Whether a restored turn is the one Longwatch submitted last, judged by the
/// `clientUserMessageId` echoed on its user message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnOwnership {
    Ours,
    NotOurs,
    /// No id was recorded, or the server does not report client ids.
    Unknown,
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
        transport.set_app_server_window_hidden(true);
        let now = Utc::now();
        let monotonic = Instant::now();
        let current_prompt_recorded = machine
            .snapshot()
            .conversation
            .iter()
            .rev()
            .find(|message| message.role == ConversationRole::User)
            .is_some_and(|message| message.content == config.prompt);
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
            current_prompt_recorded,
            resume_requested: false,
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
                            let result = self.handle_start_configured(config, None).await;
                            self.machine.mark_request_processed(0);
                            self.publish();
                            result?;
                        }
                        Some(RuntimeCommand::SubmitConfigured { config, request_id }) => {
                            let result = self.handle_start_configured(config, Some(request_id)).await;
                            self.machine.mark_request_processed(request_id);
                            self.publish();
                            result?;
                        }
                        Some(RuntimeCommand::Start) => {
                            let result = self.handle_start().await;
                            self.machine.mark_request_processed(0);
                            self.publish();
                            result?;
                        }
                        Some(RuntimeCommand::Resume) => {
                            let result = self.handle_resume().await;
                            self.machine.mark_request_processed(0);
                            self.publish();
                            result?;
                        }
                        Some(RuntimeCommand::QueueMessage(message)) => {
                            self.handle_queue_message(message, None);
                        }
                        Some(RuntimeCommand::QueueMessageWithAck { message, request_id }) => {
                            self.handle_queue_message(message, Some(request_id));
                            self.machine.mark_request_processed(request_id);
                            self.publish();
                        }
                        Some(RuntimeCommand::Pause) => self.handle_pause().await?,
                        Some(RuntimeCommand::Stop) => self.handle_stop().await?,
                        Some(RuntimeCommand::StartNewConfigured { config, request_id }) => {
                            let result = self.handle_start_new_configured(config, request_id).await;
                            self.machine.mark_request_processed(request_id);
                            self.publish();
                            result?;
                        }
                        Some(
                            RuntimeCommand::SelectSession(_)
                            | RuntimeCommand::CancelPending(_)
                            | RuntimeCommand::DismissSession(_)
                            | RuntimeCommand::ClearFinished,
                        ) => {}
                        Some(RuntimeCommand::ClearQueuedMessage) => {
                            if self.machine.snapshot().queued_message.is_some() {
                                self.machine.clear_queued_message();
                                self.machine.set_status("已取消待发送的消息");
                                self.publish_and_persist();
                            }
                        }
                        Some(RuntimeCommand::Reset) => self.handle_reset().await?,
                        Some(RuntimeCommand::RetryNow) => self.handle_retry_now(),
                        Some(RuntimeCommand::Wake) => self.handle_explicit_wake(),
                        Some(RuntimeCommand::Shutdown) => {
                            self.handle_shutdown();
                            break;
                        }
                        None => {
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

    async fn handle_start_configured(
        &mut self,
        config: QueueConfig,
        request_id: Option<u64>,
    ) -> Result<(), RuntimeError> {
        if !self.handle_configure(config) {
            return Ok(());
        }
        if matches!(
            self.machine.snapshot().phase,
            QueuePhase::Success | QueuePhase::Paused | QueuePhase::FatalError
        ) && self.machine.snapshot().active_turn_id.is_none()
        {
            self.machine.prepare_follow_up();
            self.publish_and_persist();
        }
        self.handle_start().await?;
        if let Some(request_id) = request_id
            && (self.started
                || matches!(
                    self.machine.snapshot().phase,
                    QueuePhase::Connecting
                        | QueuePhase::Sending
                        | QueuePhase::Waiting
                        | QueuePhase::Backoff
                        | QueuePhase::Success
                ))
        {
            self.machine.acknowledge_request(request_id);
            self.publish_and_persist();
        }
        Ok(())
    }

    async fn handle_start_new_configured(
        &mut self,
        config: QueueConfig,
        request_id: u64,
    ) -> Result<(), RuntimeError> {
        // A fresh task must never inherit the previous thread. Interrupt and
        // close the old transport first, then clear the local state before
        // applying the new configuration. This command is primarily used by
        // the multi-session manager when reusing an idle slot.
        if self.started || self.machine.snapshot().active_turn_id.is_some() {
            self.handle_stop().await?;
        } else {
            self.shutdown_transport().await;
        }
        self.machine.clear_session();
        self.started = false;
        self.connected = false;
        self.connection_failures = 0;
        self.turn_deadline = None;
        self.success_notified = false;
        self.current_prompt_recorded = false;
        self.config = config;
        if let Err(error) = self.config.validate() {
            self.machine.pause(&format!("配置无效：{error}"));
            self.publish_and_persist();
            return Ok(());
        }
        self.started = true;
        self.machine.reactivate();
        self.machine.acknowledge_request(request_id);
        self.publish_and_persist();
        self.ensure_connection_and_thread().await
    }

    /// Resume the existing Codex thread without consuming whatever happens to
    /// be in the editor as a fresh prompt.  A paused turn is reconciled by
    /// `thread/resume`; if it was interrupted, the normal completion path will
    /// schedule the conservative continuation retry.
    async fn handle_resume(&mut self) -> Result<(), RuntimeError> {
        if self.started {
            return Ok(());
        }
        if self.machine.snapshot().active_thread_id.is_none() {
            return self.handle_start().await;
        }
        if let Err(error) = self.config.validate_settings() {
            self.machine.pause(&format!("配置无效：{error}"));
            self.publish_and_persist();
            return Ok(());
        }
        self.started = true;
        self.success_notified = false;
        if self.machine.snapshot().active_turn_id.is_some() {
            self.machine.mark_retry_as_follow_up();
        }
        self.resume_requested = true;
        self.machine.reactivate();
        self.publish_and_persist();
        self.ensure_connection_and_thread().await
    }

    /// Return this worker to a pristine idle slot: interrupt anything still
    /// running, release app-server and forget the task.
    async fn handle_reset(&mut self) -> Result<(), RuntimeError> {
        if self.started || self.machine.snapshot().active_turn_id.is_some() {
            self.handle_stop().await?;
        } else {
            self.shutdown_transport().await;
        }
        self.machine.clear_session();
        self.started = false;
        self.connection_failures = 0;
        self.turn_deadline = None;
        self.success_notified = false;
        self.current_prompt_recorded = false;
        self.resume_requested = false;
        self.config.prompt.clear();
        self.persist_config();
        self.publish_and_persist();
        Ok(())
    }

    /// Bring a scheduled retry forward to now. The main loop recomputes its
    /// deadline after every command, so the attempt starts immediately.
    fn handle_retry_now(&mut self) {
        let snapshot = self.machine.snapshot();
        if !self.started
            || snapshot.next_attempt_at.is_none()
            || !matches!(snapshot.phase, QueuePhase::Backoff | QueuePhase::Connecting)
        {
            return;
        }
        info!("用户要求立即重试");
        self.machine.set_next_attempt_at(Utc::now());
        self.machine.set_status("正在立即重试");
        self.publish_and_persist();
    }

    fn handle_queue_message(&mut self, message: PendingMessage, request_id: Option<u64>) {
        if message.prompt.trim().is_empty() {
            self.machine.set_status("待发送消息为空，未加入队列");
        } else if !self.started
            || !matches!(
                self.machine.snapshot().phase,
                QueuePhase::Connecting
                    | QueuePhase::Sending
                    | QueuePhase::Waiting
                    | QueuePhase::Backoff
            )
        {
            self.machine
                .set_status("当前没有正在运行的任务，请直接发送消息");
        } else if self.machine.snapshot().queued_message.is_some() {
            self.machine
                .set_status("已有一条待发送消息；请等待它发送后再加入下一条");
        } else {
            self.machine.set_queued_message(message);
            if let Some(request_id) = request_id {
                self.machine.acknowledge_request(request_id);
            }
            self.machine
                .set_status("已加入待发送队列，将在当前任务结束后继续");
        }
        self.publish_and_persist();
    }

    fn handle_configure(&mut self, config: QueueConfig) -> bool {
        if config == self.config {
            return true;
        }
        let prompt_changed = config.prompt != self.config.prompt;
        let task_locked = self.started
            || self.machine.snapshot().active_turn_id.is_some()
            || self.machine.snapshot().submission_uncertain;
        if task_locked {
            let mut accepted = self.config.clone();
            accepted.default_prompt = config.default_prompt.clone();
            accepted.auto_send_default_prompt = config.auto_send_default_prompt;
            accepted.full_screen_flash_enabled = config.full_screen_flash_enabled;
            accepted.audio_alert_enabled = config.audio_alert_enabled;
            accepted.max_concurrent_sessions = config.max_concurrent_sessions;
            accepted.client_identity = config.client_identity;
            accepted
                .default_working_directory
                .clone_from(&config.default_working_directory);
            let only_live_settings_changed = accepted == config;

            self.config.default_prompt = config.default_prompt;
            self.config.auto_send_default_prompt = config.auto_send_default_prompt;
            self.config.full_screen_flash_enabled = config.full_screen_flash_enabled;
            self.config.audio_alert_enabled = config.audio_alert_enabled;
            self.config.max_concurrent_sessions = config.max_concurrent_sessions;
            self.config.default_working_directory = config.default_working_directory;
            // The identity is read on the next connect, so a running task
            // keeps its current connection and picks it up on reconnect.
            self.config.client_identity = config.client_identity;
            self.persist_config();

            if !only_live_settings_changed {
                self.machine.set_runtime_warning(
                    "任务运行中：提醒设置已生效，任务内容和重试参数需停止后修改",
                );
            }
            self.publish_and_persist();
            return only_live_settings_changed;
        }
        if let Err(error) = config.validate_settings() {
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
        if prompt_changed {
            self.current_prompt_recorded = false;
            self.machine.clear_retry_mode();
        }
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
            self.transport
                .set_client_identity(self.config.client_identity);
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
            // 保存的线程在 Codex 侧被删掉后，resume 会返回 "no rollout found"。
            // 反复重试同一个 id 永远不会成功，直接换一个新线程继续排队。
            Err(error) if existing_thread.is_some() && is_missing_thread_error(&error) => {
                warn!(%error, thread_id = ?existing_thread, "原 Codex 会话已不存在，改为新建会话");
                self.machine.clear_thread();
                self.publish_and_persist();
                match self
                    .transport
                    .start_thread(self.config.working_directory.as_deref())
                    .await
                {
                    Ok(session) => session,
                    Err(error) => return self.handle_transport_failure(error).await,
                }
            }
            Err(error) => return self.handle_transport_failure(error).await,
        };
        self.connection_failures = 0;
        self.apply_thread_session(session).await
    }

    async fn apply_thread_session(&mut self, session: ThreadSession) -> Result<(), RuntimeError> {
        self.machine.set_thread(session.id);
        self.publish_and_persist();
        let resume_requested = std::mem::take(&mut self.resume_requested);
        let awaiting_outcome = self.machine.snapshot().active_turn_id.is_some()
            || self.machine.snapshot().submission_uncertain
            || matches!(
                self.machine.snapshot().phase,
                QueuePhase::Sending | QueuePhase::Waiting
            );
        let Some(turn) = session.latest_turn else {
            if self.started {
                // If an earlier process died between `turn/start` and
                // receiving its response, do not immediately resend.  Enter a
                // normal backoff and let the user inspect the persisted state
                // if the server cannot reconcile the thread.
                if awaiting_outcome {
                    self.schedule_retry("无法确认上一次回合是否已提交");
                } else {
                    self.send_attempt().await?;
                }
            }
            return Ok(());
        };
        if awaiting_outcome && self.turn_ownership(&turn) == TurnOwnership::NotOurs {
            // The newest turn carries someone else's clientUserMessageId, so
            // our last request never reached Codex. Treating that older turn
            // as our result would report a stale reply as the outcome.
            self.machine.clear_active_turn();
            self.machine.clear_submission_uncertain();
            if turn.status == TurnStatus::InProgress {
                self.schedule_retry("会话中另有回合正在进行，稍后再发送");
            } else if self.started {
                info!("上一次提交没有到达 Codex，重新发送");
                self.send_attempt().await?;
            }
            return Ok(());
        }
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
            TurnStatus::Interrupted if resume_requested && awaiting_outcome => {
                // The user paused this turn and has now asked to continue, so
                // the continuation goes out at once rather than after backoff.
                self.machine.mark_retry_as_follow_up();
                self.machine.clear_active_turn();
                self.machine.clear_submission_uncertain();
                self.send_attempt().await?;
            }
            TurnStatus::Completed | TurnStatus::Interrupted | TurnStatus::Failed => {
                if awaiting_outcome {
                    self.handle_completed_turn(&turn).await?;
                } else if self.started {
                    self.send_attempt().await?;
                }
            }
        }
        Ok(())
    }

    fn turn_ownership(&self, turn: &RestoredTurn) -> TurnOwnership {
        let Some(pending) = self.machine.snapshot().pending_client_message_id.as_deref() else {
            return TurnOwnership::Unknown;
        };
        if turn.client_message_ids.iter().any(|id| id == pending) {
            TurnOwnership::Ours
        } else if turn.client_ids_reported {
            TurnOwnership::NotOurs
        } else {
            TurnOwnership::Unknown
        }
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
        let continuation = self.machine.snapshot().retry_as_follow_up;
        if !self.current_prompt_recorded {
            self.machine.append_conversation_message(
                ConversationRole::User,
                &self.config.prompt,
                false,
            );
            self.current_prompt_recorded = true;
        }
        let prompt = if continuation {
            continuation_prompt()
        } else {
            self.config.prompt.clone()
        };
        if let Err(error) = self.machine.begin_sending(now) {
            match error {
                QueueError::TurnAlreadyActive => return Ok(()),
            }
        }
        self.publish_and_persist();
        // Persist the uncertainty marker before crossing the process boundary
        // so a crash at any point during `turn/start` is reconciled by
        // `thread/resume` rather than immediately duplicated. The client
        // message id recorded alongside it identifies the turn afterwards.
        let client_message_id = uuid::Uuid::new_v4().to_string();
        self.machine
            .set_pending_client_message_id(client_message_id.clone());
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
                &prompt,
                self.config.working_directory.as_deref(),
                &client_message_id,
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
                self.machine.set_reply_notice(&error.to_string());
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
                    self.machine.set_reply_notice(&conversation_error);
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
                        self.shutdown_transport().await;
                        self.publish_and_persist();
                    }
                    TurnDecision::Success => {}
                }
            }
            TransportEvent::TurnCompleted { thread_id, turn } => {
                if self.machine.snapshot().active_thread_id.as_deref() == Some(thread_id.as_str())
                    && self.machine.snapshot().active_turn_id.as_deref() == Some(turn.id.as_str())
                {
                    let mut turn = turn;
                    // turn/completed 从 0.160 起只带 notLoaded 的空 items，
                    // 正文依赖流式阶段的 item/completed 缓存。缓存缺失时
                    // 宁可回捞一次历史，也不能把"有答案"误判成空回复。
                    if matches!(turn.status, TurnStatus::Completed)
                        && turn.final_message.trim().is_empty()
                        && let Some(text) = self
                            .transport
                            .fetch_turn_message(&thread_id, &turn.id)
                            .await
                    {
                        turn.final_message = text;
                    }
                    self.handle_completed_turn(&turn).await?;
                }
            }
            TransportEvent::ThreadSystemError { thread_id } => {
                if !self.started {
                    return Ok(());
                }
                if self.machine.snapshot().active_thread_id.as_deref() != Some(thread_id.as_str()) {
                    return Ok(());
                }
                if !matches!(
                    self.machine.snapshot().phase,
                    QueuePhase::Sending | QueuePhase::Waiting
                ) {
                    return Ok(());
                }
                // systemError 之后不会再有 turn 事件，等下去只能等到超时。
                self.turn_deadline = None;
                self.machine.clear_active_turn();
                self.shutdown_transport().await;
                self.schedule_retry("Codex 线程进入系统错误状态，正在重建连接");
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
        if turn.final_message.trim().is_empty() {
            self.machine.set_reply_notice(&conversation_text);
        } else {
            self.machine.set_reply_preview(&conversation_text);
        }
        if !matches!(turn.status, TurnStatus::InProgress) {
            self.machine.set_reply_completed_at(turn.completed_at);
            self.machine.clear_submission_uncertain();
        }
        self.turn_deadline = None;
        match decision {
            TurnDecision::Success => {
                self.machine.append_conversation_message(
                    ConversationRole::Assistant,
                    &turn.final_message,
                    false,
                );
                self.machine.succeed("任务成功完成");
                info!(
                    attempts = self.machine.snapshot().attempt_count,
                    "Longwatch 任务已成功完成"
                );
                self.notify_success();
                if self.machine.snapshot().queued_message.is_some() {
                    self.start_queued_message().await?;
                } else {
                    self.started = false;
                }
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
                let terminal_text = if conversation_text.trim().is_empty() {
                    reason.clone()
                } else {
                    conversation_text
                };
                self.machine.append_conversation_message(
                    ConversationRole::Assistant,
                    &terminal_text,
                    true,
                );
                self.machine.pause(&reason);
                self.started = false;
            }
        }
        // A finished or paused task no longer needs its app-server. Releasing
        // it keeps idle sessions from each holding a Codex process; a
        // follow-up reconnects and resumes the same thread.
        if !self.started {
            self.shutdown_transport().await;
        }
        self.publish_and_persist();
        Ok(())
    }

    async fn start_queued_message(&mut self) -> Result<(), RuntimeError> {
        let Some(message) = self.machine.take_queued_message() else {
            self.started = false;
            return Ok(());
        };
        self.config.prompt = message.prompt;
        self.config.working_directory = message.working_directory;
        self.config.failure_phrases = message.failure_phrases;
        self.current_prompt_recorded = false;
        self.success_notified = false;
        self.machine.prepare_follow_up();
        self.started = true;
        self.persist_config();
        if self.connected && self.machine.snapshot().active_thread_id.is_some() {
            self.send_attempt().await?;
        } else {
            // Let the main loop perform connection establishment on its next
            // deadline.  Calling ensure_connection_and_thread here would make
            // the completion -> queued-message path recursively await itself.
            self.machine.begin_connecting();
            self.machine.set_next_attempt_at(Utc::now());
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
        self.machine.set_reply_notice(&error_text);
        self.schedule_connection_retry(&format!("{error_text}；正在恢复线程"));
        Ok(())
    }

    async fn handle_transport_failure(
        &mut self,
        error: TransportError,
    ) -> Result<(), RuntimeError> {
        let error_text = error.to_string();
        self.machine.set_reply_notice(&error_text);
        match error {
            error @ TransportError::ExecutableNotFound(_) => {
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
                    .set_reply_notice(&error_conversation_text(&error));
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
        if self.machine.snapshot().active_turn_id.is_some() {
            self.machine.mark_retry_as_follow_up();
        }
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
        self.shutdown_transport().await;
        self.publish_and_persist();
        Ok(())
    }

    async fn handle_stop(&mut self) -> Result<(), RuntimeError> {
        self.started = false;
        let active_turn = (
            self.machine.snapshot().active_thread_id.clone(),
            self.machine.snapshot().active_turn_id.clone(),
        );
        self.machine.stop_current_turn();
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

    fn handle_shutdown(&mut self) {
        self.started = false;
        self.turn_deadline = None;
        self.machine.clear_session();
        self.publish_and_persist();
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
                self.machine.mark_retry_as_follow_up();
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
        if self.machine.snapshot().active_turn_id.is_some() {
            self.machine.mark_retry_as_follow_up();
        }
        if self.machine.snapshot().reply_preview.trim().is_empty() {
            self.machine.set_reply_notice(reason);
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
        if self.machine.snapshot().active_turn_id.is_some() {
            self.machine.mark_retry_as_follow_up();
        }
        if self.machine.snapshot().reply_preview.trim().is_empty() {
            self.machine.set_reply_notice(reason);
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
        if self.machine.snapshot().active_turn_id.is_some() {
            self.machine.mark_retry_as_follow_up();
        }
        if self.machine.snapshot().reply_preview.trim().is_empty() {
            self.machine.set_reply_notice(reason);
        }
        let delay = self.config.retry_policy.high_demand_delay(&mut self.rng);
        let next = self.machine.schedule_quiet_retry(now, delay, reason);
        info!(
            retry_round = self.machine.snapshot().consecutive_retries,
            next_attempt_at = %next,
            delay_secs = delay.as_secs(),
            reason,
            "高需求提示已按官方节奏重新排队"
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
        let mut snapshot = self.machine.snapshot().clone();
        snapshot.session_prompt = self.config.prompt.clone();
        snapshot.session_working_directory = self.config.working_directory.clone();
        snapshot.session_failure_phrases = self.config.failure_phrases.clone();
        snapshot.running_session_count = usize::from(snapshot.phase.is_running());
        snapshot.pending_task_count = usize::from(snapshot.queued_message.is_some());
        snapshot.max_concurrent_sessions = self.config.max_concurrent_sessions;
        let _ = self.snapshot_sender.send(snapshot);
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

/// Spawn a scheduler capable of keeping several independent Codex sessions
/// alive at the same time.
///
/// The original [`spawn_runtime`] API remains the implementation used for the
/// default one-session mode. This manager composes that battle-tested worker
/// unchanged: each slot gets its own transport, thread, retry state and
/// persistence namespace, while the manager only assigns work and aggregates
/// snapshots for the UI.
pub fn spawn_multi_runtime<F, T>(
    initial_transport: T,
    config: QueueConfig,
    state: PersistedQueueState,
    store: Option<ConfigStore>,
    transport_factory: F,
) -> RuntimeHandle
where
    F: Fn() -> T + Send + Sync + 'static,
    T: CodexTransport + 'static,
{
    let initial_snapshot = QueueSnapshot {
        session_id: Some(1),
        max_concurrent_sessions: config.max_concurrent_sessions,
        session_prompt: config.prompt.clone(),
        session_working_directory: config.working_directory.clone(),
        session_failure_phrases: config.failure_phrases.clone(),
        ..QueueSnapshot::default()
    };
    let (commands, command_receiver) = mpsc::channel(64);
    let (snapshot_sender, snapshot_receiver) = watch::channel(initial_snapshot);
    let manager = MultiSessionRuntime::new(
        initial_transport,
        config,
        state,
        store,
        Arc::new(transport_factory),
        snapshot_sender,
    );
    let join = tokio::spawn(async move { manager.run(command_receiver).await });
    RuntimeHandle {
        commands,
        snapshot: snapshot_receiver,
        join,
    }
}

/// Convenience constructor for production callers that create the official
/// stdio transport. Keeping the generic factory API above makes the scheduler
/// straightforward to exercise with deterministic test transports.
pub fn spawn_multi_app_server_runtime(
    config: QueueConfig,
    state: PersistedQueueState,
    store: Option<ConfigStore>,
) -> RuntimeHandle {
    let path = config.codex_path.clone();
    let initial = crate::app_server::AppServerTransport::new(path.clone()).with_hidden_window(true);
    spawn_multi_runtime(initial, config, state, store, move || {
        crate::app_server::AppServerTransport::new(path.clone()).with_hidden_window(true)
    })
}

#[derive(Clone, Debug)]
struct PendingTask {
    /// Stable id so the UI can show and cancel a waiting task.
    id: u64,
    config: QueueConfig,
    request_id: u64,
    target_session_id: Option<u64>,
    resume_existing: bool,
}

/// Finished sessions stay listed so their results remain visible; beyond this
/// many, the oldest are dismissed automatically.
const MAX_FINISHED_SESSIONS: usize = 20;

struct WorkerSlot<T> {
    id: u64,
    handle: RuntimeHandle,
    snapshot_receiver: watch::Receiver<QueueSnapshot>,
    snapshot: QueueSnapshot,
    config: QueueConfig,
    /// Set while a task has been assigned but its worker has not yet reached a
    /// terminal phase. This closes the small polling gap between dispatches.
    assigned: bool,
    /// Snapshot counter recorded immediately before a request-bearing start
    /// command is sent. A terminal snapshot cannot release the slot until the
    /// worker has processed that command and advanced this counter.
    assignment_processed_count: Option<u64>,
    /// Pending task this slot was started from, if any.
    origin_pending_id: Option<u64>,
    /// Monotonic assignment order; the oldest finished slots are pruned first.
    assigned_order: u64,
    _transport: std::marker::PhantomData<T>,
}

impl<T> WorkerSlot<T> {
    fn reserve_for_start(&mut self, order: u64) {
        self.assigned = true;
        self.assignment_processed_count = Some(self.snapshot.processed_request_count);
        self.assigned_order = order;
    }

    /// Whether this slot has ever carried a task. Pristine slots are reused
    /// for new work and hidden from the task list.
    fn has_history(&self) -> bool {
        self.assigned
            || self.snapshot.attempt_count > 0
            || self.snapshot.active_thread_id.is_some()
            || self.snapshot.phase != QueuePhase::Idle
            || !self.snapshot.session_prompt.trim().is_empty()
    }

    /// Done and safe to dismiss: not running and not merely paused.
    fn is_finished(&self) -> bool {
        self.has_history()
            && !self.assigned
            && !self.snapshot.phase.is_running()
            && self.snapshot.phase != QueuePhase::Paused
    }
}

impl<T> std::fmt::Debug for WorkerSlot<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerSlot")
            .field("id", &self.id)
            .field("phase", &self.snapshot.phase)
            .field("assigned", &self.assigned)
            .field(
                "assignment_processed_count",
                &self.assignment_processed_count,
            )
            .finish_non_exhaustive()
    }
}

struct MultiSessionRuntime<F, T>
where
    F: Fn() -> T + Send + Sync + 'static,
    T: CodexTransport + 'static,
{
    factory: Arc<F>,
    initial_transport: Option<T>,
    base_config: QueueConfig,
    initial_state: PersistedQueueState,
    store: Option<ConfigStore>,
    slots: Vec<WorkerSlot<T>>,
    pending: VecDeque<PendingTask>,
    selected_session_id: u64,
    next_session_id: u64,
    next_pending_id: u64,
    assignment_counter: u64,
    accepted_request_id: u64,
    processed_request_id: u64,
    last_submission: Option<SubmissionPlacement>,
    snapshot_sender: watch::Sender<QueueSnapshot>,
    last_published: Option<QueueSnapshot>,
}

impl<F, T> MultiSessionRuntime<F, T>
where
    F: Fn() -> T + Send + Sync + 'static,
    T: CodexTransport + 'static,
{
    fn new(
        initial_transport: T,
        base_config: QueueConfig,
        initial_state: PersistedQueueState,
        store: Option<ConfigStore>,
        factory: Arc<F>,
        snapshot_sender: watch::Sender<QueueSnapshot>,
    ) -> Self {
        Self {
            factory,
            initial_transport: Some(initial_transport),
            base_config,
            initial_state,
            store,
            slots: Vec::new(),
            pending: VecDeque::new(),
            selected_session_id: 1,
            next_session_id: 2,
            next_pending_id: 1,
            assignment_counter: 0,
            accepted_request_id: 0,
            processed_request_id: 0,
            last_submission: None,
            snapshot_sender,
            last_published: None,
        }
    }

    async fn run(
        mut self,
        mut commands: mpsc::Receiver<RuntimeCommand>,
    ) -> Result<(), RuntimeError> {
        if let Err(error) = self.initialize().await {
            self.shutdown_all().await;
            return Err(error);
        }
        self.publish();
        let mut poll = time::interval(Duration::from_millis(80));
        poll.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                command = commands.recv() => {
                    match command {
                        Some(command) => {
                            if self.handle_command(command).await? {
                                self.refresh_workers();
                                self.dispatch_pending().await?;
                                self.publish();
                            } else {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = poll.tick() => {
                    self.refresh_workers();
                    self.dispatch_pending().await?;
                    self.publish();
                }
            }
        }
        self.shutdown_all().await;
        Ok(())
    }

    async fn initialize(&mut self) -> Result<(), RuntimeError> {
        let scheduler_state = self
            .store
            .as_ref()
            .map(ConfigStore::load_scheduler_state)
            .transpose()?;
        if let Some(scheduler_state) = scheduler_state {
            self.selected_session_id = scheduler_state.selected_session_id.unwrap_or(1);
            self.next_session_id = scheduler_state.next_session_id.max(2);
            for task in scheduler_state.pending_tasks {
                let id = self.allocate_pending_id();
                self.pending.push_back(PendingTask {
                    id,
                    config: task.config,
                    // Restored tasks are not tied to the fresh UI process.
                    request_id: 0,
                    target_session_id: task.target_session_id,
                    resume_existing: task.resume_existing,
                });
            }
        }

        let first_transport = self
            .initial_transport
            .take()
            .ok_or(RuntimeError::Invariant("缺少初始 Codex transport"))?;
        let mut first_config = self.base_config.clone();
        let mut first_state = self.initial_state.clone();
        if !persisted_state_requires_recovery(&first_state) {
            first_state = PersistedQueueState::default();
            if let Some(store) = self.store.as_ref() {
                store.save_state(&first_state)?;
            }
        }
        if (first_config.default_prompt.trim().is_empty() || !first_config.auto_send_default_prompt)
            && !persisted_state_requires_recovery(&first_state)
        {
            first_config.prompt.clear();
        }
        self.spawn_slot(
            1,
            first_transport,
            first_config,
            first_state,
            self.store.clone(),
        )
        .await?;

        // Restore only work that can still make progress. Completed and failed
        // sessions are temporary UI history and are removed on startup.
        if let Some(store) = self.store.clone() {
            let mut active_count = self.active_slot_count();
            let active_limit = usize::from(self.base_config.max_concurrent_sessions);
            for id in store.session_ids()?.into_iter().filter(|id| *id != 1) {
                let session_store = store.session(id);
                let mut session_config = session_store
                    .load_config()
                    .unwrap_or_else(|_| self.base_config.clone());
                session_config.auto_send_default_prompt = false;
                let mut session_state = session_store.load_state().unwrap_or_default();
                if !persisted_state_requires_recovery(&session_state) {
                    store.remove_session(id)?;
                    continue;
                }
                if session_state.phase.is_running() {
                    if active_count < active_limit {
                        active_count += 1;
                    } else {
                        session_state.phase = QueuePhase::Paused;
                        session_state.status_message =
                            "并发会话上限已降低；此会话已安全暂停，选择后可继续".into();
                        session_state.next_attempt_at = None;
                    }
                }
                let transport = (self.factory)();
                self.spawn_slot(
                    id,
                    transport,
                    session_config,
                    session_state,
                    Some(session_store),
                )
                .await?;
                self.next_session_id = self.next_session_id.max(id.saturating_add(1));
            }
        }
        if !self
            .slots
            .iter()
            .any(|slot| slot.id == self.selected_session_id)
        {
            self.selected_session_id = self.slots.first().map_or(1, |slot| slot.id);
        }
        Ok(())
    }

    async fn spawn_slot(
        &mut self,
        id: u64,
        transport: T,
        mut config: QueueConfig,
        state: PersistedQueueState,
        store: Option<ConfigStore>,
    ) -> Result<(), RuntimeError> {
        config.max_concurrent_sessions = self.base_config.max_concurrent_sessions;
        let restore_is_active = state.phase.is_running();
        let auto_start_is_active = state.phase == QueuePhase::Idle
            && config.auto_send_default_prompt
            && !config.default_prompt.trim().is_empty();
        let handle = spawn_runtime(transport, config.clone(), state, store);
        let snapshot_receiver = handle.snapshot();
        let mut snapshot = snapshot_receiver.borrow().clone();
        snapshot.session_id = Some(id);
        snapshot.session_prompt = config.prompt.clone();
        snapshot.session_working_directory = config.working_directory.clone();
        snapshot.session_failure_phrases = config.failure_phrases.clone();
        let assigned = snapshot.phase.is_running() || restore_is_active || auto_start_is_active;
        let assigned_order = self.next_assignment_order();
        self.slots.push(WorkerSlot {
            id,
            handle,
            snapshot_receiver,
            snapshot,
            config,
            assigned,
            assignment_processed_count: None,
            origin_pending_id: None,
            assigned_order,
            _transport: std::marker::PhantomData,
        });
        Ok(())
    }

    async fn handle_command(&mut self, command: RuntimeCommand) -> Result<bool, RuntimeError> {
        match command {
            RuntimeCommand::Configure(config) => {
                self.configure(config).await;
                Ok(true)
            }
            RuntimeCommand::StartConfigured(config) => {
                self.start_configured(config, None).await?;
                Ok(true)
            }
            RuntimeCommand::SubmitConfigured { config, request_id } => {
                let result = self.start_configured(config, Some(request_id)).await;
                self.processed_request_id = self.processed_request_id.max(request_id);
                result?;
                Ok(true)
            }
            RuntimeCommand::StartNewConfigured { config, request_id } => {
                let result = self.start_new_task(config, request_id, true).await;
                self.processed_request_id = self.processed_request_id.max(request_id);
                result?;
                Ok(true)
            }
            RuntimeCommand::Start | RuntimeCommand::Resume => {
                self.start_or_queue_selected(true).await?;
                Ok(true)
            }
            RuntimeCommand::QueueMessageWithAck {
                message,
                request_id,
            } => {
                self.queue_message(message, Some(request_id)).await?;
                Ok(true)
            }
            RuntimeCommand::QueueMessage(message) => {
                self.queue_message(message, None).await?;
                Ok(true)
            }
            RuntimeCommand::Pause => {
                if let Some(slot) = self.selected_slot_mut() {
                    let _ = slot.handle.send(RuntimeCommand::Pause).await;
                }
                Ok(true)
            }
            RuntimeCommand::Stop => {
                if let Some(slot) = self.selected_slot_mut() {
                    let _ = slot.handle.send(RuntimeCommand::Stop).await;
                    slot.assigned = false;
                    slot.assignment_processed_count = None;
                }
                Ok(true)
            }
            RuntimeCommand::SelectSession(id) => {
                if self.slots.iter().any(|slot| slot.id == id) {
                    self.selected_session_id = id;
                    self.persist_scheduler()?;
                }
                Ok(true)
            }
            RuntimeCommand::ClearQueuedMessage => {
                if let Some(slot) = self.selected_slot_mut() {
                    let _ = slot.handle.send(RuntimeCommand::ClearQueuedMessage).await;
                }
                Ok(true)
            }
            RuntimeCommand::RetryNow => {
                if let Some(slot) = self.selected_slot_mut() {
                    let _ = slot.handle.send(RuntimeCommand::RetryNow).await;
                }
                Ok(true)
            }
            RuntimeCommand::CancelPending(id) => {
                let waiting = self.pending.len();
                self.pending.retain(|task| task.id != id);
                if self.pending.len() != waiting {
                    self.persist_scheduler()?;
                }
                Ok(true)
            }
            RuntimeCommand::DismissSession(id) => {
                self.dismiss_session(id).await;
                self.persist_scheduler()?;
                Ok(true)
            }
            RuntimeCommand::ClearFinished => {
                let finished = self
                    .slots
                    .iter()
                    .filter(|slot| slot.is_finished())
                    .map(|slot| slot.id)
                    .collect::<Vec<_>>();
                for id in finished {
                    self.dismiss_session(id).await;
                }
                self.persist_scheduler()?;
                Ok(true)
            }
            RuntimeCommand::Reset => Ok(true),
            RuntimeCommand::Wake => {
                if let Some(slot) = self.selected_slot_mut() {
                    let _ = slot.handle.send(RuntimeCommand::Wake).await;
                }
                Ok(true)
            }
            RuntimeCommand::Shutdown => {
                self.pending.clear();
                self.selected_session_id = 1;
                self.next_session_id = 2;
                self.persist_scheduler()?;
                Ok(false)
            }
        }
    }

    async fn configure(&mut self, config: QueueConfig) {
        if let Err(error) = config.validate_settings() {
            if let Some(slot) = self.selected_slot_mut() {
                slot.snapshot.status_message = format!("配置无效：{error}");
            }
            return;
        }
        self.base_config = config.clone();
        for slot in &mut self.slots {
            let mut worker_config = slot.config.clone();
            worker_config.default_prompt = config.default_prompt.clone();
            worker_config.auto_send_default_prompt = config.auto_send_default_prompt;
            worker_config.full_screen_flash_enabled = config.full_screen_flash_enabled;
            worker_config.audio_alert_enabled = config.audio_alert_enabled;
            worker_config.max_concurrent_sessions = config.max_concurrent_sessions;
            worker_config.client_identity = config.client_identity;
            worker_config
                .default_working_directory
                .clone_from(&config.default_working_directory);
            // Task content (prompt and working directory) arrives with each
            // submission; only queue-wide settings reach idle sessions here.
            if !slot.assigned && !slot.snapshot.phase.is_running() {
                worker_config.failure_phrases = config.failure_phrases.clone();
                worker_config.retry_policy = config.retry_policy.clone();
                worker_config.turn_timeout_secs = config.turn_timeout_secs;
                worker_config.codex_path = config.codex_path.clone();
            }
            slot.config = worker_config.clone();
            let _ = slot
                .handle
                .send(RuntimeCommand::Configure(worker_config))
                .await;
        }
        let _ = self.persist_scheduler();
    }

    async fn start_configured(
        &mut self,
        config: QueueConfig,
        request_id: Option<u64>,
    ) -> Result<(), RuntimeError> {
        let selected_id = self.selected_session_id;
        if self.selected_is_running() {
            // A regular submission while the selected task is busy remains a
            // same-session follow-up, matching the original single-session UX.
            let message = PendingMessage {
                prompt: config.prompt.clone(),
                working_directory: config.working_directory.clone(),
                failure_phrases: config.failure_phrases.clone(),
            };
            self.queue_message(message, request_id).await?;
            return Ok(());
        }
        let selected_available = self
            .slots
            .iter()
            .find(|slot| slot.id == selected_id)
            .is_some_and(|slot| !slot.assigned && !slot.snapshot.phase.is_running());
        if selected_available
            && self.active_slot_count() >= usize::from(self.base_config.max_concurrent_sessions)
        {
            let request_id = request_id.unwrap_or_default();
            let pending_id = self.enqueue(config, request_id, Some(selected_id), false);
            self.acknowledge_scheduler_request(request_id);
            self.record_placement(request_id, None, Some(pending_id));
            self.persist_scheduler()?;
            return Ok(());
        }
        if !self.slots.iter().any(|slot| slot.id == selected_id) {
            let request_id = request_id.unwrap_or_default();
            return self.start_new_task(config, request_id, true).await;
        }
        let order = self.next_assignment_order();
        if let Some(slot) = self.selected_slot_mut() {
            slot.config = config.clone();
            slot.reserve_for_start(order);
            let command = match request_id {
                Some(request_id) => RuntimeCommand::SubmitConfigured { config, request_id },
                None => RuntimeCommand::StartConfigured(config),
            };
            slot.handle
                .send(command)
                .await
                .map_err(|_| RuntimeError::ChannelClosed)?;
        }
        self.record_placement(request_id.unwrap_or_default(), Some(selected_id), None);
        Ok(())
    }

    async fn start_or_queue_selected(&mut self, resume_existing: bool) -> Result<(), RuntimeError> {
        let selected_id = self.selected_session_id;
        let Some(index) = self.slots.iter().position(|slot| slot.id == selected_id) else {
            return Ok(());
        };
        if self.slots[index].assigned || self.slots[index].snapshot.phase.is_running() {
            return Ok(());
        }
        if self.active_slot_count() >= usize::from(self.base_config.max_concurrent_sessions) {
            if !self
                .pending
                .iter()
                .any(|task| task.target_session_id == Some(selected_id))
            {
                let config = self.slots[index].config.clone();
                self.enqueue(config, 0, Some(selected_id), resume_existing);
                self.persist_scheduler()?;
            }
            return Ok(());
        }
        let order = self.next_assignment_order();
        let slot = &mut self.slots[index];
        slot.reserve_for_start(order);
        slot.handle
            .send(if resume_existing {
                RuntimeCommand::Resume
            } else {
                RuntimeCommand::Start
            })
            .await
            .map_err(|_| RuntimeError::ChannelClosed)
    }

    async fn start_new_task(
        &mut self,
        config: QueueConfig,
        request_id: u64,
        select_session: bool,
    ) -> Result<(), RuntimeError> {
        if let Err(error) = config.validate() {
            if let Some(slot) = self.selected_slot_mut() {
                slot.snapshot.status_message = format!("配置无效：{error}");
            }
            return Ok(());
        }
        if self.active_slot_count() >= usize::from(self.base_config.max_concurrent_sessions) {
            let pending_id = self.enqueue(config, request_id, None, false);
            self.acknowledge_scheduler_request(request_id);
            self.record_placement(request_id, None, Some(pending_id));
            self.persist_scheduler()?;
            return Ok(());
        }
        let session_id = self.assign_new_task(config, request_id, None).await?;
        if select_session {
            self.selected_session_id = session_id;
        }
        self.acknowledge_scheduler_request(request_id);
        self.record_placement(request_id, Some(session_id), None);
        self.persist_scheduler()?;
        Ok(())
    }

    /// Put a fresh task into a pristine lane, creating one when none is free.
    /// Finished sessions are never recycled, so their results stay visible.
    async fn assign_new_task(
        &mut self,
        mut config: QueueConfig,
        request_id: u64,
        origin_pending_id: Option<u64>,
    ) -> Result<u64, RuntimeError> {
        config.max_concurrent_sessions = self.base_config.max_concurrent_sessions;
        config.auto_send_default_prompt = false;
        let index = if let Some(index) = self.available_slot_index() {
            index
        } else {
            let id = self.allocate_session_id();
            let store = self.store.as_ref().map(|store| store.session(id));
            let transport = (self.factory)();
            self.spawn_slot(
                id,
                transport,
                config.clone(),
                PersistedQueueState::default(),
                store,
            )
            .await?;
            self.slots.len() - 1
        };
        let order = self.next_assignment_order();
        let slot = &mut self.slots[index];
        slot.config = config.clone();
        slot.origin_pending_id = origin_pending_id;
        slot.reserve_for_start(order);
        let session_id = slot.id;
        slot.handle
            .send(RuntimeCommand::StartNewConfigured { config, request_id })
            .await
            .map_err(|_| RuntimeError::ChannelClosed)?;
        self.prune_finished_sessions().await;
        Ok(session_id)
    }

    async fn queue_message(
        &mut self,
        message: PendingMessage,
        request_id: Option<u64>,
    ) -> Result<(), RuntimeError> {
        let selected_id = self.selected_session_id;
        let selected_running = self.selected_is_running();
        let Some(index) = self.slots.iter().position(|slot| slot.id == selected_id) else {
            if let Some(request_id) = request_id {
                let config = QueueConfig {
                    prompt: message.prompt,
                    working_directory: message.working_directory,
                    failure_phrases: message.failure_phrases,
                    ..self.base_config.clone()
                };
                let pending_id = self.enqueue(config, request_id, None, false);
                self.acknowledge_scheduler_request(request_id);
                self.record_placement(request_id, None, Some(pending_id));
                self.persist_scheduler()?;
            }
            return Ok(());
        };
        if !selected_running {
            let config = QueueConfig {
                prompt: message.prompt,
                working_directory: message.working_directory,
                failure_phrases: message.failure_phrases,
                ..self.slots[index].config.clone()
            };
            if self.active_slot_count() >= usize::from(self.base_config.max_concurrent_sessions) {
                let request_id = request_id.unwrap_or_default();
                let pending_id = self.enqueue(config, request_id, Some(selected_id), false);
                self.acknowledge_scheduler_request(request_id);
                self.record_placement(request_id, None, Some(pending_id));
                self.persist_scheduler()?;
                return Ok(());
            }
            let order = self.next_assignment_order();
            let slot = &mut self.slots[index];
            slot.config = config.clone();
            slot.reserve_for_start(order);
            let command = match request_id {
                Some(request_id) => RuntimeCommand::SubmitConfigured { config, request_id },
                None => RuntimeCommand::StartConfigured(config),
            };
            slot.handle
                .send(command)
                .await
                .map_err(|_| RuntimeError::ChannelClosed)?;
            self.record_placement(request_id.unwrap_or_default(), Some(selected_id), None);
            return Ok(());
        }
        let slot = &mut self.slots[index];
        slot.handle
            .send(match request_id {
                Some(request_id) => RuntimeCommand::QueueMessageWithAck {
                    message,
                    request_id,
                },
                None => RuntimeCommand::QueueMessage(message),
            })
            .await
            .map_err(|_| RuntimeError::ChannelClosed)?;
        self.record_placement(request_id.unwrap_or_default(), Some(selected_id), None);
        Ok(())
    }

    async fn dispatch_pending(&mut self) -> Result<(), RuntimeError> {
        loop {
            if self.pending.is_empty()
                || self.active_slot_count() >= usize::from(self.base_config.max_concurrent_sessions)
            {
                break;
            }
            if let Some(target_id) = self.pending.front().and_then(|task| task.target_session_id) {
                let Some(index) = self.slots.iter().position(|slot| slot.id == target_id) else {
                    let mut task = self
                        .pending
                        .pop_front()
                        .ok_or(RuntimeError::Invariant("调度队列状态异常"))?;
                    task.target_session_id = None;
                    task.resume_existing = false;
                    self.pending.push_front(task);
                    continue;
                };
                if self.slots[index].assigned || self.slots[index].snapshot.phase.is_running() {
                    break;
                }
                let task = self
                    .pending
                    .pop_front()
                    .ok_or(RuntimeError::Invariant("调度队列状态异常"))?;
                let order = self.next_assignment_order();
                let slot = &mut self.slots[index];
                slot.config = task.config.clone();
                slot.origin_pending_id = Some(task.id);
                slot.reserve_for_start(order);
                let command = if task.resume_existing {
                    RuntimeCommand::Resume
                } else if task.request_id == 0 {
                    RuntimeCommand::StartConfigured(task.config)
                } else {
                    RuntimeCommand::SubmitConfigured {
                        config: task.config,
                        request_id: task.request_id,
                    }
                };
                slot.handle
                    .send(command)
                    .await
                    .map_err(|_| RuntimeError::ChannelClosed)?;
                continue;
            }
            let task = self
                .pending
                .pop_front()
                .ok_or(RuntimeError::Invariant("调度队列状态异常"))?;
            self.assign_new_task(task.config, task.request_id, Some(task.id))
                .await?;
        }
        self.persist_scheduler()?;
        Ok(())
    }

    fn refresh_workers(&mut self) {
        for slot in &mut self.slots {
            slot.snapshot = slot.snapshot_receiver.borrow().clone();
            slot.snapshot.session_id = Some(slot.id);
            slot.config.prompt.clone_from(&slot.snapshot.session_prompt);
            slot.config.working_directory = slot.snapshot.session_working_directory.clone();
            slot.config
                .failure_phrases
                .clone_from(&slot.snapshot.session_failure_phrases);
            if slot
                .assignment_processed_count
                .is_some_and(|baseline| slot.snapshot.processed_request_count > baseline)
            {
                slot.assignment_processed_count = None;
            }
            if slot.assigned
                && slot.assignment_processed_count.is_none()
                && !slot.snapshot.phase.is_running()
                && (slot.snapshot.attempt_count > 0
                    || matches!(
                        slot.snapshot.phase,
                        QueuePhase::Success | QueuePhase::Paused | QueuePhase::FatalError
                    ))
            {
                slot.assigned = false;
            }
        }
    }

    fn publish(&mut self) {
        let selected = self
            .slots
            .iter()
            .find(|slot| slot.id == self.selected_session_id)
            .or_else(|| self.slots.first());
        let mut snapshot =
            selected.map_or_else(QueueSnapshot::default, |slot| slot.snapshot.clone());
        let selected_id = selected.map(|slot| slot.id);
        snapshot.session_id = selected_id;
        snapshot.max_concurrent_sessions = self.base_config.max_concurrent_sessions;
        snapshot.accepted_request_id = self.accepted_request_id.max(snapshot.accepted_request_id);
        snapshot.processed_request_id =
            self.processed_request_id.max(snapshot.processed_request_id);
        snapshot.running_session_count = self.active_slot_count();
        snapshot.pending_task_count = self.pending.len()
            + self
                .slots
                .iter()
                .filter_map(|slot| slot.snapshot.queued_message.as_ref())
                .count();
        snapshot.sessions = self
            .slots
            .iter()
            .map(|slot| QueueSessionSummary {
                id: slot.id,
                phase: slot.snapshot.phase,
                // A conversation keeps the title of its first message even
                // after follow-ups change the current prompt.
                title: session_title(
                    slot.snapshot
                        .conversation
                        .iter()
                        .find(|message| message.role == ConversationRole::User)
                        .map_or(slot.snapshot.session_prompt.as_str(), |message| {
                            message.content.as_str()
                        }),
                    slot.id,
                ),
                status_message: slot.snapshot.status_message.clone(),
                active_thread_id: slot.snapshot.active_thread_id.clone(),
                attempt_count: slot.snapshot.attempt_count,
                queued_message_count: usize::from(slot.snapshot.queued_message.is_some()),
                consecutive_retries: slot.snapshot.consecutive_retries,
                next_attempt_at: slot.snapshot.next_attempt_at,
                first_attempt_at: slot.snapshot.first_attempt_at,
                reply_completed_at: slot.snapshot.reply_completed_at,
                origin_pending_id: slot.origin_pending_id,
                has_history: slot.has_history(),
                active: slot.assigned || slot.snapshot.phase.is_running(),
            })
            .collect();
        snapshot.pending_tasks = self
            .pending
            .iter()
            .map(|task| PendingTaskSummary {
                id: task.id,
                title: session_title(&task.config.prompt, task.id),
                prompt: task.config.prompt.clone(),
                working_directory: task.config.working_directory.clone(),
                target_session_id: task.target_session_id,
            })
            .collect();
        snapshot.last_submission = self.last_submission;
        if self
            .last_published
            .as_ref()
            .is_none_or(|previous| previous != &snapshot)
        {
            let _ = self.snapshot_sender.send(snapshot.clone());
            self.last_published = Some(snapshot);
        }
    }

    fn selected_is_running(&self) -> bool {
        self.slots
            .iter()
            .find(|slot| slot.id == self.selected_session_id)
            .is_some_and(|slot| slot.assigned || slot.snapshot.phase.is_running())
    }

    fn active_slot_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.assigned || slot.snapshot.phase.is_running())
            .count()
    }

    /// A lane that has never carried a task.
    fn available_slot_index(&self) -> Option<usize> {
        self.slots.iter().position(|slot| !slot.has_history())
    }

    fn selected_slot_mut(&mut self) -> Option<&mut WorkerSlot<T>> {
        self.slots
            .iter_mut()
            .find(|slot| slot.id == self.selected_session_id)
    }

    fn allocate_session_id(&mut self) -> u64 {
        let id = self.next_session_id.max(1);
        self.next_session_id = id.saturating_add(1);
        id
    }

    fn allocate_pending_id(&mut self) -> u64 {
        let id = self.next_pending_id.max(1);
        self.next_pending_id = id.saturating_add(1);
        id
    }

    fn next_assignment_order(&mut self) -> u64 {
        self.assignment_counter = self.assignment_counter.saturating_add(1);
        self.assignment_counter
    }

    fn enqueue(
        &mut self,
        config: QueueConfig,
        request_id: u64,
        target_session_id: Option<u64>,
        resume_existing: bool,
    ) -> u64 {
        let id = self.allocate_pending_id();
        self.pending.push_back(PendingTask {
            id,
            config,
            request_id,
            target_session_id,
            resume_existing,
        });
        id
    }

    fn record_placement(
        &mut self,
        request_id: u64,
        session_id: Option<u64>,
        pending_id: Option<u64>,
    ) {
        if request_id != 0 {
            self.last_submission = Some(SubmissionPlacement {
                request_id,
                session_id,
                pending_id,
            });
        }
    }

    /// Remove a session that is not running. The root session owns the
    /// primary state files, so it is reset in place and becomes a pristine
    /// lane; every other worker is shut down and its folder deleted.
    async fn dismiss_session(&mut self, id: u64) {
        let Some(index) = self.slots.iter().position(|slot| slot.id == id) else {
            return;
        };
        if self.slots[index].assigned || self.slots[index].snapshot.phase.is_running() {
            return;
        }
        // Follow-ups staged for this conversation would otherwise start in a
        // fresh thread; they leave together with it.
        self.pending
            .retain(|task| task.target_session_id != Some(id));
        if id == 1 {
            let slot = &mut self.slots[index];
            slot.origin_pending_id = None;
            let _ = slot.handle.send(RuntimeCommand::Reset).await;
        } else {
            let slot = self.slots.remove(index);
            let handle = slot.handle;
            let _ = handle.command_sender().try_send(RuntimeCommand::Shutdown);
            let store = self.store.clone();
            tokio::spawn(async move {
                let _ = time::timeout(Duration::from_secs(3), handle.join()).await;
                if let Some(store) = store
                    && let Err(error) = store.remove_session(id)
                {
                    warn!(session_id = id, %error, "移除会话目录失败");
                }
            });
        }
        if self.selected_session_id == id {
            self.selected_session_id = self
                .slots
                .iter()
                .filter(|slot| slot.id != id && slot.has_history())
                .max_by_key(|slot| slot.assigned_order)
                .map_or(1, |slot| slot.id);
        }
    }

    async fn prune_finished_sessions(&mut self) {
        let mut finished = self
            .slots
            .iter()
            .filter(|slot| slot.is_finished() && slot.id != self.selected_session_id)
            .map(|slot| (slot.assigned_order, slot.id))
            .collect::<Vec<_>>();
        if finished.len() <= MAX_FINISHED_SESSIONS {
            return;
        }
        finished.sort_unstable();
        let excess = finished.len() - MAX_FINISHED_SESSIONS;
        for (_, id) in finished.into_iter().take(excess) {
            self.dismiss_session(id).await;
        }
    }

    fn acknowledge_scheduler_request(&mut self, request_id: u64) {
        self.accepted_request_id = self.accepted_request_id.max(request_id);
        self.processed_request_id = self.processed_request_id.max(request_id);
    }

    fn persist_scheduler(&self) -> Result<(), RuntimeError> {
        let Some(store) = self.store.as_ref() else {
            return Ok(());
        };
        let state = PersistedSchedulerState {
            selected_session_id: Some(self.selected_session_id),
            next_session_id: self.next_session_id,
            pending_tasks: self
                .pending
                .iter()
                .map(|task| PersistedPendingTask {
                    config: task.config.clone(),
                    target_session_id: task.target_session_id,
                    resume_existing: task.resume_existing,
                })
                .collect(),
            ..PersistedSchedulerState::default()
        };
        store.save_scheduler_state(&state)?;
        Ok(())
    }

    async fn shutdown_all(&mut self) {
        let mut slots = std::mem::take(&mut self.slots);
        for slot in &slots {
            let _ = slot
                .handle
                .command_sender()
                .try_send(RuntimeCommand::Shutdown);
        }
        for slot in slots.drain(..) {
            let _ = time::timeout(Duration::from_secs(3), slot.handle.join()).await;
        }
        // Worker shutdown clears its state so a normal app exit does not leave
        // completed temporary conversations behind for the next launch.
        if let Some(store) = self.store.as_ref() {
            if let Err(error) = store.save_state(&PersistedQueueState::default()) {
                warn!(%error, "退出时清理根会话状态失败");
            }
            for id in store.session_ids().unwrap_or_default() {
                if let Err(error) = store.remove_session(id) {
                    warn!(session_id = id, %error, "退出时清理临时会话失败");
                }
            }
        }
    }
}

/// First non-empty line of the prompt. The UI ellipsizes to its own width, so
/// this only bounds the length carried in every snapshot.
fn session_title(prompt: &str, id: u64) -> String {
    const MAX_TITLE_CHARS: usize = 60;
    let Some(line) = prompt.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return format!("新任务 #{id}");
    };
    let mut title = line.chars().take(MAX_TITLE_CHARS).collect::<String>();
    if line.chars().count() > MAX_TITLE_CHARS {
        title.push('…');
    }
    title
}

fn persisted_state_requires_recovery(state: &PersistedQueueState) -> bool {
    state.phase.is_running()
        || state.phase == QueuePhase::Paused
        || state.active_turn_id.is_some()
        || state.submission_uncertain
        || state.queued_message.is_some()
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

/// Prompt used for a new external turn after the previous turn was interrupted
/// or failed.  The persistent Codex thread already contains the original task,
/// so repeating it verbatim can cause duplicated edits or repeated work.
fn continuation_prompt() -> String {
    "请继续完成上一个任务。先检查当前工作区和已有进度，从中断处继续；不要重复已经完成的工作。完成后简要说明本轮新增或修正的内容。".into()
}

/// Codex 会在会话目录被清理后返回 "no rollout found for thread id ..."。
/// 这类错误不能重试，只能换一个新线程。
fn is_missing_thread_error(error: &TransportError) -> bool {
    let TransportError::Rpc { message, .. } = error else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    message.contains("no rollout found")
        || message.contains("thread not found")
        || message.contains("no such thread")
        || message.contains("unknown thread")
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
        config::{ClientIdentity, PersistedQueueState},
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

    #[test]
    fn external_retry_uses_a_continue_instruction_instead_of_the_original_task() {
        let prompt = continuation_prompt();

        assert!(prompt.contains("继续完成上一个任务"));
        assert!(prompt.contains("不要重复已经完成的工作"));
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
            _: &str,
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
    struct ConcurrentTransport {
        instance: u64,
        starts: Arc<AtomicUsize>,
        prompts: Arc<std::sync::Mutex<Vec<(u64, String)>>>,
        event_receiver: mpsc::Receiver<TransportEvent>,
    }

    impl ConcurrentTransport {
        fn new(
            instance: u64,
            starts: Arc<AtomicUsize>,
            prompts: Arc<std::sync::Mutex<Vec<(u64, String)>>>,
        ) -> (Self, mpsc::Sender<TransportEvent>) {
            let (event_sender, event_receiver) = mpsc::channel(16);
            (
                Self {
                    instance,
                    starts,
                    prompts,
                    event_receiver,
                },
                event_sender,
            )
        }
    }

    #[async_trait::async_trait]
    impl CodexTransport for ConcurrentTransport {
        async fn connect(&mut self) -> Result<(), TransportError> {
            Ok(())
        }

        async fn start_thread(
            &mut self,
            _: Option<&std::path::Path>,
        ) -> Result<ThreadSession, TransportError> {
            Ok(ThreadSession {
                id: format!("thread-{}", self.instance),
                latest_turn: None,
            })
        }

        async fn resume_thread(
            &mut self,
            id: &str,
            _: Option<&std::path::Path>,
        ) -> Result<ThreadSession, TransportError> {
            Ok(ThreadSession {
                id: id.to_owned(),
                latest_turn: None,
            })
        }

        async fn start_turn(
            &mut self,
            _: &str,
            prompt: &str,
            _: Option<&std::path::Path>,
            _: &str,
        ) -> Result<StartedTurn, TransportError> {
            let number = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
            self.prompts
                .lock()
                .unwrap()
                .push((self.instance, prompt.to_owned()));
            Ok(StartedTurn {
                id: format!("turn-{}-{number}", self.instance),
            })
        }

        async fn interrupt_turn(&mut self, _: &str, _: &str) -> Result<(), TransportError> {
            Ok(())
        }

        async fn next_event(&mut self) -> Option<TransportEvent> {
            self.event_receiver.recv().await
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
            _: &str,
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

    #[tokio::test]
    async fn enabled_default_message_is_sent_once_on_launch() {
        let (transport, starts, thread_starts) = ScriptedTransport::new(Script::Success);
        let config = QueueConfig {
            default_prompt: "automatic task".into(),
            auto_send_default_prompt: true,
            ..QueueConfig::default()
        };
        let handle = spawn_runtime(transport, config, PersistedQueueState::default(), None);
        let mut snapshots = handle.snapshot();

        wait_for_phase(&mut snapshots, QueuePhase::Success).await;

        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(thread_starts.load(Ordering::SeqCst), 1);
        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn empty_default_message_never_auto_sends() {
        let (transport, starts, thread_starts) = ScriptedTransport::new(Script::Success);
        let config = QueueConfig {
            auto_send_default_prompt: true,
            ..QueueConfig::default()
        };
        let handle = spawn_runtime(transport, config, PersistedQueueState::default(), None);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert_eq!(thread_starts.load(Ordering::SeqCst), 0);
        assert_eq!(handle.snapshot().borrow().phase, QueuePhase::Idle);
        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn an_active_persisted_task_wins_over_startup_auto_send() {
        let resumes = Arc::new(AtomicUsize::new(0));
        let starts = Arc::new(AtomicUsize::new(0));
        let transport = ResumeCompletedTransport {
            resumes: Arc::clone(&resumes),
            starts: Arc::clone(&starts),
        };
        let config = QueueConfig {
            prompt: "persisted task".into(),
            default_prompt: "automatic task".into(),
            auto_send_default_prompt: true,
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

    #[derive(Clone, Copy, Debug)]
    enum Script {
        HighDemand,
        CurrentHighDemandThenSuccess,
        InternalRetryThenSuccess,
        StreamDisconnect,
        MissingThreadThenSuccess,
        Success,
    }

    #[derive(Debug)]
    struct ScriptedTransport {
        script: Script,
        starts: Arc<AtomicUsize>,
        thread_starts: Arc<AtomicUsize>,
        sender: mpsc::Sender<TransportEvent>,
        receiver: mpsc::Receiver<TransportEvent>,
    }

    impl ScriptedTransport {
        fn new(script: Script) -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let (sender, receiver) = mpsc::channel(16);
            let starts = Arc::new(AtomicUsize::new(0));
            let thread_starts = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    script,
                    starts: Arc::clone(&starts),
                    thread_starts: Arc::clone(&thread_starts),
                    sender,
                    receiver,
                },
                starts,
                thread_starts,
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
            self.thread_starts.fetch_add(1, Ordering::SeqCst);
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
            if matches!(self.script, Script::MissingThreadThenSuccess) {
                return Err(TransportError::Rpc {
                    code: -32000,
                    message: format!("no rollout found for thread id {id}"),
                    data: None,
                });
            }
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
            _: &str,
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
                            completed_at: None,
                            client_message_ids: Vec::new(),
                            client_ids_reported: false,
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
                                completed_at: None,
                                client_message_ids: Vec::new(),
                                client_ids_reported: false,
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
                                completed_at: None,
                                client_message_ids: Vec::new(),
                                client_ids_reported: false,
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
                Script::MissingThreadThenSuccess | Script::Success => {
                    self.sender
                        .send(TransportEvent::TurnCompleted {
                            thread_id: "thread".into(),
                            turn: RestoredTurn {
                                id: id.clone(),
                                status: TurnStatus::Completed,
                                final_message: "success".into(),
                                error: None,
                                completed_at: None,
                                client_message_ids: Vec::new(),
                                client_ids_reported: false,
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
    async fn multi_runtime_starts_independent_sessions_and_exposes_aggregate_state() {
        let starts = Arc::new(AtomicUsize::new(0));
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let factory_counter = Arc::new(AtomicUsize::new(1));
        let (initial, _) = ConcurrentTransport::new(1, Arc::clone(&starts), Arc::clone(&prompts));
        let mut config = QueueConfig {
            prompt: "first task".into(),
            max_concurrent_sessions: 2,
            ..QueueConfig::default()
        };
        let handle = spawn_multi_runtime(
            initial,
            config.clone(),
            PersistedQueueState::default(),
            None,
            {
                let starts = Arc::clone(&starts);
                let prompts = Arc::clone(&prompts);
                let counter = Arc::clone(&factory_counter);
                move || {
                    let id = counter.fetch_add(1, Ordering::SeqCst) as u64 + 1;
                    ConcurrentTransport::new(id, Arc::clone(&starts), Arc::clone(&prompts)).0
                }
            },
        );
        let mut snapshots = handle.snapshot();
        handle
            .send(RuntimeCommand::SubmitConfigured {
                config: config.clone(),
                request_id: 1,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().running_session_count >= 1 {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        config.prompt = "second task".into();
        handle
            .send(RuntimeCommand::StartNewConfigured {
                config,
                request_id: 2,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = snapshots.borrow().clone();
                if snapshot.sessions.len() >= 2 && snapshot.running_session_count >= 2 {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        let snapshot = snapshots.borrow().clone();
        assert_eq!(snapshot.sessions.len(), 2);
        assert_eq!(snapshot.running_session_count, 2);
        assert_eq!(snapshot.session_id, Some(2));
        assert_eq!(snapshot.session_prompt, "second task");
        assert_eq!(starts.load(Ordering::SeqCst), 2);

        handle.send(RuntimeCommand::SelectSession(1)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = snapshots.borrow().clone();
                if snapshot.session_id == Some(1) {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(snapshots.borrow().session_prompt, "first task");
        assert_eq!(
            snapshots.borrow().active_thread_id.as_deref(),
            Some("thread-1")
        );
        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn full_session_pool_dispatches_waiting_tasks_in_fifo_order() {
        let starts = Arc::new(AtomicUsize::new(0));
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let senders = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (initial, first_sender) =
            ConcurrentTransport::new(1, Arc::clone(&starts), Arc::clone(&prompts));
        senders.lock().unwrap().push(first_sender);
        let factory_counter = Arc::new(AtomicUsize::new(1));
        let mut config = QueueConfig {
            prompt: "first task".into(),
            max_concurrent_sessions: 2,
            ..QueueConfig::default()
        };
        let handle = spawn_multi_runtime(
            initial,
            config.clone(),
            PersistedQueueState::default(),
            None,
            {
                let starts = Arc::clone(&starts);
                let prompts = Arc::clone(&prompts);
                let senders = Arc::clone(&senders);
                let counter = Arc::clone(&factory_counter);
                move || {
                    let id = counter.fetch_add(1, Ordering::SeqCst) as u64 + 1;
                    let (transport, sender) =
                        ConcurrentTransport::new(id, Arc::clone(&starts), Arc::clone(&prompts));
                    senders.lock().unwrap().push(sender);
                    transport
                }
            },
        );
        let mut snapshots = handle.snapshot();

        handle
            .send(RuntimeCommand::SubmitConfigured {
                config: config.clone(),
                request_id: 1,
            })
            .await
            .unwrap();
        config.prompt = "second task".into();
        handle
            .send(RuntimeCommand::StartNewConfigured {
                config: config.clone(),
                request_id: 2,
            })
            .await
            .unwrap();
        config.prompt = "third task".into();
        handle
            .send(RuntimeCommand::StartNewConfigured {
                config,
                request_id: 3,
            })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = snapshots.borrow().clone();
                if snapshot.running_session_count == 2 && snapshot.pending_task_count == 1 {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(snapshots.borrow().accepted_request_id, 3);

        let first_sender = senders.lock().unwrap()[0].clone();
        first_sender
            .send(TransportEvent::TurnCompleted {
                thread_id: "thread-1".into(),
                turn: RestoredTurn {
                    id: "turn-1-1".into(),
                    status: TurnStatus::Completed,
                    final_message: "first complete".into(),
                    error: None,
                    completed_at: None,
                    client_message_ids: Vec::new(),
                    client_ids_reported: false,
                },
            })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if prompts
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(_, prompt)| prompt == "third task")
                {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        assert_eq!(
            prompts.lock().unwrap().clone(),
            vec![
                (1, "first task".into()),
                (2, "second task".into()),
                // The finished first task keeps its lane (and its result);
                // the waiting task gets a fresh one.
                (3, "third task".into()),
            ]
        );
        assert_eq!(snapshots.borrow().pending_task_count, 0);
        let snapshot = snapshots.borrow().clone();
        assert_eq!(snapshot.sessions.len(), 3);
        assert_eq!(snapshot.sessions[0].phase, QueuePhase::Success);
        assert!(snapshot.sessions[2].origin_pending_id.is_some());
        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn new_tasks_after_a_success_respect_capacity_and_keep_the_finished_lane() {
        let starts = Arc::new(AtomicUsize::new(0));
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let senders = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (initial, sender) =
            ConcurrentTransport::new(1, Arc::clone(&starts), Arc::clone(&prompts));
        senders.lock().unwrap().push(sender);
        let config = QueueConfig {
            prompt: "first task".into(),
            max_concurrent_sessions: 1,
            ..QueueConfig::default()
        };
        let handle = spawn_multi_runtime(
            initial,
            config.clone(),
            PersistedQueueState::default(),
            None,
            {
                let starts = Arc::clone(&starts);
                let prompts = Arc::clone(&prompts);
                move || ConcurrentTransport::new(99, Arc::clone(&starts), Arc::clone(&prompts)).0
            },
        );
        let mut snapshots = handle.snapshot();
        handle
            .send(RuntimeCommand::SubmitConfigured {
                config: config.clone(),
                request_id: 1,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if starts.load(Ordering::SeqCst) == 1 {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        let first_sender = senders.lock().unwrap()[0].clone();
        first_sender
            .send(TransportEvent::TurnCompleted {
                thread_id: "thread-1".into(),
                turn: RestoredTurn {
                    id: "turn-1-1".into(),
                    status: TurnStatus::Completed,
                    final_message: "first complete".into(),
                    error: None,
                    completed_at: None,
                    client_message_ids: Vec::new(),
                    client_ids_reported: false,
                },
            })
            .await
            .unwrap();
        wait_for_phase(&mut snapshots, QueuePhase::Success).await;

        let mut second = config.clone();
        second.prompt = "second task".into();
        handle
            .send(RuntimeCommand::StartNewConfigured {
                config: second,
                request_id: 2,
            })
            .await
            .unwrap();
        let mut third = config;
        third.prompt = "third task".into();
        handle
            .send(RuntimeCommand::StartNewConfigured {
                config: third,
                request_id: 3,
            })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let prompts = prompts.lock().unwrap().clone();
                if prompts.iter().any(|(_, prompt)| prompt == "second task")
                    && snapshots.borrow().pending_task_count == 1
                {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert!(
            !prompts
                .lock()
                .unwrap()
                .iter()
                .any(|(_, prompt)| prompt == "third task")
        );
        assert!(
            prompts
                .lock()
                .unwrap()
                .contains(&(99, "second task".into()))
        );
        let sessions = snapshots.borrow().sessions.clone();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].phase, QueuePhase::Success);

        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn a_paused_session_frees_capacity_but_stays_listed() {
        let starts = Arc::new(AtomicUsize::new(0));
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (initial, _) = ConcurrentTransport::new(1, Arc::clone(&starts), Arc::clone(&prompts));
        let config = QueueConfig {
            prompt: "paused task".into(),
            max_concurrent_sessions: 1,
            ..QueueConfig::default()
        };
        let handle = spawn_multi_runtime(
            initial,
            config.clone(),
            PersistedQueueState::default(),
            None,
            {
                let starts = Arc::clone(&starts);
                let prompts = Arc::clone(&prompts);
                move || ConcurrentTransport::new(2, Arc::clone(&starts), Arc::clone(&prompts)).0
            },
        );
        let mut snapshots = handle.snapshot();
        handle
            .send(RuntimeCommand::SubmitConfigured {
                config: config.clone(),
                request_id: 1,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().running_session_count == 1 {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        handle.send(RuntimeCommand::Pause).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().phase == QueuePhase::Paused
                    && snapshots.borrow().running_session_count == 0
                {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        let mut next = config;
        next.prompt = "replacement task".into();
        handle
            .send(RuntimeCommand::StartNewConfigured {
                config: next,
                request_id: 2,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if prompts
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(_, prompt)| prompt == "replacement task")
                {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let snapshot = snapshots.borrow().clone();
        assert_eq!(snapshot.sessions.len(), 2);
        assert_eq!(snapshot.sessions[0].phase, QueuePhase::Paused);
        assert_eq!(snapshot.running_session_count, 1);

        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn startup_prunes_terminal_sessions_but_keeps_paused_work() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::in_directory(directory.path().to_path_buf());
        store
            .session(2)
            .save_state(&PersistedQueueState::default())
            .unwrap();
        let restored_config = QueueConfig {
            prompt: "restored task".into(),
            max_concurrent_sessions: 1,
            ..QueueConfig::default()
        };
        store.session(3).save_config(&restored_config).unwrap();
        store
            .session(3)
            .save_state(&PersistedQueueState {
                phase: QueuePhase::Success,
                active_thread_id: Some("thread-3".into()),
                attempt_count: 1,
                reply_preview: "done".into(),
                prompt_digest: Some(prompt_digest("restored task")),
                ..PersistedQueueState::default()
            })
            .unwrap();
        let paused_config = QueueConfig {
            prompt: "paused task".into(),
            max_concurrent_sessions: 1,
            ..QueueConfig::default()
        };
        store.session(4).save_config(&paused_config).unwrap();
        store
            .session(4)
            .save_state(&PersistedQueueState {
                phase: QueuePhase::Paused,
                active_thread_id: Some("thread-4".into()),
                prompt_digest: Some(prompt_digest("paused task")),
                ..PersistedQueueState::default()
            })
            .unwrap();

        let starts = Arc::new(AtomicUsize::new(0));
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (initial, _) = ConcurrentTransport::new(1, Arc::clone(&starts), Arc::clone(&prompts));
        let handle = spawn_multi_runtime(
            initial,
            QueueConfig {
                max_concurrent_sessions: 1,
                ..QueueConfig::default()
            },
            PersistedQueueState {
                phase: QueuePhase::Success,
                active_thread_id: Some("old-root-thread".into()),
                reply_preview: "old result".into(),
                ..PersistedQueueState::default()
            },
            Some(store.clone()),
            {
                let starts = Arc::clone(&starts);
                let prompts = Arc::clone(&prompts);
                move || ConcurrentTransport::new(3, Arc::clone(&starts), Arc::clone(&prompts)).0
            },
        );
        let mut snapshots = handle.snapshot();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots
                    .borrow()
                    .sessions
                    .iter()
                    .any(|session| session.id == 4)
                    && !directory.path().join("sessions").join("2").exists()
                    && !directory.path().join("sessions").join("3").exists()
                {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(snapshots.borrow().sessions.len(), 2);
        assert_eq!(snapshots.borrow().running_session_count, 0);
        assert!(!directory.path().join("sessions").join("3").exists());
        assert!(directory.path().join("sessions").join("4").exists());
        assert_eq!(store.load_state().unwrap(), PersistedQueueState::default());

        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
        assert!(!directory.path().join("sessions").join("4").exists());
    }

    #[tokio::test]
    async fn an_idle_selected_session_waits_for_capacity_before_sending() {
        let starts = Arc::new(AtomicUsize::new(0));
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let senders = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (initial, first_sender) =
            ConcurrentTransport::new(1, Arc::clone(&starts), Arc::clone(&prompts));
        senders.lock().unwrap().push(first_sender);
        let counter = Arc::new(AtomicUsize::new(1));
        let mut config = QueueConfig {
            prompt: "first task".into(),
            max_concurrent_sessions: 2,
            ..QueueConfig::default()
        };
        let handle = spawn_multi_runtime(
            initial,
            config.clone(),
            PersistedQueueState::default(),
            None,
            {
                let starts = Arc::clone(&starts);
                let prompts = Arc::clone(&prompts);
                let senders = Arc::clone(&senders);
                let counter = Arc::clone(&counter);
                move || {
                    let id = counter.fetch_add(1, Ordering::SeqCst) as u64 + 1;
                    let (transport, sender) =
                        ConcurrentTransport::new(id, Arc::clone(&starts), Arc::clone(&prompts));
                    senders.lock().unwrap().push(sender);
                    transport
                }
            },
        );
        let mut snapshots = handle.snapshot();
        handle
            .send(RuntimeCommand::SubmitConfigured {
                config: config.clone(),
                request_id: 1,
            })
            .await
            .unwrap();
        config.prompt = "second task".into();
        handle
            .send(RuntimeCommand::StartNewConfigured {
                config: config.clone(),
                request_id: 2,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().running_session_count == 2 {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        let second_sender = senders.lock().unwrap()[1].clone();
        second_sender
            .send(TransportEvent::TurnCompleted {
                thread_id: "thread-2".into(),
                turn: RestoredTurn {
                    id: "turn-2-2".into(),
                    status: TurnStatus::Completed,
                    final_message: "second complete".into(),
                    error: None,
                    completed_at: None,
                    client_message_ids: Vec::new(),
                    client_ids_reported: false,
                },
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().session_id == Some(2)
                    && snapshots.borrow().phase == QueuePhase::Success
                    && snapshots.borrow().running_session_count == 1
                    && snapshots
                        .borrow()
                        .sessions
                        .iter()
                        .find(|session| session.id == 2)
                        .is_some_and(|session| session.phase == QueuePhase::Success)
                {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        let mut reduced = config.clone();
        reduced.max_concurrent_sessions = 1;
        handle
            .send(RuntimeCommand::Configure(reduced.clone()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().max_concurrent_sessions == 1 {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        reduced.prompt = "follow up on second".into();
        handle
            .send(RuntimeCommand::SubmitConfigured {
                config: reduced,
                request_id: 3,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().pending_task_count == 1 {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert!(
            !prompts
                .lock()
                .unwrap()
                .iter()
                .any(|(_, prompt)| prompt == "follow up on second")
        );

        let first_sender = senders.lock().unwrap()[0].clone();
        first_sender
            .send(TransportEvent::TurnCompleted {
                thread_id: "thread-1".into(),
                turn: RestoredTurn {
                    id: "turn-1-1".into(),
                    status: TurnStatus::Completed,
                    final_message: "first complete".into(),
                    error: None,
                    completed_at: None,
                    client_message_ids: Vec::new(),
                    client_ids_reported: false,
                },
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if prompts
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(id, prompt)| *id == 2 && prompt == "follow up on second")
                {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn resuming_a_paused_session_waits_for_capacity() {
        let starts = Arc::new(AtomicUsize::new(0));
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let senders = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (initial, first_sender) =
            ConcurrentTransport::new(1, Arc::clone(&starts), Arc::clone(&prompts));
        senders.lock().unwrap().push(first_sender);
        let counter = Arc::new(AtomicUsize::new(1));
        let mut config = QueueConfig {
            prompt: "first task".into(),
            max_concurrent_sessions: 2,
            ..QueueConfig::default()
        };
        let handle = spawn_multi_runtime(
            initial,
            config.clone(),
            PersistedQueueState::default(),
            None,
            {
                let starts = Arc::clone(&starts);
                let prompts = Arc::clone(&prompts);
                let senders = Arc::clone(&senders);
                let counter = Arc::clone(&counter);
                move || {
                    let id = counter.fetch_add(1, Ordering::SeqCst) as u64 + 1;
                    let (transport, sender) =
                        ConcurrentTransport::new(id, Arc::clone(&starts), Arc::clone(&prompts));
                    senders.lock().unwrap().push(sender);
                    transport
                }
            },
        );
        let mut snapshots = handle.snapshot();
        handle
            .send(RuntimeCommand::SubmitConfigured {
                config: config.clone(),
                request_id: 1,
            })
            .await
            .unwrap();
        config.prompt = "second task".into();
        handle
            .send(RuntimeCommand::StartNewConfigured {
                config: config.clone(),
                request_id: 2,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().running_session_count == 2 {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        handle.send(RuntimeCommand::Pause).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().session_id == Some(2)
                    && snapshots.borrow().phase == QueuePhase::Paused
                    && snapshots.borrow().running_session_count == 1
                {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();

        handle.send(RuntimeCommand::SelectSession(1)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().session_id == Some(1) {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        config.prompt = "third task".into();
        handle
            .send(RuntimeCommand::StartNewConfigured {
                config,
                request_id: 3,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().running_session_count == 2 {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        handle.send(RuntimeCommand::SelectSession(2)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().session_id == Some(2) {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        handle.send(RuntimeCommand::Resume).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if snapshots.borrow().pending_task_count == 1 {
                    break;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(snapshots.borrow().phase, QueuePhase::Paused);
        assert_eq!(starts.load(Ordering::SeqCst), 3);

        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn high_demand_schedules_one_future_request_without_an_immediate_duplicate() {
        let (transport, starts, _) = ScriptedTransport::new(Script::HighDemand);
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
    async fn retry_now_skips_the_remaining_backoff() {
        let (transport, starts, _) = ScriptedTransport::new(Script::HighDemand);
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
        // The default backoff is at least 15 seconds away.
        assert!(snapshot.next_attempt_at.unwrap() > Utc::now() + chrono::Duration::seconds(10));

        commands.send(RuntimeCommand::RetryNow).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while starts.load(Ordering::SeqCst) < 2 {
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .expect("the retry started without waiting for the backoff");
        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn current_high_demand_notice_requeues_without_alert() {
        let (transport, starts, _) = ScriptedTransport::new(Script::CurrentHighDemandThenSuccess);
        let config = QueueConfig {
            prompt: "do real work".into(),
            // 生产节奏是 10 秒；测试里压到 1 秒，仍然验证"不告警、只重排一次"。
            retry_policy: crate::backoff::RetryPolicy {
                high_demand_delay_secs: 1,
                ..crate::backoff::RetryPolicy::default()
            },
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

    #[test]
    fn missing_rollout_errors_are_classified_as_missing_threads() {
        let missing = TransportError::Rpc {
            code: -32000,
            message: "no rollout found for thread id abc".into(),
            data: None,
        };
        assert!(is_missing_thread_error(&missing));

        let unrelated = TransportError::Rpc {
            code: -32000,
            message: "internal error".into(),
            data: None,
        };
        assert!(!is_missing_thread_error(&unrelated));
    }

    #[tokio::test]
    async fn a_deleted_thread_is_replaced_instead_of_retried_forever() {
        let (transport, starts, thread_starts) =
            ScriptedTransport::new(Script::MissingThreadThenSuccess);
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let state = PersistedQueueState {
            active_thread_id: Some("01a17dc5-gone".into()),
            ..PersistedQueueState::default()
        };
        let machine = QueueMachine::restore(state, config.codex_path.clone());
        let (sender, mut snapshots) = watch::channel(machine.snapshot().clone());
        let (commands, receiver) = mpsc::channel(4);
        let runtime = QueueRuntime::new(transport, config, machine, None, sender);
        let task = tokio::spawn(runtime.run(receiver));

        commands.send(RuntimeCommand::Start).await.unwrap();
        let snapshot = wait_for_phase(&mut snapshots, QueuePhase::Success).await;

        assert_eq!(snapshot.active_thread_id.as_deref(), Some("thread"));
        assert_eq!(thread_starts.load(Ordering::SeqCst), 1);
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(snapshot.retry_alert_count, 0);

        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn internal_retry_and_success_never_start_a_second_turn() {
        let (transport, starts, _) = ScriptedTransport::new(Script::InternalRetryThenSuccess);
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
        let (transport, starts, _) = ScriptedTransport::new(Script::StreamDisconnect);
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
        let (transport, starts, _) = ScriptedTransport::new(Script::Success);
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
    async fn success_can_continue_the_same_thread_with_a_follow_up() {
        let (transport, starts, thread_starts) = ScriptedTransport::new(Script::Success);
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

        assert_eq!(thread_starts.load(Ordering::SeqCst), 1);
        assert_eq!(
            snapshots.borrow().active_thread_id.as_deref(),
            Some("thread")
        );

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
                    completed_at: None,
                    client_message_ids: Vec::new(),
                    client_ids_reported: false,
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
                    completed_at: None,
                    client_message_ids: Vec::new(),
                    client_ids_reported: false,
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
    fn a_follow_up_message_keeps_the_existing_thread() {
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

        assert!(runtime.handle_configure(edited));
        assert_eq!(runtime.config.prompt, "new task");
        assert_eq!(
            runtime.machine().snapshot().active_thread_id.as_deref(),
            Some("thread")
        );
    }

    #[tokio::test]
    async fn graceful_shutdown_clears_the_session_for_the_next_launch() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::in_directory(directory.path().to_path_buf());
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let mut machine = QueueMachine::new(config.codex_path.clone());
        machine.set_thread("thread".into());
        let (sender, _) = watch::channel(machine.snapshot().clone());
        let (commands, receiver) = mpsc::channel(4);
        let runtime =
            QueueRuntime::new(NoopTransport, config, machine, Some(store.clone()), sender);
        let task = tokio::spawn(runtime.run(receiver));

        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        task.await.unwrap().unwrap();

        let state = store.load_state().unwrap();
        assert_eq!(state.phase, QueuePhase::Idle);
        assert!(state.active_thread_id.is_none());
        assert!(state.active_turn_id.is_none());
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
                    completed_at: None,
                    client_message_ids: Vec::new(),
                    client_ids_reported: false,
                }),
            })
        }

        async fn start_turn(
            &mut self,
            _: &str,
            _: &str,
            _: Option<&std::path::Path>,
            _: &str,
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

    /// Resumes with a scripted latest turn, completes every started turn with
    /// "fresh reply", and records what was sent.
    #[derive(Debug)]
    struct ReconcileTransport {
        latest_turn: Option<RestoredTurn>,
        sent: Arc<std::sync::Mutex<Vec<(String, String)>>>,
        identity: Arc<std::sync::Mutex<Option<ClientIdentity>>>,
        events: (mpsc::Sender<TransportEvent>, mpsc::Receiver<TransportEvent>),
    }

    impl ReconcileTransport {
        fn new(latest_turn: Option<RestoredTurn>) -> Self {
            Self {
                latest_turn,
                sent: Arc::default(),
                identity: Arc::default(),
                events: mpsc::channel(8),
            }
        }
    }

    #[async_trait::async_trait]
    impl CodexTransport for ReconcileTransport {
        fn set_client_identity(&mut self, identity: ClientIdentity) {
            *self.identity.lock().unwrap() = Some(identity);
        }

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
                latest_turn: self.latest_turn.clone(),
            })
        }

        async fn start_turn(
            &mut self,
            thread_id: &str,
            prompt: &str,
            _: Option<&std::path::Path>,
            client_message_id: &str,
        ) -> Result<StartedTurn, TransportError> {
            let mut sent = self.sent.lock().unwrap();
            sent.push((prompt.to_owned(), client_message_id.to_owned()));
            let id = format!("fresh-{}", sent.len());
            let _ = self.events.0.try_send(TransportEvent::TurnCompleted {
                thread_id: thread_id.into(),
                turn: RestoredTurn {
                    id: id.clone(),
                    status: TurnStatus::Completed,
                    final_message: "fresh reply".into(),
                    error: None,
                    completed_at: None,
                    client_message_ids: vec![client_message_id.to_owned()],
                    client_ids_reported: true,
                },
            });
            Ok(StartedTurn { id })
        }

        async fn interrupt_turn(&mut self, _: &str, _: &str) -> Result<(), TransportError> {
            Ok(())
        }

        async fn next_event(&mut self) -> Option<TransportEvent> {
            self.events.1.recv().await
        }

        async fn shutdown(&mut self) {}
    }

    fn restored(status: TurnStatus, reply: &str, client_ids: &[&str]) -> RestoredTurn {
        RestoredTurn {
            id: "restored-turn".into(),
            status,
            final_message: reply.into(),
            error: None,
            completed_at: None,
            client_message_ids: client_ids.iter().map(|id| (*id).to_owned()).collect(),
            client_ids_reported: true,
        }
    }

    fn uncertain_state(config: &QueueConfig, pending_id: &str) -> PersistedQueueState {
        PersistedQueueState {
            phase: QueuePhase::Sending,
            active_thread_id: Some("thread".into()),
            submission_uncertain: true,
            pending_client_message_id: Some(pending_id.into()),
            attempt_count: 1,
            prompt_digest: Some(prompt_digest(&config.prompt)),
            ..PersistedQueueState::default()
        }
    }

    #[tokio::test]
    async fn a_submission_that_never_arrived_is_resent_instead_of_adopting_an_older_turn() {
        let config = QueueConfig {
            prompt: "second message".into(),
            ..QueueConfig::default()
        };
        let transport = ReconcileTransport::new(Some(restored(
            TurnStatus::Completed,
            "reply to an earlier message",
            &["earlier-client-id"],
        )));
        let sent = Arc::clone(&transport.sent);
        let handle = spawn_runtime(
            transport,
            config.clone(),
            uncertain_state(&config, "lost-client-id"),
            None,
        );
        let mut snapshots = handle.snapshot();

        let snapshot = wait_for_phase(&mut snapshots, QueuePhase::Success).await;

        assert_eq!(snapshot.reply_preview, "fresh reply");
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "second message");
        assert_ne!(sent[0].1, "lost-client-id");
        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn the_restored_turn_carrying_our_client_id_is_adopted_without_resending() {
        let config = QueueConfig {
            prompt: "do real work".into(),
            ..QueueConfig::default()
        };
        let transport = ReconcileTransport::new(Some(restored(
            TurnStatus::Completed,
            "finished while Longwatch was closed",
            &["our-client-id"],
        )));
        let sent = Arc::clone(&transport.sent);
        let handle = spawn_runtime(
            transport,
            config.clone(),
            uncertain_state(&config, "our-client-id"),
            None,
        );
        let mut snapshots = handle.snapshot();

        let snapshot = wait_for_phase(&mut snapshots, QueuePhase::Success).await;

        assert_eq!(
            snapshot.reply_preview,
            "finished while Longwatch was closed"
        );
        assert!(sent.lock().unwrap().is_empty());
        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn resuming_a_paused_turn_continues_without_waiting_for_a_backoff() {
        let config = QueueConfig {
            prompt: "long task".into(),
            ..QueueConfig::default()
        };
        let transport = ReconcileTransport::new(Some(restored(
            TurnStatus::Interrupted,
            "",
            &["paused-client-id"],
        )));
        let sent = Arc::clone(&transport.sent);
        let state = PersistedQueueState {
            phase: QueuePhase::Paused,
            active_thread_id: Some("thread".into()),
            active_turn_id: Some("restored-turn".into()),
            pending_client_message_id: Some("paused-client-id".into()),
            attempt_count: 1,
            prompt_digest: Some(prompt_digest(&config.prompt)),
            ..PersistedQueueState::default()
        };
        let handle = spawn_runtime(transport, config, state, None);
        let mut snapshots = handle.snapshot();

        handle.send(RuntimeCommand::Resume).await.unwrap();
        let snapshot = wait_for_phase(&mut snapshots, QueuePhase::Success).await;

        // The default backoff is 30 s; reaching success inside the 2 s wait
        // proves the continuation went out immediately.
        assert_eq!(snapshot.reply_preview, "fresh reply");
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, continuation_prompt());
        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn the_configured_client_identity_is_applied_before_connecting() {
        let config = QueueConfig {
            prompt: "task".into(),
            client_identity: ClientIdentity::CodexCli,
            ..QueueConfig::default()
        };
        let transport = ReconcileTransport::new(None);
        let identity = Arc::clone(&transport.identity);
        let handle = spawn_runtime(transport, config, PersistedQueueState::default(), None);
        let mut snapshots = handle.snapshot();

        handle.send(RuntimeCommand::Start).await.unwrap();
        wait_for_phase(&mut snapshots, QueuePhase::Success).await;

        assert_eq!(*identity.lock().unwrap(), Some(ClientIdentity::CodexCli));
        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    async fn wait_for_snapshot(
        snapshots: &mut watch::Receiver<QueueSnapshot>,
        condition: impl Fn(&QueueSnapshot) -> bool,
    ) -> QueueSnapshot {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = snapshots.borrow().clone();
                if condition(&snapshot) {
                    return snapshot;
                }
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .expect("scheduler reached the expected state")
    }

    #[tokio::test]
    async fn waiting_tasks_are_listed_with_their_placement_and_can_be_cancelled() {
        let starts = Arc::new(AtomicUsize::new(0));
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (initial, _) = ConcurrentTransport::new(1, Arc::clone(&starts), Arc::clone(&prompts));
        let mut config = QueueConfig {
            prompt: "running task".into(),
            ..QueueConfig::default()
        };
        let handle = spawn_multi_runtime(
            initial,
            config.clone(),
            PersistedQueueState::default(),
            None,
            {
                let starts = Arc::clone(&starts);
                let prompts = Arc::clone(&prompts);
                move || ConcurrentTransport::new(2, Arc::clone(&starts), Arc::clone(&prompts)).0
            },
        );
        let mut snapshots = handle.snapshot();
        handle
            .send(RuntimeCommand::StartNewConfigured {
                config: config.clone(),
                request_id: 1,
            })
            .await
            .unwrap();
        let snapshot = wait_for_snapshot(&mut snapshots, |snapshot| {
            snapshot.running_session_count == 1
                && snapshot
                    .last_submission
                    .is_some_and(|placement| placement.request_id == 1)
        })
        .await;
        assert_eq!(
            snapshot
                .last_submission
                .and_then(|placement| placement.session_id),
            Some(1)
        );

        config.prompt = "waiting task".into();
        handle
            .send(RuntimeCommand::StartNewConfigured {
                config,
                request_id: 2,
            })
            .await
            .unwrap();
        let snapshot =
            wait_for_snapshot(&mut snapshots, |snapshot| snapshot.pending_tasks.len() == 1).await;
        let waiting = snapshot.pending_tasks[0].clone();
        assert_eq!(waiting.prompt, "waiting task");
        let placement = snapshot.last_submission.unwrap();
        assert_eq!(placement.request_id, 2);
        assert_eq!(placement.pending_id, Some(waiting.id));

        handle
            .send(RuntimeCommand::CancelPending(waiting.id))
            .await
            .unwrap();
        wait_for_snapshot(&mut snapshots, |snapshot| snapshot.pending_tasks.is_empty()).await;
        assert!(
            !prompts
                .lock()
                .unwrap()
                .iter()
                .any(|(_, prompt)| prompt == "waiting task")
        );
        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }

    #[tokio::test]
    async fn finished_sessions_can_be_dismissed_and_cleared() {
        let starts = Arc::new(AtomicUsize::new(0));
        let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let senders = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (initial, sender) =
            ConcurrentTransport::new(1, Arc::clone(&starts), Arc::clone(&prompts));
        senders.lock().unwrap().push(sender);
        let counter = Arc::new(AtomicUsize::new(1));
        let mut config = QueueConfig {
            prompt: "first".into(),
            max_concurrent_sessions: 3,
            ..QueueConfig::default()
        };
        let handle = spawn_multi_runtime(
            initial,
            config.clone(),
            PersistedQueueState::default(),
            None,
            {
                let starts = Arc::clone(&starts);
                let prompts = Arc::clone(&prompts);
                let senders = Arc::clone(&senders);
                let counter = Arc::clone(&counter);
                move || {
                    let id = counter.fetch_add(1, Ordering::SeqCst) as u64 + 1;
                    let (transport, sender) =
                        ConcurrentTransport::new(id, Arc::clone(&starts), Arc::clone(&prompts));
                    senders.lock().unwrap().push(sender);
                    transport
                }
            },
        );
        let mut snapshots = handle.snapshot();
        for (request_id, prompt) in [(1, "first"), (2, "second"), (3, "third")] {
            config.prompt = prompt.into();
            handle
                .send(RuntimeCommand::StartNewConfigured {
                    config: config.clone(),
                    request_id,
                })
                .await
                .unwrap();
        }
        wait_for_snapshot(&mut snapshots, |snapshot| {
            snapshot.running_session_count == 3 && starts.load(Ordering::SeqCst) == 3
        })
        .await;

        // Finish the first two tasks; the third keeps running.
        for instance in [1_usize, 2] {
            let sender = senders.lock().unwrap()[instance - 1].clone();
            sender
                .send(TransportEvent::TurnCompleted {
                    thread_id: format!("thread-{instance}"),
                    turn: RestoredTurn {
                        id: format!("turn-{instance}-{instance}"),
                        status: TurnStatus::Completed,
                        final_message: "done".into(),
                        error: None,
                        completed_at: None,
                        client_message_ids: Vec::new(),
                        client_ids_reported: false,
                    },
                })
                .await
                .unwrap();
        }
        wait_for_snapshot(&mut snapshots, |snapshot| {
            snapshot
                .sessions
                .iter()
                .filter(|session| session.phase == QueuePhase::Success)
                .count()
                == 2
        })
        .await;

        // A running session cannot be dismissed.
        handle
            .send(RuntimeCommand::DismissSession(3))
            .await
            .unwrap();
        handle
            .send(RuntimeCommand::DismissSession(2))
            .await
            .unwrap();
        let snapshot = wait_for_snapshot(&mut snapshots, |snapshot| {
            !snapshot.sessions.iter().any(|session| session.id == 2)
        })
        .await;
        assert!(snapshot.sessions.iter().any(|session| session.id == 3));

        handle.send(RuntimeCommand::ClearFinished).await.unwrap();
        // The root session is reset in place and becomes a pristine lane.
        let snapshot = wait_for_snapshot(&mut snapshots, |snapshot| {
            snapshot
                .sessions
                .iter()
                .filter(|session| session.has_history)
                .count()
                == 1
        })
        .await;
        let remaining = snapshot
            .sessions
            .iter()
            .find(|session| session.has_history)
            .unwrap();
        assert_eq!(remaining.id, 3);
        assert!(remaining.phase.is_running());
        handle.send(RuntimeCommand::Shutdown).await.unwrap();
        handle.join().await.unwrap();
    }
}
