use inanna::{engine::AppEngine, protocol};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing_appender::non_blocking::WorkerGuard;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data_root = parse_data_root()?;
    let _log_guard = initialize_logging(&data_root);
    let engine = Arc::new(AppEngine::new(data_root)?);
    protocol::run(engine).await?;
    Ok(())
}

fn parse_data_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--data-root" {
            return arguments
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| "--data-root requires a path".into());
        }
    }
    Err("the backend must be started with --data-root".into())
}

fn initialize_logging(root: &Path) -> Option<WorkerGuard> {
    let log_directory = root.join("logs");
    std::fs::create_dir_all(&log_directory).ok()?;
    let appender = tracing_appender::rolling::daily(log_directory, "inanna.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter("info")
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
    Some(guard)
}
