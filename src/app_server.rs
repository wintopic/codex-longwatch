//! JSON-RPC transport for the official Codex app-server.
//!
//! The app-server speaks newline-delimited JSON on stdin/stdout.  This module
//! deliberately keeps the wire format as `serde_json::Value`: the protocol is
//! versioned independently of this application and a small, tolerant decoder
//! lets older and newer Codex CLI releases interoperate.

use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tracing::{debug, warn};

use crate::{
    classifier::TurnError,
    transport::{
        CodexTransport, RestoredTurn, StartedTurn, ThreadSession, TransportError, TransportEvent,
        TransportKind, TransportStatus, TurnStatus,
    },
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const CLIENT_NAME: &str = "codex-longwatch";
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

type PendingMap = Arc<Mutex<HashMap<RequestId, oneshot::Sender<Result<Value, TransportError>>>>>;
type SharedWriter = Arc<Mutex<Option<ChildStdin>>>;
type CompletedMessages = Arc<Mutex<HashMap<(String, String), CompletedAgentMessage>>>;

#[derive(Clone, Debug)]
struct CompletedAgentMessage {
    text: String,
    explicit_final: bool,
}

/// JSON-RPC request identifiers are either strings or signed integers. We
/// generate positive integer ids, but server-initiated requests may use either
/// representation and must be answered with the exact same id.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RequestId {
    Number(i64),
    String(String),
}

impl RequestId {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(value) => Some(Self::String(value.clone())),
            Value::Number(value) => value.as_i64().map(Self::Number),
            _ => None,
        }
    }

    fn to_value(&self) -> Value {
        match self {
            Self::Number(value) => Value::from(*value),
            Self::String(value) => Value::from(value.clone()),
        }
    }
}

/// A stdio app-server connection.
pub struct AppServerTransport {
    codex_path: PathBuf,
    request_id: AtomicU64,
    pending: PendingMap,
    writer: SharedWriter,
    events: Option<mpsc::Receiver<TransportEvent>>,
    reader_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
    child_task: Option<JoinHandle<()>>,
    connected: bool,
    server_agent: Option<String>,
    hide_window: bool,
    environment: Vec<(OsString, OsString)>,
    completed_messages: CompletedMessages,
}

impl std::fmt::Debug for AppServerTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppServerTransport")
            .field("codex_path", &self.codex_path)
            .field("connected", &self.connected)
            .finish_non_exhaustive()
    }
}

