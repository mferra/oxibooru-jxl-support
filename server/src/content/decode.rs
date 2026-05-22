use crate::api::error::{ApiError, ApiResult};
use crate::config::Config;
use crate::content::{self, flash};
use crate::model::enums::{MimeType, PostType};
use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel};
use image::{DynamicImage, ImageFormat, ImageReader, Limits, RgbImage, RgbaImage};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use swf::Tag;
use tracing::error;

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
    let iter = FfmpegCommand::new_with_path(FFMPEG_PATH)
        .input(path_str)
        .args(["-c", "copy", "-t", "0", "-f", "null", "-"])
        .spawn()?
        .iter()
        .map_err(|err| ApiError::FfmpegError(err.into_boxed_dyn_error()))?;

    let mut has_audio = None;
    let mut errors = Vec::new();
    for event in iter {
        match event {
            FfmpegEvent::ParsedInputStream(stream) if stream.is_audio() => {
                has_audio = Some(true);
            }
            FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, err) => errors.push(err),
            _ => {}
        }
    }
    if has_audio.is_none() && !errors.is_empty() {
        return Err(ApiError::FfmpegError(errors.join("; ").into()));
    }
    Ok(has_audio.unwrap_or(false))
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

const FFMPEG_PATH: &str = "/opt/app/ffmpeg";

/// Decodes a representative frame of the image or video at the given `path`.
fn ffmpeg_frame(path: &Path, post_type: PostType) -> ApiResult<Option<DynamicImage>> {
    let filter = match post_type {
        PostType::Image | PostType::Animation => "format=rgba",
        PostType::Video | PostType::Flash => "thumbnail,format=rgb24",
    };

    let path_str = path.to_string_lossy();
    let iter = FfmpegCommand::new_with_path(FFMPEG_PATH)
        .input(&path_str)
        .args(["-vf", filter, "-frames:v", "1", "-f", "rawvideo", "-"])
        .spawn()?
        .iter()
        .map_err(|err| ApiError::FfmpegError(err.into_boxed_dyn_error()))?;

    let mut frame = None;
    let mut errors = Vec::new();
    for event in iter {
        match event {
            FfmpegEvent::OutputFrame(f) => {
                let buffer_len = f.data.len();
                let extracted_frame = if filter.contains("rgba") {
                    RgbaImage::from_raw(f.width, f.height, f.data).map(DynamicImage::ImageRgba8)
                } else {
                    RgbImage::from_raw(f.width, f.height, f.data).map(DynamicImage::ImageRgb8)
                }
                .ok_or(ApiError::FrameBufferMismatch(f.width, f.height, buffer_len))?;
                frame = Some(extracted_frame);
            }
            FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, err) => errors.push(err),
            _ => {}
        }
    }
    if frame.is_none() && !errors.is_empty() {
        return Err(ApiError::FfmpegError(errors.join("; ").into()));
    }
    Ok(frame)
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

/// Returns maximum decoded image size.
fn image_reader_limits() -> Limits {
    const GB: u64 = 1024_u64.pow(3);

    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(4 * GB);
    limits
}
