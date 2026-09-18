use crate::model::{
    ActiveToolset, DownloadMode, DownloadRequest, JobPhase, ProgressUpdate, VideoQuality,
};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    future::Future,
    io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
};
use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const PROGRESS_PREFIX: &str = "__YTDLP_WRAPPER_PROGRESS__";
const FILE_PREFIX: &str = "__YTDLP_WRAPPER_FILE__";

pub type MediaProgress = Arc<dyn Fn(ProgressUpdate) + Send + Sync>;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("file operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("media information is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the operation was cancelled")]
    Cancelled,
    #[error("yt-dlp failed: {0}")]
    YtDlp(String),
    #[error("automatic retries exhausted: {0}")]
    RetriesExhausted(Box<MediaError>),
    #[error("FFmpeg failed: {0}")]
    Ffmpeg(String),
    #[error("yt-dlp did not report the downloaded file")]
    MissingOutput,
    #[error("no suitable media format is available: {0}")]
    NoSuitableFormat(String),
    #[error("the downloaded file has no supported media stream")]
    MissingStream,
}

#[derive(Debug, Deserialize)]
struct MediaInfo {
    #[serde(default)]
    formats: Vec<AvailableFormat>,
}

#[derive(Debug, Clone, Deserialize)]
struct AvailableFormat {
    format_id: String,
    ext: Option<String>,
    vcodec: Option<String>,
    acodec: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    has_drm: Option<bool>,
}

#[derive(Debug, Clone)]
struct FormatSelection {
    format_spec: String,
    merge_container: Option<&'static str>,
    summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum H264Encoder {
    NvidiaNvenc,
    IntelQuickSync,
    AmdAmf,
    AppleVideoToolbox,
    CpuX264,
}

impl H264Encoder {
    fn gpu_candidates(platform: &str) -> &'static [Self] {
        match platform {
            crate::platform::WINDOWS_X64 => {
                &[Self::NvidiaNvenc, Self::IntelQuickSync, Self::AmdAmf]
            }
            crate::platform::MACOS_ARM64 => &[Self::AppleVideoToolbox],
            _ => &[],
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::NvidiaNvenc => "NVIDIA GPU",
            Self::IntelQuickSync => "Intel GPU",
            Self::AmdAmf => "AMD GPU",
            Self::AppleVideoToolbox => "Apple GPU",
            Self::CpuX264 => "CPU",
        }
    }

