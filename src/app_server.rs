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

use chrono::{DateTime, Utc};
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
    config::ClientIdentity,
    transport::{
        CodexTransport, RestoredTurn, StartedTurn, ThreadSession, TransportError, TransportEvent,
        TransportKind, TransportStatus, TurnStatus,
    },
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
/// Read by `codex app-server` and preferred over `clientInfo.name` for the
/// upstream `originator` header. The official desktop app sets it to
/// `Codex Desktop` for every app-server it starts.
const ORIGINATOR_OVERRIDE_ENV: &str = "CODEX_INTERNAL_ORIGINATOR_OVERRIDE";
/// `thread/start` source used by the official clients for user threads.
const THREAD_SOURCE: &str = "user";
/// High-volume notifications Longwatch never reads. Opting out keeps reasoning,
/// command output and diff streams off the stdio pipe, as the official desktop
/// app does for the ones it ignores.
const IGNORED_NOTIFICATIONS: &[&str] = &[
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/textDelta",
    "item/commandExecution/outputDelta",
    "item/commandExecution/terminalInteraction",
    "item/fileChange/outputDelta",
    "item/fileChange/patchUpdated",
    "item/plan/delta",
    "item/mcpToolCall/progress",
    "turn/diff/updated",
    "turn/plan/updated",
    "thread/tokenUsage/updated",
    "account/rateLimits/updated",
    "mcpServer/startupStatus/updated",
    "command/exec/outputDelta",
    "process/outputDelta",
    "fs/changed",
];
/// JSON-RPC codes the app-server uses for requests it cannot route.
const UNSUPPORTED_REQUEST_CODES: [i64; 2] = [-32600, -32601];
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The wire form of a [`ClientIdentity`]: `clientInfo` for `initialize` and
/// the originator override for the child environment.
struct WireIdentity {
    name: &'static str,
    title: Option<&'static str>,
    originator_override: Option<&'static str>,
}

const fn wire_identity(identity: ClientIdentity) -> WireIdentity {
    match identity {
        ClientIdentity::OfficialDesktop => WireIdentity {
            name: "codex_desktop",
            title: Some("Codex Desktop"),
            originator_override: Some("Codex Desktop"),
        },
        ClientIdentity::CodexCli => WireIdentity {
            name: "codex_cli_rs",
            title: None,
            originator_override: None,
        },
        ClientIdentity::Longwatch => WireIdentity {
            name: "codex-longwatch",
            title: Some("Longwatch"),
            originator_override: None,
        },
    }
}

type PendingMap = Arc<Mutex<HashMap<RequestId, oneshot::Sender<Result<Value, TransportError>>>>>;
type SharedWriter = Arc<Mutex<Option<ChildStdin>>>;
type CompletedMessages = Arc<Mutex<HashMap<(String, String), CompletedAgentMessage>>>;

#[derive(Clone, Debug)]
struct CompletedAgentMessage {
    text: String,
    explicit_final: bool,
    completed_at: Option<DateTime<Utc>>,
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
    identity: ClientIdentity,
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
            identity: ClientIdentity::default(),
            environment: Vec::new(),
            completed_messages: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[must_use]
    pub fn with_hidden_window(mut self, hidden: bool) -> Self {
        self.hide_window = hidden;
        self
    }

    #[must_use]
    pub fn with_client_identity(mut self, identity: ClientIdentity) -> Self {
        self.identity = identity;
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
                        let line = strip_ansi(&line);
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

    /// Latest turn of a thread with full items, for servers whose
    /// `thread/resume` response did not carry `initialTurnsPage`.
    async fn read_latest_turn(&self, thread_id: &str) -> Option<RestoredTurn> {
        match self
            .request(
                "thread/turns/list",
                json!({
                    "threadId": thread_id,
                    "limit": 1,
                    "sortDirection": "desc",
                    "itemsView": "full"
                }),
            )
            .await
        {
            Ok(response) => response
                .get("data")?
                .as_array()?
                .first()
                .and_then(|turn| parse_turn(turn).ok()),
            Err(error) if is_unsupported_request(&error) => {
                let response = self
                    .request(
                        "thread/read",
                        json!({"threadId": thread_id, "includeTurns": true}),
                    )
                    .await
                    .ok()?;
                response
                    .get("thread")?
                    .get("turns")?
                    .as_array()?
                    .last()
                    .and_then(|turn| parse_turn(turn).ok())
            }
            Err(error) => {
                debug!(%error, "无法读取线程的最近回合");
                None
            }
        }
    }
}

#[async_trait::async_trait]
impl CodexTransport for AppServerTransport {
    fn set_app_server_window_hidden(&mut self, hidden: bool) {
        self.hide_window = hidden;
    }

