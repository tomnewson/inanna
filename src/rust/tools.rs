use crate::{
    config::{self, retry_transient_windows_file_operation},
    model::ActiveToolset,
    platform::ToolPlatform,
};
use fs2::FileExt;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::{io::AsyncWriteExt, process::Command};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const YT_DLP_RELEASE_API: &str =
    "https://api.github.com/repos/yt-dlp/yt-dlp-nightly-builds/releases/latest";
const DENO_RELEASE_API: &str = "https://api.github.com/repos/denoland/deno/releases/latest";
const UPDATE_CHECK_TIMESTAMP_FILE: &str = "last-update-check.txt";
const MINIMUM_UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

pub type ToolProgress = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("network request failed: {0}")]
    Network(#[from] reqwest::Error),
    #[error("file operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("release metadata is invalid: {0}")]
    Metadata(String),
    #[error("archive is invalid: {0}")]
    Archive(#[from] zip::result::ZipError),
    #[error("checksum verification failed for {0}")]
    Checksum(String),
    #[error("tool validation failed: {0}")]
    Validation(String),
    #[error("another process is updating the tools")]
    UpdateLocked,
    #[error("tool installation was cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    updated_at: String,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedRelease {
    etag: Option<String>,
    release: GithubRelease,
}

impl GithubRelease {
    fn asset(&self, name: &str) -> Result<GithubAsset, ToolError> {
        self.assets
            .iter()
            .find(|asset| asset.name == name)
            .cloned()
            .ok_or_else(|| ToolError::Metadata(format!("release asset {name:?} is missing")))
    }
}

#[derive(Debug, Clone)]
struct RemoteComponent {
    version: String,
    archive: GithubAsset,
    companion: Option<GithubAsset>,
    checksums: Option<GithubAsset>,
}

#[derive(Debug, Clone)]
pub struct UpdatePlan {
    yt_dlp: RemoteComponent,
    ffmpeg: RemoteComponent,
    deno: RemoteComponent,
    pub update_yt_dlp: bool,
    pub update_ffmpeg: bool,
    pub update_deno: bool,
}

impl UpdatePlan {
    pub fn has_updates(&self) -> bool {
        self.update_yt_dlp || self.update_ffmpeg || self.update_deno
    }

    pub fn required_download_bytes(&self) -> u64 {
        [
            (self.update_yt_dlp, self.yt_dlp.archive.size),
            (self.update_ffmpeg, self.ffmpeg.archive.size),
            (
                self.update_ffmpeg,
                self.ffmpeg.companion.as_ref().map_or(0, |asset| asset.size),
            ),
            (self.update_deno, self.deno.archive.size),
        ]
        .into_iter()
        .filter_map(|(needed, size)| needed.then_some(size))
        .sum()
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.update_yt_dlp {
            parts.push(format!("yt-dlp {}", self.yt_dlp.version));
        }
        if self.update_ffmpeg {
            parts.push(format!("FFmpeg {}", short_version(&self.ffmpeg.version)));
        }
        if self.update_deno {
            parts.push(format!("Deno {}", self.deno.version));
        }
        let mib = self.required_download_bytes() as f64 / 1_048_576.0;
        format!("{} ({mib:.0} MiB download)", parts.join(", "))
    }
}

pub struct ToolManager {
    root: PathBuf,
    platform: ToolPlatform,
    client: reqwest::Client,
    release_cache: tokio::sync::Mutex<HashMap<String, CachedRelease>>,
}

impl ToolManager {
    pub fn new(root: PathBuf, platform: ToolPlatform) -> Result<Self, ToolError> {
        fs::create_dir_all(root.join("tools"))?;
        fs::create_dir_all(root.join("staging"))?;
        let client = reqwest::Client::builder()
            .user_agent(format!("inanna/{}", crate::application_version()))
            .connect_timeout(std::time::Duration::from_secs(8))
            .build()?;
        let release_cache = match config::read_json(&root.join("release-cache.json")) {
            Ok(cache) => cache.unwrap_or_default(),
            Err(error) => {
                tracing::warn!(%error, "could not read the release cache; using an empty cache");
                HashMap::new()
            }
        };
        Ok(Self {
            root,
            platform,
            client,
            release_cache: tokio::sync::Mutex::new(release_cache),
        })
    }

    pub fn load_active(&self) -> Result<Option<ActiveToolset>, ToolError> {
        let path = self.root.join("active-tools.json");
        let mut active = match config::read_json::<ActiveToolset>(&path) {
            Ok(Some(active)) => active,
            Ok(None) => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                tracing::warn!(%error, "ignoring an invalid active tool manifest");
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        if active.platform != self.platform.id {
            return Ok(None);
        }
        if active.directory.is_relative() {
            active.directory = self.root.join(&active.directory);
        }
        if !self.validate_paths(&active) {
            return Ok(None);
        }
        Ok(Some(active))
    }

    pub async fn check_updates(
        &self,
        active: Option<&ActiveToolset>,
    ) -> Result<Option<UpdatePlan>, ToolError> {
        let timestamp_path = self.root.join(UPDATE_CHECK_TIMESTAMP_FILE);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if active.is_some() && !update_check_is_due(&timestamp_path, now) {
            return Ok(None);
        }
        fs::write(&timestamp_path, now.to_string())?;

        let (yt, ffmpeg, deno) = tokio::try_join!(
            self.release(YT_DLP_RELEASE_API),
            self.release(self.platform.ffmpeg_release_api),
            self.release(DENO_RELEASE_API),
        )?;

        let yt_component = RemoteComponent {
            version: yt.tag_name.clone(),
            archive: yt.asset(self.platform.yt_dlp_asset)?,
            companion: None,
            checksums: Some(yt.asset(self.platform.yt_dlp_checksums)?),
        };
        let ffmpeg_archive = ffmpeg.asset(self.platform.ffmpeg_asset)?;
        let ffmpeg_component = RemoteComponent {
            version: ffmpeg_archive.updated_at.clone(),
            archive: ffmpeg_archive,
            companion: self
                .platform
                .ffprobe_asset
                .map(|name| ffmpeg.asset(name))
                .transpose()?,
            checksums: self
                .platform
                .ffmpeg_checksums
                .map(|name| ffmpeg.asset(name))
                .transpose()?,
        };
        let deno_component = RemoteComponent {
            version: deno.tag_name.clone(),
            archive: deno.asset(self.platform.deno_asset)?,
            companion: None,
            checksums: Some(deno.asset(self.platform.deno_checksums)?),
        };

        Ok(Some(UpdatePlan {
            update_yt_dlp: active.is_none_or(|a| a.yt_dlp_version != yt_component.version),
            update_ffmpeg: active.is_none_or(|a| a.ffmpeg_version != ffmpeg_component.version),
            update_deno: active.is_none_or(|a| a.deno_version != deno_component.version),
            yt_dlp: yt_component,
            ffmpeg: ffmpeg_component,
            deno: deno_component,
        }))
    }

    pub async fn install(
        &self,
        plan: &UpdatePlan,
        active: Option<&ActiveToolset>,
        progress: ToolProgress,
        cancel: CancellationToken,
    ) -> Result<ActiveToolset, ToolError> {
        let lock_path = self.root.join("update.lock");
        let lock = File::create(lock_path)?;
        lock.try_lock_exclusive()
            .map_err(|_| ToolError::UpdateLocked)?;

        let stage = self.root.join("staging").join(Uuid::new_v4().to_string());
        fs::create_dir_all(&stage)?;
        let result = self
            .install_into(&stage, plan, active, progress, &cancel)
            .await;
        if result.is_err() {
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }

    async fn install_into(
        &self,
        stage: &Path,
        plan: &UpdatePlan,
        active: Option<&ActiveToolset>,
        progress: ToolProgress,
        cancel: &CancellationToken,
    ) -> Result<ActiveToolset, ToolError> {
        ensure_not_cancelled(cancel)?;
        if plan.update_yt_dlp {
            progress("Downloading yt-dlp nightly…".into());
            let destination = stage.join(&self.platform.layout.yt_dlp);
            self.download(&plan.yt_dlp.archive, &destination, cancel)
                .await?;
            self.verify_from_asset(
                &destination,
                &plan.yt_dlp.archive,
                &plan.yt_dlp.checksums,
                self.platform.yt_dlp_asset,
            )
            .await?;
        } else {
            copy_active(active, &self.platform.layout.yt_dlp, stage)?;
        }

        ensure_not_cancelled(cancel)?;
        if plan.update_ffmpeg {
            progress("Downloading FFmpeg and ffprobe…".into());
            if let Some(ffprobe) = &plan.ffmpeg.companion {
                let ffmpeg_path = stage.join(&self.platform.layout.ffmpeg);
                self.download(&plan.ffmpeg.archive, &ffmpeg_path, cancel)
                    .await?;
                self.verify_from_asset(
                    &ffmpeg_path,
                    &plan.ffmpeg.archive,
                    &plan.ffmpeg.checksums,
                    self.platform.ffmpeg_asset,
                )
                .await?;

                let ffprobe_path = stage.join(&self.platform.layout.ffprobe);
                self.download(ffprobe, &ffprobe_path, cancel).await?;
                self.verify_from_asset(
                    &ffprobe_path,
                    ffprobe,
                    &plan.ffmpeg.checksums,
                    self.platform.ffprobe_asset.unwrap_or("ffprobe"),
                )
                .await?;
            } else {
                let archive = stage.join("ffmpeg.zip");
                self.download(&plan.ffmpeg.archive, &archive, cancel)
                    .await?;
                self.verify_from_asset(
                    &archive,
                    &plan.ffmpeg.archive,
                    &plan.ffmpeg.checksums,
                    self.platform.ffmpeg_asset,
                )
                .await?;
                let stage_owned = stage.to_path_buf();
                let layout = self.platform.layout.clone();
                tokio::task::spawn_blocking(move || {
                    extract_ffmpeg(&archive, &stage_owned, &layout)
                })
                .await
                .map_err(|error| ToolError::Validation(error.to_string()))??;
            }
        } else {
            copy_active(active, &self.platform.layout.ffmpeg, stage)?;
            copy_active(active, &self.platform.layout.ffprobe, stage)?;
            copy_optional_active(active, Path::new("licences/ffmpeg-license.txt"), stage)?;
        }

        ensure_not_cancelled(cancel)?;
        if plan.update_deno {
            progress("Downloading Deno…".into());
            let archive = stage.join("deno.zip");
            self.download(&plan.deno.archive, &archive, cancel).await?;
            self.verify_from_asset(
                &archive,
                &plan.deno.archive,
                &plan.deno.checksums,
                self.platform.deno_asset,
            )
            .await?;
            let stage_owned = stage.to_path_buf();
            let layout = self.platform.layout.clone();
            tokio::task::spawn_blocking(move || extract_deno(&archive, &stage_owned, &layout))
                .await
                .map_err(|error| ToolError::Validation(error.to_string()))??;
        } else {
            copy_active(active, &self.platform.layout.deno, stage)?;
            copy_optional_active(active, Path::new("licences/deno-license.txt"), stage)?;
        }

        ensure_not_cancelled(cancel)?;
        for temporary in [stage.join("ffmpeg.zip"), stage.join("deno.zip")] {
            let _ = fs::remove_file(temporary);
        }

        for executable in [
            stage.join(&self.platform.layout.yt_dlp),
            stage.join(&self.platform.layout.ffmpeg),
            stage.join(&self.platform.layout.ffprobe),
            stage.join(&self.platform.layout.deno),
        ] {
            make_executable(&executable)?;
        }

        progress("Validating downloaded tools…".into());
        smoke_test(&stage.join(&self.platform.layout.yt_dlp), &["--version"]).await?;
        smoke_test(&stage.join(&self.platform.layout.ffmpeg), &["-version"]).await?;
        smoke_test(&stage.join(&self.platform.layout.ffprobe), &["-version"]).await?;
        smoke_test(&stage.join(&self.platform.layout.deno), &["--version"]).await?;

        let id = toolset_id(
            self.platform.id,
            &plan.yt_dlp.version,
            &plan.ffmpeg.version,
            &plan.deno.version,
        );
        let final_directory = self.root.join("tools").join(&id);
        retry_transient_windows_file_operation(|| fs::rename(stage, &final_directory))?;

        let relative_directory = PathBuf::from("tools").join(&id);
        let stored = ActiveToolset {
            id,
            yt_dlp_version: plan.yt_dlp.version.clone(),
            ffmpeg_version: plan.ffmpeg.version.clone(),
            deno_version: plan.deno.version.clone(),
            directory: relative_directory,
            platform: self.platform.id.into(),
            yt_dlp_path: self.platform.layout.yt_dlp.clone(),
            ffmpeg_path: self.platform.layout.ffmpeg.clone(),
            ffprobe_path: self.platform.layout.ffprobe.clone(),
            deno_path: self.platform.layout.deno.clone(),
        };
        config::write_json_atomic(&self.root.join("active-tools.json"), &stored)?;

        let mut active = stored;
        active.directory = self.root.join(&active.directory);
        self.cleanup_old_toolsets(&active.directory);
        Ok(active)
    }

    async fn release(&self, url: &str) -> Result<GithubRelease, ToolError> {
        let cached = self.release_cache.lock().await.get(url).cloned();
        let mut request = self.client.get(url).timeout(Duration::from_secs(20));
        if let Some(etag) = cached.as_ref().and_then(|entry| entry.etag.as_ref()) {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = request.send().await?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return cached.map(|entry| entry.release).ok_or_else(|| {
                ToolError::Metadata("server returned 304 without cached metadata".into())
            });
        }
        let response = response.error_for_status()?;
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let release: GithubRelease = response.json().await?;
        let mut cache = self.release_cache.lock().await;
        cache.insert(
            url.to_owned(),
            CachedRelease {
                etag,
                release: release.clone(),
            },
        );
        config::write_json_atomic(&self.root.join("release-cache.json"), &*cache)?;
        Ok(release)
    }

    async fn download(
        &self,
        asset: &GithubAsset,
        destination: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), ToolError> {
        let response = self
            .client
            .get(&asset.browser_download_url)
            .send()
            .await?
            .error_for_status()?;
        let mut stream = response.bytes_stream();
        let mut file = tokio::fs::File::create(destination).await?;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Err(ToolError::Cancelled),
                chunk = stream.next() => match chunk {
                    Some(chunk) => file.write_all(&chunk?).await?,
                    None => break,
                }
            }
        }
        file.flush().await?;
        Ok(())
    }

    async fn verify_from_asset(
        &self,
        file: &Path,
        asset: &GithubAsset,
        checksums: &Option<GithubAsset>,
        expected_name: &str,
    ) -> Result<(), ToolError> {
        let expected = if let Some(checksums) = checksums {
            let text = self
                .client
                .get(&checksums.browser_download_url)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            checksum_for(&text, expected_name).ok_or_else(|| {
                ToolError::Metadata(format!("no checksum exists for {expected_name}"))
            })?
        } else {
            asset_sha256(asset).ok_or_else(|| {
                ToolError::Metadata(format!(
                    "release metadata has no SHA-256 digest for {expected_name}"
                ))
            })?
        };
        let path = file.to_path_buf();
        let actual = tokio::task::spawn_blocking(move || sha256_file(&path))
            .await
            .map_err(|error| ToolError::Validation(error.to_string()))??;
        if !actual.eq_ignore_ascii_case(&expected) {
            return Err(ToolError::Checksum(expected_name.into()));
        }
        Ok(())
    }

    fn validate_paths(&self, active: &ActiveToolset) -> bool {
        [
            active.yt_dlp(),
            active.ffmpeg(),
            active.ffprobe(),
            active.deno(),
        ]
        .into_iter()
        .all(|path| path.is_file())
    }

    fn cleanup_old_toolsets(&self, current: &Path) {
        let Ok(entries) = fs::read_dir(self.root.join("tools")) else {
            return;
        };
        let mut directories: Vec<_> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir() && entry.path() != current)
            .collect();
        directories.sort_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok());
        while directories.len() > 1 {
            let entry = directories.remove(0);
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn update_check_is_due(path: &Path, now: u64) -> bool {
    let Ok(value) = fs::read_to_string(path) else {
        return true;
    };
    let Ok(last_check) = value.trim().parse::<u64>() else {
        return true;
    };
    last_check > now || now - last_check >= MINIMUM_UPDATE_CHECK_INTERVAL.as_secs()
}

fn asset_sha256(asset: &GithubAsset) -> Option<String> {
    let digest = asset.digest.as_deref()?.strip_prefix("sha256:")?;
    is_sha256(digest).then(|| digest.to_ascii_lowercase())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn ensure_not_cancelled(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError::Cancelled)
    } else {
        Ok(())
    }
}

fn copy_active(
    active: Option<&ActiveToolset>,
    relative: &Path,
    stage: &Path,
) -> Result<(), ToolError> {
    let active = active
        .ok_or_else(|| ToolError::Validation(format!("missing cached {}", relative.display())))?;
    let source = active.directory.join(relative);
    let destination = stage.join(relative);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)?;
    Ok(())
}

