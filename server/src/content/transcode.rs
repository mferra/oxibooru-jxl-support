use crate::api::error::ApiResult;
use crate::config::AnimationFormat;
use crate::content::ffmpeg;
use crate::model::enums::MimeType;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::Duration;
use tracing::{info, warn};

/// Wall-clock limit for a single transcoding run, overridable with the
/// `FFMPEG_TRANSCODE_TIMEOUT` environment variable (in seconds).
///
/// Far larger than the limit on probing and frame extraction in [`decode`](super::decode):
/// re-encoding a long GIF to AV1 is legitimately slow, so this is only here to catch an
/// `FFmpeg` that is wedged, not one that is merely busy.
static TRANSCODE_TIMEOUT: LazyLock<Duration> =
    LazyLock::new(|| ffmpeg::env_timeout("FFMPEG_TRANSCODE_TIMEOUT", 600));

/// Probes whether the FFmpeg binary at startup supports AV1 encoding via libaom-av1.
/// Returns false (with a warning) if the binary is missing or the encoder is absent.
pub fn probe_av1_support() -> bool {
    match Command::new(ffmpeg::PATH).args(["-encoders", "-v", "quiet"]).output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let supported = stdout.contains("libaom-av1");
            if supported {
                info!("AV1 encoding (libaom-av1) is available");
            } else {
                warn!("AV1 encoding (libaom-av1) not found in FFmpeg; GIF animation transcoding will use WebP");
            }
            supported
        }
        Err(err) => {
            warn!("Failed to probe FFmpeg encoders: {err}; GIF animation transcoding will use WebP");
            false
        }
    }
}

/// Returns whether the WebP file at `path` is animated by scanning for an ANIM chunk.
/// Returns false on any I/O error (non-WebP files, truncated reads, etc.).
pub fn webp_is_animated(path: &Path) -> bool {
    webp_is_animated_inner(path).unwrap_or(false)
}

fn webp_is_animated_inner(path: &Path) -> std::io::Result<bool> {
    let mut file = File::open(path)?;

    // RIFF <size> WEBP
    let mut header = [0u8; 12];
    file.read_exact(&mut header)?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WEBP" {
        return Ok(false);
    }

    // Walk the chunk list looking for "ANIM"
    let mut chunk_header = [0u8; 8];
    loop {
        if file.read_exact(&mut chunk_header).is_err() {
            break;
        }
        if &chunk_header[0..4] == b"ANIM" {
            return Ok(true);
        }
        // Skip chunk data; chunks are padded to even byte boundaries
        let size = u32::from_le_bytes(chunk_header[4..8].try_into().unwrap()) as u64;
        let padded = size + (size & 1);
        if file.seek(SeekFrom::Current(padded as i64)).is_err() {
            break;
        }
    }
    Ok(false)
}

/// Builds a unique sibling path for an FFmpeg output temp file.
/// e.g. `/tmp/uploads/abc123.gif` → `/tmp/uploads/abc123_tc.webp`
fn tc_output_path(input: &Path, ext: &str) -> PathBuf {
    let stem = input
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let dir = input.parent().unwrap_or(Path::new("."));
    dir.join(format!("{stem}_tc.{ext}"))
}

/// Transcodes a GIF to animated WebP using FFmpeg's libwebp_anim encoder.
fn gif_to_webp(path: &Path) -> ApiResult<Vec<u8>> {
    let out = tc_output_path(path, "webp");
    let description = format!("transcoding {} to WebP", path.display());
    let result = ffmpeg::run(
        &description,
        &[
            "-y",
            "-i",
            &path.to_string_lossy(),
            "-vcodec",
            "libwebp_anim",
            "-loop",
            "0",
            "-lossless",
            "0",
            "-quality",
            "80",
            "-compression_level",
            "6",
            &out.to_string_lossy(),
        ],
        *TRANSCODE_TIMEOUT,
    );

    if let Err(err) = result {
        // A killed or failed FFmpeg leaves a partial file behind.
        let _ = std::fs::remove_file(&out);
        return Err(err);
    }
    let bytes = std::fs::read(&out)?;
    let _ = std::fs::remove_file(&out);
    Ok(bytes)
}

/// Transcodes a GIF to AV1 MP4 using FFmpeg's libaom-av1 encoder.
fn gif_to_av1(path: &Path) -> ApiResult<Vec<u8>> {
    let out = tc_output_path(path, "mp4");
    let description = format!("transcoding {} to AV1", path.display());
    let result = ffmpeg::run(
        &description,
        &[
            "-y",
            "-i",
            &path.to_string_lossy(),
            "-c:v",
            "libaom-av1",
            "-crf",
            "35",
            "-b:v",
            "0",
            "-cpu-used",
            "6",
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
            &out.to_string_lossy(),
        ],
        *TRANSCODE_TIMEOUT,
    );

    if let Err(err) = result {
        // A killed or failed FFmpeg leaves a partial file behind.
        let _ = std::fs::remove_file(&out);
        return Err(err);
    }
    let bytes = std::fs::read(&out)?;
    let _ = std::fs::remove_file(&out);
    Ok(bytes)
}

/// Transcodes a GIF animation and returns `(bytes, target_mime)`.
///
/// - `Smallest`: attempts both WebP and AV1; keeps whichever is smaller.
/// - `Webp`: always produces animated WebP.
/// - `Av1`: produces AV1 MP4, falls back to WebP if AV1 fails.
///
/// If `av1_supported` is false, AV1 is never attempted regardless of `animation_format`.
pub fn transcode_gif(
    path: &Path,
    animation_format: AnimationFormat,
    av1_supported: bool,
) -> ApiResult<(Vec<u8>, MimeType)> {
    let webp = gif_to_webp(path)?;

    if !animation_format.use_av1(av1_supported) {
        return Ok((webp, MimeType::Webp));
    }

    match gif_to_av1(path) {
        Ok(av1) if av1.len() < webp.len() => {
            info!(
                "AV1 ({} B) < WebP ({} B) for {:?}; storing as AV1",
                av1.len(),
                webp.len(),
                path.file_name().unwrap_or_default()
            );
            Ok((av1, MimeType::Mp4))
        }
        Ok(av1) => {
            info!(
                "WebP ({} B) <= AV1 ({} B) for {:?}; storing as WebP",
                webp.len(),
                av1.len(),
                path.file_name().unwrap_or_default()
            );
            Ok((webp, MimeType::Webp))
        }
        Err(err) => {
            warn!("AV1 transcoding failed ({err}); falling back to WebP");
            Ok((webp, MimeType::Webp))
        }
    }
}
