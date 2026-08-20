use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::classifier::TurnError;
pub use crate::gui_fallback::GuiFallbackTransport;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum TransportKind {
    #[default]
    Disconnected,
    AppServer,
    GuiFallback,
}

impl TransportKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "未连接",
            Self::AppServer => "Codex app-server",
            Self::GuiFallback => "GUI 兼容回退",
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
    ThreadResolved {
        previous_thread_id: String,
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
    #[error("GUI fallback is disabled: {0}")]
    FallbackDisabled(String),
    #[error("transport is unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait CodexTransport: Send {
    fn set_gui_fallback_enabled(&mut self, _enabled: bool) {}
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ActiveTransport {
    #[default]
    None,
    Primary,
    Fallback,
}

/// Routes to the app-server first and consults the GUI fallback only after an
/// explicit opt-in.  A primary connection failure can therefore never
/// silently activate desktop automation.
#[derive(Debug)]
pub struct PreferredTransport<P, F> {
    primary: P,
    fallback: F,
    fallback_enabled: bool,
    active: ActiveTransport,
}

impl<P, F> PreferredTransport<P, F> {
    #[must_use]
    pub fn new(primary: P, fallback: F, fallback_enabled: bool) -> Self {
        Self {
            primary,
            fallback,
            fallback_enabled,
            active: ActiveTransport::None,
        }
    }
}

#[async_trait]
impl<P, F> CodexTransport for PreferredTransport<P, F>
where
    P: CodexTransport,
    F: CodexTransport,
{
    fn set_gui_fallback_enabled(&mut self, enabled: bool) {
        self.fallback_enabled = enabled;
        self.fallback.set_gui_fallback_enabled(enabled);
    }

    fn set_app_server_window_hidden(&mut self, hidden: bool) {
        self.primary.set_app_server_window_hidden(hidden);
    }

    fn status(&self) -> TransportStatus {
        match self.active {
            ActiveTransport::Primary => self.primary.status(),
            ActiveTransport::Fallback => self.fallback.status(),
            ActiveTransport::None => TransportStatus::default(),
        }
    }

    async fn connect(&mut self) -> Result<(), TransportError> {
        match self.primary.connect().await {
            Ok(()) => {
                self.active = ActiveTransport::Primary;
                Ok(())
            }
            Err(primary_error) if self.fallback_enabled => {
                self.primary.shutdown().await;
                self.fallback.set_gui_fallback_enabled(true);
                self.fallback.connect().await.map_err(|fallback_error| {
                    TransportError::Unavailable(format!(
                        "app-server failed ({primary_error}); explicitly enabled GUI fallback also failed ({fallback_error})"
                    ))
                })?;
                self.active = ActiveTransport::Fallback;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn start_thread(&mut self, cwd: Option<&Path>) -> Result<ThreadSession, TransportError> {
        match self.active {
            ActiveTransport::Primary => self.primary.start_thread(cwd).await,
            ActiveTransport::Fallback => self.fallback.start_thread(cwd).await,
            ActiveTransport::None => Err(TransportError::NotConnected),
        }
    }

    async fn resume_thread(
        &mut self,
        thread_id: &str,
        cwd: Option<&Path>,
    ) -> Result<ThreadSession, TransportError> {
        match self.active {
            ActiveTransport::Primary => self.primary.resume_thread(thread_id, cwd).await,
            ActiveTransport::Fallback => self.fallback.resume_thread(thread_id, cwd).await,
            ActiveTransport::None => Err(TransportError::NotConnected),
        }
    }

    async fn start_turn(
        &mut self,
        thread_id: &str,
        prompt: &str,
        cwd: Option<&Path>,
    ) -> Result<StartedTurn, TransportError> {
        match self.active {
            ActiveTransport::Primary => self.primary.start_turn(thread_id, prompt, cwd).await,
            ActiveTransport::Fallback => self.fallback.start_turn(thread_id, prompt, cwd).await,
            ActiveTransport::None => Err(TransportError::NotConnected),
        }
    }

    async fn interrupt_turn(
        &mut self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<(), TransportError> {
        match self.active {
            ActiveTransport::Primary => self.primary.interrupt_turn(thread_id, turn_id).await,
            ActiveTransport::Fallback => self.fallback.interrupt_turn(thread_id, turn_id).await,
            ActiveTransport::None => Err(TransportError::NotConnected),
        }
    }

    async fn next_event(&mut self) -> Option<TransportEvent> {
        match self.active {
            ActiveTransport::Primary => self.primary.next_event().await,
            ActiveTransport::Fallback => self.fallback.next_event().await,
            ActiveTransport::None => None,
        }
    }

    async fn shutdown(&mut self) {
        self.primary.shutdown().await;
        self.fallback.shutdown().await;
        self.active = ActiveTransport::None;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[derive(Debug)]
    struct ConnectOnlyTransport {
        fail: bool,
        connects: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl CodexTransport for ConnectOnlyTransport {
        async fn connect(&mut self) -> Result<(), TransportError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(TransportError::ExecutableNotFound("codex".into()))
            } else {
                Ok(())
            }
        }

        async fn start_thread(
            &mut self,
            _: Option<&Path>,
        ) -> Result<ThreadSession, TransportError> {
            unreachable!()
        }

        async fn resume_thread(
            &mut self,
            _: &str,
            _: Option<&Path>,
        ) -> Result<ThreadSession, TransportError> {
            unreachable!()
        }

        async fn start_turn(
            &mut self,
            _: &str,
            _: &str,
            _: Option<&Path>,
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

    #[tokio::test]
    async fn a_disabled_gui_fallback_is_never_connected_silently() {
        let primary_connects = Arc::new(AtomicUsize::new(0));
        let fallback_connects = Arc::new(AtomicUsize::new(0));
        let mut transport = PreferredTransport::new(
            ConnectOnlyTransport {
                fail: true,
                connects: Arc::clone(&primary_connects),
            },
            ConnectOnlyTransport {
                fail: false,
                connects: Arc::clone(&fallback_connects),
            },
            false,
        );

        assert!(matches!(
            transport.connect().await,
            Err(TransportError::ExecutableNotFound(_))
        ));
        assert_eq!(primary_connects.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_connects.load(Ordering::SeqCst), 0);
    }
}
