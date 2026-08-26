use crate::api::error::{ApiError, ApiResult};
use crate::config::Config;
use crate::content::{self, ffmpeg, flash};
use crate::model::enums::{MimeType, PostType};
use ffmpeg_sidecar::child::FfmpegChild;
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel};
use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbImage, RgbaImage};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::str::FromStr;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};
use swf::Tag;
use tracing::error;

/// Infers a [`MimeType`] from the magic bytes at the start of a file's contents,
/// rather than trusting a client-supplied extension or `Content-Type` header.
pub fn infer_mime_type(prefix: &[u8]) -> ApiResult<MimeType> {
    let kind = infer::get(prefix).ok_or(ApiError::MissingContentType)?;
    MimeType::from_str(kind.mime_type()).map_err(Box::from).map_err(ApiError::from)
}

/// Decodes a JPEG XL file at `file_path` using jxl-oxide.
fn decode_jxl(file_path: &Path) -> ApiResult<DynamicImage> {
    let jxl = jxl_oxide::JxlImage::builder()
        .open(file_path)
        .map_err(|e| ApiError::FfmpegError(e.to_string().into()))?;

    let width = jxl.width();
    let height = jxl.height();
    let frame = jxl
        .render_frame(0)
        .map_err(|e| ApiError::FfmpegError(e.to_string().into()))?;

    // image_planar() → Vec<FrameBuffer>, one element per channel
    let fb = frame.image_planar();
    let num_channels = fb.len();

    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let npixels = (width * height) as usize;

    if num_channels >= 4 {
        let (r, g, b, a) = (fb[0].buf(), fb[1].buf(), fb[2].buf(), fb[3].buf());
        let mut bytes = Vec::with_capacity(npixels * 4);
        for i in 0..npixels {
            bytes.push(to_u8(r[i]));
            bytes.push(to_u8(g[i]));
            bytes.push(to_u8(b[i]));
            bytes.push(to_u8(a[i]));
        }
        RgbaImage::from_raw(width, height, bytes)
            .map(DynamicImage::ImageRgba8)
            .ok_or_else(|| ApiError::FfmpegError("JXL buffer size mismatch".into()))
    } else {
        let r = fb[0].buf();
        let g = if num_channels >= 2 { fb[1].buf() } else { r };
        let b = if num_channels >= 3 { fb[2].buf() } else { r };
        let mut bytes = Vec::with_capacity(npixels * 3);
        for i in 0..npixels {
            bytes.push(to_u8(r[i]));
            bytes.push(to_u8(g[i]));
            bytes.push(to_u8(b[i]));
        }
        RgbImage::from_raw(width, height, bytes)
            .map(DynamicImage::ImageRgb8)
            .ok_or_else(|| ApiError::FfmpegError("JXL buffer size mismatch".into()))
    }
}

/// Returns a representative image for the given content.
/// For images, this is simply the decoded image.
/// For videos, it is the first frame of the video.
/// For Flash media, it is the largest image that can be decoded from the Flash tags.
pub fn representative_image(config: &Config, file_path: &Path, mime_type: MimeType) -> ApiResult<DynamicImage> {
    match PostType::from(mime_type) {
        PostType::Image | PostType::Animation => image(file_path, mime_type),
        PostType::Video => ffmpeg_frame(file_path, PostType::Video).and_then(|frame| frame.ok_or(ApiError::EmptyVideo)),
        PostType::Flash => flash_image(config, file_path).and_then(|frame| frame.ok_or(ApiError::EmptySwf)),
    }
}

/// Returns if the video at `path` has an audio channel.
pub fn video_has_audio(path: &Path) -> ApiResult<bool> {
    let path_str = path.to_string_lossy();
    let mut command = FfmpegCommand::new_with_path(ffmpeg::PATH);
    command
        .input(path_str)
        .args(["-c", "copy", "-t", "0", "-f", "null", "-"]);

    let description = format!("probing {} for audio", path.display());
    let run = run_ffmpeg(&description, &mut command, None::<bool>, |has_audio, event| {
        if let FfmpegEvent::ParsedInputStream(stream) = event
            && stream.is_audio()
        {
            *has_audio = Some(true);
        }
    })?;

    let FfmpegRun { state, errors } = run;
    match state {
        Some(has_audio) => Ok(has_audio),
        None if !errors.is_empty() => Err(ApiError::FfmpegError(errors.join("; ").into())),
        None => Ok(false),
    }
}