    fn is_gpu(self) -> bool {
        self != Self::CpuX264
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VideoBitrateProfile {
    target_kbps: u32,
    maximum_kbps: u32,
}

impl VideoBitrateProfile {
    fn buffer_kbps(self) -> u32 {
        self.maximum_kbps.saturating_mul(2)
    }
}

#[derive(Debug, Deserialize)]
struct ProbeResult {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: ProbeFormat,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    pix_fmt: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

pub async fn download_media(
    tools: ActiveToolset,
    request: DownloadRequest,
    cancel: CancellationToken,
    progress: MediaProgress,
) -> Result<PathBuf, MediaError> {
    (progress)(ProgressUpdate::message(
        JobPhase::Preparing,
        "Preparing download…",
    ));
    fs::create_dir_all(&request.output_directory).await?;
    let staging = request
        .output_directory
        .join(format!(".inanna-{}", Uuid::new_v4()));
    fs::create_dir_all(&staging).await?;

    let result = retry_download(&request.url, &cancel, &progress, || {
        run_pipeline(&tools, &request, &staging, &cancel, &progress)
    })
    .await;
    let _ = fs::remove_dir_all(&staging).await;
    match &result {
        Err(MediaError::Cancelled) => (progress)(ProgressUpdate::message(
            JobPhase::Cancelled,
            "Download cancelled.",
        )),
        Err(_) => (progress)(ProgressUpdate::message(
            JobPhase::Failed,
            "The download failed.",
        )),
        Ok(_) => {}
    }
    result
}

const MAX_DOWNLOAD_RETRIES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryReason {
    Blocked,
    RateLimited,
    Network,
}

fn retry_reason(url: &str, error: &MediaError) -> Option<RetryReason> {
    let MediaError::YtDlp(details) = error else {
        return None;
    };
    // Exit code 1 covers permanent and transient failures alike. Use the final
    // error, not earlier warnings from an extractor that subsequently recovered.
    let details = details.to_lowercase().replace('’', "'");
    let error = details
        .lines()
        .rev()
        .find(|line| line.starts_with("error:"))?;
    let youtube = url::Url::parse(url).ok().is_some_and(|url| {
        url.host_str().is_some_and(|host| {
            host == "youtu.be" || host == "youtube.com" || host.ends_with(".youtube.com")
        })
    });
    let http_code = error.split("http error ").nth(1).and_then(|tail| {
        tail.split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse::<u16>()
            .ok()
    });
    match http_code {
        Some(429) => return Some(RetryReason::RateLimited),
        Some(403) if youtube => return Some(RetryReason::Blocked),
        Some(408 | 500 | 502 | 503 | 504) => return Some(RetryReason::Network),
        Some(_) => return None,
        None => {}
    }
    if youtube && error.contains("sign in to confirm you're not a bot") {
        return Some(RetryReason::Blocked);
    }
    [
        "timed out",
        "connection reset",
        "connection aborted",
        "remote end closed connection",
        "temporary failure in name resolution",
    ]
    .iter()
    .any(|message| error.contains(message))
    .then_some(RetryReason::Network)
}

async fn retry_download<T, F, Fut>(
    url: &str,
    cancel: &CancellationToken,
    progress: &MediaProgress,
    mut attempt: F,
) -> Result<T, MediaError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, MediaError>>,
{
    for retries in 0..=MAX_DOWNLOAD_RETRIES {
        if cancel.is_cancelled() {
            return Err(MediaError::Cancelled);
        }
        let error = match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if cancel.is_cancelled() {
            return Err(MediaError::Cancelled);
        }
        let Some(reason) = retry_reason(url, &error) else {
            return Err(error);
        };
        if retries == MAX_DOWNLOAD_RETRIES {
            return Err(MediaError::RetriesExhausted(Box::new(error)));
        }
        let (base_delay, message) = match reason {
            RetryReason::Blocked => (5, "YouTube temporarily blocked the request."),
            RetryReason::RateLimited => (30, "The site is limiting requests."),
            RetryReason::Network => (5, "The connection was interrupted."),
        };
        if retries == 0 {
            let mut update = ProgressUpdate::message(
                JobPhase::Retrying,
                format!("{message} Retrying now (retry 1 of {MAX_DOWNLOAD_RETRIES})…"),
            );
            update.fraction = Some(0.0);
            progress(update);
            continue;
        }
        for remaining in (1..=base_delay * (1 << retries)).rev() {
            let mut update = ProgressUpdate::message(
                JobPhase::Retrying,
                format!(
                    "{message} Retrying in {remaining}s (retry {} of {MAX_DOWNLOAD_RETRIES})…",
                    retries + 1
                ),
            );
            // Clear the previous attempt's percentage; the existing UI shows an
            // indeterminate bar at zero and keeps the same Cancel action active.
            update.fraction = Some(0.0);
            progress(update);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(MediaError::Cancelled),
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
            }
        }
    }
    unreachable!("the last attempt always returns")
}

async fn run_pipeline(
    tools: &ActiveToolset,
    request: &DownloadRequest,
    staging: &Path,
    cancel: &CancellationToken,
    progress: &MediaProgress,
) -> Result<PathBuf, MediaError> {
    let media_info = with_preparation_activity(
        ProgressUpdate::message(JobPhase::Inspecting, "Checking available formats…"),
        progress,
        |updates| async move { inspect_formats(tools, request, cancel, &updates).await },
    )
    .await?;
    let av1_supported = if request.mode == DownloadMode::Video
        && media_info
            .formats
            .iter()
            .any(|format| is_av1_codec(format.vcodec.as_deref()))
    {
        supports_software_av1(&tools.ffmpeg(), cancel).await?
    } else {
        true
    };
    let selection = select_formats(
        &media_info.formats,
        request.mode,
        request.video_quality,
        av1_supported,
    )?;
    let downloaded = with_preparation_activity(
        ProgressUpdate::message(
            JobPhase::Preparing,
            format!("Preparing download…\n{}", selection.summary),
        ),
        progress,
        |updates| async move {
            run_yt_dlp(
                tools,
                request,
                staging,
                &selection.format_spec,
                selection.merge_container,
                cancel,
                &updates,
            )
            .await
        },
    )
    .await?;
    if cancel.is_cancelled() {
        return Err(MediaError::Cancelled);
    }

    (progress)(ProgressUpdate::message(
        JobPhase::Inspecting,
        "Checking media codecs…",
    ));
    let probe = probe_media(&tools.ffprobe(), &downloaded, cancel).await?;
    let output = unique_output_path(
        &request.output_directory,
        downloaded
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("download"),
        match request.mode {
            DownloadMode::Video => "mp4",
            DownloadMode::AudioOnly => "m4a",
        },
    );

    finalize_media(
        tools,
        &downloaded,
        &output,
        request.mode,
        &probe,
        cancel,
        progress,
    )
    .await?;

    (progress)(ProgressUpdate {
        phase: JobPhase::Completed,
        fraction: Some(1.0),
        downloaded_bytes: None,
        total_bytes: None,
        speed_bytes_per_second: None,
        message: format!("Saved {}", output.display()),
    });
    Ok(output)
}

// Keep unmeasurable preparation visibly active until the subprocess reports progress.
// The guard serializes timer and subprocess updates so an old preparation message
// can never overwrite the first download update.
async fn with_preparation_activity<T, F, Fut>(
    initial: ProgressUpdate,
    progress: &MediaProgress,
    operation: F,
) -> T
where
    F: FnOnce(MediaProgress) -> Fut,
    Fut: Future<Output = T>,
{
    progress(initial.clone());
    let activity = Arc::new(Mutex::new(Some((
        initial.clone(),
        tokio::time::Instant::now(),
    ))));
    let activity_sink = Arc::clone(&activity);
    let progress_sink = Arc::clone(progress);
    let updates: MediaProgress = Arc::new(move |update| {
        let mut activity = activity_sink.lock().expect("activity mutex poisoned");
        // Extraction can report several unmeasurable stages before byte progress.
        if matches!(update.phase, JobPhase::Inspecting | JobPhase::Preparing)
            && update.fraction.is_none()
        {
            if activity.as_ref().is_some_and(|(current, _)| {
                current.phase == update.phase && current.message == update.message
            }) {
                return;
            }
            *activity = Some((update.clone(), tokio::time::Instant::now()));
        } else {
            *activity = None;
        }
        progress_sink(update);
    });
    let started = tokio::time::Instant::now();
    let mut timer = tokio::time::interval_at(
        started + std::time::Duration::from_secs(1),
        std::time::Duration::from_secs(1),
    );
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let operation = operation(updates);
    tokio::pin!(operation);
    loop {
        tokio::select! {
            biased;
            result = &mut operation => return result,
            _ = timer.tick() => {
                let activity = activity.lock().expect("activity mutex poisoned");
                if let Some((current, step_started)) = activity.as_ref() {
                    let elapsed = step_started.elapsed().as_secs();
                    if elapsed >= 10 {
                        let mut update = current.clone();
                        update.message = match current.message.split_once('\n') {
                            Some((status, details)) => format!("{status} · {elapsed}s elapsed\n{details}"),
                            None => format!("{} · {elapsed}s elapsed", current.message),
                        };
                        progress(update);
                    }
                }
            }
        }
    }
}

async fn inspect_formats(
    tools: &ActiveToolset,
    request: &DownloadRequest,
    cancel: &CancellationToken,
    progress: &MediaProgress,
) -> Result<MediaInfo, MediaError> {
    if cancel.is_cancelled() {
        return Err(MediaError::Cancelled);
    }

    let mut command = Command::new(tools.yt_dlp());
    command
        .arg("--no-config")
        .arg("--no-update")
        .arg("--no-playlist")
        .arg("--no-quiet")
        .arg("--no-colors")
        .arg("--dump-single-json")
        .arg("--ffmpeg-location")
        .arg(&tools.directory)
        .arg("--js-runtimes")
        .arg(format!("deno:{}", tools.deno().display()))
        .arg("--")
        .arg(&request.url);
    configure_child(&mut command, &tools.directory);

    let json = Arc::new(Mutex::new(String::new()));
    let json_sink = Arc::clone(&json);
    let progress_sink = Arc::clone(progress);
    let stdout_handler = Arc::new(move |line: String| {
        // --no-quiet mixes extractor messages with the single-line JSON on stdout.
        if !line.trim_start().starts_with('{') {
            if let Some(update) = parse_format_inspection_activity(&line) {
                progress_sink(update);
            }
            return;
        }
        let mut output = json_sink.lock().expect("format JSON mutex poisoned");
        output.push_str(&line);
        output.push('\n');
    });
    let diagnostics = Arc::new(Mutex::new(VecDeque::<String>::with_capacity(30)));
    let diagnostic_sink = Arc::clone(&diagnostics);
    let redacted_url = request.url.clone();
    let progress_sink = Arc::clone(progress);
    let stderr_handler = Arc::new(move |line: String| {
        if let Some(update) = parse_format_inspection_activity(&line) {
            progress_sink(update);
        }
        let safe = line.replace(&redacted_url, "[URL]");
        let mut lines = diagnostic_sink.lock().expect("diagnostics mutex poisoned");
        if lines.len() == 30 {
            lines.pop_front();
        }
        lines.push_back(safe);
    });

    let status = run_command(command, cancel, stdout_handler, stderr_handler).await?;
    if !status.success() {
        let details = diagnostics
            .lock()
            .expect("diagnostics mutex poisoned")
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(MediaError::YtDlp(if details.is_empty() {
            status.to_string()
        } else {
            details
        }));
    }

    progress(ProgressUpdate::message(
        JobPhase::Inspecting,
        "Choosing video and audio formats…",
    ));
    let output = json.lock().expect("format JSON mutex poisoned");
    Ok(serde_json::from_str(output.trim())?)
}

// Recognize known extractor activity only; never show URLs, IDs or raw diagnostics.
// Unknown/new messages leave the current stage and its elapsed timer intact.
fn parse_format_inspection_activity(line: &str) -> Option<ProgressUpdate> {
    let line = line.trim();
    if !line.starts_with('[') || line.starts_with("[debug]") {
        return None;
    }
    let message = if line.contains(": Extracting URL:") || line.contains("] Extracting URL:") {
        "Connecting to the video site…"
    } else if line.contains(": Downloading webpage") {
        "Loading video page…"
    } else if line.contains("Solving JS challenges") {
        "Resolving playback checks…"
    } else if line.contains("Downloading challenge solver") {
        "Preparing playback checks…"
    } else if line.contains("player API JSON") {
        "Fetching player information…"
    } else if line.contains(": Downloading player ") {
        "Loading video player…"
    } else if line.contains("Downloading m3u8")
        || line.contains("Downloading MPD")
        || line.contains("Downloading f4m")
        || line.contains("Downloading ISM")
    {
        "Reading available streams…"
    } else if line.contains(": Downloading JSON metadata")
        || line.contains(": Downloading API JSON")
    {
        "Fetching video information…"
    } else {
        return None;
    };
    Some(ProgressUpdate::message(JobPhase::Inspecting, message))
}

fn select_formats(
    formats: &[AvailableFormat],
    mode: DownloadMode,
    video_quality: VideoQuality,
    av1_supported: bool,
) -> Result<FormatSelection, MediaError> {
    match mode {
        DownloadMode::Video => select_video_formats(formats, video_quality, av1_supported),
        DownloadMode::AudioOnly => select_audio_format(formats),
    }
}

fn select_video_formats(
    formats: &[AvailableFormat],
    video_quality: VideoQuality,
    av1_supported: bool,
) -> Result<FormatSelection, MediaError> {
    let video_formats = formats
        .iter()
        .filter(|format| has_video(format) && format.has_drm != Some(true));
    if video_formats.clone().next().is_none() {
        return Err(MediaError::NoSuitableFormat(
            "no video stream was reported".into(),
        ));
    }

    let selected_resolution = select_resolution(video_formats.clone(), video_quality);
    let video = video_formats
        .filter(|format| {
            selected_resolution.is_none() || format_resolution(format) == selected_resolution
        })
        // Preserve the requested resolution rather than silently falling back to 1080p.
        .filter(|format| av1_supported || !is_av1_codec(format.vcodec.as_deref()))
        // yt-dlp orders formats from worst to best; max_by_key keeps the last tie.
        .max_by_key(|format| is_h264_codec(format.vcodec.as_deref()))
        .ok_or_else(|| {
            MediaError::NoSuitableFormat(
                if !av1_supported {
                    "the selected resolution requires AV1 decoding, but this FFmpeg build has no software AV1 decoder. Choose a lower resolution or use an FFmpeg build with libdav1d support".into()
                } else {
                    "no video stream was reported at the selected resolution".into()
                },
            )
        })?;

    let audio = if has_audio(video) {
        None
    } else {
        best_audio_only(formats)
    };
    let format_spec = match audio {
        Some(audio) => format!("{}+{}", video.format_id, audio.format_id),
        None => video.format_id.clone(),
    };
    let video_compatible = is_h264_codec(video.vcodec.as_deref());
    let audio_compatible = audio
        .map(|format| is_aac_codec(format.acodec.as_deref()))
        .unwrap_or_else(|| !has_audio(video) || is_aac_codec(video.acodec.as_deref()));
    let already_mp4 = audio.is_none()
        && video.ext.as_deref().is_some_and(is_mp4_container)
        && video_compatible
        && audio_compatible;
    let resolution = selected_resolution
        .map(|dimension| format!("{dimension}p"))
        .unwrap_or_else(|| "best resolution".into());
    let summary = match (video_compatible, audio_compatible, already_mp4) {
        (true, true, true) => {
            format!("Selected {resolution} H.264/AAC MP4; no conversion expected.")
        }
        (true, true, false) => {
            format!("Selected {resolution} H.264/AAC; remuxing without conversion.")
        }
        (true, false, _) => {
            format!("Selected {resolution} H.264; only audio requires conversion.")
        }
        (false, true, _) => {
            format!("No H.264 stream is available at {resolution}; video requires conversion.")
        }
        (false, false, _) => {
            format!("No compatible codecs are available at {resolution}; conversion is required.")
        }
    };

    Ok(FormatSelection {
        format_spec,
        merge_container: Some(if video_compatible && audio_compatible {
            "mp4"
        } else {
            "mkv"
        }),
        summary,
    })
}

fn select_resolution<'a>(
    formats: impl Iterator<Item = &'a AvailableFormat> + Clone,
    video_quality: VideoQuality,
) -> Option<u32> {
    let resolutions = formats.filter_map(format_resolution);
    match video_quality.maximum_dimension() {
        Some(limit) => resolutions
            .clone()
            .filter(|resolution| *resolution <= limit)
            .max()
            .or_else(|| resolutions.min()),
        None => resolutions.max(),
    }
}

fn format_resolution(format: &AvailableFormat) -> Option<u32> {
    match (format.width, format.height) {
        (Some(width), Some(height)) => Some(width.min(height)),
        (Some(width), None) => Some(width),
        (None, Some(height)) => Some(height),
        (None, None) => None,
    }
}

fn select_audio_format(formats: &[AvailableFormat]) -> Result<FormatSelection, MediaError> {
    let audio = formats
        .iter()
        .filter(|format| has_audio(format) && format.has_drm != Some(true))
        .max_by_key(|format| (!has_video(format), is_aac_codec(format.acodec.as_deref())))
        .ok_or_else(|| MediaError::NoSuitableFormat("no audio stream was reported".into()))?;
    let compatible = is_aac_codec(audio.acodec.as_deref());
    Ok(FormatSelection {
        format_spec: audio.format_id.clone(),
        merge_container: None,
        summary: if compatible {
            "Selected the best AAC audio stream; no conversion expected.".into()
        } else {
            "No AAC audio stream is available; audio conversion is required.".into()
        },
    })
}

fn best_audio_only(formats: &[AvailableFormat]) -> Option<&AvailableFormat> {
    formats
        .iter()
        .filter(|format| has_audio(format) && !has_video(format) && format.has_drm != Some(true))
        .max_by_key(|format| is_aac_codec(format.acodec.as_deref()))
}

fn has_video(format: &AvailableFormat) -> bool {
    codec_is_present(format.vcodec.as_deref())
}

fn has_audio(format: &AvailableFormat) -> bool {
    codec_is_present(format.acodec.as_deref())
}

fn codec_is_present(codec: Option<&str>) -> bool {
    codec.is_some_and(|codec| !codec.is_empty() && !codec.eq_ignore_ascii_case("none"))
}

fn is_av1_codec(codec: Option<&str>) -> bool {
    codec.is_some_and(|codec| {
        let codec = codec.to_ascii_lowercase();
        codec == "av1" || codec.starts_with("av01")
    })
}

// The native `av1` decoder is hardware-only. Listing it does not mean the
// software decoding path used by our conversion commands can read AV1.
fn has_software_av1_decoder(listing: &str) -> bool {
    listing.lines().any(|line| {
        let mut columns = line.split_whitespace();
        columns.next().is_some_and(|flags| flags.starts_with('V'))
            && matches!(columns.next(), Some("libdav1d" | "libaom-av1"))
    })
}

async fn supports_software_av1(
    ffmpeg: &Path,
    cancel: &CancellationToken,
) -> Result<bool, MediaError> {
    let mut command = Command::new(ffmpeg);
    command.args(["-hide_banner", "-decoders"]);
    configure_child(&mut command, ffmpeg.parent().unwrap_or(Path::new(".")));
    let supported = Arc::new(Mutex::new(false));
    let sink = Arc::clone(&supported);
    let stdout_handler = Arc::new(move |line: String| {
        if has_software_av1_decoder(&line) {
            *sink.lock().expect("decoder mutex poisoned") = true;
        }
    });
    let status = run_command(command, cancel, stdout_handler, Arc::new(|_: String| {})).await?;
    if !status.success() {
        return Err(MediaError::Ffmpeg(format!(
            "could not inspect available decoders: {status}"
        )));
    }
    let result = *supported.lock().expect("decoder mutex poisoned");
    Ok(result)
}

fn is_h264_codec(codec: Option<&str>) -> bool {
    codec.is_some_and(|codec| {
        let codec = codec.to_ascii_lowercase();
        codec == "h264" || codec.starts_with("avc1") || codec.starts_with("avc3")
    })
}

fn is_aac_codec(codec: Option<&str>) -> bool {
    codec.is_some_and(|codec| {
        let codec = codec.to_ascii_lowercase();
        codec == "aac" || codec.starts_with("mp4a")
    })
}

fn is_mp4_container(extension: &str) -> bool {
    extension.eq_ignore_ascii_case("mp4") || extension.eq_ignore_ascii_case("m4v")
}

async fn run_yt_dlp(
    tools: &ActiveToolset,
    request: &DownloadRequest,
    staging: &Path,
    format_spec: &str,
    merge_container: Option<&str>,
    cancel: &CancellationToken,
    progress: &MediaProgress,
) -> Result<PathBuf, MediaError> {
    let final_path: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let diagnostics = Arc::new(Mutex::new(VecDeque::<String>::with_capacity(30)));
    let mut command = Command::new(tools.yt_dlp());
    command
        .arg("--no-config")
        .arg("--no-update")
        .arg("--no-playlist")
        .arg("--newline")
        .arg("--progress")
        .args(["--progress-delta", "0.2"])
        .arg("--ffmpeg-location")
        .arg(&tools.directory)
        .arg("--js-runtimes")
        .arg(format!("deno:{}", tools.deno().display()))
        .arg("--paths")
        .arg(format!("home:{}", staging.display()))
        .arg("--output")
        .arg("%(title).180B [%(id)s].%(ext)s")
        .arg("--progress-template")
        .arg(format!(
            "download:{PROGRESS_PREFIX}%(progress._percent)f|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.speed)s"
        ))
        .arg("--print")
        .arg(format!("after_move:{FILE_PREFIX}%(filepath)j"));

    command.args(crate::platform::yt_dlp_filename_arguments(&tools.platform));

    command.args(["--format", format_spec]);
    if request.mode == DownloadMode::Video
        && let Some(container) = merge_container
    {
        command.args(["--merge-output-format", container]);
    }
    command.arg("--").arg(&request.url);
    configure_child(&mut command, &tools.directory);

    let output_path = Arc::clone(&final_path);
    let progress_sink = Arc::clone(progress);
    let stdout_handler = Arc::new(move |line: String| {
        if let Some(update) = parse_ytdlp_progress_line(&line) {
            (progress_sink)(update);
        } else if let Some(value) = line.strip_prefix(FILE_PREFIX)
            && let Ok(path) = serde_json::from_str::<String>(value)
        {
            *output_path.lock().expect("output path mutex poisoned") = Some(PathBuf::from(path));
        }
    });
    let redacted_url = request.url.clone();
    let diagnostic_sink = Arc::clone(&diagnostics);
    let progress_sink = Arc::clone(progress);
    let stderr_handler = Arc::new(move |line: String| {
        if let Some(update) = parse_ytdlp_progress_line(&line) {
            (progress_sink)(update);
            return;
        }
        let safe = line.replace(&redacted_url, "[URL]");
        let mut lines = diagnostic_sink.lock().expect("diagnostics mutex poisoned");
        if lines.len() == 30 {
            lines.pop_front();
        }
        lines.push_back(safe);
    });

    let status = run_command(command, cancel, stdout_handler, stderr_handler).await?;
    if !status.success() {
        let details = diagnostics
            .lock()
            .expect("diagnostics mutex poisoned")
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(MediaError::YtDlp(if details.is_empty() {
            status.to_string()
        } else {
            details
        }));
    }
    final_path
        .lock()
        .expect("output path mutex poisoned")
        .clone()
        .ok_or(MediaError::MissingOutput)
}

async fn probe_media(
    ffprobe: &Path,
    input: &Path,
    cancel: &CancellationToken,
) -> Result<ProbeResult, MediaError> {
    if cancel.is_cancelled() {
        return Err(MediaError::Cancelled);
    }
    let mut command = Command::new(ffprobe);
    command.args([
        "-v",
        "error",
        "-show_entries",
        "stream=codec_type,codec_name,pix_fmt,width,height,avg_frame_rate:format=duration",
        "-of",
        "json",
    ]);
    command
        .arg(input)
        .stdin(Stdio::null())
        .stderr(Stdio::piped());
    hide_console(&mut command);
    let output = command.output().await?;
    if !output.status.success() {
        return Err(MediaError::Ffmpeg(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

async fn finalize_media(
    tools: &ActiveToolset,
    input: &Path,
    output: &Path,
    mode: DownloadMode,
    probe: &ProbeResult,
    cancel: &CancellationToken,
    progress: &MediaProgress,
) -> Result<(), MediaError> {
    let ffmpeg = tools.ffmpeg();
    let partial = output.with_file_name(format!(
        "{}.partial.{}",
        output
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("download"),
        output
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("tmp")
    ));
    let _ = fs::remove_file(&partial).await;

    let video = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"));
    let audio = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("audio"));
    let direct_compatible = match mode {
        DownloadMode::Video => {
            video.is_some_and(is_compatible_h264)
                && audio.is_none_or(|stream| stream.codec_name.as_deref() == Some("aac"))
                && input
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(is_mp4_container)
        }
        DownloadMode::AudioOnly => {
            audio.is_some_and(|stream| stream.codec_name.as_deref() == Some("aac"))
                && input
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        extension.eq_ignore_ascii_case("m4a")
                            || extension.eq_ignore_ascii_case("mp4")
                    })
        }
    };
    if direct_compatible {
        (progress)(ProgressUpdate::message(
            JobPhase::Finalizing,
            "Finalizing compatible output without conversion…",
        ));
        fs::rename(input, output).await?;
        return Ok(());
    }

    let video_needs_conversion =
        mode == DownloadMode::Video && video.is_none_or(|stream| !is_compatible_h264(stream));
    let audio_needs_conversion =
        audio.is_some_and(|stream| stream.codec_name.as_deref() != Some("aac"));
    let work_phase = if video_needs_conversion || audio_needs_conversion {
        JobPhase::Converting
    } else {
        JobPhase::Remuxing
    };
    let duration_us = probe
        .format
        .duration
        .as_deref()
        .and_then(|value| value.parse::<f64>().ok())
        .map(|seconds| seconds * 1_000_000.0);
    let preferred_encoder = if video_needs_conversion {
        (progress)(ProgressUpdate::message(
            JobPhase::Inspecting,
            "Checking for a supported GPU encoder…",
        ));
        detect_gpu_h264_encoder(&ffmpeg, &tools.platform, cancel)
            .await?
            .unwrap_or(H264Encoder::CpuX264)
    } else {
        H264Encoder::CpuX264
    };
    let mut encoders = vec![preferred_encoder];
    if preferred_encoder.is_gpu() {
        encoders.push(H264Encoder::CpuX264);
    }

    let mut completed = false;
    let mut final_error = String::new();
    for (attempt, encoder) in encoders.into_iter().enumerate() {
        if attempt > 0 {
            let _ = fs::remove_file(&partial).await;
            (progress)(ProgressUpdate::message(
                JobPhase::Converting,
                "GPU encoding failed; retrying with the CPU…",
            ));
        }
        let work_message = finalization_message(
            mode,
            video_needs_conversion,
            audio_needs_conversion,
            encoder,
        );
        let command =
            build_finalization_command(&ffmpeg, input, &partial, mode, video, audio, encoder)?;
        match run_ffmpeg_attempt(
            command,
            cancel,
            progress,
            work_phase,
            &work_message,
            duration_us,
        )
        .await?
        {
            None => {
                completed = true;
                break;
            }
            Some(details) => final_error = details,
        }
    }
    if !completed {
        let _ = fs::remove_file(&partial).await;
        return Err(MediaError::Ffmpeg(final_error));
    }

    (progress)(ProgressUpdate::message(
        JobPhase::Finalizing,
        "Finalizing output…",
    ));
    fs::rename(partial, output).await?;
    Ok(())
}

async fn detect_gpu_h264_encoder(
    ffmpeg: &Path,
    platform: &str,
    cancel: &CancellationToken,
) -> Result<Option<H264Encoder>, MediaError> {
    for &encoder in H264Encoder::gpu_candidates(platform) {
        let mut command = Command::new(ffmpeg);
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=1280x720:r=1",
            "-frames:v",
            "1",
            "-an",
        ]);
        configure_h264_encoder(
            &mut command,
            encoder,
            youtube_sdr_bitrate_profile(720, 30.0),
        );
        command.args(["-f", "null", "-"]);
        configure_child(&mut command, ffmpeg.parent().unwrap_or(Path::new(".")));
        let ignore_output = Arc::new(|_: String| {});
        let status = run_command(command, cancel, ignore_output.clone(), ignore_output).await?;
        if status.success() {
            return Ok(Some(encoder));
        }
    }
    Ok(None)
}

