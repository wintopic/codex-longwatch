//! Explicitly enabled GUI/JSONL compatibility transport.
//!
//! This path is intentionally secondary.  It submits the exact user prompt
//! through the platform accessibility boundary and observes Codex's persisted
//! session JSONL for completion.  It never activates itself.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use serde_json::Value;
use tokio::{sync::mpsc, task::JoinHandle, time};
use uuid::Uuid;

use crate::{
    classifier::TurnError,
    jsonl::JsonlTailer,
    transport::{
        CodexTransport, RestoredTurn, StartedTurn, ThreadSession, TransportError, TransportEvent,
        TransportKind, TransportStatus, TurnStatus,
    },
};

#[derive(Debug)]
pub struct GuiFallbackTransport {
    enabled: bool,
    sessions_root: PathBuf,
    events_sender: Option<mpsc::Sender<TransportEvent>>,
    events: Option<mpsc::Receiver<TransportEvent>>,
    monitor_task: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct MonitorPlan {
    tailers: BTreeMap<PathBuf, JsonlTailer>,
    active_path: Option<PathBuf>,
    expected_prompt: Option<String>,
}

impl GuiFallbackTransport {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            sessions_root: codex_sessions_root(),
            events_sender: None,
            events: None,
            monitor_task: None,
        }
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }
}

#[async_trait]
impl CodexTransport for GuiFallbackTransport {
    fn set_gui_fallback_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    fn status(&self) -> TransportStatus {
        TransportStatus {
            kind: TransportKind::GuiFallback,
            connected: self.events.is_some(),
            server_agent: None,
        }
    }

    async fn connect(&mut self) -> Result<(), TransportError> {
        if !self.enabled {
            return Err(TransportError::FallbackDisabled(
                "GUI fallback requires an explicit opt-in".into(),
            ));
        }
        if gpui_platform::is_wayland_session() {
            return Err(TransportError::FallbackDisabled(
                "Wayland global input injection is disabled; use app-server".into(),
            ));
        }
        fs::create_dir_all(&self.sessions_root)
            .map_err(|error| TransportError::Unavailable(error.to_string()))?;
        let (sender, receiver) = mpsc::channel(128);
        self.events_sender = Some(sender);
        self.events = Some(receiver);
        Ok(())
    }

    async fn start_thread(&mut self, cwd: Option<&Path>) -> Result<ThreadSession, TransportError> {
        reject_working_directory(cwd)?;
        let path = latest_session_file(&self.sessions_root);
        let id = if let Some(path) = path.as_deref() {
            scan_session(path).unwrap_or_else(|_| (None, None)).0
        } else {
            None
        };
        Ok(ThreadSession {
            id: id.unwrap_or_else(|| format!("gui-pending-{}", Uuid::new_v4())),
            // A fresh queue must not adopt an unrelated turn that happened to
            // be active in the most recently modified Codex session.
            latest_turn: None,
        })
    }

    async fn resume_thread(
        &mut self,
        thread_id: &str,
        cwd: Option<&Path>,
    ) -> Result<ThreadSession, TransportError> {
        reject_working_directory(cwd)?;
        let path = find_session_file(&self.sessions_root, thread_id).or_else(|| {
            thread_id
                .starts_with("gui-pending-")
                .then(|| latest_session_file(&self.sessions_root))
                .flatten()
        });
        if path.is_none() && !thread_id.starts_with("gui-pending-") {
            return Err(TransportError::Unavailable(format!(
                "GUI fallback could not find persisted session {thread_id}"
            )));
        }
        // Establish the incremental baseline before the full-state scan. If a
        // completion lands between these two operations, either the scan sees
        // it or the tailer remains positioned before it; no event can fall
        // into a scan-then-seek gap during crash recovery.
        let mut prepared_tailer = if let Some(path) = path.as_ref() {
            let mut tailer = JsonlTailer::new(path.clone());
            tailer
                .seek_to_end()
                .map_err(|error| TransportError::Unavailable(error.to_string()))?;
            Some((path.clone(), tailer))
        } else {
            None
        };
        let (resolved_thread_id, latest_turn) = if let Some(path) = path.as_deref() {
            scan_session(path).unwrap_or_else(|_| (None, None))
        } else {
            (None, None)
        };
        let session_id = resolved_thread_id.unwrap_or_else(|| thread_id.to_owned());
        if let Some(turn) = latest_turn
            .as_ref()
            .filter(|turn| turn.status == TurnStatus::InProgress)
        {
            let mut tailers = BTreeMap::new();
            if let Some((path, tailer)) = prepared_tailer.take() {
                tailers.insert(path.clone(), tailer);
                self.start_monitor(
                    session_id.clone(),
                    turn.id.clone(),
                    MonitorPlan {
                        tailers,
                        active_path: Some(path),
                        expected_prompt: None,
                    },
                )?;
            }
        }
        Ok(ThreadSession {
            id: session_id,
            latest_turn,
        })
    }