impl AppServerTransport {
    #[must_use]
    pub fn new(codex_path: PathBuf) -> Self {
        Self {
            codex_path,
            request_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            writer: Arc::new(Mutex::new(None)),
            events: None,
            reader_task: None,
            stderr_task: None,
            child_task: None,
            connected: false,
            server_agent: None,
            hide_window: false,
            environment: Vec::new(),
            completed_messages: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[must_use]
    pub fn with_hidden_window(mut self, hidden: bool) -> Self {
        self.hide_window = hidden;
        self
    }

    /// Add an environment override for the child process.  Normal application
    /// use does not need this; it is also useful for deterministic fake-server
    /// integration tests and custom Codex wrappers.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.push((key.into(), value.into()));
        self
    }

    #[must_use]
    pub fn codex_path(&self) -> &Path {
        &self.codex_path
    }

    async fn send_line(&self, message: &Value) -> Result<(), TransportError> {
        let line = serde_json::to_vec(message)
            .map_err(|error| TransportError::Protocol(error.to_string()))?;
        let mut guard = self.writer.lock().await;
        let stdin = guard.as_mut().ok_or(TransportError::NotConnected)?;
        stdin
            .write_all(&line)
            .await
            .map_err(|error| TransportError::Process(error.to_string()))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|error| TransportError::Process(error.to_string()))?;
        stdin
            .flush()
            .await
            .map_err(|error| TransportError::Process(error.to_string()))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, TransportError> {
        let id = RequestId::Number(self.request_id.fetch_add(1, Ordering::Relaxed) as i64);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), sender);
        let message = json!({"id": id.to_value(), "method": method, "params": params});
        if let Err(error) = self.send_line(&message).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        match timeout(REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(TransportError::Process(
                "app-server reader stopped before the response".into(),
            )),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(TransportError::Timeout(method.into()))
            }
        }
    }

    fn spawn_io(
        &mut self,
        mut child: Child,
        stdout: tokio::process::ChildStdout,
        stderr: tokio::process::ChildStderr,
        stdin: ChildStdin,
        #[cfg(target_os = "windows")] process_job: Option<gpui_platform::ProcessJob>,
    ) {
        let (event_sender, event_receiver) = mpsc::channel(128);
        // Every connection owns physically separate RPC state. A reader from
        // an aborted connection can therefore never drain or overwrite a new
        // connection's pending requests or writer.
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let writer = Arc::new(Mutex::new(Some(stdin)));
        let completed_messages = Arc::new(Mutex::new(HashMap::new()));
        self.pending = Arc::clone(&pending);
        self.writer = Arc::clone(&writer);
        self.completed_messages = Arc::clone(&completed_messages);
        let reader_events = event_sender.clone();
        self.events = Some(event_receiver);

        self.reader_task = Some(tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(&line) {
                            Ok(message) => {
                                if let Err(error) = dispatch_message(
                                    message,
                                    &pending,
                                    &writer,
                                    &completed_messages,
                                    &reader_events,
                                )
                                .await
                                {
                                    debug!(%error, "ignored malformed app-server message");
                                }
                            }
                            Err(error) => {
                                let _ = reader_events
                                    .send(TransportEvent::Diagnostic {
                                        message: format!("无法解析 app-server 消息：{error}"),
                                    })
                                    .await;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = reader_events
                            .send(TransportEvent::Disconnected {
                                message: error.to_string(),
                            })
                            .await;
                        break;
                    }
                }
            }
            let _ = reader_events
                .send(TransportEvent::Disconnected {
                    message: "app-server stdout closed".into(),
                })
                .await;
            let mut pending = pending.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(TransportError::Process(
                    "app-server stdout closed".into(),
                )));
            }
        }));

        self.stderr_task = Some(tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if !line.trim().is_empty() => {
                        warn!(target: "app_server_stderr", message = %line, "Codex app-server stderr");
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(error) => {
                        warn!(target: "app_server_stderr", %error, "读取 Codex app-server stderr 失败");
                        break;
                    }
                }
            }
        }));

        self.child_task = Some(tokio::spawn(async move {
            #[cfg(target_os = "windows")]
            let _process_job = process_job;
            match child.wait().await {
                Ok(status) if !status.success() => {
                    warn!(%status, "app-server exited with a failure status");
                    let _ = event_sender
                        .send(TransportEvent::Disconnected {
                            message: format!("app-server exited with {status}"),
                        })
                        .await;
                }
                Ok(_) => {
                    let _ = event_sender
                        .send(TransportEvent::Disconnected {
                            message: "app-server exited".into(),
                        })
                        .await;
                }
                Err(error) => {
                    let _ = event_sender
                        .send(TransportEvent::Disconnected {
                            message: error.to_string(),
                        })
                        .await;
                }
            }
        }));

        // The writer is kept behind a mutex so shutdown is deterministic and
        // all JSON lines are serialized.
    }

    async fn initialized(&self) -> Result<(), TransportError> {
        self.send_line(&json!({"method": "initialized", "params": {}}))
            .await
    }
}

#[async_trait::async_trait]
impl CodexTransport for AppServerTransport {
    fn set_app_server_window_hidden(&mut self, hidden: bool) {
        self.hide_window = hidden;
    }

    fn status(&self) -> TransportStatus {
        TransportStatus {
            kind: TransportKind::AppServer,
            connected: self.connected,
            server_agent: self.server_agent.clone(),
        }
    }