fn build_finalization_command(
    ffmpeg: &Path,
    input: &Path,
    partial: &Path,
    mode: DownloadMode,
    video: Option<&ProbeStream>,
    audio: Option<&ProbeStream>,
    encoder: H264Encoder,
) -> Result<Command, MediaError> {
    let mut command = Command::new(ffmpeg);
    command.args(["-hide_banner", "-y", "-i"]).arg(input);

    match mode {
        DownloadMode::Video => {
            let video = video.ok_or(MediaError::MissingStream)?;
            command.args(["-map", "0:v:0", "-map", "0:a:0?"]);
            if is_compatible_h264(video) {
                command.args(["-c:v", "copy"]);
            } else {
                configure_h264_encoder(&mut command, encoder, video_bitrate_profile(video));
            }
            command.args(["-tag:v", "avc1"]);
            if let Some(audio) = audio {
                if audio.codec_name.as_deref() == Some("aac") {
                    command.args(["-c:a", "copy"]);
                } else {
                    command.args(["-c:a", "aac", "-b:a", "256k"]);
                }
            }
            command.args([
                "-map_metadata",
                "0",
                "-map_chapters",
                "0",
                "-movflags",
                "+faststart",
            ]);
        }
        DownloadMode::AudioOnly => {
            let audio = audio.ok_or(MediaError::MissingStream)?;
            command.args(["-map", "0:a:0", "-vn"]);
            if audio.codec_name.as_deref() == Some("aac") {
                command.args(["-c:a", "copy"]);
            } else {
                command.args(["-c:a", "aac", "-b:a", "256k"]);
            }
            command.args(["-map_metadata", "0", "-movflags", "+faststart"]);
        }
    }
    command
        .args(["-progress", "pipe:1", "-nostats"])
        .arg(partial);
    configure_child(&mut command, ffmpeg.parent().unwrap_or(Path::new(".")));
    Ok(command)
}

