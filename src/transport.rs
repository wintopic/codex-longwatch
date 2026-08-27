use std::path::Path;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::classifier::TurnError;

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
pub trait CodexTransport: Send {
    fn set_app_server_window_hidden(&mut self, _hidden: bool) {}
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
    async fn start_turn(
        &mut self,
        thread_id: &str,
        prompt: &str,
        cwd: Option<&Path>,
    ) -> Result<StartedTurn, TransportError>;
    async fn interrupt_turn(
        &mut self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<(), TransportError>;
    async fn next_event(&mut self) -> Option<TransportEvent>;
    async fn shutdown(&mut self);
}
