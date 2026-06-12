use crate::api::error::{ApiError, ApiResult};
use crate::app::Context;
use crate::content::cache;
use crate::content::hash::PostHash;
use crate::content::signature::{self, SignatureCache};
use crate::content::thumbnail::ThumbnailCategory;
use crate::content::upload::{MAX_UPLOAD_SIZE, UploadToken};
use crate::model::enums::{MimeType, PostSafety, PostType};
use crate::model::pool::{NewPool, Pool};
use crate::model::post::{NewPost, NewPostSignature, Post, PostSignature};
use crate::schema::{pool, pool_category, post, post_signature};
use crate::string::SmallString;
use crate::{filesystem, snapshot, update};
use diesel::{Connection, ExpressionMethods, Insertable, OptionalExtension, PgConnection, QueryDsl, RunQueryDsl};
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufReader, Read};
use std::iter::Peekable;
use std::path::Path;
use std::str::Chars;
use tracing::{error, info, warn};
use zip::ZipArchive;

/// Outcome of importing a comic archive as a pool.
pub struct ImportSummary {
    pub pool_id: i64,
    pub post_count: usize,
    pub matched_exact: usize,
    pub matched_similar: usize,
    pub created: usize,
    pub failed_pages: Vec<String>,
}

/// How an archive page was resolved to a post.
enum PageResolution {
    /// An existing post has the exact same content checksum.
    MatchedExact(i64),
    /// An existing post is visually similar within the configured threshold.
    MatchedSimilar(i64, f64),
    /// No existing post matched, so a new one was created.
    Created(i64),
}

/// Constructs a free-form import error without adding new [`ApiError`] variants.
fn import_error(message: String) -> ApiError {
    ApiError::FromStr(message.into())
}