    async fn connect(&mut self) -> Result<(), TransportError> {
        if self.connected {
            return Ok(());
        }
        if self.reader_task.is_some() || self.stderr_task.is_some() || self.child_task.is_some() {
            self.shutdown().await;
        }
        // Resolve through PATH/PATHEXT before spawning. On Windows, npm places
        // both an extensionless POSIX shim and a runnable `codex.cmd` in the
        // same directory; passing bare `codex` to CreateProcess can select the
        // former and fail with Access Denied.
        let executable = resolve_codex_path(&self.codex_path);
        let mut command = Command::new(&executable);
        command
            .arg("app-server")
            .arg("--listen")
            .arg("stdio://")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(target_os = "windows")]
        if self.hide_window {
            command.creation_flags(CREATE_NO_WINDOW);
        }
        // Keep the parent environment intact so the official CLI can resolve
        // the user's existing CODEX_HOME, login state, and provider config.
        command.envs(self.environment.iter().cloned());
        command.kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                TransportError::ExecutableNotFound(self.codex_path.display().to_string())
            } else {
                TransportError::Process(error.to_string())
            }
        })?;
        #[cfg(target_os = "windows")]
        let process_job = child.id().and_then(
            |process_id| match gpui_platform::ProcessJob::assign(process_id) {
                Ok(job) => Some(job),
                Err(error) => {
                    warn!(%error, process_id, "无法将 app-server 加入 Windows Job Object");
                    None
                }
            },
        );
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Process("app-server stdin was not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Process("app-server stdout was not piped".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| TransportError::Process("app-server stderr was not piped".into()))?;
        self.spawn_io(
            child,
            stdout,
            stderr,
            stdin,
            #[cfg(target_os = "windows")]
            process_job,
        );
        self.connected = true;

        let initialize = self
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": CLIENT_NAME,
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await;
        let initialize = match initialize {
            Ok(initialize) => initialize,
            Err(error) => {
                self.shutdown().await;
                return Err(error);
            }
        };
        self.server_agent = initialize
            .get("userAgent")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Err(error) = self.initialized().await {
            self.shutdown().await;
            return Err(error);
        }
        Ok(())
    }

    async fn start_thread(&mut self, cwd: Option<&Path>) -> Result<ThreadSession, TransportError> {
        if !self.connected {
            return Err(TransportError::NotConnected);
        }
        let params = json!({
            "cwd": cwd.map(|path| path.to_string_lossy().into_owned()),
            "ephemeral": false,
            "sessionStartSource": "startup",
            "threadSource": "codex-longwatch"
        });
        let response = self.request("thread/start", params).await?;
        parse_thread_session(&response)
    }

    async fn resume_thread(
        &mut self,
        thread_id: &str,
        cwd: Option<&Path>,
    ) -> Result<ThreadSession, TransportError> {
        if !self.connected {
            return Err(TransportError::NotConnected);
        }
        let params = json!({
            "threadId": thread_id,
            "cwd": cwd.map(|path| path.to_string_lossy().into_owned())
        });
        let response = self.request("thread/resume", params).await?;
        parse_thread_session(&response)
    }

    async fn start_turn(
        &mut self,
        thread_id: &str,
        prompt: &str,
        cwd: Option<&Path>,
    ) -> Result<StartedTurn, TransportError> {
        if !self.connected {
            return Err(TransportError::NotConnected);
        }
        let response = self
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": prompt}],
                    "cwd": cwd.map(|path| path.to_string_lossy().into_owned())
                }),
            )
            .await?;
        let turn_id = response
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| TransportError::Protocol("turn/start response has no turn.id".into()))?;
        Ok(StartedTurn {
            id: turn_id.to_owned(),
        })
    }

    async fn interrupt_turn(
        &mut self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<(), TransportError> {
        if !self.connected {
            return Err(TransportError::NotConnected);
        }
        self.request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )
        .await
        .map(|_| ())
    }

    async fn next_event(&mut self) -> Option<TransportEvent> {
        self.events.as_mut()?.recv().await
    }

    async fn shutdown(&mut self) {
        self.connected = false;
        self.server_agent = None;
        self.writer.lock().await.take();
        if let Some(mut task) = self.child_task.take()
            && timeout(Duration::from_secs(3), &mut task).await.is_err()
        {
            // Aborting drops the Child and the Windows Job Object guard. The
            // latter terminates any descendants spawned by Codex as well.
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        self.events = None;
        self.pending.lock().await.clear();
        self.completed_messages.lock().await.clear();
    }
}