    async fn start_turn(
        &mut self,
        thread_id: &str,
        prompt: &str,
        cwd: Option<&Path>,
    ) -> Result<StartedTurn, TransportError> {
        if !self.enabled {
            return Err(TransportError::FallbackDisabled(
                "GUI fallback requires an explicit opt-in".into(),
            ));
        }
        reject_working_directory(cwd)?;
        #[cfg(feature = "gui-fallback")]
        {
            use gpui_platform::{GuiAutomation, SystemGuiAutomation};
            let synthetic_turn = format!("gui-turn-{}", Uuid::new_v4());
            let tailers = session_tailers_at_end(&self.sessions_root)?;
            let monitor_prompt = prompt.to_owned();
            let automation_prompt = monitor_prompt.clone();
            tokio::task::spawn_blocking(move || {
                SystemGuiAutomation.submit_prompt(&automation_prompt)
            })
            .await
            .map_err(|error| TransportError::Unavailable(error.to_string()))?
            .map_err(|error| TransportError::Unavailable(error.to_string()))?;

            self.start_monitor(
                thread_id.to_owned(),
                synthetic_turn.clone(),
                MonitorPlan {
                    tailers,
                    active_path: None,
                    expected_prompt: Some(monitor_prompt),
                },
            )?;
            Ok(StartedTurn { id: synthetic_turn })
        }
        #[cfg(not(feature = "gui-fallback"))]
        {
            let _ = (thread_id, prompt);
            Err(TransportError::FallbackDisabled(
                "this build does not include the gui-fallback feature".into(),
            ))
        }
    }

    async fn interrupt_turn(
        &mut self,
        _thread_id: &str,
        _turn_id: &str,
    ) -> Result<(), TransportError> {
        Err(TransportError::Unavailable(
            "GUI fallback cannot safely issue a global interrupt; use the Codex window".into(),
        ))
    }

    async fn next_event(&mut self) -> Option<TransportEvent> {
        self.events.as_mut()?.recv().await
    }

    async fn shutdown(&mut self) {
        if let Some(task) = self.monitor_task.take() {
            task.abort();
        }
        self.events_sender = None;
        self.events = None;
    }
}

impl GuiFallbackTransport {
    fn start_monitor(
        &mut self,
        thread_id: String,
        synthetic_turn: String,
        plan: MonitorPlan,
    ) -> Result<(), TransportError> {
        if let Some(task) = self.monitor_task.take() {
            task.abort();
        }
        let sender = self
            .events_sender
            .clone()
            .ok_or(TransportError::NotConnected)?;
        let sessions_root = self.sessions_root.clone();
        self.monitor_task = Some(tokio::spawn(async move {
            monitor_session(sessions_root, thread_id, synthetic_turn, plan, sender).await;
        }));
        Ok(())
    }
}