/// Returns if the swf at `path` has audio.
pub fn swf_has_audio(path: &Path) -> ApiResult<bool> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let swf_buf = swf::decompress_swf(reader)?;
    let swf = swf::parse_swf(&swf_buf)?;

    Ok(swf.tags.iter().any(|tag| {
        matches!(
            tag,
            Tag::DefineButtonSound(_)
                | Tag::DefineSound(_)
                | Tag::SoundStreamBlock(_)
                | Tag::SoundStreamHead(_)
                | Tag::SoundStreamHead2(_)
                | Tag::StartSound(_)
                | Tag::StartSound2 { .. }
        )
    }))
}

/// Decodes a raw array of bytes into pixel data.
pub fn image(file_path: &Path, mime_type: MimeType) -> ApiResult<DynamicImage> {
    if mime_type == MimeType::Jxl {
        return decode_jxl(file_path);
    }
    if let Some(format) = mime_type.to_image_format() {
        let file = content::map_read_result(File::open(file_path))?;

        let mut reader = ImageReader::new(BufReader::new(file));
        reader.set_format(format);
        reader.limits(image_reader_limits());
        reader.decode().map_err(ApiError::from)
    } else {
        ffmpeg_frame(file_path, PostType::Image)?
            .ok_or(ApiError::FfmpegError(format!("Unable to decode {mime_type} image with FFmpeg").into()))
    }
}

/// Wall-clock limit for a single `FFmpeg` invocation, overridable with the `FFMPEG_TIMEOUT`
/// environment variable (in seconds).
///
/// `FFmpeg` has no timeout of its own, and a truncated or malformed file can leave it spinning
/// or blocked forever. Without a limit, one bad post wedges whichever worker thread picked it
/// up for the rest of an admin task, and enough of them stall the task completely.
static FFMPEG_TIMEOUT: LazyLock<Duration> = LazyLock::new(|| ffmpeg::env_timeout("FFMPEG_TIMEOUT", 120));

/// The outcome of an `FFmpeg` run: whatever the event handler accumulated, along with the
/// error-level log lines `FFmpeg` emitted while producing it.
struct FfmpegRun<S> {
    state: S,
    errors: Vec<String>,
}

/// Runs `command` to completion, passing every `FFmpeg` event to `handler`.
///
/// The event stream is consumed on a worker thread so that the child handle stays reachable
/// here: if `FFmpeg` hasn't finished within [`FFMPEG_TIMEOUT`] it is killed, and either way it
/// is waited on. That wait is what keeps exited `FFmpeg` processes from piling up as zombies,
/// since dropping the handle neither kills nor reaps the process.
///
/// `description` is a participle phrase naming the work ("extracting a frame from ..."), and
/// shows up in the timeout logs.
fn run_ffmpeg<S, F>(
    description: &str,
    command: &mut FfmpegCommand,
    initial_state: S,
    mut handler: F,
) -> ApiResult<FfmpegRun<S>>
where
    S: Send + 'static,
    F: FnMut(&mut S, FfmpegEvent) + Send + 'static,
{
    let mut child = command.spawn()?;
    let events = child
        .iter()
        .map_err(|err| ApiError::FfmpegError(err.into_boxed_dyn_error()))?;

    let errors = Arc::new(Mutex::new(Vec::new()));
    let thread_errors = Arc::clone(&errors);
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut state = initial_state;
        for event in events {
            if let FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, message) = &event {
                let mut errors = thread_errors.lock().unwrap_or_else(PoisonError::into_inner);
                if errors.len() < ffmpeg::MAX_CAPTURED_LOG_LINES {
                    errors.push(message.clone());
                }
            }
            handler(&mut state, event);
        }
        // The receiver is gone if we already timed out, in which case the state is moot.
        // Killing the child ends the iteration above, so this thread never outlives it.
        let _ = sender.send(state);
    });

    match receiver.recv_timeout(*FFMPEG_TIMEOUT) {
        Ok(state) => {
            reap(&mut child, description);
            Ok(FfmpegRun {
                state,
                errors: take_errors(&errors),
            })
        }
        Err(RecvTimeoutError::Timeout) => {
            let timeout = FFMPEG_TIMEOUT.as_secs();
            let logged = take_errors(&errors);
            let log_tail = if logged.is_empty() {
                "It logged no errors before hanging.".to_owned()
            } else {
                format!("Last FFmpeg errors: {}", logged.join("; "))
            };
            error!("FFmpeg timed out after {timeout}s while {description}; killing it. {log_tail}");

            kill(&mut child, description);
            reap(&mut child, description);
            Err(ApiError::FfmpegError(
                format!("FFmpeg timed out after {timeout}s while {description}").into(),
            ))
        }
        Err(RecvTimeoutError::Disconnected) => {
            kill(&mut child, description);
            reap(&mut child, description);
            Err(ApiError::FfmpegError(
                format!("FFmpeg event reader died while {description}").into(),
            ))
        }
    }
}

