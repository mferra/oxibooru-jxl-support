use crate::admin::input::PostEditor;
use crate::admin::{AdminResult, PRINT_INTERVAL, ProgressReporter, input};
use crate::api::error::ApiResult;
use crate::app::AppState;
use crate::config::ThumbnailFormat;
use crate::content::encode;
use crate::content::hash::{Checksum, PostHash};
use crate::content::signature::SIGNATURE_VERSION;
use crate::content::thumbnail::{ThumbnailCategory, ThumbnailType};
use crate::content::transcode;
use crate::content::{decode, hash, signature, thumbnail};
use crate::extract::Ctx;
use crate::filesystem;
use crate::model::enums::{MimeType, PostType};
use crate::model::post::{CompressedSignature, NewPostSignature};
use crate::schema::{database_statistics, post, post_signature};
use crate::search::Builder;
use crate::search::post::{QueryBuilder, Token};
use crate::time::{DateTime, Timer};
use crate::{admin, update};
use diesel::dsl::exists;
use diesel::{Connection, ExpressionMethods, Insertable, OptionalExtension, QueryDsl, RunQueryDsl};
use image::DynamicImage;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use tracing::{error, info, warn};

/// Checks the integrity of all posts on the filesystem by comparing the stored
/// checksum with the checksum of the post content in its current state.
/// Meant to detect file corruption or silent modification.
pub fn check_integrity(state: &AppState, editor: &mut PostEditor) {
    input::user_input_loop(state, editor, |state: &AppState, editor: &mut PostEditor| {
        let post_ids = user_query(state, editor)?;

        let _timer = Timer::new("check_integrity");
        let progress = ProgressReporter::new("Posts checked", PRINT_INTERVAL);
        let failures = ProgressReporter::new("Integrity checks failed", None);
        post_ids
            .into_par_iter()
            .try_for_each(|post_id| check_integrity_in_parallel(state, post_id, &progress, &failures))?;
        failures.report();
        Ok(())
    });
}

/// Recomputes posts checksums.
/// Useful when the way we compute checksums changes.
pub fn recompute_checksums(state: &AppState, editor: &mut PostEditor) {
    input::user_input_loop(state, editor, |state: &AppState, editor: &mut PostEditor| {
        let post_ids = user_query(state, editor)?;

        let _timer = Timer::new("recompute_checksums");
        let progress = ProgressReporter::new("Checksums computed", PRINT_INTERVAL);
        let duplicate_count = ProgressReporter::new("Duplicates found", PRINT_INTERVAL);
        post_ids
            .into_par_iter()
            .try_for_each(|post_id| recompute_checksum_in_parallel(state, post_id, &progress, &duplicate_count))?;
        duplicate_count.report();
        Ok(())
    });
}

/// Recomputes both post signatures and signature indexes.
/// Useful when the post signature parameters change.
pub fn recompute_signatures(state: &AppState, editor: &mut PostEditor) {
    input::user_input_loop(state, editor, |state: &AppState, editor: &mut PostEditor| {
        let post_ids = user_query(state, editor)?;

        // Update signature version only after a successful data retrieval.
        // We do this before actually recomputing signatures so that server
        // can continue running during computation.
        diesel::update(database_statistics::table)
            .set(database_statistics::signature_version.eq(SIGNATURE_VERSION))
            .execute(&mut state.connection_pool.get_blocking()?)?;

        let _timer = Timer::new("recompute_signatures");
        let progress = ProgressReporter::new("Signatures computed", PRINT_INTERVAL);
        post_ids
            .into_par_iter()
            .try_for_each(|post_id| recompute_signature_in_parallel(state, post_id, &progress))
    });
}