/// Imports the comic archive (CBZ) at `archive_path` as a pool. Pages are
/// matched against existing posts by exact checksum first and perceptual
/// similarity second; pages without a match are uploaded as new posts through
/// the regular content pipeline. `is_cancelled` is polled between pages.
pub fn import_archive_as_pool(
    context: &Context,
    conn: &mut PgConnection,
    archive_path: &Path,
    pool_name: &str,
    safety: PostSafety,
    is_cancelled: &dyn Fn() -> bool,
) -> ApiResult<ImportSummary> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(BufReader::new(file))
        .map_err(|err| import_error(format!("Cannot read {} as a ZIP archive: {err}", archive_path.display())))?;

    // Collect still-image entries; the page order is the natural order of entry names.
    let mut pages: Vec<(usize, String, MimeType)> = Vec::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|err| import_error(format!("Cannot read archive entry {index}: {err}")))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_owned();
        let extension = Path::new(&name).extension().and_then(OsStr::to_str).unwrap_or("");
        let Ok(mime_type) = MimeType::from_extension(extension) else {
            info!("Skipping {name}: not a supported content format");
            continue;
        };
        if PostType::from(mime_type) != PostType::Image {
            info!("Skipping {name}: not a still image");
            continue;
        }
        if entry.size() > MAX_UPLOAD_SIZE as u64 {
            return Err(import_error(format!("Archive entry {name} is larger than the upload size limit")));
        }
        pages.push((index, name, mime_type));
    }
    if pages.is_empty() {
        return Err(import_error(format!("{} contains no supported images", archive_path.display())));
    }
    pages.sort_by(|(_, name_a, _), (_, name_b, _)| natural_order(name_a, name_b));
    info!("Found {} pages in {}", pages.len(), archive_path.display());

    let mut post_ids: Vec<i64> = Vec::new();
    let mut failed_pages: Vec<String> = Vec::new();
    let (mut exact_count, mut similar_count, mut created_count) = (0_usize, 0_usize, 0_usize);

    for (page_number, (index, name, mime_type)) in pages.iter().enumerate() {
        if is_cancelled() {
            return Err(import_error("Import was cancelled".to_owned()));
        }

        match import_page(context, conn, &mut archive, *index, *mime_type, safety) {
            Ok(resolution) => {
                let post_id = match resolution {
                    PageResolution::MatchedExact(post_id) => {
                        info!("Page {} ({name}): matched post {post_id} by checksum", page_number + 1);
                        exact_count += 1;
                        post_id
                    }
                    PageResolution::MatchedSimilar(post_id, distance) => {
                        info!(
                            "Page {} ({name}): matched post {post_id} by similarity (distance {distance:.4})",
                            page_number + 1
                        );
                        similar_count += 1;
                        post_id
                    }
                    PageResolution::Created(post_id) => {
                        info!("Page {} ({name}): created post {post_id}", page_number + 1);
                        created_count += 1;
                        post_id
                    }
                };
                if post_ids.contains(&post_id) {
                    warn!(
                        "Page {} ({name}): post {post_id} is already in the pool, skipping duplicate page",
                        page_number + 1
                    );
                } else {
                    post_ids.push(post_id);
                }
            }
            Err(err) => {
                error!("Page {} ({name}): {err}", page_number + 1);
                failed_pages.push(name.clone());
            }
        }
    }

    if post_ids.is_empty() {
        return Err(import_error("No pages could be resolved to posts; pool was not created".to_owned()));
    }

    // Create the pool with the resolved posts in page order.
    let file_name = archive_path.file_name().map(OsStr::to_string_lossy).unwrap_or_default();
    let description = format!("Imported from {file_name}");
    let creation_result: ApiResult<i64> = conn.transaction(|conn| {
        let (category_id, category): (i64, SmallString) = pool_category::table
            .select((pool_category::id, pool_category::name))
            .order(pool_category::id.asc())
            .first(conn)?;
        let new_pool: Pool = NewPool {
            category_id,
            description: &description,
        }
        .insert_into(pool::table)
        .get_result(conn)?;

        let names = vec![SmallString::from(pool_name.to_owned())];
        update::pool::set_names(conn, &context.config, new_pool.id, &names)?;
        update::pool::add_posts(conn, new_pool.id, 0, &post_ids)?;

        let pool_data = snapshot::pool::SnapshotData {
            description: description.clone().into(),
            category,
            names,
            posts: post_ids.clone(),
        };
        snapshot::pool::creation_snapshot(conn, context.client, new_pool.id, pool_data)?;
        Ok(new_pool.id)
    });
    let pool_id = creation_result.map_err(|err| {
        import_error(format!(
            "Cannot create pool '{pool_name}': {err}. The resolved posts still exist; \
             rerunning the import with another name will match them by checksum."
        ))
    })?;

    info!(
        "Created pool {pool_id} ('{pool_name}') with {} posts: {exact_count} matched by checksum, \
         {similar_count} matched by similarity, {created_count} newly created",
        post_ids.len()
    );
    if !failed_pages.is_empty() {
        warn!(
            "{} pages failed and were left out of the pool: {}",
            failed_pages.len(),
            failed_pages.join(", ")
        );
    }
    Ok(ImportSummary {
        pool_id,
        post_count: post_ids.len(),
        matched_exact: exact_count,
        matched_similar: similar_count,
        created: created_count,
        failed_pages,
    })
}

