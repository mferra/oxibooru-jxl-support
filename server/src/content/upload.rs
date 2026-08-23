use crate::api::error::{ApiError, ApiResult};
use crate::config::Config;
use crate::filesystem::{self, Directory};
use crate::model::enums::MimeType;
use axum::extract::multipart::Multipart;
use axum::extract::rejection::{JsonRejection, MissingJsonContentType};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use strum::IntoStaticStr;
use uuid::Uuid;

pub const MAX_UPLOAD_SIZE: usize = 4 * 1024_usize.pow(3);

/// A token that represents a file that's been streamed to disk during upload.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct UploadToken {
    token: String,
    #[serde(skip)]
    mime_type: MimeType,
}

impl UploadToken {
    pub fn new(mime_type: MimeType) -> Self {
        let token = format!("{}.{}", Uuid::new_v4(), mime_type.extension());
        Self { token, mime_type }
    }

    pub fn mime_type(&self) -> MimeType {
        self.mime_type
    }

    pub fn path(&self, config: &Config) -> PathBuf {
        config.path(Directory::TemporaryUploads).join(&self.token)
    }
}

impl<'de> Deserialize<'de> for UploadToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let token = String::deserialize(deserializer)?;
        if token.contains('/') || token.contains('\\') {
            return Err(serde::de::Error::custom("invalid upload token"));
        }

        let (_uuid, extension) = token.split_once('.').unwrap_or((&token, ""));
        let mime_type = MimeType::from_extension(extension).map_err(serde::de::Error::custom)?;
        Ok(Self { token, mime_type })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum PartName {
    Content,
    Thumbnail,
    Avatar,
}

pub struct Body<const N: usize> {
    pub files: [Option<UploadToken>; N],
    pub metadata: Option<Vec<u8>>,
}

/// Attempts to extract given `fields` and optional JSON "metadata" field from given `form_data`.
pub async fn extract<const N: usize>(
    config: &Config,
    mut form_data: Multipart,
    fields: [PartName; N],
) -> ApiResult<Body<N>> {
    let mut files = std::array::from_fn(|_| None);
    let mut metadata = None;
    while let Some(field) = form_data.next_field().await? {
        let position = fields
            .iter()
            .map(Into::<&str>::into)
            .position(|name| field.name() == Some(name));
        if position.is_none() && field.name() != Some("metadata") {
            continue;
        }

        // Ensure metadata is JSON
        if position.is_none() && field.content_type() != Some("application/json") {
            return Err(ApiError::JsonRejection(JsonRejection::MissingJsonContentType(
                MissingJsonContentType::default(),
            )));
        }

        match position {
            // MIME type is inferred from the file's magic bytes, not caller-supplied metadata.
            Some(index) => {
                files[index] = filesystem::save_uploaded_file(config, field).await.map(Some)?;
            }
            None => metadata = field.bytes().await.map(|bytes| bytes.to_vec()).map(Some)?,
        }
    }
    Ok(Body { files, metadata })
}