fn copy_optional_active(
    active: Option<&ActiveToolset>,
    relative: &Path,
    stage: &Path,
) -> Result<(), ToolError> {
    let Some(active) = active else { return Ok(()) };
    let source = active.directory.join(relative);
    if source.exists() {
        let destination = stage.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
    }
    Ok(())
}

fn checksum_for(contents: &str, expected_name: &str) -> Option<String> {
    contents
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let hash = fields.next()?;
            let name = fields.next()?.trim_start_matches('*');
            (name == expected_name && is_sha256(hash)).then(|| hash.to_ascii_lowercase())
        })
        .or_else(|| checksum_from_get_file_hash(contents, expected_name))
}

fn checksum_from_get_file_hash(contents: &str, expected_name: &str) -> Option<String> {
    let mut algorithm = None;
    let mut hash = None;
    let mut path = None;

    for line in contents.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "algorithm" => algorithm = Some(value.trim()),
            "hash" => hash = Some(value.trim()),
            "path" => path = Some(value.trim()),
            _ => {}
        }
    }

    let hash = hash?;
    let file_name = path?.rsplit(['/', '\\']).next()?;
    (algorithm?.eq_ignore_ascii_case("SHA256") && file_name == expected_name && is_sha256(hash))
        .then(|| hash.to_ascii_lowercase())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn extract_ffmpeg(
    archive: &Path,
    stage: &Path,
    layout: &crate::platform::ToolLayout,
) -> Result<(), ToolError> {
    let file = File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)?;
    let licence_dir = stage.join("licences");
    fs::create_dir_all(&licence_dir)?;
    let mut found_ffmpeg = false;
    let mut found_ffprobe = false;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let Some(enclosed) = entry.enclosed_name() else {
            return Err(ToolError::Validation(
                "FFmpeg ZIP contains an unsafe path".into(),
            ));
        };
        let Some(name) = enclosed.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let destination = if name.eq_ignore_ascii_case(
            layout
                .ffmpeg
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("ffmpeg"),
        ) {
            found_ffmpeg = true;
            Some(stage.join(&layout.ffmpeg))
        } else if name.eq_ignore_ascii_case(
            layout
                .ffprobe
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("ffprobe"),
        ) {
            found_ffprobe = true;
            Some(stage.join(&layout.ffprobe))
        } else {
            match name.to_ascii_lowercase().as_str() {
                "license.txt" | "copying" => Some(licence_dir.join("ffmpeg-license.txt")),
                _ => None,
            }
        };
        if let Some(destination) = destination {
            let mut output = File::create(destination)?;
            io::copy(&mut entry, &mut output)?;
            output.flush()?;
        }
    }
    if !found_ffmpeg || !found_ffprobe {
        return Err(ToolError::Validation(
            "FFmpeg ZIP does not contain the expected ffmpeg and ffprobe executables".into(),
        ));
    }
    Ok(())
}

