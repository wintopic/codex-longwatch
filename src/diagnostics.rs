//! Privacy-conscious diagnostics and crash-report support.

use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime},
};

use chrono::Local;

use crate::queue::QueueSnapshot;

const VERSION_TIMEOUT: Duration = Duration::from_secs(3);
const LOG_TAIL_BYTES: usize = 48 * 1024;
const REPLY_PREVIEW_CHARS: usize = 2_000;

static REPORT_DIRECTORY: OnceLock<Mutex<PathBuf>> = OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexCliInfo {
    pub requested_path: PathBuf,
    pub resolved_path: Option<PathBuf>,
    pub version: Option<String>,
    pub error: Option<String>,
}

impl CodexCliInfo {
    #[must_use]
    pub fn available(&self) -> bool {
        self.resolved_path.is_some() && self.version.is_some()
    }

    #[must_use]
    pub fn path_label(&self) -> String {
        self.resolved_path.as_ref().map_or_else(
            || self.requested_path.display().to_string(),
            |path| path.display().to_string(),
        )
    }

    #[must_use]
    pub fn version_label(&self) -> &str {
        self.version.as_deref().unwrap_or("版本读取失败")
    }
}

/// Resolve the configured executable and read its real `--version` output.
#[must_use]
pub fn inspect_codex_cli(requested_path: &Path) -> CodexCliInfo {
    let resolved_path = match which::which(requested_path) {
        Ok(path) => path,
        Err(error) => {
            return CodexCliInfo {
                requested_path: requested_path.to_path_buf(),
                resolved_path: None,
                version: None,
                error: Some(error.to_string()),
            };
        }
    };
    match command_version(&resolved_path) {
        Ok(version) => CodexCliInfo {
            requested_path: requested_path.to_path_buf(),
            resolved_path: Some(resolved_path),
            version: Some(version),
            error: None,
        },
        Err(error) => CodexCliInfo {
            requested_path: requested_path.to_path_buf(),
            resolved_path: Some(resolved_path),
            version: None,
            error: Some(error.to_string()),
        },
    }
}

fn command_version(executable: &Path) -> io::Result<String> {
    let mut command = Command::new(executable);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + VERSION_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "读取 Codex CLI 版本超时",
            ));
        }
        thread::sleep(Duration::from_millis(40));
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout)?;
    }
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr)?;
    }
    let version = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if !status.success() || version.is_empty() {
        return Err(io::Error::other(if version.is_empty() {
            format!("Codex CLI 未返回版本信息（{status}）")
        } else {
            format!("Codex CLI 版本命令失败（{status}）：{version}")
        }));
    }
    Ok(version.to_owned())
}

