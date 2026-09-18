use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadMode {
    Video,
    AudioOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoQuality {
    P1080,
    P1440,
    Best,
}

impl VideoQuality {
    pub fn maximum_dimension(self) -> Option<u32> {
        match self {
            Self::P1080 => Some(1080),
            Self::P1440 => Some(1440),
            Self::Best => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub url: String,
    pub mode: DownloadMode,
    pub video_quality: VideoQuality,
    pub output_directory: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JobPhase {
    Preparing,
    Retrying,
    Downloading,
    Inspecting,
    Remuxing,
    Converting,
    Finalizing,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub phase: JobPhase,
    pub fraction: Option<f32>,
    pub downloaded_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub speed_bytes_per_second: Option<u64>,
    pub message: String,
}

impl ProgressUpdate {
    pub fn message(phase: JobPhase, message: impl Into<String>) -> Self {
        Self {
            phase,
            fraction: None,
            downloaded_bytes: None,
            total_bytes: None,
            speed_bytes_per_second: None,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveToolset {
    pub id: String,
    pub yt_dlp_version: String,
    pub ffmpeg_version: String,
    pub deno_version: String,
    pub directory: PathBuf,
    pub platform: String,
    pub yt_dlp_path: PathBuf,
    pub ffmpeg_path: PathBuf,
    pub ffprobe_path: PathBuf,
    pub deno_path: PathBuf,
}

impl ActiveToolset {
    pub fn yt_dlp(&self) -> PathBuf {
        self.directory.join(&self.yt_dlp_path)
    }

    pub fn ffmpeg(&self) -> PathBuf {
        self.directory.join(&self.ffmpeg_path)
    }

    pub fn ffprobe(&self) -> PathBuf {
        self.directory.join(&self.ffprobe_path)
    }

    pub fn deno(&self) -> PathBuf {
        self.directory.join(&self.deno_path)
    }
}