/// Recomputes post signature indexes.
/// Useful when the post signature index parameters change.
///
/// This is much faster than recomputing the signatures, as this function doesn't require
/// reading post content from disk.
pub fn recompute_indexes(state: &AppState, editor: &mut PostEditor) {
    input::user_input_loop(state, editor, |state: &AppState, editor: &mut PostEditor| {
        let post_ids = user_query(state, editor)?;

        let _timer = Timer::new("recompute_indexes");
        let progress = ProgressReporter::new("Indexes computed", PRINT_INTERVAL);
        post_ids
            .into_par_iter()
            .try_for_each(|post_id| recompute_index_in_parallel(state, post_id, &progress))
    });
}

pub fn regenerate_thumbnails(state: &AppState, editor: &mut PostEditor) {
    input::user_input_loop(state, editor, |state: &AppState, editor: &mut PostEditor| {
        let post_ids = user_query(state, editor)?;

        let _timer = Timer::new("regenerate_thumbnails");
        let progress = ProgressReporter::new("Thumbnails regenerated", PRINT_INTERVAL);
        post_ids
            .into_par_iter()
            .try_for_each(|post_id| regenerate_thumbnail_in_parallel(state, post_id, &progress))
    });
}

/// Checks content integrity for post with id `post_id`. Designed to operate in a parallel iterator.
fn check_integrity_in_parallel(
    state: &AppState,
    post_id: i64,
    progress: &ProgressReporter,
    failures: &ProgressReporter,
) -> AdminResult<()> {
    admin::is_cancelled()?;

    let mut conn = state.connection_pool.get_blocking()?;
    let (mime_type, checksum): (MimeType, Checksum) = match post::table
        .find(post_id)
        .select((post::mime_type, post::checksum))
        .first(&mut conn)
        .optional()
    {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return Ok(()), // Post must have been deleted after starting task, skip
        Err(err) => {
            error!("Cannot retrieve metadata for post {post_id} for reason: {err}");
            return Ok(());
        }
    };

    let content_path = PostHash::new(&state.config, post_id).content_path(mime_type);
    let file_checksum = match hash::compute_checksums(&content_path) {
        Ok((checksum, _)) => checksum,
        Err(err) => {
            error!("Unable to read file for post {post_id} for reason: {err}");
            return Ok(());
        }
    };

    if checksum != file_checksum {
        warn!("Post {post_id} failed integrity check. The file may have been corrupted or silently modified.");
        failures.increment();
    }
    progress.increment();
    Ok(())
}

/// Recomputes index for post with id `post_id`. Designed to operate in a parallel iterator.
fn recompute_index_in_parallel(state: &AppState, post_id: i64, progress: &ProgressReporter) -> AdminResult<()> {
    admin::is_cancelled()?;

    let mut conn = state.connection_pool.get_blocking()?;
    let signature: CompressedSignature = post_signature::table
        .find(post_id)
        .select(post_signature::signature)
        .for_no_key_update()
        .first(&mut conn)
        .unwrap();

    let indexes = signature::generate_indexes(&signature);
    match diesel::update(post_signature::table.find(post_id))
        .set(post_signature::words.eq(indexes.as_slice()))
        .execute(&mut conn)
    {
        Ok(_) => progress.increment(),
        Err(err) => error!("Index update failed for post {post_id} for reason: {err}"),
    }
    Ok(())
}