fn extract_deno(
    archive: &Path,
    stage: &Path,
    layout: &crate::platform::ToolLayout,
) -> Result<(), ToolError> {
    let file = File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)?;
    let mut found = false;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let Some(enclosed) = entry.enclosed_name() else {
            return Err(ToolError::Validation(
                "Deno ZIP contains an unsafe path".into(),
            ));
        };
        let Some(name) = enclosed.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.eq_ignore_ascii_case(
            layout
                .deno
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("deno"),
        ) {
            let mut output = File::create(stage.join(&layout.deno))?;
            io::copy(&mut entry, &mut output)?;
            output.flush()?;
            found = true;
        } else if name.eq_ignore_ascii_case("LICENSE.md") {
            fs::create_dir_all(stage.join("licences"))?;
            let mut output = File::create(stage.join("licences/deno-license.txt"))?;
            io::copy(&mut entry, &mut output)?;
        }
    }
    if !found {
        return Err(ToolError::Validation(
            "Deno ZIP does not contain the expected executable".into(),
        ));
    }
    Ok(())
}

async fn smoke_test(executable: &Path, arguments: &[&str]) -> Result<(), ToolError> {
    let output = Command::new(executable)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|error| {
            ToolError::Validation(format!("could not start {}: {error}", executable.display()))
        })?;
    if !output.status.success() {
        return Err(ToolError::Validation(format!(
            "{} returned {}",
            executable.display(),
            output.status
        )));
    }
    Ok(())
}

