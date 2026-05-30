use crate::api::error::ApiResult;
use crate::content::hash::{Checksum, Md5Checksum};
use crate::content::signature::COMPRESSED_SIGNATURE_LEN;
use crate::content::thumbnail::ThumbnailType;
use crate::content::upload::UploadToken;
use crate::content::{decode, encode, hash, signature, thumbnail, transcode};
use crate::extract::Ctx;
use crate::model::enums::{MimeType, PostFlag, PostFlags, PostType};
use crate::{content, filesystem};
use image::DynamicImage;
use image::error::LimitErrorKind;
use std::collections::VecDeque;
use tracing::warn;

/// Stores properties of content that are costly to compute (usually require reading/decoding entire file).
#[derive(Clone)]
pub struct CachedProperties {
    pub token: UploadToken,
    pub checksum: Checksum,
    pub md5_checksum: Md5Checksum,
    pub signature: [i64; COMPRESSED_SIGNATURE_LEN],
    pub thumbnail: DynamicImage,
    pub width: i32,
    pub height: i32,
    pub mime_type: MimeType,
    pub file_size: i64,
    pub flags: PostFlags,
    /// The semantic post type, which may differ from PostType::from(mime_type) for animated WebP
    /// and for GIF transcoded to MP4 AV1 (which becomes Video).
    pub post_type: PostType,
    /// DCT-based perceptual hash computed from the representative image before transcoding.
    pub phash: i64,
}

/// A simple ring buffer that stores [`CachedProperties`].
pub struct RingCache {
    data: VecDeque<(UploadToken, CachedProperties)>,
    max_size: usize,
}

impl RingCache {
    pub fn new(max_size: usize) -> Self {
        Self {
            data: VecDeque::new(),
            max_size,
        }
    }

    pub fn clear(&mut self) {
        self.data = VecDeque::new();
    }

    fn insert(&mut self, key: UploadToken, value: CachedProperties) {
        self.data.push_back((key, value));
        if self.data.len() > self.max_size {
            self.data.pop_front();
        }
    }

    fn remove(&mut self, key: &UploadToken) -> Option<CachedProperties> {
        self.data
            .iter()
            .position(|(cache_key, _)| cache_key == key)
            .and_then(|pos| self.data.remove(pos))
            .map(|(_, cache_value)| cache_value)
    }
}

/// Computes content properties and caches them in memory.
pub fn compute_properties(ctx: &Ctx, content_token: UploadToken) -> ApiResult<CachedProperties> {
    let properties = compute_properties_no_cache(ctx, content_token.clone())?;

    // Clone this here to make sure we aren't holding onto lock for longer than necessary
    let properties_copy = properties.clone();
    ctx.get_content_cache().insert(content_token, properties_copy);

    Ok(properties)
}

/// Returns cached properties of content or computes them if not in cache.
pub fn get_or_compute_properties(ctx: &Ctx, content_token: UploadToken) -> ApiResult<CachedProperties> {
    let maybe_properties = ctx.get_content_cache().remove(&content_token);
    match maybe_properties {
        Some(properties) => Ok(properties),
        None => compute_properties_no_cache(ctx, content_token),
    }
}

