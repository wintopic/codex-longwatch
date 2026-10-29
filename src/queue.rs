use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::backoff::{AttemptLedger, RetryPolicy};
use crate::config::PersistedQueueState;
use crate::transport::TransportStatus;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ConversationRole {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConversationMessage {
    pub role: ConversationRole,
    pub content: String,
    #[serde(default)]
    pub automatic: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingMessage {
    pub prompt: String,
    #[serde(default)]
    pub working_directory: Option<PathBuf>,
    #[serde(default)]
    pub failure_phrases: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum QueuePhase {
    #[default]
    Idle,
    Connecting,
    Sending,
    Waiting,
    Backoff,
    Success,
    Paused,
    FatalError,
}

impl QueuePhase {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "空闲",
            Self::Connecting => "连接中",
            Self::Sending => "发送中",
            Self::Waiting => "等待 Codex",
            Self::Backoff => "退避等待",
            Self::Success => "成功",
            Self::Paused => "已暂停",
            Self::FatalError => "致命错误",
        }
    }

    #[must_use]
    pub const fn is_running(self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Sending | Self::Waiting | Self::Backoff
        )
    }
}

/// Compact, non-persistent information about one independent Codex session.
/// The selected session still supplies the full [`QueueSnapshot`]; these
/// summaries are only used for switching and aggregate queue status in the UI.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct QueueSessionSummary {
    pub id: u64,
    pub phase: QueuePhase,
    pub title: String,
    pub status_message: String,
    pub active_thread_id: Option<String>,
    pub attempt_count: u64,
    pub queued_message_count: usize,
    pub consecutive_retries: u32,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub first_attempt_at: Option<DateTime<Utc>>,
    pub reply_completed_at: Option<DateTime<Utc>>,
    /// Pending scheduler task this session was started from, so a view that
    /// was showing the waiting task can follow it once it gets a lane.
    pub origin_pending_id: Option<u64>,
    /// False for a slot that has never been given a task.
    pub has_history: bool,
    /// Holds a lane: running, or assigned and about to start.
    pub active: bool,
}

impl Default for QueueSessionSummary {
    fn default() -> Self {
        Self {
            id: 0,
            phase: QueuePhase::Idle,
            title: "新会话".into(),
            status_message: "等待任务".into(),
            active_thread_id: None,
            attempt_count: 0,
            queued_message_count: 0,
            consecutive_retries: 0,
            next_attempt_at: None,
            first_attempt_at: None,
            reply_completed_at: None,
            origin_pending_id: None,
            has_history: false,
            active: false,
        }
    }
}

/// A task waiting in the scheduler for a free lane.
#[derive(Clone, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct PendingTaskSummary {
    pub id: u64,
    pub title: String,
    pub prompt: String,
    pub working_directory: Option<PathBuf>,
    /// Set when the task continues an existing session's thread.
    pub target_session_id: Option<u64>,
}

