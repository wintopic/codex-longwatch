#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::sync::Arc;

use anyhow::{Context, Result};
use codex_longwatch::{
    app_server::AppServerTransport,
    config::ConfigStore,
    diagnostics,
    runtime::spawn_runtime,
    transport::{GuiFallbackTransport, PreferredTransport},
    ui,
};
use single_instance::SingleInstance;
use tracing::{info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

fn main() {
    install_panic_hook();
    if let Err(error) = run() {
        gpui_platform::show_error_dialog("Longwatch 启动失败", &format!("{error:#}"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let store = ConfigStore::discover()?;
    diagnostics::configure_report_directory(store.directory());
    std::fs::create_dir_all(store.directory())
        .with_context(|| format!("无法创建配置目录 {}", store.directory().display()))?;
    let _log_guard = init_logging(&store)?;
    if let Err(error) = gpui_platform::initialize_app_identity() {
        warn!(%error, "注册桌面应用身份失败，将继续启动");
    }
    info!(
        version = env!("CARGO_PKG_VERSION"),
        config_dir = %store.directory().display(),
        "Longwatch 正在启动"
    );
    let lock_name = single_instance_name(&store);
    let instance = SingleInstance::new(&lock_name).context("无法创建单实例锁")?;
    if !instance.is_single() {
        #[cfg(target_os = "windows")]
        if gpui_platform::request_existing_instance_show() {
            info!("已唤醒正在运行的 Longwatch 窗口");
            return Ok(());
        }
        gpui_platform::show_error_dialog(
            "Longwatch 已在运行",
            "无法自动唤醒现有窗口，请从任务栏或系统托盘打开。",
        );
        return Ok(());
    }

    let config = store.load_config()?;
    let state = store.load_state()?;
    let transport = PreferredTransport::new(
        AppServerTransport::new(config.codex_path.clone()).with_hidden_window(true),
        GuiFallbackTransport::new(config.gui_fallback_enabled),
        config.gui_fallback_enabled,
    );
    let tokio_runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("longwatch-runtime")
            .build()
            .context("无法创建后台运行时")?,
    );
    let runtime = tokio_runtime
        .block_on(async { spawn_runtime(transport, config.clone(), state, Some(store.clone())) });
    ui::run(config, runtime, Arc::clone(&tokio_runtime));
    info!("Longwatch 已正常退出");
    drop(instance);
    Ok(())
}

fn init_logging(store: &ConfigStore) -> Result<WorkerGuard> {
    let log_directory = store.directory().join("logs");
    std::fs::create_dir_all(&log_directory)
        .with_context(|| format!("无法创建日志目录 {}", log_directory.display()))?;
    let appender = tracing_appender::rolling::daily(log_directory, "longwatch.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .compact()
        .try_init()
        .map_err(|error| anyhow::anyhow!("无法初始化文件日志：{error}"))?;
    Ok(guard)
}

#[cfg(target_os = "windows")]
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|panic| {
        let panic_message = panic.to_string();
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let report_path = diagnostics::write_crash_report(&panic_message, &backtrace).ok();
        tracing::error!(panic = %panic, "Longwatch 发生未捕获 panic");
        let message = report_path.map_or_else(
            || format!("{panic_message}\n\n崩溃报告写入失败，请查看日志目录。"),
            |path| {
                format!(
                    "{panic_message}\n\n已保存完整调用栈与最近日志：\n{}",
                    path.display()
                )
            },
        );
        gpui_platform::show_error_dialog("Longwatch 意外退出", &message);
    }));
}

#[cfg(not(target_os = "windows"))]
fn install_panic_hook() {}

fn single_instance_name(store: &ConfigStore) -> String {
    #[cfg(target_os = "macos")]
    {
        store.lock_path().to_string_lossy().into_owned()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = store;
        "Longwatch-for-Codex-98b44998-f31f-45f1-9a36-bf78c7370f87".into()
    }
}