/// Computes content properties without storing them in cache.
fn compute_properties_no_cache(ctx: &Ctx, token: UploadToken) -> ApiResult<CachedProperties> {
    let temp_path = token.path(&ctx.config);
    let mime_type = token.mime_type();

    // Detect animated WebP before classifying post type — always active regardless of the
    // transcoding flag so the stored type is always correct.
    let is_animated_webp = mime_type == MimeType::Webp && transcode::webp_is_animated(&temp_path);
    let post_type = if is_animated_webp {
        PostType::Animation
    } else {
        PostType::from(mime_type)
    };

    let has_sound = match post_type {
        PostType::Image | PostType::Animation => false,
        PostType::Video => decode::video_has_audio(&temp_path)?,
        PostType::Flash => decode::swf_has_audio(&temp_path)?,
    };
    let flags = if has_sound {
        PostFlags::new_with(PostFlag::Sound)
    } else {
        PostFlags::new()
    };

    // Decode representative image for signature, thumbnail, and pHash computation (from original file).
    let image = decode::representative_image(&ctx.config, &temp_path, mime_type)?;
    let computed_signature = signature::compute(&image);
    let computed_thumbnail = thumbnail::create(&ctx.config, &image, ThumbnailType::Post);
    let computed_phash = hash::compute_phash(&image);
    let width = i32::try_from(image.width()).map_err(|_| LimitErrorKind::DimensionError)?;
    let height = i32::try_from(image.height()).map_err(|_| LimitErrorKind::DimensionError)?;

    // Optionally transcode; this replaces the temp file and recomputes token/mime/checksums.
    let (final_token, final_mime, final_post_type, file_size, checksum, md5_checksum) =
        if ctx.config.transcoding.enabled {
            maybe_transcode(ctx, token, mime_type, post_type, is_animated_webp, &image)?
        } else {
            let file_size = content::map_read_result(filesystem::file_size(&temp_path))?;
            let (checksum, md5_checksum) = content::map_read_result(hash::compute_checksums(&temp_path))?;
            (token, mime_type, post_type, file_size, checksum, md5_checksum)
        };

    Ok(CachedProperties {
        token: final_token,
        checksum,
        md5_checksum,
        signature: computed_signature,
        thumbnail: computed_thumbnail,
        width,
        height,
        mime_type: final_mime,
        file_size,
        flags,
        post_type: final_post_type,
        phash: computed_phash,
    })
}

/// Transcodes the uploaded content when applicable and writes it to a new temp file.
///
/// Returns `(token, mime_type, post_type, file_size, checksum, md5_checksum)`.
/// Checksums always correspond to the stored (possibly transcoded) file so that the
/// integrity check and duplicate detection work correctly.
fn maybe_transcode(
    ctx: &Ctx,
    token: UploadToken,
    mime_type: MimeType,
    post_type: PostType,
    is_animated_webp: bool,
    image: &DynamicImage,
) -> ApiResult<(UploadToken, MimeType, PostType, i64, Checksum, Md5Checksum)> {
    let temp_path = token.path(&ctx.config);
    let tc = &ctx.config.transcoding;

    let transcoded: Option<(Vec<u8>, MimeType)> = match (post_type, mime_type) {
        // Already JXL — nothing to do.
        (PostType::Image, MimeType::Jxl) => None,
        // Animated WebP — already the best animation format, leave it.
        (PostType::Animation, MimeType::Webp) if is_animated_webp => None,
        // GIF animation → animated WebP or AV1 MP4 (pick smaller when configured).
        (PostType::Animation, MimeType::Gif) => Some(transcode::transcode_gif(
            &temp_path,
            tc.animation_format,
            ctx.av1_supported,
        )?),
        // Any other static image → JXL at configured quality.
        (PostType::Image, _) => Some((encode::to_jxl(image, tc.image_quality)?, MimeType::Jxl)),
        // Video, Flash, and anything else: leave untouched.
        _ => None,
    };

    if let Some((bytes, new_mime)) = transcoded {
        // Write transcoded bytes to a new temp token so the file extension is correct.
        let new_token = UploadToken::new(new_mime);
        let new_path = new_token.path(&ctx.config);
        content::map_read_result(filesystem::create_parent_directories(&new_path))?;
        content::map_read_result(std::fs::write(&new_path, &bytes))?;

        // Remove the original temp file; non-fatal — the cleanup task handles leftovers.
        if let Err(err) = std::fs::remove_file(&temp_path) {
            warn!("Could not remove original temp file after transcoding: {err}");
        }

        let file_size = bytes.len() as i64;
        let (checksum, md5_checksum) = content::map_read_result(hash::compute_checksums(&new_path))?;

        // PostType::from(Webp) == Image, but a GIF transcoded to WebP is still an Animation.
        let new_post_type = if new_mime == MimeType::Webp && post_type == PostType::Animation {
            PostType::Animation
        } else {
            PostType::from(new_mime)
        };

        Ok((new_token, new_mime, new_post_type, file_size, checksum, md5_checksum))
    } else {
        // No transcoding: compute checksums of the original file.
        let file_size = content::map_read_result(filesystem::file_size(&temp_path))?;
        let (checksum, md5_checksum) = content::map_read_result(hash::compute_checksums(&temp_path))?;
        Ok((token, mime_type, post_type, file_size, checksum, md5_checksum))
    }
}