/// Recomputes checksum for post with id `post_id`. Designed to operate in a parallel iterator.
fn recompute_checksum_in_parallel(
    state: &AppState,
    post_id: i64,
    progress: &ProgressReporter,
    duplicate_count: &ProgressReporter,
) -> AdminResult<()> {
    admin::is_cancelled()?;

    let mut conn = state.connection_pool.get_blocking()?;
    let mime_type = match post::table
        .find(post_id)
        .select(post::mime_type)
        .first(&mut conn)
        .optional()
    {
        Ok(Some(mime_type)) => mime_type,
        Ok(None) => return Ok(()), // Post must have been deleted after starting task, skip
        Err(err) => {
            error!("Cannot retrieve MIME type for post {post_id} for reason: {err}");
            return Ok(());
        }
    };

    let image_path = PostHash::new(&state.config, post_id).content_path(mime_type);
    let (checksum, md5_checksum) = match hash::compute_checksums(&image_path) {
        Ok(checksums) => checksums,
        Err(err) => {
            error!("Unable to compute checksum for post {post_id} for reason: {err}");
            return Ok(());
        }
    };

    let duplicate: Option<i64> = match post::table
        .select(post::id)
        .filter(post::checksum.eq(&checksum))
        .filter(post::id.ne(post_id))
        .first(&mut conn)
        .optional()
    {
        Ok(dup) => dup,
        Err(err) => {
            error!("Duplicate check failed for post {post_id} for reason: {err}");
            return Ok(());
        }
    };
    if let Some(dup_id) = duplicate {
        warn!("Potential duplicate post {dup_id} for post {post_id}");
        duplicate_count.increment();
        return Ok(());
    }

    match diesel::update(post::table.find(post_id))
        .set((
            post::checksum.eq(checksum),
            post::checksum_md5.eq(md5_checksum),
            post::last_edit_time.eq(DateTime::now()),
        ))
        .execute(&mut conn)
    {
        Ok(_) => progress.increment(),
        Err(err) => error!("Checksum update failed for post {post_id} for reason: {err}"),
    }
    Ok(())
}

/// Recomputes signature for post with id `post_id`. Designed to operate in a parallel iterator.
fn recompute_signature_in_parallel(state: &AppState, post_id: i64, progress: &ProgressReporter) -> AdminResult<()> {
    admin::is_cancelled()?;

    let mut conn = state.connection_pool.get_blocking()?;
    let mime_type = match post::table
        .find(post_id)
        .select(post::mime_type)
        .first(&mut conn)
        .optional()
    {
        Ok(Some(mime_type)) => mime_type,
        Ok(None) => return Ok(()), // Post must have been deleted after starting task, skip
        Err(err) => {
            error!("Cannot retrieve MIME type for post {post_id} for reason: {err}");
            return Ok(());
        }
    };

    let content_path = PostHash::new(&state.config, post_id).content_path(mime_type);
    let image = match decode::representative_image(&state.config, &content_path, mime_type) {
        Ok(image) => image,
        Err(err) => {
            error!("Unable to get representative image for post {post_id} for reason: {err}");
            return Ok(());
        }
    };

    let image_signature = signature::compute(&image);
    let signature_indexes = signature::generate_indexes(&image_signature);
    let transaction_result = conn.transaction(|conn| {
        // Post may have been deleted, so make sure it still exists first
        let post_exists: bool = diesel::select(exists(post::table.find(post_id))).first(conn)?;
        if !post_exists {
            return Ok(0);
        }

        let signature_exists: bool = diesel::select(exists(post_signature::table.find(post_id))).first(conn)?;
        if signature_exists {
            diesel::update(post_signature::table.find(post_id))
                .set((
                    post_signature::signature.eq(image_signature.as_slice()),
                    post_signature::words.eq(signature_indexes.as_slice()),
                ))
                .execute(conn)
        } else {
            NewPostSignature {
                post_id,
                signature: image_signature.into(),
                words: signature_indexes.into(),
            }
            .insert_into(post_signature::table)
            .execute(conn)
        }
    });

    match transaction_result {
        Ok(_) => progress.increment(),
        Err(err) => error!("Unable to update post signature for post {post_id} for reason: {err}"),
    }
    Ok(())
}

