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
use crate::model::post::{CompressedSignature, NewPostSignature, Post};
use crate::schema::{database_statistics, post, post_relation, post_signature};
use crate::search::Builder;
use crate::search::post::{QueryBuilder, Token};
use crate::time::{DateTime, Timer};
use crate::{admin, snapshot, update};
use diesel::dsl::exists;
use diesel::{
    Connection, ExpressionMethods, Insertable, OptionalExtension, PgConnection, QueryDsl, QueryResult, RunQueryDsl,
    SelectableHelper,
};
use image::{DynamicImage, GenericImageView};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::Path;
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
        let metadata = post_checksums(state, &post_ids)?;
        metadata.into_par_iter().try_for_each(|(post_id, mime_type, checksum)| {
            check_integrity_in_parallel(state, post_id, mime_type, &checksum, &progress, &failures)
        })?;
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
        let metadata = post_mime_types(state, &post_ids)?;
        metadata.into_par_iter().try_for_each(|(post_id, mime_type)| {
            recompute_checksum_in_parallel(state, post_id, mime_type, &progress, &duplicate_count)
        })?;
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
        let metadata = post_mime_types(state, &post_ids)?;
        metadata
            .into_par_iter()
            .try_for_each(|(post_id, mime_type)| recompute_signature_in_parallel(state, post_id, mime_type, &progress))
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

/// Recomputes post types.
/// Meant to apply changes to existing posts when post type detection changes,
/// such as picking up posts that are/aren't actually animated.
pub fn recompute_post_types(state: &AppState, editor: &mut PostEditor) {
    input::user_input_loop(state, editor, |state: &AppState, editor: &mut PostEditor| {
        let post_ids = user_query(state, editor)?;

        let _timer = Timer::new("recompute_post_types");
        let progress = ProgressReporter::new("Post types computed", PRINT_INTERVAL);
        let metadata = post_mime_types(state, &post_ids)?;
        metadata
            .into_par_iter()
            .try_for_each(|(post_id, mime_type)| recompute_post_type_in_parallel(state, post_id, mime_type, &progress))
    });
}

/// Regenerates the generated thumbnails of the selected posts.
///
/// Posts whose thumbnail already exists on disk in the format configured under
/// `[thumbnails]` are left alone, since that file is exactly what the server serves and
/// re-encoding it would only cost CPU. A thumbnail is regenerated when it is missing,
/// empty, or only present in the other format, which is the state left behind after
/// switching `thumbnails.format` between JPEG and JXL. Use `regenerate_thumbnails_force`
/// to rebuild thumbnails that are already in the configured format, such as after
/// changing the thumbnail dimensions or JXL quality.
pub fn regenerate_thumbnails(state: &AppState, editor: &mut PostEditor) {
    regenerate_thumbnails_impl(state, editor, false);
}

/// Regenerates the generated thumbnails of the selected posts unconditionally,
/// replacing thumbnails that already exist in the configured format.
pub fn force_regenerate_thumbnails(state: &AppState, editor: &mut PostEditor) {
    regenerate_thumbnails_impl(state, editor, true);
}

fn regenerate_thumbnails_impl(state: &AppState, editor: &mut PostEditor, force: bool) {
    input::user_input_loop(state, editor, |state: &AppState, editor: &mut PostEditor| {
        let post_ids = user_query(state, editor)?;

        let _timer = Timer::new("regenerate_thumbnails");
        let progress = ProgressReporter::new("Thumbnails regenerated", PRINT_INTERVAL);
        let skipped = ProgressReporter::new("Posts skipped (thumbnail already in configured format)", None);
        let metadata = thumbnail_metadata(state, &post_ids)?;
        metadata.into_par_iter().try_for_each(|(post_id, mime_type, thumbnail_size)| {
            regenerate_thumbnail_in_parallel(state, post_id, mime_type, thumbnail_size, force, &progress, &skipped)
        })?;
        skipped.report();
        Ok(())
    });
}