fn video_bitrate_profile(stream: &ProbeStream) -> VideoBitrateProfile {
    let resolution = match (stream.width, stream.height) {
        (Some(width), Some(height)) => width.min(height),
        (Some(width), None) => width,
        (None, Some(height)) => height,
        (None, None) => 1080,
    };
    let frames_per_second = stream
        .avg_frame_rate
        .as_deref()
        .and_then(parse_frame_rate)
        .unwrap_or(30.0);
    youtube_sdr_bitrate_profile(resolution, frames_per_second)
}

fn parse_frame_rate(value: &str) -> Option<f64> {
    if let Some((numerator, denominator)) = value.split_once('/') {
        let numerator = numerator.parse::<f64>().ok()?;
        let denominator = denominator.parse::<f64>().ok()?;
        if denominator == 0.0 {
            None
        } else {
            Some(numerator / denominator)
        }
    } else {
        value.parse::<f64>().ok()
    }
}

fn youtube_sdr_bitrate_profile(resolution: u32, frames_per_second: f64) -> VideoBitrateProfile {
    let (target_kbps, maximum_kbps) = match (resolution, frames_per_second >= 48.0) {
        (4320.., false) => (80_000, 160_000),
        (4320.., true) => (120_000, 240_000),
        (2160.., false) => (35_000, 45_000),
        (2160.., true) => (53_000, 68_000),
        (1440.., false) => (16_000, 16_000),
        (1440.., true) => (24_000, 24_000),
        (1080.., false) => (8_000, 8_000),
        (1080.., true) => (12_000, 12_000),
        (720.., false) => (5_000, 5_000),
        (720.., true) => (7_500, 7_500),
        (480.., false) => (2_500, 2_500),
        (480.., true) => (4_000, 4_000),
        (_, false) => (1_000, 1_000),
        (_, true) => (1_500, 1_500),
    };
    VideoBitrateProfile {
        target_kbps,
        maximum_kbps,
    }
}

fn configure_h264_encoder(
    command: &mut Command,
    encoder: H264Encoder,
    bitrate: VideoBitrateProfile,
) {
    match encoder {
        H264Encoder::NvidiaNvenc => {
            command.args([
                "-c:v",
                "h264_nvenc",
                "-preset",
                "p5",
                "-tune",
                "hq",
                "-rc",
                "vbr",
                "-cq",
                "23",
                "-spatial-aq",
                "1",
                "-pix_fmt",
                "yuv420p",
            ]);
        }
        H264Encoder::IntelQuickSync => {
            command.args(["-c:v", "h264_qsv", "-preset", "medium", "-pix_fmt", "nv12"]);
        }
        H264Encoder::AmdAmf => {
            command.args([
                "-c:v", "h264_amf", "-quality", "quality", "-rc", "vbr_peak", "-pix_fmt", "nv12",
            ]);
        }
        H264Encoder::AppleVideoToolbox => {
            command.args([
                "-c:v",
                "h264_videotoolbox",
                "-profile:v",
                "high",
                "-pix_fmt",
                "yuv420p",
                "-allow_sw",
                "0",
            ]);
        }
        H264Encoder::CpuX264 => {
            command.args([
                "-c:v", "libx264", "-preset", "medium", "-crf", "18", "-pix_fmt", "yuv420p",
            ]);
        }
    }
    configure_bitrate_limits(command, bitrate, encoder.is_gpu());
}