fn resolve_codex_path(path: &Path) -> PathBuf {
    which::which(path).unwrap_or_else(|_| path.to_path_buf())
}

async fn dispatch_message(
    message: Value,
    pending: &PendingMap,
    writer: &SharedWriter,
    completed_messages: &CompletedMessages,
    events: &mpsc::Sender<TransportEvent>,
) -> Result<(), TransportError> {
    if message.get("id").is_some() {
        let id =
            RequestId::from_value(message.get("id").unwrap_or(&Value::Null)).ok_or_else(|| {
                TransportError::Protocol("JSON-RPC id must be a string or integer".into())
            })?;
        if message.get("method").is_some() {
            // App-server requests (approvals, user input, etc.) are never
            // silently automated.  Return a JSON-RPC method-not-found error so
            // the turn stops visibly instead of granting permissions.
            let method = message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            warn!(method, "已拒绝需要人工交互的 Codex 服务端请求");
            let _ = events.try_send(TransportEvent::Diagnostic {
                message: format!(
                    "Codex 请求了交互式操作（{method}），已自动拒绝；请改用无需审批的运行策略后重试"
                ),
            });
            let response = json!({
                "id": id.to_value(),
                "error": {"code": -32601, "message": "Longwatch does not handle server requests"}
            });
            let mut guard = writer.lock().await;
            if let Some(stdin) = guard.as_mut() {
                let mut line = serde_json::to_vec(&response)
                    .map_err(|error| TransportError::Protocol(error.to_string()))?;
                line.push(b'\n');
                stdin
                    .write_all(&line)
                    .await
                    .map_err(|error| TransportError::Process(error.to_string()))?;
                stdin
                    .flush()
                    .await
                    .map_err(|error| TransportError::Process(error.to_string()))?;
            }
            return Ok(());
        }
        let result = if let Some(error) = message.get("error") {
            Err(parse_rpc_error(error)?)
        } else {
            Ok(message.get("result").cloned().unwrap_or(Value::Null))
        };
        if let Some(sender) = pending.lock().await.remove(&id) {
            let _ = sender.send(result);
        }
        return Ok(());
    }

    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    if method == "item/completed" {
        if let Some((key, message)) = parse_completed_agent_message(&params) {
            let mut messages = completed_messages.lock().await;
            let should_replace = messages
                .get(&key)
                .is_none_or(|current| message.explicit_final || !current.explicit_final);
            if should_replace {
                messages.insert(key, message);
            }
        }
        return Ok(());
    }
    if let Some(mut event) = parse_event(method, &params) {
        if let TransportEvent::TurnCompleted { thread_id, turn } = &mut event {
            let cached = completed_messages
                .lock()
                .await
                .remove(&(thread_id.clone(), turn.id.clone()));
            if turn.final_message.trim().is_empty()
                && let Some(cached) = cached
            {
                turn.final_message = cached.text;
            }
        }
        if matches!(&event, TransportEvent::AgentMessageDelta { .. }) {
            // Streaming preview updates are lossy by design. Dropping a delta
            // under backpressure is preferable to blocking the same reader
            // that must deliver authoritative RPC responses and completion.
            let _ = events.try_send(event);
        } else {
            events
                .send(event)
                .await
                .map_err(|error| TransportError::Process(error.to_string()))?;
        }
    }
    Ok(())
}