/// Sends `SIGKILL` (or the platform equivalent) to a stuck `FFmpeg` process.
fn kill(child: &mut FfmpegChild, description: &str) {
    if let Err(err) = child.kill() {
        error!("Cannot kill stuck FFmpeg process while {description}: {err}");
    }
}

/// Waits on `child` so that the exited process leaves the process table instead of lingering
/// as a zombie. Dropping the handle does neither, which is what let stray `ffmpeg` processes
/// accumulate for the lifetime of the server.
///
/// `FFmpeg`'s event stream has already ended by the time this runs, so the process has nothing
/// left to say and should be on its way out; one that is still alive after a short grace
/// period is stuck, and gets killed rather than blocking the caller.
fn reap(child: &mut FfmpegChild, description: &str) {
    const GRACE_PERIOD: Duration = Duration::from_secs(5);
    const POLL_INTERVAL: Duration = Duration::from_millis(25);

    let deadline = Instant::now() + GRACE_PERIOD;
    loop {
        match child.as_inner_mut().try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => break,
            Err(err) => {
                error!("Cannot wait on FFmpeg process while {description}: {err}");
                return;
            }
        }
    }

    error!("FFmpeg is still running after its output ended while {description}; killing it");
    kill(child, description);
    if let Err(err) = child.wait() {
        error!("Cannot reap FFmpeg process while {description}: {err}");
    }
}

fn take_errors(errors: &Mutex<Vec<String>>) -> Vec<String> {
    let mut errors = errors.lock().unwrap_or_else(PoisonError::into_inner);
    std::mem::take(&mut errors)
}

/// Decodes a representative frame of the image or video at the given `path`.
///
/// This reads the raw frame off `FFmpeg`'s stdout directly rather than through
/// [`ffmpeg_sidecar`]'s event stream. The sidecar only starts draining stdout once it has
/// parsed the output stream description, and it gives up on that description when the frame
/// rate is printed in `FFmpeg`'s abbreviated form ("1k fps"), which is common for variable
/// frame rate recordings. `FFmpeg` then blocks forever writing a frame far larger than the
/// pipe buffer into a pipe nobody is reading.
fn ffmpeg_frame(path: &Path, post_type: PostType) -> ApiResult<Option<DynamicImage>> {
    let (filter, channels) = match post_type {
        PostType::Image | PostType::Animation => ("format=rgba", 4),
        PostType::Video | PostType::Flash => ("thumbnail,format=rgb24", 3),
    };

    let description = format!("extracting a frame from {}", path.display());
    let output = ffmpeg::run(
        &description,
        &[
            "-i",
            &path.to_string_lossy(),
            "-vf",
            filter,
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-",
        ],
        *FFMPEG_TIMEOUT,
    )?;

    if output.stdout.is_empty() {
        return Ok(None);
    }

    let byte_count = output.stdout.len();
    let (width, height) = frame_dimensions(&output.log_tail, byte_count, channels).ok_or_else(|| {
        let message = format!(
            "Cannot determine the dimensions of the {byte_count} byte frame produced while \
             {description}. {}",
            ffmpeg::summarize(&output.log_tail)
        );
        ApiError::FfmpegError(message.into())
    })?;

    let mut data = output.stdout;
    data.truncate(width as usize * height as usize * channels);

    let frame = if channels == 4 {
        RgbaImage::from_raw(width, height, data).map(DynamicImage::ImageRgba8)
    } else {
        RgbImage::from_raw(width, height, data).map(DynamicImage::ImageRgb8)
    };
    frame
        .map(Some)
        .ok_or(ApiError::FrameBufferMismatch(width, height, byte_count))
}

