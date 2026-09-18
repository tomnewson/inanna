# Inanna
## Install
For normal use: [Download Latest Release](https://github.com/tomnewson/inanna/releases)

For development: See build instructions below.

## Description
A small Windows and Apple Silicon macOS app for downloading videos from websites. It uses yt-dlp and ffmpeg to produce editor-friendly files from their URL:

- Video: MP4 with H.264 video and AAC audio
- Audio only: M4A with AAC audio

It supports downloads from any video hosting platform that yt-dlp supports, including:
- YouTube
- TikTok
- Instagram
- Pinterest
- Many more! See all [here](https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md).

## Automatic download retries

Inanna retries a failed download up to **three times after the initial attempt**. Each retry checks formats again to refresh media URLs, while retaining staged partial downloads for yt-dlp to resume when possible.

- YouTube HTTP 403 and “Sign in to confirm you’re not a bot”: retry immediately, then wait 10 and 20 seconds.
- HTTP 429: retry immediately, then wait 60 and 120 seconds.
- HTTP 408, 500, 502, 503, 504 and explicit connection timeout/reset/abort, remote disconnect, or temporary DNS failures: retry immediately, then wait 10 and 20 seconds.
- Other failures (including private/deleted/age-restricted videos, missing formats, disk errors and conversion failures) are not automatically retried.

These rules inspect the final yt-dlp error diagnostic, since its general failure exit status does not identify the cause. yt-dlp’s own request/fragment retries still apply within each attempt. Persistent blocks may require action beyond retrying; see the [yt-dlp FAQ](https://github.com/yt-dlp/yt-dlp/wiki/FAQ#http-error-429-too-many-requests-or-402-payment-required).

The first retry shows “Retrying now (retry 1 of 3)…”. During subsequent waits, the existing progress bar becomes indeterminate and the status shows, for example, “YouTube temporarily blocked the request. Retrying in 10s (retry 2 of 3)…”, counting down each second. Cancel remains available. Only the final failure shows an error and diagnostic details: “The download failed after 3 automatic retries. Try again later.”

## Video Conversion
For video conversion, FFmpeg reads the source dimensions and average frame rate and applies the [YouTube SDR encoding guidance](https://support.google.com/youtube/answer/1722171). GPU encoders use the target and maximum; the CRF-based CPU fallback uses the same maximum. High frame rate means 48 fps or greater. Values are target/maximum Mbps:

| Resolution | Standard frame rate | High frame rate |
| --- | ---: | ---: |
| 8K | 80/160 | 120/240 |
| 4K | 35/45 | 53/68 |
| 1440p | 16/16 | 24/24 |
| 1080p | 8/8 | 12/12 |
| 720p | 5/5 | 7.5/7.5 |
| 480p | 2.5/2.5 | 4/4 |
| 360p or lower | 1/1 | 1.5/1.5 |

Portrait video is classified by its shorter dimension, so 1080x1920 uses the 1080p tier.

## Architecture

The C# Avalonia process owns the window and operating-system integration. It starts the Rust backend without a console window and exchanges versioned, newline-delimited JSON over redirected standard input and output. Media bytes never cross this protocol. Rust owns tool updates, validation, downloads, transcoding, cancellation, configuration, and logs.

The sidecar protocol is private to matching application releases. Standard output is reserved for protocol messages, and submitted video URLs are not written to normal application logs.

## Updates

Installed builds check this repository's GitHub Releases at startup. When an application update is available, the interface can download it and restart into the new version. Release packages are produced with Velopack; delta packages are used when a compatible previous release is available. Development and portable builds do not attempt to update themselves.

Application code and managed tools have separate update lifecycles. Replacing the application does not replace the managed tools or settings.

The backend checks these official release sources at startup:

- yt-dlp nightly builds
- FFmpeg builds for yt-dlp on Windows; Shaka Project static FFmpeg builds on macOS arm64
- Deno stable releases

## Build

Install the stable Rust toolchain and .NET 10 SDK. Windows builds additionally need the MSVC build tools. Restore and test both parts:

```powershell
dotnet restore Inanna.slnx
cargo test --all-targets --locked
dotnet test Inanna.slnx --no-restore -m:1 --disable-build-servers
```

Build both development executables, place the Rust sidecar beside the frontend, and start the app:

```powershell
./scripts/run-dev.ps1
```

On an Apple Silicon Mac, use the native development script instead:

```bash
./scripts/run-dev.sh
```

Build the Windows backend and publish the self-contained Avalonia frontend:

```powershell
$buildVersion = (git describe --tags --abbrev=0 --match "v[0-9]*").Substring(1)
$env:INANNA_VERSION = $buildVersion
cargo build --release --target x86_64-pc-windows-msvc --locked
dotnet publish src/dotnet/Inanna.App/Inanna.App.csproj `
  --configuration Release --runtime win-x64 --self-contained true `
  -p:PublishSingleFile=true -p:IncludeNativeLibrariesForSelfExtract=true `
  -p:PublishTrimmed=false --output dist/inanna
Copy-Item target/x86_64-pc-windows-msvc/release/inanna-backend.exe dist/inanna/
```

Build an ad-hoc-signed, self-contained macOS arm64 app bundle and portable ZIP on an Apple Silicon Mac. The full Xcode 26 toolchain must be active so the layered Icon Composer source can be compiled into the bundle:

```bash
./scripts/build-macos-arm64.sh
open 'dist/macos-arm64/Inanna.app'
```

## Privacy and security

- The backend passes URLs directly to yt-dlp without a command shell.
- It does not write submitted URLs to its normal log file.
- It ignores external yt-dlp configuration files.
- It does not support cookies, accounts, playlists, or private-video login in the MVP.
- Avalonia starts only the backend shipped beside the frontend and terminates its owned process during shutdown.

Only download media when you have permission to do so. The user is responsible for applicable terms and copyright law.

## Licence

This program is free software under GNU GPL version 3 only. See `COPYING`.

Avalonia, .NET, the .NET Community Toolkit, and Velopack use their respective permissive licences. Downloaded tools have their own licences. Available third-party licence files are stored in each managed tool directory; see `THIRD_PARTY.md` for the project-level summary.