fn parse_rpc_error(error: &Value) -> Result<TransportError, TransportError> {
    let code = error.get("code").and_then(Value::as_i64).ok_or_else(|| {
        TransportError::Protocol(format!("JSON-RPC error has no integer code: {error}"))
    })?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| TransportError::Protocol(format!("JSON-RPC error has no message: {error}")))?
        .to_owned();
    let data = error
        .get("data")
        .cloned()
        .or_else(|| error.get("codexErrorInfo").cloned());
    Ok(TransportError::Rpc {
        code,
        message,
        data,
    })
}

fn parse_completed_agent_message(
    params: &Value,
) -> Option<((String, String), CompletedAgentMessage)> {
    let item = params.get("item")?;
    if string_field(item, "type").as_deref() != Some("agentMessage") {
        return None;
    }
    let phase = item.get("phase").and_then(Value::as_str);
    if phase == Some("commentary") {
        return None;
    }
    Some((
        (
            string_field(params, "threadId")?,
            string_field(params, "turnId")?,
        ),
        CompletedAgentMessage {
            text: string_field(item, "text")?,
            explicit_final: phase == Some("final_answer"),
        },
    ))
}

fn parse_event(method: &str, params: &Value) -> Option<TransportEvent> {
    match method {
        "item/agentMessage/delta" => Some(TransportEvent::AgentMessageDelta {
            thread_id: string_field(params, "threadId")?,
            turn_id: string_field(params, "turnId")?,
            delta: string_field(params, "delta")?,
        }),
        "turn/started" => Some(TransportEvent::TurnStarted {
            thread_id: string_field(params, "threadId")?,
            turn_id: params
                .get("turn")
                .and_then(|turn| string_field(turn, "id"))
                .or_else(|| string_field(params, "turnId"))?,
        }),
        "error" => Some(TransportEvent::Error {
            thread_id: string_field(params, "threadId")?,
            turn_id: string_field(params, "turnId")?,
            error: serde_json::from_value(params.get("error")?.clone()).ok()?,
            will_retry: params
                .get("willRetry")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "turn/completed" => {
            let thread_id = string_field(params, "threadId")?;
            let turn = parse_turn(params.get("turn")?).ok()?;
            Some(TransportEvent::TurnCompleted { thread_id, turn })
        }
        _ => None,
    }
}

fn parse_thread_session(value: &Value) -> Result<ThreadSession, TransportError> {
    let thread = value
        .get("thread")
        .ok_or_else(|| TransportError::Protocol("thread response has no thread object".into()))?;
    let id = string_field(thread, "id")
        .ok_or_else(|| TransportError::Protocol("thread response has no thread.id".into()))?;
    let latest_turn = thread
        .get("turns")
        .and_then(Value::as_array)
        .and_then(|turns| turns.last())
        .and_then(|turn| parse_turn(turn).ok());
    Ok(ThreadSession { id, latest_turn })
}

fn parse_turn(value: &Value) -> Result<RestoredTurn, TransportError> {
    let id = string_field(value, "id")
        .ok_or_else(|| TransportError::Protocol("turn payload has no id".into()))?;
    let status = match string_field(value, "status")
        .ok_or_else(|| TransportError::Protocol("turn payload has no status".into()))?
        .as_str()
    {
        "completed" => TurnStatus::Completed,
        "interrupted" => TurnStatus::Interrupted,
        "failed" => TurnStatus::Failed,
        "inProgress" | "in_progress" => TurnStatus::InProgress,
        other => {
            return Err(TransportError::Protocol(format!(
                "unknown turn status {other}"
            )));
        }
    };
    let error = value
        .get("error")
        .filter(|error| !error.is_null())
        .and_then(|error| serde_json::from_value::<TurnError>(error.clone()).ok());
    let final_message = extract_final_message(value.get("items"));
    Ok(RestoredTurn {
        id,
        status,
        final_message,
        error,
    })
}

fn extract_final_message(items: Option<&Value>) -> String {
    let Some(items) = items.and_then(Value::as_array) else {
        return String::new();
    };
    items
        .iter()
        .rev()
        .find_map(|item| {
            (string_field(item, "type").as_deref() == Some("agentMessage")
                && string_field(item, "phase").as_deref() == Some("final_answer"))
            .then(|| string_field(item, "text").unwrap_or_default())
        })
        .or_else(|| {
            items.iter().rev().find_map(|item| {
                (string_field(item, "type").as_deref() == Some("agentMessage")
                    && string_field(item, "phase").as_deref() != Some("commentary"))
                .then(|| string_field(item, "text").unwrap_or_default())
            })
        })
        .unwrap_or_default()
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_agent_messages_from_a_turn() {
        let turn = json!({
            "id": "t1",
            "status": "completed",
            "items": [
                {"type": "reasoning", "id": "r", "summary": []},
                {"type": "agentMessage", "id": "a1", "text": "first"},
                {"type": "agentMessage", "id": "a2", "text": "final", "phase": "final_answer"},
                {"type": "agentMessage", "id": "a3", "text": "progress", "phase": "commentary"}
            ]
        });
        let parsed = parse_turn(&turn).unwrap();
        assert_eq!(parsed.final_message, "final");
    }

    #[test]
    fn completed_agent_item_is_authoritative_but_commentary_is_not() {
        let final_item = parse_completed_agent_message(&json!({
            "threadId": "th",
            "turnId": "tu",
            "item": {
                "id": "item-1",
                "type": "agentMessage",
                "text": "final answer",
                "phase": "final_answer"
            }
        }))
        .unwrap();
        assert_eq!(final_item.0, ("th".into(), "tu".into()));
        assert_eq!(final_item.1.text, "final answer");
        assert!(final_item.1.explicit_final);

        assert!(
            parse_completed_agent_message(&json!({
                "threadId": "th",
                "turnId": "tu",
                "item": {
                    "id": "item-2",
                    "type": "agentMessage",
                    "text": "still working",
                    "phase": "commentary"
                }
            }))
            .is_none()
        );
    }

    #[test]
    fn decodes_internal_retry_error() {
        let event = parse_event(
            "error",
            &json!({
                "threadId": "th",
                "turnId": "tu",
                "willRetry": true,
                "error": {"message": "busy", "codexErrorInfo": "serverOverloaded"}
            }),
        );
        assert!(matches!(
            event,
            Some(TransportEvent::Error {
                will_retry: true,
                ..
            })
        ));
    }

    #[test]
    fn preserves_string_and_integer_json_rpc_ids() {
        let string_id = RequestId::from_value(&json!("server-request")).unwrap();
        assert_eq!(string_id.to_value(), json!("server-request"));
        let integer_id = RequestId::from_value(&json!(-7)).unwrap();
        assert_eq!(integer_id.to_value(), json!(-7));
        assert!(RequestId::from_value(&Value::Null).is_none());
    }

    #[test]
    fn preserves_structured_json_rpc_errors_for_runtime_classification() {
        let error = parse_rpc_error(&json!({
            "code": -32000,
            "message": "Server overloaded; retry later.",
            "data": {
                "codexErrorInfo": "serverOverloaded",
                "httpStatusCode": 503
            }
        }))
        .unwrap();

        assert!(matches!(
            error,
            TransportError::Rpc {
                code: -32000,
                message,
                data: Some(_),
            } if message.contains("overloaded")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn resolves_a_windows_command_shim_instead_of_an_extensionless_shell_script() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("codex"), "#!/bin/sh\n").unwrap();
        std::fs::write(directory.path().join("codex.cmd"), "@echo off\r\n").unwrap();

        let resolved = resolve_codex_path(&directory.path().join("codex"));

        assert_eq!(
            resolved
                .extension()
                .and_then(|extension| extension.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("cmd")
        );
    }
}