async fn monitor_session(
    sessions_root: PathBuf,
    mut thread_id: String,
    logical_turn: String,
    mut plan: MonitorPlan,
    sender: mpsc::Sender<TransportEvent>,
) {
    let mut resolved_path = None;
    loop {
        if plan.active_path.is_none() {
            for path in session_files(&sessions_root) {
                plan.tailers
                    .entry(path.clone())
                    .or_insert_with(|| JsonlTailer::new(path));
            }
        }

        let paths = plan.active_path.as_ref().map_or_else(
            || plan.tailers.keys().cloned().collect::<Vec<_>>(),
            |path| vec![path.clone()],
        );
        for path in paths {
            let Some(tailer) = plan.tailers.get_mut(&path) else {
                continue;
            };
            match tailer.poll() {
                Ok(records) => {
                    for record in records {
                        let payload = record.value.get("payload").unwrap_or(&Value::Null);
                        let kind = payload
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if plan.active_path.is_none() {
                            let matches_prompt = kind == "user_message"
                                && plan.expected_prompt.as_deref().is_some_and(|expected| {
                                    payload
                                        .get("message")
                                        .or_else(|| payload.get("text"))
                                        .and_then(Value::as_str)
                                        == Some(expected)
                                });
                            if matches_prompt || kind == "task_started" {
                                plan.active_path = Some(path.clone());
                                match resolve_session_thread(
                                    &path,
                                    &mut thread_id,
                                    &logical_turn,
                                    &sender,
                                )
                                .await
                                {
                                    Ok(true) => resolved_path = Some(path.clone()),
                                    Ok(false) => {}
                                    Err(()) => return,
                                }
                            } else {
                                continue;
                            }
                        }
                        if plan.active_path.as_ref() != Some(&path) {
                            continue;
                        }
                        match kind {
                            "task_started" => {
                                if sender
                                    .send(TransportEvent::TurnStarted {
                                        thread_id: thread_id.clone(),
                                        turn_id: logical_turn.clone(),
                                    })
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            "agent_message" => {
                                if let Some(message) =
                                    payload.get("message").and_then(Value::as_str)
                                {
                                    if sender
                                        .send(TransportEvent::AgentMessageDelta {
                                            thread_id: thread_id.clone(),
                                            turn_id: logical_turn.clone(),
                                            delta: message.to_owned(),
                                        })
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            "task_complete" => {
                                let error = payload
                                    .get("error")
                                    .filter(|error| !error.is_null())
                                    .map(turn_error_from_json);
                                let status = if error.is_some() {
                                    TurnStatus::Failed
                                } else {
                                    TurnStatus::Completed
                                };
                                let final_message = payload
                                    .get("last_agent_message")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned();
                                let _ = sender
                                    .send(TransportEvent::TurnCompleted {
                                        thread_id,
                                        turn: RestoredTurn {
                                            id: logical_turn,
                                            status,
                                            final_message,
                                            error,
                                        },
                                    })
                                    .await;
                                return;
                            }
                            "turn_aborted" | "task_aborted" => {
                                let _ = sender
                                    .send(TransportEvent::TurnCompleted {
                                        thread_id,
                                        turn: RestoredTurn {
                                            id: logical_turn,
                                            status: TurnStatus::Interrupted,
                                            final_message: String::new(),
                                            error: None,
                                        },
                                    })
                                    .await;
                                return;
                            }
                            _ => {}
                        }
                    }
                }
                Err(error) => {
                    if sender
                        .send(TransportEvent::Diagnostic {
                            message: format!("GUI 回退 JSONL 读取失败：{error}"),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
        if let Some(path) = plan.active_path.as_ref()
            && resolved_path.as_ref() != Some(path)
        {
            match resolve_session_thread(path, &mut thread_id, &logical_turn, &sender).await {
                Ok(true) => resolved_path = Some(path.clone()),
                Ok(false) => {}
                Err(()) => return,
            }
        }
        time::sleep(Duration::from_millis(400)).await;
    }
}

async fn resolve_session_thread(
    path: &Path,
    thread_id: &mut String,
    turn_id: &str,
    sender: &mpsc::Sender<TransportEvent>,
) -> Result<bool, ()> {
    let Some(actual_thread_id) = scan_session(path).ok().and_then(|(id, _)| id) else {
        return Ok(false);
    };
    if actual_thread_id != *thread_id {
        sender
            .send(TransportEvent::ThreadResolved {
                previous_thread_id: thread_id.clone(),
                thread_id: actual_thread_id.clone(),
                turn_id: turn_id.to_owned(),
            })
            .await
            .map_err(|_| ())?;
        *thread_id = actual_thread_id;
    }
    Ok(true)
}

fn reject_working_directory(cwd: Option<&Path>) -> Result<(), TransportError> {
    if let Some(cwd) = cwd {
        return Err(TransportError::Unavailable(format!(
            "GUI fallback cannot safely select working directory {}; clear it or use app-server",
            cwd.display()
        )));
    }
    Ok(())
}

#[cfg(any(feature = "gui-fallback", test))]
fn session_tailers_at_end(
    sessions_root: &Path,
) -> Result<BTreeMap<PathBuf, JsonlTailer>, TransportError> {
    let mut tailers = BTreeMap::new();
    for path in session_files(sessions_root) {
        let mut tailer = JsonlTailer::new(path.clone());
        tailer
            .seek_to_end()
            .map_err(|error| TransportError::Unavailable(error.to_string()))?;
        tailers.insert(path, tailer);
    }
    Ok(tailers)
}

fn codex_sessions_root() -> PathBuf {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return PathBuf::from(home).join("sessions");
    }
    directories::BaseDirs::new().map_or_else(
        || PathBuf::from(".codex").join("sessions"),
        |directories| directories.home_dir().join(".codex").join("sessions"),
    )
}

fn session_files(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                files.push(path);
            }
        }
    }
    files
}

fn latest_session_file(root: &Path) -> Option<PathBuf> {
    session_files(root).into_iter().max_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    })
}

fn find_session_file(root: &Path, thread_id: &str) -> Option<PathBuf> {
    session_files(root).into_iter().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(thread_id))
    })
}