    fn set_client_identity(&mut self, identity: ClientIdentity) {
        self.identity = identity;
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
        // Only the originator override is owned by the identity setting, so
        // an inherited value (e.g. when launched from the desktop app's
        // terminal) cannot silently change who the requests claim to be.
        match wire_identity(self.identity).originator_override {
            Some(originator) => {
                command.env(ORIGINATOR_OVERRIDE_ENV, originator);
            }
            None => {
                command.env_remove(ORIGINATOR_OVERRIDE_ENV);
            }
        }
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
            .request("initialize", initialize_params(self.identity))
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
        let response = self
            .request("thread/start", thread_start_params(cwd))
            .await?;
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
        let response = self
            .request("thread/resume", thread_resume_params(thread_id, cwd))
            .await?;
        let mut session = parse_thread_session(&response)?;
        // Servers that predate `initialTurnsPage` may still honour
        // `excludeTurns`; ask for the latest turn explicitly so a crash
        // between `turn/start` and its response can be reconciled.
        if session.latest_turn.is_none()
            && response.get("initialTurnsPage").is_none_or(Value::is_null)
        {
            session.latest_turn = self.read_latest_turn(&session.id).await;
        }
        Ok(session)
    }

    async fn start_turn(
        &mut self,
        thread_id: &str,
        prompt: &str,
        cwd: Option<&Path>,
        client_message_id: &str,
    ) -> Result<StartedTurn, TransportError> {
        if !self.connected {
            return Err(TransportError::NotConnected);
        }
        let response = self
            .request(
                "turn/start",
                turn_start_params(thread_id, prompt, cwd, client_message_id),
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

    async fn fetch_turn_message(&mut self, thread_id: &str, turn_id: &str) -> Option<String> {
        if !self.connected {
            return None;
        }
        // The paginated item API reads one turn without hydrating the whole
        // thread; full `thread/read` hydration is deprecated and only kept as
        // a fallback for servers that predate `thread/items/list`.
        let items = match self
            .request(
                "thread/items/list",
                json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "sortDirection": "desc",
                    "limit": 50
                }),
            )
            .await
        {
            Ok(response) => {
                let mut items = response
                    .get("data")?
                    .as_array()?
                    .iter()
                    .filter(|entry| {
                        entry
                            .get("turnId")
                            .and_then(Value::as_str)
                            .is_none_or(|id| id == turn_id)
                    })
                    .filter_map(|entry| entry.get("item").cloned())
                    .collect::<Vec<_>>();
                items.reverse();
                Value::Array(items)
            }
            Err(error) if is_unsupported_request(&error) => {
                let response = self
                    .request(
                        "thread/read",
                        json!({"threadId": thread_id, "includeTurns": true}),
                    )
                    .await
                    .ok()?;
                let turns = response.get("thread")?.get("turns")?.as_array()?;
                turns
                    .iter()
                    .find(|turn| string_field(turn, "id").as_deref() == Some(turn_id))?
                    .get("items")?
                    .clone()
            }
            Err(error) => {
                debug!(%error, "无法回读回合正文");
                return None;
            }
        };
        let text = extract_final_message(Some(&items));
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
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

/// `initialize` exactly as the official clients send it: `clientInfo` with a
/// title plus `capabilities`.
fn initialize_params(identity: ClientIdentity) -> Value {
    let wire = wire_identity(identity);
    json!({
        "clientInfo": {
            "name": wire.name,
            "title": wire.title,
            "version": env!("CARGO_PKG_VERSION")
        },
        "capabilities": {
            "experimentalApi": true,
            "optOutNotificationMethods": IGNORED_NOTIFICATIONS
        }
    })
}

fn thread_start_params(cwd: Option<&Path>) -> Value {
    let mut params = json!({"threadSource": THREAD_SOURCE});
    if let Some(cwd) = cwd {
        params["cwd"] = Value::from(cwd.to_string_lossy().into_owned());
    }
    params
}

/// Resume without the deprecated full-history hydration; the latest turn is
/// requested as the first page instead.
fn thread_resume_params(thread_id: &str, cwd: Option<&Path>) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "excludeTurns": true,
        "initialTurnsPage": {"limit": 1, "sortDirection": "desc", "itemsView": "full"}
    });
    if let Some(cwd) = cwd {
        params["cwd"] = Value::from(cwd.to_string_lossy().into_owned());
    }
    params
}

