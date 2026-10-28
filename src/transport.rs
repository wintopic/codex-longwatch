use std::path::Path;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{classifier::TurnError, config::ClientIdentity};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum TransportKind {
    #[default]
    Disconnected,
    AppServer,
}

impl TransportKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "未连接",
            Self::AppServer => "Codex app-server",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TransportStatus {
    pub kind: TransportKind,
    pub connected: bool,
    #[serde(default)]
    pub server_agent: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
    InProgress,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RestoredTurn {
    pub id: String,
    pub status: TurnStatus,
    #[serde(default)]
    pub final_message: String,
    #[serde(default)]
    pub error: Option<TurnError>,
    /// Authoritative completion timestamp supplied by Codex app-server.
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    /// `clientId` values of the user messages in this turn, i.e. the
    /// `clientUserMessageId`s that started or steered it.
    #[serde(default)]
    pub client_message_ids: Vec<String>,
    /// Whether the server reports `clientId` on user messages at all. Older
    /// servers omit the field, so a missing id proves nothing there.
    #[serde(default)]
    pub client_ids_reported: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThreadSession {
    pub id: String,
    pub latest_turn: Option<RestoredTurn>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StartedTurn {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransportEvent {
    AgentMessageDelta {
        thread_id: String,
        turn_id: String,
        delta: String,
    },
    TurnStarted {
        thread_id: String,
        turn_id: String,
    },
    Error {
        thread_id: String,
        turn_id: String,
        error: TurnError,
        will_retry: bool,
    },
    TurnCompleted {
        thread_id: String,
        turn: RestoredTurn,
    },
    /// Codex 0.160+ 新增的线程级状态通知。`systemError` 表示线程已经
    /// 无法继续推进，此时既不会再有 turn 事件，也不会自动恢复。
    ThreadSystemError {
        thread_id: String,
    },
    Disconnected {
        message: String,
    },
    Diagnostic {
        message: String,
    },
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("Codex executable was not found: {0}")]
    ExecutableNotFound(String),
    #[error("app-server process failed: {0}")]
    Process(String),
    #[error("app-server protocol error: {0}")]
    Protocol(String),
    #[error("app-server RPC error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("app-server request timed out: {0}")]
    Timeout(String),
    #[error("transport is not connected")]
    NotConnected,
}

#[async_trait]
// async-trait marks the boxed futures it returns `#[must_use]`, which
// clippy's `double_must_use` reports on every method.
#[allow(clippy::double_must_use)]
pub trait CodexTransport: Send {
    fn set_app_server_window_hidden(&mut self, _hidden: bool) {}
    /// Identity presented on the next `connect`.
    fn set_client_identity(&mut self, _identity: ClientIdentity) {}
    fn status(&self) -> TransportStatus {
        TransportStatus::default()
    }
    async fn connect(&mut self) -> Result<(), TransportError>;
    async fn start_thread(&mut self, cwd: Option<&Path>) -> Result<ThreadSession, TransportError>;
    async fn resume_thread(
        &mut self,
        thread_id: &str,
        cwd: Option<&Path>,
    ) -> Result<ThreadSession, TransportError>;
    /// `client_message_id` is sent as `clientUserMessageId`, exactly like the
    /// official clients, so the turn can be recognised after a reconnect.
    async fn start_turn(
        &mut self,
        thread_id: &str,
        prompt: &str,
        cwd: Option<&Path>,
        client_message_id: &str,
    ) -> Result<StartedTurn, TransportError>;
    async fn interrupt_turn(
        &mut self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<(), TransportError>;
    /// 从持久化历史里回捞某个回合的最终助手文本。
    ///
    /// Codex 0.160+ 的 `turn/completed` 只带 `itemsView: "notLoaded"` 的空
    /// items，流式阶段的 `item/completed` 才是正文来源。若客户端在断流后
    /// 重连、缓存缺失，就必须回到 `thread/read` 才能拿到权威答案。
    async fn fetch_turn_message(&mut self, _thread_id: &str, _turn_id: &str) -> Option<String> {
        None
    }
    async fn next_event(&mut self) -> Option<TransportEvent>;
    async fn shutdown(&mut self);
}