/// Regenerates thumbnail for post with id `post_id`. Designed to operate in a parallel iterator.
fn regenerate_thumbnail_in_parallel(state: &AppState, post_id: i64, progress: &ProgressReporter) -> AdminResult<()> {
    admin::is_cancelled()?;

    let mut conn = state.connection_pool.get_blocking()?;
    let mime_type = match post::table
        .find(post_id)
        .select(post::mime_type)
        .first(&mut conn)
        .optional()
    {
        Ok(Some(mime_type)) => mime_type,
        Ok(None) => return Ok(()), // Post must have been deleted after starting task, skip
        Err(err) => {
            error!("Cannot retrieve MIME type for post {post_id} for reason: {err}");
            return Ok(());
        }
    };

    let post_hash = PostHash::new(&state.config, post_id);
    let content_path = post_hash.content_path(mime_type);
    let thumbnail = match decode::representative_image(&state.config, &content_path, mime_type) {
        Ok(image) => thumbnail::create(&state.config, &image, ThumbnailType::Post),
        Err(err) => {
            error!("Cannot decode content for post {post_id} for reason: {err}");
            return Ok(());
        }
    };
    if let Err(err) = update::post::thumbnail(&mut conn, &post_hash, &thumbnail, ThumbnailCategory::Generated) {
        error!("Cannot save thumbnail for post {post_id} for reason: {err}");
    } else {
        progress.increment();
    }
    Ok(())
}

/// Re-encodes image-type post content as JPEG XL and regenerates thumbnails.
///
/// Skips animated (GIF), video, and Flash posts. For each eligible post:
/// 1. Decodes the existing content file.
/// 2. Encodes to JXL and writes the new file.
/// 3. Removes the old content file.
/// 4. Updates `mime_type` and `checksum` in the database.
/// 5. Regenerates and saves the thumbnail.
pub fn convert_posts_to_jxl(state: &AppState, editor: &mut PostEditor) {
    input::user_input_loop(state, editor, |state: &AppState, editor: &mut PostEditor| {
        let post_ids = user_query(state, editor)?;

        let _timer = Timer::new("convert_posts_to_jxl");
        let converted = ProgressReporter::new("Posts converted to JXL", PRINT_INTERVAL);
        let skipped = ProgressReporter::new("Posts skipped", None);
        let failed = ProgressReporter::new("Posts failed", None);
        post_ids
            .into_par_iter()
            .try_for_each(|post_id| convert_post_to_jxl_in_parallel(state, post_id, &converted, &skipped, &failed))?;
        skipped.report();
        failed.report();
        Ok(())
    });
}