fn short_version(value: &str) -> String {
    value.chars().take(19).collect()
}

fn safe_id(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn toolset_id(platform: &str, yt_dlp: &str, ffmpeg: &str, deno: &str) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        platform,
        safe_id(yt_dlp),
        safe_id(&short_version(ffmpeg)),
        safe_id(deno),
        Uuid::new_v4()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_invalid_active_tool_manifest() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("active-tools.json"), "{}").unwrap();
        let manager =
            ToolManager::new(root.path().to_path_buf(), ToolPlatform::windows_x64()).unwrap();

        assert!(manager.load_active().unwrap().is_none());
    }

    #[test]
    fn parses_common_checksum_formats() {
        let hash = "a".repeat(64);
        assert_eq!(
            checksum_for(&format!("{hash}  yt-dlp.exe\n"), "yt-dlp.exe"),
            Some(hash.clone())
        );
        assert_eq!(
            checksum_for(&format!("{hash} *yt-dlp.exe\n"), "yt-dlp.exe"),
            Some(hash)
        );
    }

    #[test]
    fn parses_deno_get_file_hash_format() {
        let hash = "ABCDEF0123456789".repeat(4);
        let contents = format!(
            "\r\nAlgorithm : SHA256\r\nHash      : {hash}\r\nPath      : C:\\a\\deno\\target\\release\\deno-x86_64-pc-windows-msvc.zip\r\n\r\n"
        );
        assert_eq!(
            checksum_for(&contents, "deno-x86_64-pc-windows-msvc.zip"),
            Some(hash.to_ascii_lowercase())
        );
    }

    #[test]
    fn rejects_get_file_hash_for_wrong_file_or_algorithm() {
        let hash = "c".repeat(64);
        let wrong_file = format!("Algorithm : SHA256\nHash : {hash}\nPath : C:\\temp\\other.zip\n");
        assert_eq!(
            checksum_for(&wrong_file, "deno-x86_64-pc-windows-msvc.zip"),
            None
        );

        let wrong_algorithm = format!(
            "Algorithm : SHA512\nHash : {hash}\nPath : C:\\temp\\deno-x86_64-pc-windows-msvc.zip\n"
        );
        assert_eq!(
            checksum_for(&wrong_algorithm, "deno-x86_64-pc-windows-msvc.zip"),
            None
        );
    }

    #[test]
    fn rejects_wrong_checksum_name() {
        let hash = "b".repeat(64);
        assert_eq!(
            checksum_for(&format!("{hash} other.exe\n"), "yt-dlp.exe"),
            None
        );
    }

    #[test]
    fn reads_sha256_from_github_asset_metadata() {
        let hash = "d".repeat(64);
        let asset = GithubAsset {
            name: "ffmpeg-osx-arm64".into(),
            browser_download_url: "https://example.invalid/ffmpeg".into(),
            size: 1,
            updated_at: "2026-01-01T00:00:00Z".into(),
            digest: Some(format!("sha256:{hash}")),
        };

        assert_eq!(asset_sha256(&asset), Some(hash));
    }

    #[test]
    fn repeated_installations_use_distinct_toolset_directories() {
        let first = toolset_id(
            "windows-x64",
            "2026.08.20",
            "2026-08-20T12:00:00Z",
            "v2.4.0",
        );
        let second = toolset_id(
            "windows-x64",
            "2026.08.20",
            "2026-08-20T12:00:00Z",
            "v2.4.0",
        );

        assert_ne!(first, second);
        assert!(first.starts_with("windows-x64-2026-08-20-2026-08-20T12-00-00-v2-4-0-"));
    }
}
