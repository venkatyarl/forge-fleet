//! Shell out to `yt-dlp` + `ffmpeg` to fetch media + frames for a social post.
//!
//! # External dependencies
//!
//! This module depends on two binaries being available on `PATH`:
//!
//! - **yt-dlp** — handles Twitter/X, Instagram, TikTok, YouTube. Install via
//!   `pip install yt-dlp` or `brew install yt-dlp`.
//! - **ffmpeg** — used to sample one frame every 5 seconds from video media.
//!   Install via `brew install ffmpeg` (macOS) or `apt install ffmpeg` (Linux).
//!
//! Missing binaries return a helpful error — we do NOT try to install them
//! for the operator.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use super::platform::detect_platform;

/// Max number of frames we extract per video, regardless of length.
const MAX_FRAMES: usize = 20;

/// One media artifact fetched from the post.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    pub kind: String, // image | video | audio | frame
    pub local_path: String,
    pub mime: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_count: Option<usize>,
}

/// Result of the fetch pass — ready to hand off to the analyzer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchedPost {
    pub platform: String,
    pub author: Option<String>,
    pub caption: Option<String>,
    pub media_items: Vec<MediaItem>,
    pub raw_metadata: serde_json::Value,
}

/// Fetch the post's media + metadata into `out_dir`.
///
/// Strategy:
///   1. `yt-dlp --write-info-json --write-description -o "<out>/%(id)s.%(ext)s" <url>`
///   2. Parse the resulting `.info.json` for author/caption.
///   3. For each downloaded video, extract up to `MAX_FRAMES` frames at 1fps/5s.
pub async fn fetch(url: &str, out_dir: &Path) -> Result<FetchedPost> {
    ensure_binary("yt-dlp").await?;
    tokio::fs::create_dir_all(out_dir)
        .await
        .with_context(|| format!("create out_dir {}", out_dir.display()))?;

    let platform = detect_platform(url);

    // yt-dlp: download best-quality media + metadata JSON + description.
    let output_template = out_dir.join("%(id)s.%(ext)s");
    let status = Command::new("yt-dlp")
        .arg("--write-info-json")
        .arg("--write-description")
        .arg("--no-warnings")
        .arg("--no-playlist")
        .arg("-o")
        .arg(&output_template)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("spawn yt-dlp")?;
    if !status.success() {
        return Err(anyhow!("yt-dlp exited with status {status}"));
    }

    // Find the .info.json — yt-dlp writes exactly one per fetched item
    // when --no-playlist is set.
    let info_path = find_first_with_suffix(out_dir, ".info.json").await?;
    let info_raw = tokio::fs::read_to_string(&info_path)
        .await
        .with_context(|| format!("read info_json {}", info_path.display()))?;
    let info: serde_json::Value =
        serde_json::from_str(&info_raw).context("parse yt-dlp info.json")?;

    let author = info
        .get("uploader")
        .and_then(|v| v.as_str())
        .or_else(|| info.get("channel").and_then(|v| v.as_str()))
        .map(|s| s.to_string());
    let caption = info
        .get("description")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| info.get("title").and_then(|v| v.as_str()))
        .map(|s| s.to_string());

    // Discover the primary media file yt-dlp wrote.
    let mut media_items = collect_media_files(out_dir).await?;

    // For any video we see, sample frames at 1 frame / 5s up to MAX_FRAMES.
    let mut frame_items = Vec::new();
    for item in &mut media_items {
        if item.kind == "video" {
            let n = extract_frames(Path::new(&item.local_path), out_dir).await?;
            item.frame_count = Some(n);
            for i in 1..=n {
                let fp = out_dir.join(format!("frame_{i:03}.jpg"));
                if let Ok(md) = tokio::fs::metadata(&fp).await {
                    frame_items.push(MediaItem {
                        kind: "frame".into(),
                        local_path: fp.to_string_lossy().into_owned(),
                        mime: "image/jpeg".into(),
                        bytes: md.len(),
                        frame_count: None,
                    });
                }
            }
        }
    }
    media_items.extend(frame_items);

    Ok(FetchedPost {
        platform: platform.as_str().to_string(),
        author,
        caption,
        media_items,
        raw_metadata: info,
    })
}

/// Ensure a binary is on `PATH`, or return a descriptive error.
async fn ensure_binary(name: &str) -> Result<()> {
    let status = Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => {
            let hint = match name {
                "yt-dlp" => "install yt-dlp: pip install yt-dlp",
                "ffmpeg" => {
                    "install ffmpeg: brew install ffmpeg (macOS) or apt install ffmpeg (Linux)"
                }
                other => return Err(anyhow!("{other} not found on PATH")),
            };
            Err(anyhow!("{name} not found on PATH — {hint}"))
        }
    }
}

/// Find the first file in `dir` whose name ends with `suffix`.
async fn find_first_with_suffix(dir: &Path, suffix: &str) -> Result<PathBuf> {
    let mut rd = tokio::fs::read_dir(dir)
        .await
        .with_context(|| format!("read_dir {}", dir.display()))?;
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name();
        if let Some(n) = name.to_str() {
            if n.ends_with(suffix) {
                return Ok(entry.path());
            }
        }
    }
    Err(anyhow!("no file with suffix {suffix} in {}", dir.display()))
}

/// Walk `dir` and classify each primary media file (video/image/audio),
/// skipping yt-dlp's sidecar files (`.info.json`, `.description`,
/// `.jpg` thumbnails smaller than 4 KB are likely thumbnails — we keep
/// them anyway as `image`).
async fn collect_media_files(dir: &Path) -> Result<Vec<MediaItem>> {
    let mut out = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let md = entry.metadata().await?;
        if !md.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Skip sidecars.
        if name.ends_with(".info.json")
            || name.ends_with(".description")
            || name.starts_with("frame_")
        {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let (kind, mime) = match ext.as_str() {
            "mp4" | "mov" | "mkv" | "webm" => ("video", "video/mp4"),
            "jpg" | "jpeg" => ("image", "image/jpeg"),
            "png" => ("image", "image/png"),
            "webp" => ("image", "image/webp"),
            "m4a" | "mp3" | "wav" | "ogg" => ("audio", "audio/mpeg"),
            _ => continue,
        };
        out.push(MediaItem {
            kind: kind.into(),
            local_path: path.to_string_lossy().into_owned(),
            mime: mime.into(),
            bytes: md.len(),
            frame_count: None,
        });
    }
    Ok(out)
}

/// Extract up to [`MAX_FRAMES`] JPEG frames at 1 frame / 5 seconds.
/// Returns the number of frames actually written.
async fn extract_frames(video_path: &Path, out_dir: &Path) -> Result<usize> {
    ensure_binary("ffmpeg").await?;
    let pattern = out_dir.join("frame_%03d.jpg");
    let status = Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(video_path)
        .arg("-vf")
        .arg("fps=1/5")
        .arg("-frames:v")
        .arg(MAX_FRAMES.to_string())
        .arg("-q:v")
        .arg("3")
        .arg(&pattern)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("spawn ffmpeg")?;
    if !status.success() {
        return Err(anyhow!("ffmpeg exited with {status}"));
    }
    // Count frames that actually landed.
    let mut count = 0usize;
    for i in 1..=MAX_FRAMES {
        let fp = out_dir.join(format!("frame_{i:03}.jpg"));
        if tokio::fs::metadata(&fp).await.is_ok() {
            count += 1;
        } else {
            break;
        }
    }
    Ok(count)
}
