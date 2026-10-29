use std::{
    fs::{self, File},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;
use tracing::warn;

use crate::{
    backoff::{AttemptLedger, RetryPolicy},
    queue::{ConversationMessage, PendingMessage, QueuePhase},
};

pub const CONFIG_VERSION: u32 = 3;
pub const STATE_VERSION: u32 = 1;
pub const SCHEDULER_STATE_VERSION: u32 = 2;
/// The number of independent Codex conversations Longwatch may run at once.
/// Keeping this deliberately small avoids turning a desktop helper into an
/// accidental request fan-out tool while still allowing a handful of tasks to
/// make progress in parallel.
pub const MIN_CONCURRENT_SESSIONS: u8 = 1;
pub const MAX_CONCURRENT_SESSIONS: u8 = 8;
pub const CURRENT_HIGH_DEMAND_PHRASE: &str =
    "We're currently experiencing high demand, which may cause temporary errors.";

/// How Longwatch introduces itself to `codex app-server`.
///
/// The app-server forwards this identity upstream as the `originator` header
/// and the User-Agent prefix of every model request, so it decides whether
/// Longwatch's requests are indistinguishable from the official clients.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ClientIdentity {
    /// Same identity as the official `ChatGPT` / Codex desktop app.
    #[default]
    OfficialDesktop,
    /// Same identity as the official Codex CLI.
    CodexCli,
    /// Longwatch's own identity.
    Longwatch,
}

impl ClientIdentity {
    pub const ALL: [Self; 3] = [Self::OfficialDesktop, Self::CodexCli, Self::Longwatch];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::OfficialDesktop => "官方桌面版",
            Self::CodexCli => "官方 CLI",
            Self::Longwatch => "Longwatch",
        }
    }

    /// The `originator` value upstream services will see.
    #[must_use]
    pub const fn originator(self) -> &'static str {
        match self {
            Self::OfficialDesktop => "Codex Desktop",
            Self::CodexCli => "codex_cli_rs",
            Self::Longwatch => "codex-longwatch",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
// These independent switches map directly to user-facing preferences.
#[allow(clippy::struct_excessive_bools)]
pub struct QueueConfig {
    pub version: u32,
    pub prompt: String,
    pub default_prompt: String,
    pub auto_send_default_prompt: bool,
    /// Working directory of the current task.
    pub working_directory: Option<PathBuf>,
    /// Folder preselected for new tasks. Kept apart from the task's own
    /// `working_directory` so finishing a task never changes the default.
    pub default_working_directory: Option<PathBuf>,
    /// Model of the current task; `None` lets the user's Codex config decide.
    pub model: Option<String>,
    /// Reasoning effort sent along with `model`.
    pub reasoning_effort: Option<String>,
    /// Model and effort preselected for new tasks, kept apart from the task's
    /// own choice for the same reason as the default folder. `None` is
    /// 跟随 Codex 设置: the model and effort in the user's Codex config.
    pub default_model: Option<String>,
    pub default_reasoning_effort: Option<String>,
    pub codex_path: PathBuf,
    pub failure_phrases: Vec<String>,
    pub full_screen_flash_enabled: bool,
    pub audio_alert_enabled: bool,
    /// Number of independent Codex app-server sessions to keep available.
    /// `1` preserves the original serial queue behaviour.
    pub max_concurrent_sessions: u8,
    /// Identity presented to `codex app-server`; applied on the next connect.
    pub client_identity: ClientIdentity,
    pub retry_policy: RetryPolicy,
    pub turn_timeout_secs: u64,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            prompt: String::new(),
            default_prompt: String::new(),
            auto_send_default_prompt: false,
            working_directory: None,
            default_working_directory: None,
            model: None,
            reasoning_effort: None,
            default_model: None,
            default_reasoning_effort: None,
            codex_path: PathBuf::from("codex"),
            failure_phrases: vec![
                "Server overloaded; retry later.".into(),
                "We're experiencing high demand. Please try again later.".into(),
                CURRENT_HIGH_DEMAND_PHRASE.into(),
            ],
            // 静默排队是默认体验：完成只发通知和提示音，不再整屏染色。
            full_screen_flash_enabled: false,
            audio_alert_enabled: true,
            max_concurrent_sessions: MIN_CONCURRENT_SESSIONS,
            client_identity: ClientIdentity::default(),
            retry_policy: RetryPolicy::default(),
            turn_timeout_secs: 30 * 60,
        }
    }
}