/// Set the directory used by the global panic hook.
pub fn configure_report_directory(config_directory: &Path) {
    let mut directory = REPORT_DIRECTORY
        .get_or_init(|| Mutex::new(default_report_directory()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *directory = config_directory.join("crash-reports");
}

/// Build a report that can be copied from the UI without including the task
/// prompt, authentication data, environment variables, or configuration file.
#[must_use]
pub fn build_support_report(
    config_directory: &Path,
    cli: &CodexCliInfo,
    snapshot: &QueueSnapshot,
) -> String {
    let transport = &snapshot.transport_status;
    let connection = if transport.connected {
        "已连接"
    } else {
        "未连接"
    };
    let server_agent = transport.server_agent.as_deref().unwrap_or("未提供");
    let thread_id = snapshot.active_thread_id.as_deref().unwrap_or("无");
    let turn_id = snapshot.active_turn_id.as_deref().unwrap_or("无");
    let cli_error = cli.error.as_deref().unwrap_or("无");
    let reply = truncate_chars(&snapshot.reply_preview, REPLY_PREVIEW_CHARS);
    let logs = recent_log_tail(&config_directory.join("logs"))
        .unwrap_or_else(|error| format!("读取最近日志失败：{error}"));

    format!(
        "Longwatch for Codex 诊断报告\n\
         生成时间：{}\n\
         应用版本：{}\n\
         系统：{}\n\
         配置目录：{}\n\
         Codex 配置路径：{}\n\
         Codex 解析路径：{}\n\
         Codex CLI：{}\n\
         Codex CLI 检测错误：{}\n\
         传输通道：{}\n\
         连接状态：{}\n\
         app-server：{}\n\
         队列阶段：{}\n\
         累计尝试：{}\n\
         连续重试：{}\n\
         线程 ID：{}\n\
         回合 ID：{}\n\
         当前状态：{}\n\
         最近回复或错误：{}\n\n\
         最近日志（最多 48 KiB）：\n{}\n\n\
         隐私说明：本报告不包含任务原文、配置文件、环境变量、API Key 或登录凭据。",
        Local::now().to_rfc3339(),
        env!("CARGO_PKG_VERSION"),
        gpui_platform::system_summary(),
        config_directory.display(),
        cli.requested_path.display(),
        cli.path_label(),
        cli.version_label(),
        cli_error,
        transport.kind.label(),
        connection,
        server_agent,
        snapshot.phase.label(),
        snapshot.attempt_count,
        snapshot.consecutive_retries,
        thread_id,
        turn_id,
        snapshot.status_message,
        if reply.is_empty() { "无" } else { &reply },
        logs,
    )
}

/// Persist a panic report with a forced backtrace and recent logs.
pub fn write_crash_report(panic_message: &str, backtrace: &str) -> io::Result<PathBuf> {
    let directory = REPORT_DIRECTORY
        .get_or_init(|| Mutex::new(default_report_directory()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!(
        "crash-{}.txt",
        Local::now().format("%Y%m%d-%H%M%S-%3f")
    ));
    let config_directory = directory.parent().unwrap_or(&directory);
    let logs = recent_log_tail(&config_directory.join("logs"))
        .unwrap_or_else(|error| format!("读取最近日志失败：{error}"));
    let report = format!(
        "Longwatch for Codex 崩溃报告\n\
         发生时间：{}\n\
         应用版本：{}\n\
         系统：{}\n\
         异常：{}\n\n\
         调用栈：\n{}\n\n\
         最近日志（最多 48 KiB）：\n{}\n",
        Local::now().to_rfc3339(),
        env!("CARGO_PKG_VERSION"),
        gpui_platform::system_summary(),
        panic_message,
        backtrace,
        logs,
    );
    fs::write(&path, report)?;
    Ok(path)
}

fn default_report_directory() -> PathBuf {
    std::env::temp_dir().join("Longwatch").join("crash-reports")
}

fn recent_log_tail(log_directory: &Path) -> io::Result<String> {
    let latest = fs::read_dir(log_directory)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .max_by_key(|entry| {
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        })
        .map(|entry| entry.path())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "日志目录中没有文件"))?;
    let bytes = fs::read(latest)?;
    let start = bytes.len().saturating_sub(LOG_TAIL_BYTES);
    Ok(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut truncated = value.chars().take(limit).collect::<String>();
    truncated.push_str("…（已截断）");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::TransportKind;

    #[test]
    fn support_report_contains_live_transport_but_no_prompt_field() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("logs")).unwrap();
        fs::write(directory.path().join("logs/latest.log"), "recent log").unwrap();
        let cli = CodexCliInfo {
            requested_path: PathBuf::from("codex"),
            resolved_path: Some(PathBuf::from("C:/codex.cmd")),
            version: Some("codex-cli 1.2.3".into()),
            error: None,
        };
        let mut snapshot = QueueSnapshot::default();
        snapshot.transport_status.kind = TransportKind::AppServer;
        snapshot.transport_status.connected = true;
        snapshot.transport_status.server_agent = Some("codex_cli_rs/1.2.3".into());

        let report = build_support_report(directory.path(), &cli, &snapshot);

        assert!(report.contains("codex-cli 1.2.3"));
        assert!(report.contains("Codex app-server"));
        assert!(report.contains("recent log"));
        assert!(!report.contains("任务原文："));
    }

    #[test]
    fn truncates_large_reply_previews() {
        let value = "字".repeat(REPLY_PREVIEW_CHARS + 10);
        let truncated = truncate_chars(&value, REPLY_PREVIEW_CHARS);
        assert!(truncated.ends_with("…（已截断）"));
        assert!(truncated.chars().count() < value.chars().count());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn reads_version_from_a_windows_command_shim() {
        let directory = tempfile::tempdir().unwrap();
        let shim = directory.path().join("codex.cmd");
        fs::write(&shim, "@echo off\r\necho codex-cli 9.9.9\r\n").unwrap();

        assert_eq!(command_version(&shim).unwrap(), "codex-cli 9.9.9");
    }
}
