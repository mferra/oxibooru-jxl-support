use crate::api::error::{ApiError, ApiResult};
use crate::app::Context;
use crate::content::cache::CachedProperties;
use crate::content::thumbnail::ThumbnailType;
use crate::content::upload::UploadToken;
use crate::extract::Ctx;
use image::DynamicImage;
use std::time::Duration;
use url::Url;

pub mod cache;
pub mod decode;
pub mod download;
pub mod encode;
mod flash;
pub mod hash;
pub mod signature;
pub mod thumbnail;
pub mod transcode;
pub mod upload;

/// Contains either the name of a file uploaded to the temporary uploads
/// directory or a url pointing to a file on the web.
///
/// Methods on this object consume it and will save the content to the
/// temporary uploads directory (if not already present) before operating on it.
/// This is because some operations such as video decoding require a path to the
/// content on disk.
pub enum Content {
    Token(UploadToken),
    Url(Url),
}

impl Content {
    /// Constructs a new [`Content`] from either an in-memory `direct_upload`, a `token` which represents
    /// a file in the temporary uploads directory, or a URL to download the content from.
    ///
    /// If multiple ways of retrieving content are given, the method of retrieving the content will
    /// be the first argument that is not [`None`].
    pub fn new(token: Option<UploadToken>, url: Option<Url>) -> Option<Self> {
        match (token, url) {
            (Some(token), _) => Some(Self::Token(token)),
            (None, Some(url)) => Some(Self::Url(url)),
            (None, None) => None,
        }
    }

    /// Saves content to temporary uploads directory and returns the name of the file written.
    pub async fn save(self, ctx: &Context) -> ApiResult<UploadToken> {
        match self {
            Self::Token(token) => Ok(token),
            Self::Url(url) => download::from_url(ctx, url).await,
        }
    }

    /// Computes thumbnail for uploaded content.
    pub async fn thumbnail(self, ctx: &Context, thumbnail_type: ThumbnailType) -> ApiResult<DynamicImage> {
        let token = self.save(ctx).await?;
        let temp_path = token.path(&ctx.config);
        tokio::task::block_in_place({
            || {
                decode::representative_image(&ctx.config, &temp_path, token.mime_type())
                    .map(|image| thumbnail::create(&ctx.config, &image, thumbnail_type))
            }
        })
    }

    /// Computes properties for uploaded content.
    pub async fn compute_properties(self, ctx: &Ctx) -> ApiResult<CachedProperties> {
        let token = self.save(ctx).await?;
        tokio::task::block_in_place(|| cache::compute_properties(ctx, token))
    }

    /// Retrieves content properties from cache or computes them if not present in cache.
    pub async fn get_or_compute_properties(self, ctx: &Ctx) -> ApiResult<CachedProperties> {
        let token = self.save(ctx).await?;
        tokio::task::block_in_place(|| cache::get_or_compute_properties(ctx, token))
    }
}

/// Reads a timeout in seconds from the `variable` environment variable, falling back to
/// `default_seconds`.
///
/// `FFmpeg` has no timeout of its own, so every invocation is given one; the ceiling differs
/// by an order of magnitude between probing a file and re-encoding it, hence the parameter.
fn env_timeout(variable: &str, default_seconds: u64) -> Duration {
    let seconds = std::env::var(variable)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&seconds| seconds > 0)
        .unwrap_or(default_seconds);
    Duration::from_secs(seconds)
}

fn map_read_result<T>(result: std::io::Result<T>) -> ApiResult<T> {
    result.map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ApiError::InvalidUploadToken
        } else {
            ApiError::from(err)
        }
    })
}
