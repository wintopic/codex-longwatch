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
    backoff::{ABSOLUTE_MIN_INTERVAL_SECS, AttemptLedger, RetryPolicy},
    queue::QueuePhase,
};

pub const CONFIG_VERSION: u32 = 2;
pub const STATE_VERSION: u32 = 1;
pub const CURRENT_HIGH_DEMAND_PHRASE: &str =
    "We're currently experiencing high demand, which may cause temporary errors.";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct QueueConfig {
    pub version: u32,
    pub prompt: String,
    pub working_directory: Option<PathBuf>,
    pub codex_path: PathBuf,
    pub failure_phrases: Vec<String>,
    pub gui_fallback_enabled: bool,
    pub full_screen_flash_enabled: bool,
    pub audio_alert_enabled: bool,
    pub retry_policy: RetryPolicy,
    pub turn_timeout_secs: u64,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            prompt: String::new(),
            working_directory: None,
            codex_path: PathBuf::from("codex"),
            failure_phrases: vec![
                "Server overloaded; retry later.".into(),
                "We're experiencing high demand. Please try again later.".into(),
                CURRENT_HIGH_DEMAND_PHRASE.into(),
            ],
            gui_fallback_enabled: false,
            full_screen_flash_enabled: true,
            audio_alert_enabled: true,
            retry_policy: RetryPolicy::default(),
            turn_timeout_secs: 30 * 60,
        }
    }
}

impl QueueConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.retry_policy
            .validate()
            .map_err(ConfigError::RetryPolicy)?;
        if self.prompt.trim().is_empty() {
            return Err(ConfigError::EmptyPrompt);
        }
        if self.turn_timeout_secs < 60 {
            return Err(ConfigError::TurnTimeoutTooShort);
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
    pub attempt_count: u64,
    pub consecutive_retries: u32,
    /// Monotonic counter used by the UI to show one red full-screen pulse for
    /// every retry-producing error without coupling the runtime to a window.
    #[serde(default)]
    pub retry_alert_count: u64,
    pub empty_reply_count: u8,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub reply_preview: String,
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
            attempt_count: 0,
            consecutive_retries: 0,
            retry_alert_count: 0,
            empty_reply_count: 0,
            next_attempt_at: None,
            reply_preview: String::new(),
            attempts: AttemptLedger::default(),
            prompt_digest: None,
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
            Ok(state) => Ok(state),
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
        atomic_write_json(&self.directory.join("state.json"), state)
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
        remove_legacy_attempt_limits(object);
    }
    match version {
        0 => {
            if let Value::Object(object) = &mut value {
                object.insert("version".into(), Value::from(CONFIG_VERSION));
                append_current_high_demand_phrase(object);
                if let Some(interval) = object.remove("retryIntervalSeconds") {
                    let policy = object
                        .entry("retryPolicy")
                        .or_insert_with(|| Value::Object(Default::default()));
                    if let (Value::Object(policy), Some(interval)) = (policy, interval.as_u64()) {
                        policy.insert(
                            "baseDelaySecs".into(),
                            Value::from(interval.max(ABSOLUTE_MIN_INTERVAL_SECS)),
                        );
                    }
                }
                migrate_default_retry_delay(object);
            }
            serde_json::from_value(value).map_err(ConfigError::Json)
        }
        1 => {
            if let Value::Object(object) = &mut value {
                object.insert("version".into(), Value::from(CONFIG_VERSION));
                append_current_high_demand_phrase(object);
                migrate_default_retry_delay(object);
            }
            serde_json::from_value(value).map_err(ConfigError::Json)
        }
        2 => serde_json::from_value(value).map_err(ConfigError::Json),
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

fn migrate_default_retry_delay(object: &mut serde_json::Map<String, Value>) {
    let Some(Value::Object(policy)) = object.get_mut("retryPolicy") else {
        return;
    };
    if policy.get("baseDelaySecs").and_then(Value::as_u64) == Some(90) {
        policy.insert("baseDelaySecs".into(), Value::from(30));
    }
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
    fn completion_alerts_are_enabled_by_default() {
        let config = QueueConfig::default();
        assert!(config.full_screen_flash_enabled);
        assert!(config.audio_alert_enabled);
    }

    #[test]
    fn migrates_legacy_interval_and_clamps_it() {
        let migrated = migrate_config(json!({
            "prompt": "real task",
            "retryIntervalSeconds": 12
        }))
        .unwrap();
        assert_eq!(migrated.version, CONFIG_VERSION);
        assert_eq!(
            migrated.retry_policy.base_delay_secs,
            ABSOLUTE_MIN_INTERVAL_SECS
        );
    }

    #[test]
    fn v0_default_ninety_second_delay_runs_through_the_full_migration_chain() {
        let migrated = migrate_config(json!({
            "prompt": "real task",
            "retryIntervalSeconds": 90
        }))
        .unwrap();

        assert_eq!(migrated.retry_policy.base_delay_secs, 30);
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
        assert_eq!(migrated.retry_policy.base_delay_secs, 30);
        assert_eq!(migrated.retry_policy.max_empty_replies, 5);
        assert!(migrated.full_screen_flash_enabled);
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
}
