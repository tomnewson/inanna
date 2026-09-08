use crate::{
    config::{self, AppConfig},
    media::{self, MediaError},
    model::{ActiveToolset, DownloadRequest},
    platform::{self, ToolPlatform},
    tools::{ToolError, ToolManager, UpdatePlan},
};
use directories::UserDirs;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub type EventSink = Arc<dyn Fn(EngineEvent) + Send + Sync>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineEvent {
    pub operation_id: String,
    pub event: String,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub backend_version: String,
    pub platform: String,
    pub output_folder: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCheckResult {
    pub state: String,
    pub tools_ready: bool,
    pub can_install_tools: bool,
    pub update_summary: String,
    pub status_text: String,
}

#[derive(Clone)]
pub struct OperationLease {
    pub id: String,
    pub token: CancellationToken,
}

pub struct AppEngine {
    manager: Option<ToolManager>,
    unsupported_platform: Option<String>,
    config_path: PathBuf,
    config: Mutex<AppConfig>,
    pending_update: AsyncMutex<Option<UpdatePlan>>,
    active_tools: RwLock<Option<ActiveToolset>>,
    operation: Mutex<Option<OperationLease>>,
}

impl AppEngine {
    pub fn new(data_root: PathBuf) -> Result<Self, BackendError> {
        validate_data_root(&data_root)?;
        let config_path = data_root.join("config.json");
        let app_config = match config::read_json::<AppConfig>(&config_path) {
            Ok(config) => config.unwrap_or_default(),
            Err(error) => {
                tracing::warn!(%error, "could not read settings; using defaults");
                AppConfig::default()
            }
        };
        let (manager, unsupported_platform) = match ToolPlatform::current() {
            Ok(platform) => (Some(ToolManager::new(data_root, platform)?), None),
            Err(platform::PlatformError::Unsupported(id)) => (None, Some(id)),
        };
        Ok(Self {
            manager,
            unsupported_platform,
            config_path,
            config: Mutex::new(app_config),
            pending_update: AsyncMutex::new(None),
            active_tools: RwLock::new(None),
            operation: Mutex::new(None),
        })
    }

    pub fn initialize(&self) -> Result<InitializeResult, BackendError> {
        self.manager()?;
        let configured = self
            .config
            .lock()
            .expect("config mutex poisoned")
            .output_directory
            .clone();
        let output = configured
            .or_else(default_download_directory)
            .unwrap_or_else(|| {
                self.config_path
                    .parent()
                    .unwrap_or(Path::new("."))
                    .to_path_buf()
            });
        Ok(InitializeResult {
            backend_version: crate::application_version().into(),
            platform: ToolPlatform::current()
                .map(|value| value.id.to_owned())
                .unwrap_or_else(|_| platform::current_platform_id()),
            output_folder: output.to_string_lossy().into_owned(),
        })
    }

    pub fn set_output_folder(&self, path: PathBuf) -> Result<(), BackendError> {
        if path.as_os_str().is_empty() {
            return Err(BackendError::InvalidRequest(
                "the output folder cannot be empty".into(),
            ));
        }
        let mut saved = self.config.lock().expect("config mutex poisoned");
        saved.output_directory = Some(path);
        config::write_json_atomic(&self.config_path, &*saved)?;
        Ok(())
    }

    pub fn reserve_operation(&self) -> Result<OperationLease, BackendError> {
        let mut current = self.operation.lock().expect("operation mutex poisoned");
        if current.is_some() {
            return Err(BackendError::Busy);
        }
        let lease = OperationLease {
            id: Uuid::new_v4().to_string(),
            token: CancellationToken::new(),
        };
        *current = Some(lease.clone());
        Ok(lease)
    }

    pub async fn check_tools(
        &self,
        lease: &OperationLease,
    ) -> Result<ToolCheckResult, BackendError> {
        let result = async {
            let manager = self.manager()?;
            let cached = manager.load_active()?;
            *self.active_tools.write().await = cached.clone();
            let checked = manager.check_updates(cached.as_ref()).await;
            match checked {
                Ok(Some(plan)) if plan.has_updates() => {
                    let first_setup = cached.is_none();
                    let summary = plan.summary();
                    *self.pending_update.lock().await = Some(plan);
                    Ok(ToolCheckResult {
                        state: if first_setup {
                            "setupRequired"
                        } else {
                            "updateAvailable"
                        }
                        .into(),
                        tools_ready: !first_setup,
                        can_install_tools: true,
                        update_summary: summary,
                        status_text: if first_setup {
                            String::new()
                        } else {
                            "Updates are available.".into()
                        },
                    })
                }
                Ok(_) => {
                    *self.pending_update.lock().await = None;
                    Ok(ToolCheckResult {
                        state: "ready".into(),
                        tools_ready: true,
                        can_install_tools: false,
                        update_summary: String::new(),
                        status_text: "Ready.".into(),
                    })
                }
                Err(error) if cached.is_some() => {
                    tracing::warn!(%error, "update check failed; using cached tools");
                    Ok(ToolCheckResult {
                        state: "cachedWithWarning".into(),
                        tools_ready: true,
                        can_install_tools: false,
                        update_summary: String::new(),
                        status_text: "Could not check for updates. Cached tools are ready.".into(),
                    })
                }
                Err(error) => Err(BackendError::ToolCheck(error.to_string())),
            }
        }
        .await;
        self.finish_operation(&lease.id);
        result
    }

    pub async fn install_tools(self: Arc<Self>, lease: OperationLease, events: EventSink) {
        let result = self.install_tools_inner(&lease, &events).await;
        match result {
            Ok(active) => emit(
                &events,
                &lease.id,
                "operationCompleted",
                json!({ "operationKind": "toolInstall", "platform": active.platform }),
            ),
            Err(BackendError::Cancelled) => emit(
                &events,
                &lease.id,
                "operationCancelled",
                json!({ "operationKind": "toolInstall" }),
            ),
            Err(error) => emit_failure(&events, &lease.id, "toolInstall", &error),
        }
        self.finish_operation(&lease.id);
    }

    async fn install_tools_inner(
        &self,
        lease: &OperationLease,
        events: &EventSink,
    ) -> Result<ActiveToolset, BackendError> {
        let manager = self.manager()?;
        let plan = self
            .pending_update
            .lock()
            .await
            .clone()
            .ok_or(BackendError::NoUpdatePlan)?;
        let active = self.active_tools.read().await.clone();
        let event_sink = Arc::clone(events);
        let operation_id = lease.id.clone();
        let progress = Arc::new(move |message: String| {
            emit(
                &event_sink,
                &operation_id,
                "operationProgress",
                json!({ "operationKind": "toolInstall", "message": message }),
            );
        });
        match manager
            .install(&plan, active.as_ref(), progress, lease.token.clone())
            .await
        {
            Ok(installed) => {
                *self.active_tools.write().await = Some(installed.clone());
                *self.pending_update.lock().await = None;
                Ok(installed)
            }
            Err(ToolError::Cancelled) => Err(BackendError::Cancelled),
            Err(error) => Err(error.into()),
        }
    }

    pub fn validate_download(
        &self,
        mut parameters: DownloadRequest,
    ) -> Result<DownloadRequest, BackendError> {
        parameters.url = normalize_download_url(&parameters.url)?;
        if parameters.output_directory.as_os_str().is_empty() {
            return Err(BackendError::InvalidRequest(
                "the output folder cannot be empty".into(),
            ));
        }
        Ok(parameters)
    }

    pub async fn download(
        self: Arc<Self>,
        lease: OperationLease,
        parameters: DownloadRequest,
        events: EventSink,
    ) {
        let result = self.download_inner(&lease, parameters, &events).await;
        match result {
            Ok(path) => emit(
                &events,
                &lease.id,
                "operationCompleted",
                json!({
                    "operationKind": "download",
                    "path": path.to_string_lossy()
                }),
            ),
            Err(BackendError::Cancelled) => emit(
                &events,
                &lease.id,
                "operationCancelled",
                json!({ "operationKind": "download" }),
            ),
            Err(error) => emit_failure(&events, &lease.id, "download", &error),
        }
        self.finish_operation(&lease.id);
    }

    async fn download_inner(
        &self,
        lease: &OperationLease,
        parameters: DownloadRequest,
        events: &EventSink,
    ) -> Result<PathBuf, BackendError> {
        let tools = self
            .active_tools
            .read()
            .await
            .clone()
            .ok_or(BackendError::ToolsUnavailable)?;
        let event_sink = Arc::clone(events);
        let operation_id = lease.id.clone();
        let progress = Arc::new(move |update: crate::model::ProgressUpdate| {
            emit(
                &event_sink,
                &operation_id,
                "operationProgress",
                json!({
                    "operationKind": "download",
                    "phase": update.phase,
                    "fraction": update.fraction,
                    "downloadedBytes": update.downloaded_bytes,
                    "totalBytes": update.total_bytes,
                    "speedBytesPerSecond": update.speed_bytes_per_second,
                    "message": media::describe_progress(&update)
                }),
            );
        });
        match media::download_media(tools, parameters, lease.token.clone(), progress).await {
            Ok(path) => Ok(path),
            Err(MediaError::Cancelled) => Err(BackendError::Cancelled),
            Err(error) => Err(error.into()),
        }
    }

    pub fn cancel(&self, operation_id: &str) -> bool {
        let current = self.operation.lock().expect("operation mutex poisoned");
        if let Some(active) = current.as_ref()
            && active.id == operation_id
        {
            active.token.cancel();
            return true;
        }
        false
    }

    pub fn cancel_active(&self) {
        if let Some(active) = self
            .operation
            .lock()
            .expect("operation mutex poisoned")
            .as_ref()
        {
            active.token.cancel();
        }
    }

    fn finish_operation(&self, operation_id: &str) {
        let mut current = self.operation.lock().expect("operation mutex poisoned");
        if current
            .as_ref()
            .is_some_and(|value| value.id == operation_id)
        {
            *current = None;
        }
    }

    fn manager(&self) -> Result<&ToolManager, BackendError> {
        self.manager.as_ref().ok_or_else(|| {
            BackendError::UnsupportedPlatform(
                self.unsupported_platform
                    .clone()
                    .unwrap_or_else(platform::current_platform_id),
            )
        })
    }
}

fn normalize_download_url(value: &str) -> Result<String, BackendError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(BackendError::InvalidUrl);
    }

    let candidate = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    let valid = url::Url::parse(&candidate)
        .map(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
        .unwrap_or(false);
    valid.then_some(candidate).ok_or(BackendError::InvalidUrl)
}