/// Recovers the geometry of a raw frame from `FFmpeg`'s log.
///
/// `FFmpeg` reports this nowhere except its human-readable stream lines, so it is scraped out
/// of the `648x384` in those. Every candidate is checked against the number of bytes actually
/// received, so a wrong match would have to divide the frame exactly; that check, rather than
/// the shape of the log line, is what makes this reliable.
fn frame_dimensions(log_tail: &[String], byte_count: usize, channels: usize) -> Option<(u32, u32)> {
    // Later lines describe the output, which is what was written to stdout.
    log_tail
        .iter()
        .rev()
        .flat_map(|line| dimension_candidates(line))
        .find(|&(width, height)| {
            let frame_size = width as usize * height as usize * channels;
            frame_size > 0 && byte_count >= frame_size && byte_count.is_multiple_of(frame_size)
        })
}

/// Yields every `WIDTHxHEIGHT` token in `line`, along with false positives such as the `0x...`
/// of a codec tag, which the caller is expected to reject.
fn dimension_candidates(line: &str) -> impl Iterator<Item = (u32, u32)> + '_ {
    line.split(|character: char| !character.is_ascii_digit() && character != 'x')
        .filter_map(|token| {
            let (width, height) = token.split_once('x')?;
            Some((width.parse().ok()?, height.parse().ok()?))
        })
}

/// Search swf tags for the largest decodable image
fn flash_image(config: &Config, path: &Path) -> ApiResult<Option<DynamicImage>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let swf_buf = swf::decompress_swf(reader)?;
    let swf = swf::parse_swf(&swf_buf)?;

    let encoding_table = swf
        .tags
        .iter()
        .find_map(|tag| {
            if let Tag::JpegTables(table) = tag {
                Some(table)
            } else {
                None
            }
        })
        .copied();
    let mut images: Vec<_> = swf
        .tags
        .iter()
        .filter_map(|tag| match tag {
            Tag::DefineBits { id: _, jpeg_data } => {
                let jpeg_data = flash::glue_tables_to_jpeg(jpeg_data, encoding_table);
                Some(image::load_from_memory_with_format(&jpeg_data, ImageFormat::Jpeg).map_err(flash::Error::from))
            }
            Tag::DefineBitsLossless(bits) => flash::decode_define_bits_lossless(bits).transpose(),
            Tag::DefineBitsJpeg2 { id: _, jpeg_data } => Some(flash::decode_define_bits_jpeg(jpeg_data, None)),
            Tag::DefineBitsJpeg3(bits) => Some(flash::decode_define_bits_jpeg(bits.data, Some(bits.alpha_data))),
            _ => None,
        })
        .filter_map(|image_result| match image_result {
            Ok(image) => Some(image),
            Err(err) => {
                error!("Failure to decode flash image for reason: {err}");
                None
            }
        })
        .collect();

    // Some Flash files only have video frames, which are hard to decode.
    // So, we feed to ffmpeg and see if it can decode a representaive frame.
    if let Ok(Some(frame)) = ffmpeg_frame(path, PostType::Flash) {
        images.push(frame);
    }

    // Sort images in order of decreasing effective width after cropping for thumbnails
    images.sort_by_key(|image| {
        let (thumbnail_width, thumbnail_height) = config.thumbnails.post_dimensions();

        // Condition is equivalent to image_aspect_ratio > config_thumbnail_aspect_ratio
        let effective_width = if image.width() * thumbnail_height > thumbnail_width * image.height() {
            image.height() * thumbnail_width / thumbnail_height
        } else {
            image.width()
        };
        u32::MAX - effective_width
    });
    Ok(images.into_iter().next())
}

/// Returns the post type based on file content for formats where [`PostType::from`] can be
/// wrong: not every GIF is animated, and some AVIF are. For everything else, this just
/// defers to the mime type.
pub fn detect_post_type(file_path: &Path, mime_type: MimeType) -> ApiResult<PostType> {
    let is_animated = match mime_type {
        MimeType::Avif => Some(avif_is_animated(file_path)?),
        MimeType::Gif => Some(gif_is_animated(file_path)?),
        _ => None,
    };
    Ok(match is_animated {
        Some(true) => PostType::Animation,
        Some(false) => PostType::Image,
        None => PostType::from(mime_type),
    })
}

/// Returns `true` if the GIF at `path` has more than one frame.
fn gif_is_animated(path: &Path) -> ApiResult<bool> {
    let file = content::map_read_result(File::open(path))?;
    let mut decoder = GifDecoder::new(BufReader::new(file))?;
    decoder.set_limits(image_reader_limits())?;

    // GIF doesn't store a frame count, so just check for a second frame.
    let mut frames = decoder.into_frames();
    Ok(frames.nth(1).is_some())
}