fn turn_start_params(
    thread_id: &str,
    prompt: &str,
    cwd: Option<&Path>,
    client_message_id: &str,
) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "input": [{"type": "text", "text": prompt, "text_elements": []}],
        "clientUserMessageId": client_message_id
    });
    if let Some(cwd) = cwd {
        params["cwd"] = Value::from(cwd.to_string_lossy().into_owned());
    }
    params
}

/// The "decline" reply for every interactive request the app-server may send,
/// plus a short label for the status line. Unknown requests get a JSON-RPC
/// error so the server never waits on Longwatch.
fn decline_server_request(id: &RequestId, method: &str) -> (Value, &'static str) {
    let (result, label) = match method {
        "item/commandExecution/requestApproval" => (json!({"decision": "decline"}), "执行命令审批"),
        "item/fileChange/requestApproval" => (json!({"decision": "decline"}), "修改文件审批"),
        "item/permissions/requestApproval" => {
            (json!({"permissions": {}, "scope": "turn"}), "额外权限")
        }
        "execCommandApproval" | "applyPatchApproval" => (json!({"decision": "denied"}), "操作审批"),
        "mcpServer/elicitation/request" => (json!({"action": "decline"}), "MCP 表单输入"),
        "item/tool/requestUserInput" => (json!({"answers": {}}), "补充信息"),
        _ => {
            return (
                json!({
                    "id": id.to_value(),
                    "error": {"code": -32601, "message": format!("Longwatch does not handle {method}")}
                }),
                "交互操作",
            );
        }
    };
    (json!({"id": id.to_value(), "result": result}), label)
}

fn is_unsupported_request(error: &TransportError) -> bool {
    matches!(error, TransportError::Rpc { code, .. } if UNSUPPORTED_REQUEST_CODES.contains(code))
}