fn emit(events: &EventSink, operation_id: &str, event: &str, data: Value) {
    events(EngineEvent {
        operation_id: operation_id.into(),
        event: event.into(),
        data,
    });
}

fn emit_failure(events: &EventSink, operation_id: &str, kind: &str, error: &BackendError) {
    emit(
        events,
        operation_id,
        "operationFailed",
        json!({
            "operationKind": kind,
            "error": error.payload()
        }),
    );
}

fn validate_data_root(root: &Path) -> Result<(), BackendError> {
    std::fs::create_dir_all(root)?;
    let probe = root.join(format!(".write-test-{}", Uuid::new_v4()));
    std::fs::write(&probe, b"write test")?;
    std::fs::remove_file(probe)?;
    Ok(())
}

fn default_download_directory() -> Option<PathBuf> {
    UserDirs::new().and_then(|directories| directories.download_dir().map(Path::to_path_buf))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
    pub details: Option<String>,
}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("platform {0} is not supported by this release")]
    UnsupportedPlatform(String),
    #[error("another operation is already running")]
    Busy,
    #[error("Enter a valid URL.")]
    InvalidUrl,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("required tools are not available")]
    ToolsUnavailable,
    #[error("no tool update information is available; run the tool check again")]
    NoUpdatePlan,
    #[error("the operation was cancelled")]
    Cancelled,
    #[error("could not check required tools: {0}")]
    ToolCheck(String),
    #[error("file operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("tool operation failed: {0}")]
    Tool(#[from] ToolError),
    #[error("download failed: {0}")]
    Media(#[from] MediaError),
}

impl BackendError {
    pub fn payload(&self) -> ErrorPayload {
        let (code, message, details) = match self {
            Self::UnsupportedPlatform(id) => (
                "unsupportedPlatform",
                "This build does not support the current platform.".into(),
                Some(format!("Unsupported platform: {id}")),
            ),
            Self::Busy => ("busy", self.to_string(), None),
            Self::InvalidUrl => ("invalidUrl", self.to_string(), None),
            Self::InvalidRequest(_) => ("invalidRequest", self.to_string(), None),
            Self::ToolsUnavailable => ("toolsUnavailable", self.to_string(), None),
            Self::NoUpdatePlan => ("noUpdatePlan", self.to_string(), None),
            Self::Cancelled => ("cancelled", self.to_string(), None),
            Self::ToolCheck(details) => (
                "toolCheckFailed",
                "Could not check required tools.".into(),
                Some(details.clone()),
            ),
            Self::Io(error) => (
                "fileOperationFailed",
                "A file operation failed.".into(),
                Some(error.to_string()),
            ),
            Self::Tool(error) => (
                "toolOperationFailed",
                "The tool operation failed.".into(),
                Some(error.to_string()),
            ),
            Self::Media(error) => (
                "downloadFailed",
                "The download failed.".into(),
                Some(error.to_string()),
            ),
        };
        ErrorPayload {
            code: code.into(),
            message,
            details,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_download_url;

    #[test]
    fn adds_https_to_scheme_less_url() {
        assert_eq!(
            normalize_download_url("youtu.be/dLpDMOboh0o").unwrap(),
            "https://youtu.be/dLpDMOboh0o"
        );
    }

    #[test]
    fn preserves_supported_scheme_and_trims_whitespace() {
        assert_eq!(
            normalize_download_url("  http://example.com/video  ").unwrap(),
            "http://example.com/video"
        );
    }

    #[test]
    fn rejects_unsupported_or_malformed_url() {
        assert!(normalize_download_url("ftp://example.com/video").is_err());
        assert!(normalize_download_url("not a url").is_err());
    }
}