/// Resolves a single archive page to a post id. Designed to run sequentially.
fn import_page(
    context: &Context,
    conn: &mut PgConnection,
    archive: &mut ZipArchive<BufReader<File>>,
    index: usize,
    mime_type: MimeType,
    safety: PostSafety,
) -> ApiResult<PageResolution> {
    // Extract the page to a temporary upload file so it can go through the
    // same property-computation and transcoding pipeline as a regular upload.
    let mut entry = archive
        .by_index(index)
        .map_err(|err| import_error(format!("Cannot read archive entry: {err}")))?;
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut bytes)?;

    let token = UploadToken::new(mime_type);
    let temp_path = token.path(&context.config);
    filesystem::create_parent_directories(&temp_path)?;
    std::fs::write(&temp_path, &bytes)?;

    let properties = match cache::compute_properties_no_cache(context, token) {
        Ok(properties) => properties,
        Err(err) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(import_error(format!("Cannot process page content: {err}")));
        }
    };
    let final_temp_path = properties.token.path(&context.config);

    // Checksums correspond to the transcoded file, so pages whose originals were
    // uploaded through the same transcoding pipeline match exactly.
    let exact_match: Option<i64> = post::table
        .select(post::id)
        .filter(post::checksum.eq(&properties.checksum))
        .first(conn)
        .optional()?;
    if let Some(post_id) = exact_match {
        let _ = std::fs::remove_file(&final_temp_path);
        return Ok(PageResolution::MatchedExact(post_id));
    }

    // Fall back to perceptual similarity, like the reverse search endpoint.
    let candidates = PostSignature::find_similar_candidates(conn, &signature::generate_indexes(&properties.signature))?;
    let signature_cache = SignatureCache::new(&properties.signature);
    let distance_threshold = 1.0 - context.config.post_similarity_threshold;
    let best_match = candidates
        .iter()
        .map(|candidate| (candidate.post_id, signature::distance(&signature_cache, &candidate.signature)))
        .filter(|(_, distance)| *distance < distance_threshold)
        .min_by(|(_, distance_a), (_, distance_b)| distance_a.total_cmp(distance_b));
    if let Some((post_id, distance)) = best_match {
        let _ = std::fs::remove_file(&final_temp_path);
        return Ok(PageResolution::MatchedSimilar(post_id, distance));
    }

    // No match: create a new post, mirroring the post creation API.
    let creation_result: ApiResult<i64> = conn.transaction(|conn| {
        let new_post: Post = NewPost {
            user_id: context.client.id,
            file_size: properties.file_size,
            width: properties.width,
            height: properties.height,
            safety,
            type_: properties.post_type,
            mime_type: properties.mime_type,
            checksum: properties.checksum.clone(),
            checksum_md5: properties.md5_checksum.clone(),
            flags: properties.flags,
            source: "",
            description: "",
            phash: Some(properties.phash),
        }
        .insert_into(post::table)
        .get_result(conn)?;
        let post_hash = PostHash::new(&context.config, new_post.id);

        NewPostSignature {
            post_id: new_post.id,
            signature: properties.signature.into(),
            words: signature::generate_indexes(&properties.signature).into(),
        }
        .insert_into(post_signature::table)
        .execute(conn)?;

        filesystem::move_file(&final_temp_path, &post_hash.content_path(properties.mime_type))?;
        update::post::thumbnail(conn, &post_hash, &properties.thumbnail, ThumbnailCategory::Generated)?;

        let post_data = snapshot::post::SnapshotData {
            safety: new_post.safety,
            checksum: hex::encode(&new_post.checksum),
            flags: new_post.flags,
            source: new_post.source,
            description: new_post.description,
            tags: Vec::new(),
            relations: Vec::new(),
            notes: Vec::new(),
            featured: false,
        };
        snapshot::post::creation_snapshot(conn, context.client, new_post.id, post_data)?;
        Ok(new_post.id)
    });
    match creation_result {
        Ok(post_id) => Ok(PageResolution::Created(post_id)),
        Err(err) => {
            let _ = std::fs::remove_file(&final_temp_path);
            Err(import_error(format!("Cannot create post: {err}")))
        }
    }
}

/// Compares file names treating digit runs as numbers, so that "page2" sorts before "page10".
fn natural_order(name_a: &str, name_b: &str) -> Ordering {
    let mut chars_a = name_a.chars().peekable();
    let mut chars_b = name_b.chars().peekable();
    loop {
        match (chars_a.peek().copied(), chars_b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(char_a), Some(char_b)) if char_a.is_ascii_digit() && char_b.is_ascii_digit() => {
                match take_number(&mut chars_a).cmp(&take_number(&mut chars_b)) {
                    Ordering::Equal => (),
                    order => return order,
                }
            }
            (Some(char_a), Some(char_b)) => {
                match char_a.to_ascii_lowercase().cmp(&char_b.to_ascii_lowercase()) {
                    Ordering::Equal => {
                        chars_a.next();
                        chars_b.next();
                    }
                    order => return order,
                }
            }
        }
    }
}

/// Consumes a run of ASCII digits, returning a key that compares numerically:
/// with leading zeros stripped, a longer digit string is always a larger number
/// and equal-length digit strings compare numerically when compared lexically.
fn take_number(chars: &mut Peekable<Chars>) -> (usize, String) {
    let mut digits = String::new();
    while let Some(character) = chars.peek().copied() {
        if character.is_ascii_digit() {
            digits.push(character);
            chars.next();
        } else {
            break;
        }
    }
    let trimmed = digits.trim_start_matches('0');
    (trimmed.len(), trimmed.to_owned())
}