/// Checks content integrity for post with id `post_id`. Designed to operate in a parallel iterator.
///
/// Needs no database connection: the stored checksum is passed in from the bulk metadata query.
fn check_integrity_in_parallel(
    state: &AppState,
    post_id: i64,
    mime_type: MimeType,
    checksum: &Checksum,
    progress: &ProgressReporter,
    failures: &ProgressReporter,
) -> AdminResult<()> {
    admin::is_cancelled()?;

    let content_path = PostHash::new(&state.config, post_id).content_path(mime_type);
    let file_checksum = match hash::compute_checksums(&content_path) {
        Ok((checksum, _)) => checksum,
        Err(err) => {
            error!("Unable to read content file {} for post {post_id}: {err}", content_path.display());
            return Ok(());
        }
    };

    if *checksum != file_checksum {
        warn!(
            "Post {post_id} failed integrity check (file: {}). \
             The file may have been corrupted or silently modified.",
            content_path.display()
        );
        failures.increment();
    }
    progress.increment();
    Ok(())
}

/// Recomputes index for post with id `post_id`. Designed to operate in a parallel iterator.
fn recompute_index_in_parallel(state: &AppState, post_id: i64, progress: &ProgressReporter) -> AdminResult<()> {
    admin::is_cancelled()?;

    let mut conn = state.connection_pool.get_blocking()?;
    let signature: CompressedSignature = match post_signature::table
        .find(post_id)
        .select(post_signature::signature)
        .for_no_key_update()
        .first(&mut conn)
        .optional()
    {
        Ok(Some(signature)) => signature,
        Ok(None) => {
            warn!("Post {post_id} has no signature; run recompute_signatures to create one. Skipping.");
            return Ok(());
        }
        Err(err) => {
            error!("Cannot retrieve signature for post {post_id}: {err}");
            return Ok(());
        }
    };

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

/// Recomputes post type for post with id `post_id`. Designed to operate in a parallel iterator.
fn recompute_post_type_in_parallel(
    state: &AppState,
    post_id: i64,
    mime_type: MimeType,
    progress: &ProgressReporter,
) -> AdminResult<()> {
    admin::is_cancelled()?;

    let content_path = PostHash::new(&state.config, post_id).content_path(mime_type);
    let is_animated_webp = mime_type == MimeType::Webp && transcode::webp_is_animated(&content_path);
    let post_type = if is_animated_webp {
        PostType::Animation
    } else {
        match decode::detect_post_type(&content_path, mime_type) {
            Ok(post_type) => post_type,
            Err(err) => {
                error!("Cannot detect post type for post {post_id} for reason: {err}");
                return Ok(());
            }
        }
    };

    let mut conn = state.connection_pool.get_blocking()?;
    match diesel::update(post::table.find(post_id))
        .set(post::type_.eq(post_type))
        .execute(&mut conn)
    {
        Ok(_) => progress.increment(),
        Err(err) => error!("Type update failed for post {post_id} for reason: {err}"),
    }
    Ok(())
}

/// Recomputes checksum for post with id `post_id`. Designed to operate in a parallel iterator.
fn recompute_checksum_in_parallel(
    state: &AppState,
    post_id: i64,
    mime_type: MimeType,
    progress: &ProgressReporter,
    duplicate_count: &ProgressReporter,
) -> AdminResult<()> {
    admin::is_cancelled()?;

    let image_path = PostHash::new(&state.config, post_id).content_path(mime_type);
    let (checksum, md5_checksum) = match hash::compute_checksums(&image_path) {
        Ok(checksums) => checksums,
        Err(err) => {
            error!("Unable to compute checksum for post {post_id} from {}: {err}", image_path.display());
            return Ok(());
        }
    };

    // Only now, with the file hashed, is a connection needed.
    let mut conn = state.connection_pool.get_blocking()?;
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
fn recompute_signature_in_parallel(
    state: &AppState,
    post_id: i64,
    mime_type: MimeType,
    progress: &ProgressReporter,
) -> AdminResult<()> {
    admin::is_cancelled()?;

    let content_path = PostHash::new(&state.config, post_id).content_path(mime_type);
    let image = match decode::representative_image(&state.config, &content_path, mime_type) {
        Ok(image) => image,
        Err(err) => {
            error!(
                "Unable to decode representative image for post {post_id} from {}: {err}",
                content_path.display()
            );
            return Ok(());
        }
    };

    let image_signature = signature::compute(&image);
    let signature_indexes = signature::generate_indexes(&image_signature);

    // Only now, with the image decoded and its signature computed, is a connection needed.
    let mut conn = state.connection_pool.get_blocking()?;
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

/// Regenerates the thumbnail of post `post_id`. Designed to operate in a parallel iterator.
///
/// Unless `force` is set, a thumbnail that already exists on disk in the configured format is
/// kept and the post is counted as skipped. A database connection is only checked out for the
/// final update, never while decoding or encoding, because a connection held across image work
/// keeps the whole pool busy for as long as the slowest decode takes.
fn regenerate_thumbnail_in_parallel(
    state: &AppState,
    post_id: i64,
    mime_type: MimeType,
    recorded_thumbnail_size: i64,
    force: bool,
    progress: &ProgressReporter,
    skipped: &ProgressReporter,
) -> AdminResult<()> {
    admin::is_cancelled()?;

    let post_hash = PostHash::new(&state.config, post_id);
    if !force && let Some(thumbnail_size) = usable_thumbnail_size(&post_hash.generated_thumbnail_path()) {
        // The thumbnail is up to date, but the size cached in the database may not be,
        // so correct it while we have the file size at hand.
        if thumbnail_size != recorded_thumbnail_size {
            let mut conn = state.connection_pool.get_blocking()?;
            let update_result = diesel::update(post::table.find(post_id))
                .set(post::generated_thumbnail_size.eq(thumbnail_size))
                .execute(&mut conn);
            if let Err(err) = update_result {
                warn!("Cannot update cached thumbnail size for post {post_id}: {err}");
            }
        }
        skipped.increment();
        return Ok(());
    }

    let content_path = post_hash.content_path(mime_type);
    let thumbnail = match decode::representative_image(&state.config, &content_path, mime_type) {
        Ok(image) => thumbnail::create(&state.config, &image, ThumbnailType::Post),
        Err(err) => {
            error!("Cannot decode content for post {post_id} from {}: {err}", content_path.display());
            return Ok(());
        }
    };

    // Remove thumbnails left behind in the other format so that switching
    // thumbnails.format doesn't leave orphans in the data directory.
    if let Err(err) = filesystem::delete_post_thumbnails_all_formats(&post_hash, ThumbnailCategory::Generated) {
        warn!("Cannot remove old thumbnail for post {post_id}: {err}");
    }

    let mut conn = state.connection_pool.get_blocking()?;
    if let Err(err) = update::post::thumbnail(&mut conn, &post_hash, &thumbnail, ThumbnailCategory::Generated) {
        error!("Cannot save thumbnail for post {post_id} for reason: {err}");
    } else {
        progress.increment();
    }
    Ok(())
}

/// Returns the size of the thumbnail at `path`, or `None` if it doesn't exist or is empty.
/// An empty file is treated as missing: it's what an interrupted write leaves behind and
/// it can't be served.
fn usable_thumbnail_size(path: &Path) -> Option<i64> {
    filesystem::file_size(path).ok().filter(|&size| size > 0)
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
        let metadata = post_phashes(state, &post_ids)?;
        metadata.into_par_iter().try_for_each(|(post_id, mime_type, existing_phash)| {
            convert_post_to_jxl_in_parallel(state, post_id, mime_type, existing_phash, &converted, &skipped, &failed)
        })?;
        skipped.report();
        failed.report();
        Ok(())
    });
}

/// Converts a single post to JXL. Designed for use inside a parallel iterator.
///
/// No database connection is held while the post is decoded and re-encoded, which is by far
/// the slowest part of the task; one is checked out once there are results to store.
fn convert_post_to_jxl_in_parallel(
    state: &AppState,
    post_id: i64,
    mime_type: MimeType,
    existing_phash: Option<i64>,
    converted: &ProgressReporter,
    skipped: &ProgressReporter,
    failed: &ProgressReporter,
) -> AdminResult<()> {
    admin::is_cancelled()?;

    // Skip already-JXL posts.
    if mime_type == MimeType::Jxl {
        skipped.increment();
        return Ok(());
    }

    // Skip non-still-image types (animation, video, flash).
    if PostType::from(mime_type) != PostType::Image {
        info!("Post {post_id}: skipped — type is {:?} ({mime_type})", PostType::from(mime_type));
        skipped.increment();
        return Ok(());
    }

    // Skip formats not configured for conversion (already-compressed formats like
    // WebP usually grow when re-encoded as JXL).
    if !state.config.transcoding.converts_to_jxl(mime_type) {
        info!("Post {post_id}: skipped — {mime_type} is not listed in transcoding.image_formats");
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
            error!(
                "Post {post_id}: failed — cannot decode {mime_type} content at {}: {err}",
                old_content_path.display()
            );
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

    // Keep the original when conversion doesn't actually save space.
    if orig_size > 0 && jxl_bytes.len() as u64 >= orig_size {
        info!(
            "Post {post_id}: skipped — JXL would be {} B, original {mime_type} is {orig_size} B; keeping original",
            jxl_bytes.len()
        );
        skipped.increment();
        return Ok(());
    }

    // Write new JXL content file.
    let new_content_path = post_hash.content_path(MimeType::Jxl);
    if let Err(err) = filesystem::create_parent_directories(&new_content_path)
        .and_then(|_| std::fs::write(&new_content_path, &jxl_bytes).map_err(Into::into))
    {
        error!(
            "Post {post_id}: failed — cannot write JXL file to {}: {err}. \
             Check free disk space and data directory permissions.",
            new_content_path.display()
        );
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

    // Pixel-identical duplicates stored in different formats encode to the same JXL
    // bytes, which would violate the unique checksum constraint. Detect this up front
    // and report the duplicate pair instead of failing on the database update.
    let mut conn = state.connection_pool.get_blocking()?;
    match post::table
        .select(post::id)
        .filter(post::checksum.eq(new_checksum.as_ref()))
        .filter(post::id.ne(post_id))
        .first::<i64>(&mut conn)
        .optional()
    {
        Ok(Some(duplicate_id)) => {
            warn!(
                "Post {post_id}: skipped — converting would produce content identical to post \
                 {duplicate_id} (pixel-perfect duplicate in a different format). Keeping the \
                 original file; consider merging these posts."
            );
            relate_duplicate_posts(&mut conn, post_id, duplicate_id);
            let _ = std::fs::remove_file(&new_content_path);
            skipped.increment();
            return Ok(());
        }
        Ok(None) => (),
        Err(err) => {
            error!("Post {post_id}: failed — duplicate check error: {err}");
            let _ = std::fs::remove_file(&new_content_path);
            failed.increment();
            return Ok(());
        }
    }

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

    // An update that matched no rows means the post was deleted while it was being converted,
    // so the file just written belongs to nothing and has to go.
    if let Ok(0) = db_result {
        info!("Post {post_id}: skipped — deleted while it was being converted");
        let _ = std::fs::remove_file(&new_content_path);
        skipped.increment();
        return Ok(());
    }

    if let Err(err) = db_result {
        // The up-front duplicate check can race with another worker converting the
        // other half of a duplicate pair, so a unique violation here still means
        // "pixel-perfect duplicate", not a real failure.
        if let diesel::result::Error::DatabaseError(diesel::result::DatabaseErrorKind::UniqueViolation, _) = err {
            warn!(
                "Post {post_id}: skipped — converted JXL is byte-identical to another post's \
                 content (pixel-perfect duplicate). Keeping the original file; consider \
                 merging these posts."
            );
            // Identify the post that owns the conflicting checksum to record the pair.
            match post::table
                .select(post::id)
                .filter(post::checksum.eq(new_checksum.as_ref()))
                .filter(post::id.ne(post_id))
                .first::<i64>(&mut conn)
                .optional()
            {
                Ok(Some(duplicate_id)) => relate_duplicate_posts(&mut conn, post_id, duplicate_id),
                Ok(None) => (),
                Err(query_err) => {
                    warn!("Post {post_id}: could not identify the duplicate post: {query_err}");
                }
            }
            skipped.increment();
        } else {
            error!("Post {post_id}: failed — database update error: {err}");
            failed.increment();
        }
        let _ = std::fs::remove_file(&new_content_path);
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
        let targets = posts_missing_phash(state, &post_ids, &skipped)?;
        targets
            .into_par_iter()
            .try_for_each(|(post_id, mime_type)| compute_phash_in_parallel(state, post_id, mime_type, &progress))?;
        skipped.report();
        Ok(())
    });
}

/// Computes and stores the pHash for a single post.  Designed for use in a parallel iterator.
///
/// A database connection is checked out only for the final update. Decoding a post can take
/// seconds, and a connection held for that long by every worker leaves the pool with nothing
/// to hand out, so the checkout eventually fails and aborts the task.
fn compute_phash_in_parallel(
    state: &AppState,
    post_id: i64,
    mime_type: MimeType,
    progress: &ProgressReporter,
) -> AdminResult<()> {
    admin::is_cancelled()?;

    let post_hash = PostHash::new(&state.config, post_id);
    let content_path = post_hash.content_path(mime_type);
    let image = match decode::representative_image(&state.config, &content_path, mime_type) {
        Ok(img) => img,
        // Fall back to the generated thumbnail. compute_phash downscales to 32x32 anyway,
        // so a thumbnail-derived hash is nearly identical to one from the original.
        Err(err) => match decode_generated_thumbnail(state, post_id) {
            Ok(img) => {
                warn!(
                    "Post {post_id}: content at {} undecodable ({err}); computing pHash from generated thumbnail",
                    content_path.display()
                );
                img
            }
            Err(thumbnail_err) => {
                error!(
                    "Cannot compute pHash for post {post_id}: content at {} undecodable ({err}) \
                     and thumbnail fallback failed ({thumbnail_err}). Post will be excluded from \
                     similar: searches until its pHash is computed.",
                    content_path.display()
                );
                return Ok(());
            }
        },
    };

    let phash_value = hash::compute_phash(&image);

    let mut conn = state.connection_pool.get_blocking()?;
    match diesel::update(post::table.find(post_id))
        .set(post::phash.eq(phash_value))
        .execute(&mut conn)
    {
        Ok(_) => progress.increment(),
        Err(err) => error!("pHash update failed for post {post_id}: {err}"),
    }
    Ok(())
}

/// Records a bidirectional relation between two posts discovered to be pixel-perfect
/// duplicates, so the pair remains visible in the UI for later review or merging.
/// Does nothing if the posts are already related.
fn relate_duplicate_posts(conn: &mut PgConnection, post_id: i64, duplicate_id: i64) {
    let already_related: bool = match diesel::select(exists(
        post_relation::table
            .filter(post_relation::parent_id.eq(post_id))
            .filter(post_relation::child_id.eq(duplicate_id)),
    ))
    .first(conn)
    {
        Ok(related) => related,
        Err(err) => {
            warn!("Post {post_id}: could not check for existing relation with duplicate post {duplicate_id}: {err}");
            return;
        }
    };
    if already_related {
        return;
    }

    match update::post::add_relations(conn, post_id, &[duplicate_id]) {
        Ok(()) => info!("Post {post_id}: created relation with duplicate post {duplicate_id}"),
        // A parallel worker converting the other half of the pair may have just inserted it.
        Err(err) => warn!("Post {post_id}: could not create relation with duplicate post {duplicate_id}: {err}"),
    }
}

/// Merges pairs of related posts whose content is pixel-identical.
///
/// Candidate pairs come from existing post relations (such as those created by the
/// convert_posts_to_jxl task when it detects duplicates). The relation alone is not
/// proof — users can create relations manually — so each pair is verified by decoding
/// both files and comparing pixels exactly; related posts that are not true duplicates
/// are left untouched.
///
/// Merge policy: the post with the lower ID survives, and the higher-resolution content
/// file is kept — with JXL preferred on equal resolution, and the smaller file preferred
/// when the formats match too. Tags, pools, scores, favorites, features,
/// comments, descriptions, and relations are merged into the surviving post via the
/// same logic as the post merge API, including a merge snapshot for auditing.
pub fn merge_duplicate_posts(state: &AppState, editor: &mut PostEditor) {
    input::user_input_loop(state, editor, |state: &AppState, editor: &mut PostEditor| {
        let post_ids = user_query(state, editor)?;
        let selected: HashSet<i64> = post_ids.into_iter().collect();

        let _timer = Timer::new("merge_duplicate_posts");
        let merged = ProgressReporter::new("Duplicate pairs merged", PRINT_INTERVAL);
        let skipped = ProgressReporter::new("Related pairs skipped (not pixel-identical duplicates)", None);
        let failed = ProgressReporter::new("Merges failed", None);

        // Relations are stored bidirectionally; parent_id < child_id picks each pair once.
        let pairs: Vec<(i64, i64)> = post_relation::table
            .select((post_relation::parent_id, post_relation::child_id))
            .filter(post_relation::parent_id.lt(post_relation::child_id))
            .load(&mut state.connection_pool.get_blocking()?)?;
        info!("Examining {} related post pairs", pairs.len());

        // Merges mutate many shared tables and can cascade into other pairs,
        // so pairs are processed sequentially.
        for (merge_to_id, absorbed_id) in pairs {
            admin::is_cancelled()?;
            if !selected.contains(&merge_to_id) || !selected.contains(&absorbed_id) {
                continue;
            }
            merge_pair_if_duplicate(state, merge_to_id, absorbed_id, &merged, &skipped, &failed);
        }

        merged.report();
        skipped.report();
        failed.report();
        if !state.config.delete_source_files {
            info!(
                "delete_source_files is disabled, so absorbed posts' files were left on disk. \
                 Enable it in config.toml for merges to free disk space."
            );
        }
        Ok(())
    });
}

/// Verifies that a candidate pair is pixel-identical and merges `absorbed_id` into
/// `merge_to_id` if so. `merge_to_id` must be the lower ID of the pair.
fn merge_pair_if_duplicate(
    state: &AppState,
    merge_to_id: i64,
    absorbed_id: i64,
    merged: &ProgressReporter,
    skipped: &ProgressReporter,
    failed: &ProgressReporter,
) {
    let mut conn = match state.connection_pool.get_blocking() {
        Ok(conn) => conn,
        Err(err) => {
            error!("Cannot merge posts {merge_to_id} and {absorbed_id}: could not get a database connection: {err}");
            failed.increment();
            return;
        }
    };

    let load_result = post::table
        .find(merge_to_id)
        .select(Post::as_select())
        .first(&mut conn)
        .optional()
        .and_then(|merge_to| {
            post::table
                .find(absorbed_id)
                .select(Post::as_select())
                .first(&mut conn)
                .optional()
                .map(|absorbed| merge_to.zip(absorbed))
        });
    let (merge_to, absorbed) = match load_result {
        Ok(Some(posts)) => posts,
        // One of the posts no longer exists, e.g. it was absorbed by an earlier merge.
        Ok(None) => {
            info!("Posts {merge_to_id} and {absorbed_id}: skipped — one of them no longer exists");
            return;
        }
        Err(err) => {
            error!("Cannot retrieve posts {merge_to_id} and {absorbed_id}: {err}");
            failed.increment();
            return;
        }
    };

    // Pixel comparison of a single frame only proves identity for still images.
    if merge_to.type_ != PostType::Image || absorbed.type_ != PostType::Image {
        info!("Posts {merge_to_id} and {absorbed_id}: skipped — related pair is not a pair of still images");
        skipped.increment();
        return;
    }

    let merge_to_path = PostHash::new(&state.config, merge_to_id).content_path(merge_to.mime_type);
    let absorbed_path = PostHash::new(&state.config, absorbed_id).content_path(absorbed.mime_type);
    let merge_to_image = match decode::image(&merge_to_path, merge_to.mime_type) {
        Ok(image) => image,
        Err(err) => {
            error!("Cannot decode content for post {merge_to_id} from {}: {err}", merge_to_path.display());
            failed.increment();
            return;
        }
    };
    let absorbed_image = match decode::image(&absorbed_path, absorbed.mime_type) {
        Ok(image) => image,
        Err(err) => {
            error!("Cannot decode content for post {absorbed_id} from {}: {err}", absorbed_path.display());
            failed.increment();
            return;
        }
    };

    if merge_to_image.dimensions() != absorbed_image.dimensions()
        || merge_to_image.to_rgba8().as_raw() != absorbed_image.to_rgba8().as_raw()
    {
        info!("Posts {merge_to_id} and {absorbed_id}: skipped — contents are not pixel-identical");
        skipped.increment();
        return;
    }

    // Keep the higher-resolution file; on equal resolution prefer JXL, then the smaller file.
    // Pixel-identical content means the resolutions always match today, so the tiebreakers are
    // what actually decide; the resolution comparison is what should hold first if the duplicate
    // check is ever relaxed to accept content that isn't the same size. Dimensions come from the
    // decoded images rather than the database so a stale width/height can't pick the wrong file.
    let absorbed_pixels = u64::from(absorbed_image.width()) * u64::from(absorbed_image.height());
    let merge_to_pixels = u64::from(merge_to_image.width()) * u64::from(merge_to_image.height());
    let keep_absorbed_content = match absorbed_pixels.cmp(&merge_to_pixels) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match (absorbed.mime_type == MimeType::Jxl, merge_to.mime_type == MimeType::Jxl) {
            (true, false) => true,
            (false, true) => false,
            // Both JXL, or neither: fall back to the smaller file.
            _ => absorbed.file_size < merge_to.file_size,
        },
    };

    let merge_result: ApiResult<()> = conn.transaction(|conn| {
        update::post::merge(conn, &state.config, &absorbed, &merge_to, keep_absorbed_content)?;
        snapshot::post::merge_snapshot(conn, admin::client(), absorbed_id, merge_to_id)?;
        Ok(())
    });
    match merge_result {
        Ok(()) => {
            let (kept_mime, kept_size, kept_pixels) = if keep_absorbed_content {
                (absorbed.mime_type, absorbed.file_size, absorbed_pixels)
            } else {
                (merge_to.mime_type, merge_to.file_size, merge_to_pixels)
            };
            info!(
                "Merged post {absorbed_id} into post {merge_to_id} \
                 (kept {kept_mime} content, {kept_pixels} px, {kept_size} B)"
            );
            merged.increment();
        }
        Err(err) => {
            error!("Failed to merge post {absorbed_id} into post {merge_to_id}: {err}");
            failed.increment();
        }
    }
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

/// Number of post IDs per bulk metadata query.
const METADATA_CHUNK_SIZE: usize = 10_000;

/// Loads metadata for `post_ids` in chunks, using a single database connection.
///
/// Tasks fetch what they need from the database up front so that their parallel workers don't
/// have to hold a connection while decoding, hashing, or encoding a file. A connection held
/// across that kind of work is a connection no other worker can use, which drains the pool and
/// eventually fails the task with a connection error. Rows are returned in ID order, which is
/// also the order the data directory is laid out in.
///
/// Posts deleted between the selection query and this one are simply absent from the result.
fn load_metadata_in_chunks<T, F>(state: &AppState, post_ids: &[i64], mut load_chunk: F) -> AdminResult<Vec<T>>
where
    F: FnMut(&mut PgConnection, &[i64]) -> QueryResult<Vec<T>>,
{
    let mut conn = state.connection_pool.get_blocking()?;
    let mut metadata = Vec::with_capacity(post_ids.len());
    for chunk in post_ids.chunks(METADATA_CHUNK_SIZE) {
        admin::is_cancelled()?;

        metadata.extend(load_chunk(&mut conn, chunk)?);
    }
    Ok(metadata)
}

/// Returns the `(id, mime_type)` of every post in `post_ids`.
fn post_mime_types(state: &AppState, post_ids: &[i64]) -> AdminResult<Vec<(i64, MimeType)>> {
    load_metadata_in_chunks(state, post_ids, |conn, chunk| {
        post::table
            .select((post::id, post::mime_type))
            .filter(post::id.eq_any(chunk))
            .order(post::id)
            .load(conn)
    })
}

/// Returns the `(id, mime_type, checksum)` of every post in `post_ids`.
fn post_checksums(state: &AppState, post_ids: &[i64]) -> AdminResult<Vec<(i64, MimeType, Checksum)>> {
    load_metadata_in_chunks(state, post_ids, |conn, chunk| {
        post::table
            .select((post::id, post::mime_type, post::checksum))
            .filter(post::id.eq_any(chunk))
            .order(post::id)
            .load(conn)
    })
}

/// Returns the `(id, mime_type, phash)` of every post in `post_ids`.
fn post_phashes(state: &AppState, post_ids: &[i64]) -> AdminResult<Vec<(i64, MimeType, Option<i64>)>> {
    load_metadata_in_chunks(state, post_ids, |conn, chunk| {
        post::table
            .select((post::id, post::mime_type, post::phash))
            .filter(post::id.eq_any(chunk))
            .order(post::id)
            .load(conn)
    })
}

/// Returns the `(id, mime_type)` of every post in `post_ids` that has no pHash yet,
/// counting the posts that already have one as skipped.
fn posts_missing_phash(
    state: &AppState,
    post_ids: &[i64],
    skipped: &ProgressReporter,
) -> AdminResult<Vec<(i64, MimeType)>> {
    let mut targets = Vec::new();
    for (post_id, mime_type, phash) in post_phashes(state, post_ids)? {
        if phash.is_some() {
            skipped.increment();
        } else {
            targets.push((post_id, mime_type));
        }
    }
    Ok(targets)
}

/// Returns the `(id, mime_type, generated_thumbnail_size)` of every post in `post_ids`.
fn thumbnail_metadata(state: &AppState, post_ids: &[i64]) -> AdminResult<Vec<(i64, MimeType, i64)>> {
    load_metadata_in_chunks(state, post_ids, |conn, chunk| {
        post::table
            .select((post::id, post::mime_type, post::generated_thumbnail_size))
            .filter(post::id.eq_any(chunk))
            .order(post::id)
            .load(conn)
    })
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