fn scan_session(path: &Path) -> Result<(Option<String>, Option<RestoredTurn>), std::io::Error> {
    let file = File::open(path)?;
    let mut thread_id = None;
    let mut latest = None;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let payload = value.get("payload").unwrap_or(&Value::Null);
        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                thread_id = payload
                    .get("id")
                    .or_else(|| payload.get("session_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("event_msg") => match payload.get("type").and_then(Value::as_str) {
                Some("task_started") => {
                    if let Some(id) = payload.get("turn_id").and_then(Value::as_str) {
                        latest = Some(RestoredTurn {
                            id: id.to_owned(),
                            status: TurnStatus::InProgress,
                            final_message: String::new(),
                            error: None,
                        });
                    }
                }
                Some("task_complete") => {
                    let id = payload
                        .get("turn_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown-gui-turn")
                        .to_owned();
                    let error = payload
                        .get("error")
                        .filter(|error| !error.is_null())
                        .map(turn_error_from_json);
                    latest = Some(RestoredTurn {
                        id,
                        status: if error.is_some() {
                            TurnStatus::Failed
                        } else {
                            TurnStatus::Completed
                        },
                        final_message: payload
                            .get("last_agent_message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        error,
                    });
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok((thread_id, latest))
}

fn turn_error_from_json(error: &Value) -> TurnError {
    if let Some(message) = error.as_str() {
        return TurnError {
            message: message.to_owned(),
            ..TurnError::default()
        };
    }
    TurnError {
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("GUI fallback reported an error")
            .to_owned(),
        additional_details: error
            .get("additionalDetails")
            .or_else(|| error.get("additional_details"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        codex_error_info: error
            .get("codexErrorInfo")
            .or_else(|| error.get("codex_error_info"))
            .cloned(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::tempdir;
    use tokio::time::timeout;

    use super::*;

    #[test]
    fn scans_latest_gui_turn_without_exposing_message_history() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("rollout-thread-1.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"thread-1"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"task_started","turn_id":"turn-1"}}}}"#
        )
        .unwrap();
        writeln!(file, r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"turn-1","last_agent_message":"done","error":null}}}}"#).unwrap();
        let (thread, turn) = scan_session(&path).unwrap();
        assert_eq!(thread.as_deref(), Some("thread-1"));
        assert_eq!(turn.unwrap().final_message, "done");
    }

    #[test]
    fn resume_baseline_cannot_skip_a_completion_written_before_the_state_scan() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("rollout-thread-1.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"thread-1"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"task_started","turn_id":"turn-1"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let mut tailer = JsonlTailer::new(path.clone());
        tailer.seek_to_end().unwrap();
        let mut append = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(append, r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"turn-1","last_agent_message":"done","error":null}}}}"#).unwrap();
        append.flush().unwrap();

        let (_, latest_turn) = scan_session(&path).unwrap();
        assert_eq!(latest_turn.unwrap().status, TurnStatus::Completed);
        assert!(
            tailer.poll().unwrap().iter().any(|record| {
                record.value["payload"]["type"].as_str() == Some("task_complete")
            })
        );
    }

    #[tokio::test]
    async fn completed_pending_session_resolves_to_the_persisted_thread_id() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("rollout-thread-real.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"thread-real"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"task_started","turn_id":"turn-1"}}}}"#
        )
        .unwrap();
        writeln!(file, r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"turn-1","last_agent_message":"done","error":null}}}}"#).unwrap();
        file.flush().unwrap();

        let mut transport = GuiFallbackTransport::new(true);
        transport.sessions_root = directory.path().to_path_buf();
        let session = transport
            .resume_thread("gui-pending-test", None)
            .await
            .unwrap();

        assert_eq!(session.id, "thread-real");
        assert_eq!(session.latest_turn.unwrap().status, TurnStatus::Completed);
    }

    #[tokio::test]
    async fn monitor_keeps_the_runtime_logical_turn_id() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("rollout-thread-1.jsonl");
        File::create(&path).unwrap();
        let mut tailer = JsonlTailer::new(path.clone());
        tailer.seek_to_end().unwrap();
        let mut tailers = BTreeMap::new();
        tailers.insert(path.clone(), tailer);
        let (sender, mut receiver) = mpsc::channel(8);
        let monitor = tokio::spawn(monitor_session(
            directory.path().to_path_buf(),
            "thread-1".into(),
            "gui-logical-turn".into(),
            MonitorPlan {
                tailers,
                active_path: Some(path.clone()),
                expected_prompt: None,
            },
            sender,
        ));

        let mut append = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(append, r#"{{"id":"one","type":"event_msg","payload":{{"type":"task_started","turn_id":"real-turn"}}}}"#).unwrap();
        writeln!(append, r#"{{"id":"two","type":"event_msg","payload":{{"type":"agent_message","message":"working"}}}}"#).unwrap();
        writeln!(append, r#"{{"id":"three","type":"event_msg","payload":{{"type":"task_complete","turn_id":"real-turn","last_agent_message":"done","error":null}}}}"#).unwrap();
        append.flush().unwrap();

        let events = timeout(Duration::from_secs(3), async {
            let mut events = Vec::new();
            while let Some(event) = receiver.recv().await {
                events.push(event);
                if matches!(events.last(), Some(TransportEvent::TurnCompleted { .. })) {
                    break;
                }
            }
            events
        })
        .await
        .unwrap();
        monitor.await.unwrap();

        assert!(events.iter().all(|event| match event {
            TransportEvent::TurnStarted { turn_id, .. }
            | TransportEvent::AgentMessageDelta { turn_id, .. } => {
                turn_id == "gui-logical-turn"
            }
            TransportEvent::TurnCompleted { turn, .. } => turn.id == "gui-logical-turn",
            _ => true,
        }));
    }

    #[tokio::test]
    async fn switching_sessions_does_not_replay_historical_completion() {
        let directory = tempdir().unwrap();
        let old_path = directory.path().join("rollout-thread-old.jsonl");
        let active_path = directory.path().join("rollout-thread-active.jsonl");
        for (path, thread_id) in [(&old_path, "thread-old"), (&active_path, "thread-active")] {
            let mut file = File::create(path).unwrap();
            writeln!(
                file,
                r#"{{"type":"session_meta","payload":{{"id":"{thread_id}"}}}}"#
            )
            .unwrap();
            writeln!(file, r#"{{"id":"old-start","type":"event_msg","payload":{{"type":"task_started","turn_id":"old-turn"}}}}"#).unwrap();
            writeln!(file, r#"{{"id":"old-done","type":"event_msg","payload":{{"type":"task_complete","turn_id":"old-turn","last_agent_message":"historical","error":null}}}}"#).unwrap();
        }

        let plan = MonitorPlan {
            tailers: session_tailers_at_end(directory.path()).unwrap(),
            active_path: None,
            expected_prompt: Some("real task".into()),
        };
        let (sender, mut receiver) = mpsc::channel(8);
        let monitor = tokio::spawn(monitor_session(
            directory.path().to_path_buf(),
            "logical-thread".into(),
            "logical-turn".into(),
            plan,
            sender,
        ));

        let mut append = fs::OpenOptions::new()
            .append(true)
            .open(&active_path)
            .unwrap();
        writeln!(append, r#"{{"id":"new-user","type":"event_msg","payload":{{"type":"user_message","message":"real task"}}}}"#).unwrap();
        writeln!(append, r#"{{"id":"new-start","type":"event_msg","payload":{{"type":"task_started","turn_id":"real-turn"}}}}"#).unwrap();
        writeln!(append, r#"{{"id":"new-done","type":"event_msg","payload":{{"type":"task_complete","turn_id":"real-turn","last_agent_message":"current","error":null}}}}"#).unwrap();
        append.flush().unwrap();

        let (completed_thread, completed, resolved) = timeout(Duration::from_secs(3), async {
            let mut resolved = false;
            loop {
                match receiver.recv().await {
                    Some(TransportEvent::ThreadResolved {
                        previous_thread_id,
                        thread_id,
                        turn_id,
                    }) => {
                        assert_eq!(previous_thread_id, "logical-thread");
                        assert_eq!(thread_id, "thread-active");
                        assert_eq!(turn_id, "logical-turn");
                        resolved = true;
                    }
                    Some(TransportEvent::TurnCompleted { thread_id, turn }) => {
                        break (thread_id, turn, resolved);
                    }
                    Some(_) => {}
                    None => panic!("monitor stopped before completion"),
                }
            }
        })
        .await
        .unwrap();
        monitor.await.unwrap();

        assert!(resolved);
        assert_eq!(completed_thread, "thread-active");
        assert_eq!(completed.id, "logical-turn");
        assert_eq!(completed.final_message, "current");
    }

    #[test]
    fn gui_fallback_rejects_an_unverifiable_working_directory() {
        let error = reject_working_directory(Some(Path::new("/tmp/project"))).unwrap_err();
        assert!(matches!(error, TransportError::Unavailable(_)));
    }
}