/// Where the runtime placed a submission, keyed by the editor's request id.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionPlacement {
    pub request_id: u64,
    pub session_id: Option<u64>,
    pub pending_id: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueueSnapshot {
    pub phase: QueuePhase,
    pub status_message: String,
    pub active_thread_id: Option<String>,
    pub active_turn_id: Option<String>,
    #[serde(default)]
    pub submission_uncertain: bool,
    /// `clientUserMessageId` of the latest `turn/start`; see
    /// [`PersistedQueueState::pending_client_message_id`].
    #[serde(default)]
    pub pending_client_message_id: Option<String>,
    /// The next message entered while a turn is running.  Longwatch keeps a
    /// single pending message so typing ahead never loses the user's draft.
    #[serde(default)]
    pub queued_message: Option<PendingMessage>,
    /// Messages exchanged during this Longwatch session.  This is local UI
    /// history; the Codex thread remains the source of truth for context.
    #[serde(default)]
    pub conversation: Vec<ConversationMessage>,
    /// When true, the next external attempt should ask Codex to continue the
    /// existing work instead of submitting the original prompt again.
    #[serde(default)]
    pub retry_as_follow_up: bool,
    /// Last user-submission command accepted by the runtime.  This is an
    /// in-memory acknowledgement channel for the editor; it is intentionally
    /// not part of the persisted queue state.
    #[serde(default)]
    pub accepted_request_id: u64,
    /// Last user-submission command fully handled by the runtime, whether it
    /// was accepted or rejected. This lets the editor keep rejected text while
    /// still unlocking itself for the next action, and lets the multi-session
    /// scheduler close the dispatch gap without guessing from a stale phase.
    #[serde(default)]
    pub processed_request_id: u64,
    /// Monotonic in-memory command acknowledgement. Unlike the request id,
    /// this also changes for restored scheduler tasks whose request id is 0.
    #[serde(default)]
    pub processed_request_count: u64,
    pub attempt_count: u64,
    pub consecutive_retries: u32,
    /// When the current task was first submitted (the attempt ledger's first
    /// entry); drives the "queued for …" display.
    #[serde(default)]
    pub first_attempt_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub retry_alert_count: u64,
    pub empty_reply_count: u8,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub reply_preview: String,
    /// True when `reply_preview` holds an error or status notice rather than
    /// text written by Codex, so the UI can show it as status instead of as
    /// the assistant's answer.
    #[serde(default)]
    pub reply_preview_is_notice: bool,
    /// Completion time read from the original Codex turn. Longwatch only
    /// caches this metadata for display and never writes it into the thread.
    #[serde(default)]
    pub reply_completed_at: Option<DateTime<Utc>>,
    pub codex_path: PathBuf,
    /// Ephemeral runtime warning shown in the UI. It is intentionally not
    /// persisted so a transient disk failure cannot become stale state after
    /// the next launch.
    #[serde(default)]
    pub runtime_warning: Option<String>,
    /// Live transport information. It is rebuilt on launch and never written
    /// into the persisted queue state.
    #[serde(default)]
    pub transport_status: TransportStatus,
    /// Prompt associated with the selected session. This is runtime display
    /// metadata and is never written into the original Codex conversation.
    #[serde(default)]
    pub session_prompt: String,
    #[serde(default)]
    pub session_working_directory: Option<PathBuf>,
    /// Model and reasoning effort the selected session sends to Codex.
    #[serde(default)]
    pub session_model: Option<String>,
    #[serde(default)]
    pub session_reasoning_effort: Option<String>,
    #[serde(default)]
    pub session_failure_phrases: Vec<String>,
    /// Selected independent Longwatch session, if the pool has created one.
    #[serde(default)]
    pub session_id: Option<u64>,
    /// Compact state for all independent sessions managed by Longwatch.
    #[serde(default)]
    pub sessions: Vec<QueueSessionSummary>,
    /// Tasks waiting in the scheduler for a free lane, in dispatch order.
    #[serde(default)]
    pub pending_tasks: Vec<PendingTaskSummary>,
    /// Placement of the most recent editor submission.
    #[serde(default)]
    pub last_submission: Option<SubmissionPlacement>,
    #[serde(default)]
    pub running_session_count: usize,
    /// Includes messages staged inside individual sessions and messages still
    /// waiting in the global scheduler.
    #[serde(default)]
    pub pending_task_count: usize,
    #[serde(default = "default_concurrent_sessions")]
    pub max_concurrent_sessions: u8,
}

const fn default_concurrent_sessions() -> u8 {
    1
}