fn configure_bitrate_limits(
    command: &mut Command,
    bitrate: VideoBitrateProfile,
    include_target: bool,
) {
    if include_target {
        command.arg("-b:v").arg(format!("{}k", bitrate.target_kbps));
    }
    command
        .arg("-maxrate")
        .arg(format!("{}k", bitrate.maximum_kbps))
        .arg("-bufsize")
        .arg(format!("{}k", bitrate.buffer_kbps()));
}

fn finalization_message(
    mode: DownloadMode,
    video_needs_conversion: bool,
    audio_needs_conversion: bool,
    encoder: H264Encoder,
) -> String {
    match mode {
        DownloadMode::Video => match (video_needs_conversion, audio_needs_conversion) {
            (false, false) => "Remuxing without conversion…".into(),
            (false, true) => "Copying video and converting audio…".into(),
            (true, false) => format!(
                "Converting video to H.264 with the {}…",
                encoder.display_name()
            ),
            (true, true) => format!(
                "Converting video with the {} and converting audio…",
                encoder.display_name()
            ),
        },
        DownloadMode::AudioOnly => {
            if audio_needs_conversion {
                "Converting audio to AAC…".into()
            } else {
                "Remuxing audio without conversion…".into()
            }
        }
    }
}

async fn run_ffmpeg_attempt(
    command: Command,
    cancel: &CancellationToken,
    progress: &MediaProgress,
    phase: JobPhase,
    message: &str,
    duration_us: Option<f64>,
) -> Result<Option<String>, MediaError> {
    let progress_sink = Arc::clone(progress);
    let progress_message = message.to_owned();
    let stdout_handler = Arc::new(move |line: String| {
        if let Some(value) = line.strip_prefix("out_time_us=")
            && let (Some(duration), Ok(position)) = (duration_us, value.parse::<f64>())
        {
            let fraction = (position / duration).clamp(0.0, 1.0) as f32;
            (progress_sink)(ProgressUpdate {
                phase,
                fraction: Some(fraction),
                downloaded_bytes: None,
                total_bytes: None,
                speed_bytes_per_second: None,
                message: progress_message.clone(),
            });
        }
    });
    let diagnostics = Arc::new(Mutex::new(VecDeque::<String>::with_capacity(30)));
    let diagnostic_sink = Arc::clone(&diagnostics);
    let stderr_handler = Arc::new(move |line: String| {
        let mut lines = diagnostic_sink.lock().expect("diagnostics mutex poisoned");
        if lines.len() == 30 {
            lines.pop_front();
        }
        lines.push_back(line);
    });

    (progress)(ProgressUpdate::message(phase, message));
    let status = run_command(command, cancel, stdout_handler, stderr_handler).await?;
    if status.success() {
        return Ok(None);
    }
    let details = diagnostics
        .lock()
        .expect("diagnostics mutex poisoned")
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Some(if details.is_empty() {
        status.to_string()
    } else {
        details
    }))
}

async fn run_command(
    mut command: Command,
    cancel: &CancellationToken,
    stdout_handler: Arc<dyn Fn(String) + Send + Sync>,
    stderr_handler: Arc<dyn Fn(String) + Send + Sync>,
) -> Result<ExitStatus, MediaError> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = command.spawn()?;
    #[cfg(windows)]
    let process_job = ProcessJob::attach(&child).ok();
    let process_id = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing child stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing child stderr"))?;

    let stdout_task = tokio::spawn(read_lines(stdout, stdout_handler));
    let stderr_task = tokio::spawn(read_lines(stderr, stderr_handler));
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = cancel.cancelled() => {
            #[cfg(windows)]
            if let Some(job) = process_job.as_ref() {
                job.terminate();
            } else {
                kill_process_tree(process_id).await;
            }
            #[cfg(not(windows))]
            kill_process_tree(process_id).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(MediaError::Cancelled);
        }
    };
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    Ok(status)
}

async fn read_lines<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    handler: Arc<dyn Fn(String) + Send + Sync>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        handler(line);
    }
}

fn configure_child(command: &mut Command, tool_directory: &Path) {
    let mut paths = vec![tool_directory.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    if let Ok(value) = std::env::join_paths(paths) {
        command.env("PATH", value);
    }
    hide_console(command);
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

#[cfg(windows)]
async fn kill_process_tree(process_id: Option<u32>) {
    let Some(process_id) = process_id else { return };
    let mut command = Command::new("taskkill");
    command.args(["/PID", &process_id.to_string(), "/T", "/F"]);
    hide_console(&mut command);
    let _ = command.output().await;
}

#[cfg(not(windows))]
async fn kill_process_tree(_process_id: Option<u32>) {}

#[cfg(windows)]
struct ProcessJob(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl ProcessJob {
    fn attach(child: &tokio::process::Child) -> io::Result<Self> {
        use std::{
            mem::size_of,
            os::windows::io::{AsRawHandle, FromRawHandle},
            ptr,
        };
        use windows_sys::Win32::{
            Foundation::GetLastError,
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };

        // SAFETY: The structures are initialized, the name is optional, and the child
        // process handle stays valid while this function assigns it to the job.
        unsafe {
            let raw_handle = CreateJobObjectW(ptr::null(), ptr::null());
            if raw_handle.is_null() {
                return Err(io::Error::from_raw_os_error(GetLastError() as i32));
            }
            let handle = std::os::windows::io::OwnedHandle::from_raw_handle(raw_handle);
            let mut information = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            information.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&information as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(io::Error::from_raw_os_error(GetLastError() as i32));
            }
            let process_handle = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("child process has no Windows handle"))?
                as windows_sys::Win32::Foundation::HANDLE;
            if AssignProcessToJobObject(handle.as_raw_handle(), process_handle) == 0 {
                return Err(io::Error::from_raw_os_error(GetLastError() as i32));
            }
            Ok(Self(handle))
        }
    }

    fn terminate(&self) {
        use std::os::windows::io::AsRawHandle;

        // SAFETY: self.0 is a live job handle until Drop.
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0.as_raw_handle(), 1);
        }
    }
}

fn is_compatible_h264(stream: &ProbeStream) -> bool {
    stream.codec_name.as_deref() == Some("h264")
        && matches!(stream.pix_fmt.as_deref(), Some("yuv420p" | "yuvj420p"))
}

fn parse_ytdlp_progress_line(line: &str) -> Option<ProgressUpdate> {
    let start = line.find(PROGRESS_PREFIX)? + PROGRESS_PREFIX.len();
    parse_ytdlp_progress(&line[start..])
}

fn parse_ytdlp_progress(value: &str) -> Option<ProgressUpdate> {
    let fields: Vec<_> = value.split('|').collect();
    if fields.len() != 4 {
        return None;
    }
    let percent = fields[0].trim().trim_end_matches('%').parse::<f32>().ok();
    let downloaded = optional_u64(fields[1]);
    let total = optional_u64(fields[2]);
    let speed = optional_u64(fields[3]);
    Some(ProgressUpdate {
        phase: JobPhase::Downloading,
        fraction: percent.map(|number| (number / 100.0).clamp(0.0, 1.0)),
        downloaded_bytes: downloaded,
        total_bytes: total,
        speed_bytes_per_second: speed,
        message: "Downloading media…".into(),
    })
}

fn optional_u64(value: &str) -> Option<u64> {
    value.trim().parse().ok()
}