/// Uses `FFmpeg` to determine if the AVIF at `path` contains more than one frame.
fn avif_is_animated(path: &Path) -> ApiResult<bool> {
    let path_str = path.to_string_lossy();
    let mut command = FfmpegCommand::new_with_path(ffmpeg::PATH);
    command.input(&path_str);

    let description = format!("counting the video streams of {}", path.display());
    let video_stream_count = run_ffmpeg(&description, &mut command, 0usize, |count, event| {
        if let FfmpegEvent::ParsedInputStream(stream) = event
            && stream.is_video()
        {
            *count += 1;
        }
    })?
    .state;

    for stream_index in 0..video_stream_count {
        let mut command = FfmpegCommand::new_with_path(ffmpeg::PATH);
        command
            .input(&path_str)
            .args([
                "-map",
                &format!("0:v:{stream_index}"),
                "-frames:v",
                "2",
                "-vf",
                "scale=1:1:flags=neighbor",
            ])
            .rawvideo();

        let description = format!("checking stream {stream_index} of {} for animation", path.display());
        let run = run_ffmpeg(&description, &mut command, 0u32, |frames, event| {
            if let FfmpegEvent::OutputFrame(_) = event {
                *frames += 1;
            }
        })?;

        if run.state > 1 {
            return Ok(true);
        } else if run.state == 0 && !run.errors.is_empty() {
            return Err(ApiError::FfmpegError(run.errors.join("; ").into()));
        }
    }
    Ok(false)
}

/// Returns maximum decoded image size.
fn image_reader_limits() -> Limits {
    const MB: u64 = 1024_u64.pow(2);

    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(256 * MB);
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits
}

#[cfg(test)]
mod test {
    use super::*;

    /// FFmpeg's real output for a 640x480 VP8 file whose frame rate prints as "1k fps",
    /// which is what `ffmpeg_sidecar` fails to parse.
    const RAW_VIDEO_LOG: &[&str] = &[
        "Input #0, matroska,webm, from 'hang.webm':",
        "  Duration: 00:00:00.15, start: 0.000000, bitrate: 1711 kb/s",
        "  Stream #0:0: Video: vp8, yuv420p(tv, progressive), 640x480, SAR 1:1 DAR 4:3, 1k fps, 1k tbr, 1k tbn",
        "Stream mapping:",
        "  Stream #0:0 -> #0:0 (vp8 (native) -> rawvideo (native))",
        "Output #0, rawvideo, to 'pipe:':",
        "  Stream #0:0: Video: rawvideo (RGB[24] / 0x18424752), rgb24(pc, gbr/unknown/unknown, \
         progressive), 640x480 [SAR 1:1 DAR 4:3], q=2-31, 7372800 kb/s, 1k fps, 1k tbn",
        "[out#0/rawvideo @ 0x59561a36b880] video:900KiB audio:0KiB muxing overhead: 0.000000%",
        "frame=    1 fps=0.0 q=-0.0 Lsize=     900KiB time=00:00:00.05 speed=0.982x",
    ];

    #[test]
    fn frame_dimensions_from_ffmpeg_log() {
        let log: Vec<String> = RAW_VIDEO_LOG.iter().map(|line| (*line).to_owned()).collect();
        assert_eq!(frame_dimensions(&log, 640 * 480 * 3, 3), Some((640, 480)));
    }

    #[test]
    fn frame_dimensions_rejects_codec_tags() {
        // `0x18424752` and `0x59561a36b880` look like dimensions but can't divide the frame.
        let candidates: Vec<_> = RAW_VIDEO_LOG
            .iter()
            .flat_map(|line| dimension_candidates(line))
            .collect();
        assert!(candidates.contains(&(0, 18424752)), "the codec tag is a candidate");
        assert_eq!(frame_dimensions(&["  0x18424752".to_owned()], 640 * 480 * 3, 3), None);
    }

    #[test]
    fn frame_dimensions_requires_a_whole_number_of_frames() {
        let log = ["  Stream #0:0: Video: rawvideo, rgb24, 640x480".to_owned()];
        assert_eq!(frame_dimensions(&log, 640 * 480 * 3, 3), Some((640, 480)));
        assert_eq!(frame_dimensions(&log, 640 * 480 * 3 - 1, 3), None, "truncated frame");
        assert_eq!(frame_dimensions(&log, 640 * 480 * 4, 4), Some((640, 480)), "rgba");
        assert_eq!(frame_dimensions(&log, 0, 3), None, "no output at all");
    }
}