impl Default for QueueSnapshot {
    fn default() -> Self {
        Self {
            phase: QueuePhase::Idle,
            status_message: "等待任务".into(),
            active_thread_id: None,
            active_turn_id: None,
            submission_uncertain: false,
            pending_client_message_id: None,
            queued_message: None,
            conversation: Vec::new(),
            retry_as_follow_up: false,
            accepted_request_id: 0,
            processed_request_id: 0,
            processed_request_count: 0,
            attempt_count: 0,
            consecutive_retries: 0,
            first_attempt_at: None,
            retry_alert_count: 0,
            empty_reply_count: 0,
            next_attempt_at: None,
            reply_preview: String::new(),
            reply_preview_is_notice: false,
            reply_completed_at: None,
            codex_path: PathBuf::from("codex"),
            runtime_warning: None,
            transport_status: TransportStatus::default(),
            session_prompt: String::new(),
            session_working_directory: None,
            session_model: None,
            session_reasoning_effort: None,
            session_failure_phrases: Vec::new(),
            session_id: None,
            sessions: Vec::new(),
            pending_tasks: Vec::new(),
            last_submission: None,
            running_session_count: 0,
            pending_task_count: 0,
            max_concurrent_sessions: default_concurrent_sessions(),
        }
    }
}

#[derive(Debug)]
pub struct QueueMachine {
    snapshot: QueueSnapshot,
    attempts: AttemptLedger,
}

const MAX_CONVERSATION_MESSAGES: usize = 200;

impl QueueMachine {
    #[must_use]
    pub fn new(codex_path: PathBuf) -> Self {
        Self {
            snapshot: QueueSnapshot {
                codex_path,
                ..QueueSnapshot::default()
            },
            attempts: AttemptLedger::default(),
        }
    }