/// Converts a single post to JXL. Designed for use inside a parallel iterator.
fn convert_post_to_jxl_in_parallel(
    state: &AppState,
    post_id: i64,
    converted: &ProgressReporter,
    skipped: &ProgressReporter,
    failed: &ProgressReporter,
) -> AdminResult<()> {
    admin::is_cancelled()?;

    let mut conn = state.connection_pool.get_blocking()?;
    let (mime_type, existing_phash): (MimeType, Option<i64>) = match post::table
        .find(post_id)
        .select((post::mime_type, post::phash))
        .first(&mut conn)
        .optional()
    {
        Ok(Some(row)) => row,
        Ok(None) => {
            info!("Post {post_id}: skipped — not found (deleted between query and processing)");
            return Ok(());
        }
        Err(err) => {
            error!("Post {post_id}: failed — cannot retrieve MIME type: {err}");
            failed.increment();
            return Ok(());
        }
    };

    // Skip already-JXL posts.
    if mime_type == MimeType::Jxl {
        info!("Post {post_id}: skipped — already JXL");
        skipped.increment();
        return Ok(());
    }

    // Skip non-still-image types (animation, video, flash).
    if PostType::from(mime_type) != PostType::Image {
        info!("Post {post_id}: skipped — type is {:?} ({mime_type})", PostType::from(mime_type));
        skipped.increment();
        return Ok(());
    }

    let post_hash = PostHash::new(&state.config, post_id);
    let old_content_path = post_hash.content_path(mime_type);

    // Skip animated WebP (maps to PostType::Image but is an animation).
    if mime_type == MimeType::Webp && transcode::webp_is_animated(&old_content_path) {
        info!("Post {post_id}: skipped — animated WebP");
        skipped.increment();
        return Ok(());
    }

    // Record original file size for the completion log.
    let orig_size = std::fs::metadata(&old_content_path)
        .map(|m| m.len())
        .unwrap_or(0);

    // Decode the original file.
    let decoded = match decode::image(&old_content_path, mime_type) {
        Ok(img) => img,
        Err(err) => {
            error!("Post {post_id}: failed — cannot decode {mime_type} content: {err}");
            failed.increment();
            return Ok(());
        }
    };

    // Encode to JXL using the quality configured under [transcoding].
    let jxl_bytes = match encode::to_jxl(&decoded, state.config.transcoding.image_quality) {
        Ok(bytes) => bytes,
        Err(err) => {
            error!("Post {post_id}: failed — JXL encode error: {err}");
            failed.increment();
            return Ok(());
        }
    };

    // Write new JXL content file.
    let new_content_path = post_hash.content_path(MimeType::Jxl);
    if let Err(err) = filesystem::create_parent_directories(&new_content_path)
        .and_then(|_| std::fs::write(&new_content_path, &jxl_bytes).map_err(Into::into))
    {
        error!("Post {post_id}: failed — cannot write JXL file: {err}");
        failed.increment();
        return Ok(());
    }

    // Compute new checksum.
    let (new_checksum, new_md5) = match hash::compute_checksums(&new_content_path) {
        Ok(cs) => cs,
        Err(err) => {
            error!("Post {post_id}: failed — cannot compute checksum: {err}");
            let _ = std::fs::remove_file(&new_content_path);
            failed.increment();
            return Ok(());
        }
    };

    // Generate thumbnail.
    let thumb = thumbnail::create(&state.config, &decoded, ThumbnailType::Post);

    // Compute the pHash from the already-decoded image when missing, so the pHash
    // backfill task doesn't have to decode this file a second time.
    let phash_value = existing_phash.unwrap_or_else(|| hash::compute_phash(&decoded));

    // Persist everything atomically-ish: update DB then swap files.
    let db_result = conn.transaction(|conn| {
        diesel::update(post::table.find(post_id))
            .set((
                post::mime_type.eq(MimeType::Jxl),
                post::checksum.eq(new_checksum.as_ref()),
                post::checksum_md5.eq(new_md5.as_ref()),
                post::file_size.eq(jxl_bytes.len() as i64),
                post::phash.eq(phash_value),
            ))
            .execute(conn)
    });

    if let Err(err) = db_result {
        error!("Post {post_id}: failed — database update error: {err}");
        let _ = std::fs::remove_file(&new_content_path);
        failed.increment();
        return Ok(());
    }

    // Remove old content file.
    if let Err(err) = std::fs::remove_file(&old_content_path) {
        warn!("Post {post_id}: converted but could not remove old {mime_type} file: {err}");
    }

    // Regenerate thumbnail (delete old format variants first).
    if let Err(err) = filesystem::delete_post_thumbnails_all_formats(&post_hash, ThumbnailCategory::Generated) {
        warn!("Post {post_id}: converted but could not remove old thumbnail: {err}");
    }
    if let Err(err) = update::post::thumbnail(&mut conn, &post_hash, &thumb, ThumbnailCategory::Generated) {
        warn!("Post {post_id}: converted but thumbnail regeneration failed: {err}");
    }

    let new_size = jxl_bytes.len() as u64;
    let ratio = if orig_size > 0 { new_size * 100 / orig_size } else { 0 };
    info!(
        "Post {post_id}: converted {mime_type} → JXL  \
         ({orig_size} B → {new_size} B, {ratio}% of original)"
    );
    converted.increment();
    Ok(())
}

/// Computes and stores perceptual hashes for posts that do not yet have one.
///
/// Skips posts whose `phash` column is already set.  Safe to run multiple times.
pub fn compute_phash(state: &AppState, editor: &mut PostEditor) {
    input::user_input_loop(state, editor, |state: &AppState, editor: &mut PostEditor| {
        let post_ids = user_query(state, editor)?;

        let _timer = Timer::new("compute_phash");
        let progress = ProgressReporter::new("pHash computed", PRINT_INTERVAL);
        let skipped = ProgressReporter::new("Posts skipped (pHash already set)", None);
        post_ids
            .into_par_iter()
            .try_for_each(|post_id| compute_phash_in_parallel(state, post_id, &progress, &skipped))?;
        skipped.report();
        Ok(())
    });
}