/// Codex colours its stderr logs; keep Longwatch's log file plain text.
fn strip_ansi(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && characters.peek() == Some(&'[') {
            characters.next();
            for code in characters.by_ref() {
                if code.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(character);
        }
    }
    plain
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
            // Nobody watches an unattended queue, so interactive server
            // requests are answered with the same "decline" the official
            // clients send. Nothing is ever approved, and the agent gets an
            // ordinary "rejected by user" result instead of a protocol error.
            let method = message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let (response, label) = decline_server_request(&id, method);
            warn!(method, "已自动拒绝需要人工处理的 Codex 请求");
            let _ = events.try_send(TransportEvent::Diagnostic {
                message: format!(
                    "Codex 请求了{label}，无人值守时已自动拒绝；如需放行请调整 Codex 审批策略"
                ),
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
            if let Some(cached) = cached {
                if turn.final_message.trim().is_empty() {
                    turn.final_message = cached.text;
                }
                // The final agent item is the most precise source. Fall back
                // to the turn timestamp when older servers omit it.
                turn.completed_at = cached.completed_at.or(turn.completed_at);
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
            completed_at: params
                .get("completedAtMs")
                .and_then(Value::as_i64)
                .and_then(DateTime::<Utc>::from_timestamp_millis),
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
        "thread/status/changed" => {
            let status = params.get("status")?.get("type")?.as_str()?;
            if status != "systemError" {
                return None;
            }
            Some(TransportEvent::ThreadSystemError {
                thread_id: string_field(params, "threadId")?,
            })
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
    // `thread/resume` answers with the requested first page (newest first);
    // servers that ignore the paging request still embed `thread.turns`.
    let latest_turn = value
        .get("initialTurnsPage")
        .and_then(|page| page.get("data"))
        .and_then(Value::as_array)
        .and_then(|turns| turns.first())
        .or_else(|| {
            thread
                .get("turns")
                .and_then(Value::as_array)
                .and_then(|turns| turns.last())
        })
        .and_then(|turn| parse_turn(turn).ok());
    Ok(ThreadSession { id, latest_turn })
}

fn parse_turn(value: &Value) -> Result<RestoredTurn, TransportError> {
    let id = string_field(value, "id")
        .ok_or_else(|| TransportError::Protocol("turn payload has no id".into()))?;
    let raw_status = string_field(value, "status")
        .ok_or_else(|| TransportError::Protocol("turn payload has no status".into()))?;
    // 未知状态不能直接丢弃整个 turn/completed：那会让运行时一直等到回合
    // 超时。宁可把它当成一次可见的失败，让用户看到真实原因。
    let mut unknown_status = None;
    let status = match raw_status.as_str() {
        "completed" => TurnStatus::Completed,
        "interrupted" => TurnStatus::Interrupted,
        "failed" => TurnStatus::Failed,
        "inProgress" | "in_progress" => TurnStatus::InProgress,
        other => {
            unknown_status = Some(other.to_owned());
            TurnStatus::Failed
        }
    };
    let error = value
        .get("error")
        .filter(|error| !error.is_null())
        .and_then(|error| serde_json::from_value::<TurnError>(error.clone()).ok())
        .or_else(|| {
            unknown_status.map(|status| TurnError {
                message: format!("Codex 返回了未知的回合状态：{status}"),
                additional_details: None,
                codex_error_info: Some(Value::String("other".into())),
            })
        });
    let final_message = extract_final_message(value.get("items"));
    let completed_at = value
        .get("completedAt")
        .and_then(Value::as_i64)
        .and_then(|timestamp| DateTime::<Utc>::from_timestamp(timestamp, 0));
    let user_messages = value
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| string_field(item, "type").as_deref() == Some("userMessage"));
    let client_ids_reported = user_messages
        .clone()
        .any(|item| item.get("clientId").is_some());
    let client_message_ids = user_messages
        .filter_map(|item| string_field(item, "clientId"))
        .collect();
    Ok(RestoredTurn {
        id,
        status,
        final_message,
        error,
        completed_at,
        client_message_ids,
        client_ids_reported,
    })
}

fn extract_final_message(items: Option<&Value>) -> String {
    let Some(items) = items.and_then(Value::as_array) else {
        return String::new();
    };
    let find_phase = |wanted: &str| {
        items.iter().rev().find_map(|item| {
            (string_field(item, "type").as_deref() == Some("agentMessage")
                && string_field(item, "phase").as_deref() == Some(wanted))
            .then(|| string_field(item, "text").unwrap_or_default())
        })
    };
    // Codex 0.160+ 的 MessagePhase 变为 commentary / partial_answer /
    // final_answer 三档。partial_answer 是"确定的答案但后面可能还有内容"，
    // 只能作为 final_answer 缺失时的回退，不能和 commentary 混为一谈。
    find_phase("final_answer")
        .or_else(|| find_phase("partial_answer"))
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
            "completedAt": 1_725_000_000,
            "items": [
                {"type": "reasoning", "id": "r", "summary": []},
                {"type": "agentMessage", "id": "a1", "text": "first"},
                {"type": "agentMessage", "id": "a2", "text": "final", "phase": "final_answer"},
                {"type": "agentMessage", "id": "a3", "text": "progress", "phase": "commentary"}
            ]
        });
        let parsed = parse_turn(&turn).unwrap();
        assert_eq!(parsed.final_message, "final");
        assert_eq!(
            parsed.completed_at,
            DateTime::<Utc>::from_timestamp(1_725_000_000, 0)
        );
    }

    #[test]
    fn completed_agent_item_is_authoritative_but_commentary_is_not() {
        let final_item = parse_completed_agent_message(&json!({
            "threadId": "th",
            "turnId": "tu",
            "completedAtMs": 1_725_000_000_123_i64,
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
        assert_eq!(
            final_item.1.completed_at,
            DateTime::<Utc>::from_timestamp_millis(1_725_000_000_123)
        );

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

    #[test]
    fn official_desktop_identity_matches_the_desktop_handshake() {
        let params = initialize_params(ClientIdentity::OfficialDesktop);

        assert_eq!(params["clientInfo"]["name"], "codex_desktop");
        assert_eq!(params["clientInfo"]["title"], "Codex Desktop");
        assert_eq!(params["capabilities"]["experimentalApi"], true);
        let opted_out = params["capabilities"]["optOutNotificationMethods"]
            .as_array()
            .unwrap();
        assert!(opted_out.contains(&json!("turn/diff/updated")));
        // Longwatch reads these, so they must never be suppressed.
        for needed in [
            "item/agentMessage/delta",
            "item/completed",
            "turn/started",
            "turn/completed",
            "error",
            "thread/status/changed",
        ] {
            assert!(
                !opted_out.contains(&json!(needed)),
                "{needed} was opted out"
            );
        }
        assert_eq!(
            wire_identity(ClientIdentity::OfficialDesktop).originator_override,
            Some("Codex Desktop")
        );
    }

    #[test]
    fn cli_and_longwatch_identities_never_claim_the_desktop_originator() {
        assert_eq!(
            initialize_params(ClientIdentity::CodexCli)["clientInfo"]["name"],
            "codex_cli_rs"
        );
        assert_eq!(
            initialize_params(ClientIdentity::Longwatch)["clientInfo"]["name"],
            "codex-longwatch"
        );
        assert!(
            wire_identity(ClientIdentity::CodexCli)
                .originator_override
                .is_none()
        );
        assert!(
            wire_identity(ClientIdentity::Longwatch)
                .originator_override
                .is_none()
        );
    }

    #[test]
    fn thread_and_turn_requests_use_the_official_shapes() {
        assert_eq!(thread_start_params(None), json!({"threadSource": "user"}));
        assert_eq!(
            thread_start_params(Some(Path::new("C:/work")))["cwd"],
            "C:/work"
        );

        let turn = turn_start_params("th", "do it", None, "client-1");
        assert_eq!(
            turn,
            json!({
                "threadId": "th",
                "input": [{"type": "text", "text": "do it", "text_elements": []}],
                "clientUserMessageId": "client-1"
            })
        );

        let resume = thread_resume_params("th", None);
        assert_eq!(resume["excludeTurns"], true);
        assert_eq!(resume["initialTurnsPage"]["limit"], 1);
        assert_eq!(resume["initialTurnsPage"]["sortDirection"], "desc");
        assert_eq!(resume["initialTurnsPage"]["itemsView"], "full");
    }

    #[test]
    fn resume_prefers_the_first_page_and_falls_back_to_embedded_turns() {
        let paged = parse_thread_session(&json!({
            "thread": {"id": "th", "turns": []},
            "initialTurnsPage": {"data": [{
                "id": "newest",
                "status": "completed",
                "items": [
                    {"type": "userMessage", "id": "u", "clientId": "client-9", "content": []},
                    {"type": "agentMessage", "id": "a", "text": "done", "phase": "final_answer"}
                ]
            }]}
        }))
        .unwrap();
        let latest = paged.latest_turn.unwrap();
        assert_eq!(latest.id, "newest");
        assert_eq!(latest.client_message_ids, ["client-9"]);
        assert!(latest.client_ids_reported);
        assert_eq!(latest.final_message, "done");

        let legacy = parse_thread_session(&json!({
            "thread": {"id": "th", "turns": [
                {"id": "old", "status": "completed", "items": []},
                {"id": "last", "status": "interrupted", "items": [
                    {"type": "userMessage", "id": "u", "content": []}
                ]}
            ]}
        }))
        .unwrap();
        let latest = legacy.latest_turn.unwrap();
        assert_eq!(latest.id, "last");
        assert!(latest.client_message_ids.is_empty());
        assert!(!latest.client_ids_reported);
    }

    #[test]
    fn interactive_requests_are_declined_never_approved() {
        let id = RequestId::String("req".into());
        for (method, expected) in [
            (
                "item/commandExecution/requestApproval",
                json!({"decision": "decline"}),
            ),
            (
                "item/fileChange/requestApproval",
                json!({"decision": "decline"}),
            ),
            ("execCommandApproval", json!({"decision": "denied"})),
            (
                "mcpServer/elicitation/request",
                json!({"action": "decline"}),
            ),
            ("item/tool/requestUserInput", json!({"answers": {}})),
            (
                "item/permissions/requestApproval",
                json!({"permissions": {}, "scope": "turn"}),
            ),
        ] {
            let (response, _) = decline_server_request(&id, method);
            assert_eq!(response["id"], "req");
            assert_eq!(response["result"], expected, "{method}");
        }
        let (unknown, _) = decline_server_request(&RequestId::Number(4), "attestation/generate");
        assert_eq!(unknown["id"], 4);
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[test]
    fn stderr_colour_codes_are_removed() {
        assert_eq!(
            strip_ansi("\u{1b}[2m2026\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m failed"),
            "2026 ERROR failed"
        );
        assert_eq!(strip_ansi("plain"), "plain");
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