    #[must_use]
    pub fn restore(state: PersistedQueueState, codex_path: PathBuf) -> Self {
        let first_attempt_at = state.attempts.first();
        // Restored previews of unfinished work are the last error or status.
        let reply_preview_is_notice = !state.reply_preview.is_empty()
            && matches!(
                state.phase,
                QueuePhase::Connecting
                    | QueuePhase::Backoff
                    | QueuePhase::Paused
                    | QueuePhase::FatalError
            );
        Self {
            snapshot: QueueSnapshot {
                phase: state.phase,
                status_message: state.status_message,
                active_thread_id: state.active_thread_id,
                active_turn_id: state.active_turn_id,
                submission_uncertain: state.submission_uncertain,
                pending_client_message_id: state.pending_client_message_id,
                queued_message: state.queued_message,
                conversation: state.conversation,
                retry_as_follow_up: state.retry_as_follow_up,
                accepted_request_id: 0,
                processed_request_id: 0,
                processed_request_count: 0,
                attempt_count: state.attempt_count,
                consecutive_retries: state.consecutive_retries,
                first_attempt_at,
                retry_alert_count: state.retry_alert_count,
                empty_reply_count: state.empty_reply_count,
                next_attempt_at: state.next_attempt_at,
                reply_preview: state.reply_preview,
                reply_preview_is_notice,
                reply_completed_at: state.reply_completed_at,
                codex_path,
                runtime_warning: None,
                transport_status: TransportStatus::default(),
                session_prompt: String::new(),
                session_working_directory: None,
                session_model: None,
                session_reasoning_effort: None,
                session_failure_phrases: Vec::new(),
                session_id: None,
                sessions: Vec::new(),
                pending_tasks: Vec::new(),
                last_submission: None,
                running_session_count: 0,
                pending_task_count: 0,
                max_concurrent_sessions: default_concurrent_sessions(),
            },
            attempts: state.attempts,
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> &QueueSnapshot {
        &self.snapshot
    }

    #[must_use]
    pub fn persisted(&self, prompt_digest: Option<String>) -> PersistedQueueState {
        PersistedQueueState {
            version: crate::config::STATE_VERSION,
            phase: self.snapshot.phase,
            status_message: self.snapshot.status_message.clone(),
            active_thread_id: self.snapshot.active_thread_id.clone(),
            active_turn_id: self.snapshot.active_turn_id.clone(),
            submission_uncertain: self.snapshot.submission_uncertain,
            pending_client_message_id: self.snapshot.pending_client_message_id.clone(),
            queued_message: self.snapshot.queued_message.clone(),
            // Conversation rendering is intentionally temporary. The Codex
            // thread remains the source of truth, while omitting this local UI
            // history keeps old sessions from resurfacing as noise on restart.
            conversation: Vec::new(),
            retry_as_follow_up: self.snapshot.retry_as_follow_up,
            attempt_count: self.snapshot.attempt_count,
            consecutive_retries: self.snapshot.consecutive_retries,
            retry_alert_count: self.snapshot.retry_alert_count,
            empty_reply_count: self.snapshot.empty_reply_count,
            next_attempt_at: self.snapshot.next_attempt_at,
            reply_preview: self.snapshot.reply_preview.clone(),
            reply_completed_at: self.snapshot.reply_completed_at,
            attempts: self.attempts.clone(),
            prompt_digest,
        }
    }

    pub fn begin_connecting(&mut self) {
        self.snapshot.phase = QueuePhase::Connecting;
        self.snapshot.status_message = "正在启动 Codex app-server".into();
        self.snapshot.next_attempt_at = None;
    }

    pub fn reactivate(&mut self) {
        self.snapshot.phase = QueuePhase::Connecting;
        self.snapshot.status_message = "正在恢复排队任务".into();
        self.snapshot.next_attempt_at = None;
    }

    pub fn prepare_follow_up(&mut self) {
        let active_thread_id = self.snapshot.active_thread_id.clone();
        let codex_path = self.snapshot.codex_path.clone();
        let runtime_warning = self.snapshot.runtime_warning.clone();
        let transport_status = self.snapshot.transport_status.clone();
        let conversation = self.snapshot.conversation.clone();
        let queued_message = self.snapshot.queued_message.clone();
        let processed_request_count = self.snapshot.processed_request_count;
        self.snapshot = QueueSnapshot {
            status_message: "准备发送下一条消息".into(),
            active_thread_id,
            codex_path,
            runtime_warning,
            transport_status,
            conversation,
            queued_message,
            processed_request_count,
            retry_as_follow_up: false,
            ..QueueSnapshot::default()
        };
        self.attempts = AttemptLedger::default();
    }

    pub fn clear_queued_message(&mut self) {
        self.snapshot.queued_message = None;
    }

    pub fn acknowledge_request(&mut self, request_id: u64) {
        self.snapshot.accepted_request_id = request_id;
    }

    pub fn mark_request_processed(&mut self, request_id: u64) {
        self.snapshot.processed_request_id = request_id;
        self.snapshot.processed_request_count =
            self.snapshot.processed_request_count.saturating_add(1);
    }

    pub fn set_queued_message(&mut self, message: PendingMessage) {
        self.snapshot.queued_message = Some(message);
    }

    pub fn take_queued_message(&mut self) -> Option<PendingMessage> {
        self.snapshot.queued_message.take()
    }

    pub fn append_conversation_message(
        &mut self,
        role: ConversationRole,
        content: &str,
        automatic: bool,
    ) {
        let content = content.trim();
        if content.is_empty() {
            return;
        }
        // A reconnect can replay the terminal event.  Avoid duplicating the
        // exact same terminal message while still allowing repeated prompts.
        if self
            .snapshot
            .conversation
            .last()
            .is_some_and(|last| last.role == role && last.content == content)
        {
            return;
        }
        self.snapshot.conversation.push(ConversationMessage {
            role,
            content: content.to_owned(),
            automatic,
        });
        if self.snapshot.conversation.len() > MAX_CONVERSATION_MESSAGES {
            let excess = self.snapshot.conversation.len() - MAX_CONVERSATION_MESSAGES;
            self.snapshot.conversation.drain(..excess);
        }
    }

    pub fn mark_retry_as_follow_up(&mut self) {
        self.snapshot.retry_as_follow_up = true;
    }

    pub fn clear_retry_mode(&mut self) {
        self.snapshot.retry_as_follow_up = false;
    }

    pub fn set_thread(&mut self, thread_id: String) {
        self.snapshot.active_thread_id = Some(thread_id);
        self.snapshot.status_message = "线程已恢复，准备发送".into();
    }

    /// 持久化的线程在 Codex 侧已经不存在（rollout 被删、会话过期）时使用：
    /// 丢掉旧 id，下一次发送会新建线程，而不是反复 resume 同一个坏 id。
    pub fn clear_thread(&mut self) {
        self.snapshot.active_thread_id = None;
        self.snapshot.active_turn_id = None;
        self.snapshot.submission_uncertain = false;
        self.snapshot.retry_as_follow_up = false;
        self.snapshot.status_message = "原会话已不存在，正在新建会话".into();
    }

    pub fn begin_sending(&mut self, now: DateTime<Utc>) -> Result<(), QueueError> {
        if self.snapshot.active_turn_id.is_some() {
            return Err(QueueError::TurnAlreadyActive);
        }
        self.attempts.record(now);
        self.snapshot.attempt_count = self.snapshot.attempt_count.saturating_add(1);
        self.snapshot.first_attempt_at.get_or_insert(now);
        self.snapshot.phase = QueuePhase::Sending;
        self.snapshot.status_message = "正在发送原始任务".into();
        self.snapshot.next_attempt_at = None;
        self.snapshot.reply_preview.clear();
        self.snapshot.reply_preview_is_notice = false;
        self.snapshot.reply_completed_at = None;
        self.snapshot.submission_uncertain = false;
        Ok(())
    }

    /// Remember the `clientUserMessageId` of the request about to be sent.
    pub fn set_pending_client_message_id(&mut self, id: String) {
        self.snapshot.pending_client_message_id = Some(id);
    }

    /// Mark the request as having crossed the transport boundary while its
    /// outcome is still unknown.  This flag is persisted before the request
    /// is sent and cleared only after the server has acknowledged or
    /// reconciled the turn.
    pub fn mark_submission_uncertain(&mut self) {
        self.snapshot.submission_uncertain = true;
    }

    pub fn clear_submission_uncertain(&mut self) {
        self.snapshot.submission_uncertain = false;
    }

    pub fn set_active_turn(&mut self, turn_id: String) -> Result<(), QueueError> {
        if let Some(active) = self.snapshot.active_turn_id.as_deref() {
            if active != turn_id {
                return Err(QueueError::TurnAlreadyActive);
            }
        }
        self.snapshot.active_turn_id = Some(turn_id);
        self.snapshot.submission_uncertain = false;
        self.snapshot.phase = QueuePhase::Waiting;
        self.snapshot.status_message = "Codex 正在处理；不会并发发送".into();
        Ok(())
    }

    pub fn append_reply_delta(&mut self, delta: &str) {
        const MAX_PREVIEW_CHARS: usize = 4_000;
        // The first streamed text after an error replaces the notice instead
        // of being glued onto it.
        if self.snapshot.reply_preview_is_notice {
            self.snapshot.reply_preview.clear();
            self.snapshot.reply_preview_is_notice = false;
        }
        self.snapshot.reply_preview.push_str(delta);
        let character_count = self.snapshot.reply_preview.chars().count();
        if character_count > MAX_PREVIEW_CHARS {
            self.snapshot.reply_preview = self
                .snapshot
                .reply_preview
                .chars()
                .skip(character_count - MAX_PREVIEW_CHARS)
                .collect();
        }
    }

    /// Show Codex's own text (a streamed or final answer).
    pub fn set_reply_preview(&mut self, reply: &str) {
        self.store_reply_preview(reply);
        self.snapshot.reply_preview_is_notice = false;
    }

    /// Show an error or status notice in place of an answer.
    pub fn set_reply_notice(&mut self, notice: &str) {
        self.store_reply_preview(notice);
        self.snapshot.reply_preview_is_notice = !notice.trim().is_empty();
    }

    fn store_reply_preview(&mut self, reply: &str) {
        const MAX_PREVIEW_CHARS: usize = 4_000;
        let character_count = reply.chars().count();
        self.snapshot.reply_preview = if character_count > MAX_PREVIEW_CHARS {
            reply
                .chars()
                .skip(character_count - MAX_PREVIEW_CHARS)
                .collect()
        } else {
            reply.to_owned()
        };
    }

    pub fn set_reply_completed_at(&mut self, completed_at: Option<DateTime<Utc>>) {
        self.snapshot.reply_completed_at = completed_at;
    }

    pub fn set_status(&mut self, message: &str) {
        self.snapshot.status_message = message.into();
    }

    pub fn set_runtime_warning(&mut self, message: impl Into<String>) {
        self.snapshot.runtime_warning = Some(message.into());
    }

    pub fn set_transport_status(&mut self, status: TransportStatus) {
        self.snapshot.transport_status = status;
    }

    pub fn clear_runtime_warning(&mut self) {
        self.snapshot.runtime_warning = None;
    }

    pub fn set_next_attempt_at(&mut self, next: DateTime<Utc>) {
        self.snapshot.next_attempt_at = Some(next);
    }

    pub fn note_internal_retry(&mut self, message: &str) {
        self.snapshot.phase = QueuePhase::Waiting;
        self.snapshot.status_message = format!("Codex 正在内部重试：{message}");
        self.record_retry_alert();
    }

    /// Records a server-side retry without raising a user-facing retry alert.
    /// The active turn remains owned by Codex, so no new request is scheduled.
    pub fn note_internal_retry_quiet(&mut self, message: &str) {
        self.snapshot.phase = QueuePhase::Waiting;
        self.snapshot.status_message = format!("Codex 正在静默重试：{message}");
    }

    pub fn record_retry_alert(&mut self) {
        self.snapshot.retry_alert_count = self.snapshot.retry_alert_count.saturating_add(1);
    }

    pub fn clear_active_turn(&mut self) {
        self.snapshot.active_turn_id = None;
    }

    pub fn mark_empty_reply(&mut self) {
        self.snapshot.empty_reply_count = self.snapshot.empty_reply_count.saturating_add(1);
    }

    pub fn reset_empty_replies(&mut self) {
        self.snapshot.empty_reply_count = 0;
    }

    /// Queue the next attempt `delay` from `now`. Failed attempts are retried
    /// right away, so `delay` only ever covers the minimum spacing between two
    /// attempts. A quiet retry (the plain "high demand" notice) does not raise
    /// a retry alert.
    pub fn schedule_retry(
        &mut self,
        now: DateTime<Utc>,
        delay: std::time::Duration,
        reason: &str,
        show_alert: bool,
    ) -> DateTime<Utc> {
        self.snapshot.active_turn_id = None;
        self.snapshot.submission_uncertain = false;
        self.snapshot.consecutive_retries = self.snapshot.consecutive_retries.saturating_add(1);
        if show_alert {
            self.record_retry_alert();
        }
        let next = now + chrono::Duration::from_std(delay).unwrap_or_default();
        self.snapshot.phase = QueuePhase::Backoff;
        self.snapshot.status_message = format!("{reason}；立即重试");
        self.snapshot.next_attempt_at = Some(next);
        next
    }

    pub fn delay_after_wake(&mut self, now: DateTime<Utc>, policy: &RetryPolicy) -> bool {
        if matches!(
            self.snapshot.phase,
            QueuePhase::Backoff | QueuePhase::Connecting
        ) && self.snapshot.next_attempt_at.is_some()
        {
            let wake_floor =
                now + chrono::Duration::from_std(policy.wake_delay()).unwrap_or_default();
            self.snapshot.next_attempt_at = Some(
                self.snapshot
                    .next_attempt_at
                    .map_or(wake_floor, |existing| existing.max(wake_floor)),
            );
            self.snapshot.status_message = "检测到系统唤醒；至少等待 60 秒后再试".into();
            true
        } else {
            false
        }
    }

    pub fn succeed(&mut self, message: &str) {
        self.snapshot.phase = QueuePhase::Success;
        self.snapshot.status_message = message.into();
        self.snapshot.active_turn_id = None;
        self.snapshot.submission_uncertain = false;
        self.snapshot.next_attempt_at = None;
        self.snapshot.consecutive_retries = 0;
        self.snapshot.empty_reply_count = 0;
        self.snapshot.retry_as_follow_up = false;
    }

    pub fn pause(&mut self, reason: &str) {
        self.snapshot.phase = QueuePhase::Paused;
        self.snapshot.status_message = reason.into();
        self.snapshot.next_attempt_at = None;
    }

    pub fn fatal(&mut self, reason: &str) {
        self.snapshot.phase = QueuePhase::FatalError;
        self.snapshot.status_message = reason.into();
        self.snapshot.next_attempt_at = None;
    }

    pub fn stop_current_turn(&mut self) {
        self.prepare_follow_up();
        self.snapshot.status_message = if self.snapshot.active_thread_id.is_some() {
            "已停止；下次发送将继续当前会话".into()
        } else {
            "已停止；等待发送消息".into()
        };
    }

    pub fn clear_session(&mut self) {
        let processed_request_count = self.snapshot.processed_request_count;
        self.snapshot = QueueSnapshot {
            codex_path: self.snapshot.codex_path.clone(),
            runtime_warning: self.snapshot.runtime_warning.clone(),
            transport_status: self.snapshot.transport_status.clone(),
            status_message: "等待任务".into(),
            processed_request_count,
            ..QueueSnapshot::default()
        };
        self.attempts = AttemptLedger::default();
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum QueueError {
    #[error("a turn is already active")]
    TurnAlreadyActive,
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn machine_never_allows_two_turns() {
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 1, 0, 0).unwrap();
        let mut machine = QueueMachine::new("codex".into());
        machine.begin_sending(now).unwrap();
        machine.set_active_turn("turn-1".into()).unwrap();
        assert_eq!(
            machine.begin_sending(now),
            Err(QueueError::TurnAlreadyActive)
        );
    }

    #[test]
    fn wake_reschedules_without_catch_up_burst() {
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 1, 0, 0).unwrap();
        let mut machine = QueueMachine::new("codex".into());
        machine.schedule_retry(now, std::time::Duration::ZERO, "busy", true);
        let wake = now + chrono::Duration::hours(5);
        machine.delay_after_wake(wake, &RetryPolicy::default());
        assert!(machine.snapshot.next_attempt_at.unwrap() >= wake + chrono::Duration::seconds(60));
    }

    #[test]
    fn repeated_attempts_are_not_rate_limited() {
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 1, 0, 0).unwrap();
        let mut machine = QueueMachine::new("codex".into());
        for index in 0..100_u64 {
            machine
                .begin_sending(now + chrono::Duration::seconds(index as i64))
                .unwrap();
            machine.set_active_turn(format!("turn-{index}")).unwrap();
            machine.clear_active_turn();
        }

        assert_eq!(machine.snapshot.attempt_count, 100);
    }

    #[test]
    fn a_failed_attempt_is_retried_immediately() {
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 1, 0, 0).unwrap();
        let mut machine = QueueMachine::new("codex".into());

        let next = machine.schedule_retry(now, std::time::Duration::ZERO, "busy", true);

        assert_eq!(next, now);
        assert_eq!(machine.snapshot.phase, QueuePhase::Backoff);
        assert_eq!(machine.snapshot.next_attempt_at, Some(now));
        assert_eq!(machine.snapshot.consecutive_retries, 1);
        assert_eq!(machine.snapshot.retry_alert_count, 1);
        assert!(machine.snapshot.status_message.contains("立即重试"));
    }