/// Computes and stores the pHash for a single post.  Designed for use in a parallel iterator.
fn compute_phash_in_parallel(
    state: &AppState,
    post_id: i64,
    progress: &ProgressReporter,
    skipped: &ProgressReporter,
) -> AdminResult<()> {
    admin::is_cancelled()?;

    let mut conn = state.connection_pool.get_blocking()?;

    let (mime_type, existing_phash): (MimeType, Option<i64>) = match post::table
        .find(post_id)
        .select((post::mime_type, post::phash))
        .first(&mut conn)
        .optional()
    {
        Ok(Some(row)) => row,
        Ok(None) => return Ok(()), // deleted between query and processing
        Err(err) => {
            error!("Cannot retrieve metadata for post {post_id}: {err}");
            return Ok(());
        }
    };

    if existing_phash.is_some() {
        skipped.increment();
        return Ok(());
    }

    let post_hash = PostHash::new(&state.config, post_id);
    let content_path = post_hash.content_path(mime_type);
    let image = match decode::representative_image(&state.config, &content_path, mime_type) {
        Ok(img) => img,
        // Fall back to the generated thumbnail. compute_phash downscales to 32x32 anyway,
        // so a thumbnail-derived hash is nearly identical to one from the original.
        Err(err) => match decode_generated_thumbnail(state, post_id) {
            Ok(img) => {
                warn!("Post {post_id}: content undecodable ({err}); computing pHash from generated thumbnail");
                img
            }
            Err(thumbnail_err) => {
                error!("Cannot decode content for post {post_id}: {err} (thumbnail fallback failed: {thumbnail_err})");
                return Ok(());
            }
        },
    };

    let phash_value = hash::compute_phash(&image);

    match diesel::update(post::table.find(post_id))
        .set(post::phash.eq(phash_value))
        .execute(&mut conn)
    {
        Ok(_) => progress.increment(),
        Err(err) => error!("pHash update failed for post {post_id}: {err}"),
    }
    Ok(())
}

/// Decodes a post's generated thumbnail. Thumbnails on disk may predate a thumbnail
/// format config change, so the configured format is tried first and then the other
/// known format.
fn decode_generated_thumbnail(state: &AppState, post_id: i64) -> ApiResult<DynamicImage> {
    let post_hash = PostHash::new(&state.config, post_id);
    let (first, second) = match state.config.thumbnails.format {
        ThumbnailFormat::Jpeg => (("jpg", MimeType::Jpeg), ("jxl", MimeType::Jxl)),
        ThumbnailFormat::Jxl => (("jxl", MimeType::Jxl), ("jpg", MimeType::Jpeg)),
    };
    decode::image(&post_hash.generated_thumbnail_path_with_ext(first.0), first.1)
        .or_else(|_| decode::image(&post_hash.generated_thumbnail_path_with_ext(second.0), second.1))
}

fn user_query(state: &AppState, editor: &mut PostEditor) -> AdminResult<Vec<i64>> {
    loop {
        let Ctx(ctx, _) = state.clone().make_context(admin::client());
        let user_input =
            input::read("Select posts (leave blank to select all, enter \"done\" when finished): ", editor)?;
        match QueryBuilder::new_with_anonymous_token(&ctx, &user_input, Token::Tag) {
            Err(err) => error!("Could not parse query for reason: {err}"),
            Ok(mut builder) => {
                let mut conn = state.connection_pool.get_blocking()?;
                let post_ids = conn.transaction(|conn| builder.load(conn))?;
                info!("Found {} posts", post_ids.len());
                return Ok(post_ids);
            }
        }
    }
}