fn unique_output_path(directory: &Path, stem: &str, extension: &str) -> PathBuf {
    let initial = directory.join(format!("{stem}.{extension}"));
    if !initial.exists() {
        return initial;
    }
    for number in 1..10_000 {
        let candidate = directory.join(format!("{stem} ({number}).{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    directory.join(format!("{stem}-{}.{}", Uuid::new_v4(), extension))
}

pub fn describe_progress(update: &ProgressUpdate) -> String {
    let mut parts = vec![update.message.clone()];
    if update.phase == JobPhase::Downloading {
        if let (Some(done), Some(total)) = (update.downloaded_bytes, update.total_bytes) {
            parts.push(format!("{} / {}", format_bytes(done), format_bytes(total)));
        }
        if let Some(speed) = update.speed_bytes_per_second {
            parts.push(format!("{}/s", format_bytes(speed)));
        }
    }
    parts.join(" · ")
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn preparation_activity_stops_when_download_progress_arrives() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let progress: MediaProgress = Arc::new(move |update| sink.lock().unwrap().push(update));
        let result = with_preparation_activity(
            ProgressUpdate::message(
                JobPhase::Preparing,
                "Preparing download…\nNo H.264 stream is available; video requires conversion.",
            ),
            &progress,
            |updates| async move {
                tokio::time::sleep(std::time::Duration::from_millis(10500)).await;
                updates(parse_ytdlp_progress("25|250|1000|50").unwrap());
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                Err::<(), _>(MediaError::Cancelled)
            },
        )
        .await;
        assert!(matches!(result, Err(MediaError::Cancelled)));
        let updates = received.lock().unwrap();
        assert_eq!(updates.len(), 3);
        assert_eq!(
            updates[0].message,
            "Preparing download…\nNo H.264 stream is available; video requires conversion."
        );
        assert_eq!(
            updates[1].message,
            "Preparing download… · 10s elapsed\nNo H.264 stream is available; video requires conversion."
        );
        assert_eq!(updates[1].fraction, None);
        assert_eq!(updates[2].phase, JobPhase::Downloading);
        assert_eq!(updates[2].fraction, Some(0.25));
    }

    #[tokio::test(start_paused = true)]
    async fn inspection_activity_continues_until_inspection_finishes() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let progress: MediaProgress = Arc::new(move |update| sink.lock().unwrap().push(update));
        let result = with_preparation_activity(
            ProgressUpdate::message(JobPhase::Inspecting, "Checking available formats…"),
            &progress,
            |_| async {
                tokio::time::sleep(std::time::Duration::from_millis(11500)).await;
                42
            },
        )
        .await;
        assert_eq!(result, 42);
        let updates = received.lock().unwrap();
        assert_eq!(updates.len(), 3);
        assert_eq!(
            updates[2].message,
            "Checking available formats… · 11s elapsed"
        );
        assert!(updates.iter().all(|update| update.fraction.is_none()));
    }

    #[test]
    fn recognizes_inspection_stages_without_exposing_raw_output() {
        for (line, expected) in [
            (
                "[youtube] Extracting URL: https://example.com/private",
                "Connecting to the video site…",
            ),
            ("[youtube] id: Downloading webpage", "Loading video page…"),
            (
                "[youtube] id: Downloading web safari player API JSON",
                "Fetching player information…",
            ),
            (
                "[youtube] id: Downloading player abc-main",
                "Loading video player…",
            ),
            (
                "[youtube] [jsc:deno] Downloading challenge solver lib script from https://example.com",
                "Preparing playback checks…",
            ),
            (
                "[youtube] [jsc:deno] Solving JS challenges using deno",
                "Resolving playback checks…",
            ),
            (
                "[youtube] id: Downloading m3u8 information",
                "Reading available streams…",
            ),
            (
                "[generic] id: Downloading MPD manifest",
                "Reading available streams…",
            ),
            (
                "[vimeo] id: Downloading JSON metadata",
                "Fetching video information…",
            ),
        ] {
            let update = parse_format_inspection_activity(line).unwrap();
            assert_eq!(update.message, expected);
            assert_eq!(update.phase, JobPhase::Inspecting);
            assert_eq!(update.fraction, None);
        }
        for line in [
            "WARNING: Downloading webpage failed",
            "ERROR: player API JSON unavailable",
            "[debug] Downloading player abc",
            "[generic] Unrecognized activity",
            r#"{"title":"Downloading player API JSON"}"#,
        ] {
            assert!(parse_format_inspection_activity(line).is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn inspection_timer_resets_per_step_and_waits_ten_seconds() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let progress: MediaProgress = Arc::new(move |update| sink.lock().unwrap().push(update));
        let observed = Arc::clone(&received);
        with_preparation_activity(
            ProgressUpdate::message(JobPhase::Inspecting, "Checking available formats…"),
            &progress,
            |updates| async move {
                tokio::time::sleep(std::time::Duration::from_millis(9500)).await;
                assert_eq!(observed.lock().unwrap().len(), 1);
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let stage =
                    parse_format_inspection_activity("[youtube] id: Downloading webpage").unwrap();
                updates(stage.clone());
                let count = observed.lock().unwrap().len();
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                updates(stage); // Repeated activity in the same step must not reset its timer.
                tokio::time::sleep(std::time::Duration::from_millis(6900)).await;
                assert_eq!(observed.lock().unwrap().len(), count);
                assert_eq!(
                    observed.lock().unwrap().last().unwrap().message,
                    "Loading video page…"
                );
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            },
        )
        .await;
        let updates = received.lock().unwrap();
        let step_index = updates
            .iter()
            .position(|update| update.message == "Loading video page…")
            .unwrap();
        assert_eq!(
            updates[step_index + 1].message,
            "Loading video page… · 10s elapsed"
        );
        assert_eq!(
            updates[step_index + 2].message,
            "Loading video page… · 11s elapsed"
        );
        assert_eq!(updates.len(), step_index + 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inspection_separates_activity_from_format_json() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("yt-dlp");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
printf '%s\n' '[youtube] id: Downloading webpage' '[generic] Unknown activity' '{"formats":[]}'
printf '%s\n' '[youtube] id: Downloading m3u8 information' >&2
"#,
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tools = ActiveToolset {
            id: "test".into(),
            yt_dlp_version: "test".into(),
            ffmpeg_version: "test".into(),
            deno_version: "test".into(),
            directory: directory.path().into(),
            platform: "macos-arm64".into(),
            yt_dlp_path: "yt-dlp".into(),
            ffmpeg_path: "ffmpeg".into(),
            ffprobe_path: "ffprobe".into(),
            deno_path: "deno".into(),
        };
        let request = DownloadRequest {
            url: "https://example.com/video".into(),
            mode: DownloadMode::Video,
            video_quality: VideoQuality::Best,
            output_directory: directory.path().into(),
        };
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let progress: MediaProgress = Arc::new(move |update| sink.lock().unwrap().push(update));
        let info = inspect_formats(&tools, &request, &CancellationToken::new(), &progress)
            .await
            .unwrap();
        assert!(info.formats.is_empty());
        let updates = received.lock().unwrap();
        assert!(
            updates
                .iter()
                .any(|update| update.message == "Loading video page…")
        );
        assert!(
            updates
                .iter()
                .any(|update| update.message == "Reading available streams…")
        );
        assert_eq!(
            updates.last().unwrap().message,
            "Choosing video and audio formats…"
        );
    }

    const YOUTUBE_URL: &str = "https://www.youtube.com/watch?v=test";

    #[test]
    fn classifies_only_explicit_transient_failures() {
        for code in [403, 408, 429, 500, 502, 503, 504] {
            assert!(
                retry_reason(
                    YOUTUBE_URL,
                    &MediaError::YtDlp(format!(
                        "ERROR: unable to download video data: HTTP Error {code}: failure"
                    ))
                )
                .is_some(),
                "{code}"
            );
        }
        for message in [
            "Sign in to confirm you're not a bot",
            "Sign in to confirm you’re not a bot",
            "The read operation timed out",
            "Connection reset by peer",
            "Remote end closed connection without response",
            "Temporary failure in name resolution",
        ] {
            assert!(
                retry_reason(YOUTUBE_URL, &MediaError::YtDlp(format!("ERROR: {message}")))
                    .is_some()
            );
        }
        for message in [
            "HTTP Error 400: Bad Request",
            "HTTP Error 401: Unauthorized",
            "HTTP Error 402: Payment Required",
            "HTTP Error 404: Not Found",
            "HTTP Error 410: Gone",
            "HTTP Error 501: Not Implemented",
            "Private video",
            "Video unavailable",
            "Sign in to confirm your age",
            "Requested format is not available",
            "Permission denied",
            "No space left on device",
            "WARNING: HTTP Error 403: Forbidden\nERROR: Private video",
            "WARNING: HTTP Error 429: Too Many Requests",
            "exit status: 1",
        ] {
            let details = if message.starts_with("WARNING:") || message.starts_with("exit") {
                message.to_owned()
            } else {
                format!("ERROR: {message}")
            };
            assert_eq!(
                retry_reason(YOUTUBE_URL, &MediaError::YtDlp(details)),
                None,
                "{message}"
            );
        }
        for url in [
            "https://example.com/video",
            "https://youtube.com.example.com/video",
        ] {
            assert_eq!(
                retry_reason(
                    url,
                    &MediaError::YtDlp("ERROR: HTTP Error 403: Forbidden".into())
                ),
                None
            );
        }
        assert_eq!(
            retry_reason(
                YOUTUBE_URL,
                &MediaError::Ffmpeg("ERROR: HTTP Error 503".into())
            ),
            None
        );
        assert_eq!(retry_reason(YOUTUBE_URL, &MediaError::Cancelled), None);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_three_times_then_preserves_final_error() {
        let updates = Arc::new(Mutex::new(Vec::new()));
        let sink = updates.clone();
        let progress: MediaProgress = Arc::new(move |update| sink.lock().unwrap().push(update));
        let mut attempts = 0;
        let started = tokio::time::Instant::now();
        let result = retry_download(YOUTUBE_URL, &CancellationToken::new(), &progress, || {
            attempts += 1;
            std::future::ready(Err::<(), _>(MediaError::YtDlp(
                "ERROR: HTTP Error 403: Forbidden".into(),
            )))
        })
        .await;
        assert_eq!(attempts, 4);
        assert_eq!(started.elapsed().as_secs(), 30);
        assert!(matches!(result, Err(MediaError::RetriesExhausted(_))));
        let updates = updates.lock().unwrap();
        assert!(updates[0].message.contains("Retrying now (retry 1 of 3)"));
        assert!(
            updates
                .last()
                .unwrap()
                .message
                .contains("1s (retry 3 of 3)")
        );
        assert!(
            updates
                .iter()
                .all(|u| u.phase == JobPhase::Retrying && u.fraction == Some(0.0))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn first_retry_is_immediate() {
        let progress: MediaProgress = Arc::new(|_| {});
        for code in [403, 429, 503] {
            let started = tokio::time::Instant::now();
            let mut attempts = 0;
            retry_download(YOUTUBE_URL, &CancellationToken::new(), &progress, || {
                attempts += 1;
                std::future::ready(if attempts == 1 {
                    Err(MediaError::YtDlp(format!(
                        "ERROR: HTTP Error {code}: failure"
                    )))
                } else {
                    Ok(())
                })
            })
            .await
            .unwrap();
            assert_eq!(attempts, 2);
            assert_eq!(started.elapsed(), std::time::Duration::ZERO);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stops_after_success_and_backs_off_rate_limits() {
        let progress: MediaProgress = Arc::new(|_| {});
        let mut attempts = 0;
        let started = tokio::time::Instant::now();
        let result = retry_download(YOUTUBE_URL, &CancellationToken::new(), &progress, || {
            attempts += 1;
            std::future::ready(if attempts == 3 {
                Ok("saved")
            } else {
                Err(MediaError::YtDlp(
                    "ERROR: HTTP Error 429: Too Many Requests".into(),
                ))
            })
        })
        .await
        .unwrap();
        assert_eq!(result, "saved");
        assert_eq!(attempts, 3);
        assert_eq!(started.elapsed().as_secs(), 60);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_during_backoff_prevents_another_attempt() {
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let progress: MediaProgress = Arc::new(move |update| {
            if update.message.contains("retry 2 of 3") {
                token.cancel();
            }
        });
        let mut attempts = 0;
        let result = retry_download(YOUTUBE_URL, &cancel, &progress, || {
            attempts += 1;
            std::future::ready(Err::<(), _>(MediaError::YtDlp(
                "ERROR: HTTP Error 403: Forbidden".into(),
            )))
        })
        .await;
        assert!(matches!(result, Err(MediaError::Cancelled)));
        assert_eq!(attempts, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn permanent_errors_stop_immediately_even_after_a_transient_error() {
        let progress: MediaProgress = Arc::new(|_| {});
        for transient_first in [false, true] {
            let mut attempts = 0;
            let result = retry_download(YOUTUBE_URL, &CancellationToken::new(), &progress, || {
                attempts += 1;
                std::future::ready(Err::<(), _>(MediaError::YtDlp(
                    if transient_first && attempts == 1 {
                        "ERROR: HTTP Error 403: Forbidden".into()
                    } else {
                        "ERROR: Private video".into()
                    },
                )))
            })
            .await;
            assert_eq!(attempts, if transient_first { 2 } else { 1 });
            assert!(matches!(result, Err(MediaError::YtDlp(_))));
        }
    }

    fn format(
        id: &str,
        extension: &str,
        video_codec: Option<&str>,
        audio_codec: Option<&str>,
        height: Option<u32>,
    ) -> AvailableFormat {
        AvailableFormat {
            format_id: id.into(),
            ext: Some(extension.into()),
            vcodec: video_codec.map(str::to_owned),
            acodec: audio_codec.map(str::to_owned),
            width: height.map(|height| height.saturating_mul(16) / 9),
            height,
            has_drm: None,
        }
    }

    #[test]
    fn parses_download_progress() {
        let result = parse_ytdlp_progress(" 42.5%|425|1000|50").unwrap();
        assert_eq!(result.fraction, Some(0.425));
        assert_eq!(result.downloaded_bytes, Some(425));
    }

    #[test]
    fn parses_numeric_download_progress_from_either_output_stream() {
        let line = format!("{PROGRESS_PREFIX}42.500000|425|1000|50");
        let result = parse_ytdlp_progress_line(&line).unwrap();
        assert_eq!(result.fraction, Some(0.425));

        let stderr_style = format!("[download] {PROGRESS_PREFIX}75.0|750|1000|50");
        let result = parse_ytdlp_progress_line(&stderr_style).unwrap();
        assert_eq!(result.fraction, Some(0.75));
    }

    #[test]
    fn accepts_only_compatible_h264_pixel_formats() {
        let compatible = ProbeStream {
            codec_type: Some("video".into()),
            codec_name: Some("h264".into()),
            pix_fmt: Some("yuv420p".into()),
            width: Some(1920),
            height: Some(1080),
            avg_frame_rate: Some("30000/1001".into()),
        };
        assert!(is_compatible_h264(&compatible));
        let incompatible = ProbeStream {
            pix_fmt: Some("yuv444p10le".into()),
            ..compatible
        };
        assert!(!is_compatible_h264(&incompatible));
    }

    #[test]
    fn configures_each_supported_h264_encoder() {
        let cases = [
            (H264Encoder::NvidiaNvenc, "h264_nvenc"),
            (H264Encoder::IntelQuickSync, "h264_qsv"),
            (H264Encoder::AmdAmf, "h264_amf"),
            (H264Encoder::AppleVideoToolbox, "h264_videotoolbox"),
            (H264Encoder::CpuX264, "libx264"),
        ];

        for (encoder, expected_codec) in cases {
            let mut command = Command::new("ffmpeg");
            configure_h264_encoder(
                &mut command,
                encoder,
                youtube_sdr_bitrate_profile(2160, 30.0),
            );
            let arguments = command
                .as_std()
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert!(arguments.iter().any(|argument| argument == expected_codec));
            assert!(arguments.iter().any(|argument| argument == "45000k"));
        }
    }

    #[test]
    fn parses_fractional_frame_rates() {
        let ntsc = parse_frame_rate("60000/1001").unwrap();
        assert!((ntsc - 59.94).abs() < 0.01);
        assert_eq!(parse_frame_rate("30"), Some(30.0));
        assert_eq!(parse_frame_rate("0/0"), None);
    }

    #[test]
    fn follows_youtube_bitrate_guidance_by_resolution_and_frame_rate() {
        assert_eq!(
            youtube_sdr_bitrate_profile(2160, 30.0),
            VideoBitrateProfile {
                target_kbps: 35_000,
                maximum_kbps: 45_000,
            }
        );
        assert_eq!(
            youtube_sdr_bitrate_profile(2160, 60.0),
            VideoBitrateProfile {
                target_kbps: 53_000,
                maximum_kbps: 68_000,
            }
        );
        assert_eq!(
            youtube_sdr_bitrate_profile(1080, 30.0),
            VideoBitrateProfile {
                target_kbps: 8_000,
                maximum_kbps: 8_000,
            }
        );
        assert_eq!(
            youtube_sdr_bitrate_profile(1080, 60.0),
            VideoBitrateProfile {
                target_kbps: 12_000,
                maximum_kbps: 12_000,
            }
        );
    }

    #[test]
    fn classifies_portrait_video_by_its_shorter_dimension() {
        let portrait = ProbeStream {
            codec_type: Some("video".into()),
            codec_name: Some("av1".into()),
            pix_fmt: Some("yuv420p".into()),
            width: Some(1080),
            height: Some(1920),
            avg_frame_rate: Some("60/1".into()),
        };

        assert_eq!(
            video_bitrate_profile(&portrait),
            VideoBitrateProfile {
                target_kbps: 12_000,
                maximum_kbps: 12_000,
            }
        );
    }

    #[test]
    fn describes_gpu_and_cpu_conversion_attempts() {
        assert!(
            finalization_message(DownloadMode::Video, true, false, H264Encoder::NvidiaNvenc)
                .contains("NVIDIA GPU")
        );
        assert!(
            finalization_message(DownloadMode::Video, true, false, H264Encoder::CpuX264)
                .contains("CPU")
        );
    }

    #[test]
    fn chooses_h264_when_it_exists_at_the_maximum_resolution() {
        let formats = vec![
            format("h264", "mp4", Some("avc1.640033"), Some("none"), Some(2160)),
            format(
                "av1",
                "webm",
                Some("av01.0.13M.08"),
                Some("none"),
                Some(2160),
            ),
            format("aac", "m4a", Some("none"), Some("mp4a.40.2"), None),
            format("opus", "webm", Some("none"), Some("opus"), None),
        ];

        let selection =
            select_formats(&formats, DownloadMode::Video, VideoQuality::Best, true).unwrap();

        assert_eq!(selection.format_spec, "h264+aac");
        assert!(selection.summary.contains("2160p H.264/AAC"));
        assert!(selection.summary.contains("remuxing"));
    }

    #[test]
    fn distinguishes_software_av1_decoders_from_hardware_only_decoders() {
        assert!(!has_software_av1_decoder(
            " V....D av1 Alliance for Open Media AV1\n V..... av1_cuvid Nvidia"
        ));
        assert!(has_software_av1_decoder(
            " V..... libdav1d dav1d AV1 decoder"
        ));
        assert!(has_software_av1_decoder(" V....D libaom-av1 libaom AV1"));
        assert!(!has_software_av1_decoder(
            " A....D wmav1 Windows Media Audio"
        ));
    }

    #[test]
    fn avoids_unsupported_av1_without_losing_4k_resolution() {
        let formats = vec![
            format("h264", "mp4", Some("avc1.640028"), Some("none"), Some(1080)),
            format("vp9", "webm", Some("vp9"), Some("none"), Some(2160)),
            format(
                "av1",
                "mp4",
                Some("av01.0.13M.08"),
                Some("none"),
                Some(2160),
            ),
            format("aac", "m4a", Some("none"), Some("mp4a.40.2"), None),
        ];
        let selection =
            select_formats(&formats, DownloadMode::Video, VideoQuality::Best, false).unwrap();
        assert_eq!(selection.format_spec, "vp9+aac");
        assert!(selection.summary.contains("2160p"));
        let full_build =
            select_formats(&formats, DownloadMode::Video, VideoQuality::Best, true).unwrap();
        assert_eq!(full_build.format_spec, "av1+aac");
        let hd = select_formats(&formats, DownloadMode::Video, VideoQuality::P1080, false).unwrap();
        assert_eq!(hd.format_spec, "h264+aac");
        let audio =
            select_formats(&formats, DownloadMode::AudioOnly, VideoQuality::Best, false).unwrap();
        assert_eq!(audio.format_spec, "aac");
    }

    #[test]
    fn reports_unsupported_av1_before_downloading_instead_of_lowering_resolution() {
        let formats = vec![
            format("h264", "mp4", Some("avc1"), Some("none"), Some(1080)),
            format("av1", "mp4", Some("av1"), Some("none"), Some(2160)),
        ];
        let error =
            select_formats(&formats, DownloadMode::Video, VideoQuality::Best, false).unwrap_err();
        assert!(error.to_string().contains("no software AV1 decoder"));
    }

    #[test]
    fn chooses_maximum_resolution_even_when_only_a_lower_resolution_has_h264() {
        let formats = vec![
            format(
                "h264-1080",
                "mp4",
                Some("avc1.640028"),
                Some("none"),
                Some(1080),
            ),
            format(
                "av1-2160",
                "webm",
                Some("av01.0.13M.08"),
                Some("none"),
                Some(2160),
            ),
            format("aac", "m4a", Some("none"), Some("mp4a.40.2"), None),
        ];

        let selection =
            select_formats(&formats, DownloadMode::Video, VideoQuality::Best, true).unwrap();

        assert_eq!(selection.format_spec, "av1-2160+aac");
        assert!(
            selection
                .summary
                .contains("No H.264 stream is available at 2160p")
        );
    }

    #[test]
    fn quality_limit_uses_the_highest_available_resolution_at_or_below_it() {
        let formats = vec![
            format(
                "h264-1080",
                "mp4",
                Some("avc1.640028"),
                Some("none"),
                Some(1080),
            ),
            format(
                "av1-1440",
                "webm",
                Some("av01.0.12M.08"),
                Some("none"),
                Some(1440),
            ),
            format(
                "av1-2160",
                "webm",
                Some("av01.0.13M.08"),
                Some("none"),
                Some(2160),
            ),
            format(
                "av1-4320",
                "webm",
                Some("av01.0.17M.08"),
                Some("none"),
                Some(4320),
            ),
            format("aac", "m4a", Some("none"), Some("mp4a.40.2"), None),
        ];

        let full_hd =
            select_formats(&formats, DownloadMode::Video, VideoQuality::P1080, true).unwrap();
        let fourteen_forty =
            select_formats(&formats, DownloadMode::Video, VideoQuality::P1440, true).unwrap();
        let best = select_formats(&formats, DownloadMode::Video, VideoQuality::Best, true).unwrap();

        assert_eq!(full_hd.format_spec, "h264-1080+aac");
        assert!(full_hd.summary.contains("1080p H.264/AAC"));
        assert_eq!(fourteen_forty.format_spec, "av1-1440+aac");
        assert!(fourteen_forty.summary.contains("at 1440p"));
        assert_eq!(best.format_spec, "av1-4320+aac");
        assert!(best.summary.contains("at 4320p"));
    }

    #[test]
    fn quality_limit_falls_back_to_the_best_lower_available_tier() {
        let formats = vec![
            format(
                "h264-720",
                "mp4",
                Some("avc1.64001f"),
                Some("none"),
                Some(720),
            ),
            format(
                "av1-1440",
                "webm",
                Some("av01.0.12M.08"),
                Some("none"),
                Some(1440),
            ),
            format("aac", "m4a", Some("none"), Some("mp4a.40.2"), None),
        ];

        let selection =
            select_formats(&formats, DownloadMode::Video, VideoQuality::P1080, true).unwrap();

        assert_eq!(selection.format_spec, "h264-720+aac");
        assert!(selection.summary.contains("720p H.264/AAC"));
    }

    #[test]
    fn quality_limit_handles_portrait_video_by_its_shorter_dimension() {
        let mut portrait = format(
            "portrait-1080",
            "mp4",
            Some("avc1.640028"),
            Some("none"),
            Some(1920),
        );
        portrait.width = Some(1080);
        let formats = vec![
            portrait,
            format("aac", "m4a", Some("none"), Some("mp4a.40.2"), None),
        ];

        let selection =
            select_formats(&formats, DownloadMode::Video, VideoQuality::P1080, true).unwrap();

        assert_eq!(selection.format_spec, "portrait-1080+aac");
        assert!(selection.summary.contains("1080p H.264/AAC"));
    }

    #[test]
    fn ignores_drm_formats_when_determining_maximum_resolution() {
        let mut drm = format(
            "drm-2160",
            "mp4",
            Some("avc1.640033"),
            Some("none"),
            Some(2160),
        );
        drm.has_drm = Some(true);
        let formats = vec![
            format(
                "h264-1080",
                "mp4",
                Some("avc1.640028"),
                Some("none"),
                Some(1080),
            ),
            drm,
            format("aac", "m4a", Some("none"), Some("mp4a.40.2"), None),
        ];

        let selection =
            select_formats(&formats, DownloadMode::Video, VideoQuality::Best, true).unwrap();

        assert_eq!(selection.format_spec, "h264-1080+aac");
        assert!(selection.summary.contains("1080p H.264/AAC"));
    }

    #[test]
    fn uses_a_combined_compatible_mp4_without_conversion() {
        let formats = vec![format(
            "combined",
            "mp4",
            Some("avc1.640028"),
            Some("mp4a.40.2"),
            Some(1080),
        )];

        let selection =
            select_formats(&formats, DownloadMode::Video, VideoQuality::Best, true).unwrap();

        assert_eq!(selection.format_spec, "combined");
        assert!(selection.summary.contains("no conversion expected"));
    }

    #[test]
    fn prefers_aac_audio_over_a_later_incompatible_audio_format() {
        let formats = vec![
            format("aac", "m4a", Some("none"), Some("mp4a.40.2"), None),
            format("opus", "webm", Some("none"), Some("opus"), None),
        ];

        let selection =
            select_formats(&formats, DownloadMode::AudioOnly, VideoQuality::Best, true).unwrap();

        assert_eq!(selection.format_spec, "aac");
        assert!(selection.summary.contains("best AAC"));
    }

    #[test]
    fn audio_selection_preserves_source_and_codec_preference() {
        let mut drm = format("drm", "m4a", Some("none"), Some("aac"), None);
        drm.has_drm = Some(true);
        let formats = vec![
            format("aac-first", "m4a", None, Some("aac"), None),
            format("aac-last", "m4a", None, Some("aac"), None),
            format("opus", "webm", None, Some("opus"), None),
            format("combined", "mp4", Some("h264"), Some("aac"), Some(1080)),
            drm,
        ];
        for (start, expected) in [(0, "aac-last"), (2, "opus"), (3, "combined")] {
            let selection = select_audio_format(&formats[start..]).unwrap();
            assert_eq!(selection.format_spec, expected);
        }
        assert!(select_audio_format(&formats[4..]).is_err());
        assert_eq!(best_audio_only(&formats).unwrap().format_id, "aac-last");
        assert!(best_audio_only(&formats[3..]).is_none());
    }

    #[test]
    fn video_selection_handles_ties_unknown_dimensions_and_above_limit_sources() {
        let formats = vec![
            format("unknown", "mp4", Some("h264"), None, None),
            format("1440-first", "mp4", Some("h264"), None, Some(1440)),
            format("1440-last", "mp4", Some("h264"), None, Some(1440)),
            format("2160", "webm", Some("av1"), None, Some(2160)),
        ];
        for (quality, expected) in [
            (VideoQuality::P1080, "1440-last"),
            (VideoQuality::P1440, "1440-last"),
            (VideoQuality::Best, "2160"),
        ] {
            assert_eq!(
                select_video_formats(&formats, quality, true)
                    .unwrap()
                    .format_spec,
                expected
            );
            assert_eq!(
                select_video_formats(&formats[..1], quality, true)
                    .unwrap()
                    .format_spec,
                "unknown"
            );
            assert!(select_video_formats(&[], quality, true).is_err());
        }
    }

    #[test]
    fn missing_progress_numbers_remain_optional() {
        for value in ["", "NA", "na", "None", "none", "-1", "18446744073709551616"] {
            assert_eq!(optional_u64(value), None);
        }
        assert_eq!(optional_u64(" 42 "), Some(42));
        assert_eq!(optional_u64("0"), Some(0));
    }

    #[test]
    fn creates_a_non_conflicting_name() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("clip.mp4"), b"").unwrap();
        assert_eq!(
            unique_output_path(directory.path(), "clip", "mp4"),
            directory.path().join("clip (1).mp4")
        );
    }
}