    #[test]
    fn retries_never_grow_their_delay() {
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 1, 0, 0).unwrap();
        let mut machine = QueueMachine::new("codex".into());
        let spacing = std::time::Duration::from_secs(2);

        for round in 0..20_i64 {
            let failed_at = now + chrono::Duration::minutes(round);
            let next = machine.schedule_retry(failed_at, spacing, "busy", true);
            assert_eq!(next, failed_at + chrono::Duration::seconds(2));
        }
        assert_eq!(machine.snapshot.consecutive_retries, 20);
    }

    #[test]
    fn quiet_retry_does_not_raise_an_alert() {
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 1, 0, 0).unwrap();
        let mut machine = QueueMachine::new("codex".into());

        let next = machine.schedule_retry(now, std::time::Duration::ZERO, "高需求提示", false);

        assert_eq!(next, now);
        assert_eq!(machine.snapshot.retry_alert_count, 0);
    }

    #[test]
    fn streamed_text_replaces_an_error_notice_instead_of_appending() {
        let mut machine = QueueMachine::new("codex".into());
        machine.set_reply_notice("Reconnecting... 2/5");
        assert!(machine.snapshot.reply_preview_is_notice);

        machine.append_reply_delta("The answer");

        assert_eq!(machine.snapshot.reply_preview, "The answer");
        assert!(!machine.snapshot.reply_preview_is_notice);
        machine.append_reply_delta(" continues");
        assert_eq!(machine.snapshot.reply_preview, "The answer continues");
    }

    #[test]
    fn first_attempt_time_survives_retries_and_restarts() {
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 1, 0, 0).unwrap();
        let mut machine = QueueMachine::new("codex".into());
        machine.begin_sending(now).unwrap();
        machine.set_active_turn("turn-1".into()).unwrap();
        machine.clear_active_turn();
        machine
            .begin_sending(now + chrono::Duration::minutes(3))
            .unwrap();
        assert_eq!(machine.snapshot.first_attempt_at, Some(now));

        let restored = QueueMachine::restore(machine.persisted(None), "codex".into());
        assert_eq!(restored.snapshot().first_attempt_at, Some(now));
    }

    #[test]
    fn stopping_a_turn_preserves_the_session_for_the_next_message() {
        let mut machine = QueueMachine::new("codex".into());
        machine.set_thread("thread-1".into());
        machine.begin_sending(Utc::now()).unwrap();
        machine.set_active_turn("turn-1".into()).unwrap();

        machine.stop_current_turn();

        assert_eq!(machine.snapshot.phase, QueuePhase::Idle);
        assert_eq!(
            machine.snapshot.active_thread_id.as_deref(),
            Some("thread-1")
        );
        assert!(machine.snapshot.active_turn_id.is_none());
        assert_eq!(machine.snapshot.attempt_count, 0);
    }

    #[test]
    fn clearing_the_session_makes_the_next_launch_fresh() {
        let mut machine = QueueMachine::new("codex".into());
        machine.set_thread("thread-1".into());

        machine.clear_session();

        assert_eq!(machine.snapshot.phase, QueuePhase::Idle);
        assert!(machine.snapshot.active_thread_id.is_none());
    }

    #[test]
    fn one_pending_message_is_preserved_without_local_history() {
        let mut machine = QueueMachine::new("codex".into());
        machine.append_conversation_message(ConversationRole::User, "first", false);
        machine.set_queued_message(PendingMessage {
            prompt: "second".into(),
            working_directory: None,
            failure_phrases: vec!["busy".into()],
        });

        let state = machine.persisted(None);
        let restored = QueueMachine::restore(state, "codex".into());

        assert_eq!(
            restored.snapshot().conversation,
            Vec::<ConversationMessage>::new()
        );
        assert_eq!(
            restored
                .snapshot()
                .queued_message
                .as_ref()
                .map(|message| message.prompt.as_str()),
            Some("second")
        );
    }
}
