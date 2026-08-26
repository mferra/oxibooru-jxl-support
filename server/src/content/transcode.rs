use crate::api::error::{ApiError, ApiResult};
use crate::config::AnimationFormat;
use crate::content;
use crate::model::enums::MimeType;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

const FFMPEG_PATH: &str = "/opt/app/ffmpeg";

/// Wall-clock limit for a single transcoding run, overridable with the
/// `FFMPEG_TRANSCODE_TIMEOUT` environment variable (in seconds).
///
/// Far larger than the limit on probing and frame extraction in [`decode`](super::decode):
/// re-encoding a long GIF to AV1 is legitimately slow, so this is only here to catch an
/// `FFmpeg` that is wedged, not one that is merely busy.
static TRANSCODE_TIMEOUT: LazyLock<Duration> =
    LazyLock::new(|| content::env_timeout("FFMPEG_TRANSCODE_TIMEOUT", 600));

/// Cap on the `FFmpeg` log lines kept per run, so a file that warns on every frame can't grow
/// an unbounded error message. The tail is kept rather than the head, since that is where a
/// fatal error shows up.
const MAX_CAPTURED_LOG_LINES: usize = 32;

/// Runs an `FFmpeg` command that writes its output to a file.
///
/// Unlike [`Command::status`], this cannot block forever: an `FFmpeg` that hasn't finished
/// within [`TRANSCODE_TIMEOUT`] is killed and reaped. Its stderr is captured either way and
/// included in the error, since a bare exit status says nothing about what went wrong.
///
/// `description` is a participle phrase naming the work ("transcoding ... to WebP").
fn run_ffmpeg(description: &str, args: &[&str]) -> ApiResult<()> {
    let mut child = Command::new(FFMPEG_PATH)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| ApiError::FfmpegError(Box::new(err)))?;

    // Drain stderr on a worker thread: leaving it unread deadlocks FFmpeg once the pipe
    // buffer fills, and its tail is what explains a failure.
    let stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut log_tail = VecDeque::new();
        if let Some(stderr) = stderr {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if log_tail.len() == MAX_CAPTURED_LOG_LINES {
                    log_tail.pop_front();
                }
                log_tail.push_back(line);
            }
        }
        log_tail
    });

    // Killing the child closes stderr, so the reader always finishes.
    let status = wait_with_timeout(&mut child, description);
    let log_tail = reader.join().unwrap_or_default();
    let log_tail = if log_tail.is_empty() {
        "FFmpeg logged nothing.".to_owned()
    } else {
        format!("FFmpeg output: {}", Vec::from(log_tail).join("; "))
    };

    match status {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(ApiError::FfmpegError(
            format!("FFmpeg exited with {status} while {description}. {log_tail}").into(),
        )),
        None => Err(ApiError::FfmpegError(
            format!(
                "FFmpeg was killed after failing to finish within {}s while {description}. {log_tail}",
                TRANSCODE_TIMEOUT.as_secs()
            )
            .into(),
        )),
    }
}

/// Waits for `child` to exit, killing it if it outlives [`TRANSCODE_TIMEOUT`]. Returns
/// [`None`] if it had to be killed.
///
/// The process is always waited on, which is what removes it from the process table; a killed
/// child that is never reaped lingers as a zombie for the lifetime of the server.
fn wait_with_timeout(child: &mut Child, description: &str) -> Option<ExitStatus> {
    const POLL_INTERVAL: Duration = Duration::from_millis(100);

    let deadline = Instant::now() + *TRANSCODE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                error!(
                    "FFmpeg timed out after {}s while {description}; killing it",
                    TRANSCODE_TIMEOUT.as_secs()
                );
                break;
            }
            Err(err) => {
                error!("Cannot wait on FFmpeg process while {description}: {err}; killing it");
                break;
            }
        }
    }

    if let Err(err) = child.kill() {
        error!("Cannot kill stuck FFmpeg process while {description}: {err}");
    }
    if let Err(err) = child.wait() {
        error!("Cannot reap FFmpeg process while {description}: {err}");
    }
    None
}

/// Probes whether the FFmpeg binary at startup supports AV1 encoding via libaom-av1.
/// Returns false (with a warning) if the binary is missing or the encoder is absent.
pub fn probe_av1_support() -> bool {
    match Command::new(FFMPEG_PATH).args(["-encoders", "-v", "quiet"]).output() {
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
    let result = run_ffmpeg(
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
    let result = run_ffmpeg(
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