impl QueueConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_settings()?;
        if self.prompt.trim().is_empty() {
            return Err(ConfigError::EmptyPrompt);
        }
        Ok(())
    }

    pub fn validate_settings(&self) -> Result<(), ConfigError> {
        self.retry_policy
            .validate()
            .map_err(ConfigError::RetryPolicy)?;
        if self.turn_timeout_secs < 60 {
            return Err(ConfigError::TurnTimeoutTooShort);
        }
        if !(MIN_CONCURRENT_SESSIONS..=MAX_CONCURRENT_SESSIONS)
            .contains(&self.max_concurrent_sessions)
        {
            return Err(ConfigError::ConcurrentSessionsOutOfRange {
                min: MIN_CONCURRENT_SESSIONS,
                max: MAX_CONCURRENT_SESSIONS,
                actual: self.max_concurrent_sessions,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct PersistedQueueState {
    pub version: u32,
    pub phase: QueuePhase,
    pub status_message: String,
    pub active_thread_id: Option<String>,
    pub active_turn_id: Option<String>,
    /// Set after a turn request has entered the transport but before a
    /// response is known.  A restart must reconcile the persistent thread
    /// before sending another request, otherwise the same attempt could be
    /// submitted twice.
    #[serde(default)]
    pub submission_uncertain: bool,
    /// `clientUserMessageId` of the most recent `turn/start`. Codex echoes it
    /// on the user message item, which lets a restart tell whether the latest
    /// turn in the thread is ours or the submission never arrived.
    #[serde(default)]
    pub pending_client_message_id: Option<String>,
    #[serde(default)]
    pub queued_message: Option<PendingMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversation: Vec<ConversationMessage>,
    #[serde(default)]
    pub retry_as_follow_up: bool,
    pub attempt_count: u64,
    pub consecutive_retries: u32,
    /// Monotonic counter used by the UI to show one red full-screen pulse for
    /// every retry-producing error without coupling the runtime to a window.
    #[serde(default)]
    pub retry_alert_count: u64,
    pub empty_reply_count: u8,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub reply_preview: String,
    /// Authoritative completion time copied from the Codex app-server turn.
    #[serde(default)]
    pub reply_completed_at: Option<DateTime<Utc>>,
    pub attempts: AttemptLedger,
    pub prompt_digest: Option<String>,
}

impl Default for PersistedQueueState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            phase: QueuePhase::Idle,
            status_message: "等待任务".into(),
            active_thread_id: None,
            active_turn_id: None,
            submission_uncertain: false,
            pending_client_message_id: None,
            queued_message: None,
            conversation: Vec::new(),
            retry_as_follow_up: false,
            attempt_count: 0,
            consecutive_retries: 0,
            retry_alert_count: 0,
            empty_reply_count: 0,
            next_attempt_at: None,
            reply_preview: String::new(),
            reply_completed_at: None,
            attempts: AttemptLedger::default(),
            prompt_digest: None,
        }
    }
}

/// State owned by the multi-session scheduler rather than any individual
/// Codex conversation. Session-specific configuration and recovery state live
/// below `sessions/<id>/`, so queued work can survive an unexpected exit
/// without mixing thread identifiers between workers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct PersistedSchedulerState {
    pub version: u32,
    pub selected_session_id: Option<u64>,
    pub next_session_id: u64,
    pub pending_tasks: Vec<PersistedPendingTask>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct PersistedPendingTask {
    pub config: QueueConfig,
    /// `None` starts a fresh independent conversation in the next available
    /// lane. A session id keeps a deferred follow-up attached to its original
    /// Codex thread instead of silently moving it to another slot.
    pub target_session_id: Option<u64>,
    /// Targeted tasks normally submit `config` as the next user message. This
    /// flag represents the pause/resume action, which must not consume the
    /// editor contents as a new prompt.
    pub resume_existing: bool,
}

impl Default for PersistedSchedulerState {
    fn default() -> Self {
        Self {
            version: SCHEDULER_STATE_VERSION,
            selected_session_id: None,
            next_session_id: 1,
            pending_tasks: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConfigStore {
    directory: PathBuf,
}

impl ConfigStore {
    pub fn discover() -> Result<Self, ConfigError> {
        // Keep a deterministic override for local UI/integration previews without
        // changing the paths used by packaged release builds.
        #[cfg(debug_assertions)]
        if let Some(directory) =
            std::env::var_os("LONGWATCH_CONFIG_DIR").filter(|directory| !directory.is_empty())
        {
            return Ok(Self {
                directory: PathBuf::from(directory),
            });
        }

        let project =
            ProjectDirs::from("", "", "Longwatch").ok_or(ConfigError::NoConfigDirectory)?;
        let mut directory = project.config_dir().to_path_buf();
        let legacy_projects = [
            ProjectDirs::from("io.github", "wintopic", "Longwatch"),
            ProjectDirs::from("io", "codexqueue", "CodexQueue"),
        ];
        if !directory.exists() {
            for legacy_project in legacy_projects.into_iter().flatten() {
                let legacy_directory = legacy_project.config_dir().to_path_buf();
                if !legacy_directory.exists() {
                    continue;
                }
                let migrated = directory
                    .parent()
                    .is_some_and(|parent| fs::create_dir_all(parent).is_ok())
                    && fs::rename(&legacy_directory, &directory).is_ok();
                if !migrated {
                    warn!(
                        legacy = %legacy_directory.display(),
                        current = %directory.display(),
                        "无法迁移旧版配置目录，将继续使用原目录"
                    );
                    directory = legacy_directory;
                }
                break;
            }
        }
        Ok(Self { directory })
    }

    #[must_use]
    pub fn in_directory(directory: PathBuf) -> Self {
        Self { directory }
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.directory.join("longwatch.lock")
    }

    pub fn load_config(&self) -> Result<QueueConfig, ConfigError> {
        let path = self.directory.join("config.json");
        if !path.exists() {
            return Ok(QueueConfig::default());
        }
        match read_json(&path).and_then(migrate_config) {
            Ok(config) => Ok(config),
            Err(error) if is_recoverable_document_error(&error) => {
                let backup = quarantine_invalid_document(&path)?;
                warn!(
                    %error,
                    backup = %backup.display(),
                    "配置文件损坏或版本不兼容，已备份并恢复默认配置"
                );
                Ok(QueueConfig::default())
            }
            Err(error) => Err(error),
        }
    }

    pub fn save_config(&self, config: &QueueConfig) -> Result<(), ConfigError> {
        atomic_write_json(&self.directory.join("config.json"), config)
    }

    pub fn load_state(&self) -> Result<PersistedQueueState, ConfigError> {
        let path = self.directory.join("state.json");
        if !path.exists() {
            return Ok(PersistedQueueState::default());
        }
        match read_json(&path).and_then(migrate_state) {
            Ok(mut state) => {
                // Conversation rendering is a temporary UI concern. Older
                // state files may contain a large local transcript; discard it
                // while keeping the Codex thread identifiers for recovery.
                let had_conversation = !state.conversation.is_empty();
                state.conversation.clear();
                if had_conversation && let Err(error) = atomic_write_json(&path, &state) {
                    warn!(
                        %error,
                        path = %path.display(),
                        "清理旧会话正文失败，将继续使用内存中的精简状态"
                    );
                }
                Ok(state)
            }
            Err(error) if is_recoverable_document_error(&error) => {
                let backup = quarantine_invalid_document(&path)?;
                warn!(
                    %error,
                    backup = %backup.display(),
                    "运行状态文件损坏或版本不兼容，已备份并恢复为空闲状态"
                );
                Ok(PersistedQueueState::default())
            }
            Err(error) => Err(error),
        }
    }

    pub fn save_state(&self, state: &PersistedQueueState) -> Result<(), ConfigError> {
        let mut state = state.clone();
        state.conversation.clear();
        atomic_write_json(&self.directory.join("state.json"), &state)
    }

    /// Returns an isolated persistence namespace for one Codex session.
    #[must_use]
    pub fn session(&self, id: u64) -> Self {
        Self {
            directory: self.directory.join("sessions").join(id.to_string()),
        }
    }

    /// Removes the local persistence namespace for a completed temporary session.
    pub fn remove_session(&self, id: u64) -> Result<(), ConfigError> {
        let directory = self.directory.join("sessions").join(id.to_string());
        if directory.exists() {
            fs::remove_dir_all(&directory).map_err(|source| ConfigError::Io {
                path: directory,
                source,
            })?;
        }
        Ok(())
    }

    /// Lists session namespaces that contain persisted runtime state.
    pub fn session_ids(&self) -> Result<Vec<u64>, ConfigError> {
        let directory = self.directory.join("sessions");
        if !directory.exists() {
            return Ok(Vec::new());
        }
        let entries = fs::read_dir(&directory).map_err(|source| ConfigError::Io {
            path: directory.clone(),
            source,
        })?;
        let mut ids = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u64>().ok())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    pub fn load_scheduler_state(&self) -> Result<PersistedSchedulerState, ConfigError> {
        let path = self.directory.join("scheduler.json");
        if !path.exists() {
            return Ok(PersistedSchedulerState::default());
        }
        match read_json::<Value>(&path).and_then(migrate_scheduler_state) {
            Ok(state) => Ok(state),
            Err(error) if is_recoverable_document_error(&error) => {
                let backup = quarantine_invalid_document(&path)?;
                warn!(
                    %error,
                    backup = %backup.display(),
                    "多会话调度状态损坏或版本不兼容，已备份并恢复为空队列"
                );
                Ok(PersistedSchedulerState::default())
            }
            Err(error) => Err(error),
        }
    }

    pub fn save_scheduler_state(&self, state: &PersistedSchedulerState) -> Result<(), ConfigError> {
        atomic_write_json(&self.directory.join("scheduler.json"), state)
    }
}

#[must_use]
pub fn prompt_digest(prompt: &str) -> String {
    let digest = Sha256::digest(prompt.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn migrate_config(mut value: Value) -> Result<QueueConfig, ConfigError> {
    let version = document_version(&value)?;
    if let Value::Object(object) = &mut value {
        // Older releases exposed this as a preference. The app-server window
        // is now always hidden, so the legacy value is intentionally ignored.
        object.remove("hideAppServerWindow");
        // GUI input fallback was removed; discard the old persisted switch so
        // it cannot reappear when an older configuration is upgraded.
        object.remove("guiFallbackEnabled");
        remove_legacy_attempt_limits(object);
        migrate_default_prompt(object);
        // An empty value means "none": never hand Codex an empty cwd or model.
        for key in [
            "workingDirectory",
            "defaultWorkingDirectory",
            "model",
            "reasoningEffort",
            "defaultModel",
            "defaultReasoningEffort",
        ] {
            if object
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|path| path.trim().is_empty())
            {
                object.insert(key.into(), Value::Null);
            }
        }
        // Before the default folder had its own field, the last task's
        // working directory doubled as it.
        if !object.contains_key("defaultWorkingDirectory")
            && let Some(directory) = object.get("workingDirectory").cloned()
        {
            object.insert("defaultWorkingDirectory".into(), directory);
        }
        // Concurrency is opt-in. Older documents therefore migrate to the
        // original one-session behaviour unless the user explicitly adds the
        // new setting.
        object
            .entry("maxConcurrentSessions")
            .or_insert_with(|| Value::from(MIN_CONCURRENT_SESSIONS));
    }
    match version {
        0 => {
            if let Value::Object(object) = &mut value {
                object.insert("version".into(), Value::from(CONFIG_VERSION));
                append_current_high_demand_phrase(object);
                // Failed attempts are retried right away now, so the old
                // fixed interval has nothing left to configure.
                object.remove("retryIntervalSeconds");
                migrate_default_empty_reply_limit(object);
            }
            serde_json::from_value(value).map_err(ConfigError::Json)
        }
        1 => {
            if let Value::Object(object) = &mut value {
                object.insert("version".into(), Value::from(CONFIG_VERSION));
                append_current_high_demand_phrase(object);
                migrate_default_empty_reply_limit(object);
            }
            serde_json::from_value(value).map_err(ConfigError::Json)
        }
        2 => {
            if let Value::Object(object) = &mut value {
                object.insert("version".into(), Value::from(CONFIG_VERSION));
            }
            serde_json::from_value(value).map_err(ConfigError::Json)
        }
        3 => serde_json::from_value(value).map_err(ConfigError::Json),
        unsupported => Err(ConfigError::UnsupportedVersion(unsupported)),
    }
}

fn append_current_high_demand_phrase(object: &mut serde_json::Map<String, Value>) {
    let Some(Value::Array(phrases)) = object.get_mut("failurePhrases") else {
        return;
    };
    let already_present = phrases
        .iter()
        .filter_map(Value::as_str)
        .any(|phrase| phrase == CURRENT_HIGH_DEMAND_PHRASE);
    if !already_present {
        phrases.push(Value::String(CURRENT_HIGH_DEMAND_PHRASE.into()));
    }
}

fn remove_legacy_attempt_limits(object: &mut serde_json::Map<String, Value>) {
    let Some(Value::Object(policy)) = object.get_mut("retryPolicy") else {
        return;
    };
    policy.remove("maxAttemptsPerHour");
    policy.remove("maxAttemptsPerDay");
}

fn migrate_default_prompt(object: &mut serde_json::Map<String, Value>) {
    if object.contains_key("defaultPrompt") {
        return;
    }
    let prompt = object
        .get("prompt")
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));
    object.insert("defaultPrompt".into(), prompt);
}

fn migrate_default_empty_reply_limit(object: &mut serde_json::Map<String, Value>) {
    let Some(Value::Object(policy)) = object.get_mut("retryPolicy") else {
        return;
    };
    if policy.get("maxEmptyReplies").and_then(Value::as_u64) == Some(3) {
        policy.insert("maxEmptyReplies".into(), Value::from(5));
    }
}

fn migrate_state(mut value: Value) -> Result<PersistedQueueState, ConfigError> {
    let version = document_version(&value)?;
    match version {
        0 => {
            if let Value::Object(object) = &mut value {
                object.insert("version".into(), Value::from(STATE_VERSION));
            }
            serde_json::from_value(value).map_err(ConfigError::Json)
        }
        1 => serde_json::from_value(value).map_err(ConfigError::Json),
        unsupported => Err(ConfigError::UnsupportedVersion(unsupported)),
    }
}

fn migrate_scheduler_state(mut value: Value) -> Result<PersistedSchedulerState, ConfigError> {
    let version = document_version(&value)?;
    match version {
        0 | 1 => {
            if let Value::Object(object) = &mut value {
                object.insert("version".into(), Value::from(SCHEDULER_STATE_VERSION));
                if !object.contains_key("pendingTasks") {
                    let pending_messages = object
                        .remove("pendingMessages")
                        .and_then(|messages| messages.as_array().cloned())
                        .unwrap_or_default();
                    let pending_tasks = pending_messages
                        .into_iter()
                        .filter_map(|message| {
                            serde_json::from_value::<PendingMessage>(message)
                                .ok()
                                .map(|message| {
                                    let config = QueueConfig {
                                        prompt: message.prompt,
                                        working_directory: message.working_directory,
                                        failure_phrases: message.failure_phrases,
                                        ..QueueConfig::default()
                                    };
                                    serde_json::json!({ "config": config })
                                })
                        })
                        .collect();
                    object.insert("pendingTasks".into(), Value::Array(pending_tasks));
                }
            }
            serde_json::from_value(value).map_err(ConfigError::Json)
        }
        2 => serde_json::from_value(value).map_err(ConfigError::Json),
        unsupported => Err(ConfigError::UnsupportedVersion(unsupported)),
    }
}

fn document_version(value: &Value) -> Result<u64, ConfigError> {
    match value.get("version") {
        None => Ok(0),
        Some(version) => version
            .as_u64()
            .ok_or_else(|| ConfigError::InvalidVersion(version.to_string())),
    }
}

fn is_recoverable_document_error(error: &ConfigError) -> bool {
    matches!(
        error,
        ConfigError::Json(_) | ConfigError::UnsupportedVersion(_) | ConfigError::InvalidVersion(_)
    )
}

fn quarantine_invalid_document(path: &Path) -> Result<PathBuf, ConfigError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ConfigError::InvalidPath(path.to_path_buf()))?;
    let backup = path.with_file_name(format!(
        "{file_name}.corrupt-{}",
        Utc::now().timestamp_millis()
    ));
    fs::rename(path, &backup).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(backup)
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, ConfigError> {
    let file = File::open(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_reader(BufReader::new(file)).map_err(ConfigError::Json)
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), ConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| ConfigError::InvalidPath(path.into()))?;
    fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| ConfigError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer_pretty(&mut writer, value).map_err(ConfigError::Json)?;
        writer.write_all(b"\n").map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        writer.flush().map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;

    temporary.persist(path).map_err(|error| ConfigError::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("操作系统未提供可用的标准配置目录")]
    NoConfigDirectory,
    #[error("请先输入任务内容再开始排队")]
    EmptyPrompt,
    #[error("单回合超时时间不能少于 60 秒")]
    TurnTimeoutTooShort,
    #[error("并发会话数必须在 {min} 到 {max} 之间（当前为 {actual}）")]
    ConcurrentSessionsOutOfRange { min: u8, max: u8, actual: u8 },
    #[error("不支持配置文件版本 {0}")]
    UnsupportedVersion(u64),
    #[error("配置文件版本字段无效：{0}")]
    InvalidVersion(String),
    #[error("配置路径无效：{path}", path = .0.display())]
    InvalidPath(PathBuf),
    #[error("读写 {path} 失败：{source}", path = .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON 内容无效：{0}")]
    Json(#[source] serde_json::Error),
    #[error("重试策略无效：{0}")]
    RetryPolicy(#[source] crate::backoff::RetryPolicyError),
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    #[test]
    fn completion_alert_defaults_keep_the_shell_quiet() {
        let config = QueueConfig::default();
        assert!(!config.full_screen_flash_enabled);
        assert!(config.audio_alert_enabled);
    }

    #[test]
    fn existing_prompt_becomes_the_initial_customizable_default() {
        let migrated = migrate_config(json!({
            "version": CONFIG_VERSION,
            "prompt": "continue the task"
        }))
        .unwrap();

        assert_eq!(migrated.default_prompt, "continue the task");
    }

    #[test]
    fn settings_can_be_saved_before_the_first_task_is_entered() {
        let config = QueueConfig {
            default_prompt: "quick action".into(),
            auto_send_default_prompt: true,
            ..QueueConfig::default()
        };

        assert!(config.validate_settings().is_ok());
        assert!(matches!(config.validate(), Err(ConfigError::EmptyPrompt)));
    }

    #[test]
    fn new_tasks_follow_the_codex_config_until_the_user_picks_a_model() {
        // A config written before the model picker existed sends no model, so
        // Codex keeps using the one in its own config.toml.
        let older = migrate_config(json!({"version": CONFIG_VERSION, "prompt": "task"})).unwrap();
        assert_eq!(older.default_model, None);
        assert_eq!(older.default_reasoning_effort, None);
        assert_eq!(older.model, None);

        let chosen = QueueConfig {
            default_model: Some("gpt-6-sol".into()),
            default_reasoning_effort: Some("high".into()),
            ..QueueConfig::default()
        };
        let reloaded = migrate_config(serde_json::to_value(&chosen).unwrap()).unwrap();
        assert_eq!(reloaded.default_model.as_deref(), Some("gpt-6-sol"));
        assert_eq!(reloaded.default_reasoning_effort.as_deref(), Some("high"));

        // An empty value written by hand also means "follow Codex".
        let blank = migrate_config(json!({
            "version": CONFIG_VERSION,
            "defaultModel": " ",
            "defaultReasoningEffort": ""
        }))
        .unwrap();
        assert_eq!(blank.default_model, None);
        assert_eq!(blank.default_reasoning_effort, None);
    }

    #[test]
    fn the_legacy_retry_interval_is_dropped() {
        let migrated = migrate_config(json!({
            "prompt": "real task",
            "retryIntervalSeconds": 12
        }))
        .unwrap();
        assert_eq!(migrated.version, CONFIG_VERSION);
        assert_eq!(migrated.retry_policy, RetryPolicy::default());
        assert!(
            serde_json::to_value(migrated)
                .unwrap()
                .get("retryIntervalSeconds")
                .is_none()
        );
    }

    #[test]
    fn retired_backoff_settings_are_ignored_and_not_written_back() {
        let migrated = migrate_config(json!({
            "version": CONFIG_VERSION,
            "prompt": "real task",
            "retryPolicy": {
                "baseDelaySecs": 90,
                "multiplier": 1.7,
                "maxDelaySecs": 600,
                "jitterRatio": 0.15,
                "highDemandDelaySecs": 10,
                "maxEmptyReplies": 4
            }
        }))
        .unwrap();

        assert_eq!(migrated.retry_policy.max_empty_replies, 4);
        let serialized = serde_json::to_value(migrated).unwrap();
        let policy = serialized.get("retryPolicy").unwrap();
        for retired in [
            "baseDelaySecs",
            "multiplier",
            "maxDelaySecs",
            "jitterRatio",
            "highDemandDelaySecs",
        ] {
            assert!(policy.get(retired).is_none(), "{retired} was written back");
        }
    }

    #[test]
    fn invalid_version_type_is_rejected_instead_of_treated_as_v0() {
        assert!(matches!(
            migrate_config(json!({"version": "2", "prompt": "task"})),
            Err(ConfigError::InvalidVersion(_))
        ));
    }

    #[test]
    fn damaged_config_is_quarantined_and_defaults_are_loaded() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::in_directory(directory.path().to_path_buf());
        fs::write(directory.path().join("config.json"), b"{not-json").unwrap();

        let config = store.load_config().unwrap();

        assert_eq!(config, QueueConfig::default());
        assert!(!directory.path().join("config.json").exists());
        assert!(
            fs::read_dir(directory.path())
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config.json.corrupt-"))
        );
    }

    #[test]
    fn migrates_v1_and_discards_the_legacy_window_visibility_setting() {
        let migrated = migrate_config(json!({
            "version": 1,
            "prompt": "real task",
            "hideAppServerWindow": false,
            "failurePhrases": ["Server overloaded; retry later."],
            "retryPolicy": {"baseDelaySecs": 90}
        }))
        .unwrap();

        assert_eq!(migrated.version, CONFIG_VERSION);
        assert_eq!(migrated.retry_policy.max_empty_replies, 5);
        assert!(!migrated.full_screen_flash_enabled);
        assert!(migrated.audio_alert_enabled);
        assert!(
            migrated
                .failure_phrases
                .iter()
                .any(|phrase| phrase == CURRENT_HIGH_DEMAND_PHRASE)
        );
        assert!(
            serde_json::to_value(migrated)
                .unwrap()
                .get("hideAppServerWindow")
                .is_none()
        );
    }

    #[test]
    fn migration_preserves_explicitly_disabled_completion_alerts() {
        let migrated = migrate_config(json!({
            "version": CONFIG_VERSION,
            "prompt": "real task",
            "fullScreenFlashEnabled": false,
            "audioAlertEnabled": false
        }))
        .unwrap();

        assert!(!migrated.full_screen_flash_enabled);
        assert!(!migrated.audio_alert_enabled);
    }

    #[test]
    fn migration_removes_legacy_attempt_limits() {
        let migrated = migrate_config(json!({
            "version": CONFIG_VERSION,
            "prompt": "real task",
            "retryPolicy": {
                "maxAttemptsPerHour": 8,
                "maxAttemptsPerDay": 50
            }
        }))
        .unwrap();
        let serialized = serde_json::to_value(migrated).unwrap();
        let policy = serialized.get("retryPolicy").unwrap();

        assert!(policy.get("maxAttemptsPerHour").is_none());
        assert!(policy.get("maxAttemptsPerDay").is_none());
    }

    #[test]
    fn migration_removes_the_retired_gui_fallback_setting() {
        let migrated = migrate_config(json!({
            "version": CONFIG_VERSION,
            "prompt": "real task",
            "guiFallbackEnabled": true
        }))
        .unwrap();

        assert!(
            serde_json::to_value(migrated)
                .unwrap()
                .get("guiFallbackEnabled")
                .is_none()
        );
    }

    #[test]
    fn old_config_defaults_to_one_concurrent_session() {
        let migrated = migrate_config(json!({
            "version": 2,
            "prompt": "real task"
        }))
        .unwrap();

        assert_eq!(migrated.version, CONFIG_VERSION);
        assert_eq!(migrated.max_concurrent_sessions, MIN_CONCURRENT_SESSIONS);
    }

    #[test]
    fn existing_configs_adopt_the_official_identity_and_keep_their_folder() {
        let migrated = migrate_config(json!({
            "version": CONFIG_VERSION,
            "prompt": "real task",
            "workingDirectory": "C:/work"
        }))
        .unwrap();

        assert_eq!(migrated.client_identity, ClientIdentity::OfficialDesktop);
        assert_eq!(
            migrated.default_working_directory.as_deref(),
            Some(Path::new("C:/work"))
        );
        assert_eq!(
            serde_json::to_value(&migrated).unwrap()["clientIdentity"],
            "officialDesktop"
        );
    }

    #[test]
    fn empty_folder_paths_load_as_no_folder() {
        let migrated = migrate_config(json!({
            "version": CONFIG_VERSION,
            "workingDirectory": "",
            "defaultWorkingDirectory": "  "
        }))
        .unwrap();

        assert!(migrated.working_directory.is_none());
        assert!(migrated.default_working_directory.is_none());
    }

    #[test]
    fn identity_round_trips_and_names_the_upstream_originator() {
        let config = QueueConfig {
            client_identity: ClientIdentity::CodexCli,
            ..QueueConfig::default()
        };
        let restored: QueueConfig =
            serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();

        assert_eq!(restored.client_identity, ClientIdentity::CodexCli);
        assert_eq!(
            ClientIdentity::OfficialDesktop.originator(),
            "Codex Desktop"
        );
        assert_eq!(ClientIdentity::CodexCli.originator(), "codex_cli_rs");
    }

    #[test]
    fn concurrent_session_count_is_bounded() {
        let mut config = QueueConfig {
            max_concurrent_sessions: 0,
            ..QueueConfig::default()
        };
        assert!(matches!(
            config.validate_settings(),
            Err(ConfigError::ConcurrentSessionsOutOfRange { .. })
        ));
        config.max_concurrent_sessions = MAX_CONCURRENT_SESSIONS + 1;
        assert!(matches!(
            config.validate_settings(),
            Err(ConfigError::ConcurrentSessionsOutOfRange { .. })
        ));
    }

    #[test]
    fn scheduler_state_round_trips_separately_from_session_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::in_directory(directory.path().to_path_buf());
        let state = PersistedSchedulerState {
            selected_session_id: Some(3),
            next_session_id: 4,
            pending_tasks: vec![PersistedPendingTask {
                config: QueueConfig {
                    prompt: "queued task".into(),
                    ..QueueConfig::default()
                },
                ..PersistedPendingTask::default()
            }],
            ..PersistedSchedulerState::default()
        };

        store.save_scheduler_state(&state).unwrap();

        assert_eq!(store.load_scheduler_state().unwrap(), state);
        assert!(directory.path().join("scheduler.json").exists());
        assert!(!directory.path().join("state.json").exists());
    }

    #[test]
    fn v1_scheduler_messages_migrate_to_full_task_configs() {
        let migrated = migrate_scheduler_state(json!({
            "version": 1,
            "selectedSessionId": 2,
            "nextSessionId": 3,
            "pendingMessages": [{
                "prompt": "queued task",
                "workingDirectory": "C:/workspace",
                "failurePhrases": ["busy"]
            }]
        }))
        .unwrap();

        assert_eq!(migrated.version, SCHEDULER_STATE_VERSION);
        assert_eq!(migrated.pending_tasks.len(), 1);
        let config = &migrated.pending_tasks[0].config;
        assert_eq!(config.prompt, "queued task");
        assert_eq!(
            config.working_directory.as_deref(),
            Some(Path::new("C:/workspace"))
        );
        assert_eq!(config.failure_phrases, ["busy"]);
    }

    #[test]
    fn persists_config_and_runtime_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::in_directory(directory.path().to_path_buf());
        let config = QueueConfig {
            prompt: "meaningful task".into(),
            ..QueueConfig::default()
        };
        store.save_config(&config).unwrap();
        assert_eq!(store.load_config().unwrap(), config);
        let updated = QueueConfig {
            prompt: "updated meaningful task".into(),
            ..config.clone()
        };
        store.save_config(&updated).unwrap();
        assert_eq!(store.load_config().unwrap(), updated);

        let state = PersistedQueueState {
            active_thread_id: Some("thr_123".into()),
            ..PersistedQueueState::default()
        };
        store.save_state(&state).unwrap();
        assert_eq!(store.load_state().unwrap(), state);
        assert!(
            !fs::read_to_string(store.directory().join("state.json"))
                .unwrap()
                .contains("meaningful task")
        );
    }

    #[test]
    fn local_conversation_history_is_not_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::in_directory(directory.path().to_path_buf());
        let state = PersistedQueueState {
            phase: QueuePhase::Waiting,
            active_thread_id: Some("thread".into()),
            conversation: vec![ConversationMessage {
                role: crate::queue::ConversationRole::User,
                content: "temporary local history".into(),
                automatic: false,
            }],
            ..PersistedQueueState::default()
        };

        store.save_state(&state).unwrap();

        let document = fs::read_to_string(store.directory().join("state.json")).unwrap();
        assert!(!document.contains("temporary local history"));
        assert_eq!(
            store.load_state().unwrap().conversation,
            Vec::<ConversationMessage>::new()
        );
    }
}
