use std::path::PathBuf;

use chrono::{DateTime, Utc};
use rand::Rng;
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
    pub attempt_count: u64,
    pub consecutive_retries: u32,
    #[serde(default)]
    pub retry_alert_count: u64,
    pub empty_reply_count: u8,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub reply_preview: String,
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
}

impl Default for QueueSnapshot {
    fn default() -> Self {
        Self {
            phase: QueuePhase::Idle,
            status_message: "等待任务".into(),
            active_thread_id: None,
            active_turn_id: None,
            submission_uncertain: false,
            queued_message: None,
            conversation: Vec::new(),
            retry_as_follow_up: false,
            accepted_request_id: 0,
            attempt_count: 0,
            consecutive_retries: 0,
            retry_alert_count: 0,
            empty_reply_count: 0,
            next_attempt_at: None,
            reply_preview: String::new(),
            reply_completed_at: None,
            codex_path: PathBuf::from("codex"),
            runtime_warning: None,
            transport_status: TransportStatus::default(),
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
        Self {
            snapshot: QueueSnapshot {
                phase: state.phase,
                status_message: state.status_message,
                active_thread_id: state.active_thread_id,
                active_turn_id: state.active_turn_id,
                submission_uncertain: state.submission_uncertain,
                queued_message: state.queued_message,
                conversation: state.conversation,
                retry_as_follow_up: state.retry_as_follow_up,
                accepted_request_id: 0,
                attempt_count: state.attempt_count,
                consecutive_retries: state.consecutive_retries,
                retry_alert_count: state.retry_alert_count,
                empty_reply_count: state.empty_reply_count,
                next_attempt_at: state.next_attempt_at,
                reply_preview: state.reply_preview,
                reply_completed_at: state.reply_completed_at,
                codex_path,
                runtime_warning: None,
                transport_status: TransportStatus::default(),
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
            queued_message: self.snapshot.queued_message.clone(),
            conversation: self.snapshot.conversation.clone(),
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
        self.snapshot = QueueSnapshot {
            status_message: "准备发送下一条消息".into(),
            active_thread_id,
            codex_path,
            runtime_warning,
            transport_status,
            conversation,
            queued_message,
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

    pub fn begin_sending(&mut self, now: DateTime<Utc>) -> Result<(), QueueError> {
        if self.snapshot.active_turn_id.is_some() {
            return Err(QueueError::TurnAlreadyActive);
        }
        self.attempts.record(now);
        self.snapshot.attempt_count = self.snapshot.attempt_count.saturating_add(1);
        self.snapshot.phase = QueuePhase::Sending;
        self.snapshot.status_message = "正在发送原始任务".into();
        self.snapshot.next_attempt_at = None;
        self.snapshot.reply_preview.clear();
        self.snapshot.reply_completed_at = None;
        self.snapshot.submission_uncertain = false;
        Ok(())
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

    pub fn set_reply_preview(&mut self, reply: &str) {
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

    pub fn schedule_retry<R: Rng + ?Sized>(
        &mut self,
        now: DateTime<Utc>,
        policy: &RetryPolicy,
        reason: &str,
        rng: &mut R,
    ) -> DateTime<Utc> {
        self.schedule_retry_with_alert(now, policy, reason, rng, true)
    }

    pub fn schedule_retry_with_alert<R: Rng + ?Sized>(
        &mut self,
        now: DateTime<Utc>,
        policy: &RetryPolicy,
        reason: &str,
        rng: &mut R,
        show_alert: bool,
    ) -> DateTime<Utc> {
        self.snapshot.active_turn_id = None;
        self.snapshot.submission_uncertain = false;
        self.snapshot.consecutive_retries = self.snapshot.consecutive_retries.saturating_add(1);
        if show_alert {
            self.record_retry_alert();
        }
        let delay = policy.delay_for(self.snapshot.consecutive_retries, rng);
        let next = now + chrono::Duration::from_std(delay).unwrap_or_default();
        self.snapshot.phase = QueuePhase::Backoff;
        self.snapshot.status_message = format!("{reason}；已安排低频重试");
        self.snapshot.next_attempt_at = Some(next);
        next
    }

    /// Starts a new queue attempt immediately without entering the user-facing
    /// backoff state or incrementing the retry-alert counter.
    pub fn schedule_quiet_immediate_retry(
        &mut self,
        now: DateTime<Utc>,
        reason: &str,
    ) -> DateTime<Utc> {
        self.snapshot.active_turn_id = None;
        self.snapshot.submission_uncertain = false;
        self.snapshot.consecutive_retries = self.snapshot.consecutive_retries.saturating_add(1);
        self.snapshot.phase = QueuePhase::Connecting;
        self.snapshot.status_message = format!("{reason}；立即重试");
        self.snapshot.next_attempt_at = Some(now);
        now
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
        self.snapshot = QueueSnapshot {
            codex_path: self.snapshot.codex_path.clone(),
            runtime_warning: self.snapshot.runtime_warning.clone(),
            transport_status: self.snapshot.transport_status.clone(),
            status_message: "等待任务".into(),
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
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

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
        let mut rng = ChaCha8Rng::seed_from_u64(4);
        machine.schedule_retry(now, &RetryPolicy::default(), "busy", &mut rng);
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
    fn quiet_immediate_retry_does_not_enter_backoff_or_raise_alert() {
        let now = Utc.with_ymd_and_hms(2026, 7, 28, 1, 0, 0).unwrap();
        let mut machine = QueueMachine::new("codex".into());

        let next = machine.schedule_quiet_immediate_retry(now, "高需求提示");

        assert_eq!(next, now);
        assert_eq!(machine.snapshot.phase, QueuePhase::Connecting);
        assert_eq!(machine.snapshot.next_attempt_at, Some(now));
        assert_eq!(machine.snapshot.retry_alert_count, 0);
        assert!(machine.snapshot.status_message.contains("立即重试"));
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
    fn one_pending_message_is_preserved_with_conversation_history() {
        let mut machine = QueueMachine::new("codex".into());
        machine.append_conversation_message(ConversationRole::User, "first", false);
        machine.set_queued_message(PendingMessage {
            prompt: "second".into(),
            working_directory: None,
            failure_phrases: vec!["busy".into()],
        });

        let state = machine.persisted(None);
        let restored = QueueMachine::restore(state, "codex".into());

        assert_eq!(restored.snapshot().conversation.len(), 1);
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
