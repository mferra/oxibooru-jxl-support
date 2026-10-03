use crate::admin::input::{self, CancelType, PostEditor};
use crate::admin::{AdminError, AdminResult, PRINT_INTERVAL, ProgressReporter};
use crate::app::AppState;
use crate::content::hash::PostHash;
use crate::filesystem::Directory;
use crate::model::enums::MimeType;
use crate::schema::{
    comment, comment_score, comment_statistics, database_statistics, pool, pool_category, pool_category_statistics,
    pool_post, pool_statistics, post, post_favorite, post_feature, post_note, post_relation, post_score,
    post_statistics, post_tag, tag, tag_category, tag_category_statistics, tag_implication, tag_statistics,
    tag_suggestion, user, user_statistics,
};
use crate::time::{DateTime, Timer};
use crate::{admin, filesystem};
use diesel::connection::DefaultLoadingMode;
use diesel::dsl::{count, max, sum};
use diesel::{ExpressionMethods, NullableExpressionMethods, QueryDsl, RunQueryDsl};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{Level, debug, error, info, warn};
use walkdir::WalkDir;

/// Renames post files and thumbnails.
/// Useful when the content hash changes.
pub fn reset_filenames(state: &AppState) {
    if let Err(err) = reset_filenames_impl(state) {
        error!("{err}");
    }
}

pub fn reset_filenames_impl(state: &AppState) -> AdminResult<()> {
    let _timer = Timer::new("reset_filenames");
    if state.config.path(Directory::GeneratedThumbnails).try_exists()? {
        let progress = ProgressReporter::new(Level::INFO, "Generated thumbnails renamed", PRINT_INTERVAL);
        for entry in WalkDir::new(state.config.path(Directory::GeneratedThumbnails)) {
            admin::is_cancelled()?;

            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }

            let Some(post_id) = admin::get_post_id(path) else {
                error!("Cannot determine post ID from file name of {} (expected \"<id>_<hash>.<ext>\"); skipping", path.display());
                continue;
            };

            let new_path = PostHash::new(&state.config, post_id, None).generated_thumbnail_path();
            if path != new_path {
                if let Err(err) = filesystem::move_file(path, &new_path) {
                    error!("Could not move {} to {} for reason: {err}", path.display(), new_path.display());
                }
                progress.increment();
            }
        }
    }
    if state.config.path(Directory::CustomThumbnails).try_exists()? {
        let progress = ProgressReporter::new(Level::INFO, "Custom thumbnails renamed", PRINT_INTERVAL);
        for entry in WalkDir::new(state.config.path(Directory::CustomThumbnails)) {
            admin::is_cancelled()?;

            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }

            let Some(post_id) = admin::get_post_id(path) else {
                error!("Cannot determine post ID from file name of {} (expected \"<id>_<hash>.<ext>\"); skipping", path.display());
                continue;
            };

            let new_path = PostHash::new(&state.config, post_id, None).custom_thumbnail_path();
            if path != new_path {
                if let Err(err) = filesystem::move_file(path, &new_path) {
                    error!("Could not move {} to {} for reason: {err}", path.display(), new_path.display());
                }
                progress.increment();
            }
        }
    }
    if state.config.path(Directory::Posts).try_exists()? {
        let progress = ProgressReporter::new(Level::INFO, "Posts renamed", PRINT_INTERVAL);
        for entry in WalkDir::new(state.config.path(Directory::Posts)) {
            admin::is_cancelled()?;

            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }

            let Some(post_id) = admin::get_post_id(path) else {
                error!("Cannot determine post ID from file name of {} (expected \"<id>_<hash>.<ext>\"); skipping", path.display());
                continue;
            };

            let new_path = if let Some(mime_type) = MimeType::from_path(path) {
                PostHash::new(&state.config, post_id, None).content_path(mime_type)
            } else {
                if let Some(extension) = path.extension().map(OsStr::to_string_lossy) {
                    warn!("Post {post_id} has unsupported file extension {extension}");
                } else {
                    warn!("Post {post_id} has no file extension");
                }

                let mut new_path = PostHash::new(&state.config, post_id, None).content_path(MimeType::Png);
                new_path.set_extension(path.extension().unwrap_or(OsStr::new("")));
                new_path
            };

            if path != new_path {
                if let Err(err) = filesystem::move_file(path, &new_path) {
                    error!("Could not move {} to {} for reason: {err}", path.display(), new_path.display());
                }
                progress.increment();
            }
        }
    }
    Ok(())
}

/// Why a file in a post data directory counts as an orphan.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OrphanKind {
    /// The file name doesn't start with a post ID.
    UnrecognizedName,
    /// No post has the file's ID, e.g. a deleted or merged post whose file was kept.
    NoPost,
    /// The post exists but uses a different file, e.g. the original left behind by a JXL
    /// conversion, a thumbnail in a format that is no longer configured, or a name that
    /// predates a content secret change.
    Stale,
}

impl OrphanKind {
    const ALL: [Self; 3] = [Self::UnrecognizedName, Self::NoPost, Self::Stale];

    fn label(self) -> &'static str {
        match self {
            Self::UnrecognizedName => "unrecognized-name",
            Self::NoPost => "no-post",
            Self::Stale => "stale",
        }
    }
}

/// Returns the path a post's file is expected at within one of the post data directories.
type ExpectedPath = fn(&PostHash, MimeType) -> PathBuf;

/// Directories holding per-post files, each with the path the post's file is expected at.
const POST_FILE_DIRECTORIES: [(Directory, ExpectedPath); 3] = [
    (Directory::Posts, |post_hash, mime_type| post_hash.content_path(mime_type)),
    (Directory::GeneratedThumbnails, |post_hash, _| post_hash.generated_thumbnail_path()),
    (Directory::CustomThumbnails, |post_hash, _| post_hash.custom_thumbnail_path()),
];

/// Files scanned between progress reports. Data directories can hold tens of millions of files.
const ORPHAN_SCAN_PRINT_INTERVAL: Option<u64> = Some(100_000);

/// Orphans deleted between progress reports.
const ORPHAN_DELETE_PRINT_INTERVAL: Option<u64> = Some(10_000);

/// Orphans modified more recently than this are never deleted: they may belong to an upload or
/// a conversion that was still running when the database was read.
const MIN_ORPHAN_AGE: Duration = Duration::from_secs(60 * 60);

const MIB: f64 = 1024.0 * 1024.0;

/// A file in a post data directory that no post uses.
struct Orphan {
    kind: OrphanKind,
    path: PathBuf,
    post_id: Option<i64>,
    size: u64,
    expected_path: ExpectedPath,
}

/// Lists files in the post content, generated thumbnail, and custom thumbnail directories that
/// no post uses, then offers to delete them.
///
/// With delete_source_files disabled, deleted and merged posts leave their files behind, and so
/// do JXL conversions and thumbnail format changes. The list can optionally be written to a file,
/// one `<kind>\t<path>` line per orphan. After the summary, the operator picks which kinds to
/// delete and confirms by typing "delete"; any other answer, or running non-interactively,
/// deletes nothing.
pub fn find_orphan_files(state: &AppState, editor: &mut PostEditor) {
    match find_and_delete_orphan_files(state, editor) {
        Ok(()) => (),
        Err(AdminError::Cancel(CancelType::Exit)) => std::process::exit(0),
        Err(err) => error!("{err}"),
    }
}

fn find_and_delete_orphan_files(state: &AppState, editor: &mut PostEditor) -> AdminResult<()> {
    let report_path = input::read("File to write the orphan list to (leave blank to only log it): ", editor)?;
    let report_path = (!report_path.is_empty()).then(|| PathBuf::from(report_path));
    let orphans = find_orphan_files_impl(state, report_path.as_deref())?;
    if orphans.is_empty() {
        return Ok(());
    }

    // Answering "done" here, as the non-interactive mock editor does, keeps every file.
    let kinds = match read_kinds_to_delete(editor) {
        Ok(kinds) => kinds,
        Err(CancelType::Stop) => Vec::new(),
        Err(err) => return Err(err.into()),
    };
    let selected: Vec<&Orphan> = orphans.iter().filter(|orphan| kinds.contains(&orphan.kind)).collect();
    if selected.is_empty() {
        info!("No orphan files deleted");
        return Ok(());
    }

    let selected_size: u64 = selected.iter().map(|orphan| orphan.size).sum();
    let prompt = format!(
        "Type \"delete\" to delete {} files ({:.1} MiB), or anything else to keep them: ",
        selected.len(),
        selected_size as f64 / MIB
    );
    match input::read(&prompt, editor) {
        Ok(answer) if answer == "delete" => delete_orphans(state, &selected),
        Ok(_) | Err(CancelType::Stop) => {
            info!("No orphan files deleted");
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

/// Asks which kinds of orphans to delete, re-prompting on unknown kinds. An empty answer selects
/// none, so every file is kept.
fn read_kinds_to_delete(editor: &mut PostEditor) -> Result<Vec<OrphanKind>, CancelType> {
    const PROMPT: &str = "Kinds of orphans to delete, separated by commas (no-post, stale, unrecognized-name, \
                          or all; leave blank to keep everything): ";
    loop {
        match parse_orphan_kinds(&input::read(PROMPT, editor)?) {
            Ok(kinds) => return Ok(kinds),
            Err(unknown) => error!("Unknown orphan kind \"{unknown}\""),
        }
    }
}

/// Parses a comma-separated list of orphan kind labels, or "all". Returns the first unknown label
/// as the error.
fn parse_orphan_kinds(answer: &str) -> Result<Vec<OrphanKind>, String> {
    let mut kinds = Vec::new();
    for word in answer.split(',').map(str::trim).filter(|word| !word.is_empty()) {
        if word == "all" {
            return Ok(OrphanKind::ALL.to_vec());
        }
        let kind = OrphanKind::ALL
            .into_iter()
            .find(|kind| kind.label() == word)
            .ok_or_else(|| word.to_owned())?;
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    Ok(kinds)
}

/// Scans the post data directories, logs or writes the list of orphans and a summary, and returns
/// the orphans found.
fn find_orphan_files_impl(state: &AppState, report_path: Option<&Path>) -> AdminResult<Vec<Orphan>> {
    // Created before the scan so a bad path fails immediately rather than after hours of work.
    let mut report = report_path.map(File::create).transpose()?.map(BufWriter::new);

    let _timer = Timer::new("find_orphan_files");
    let mime_types = load_post_mime_types(state)?;
    info!("Loaded {} posts; scanning data directories", mime_types.len());

    let scanned = ProgressReporter::new(Level::INFO, "Files scanned", ORPHAN_SCAN_PRINT_INTERVAL);
    let mut orphans = Vec::new();
    for (directory, expected_path) in POST_FILE_DIRECTORIES {
        let root = state.config.path(directory);
        if !root.try_exists()? {
            continue;
        }

        for entry in WalkDir::new(&root).sort_by_file_name() {
            admin::is_cancelled()?;

            let entry = entry?;
            if entry.file_type().is_dir() {
                continue;
            }
            scanned.increment();

            let path = entry.path();
            let post_id = admin::get_post_id(path);
            let kind = match post_id {
                None => OrphanKind::UnrecognizedName,
                Some(post_id) => match mime_types.get(&post_id) {
                    None => OrphanKind::NoPost,
                    Some(&mime_type) => {
                        if path == expected_path(&PostHash::new(&state.config, post_id, None), mime_type) {
                            continue;
                        }
                        OrphanKind::Stale
                    }
                },
            };

            let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            if let Some(report) = report.as_mut() {
                writeln!(report, "{}\t{}", kind.label(), path.display())?;
                debug!("Orphan file ({}): {}", kind.label(), path.display());
            } else {
                info!("Orphan file ({}): {}", kind.label(), path.display());
            }
            orphans.push(Orphan {
                kind,
                path: entry.into_path(),
                post_id,
                size,
                expected_path,
            });
        }
    }

    if let Some(mut report) = report {
        report.flush()?;
    }
    drop(scanned);

    for kind in OrphanKind::ALL {
        let (count, size) = orphans
            .iter()
            .filter(|orphan| orphan.kind == kind)
            .fold((0, 0), |(count, size), orphan| (count + 1, size + orphan.size));
        info!("Orphan files ({}): {count} ({:.1} MiB)", kind.label(), size as f64 / MIB);
    }
    let total_size: u64 = orphans.iter().map(|orphan| orphan.size).sum();
    info!("Orphan files in total: {} ({:.1} MiB)", orphans.len(), total_size as f64 / MIB);
    if let Some(report_path) = report_path {
        info!("Orphan list written to {}", report_path.display());
    }
    Ok(orphans)
}

/// Deletes `orphans`, re-checking each one first because the server keeps running and may have
/// changed the database or the data directory since the scan.
fn delete_orphans(state: &AppState, orphans: &[&Orphan]) -> AdminResult<()> {
    let _timer = Timer::new("delete_orphan_files");
    let mut post_ids: Vec<i64> = orphans.iter().filter_map(|orphan| orphan.post_id).collect();
    post_ids.sort_unstable();
    post_ids.dedup();
    let current_mime_types = load_mime_types_of(state, &post_ids)?;

    let deleted = ProgressReporter::new(Level::INFO, "Orphan files deleted", ORPHAN_DELETE_PRINT_INTERVAL);
    let kept = ProgressReporter::new(Level::INFO, "Orphan files kept after re-checking", None);
    let failed = ProgressReporter::new(Level::WARN, "Orphan files that could not be deleted", None);
    let mut freed: u64 = 0;
    for orphan in orphans {
        admin::is_cancelled()?;

        if let Some(reason) = reason_to_keep(state, &current_mime_types, orphan) {
            info!("Kept {}: {reason}", orphan.path.display());
            kept.increment();
            continue;
        }
        match std::fs::remove_file(&orphan.path) {
            Ok(()) => {
                debug!("Deleted {}", orphan.path.display());
                freed += orphan.size;
                deleted.increment();
            }
            Err(err) => {
                error!("Could not delete {}: {err}", orphan.path.display());
                failed.increment();
            }
        }
    }
    info!("Freed {:.1} MiB", freed as f64 / MIB);
    Ok(())
}

/// Returns why `orphan` must not be deleted, if anything. `current_mime_types` holds the MIME type
/// of every orphan's post that still exists, read after the operator confirmed the deletion.
///
/// A stale file is only deleted when its post's own file exists. Otherwise the stale file may be
/// the only copy: content named for an old content secret, say, or a custom thumbnail saved in
/// the previous thumbnail format.
fn reason_to_keep(
    state: &AppState,
    current_mime_types: &HashMap<i64, MimeType>,
    orphan: &Orphan,
) -> Option<&'static str> {
    match std::fs::metadata(&orphan.path).and_then(|metadata| metadata.modified()) {
        Err(err) if err.kind() == ErrorKind::NotFound => return Some("it no longer exists"),
        Err(_) => return Some("its modification time can't be read"),
        // A modification time in the future also counts as recent.
        Ok(modified) if modified.elapsed().unwrap_or(Duration::ZERO) < MIN_ORPHAN_AGE => {
            return Some("it was modified within the last hour");
        }
        Ok(_) => (),
    }

    // A file without a post ID only has the age check to pass.
    let post_id = orphan.post_id?;
    let current_mime_type = current_mime_types.get(&post_id);
    match (orphan.kind, current_mime_type) {
        (OrphanKind::UnrecognizedName, _) | (OrphanKind::NoPost, None) => None,
        (OrphanKind::NoPost, Some(_)) => Some("its post exists now"),
        (OrphanKind::Stale, None) => Some("its post no longer exists"),
        (OrphanKind::Stale, Some(&mime_type)) => {
            let expected = (orphan.expected_path)(&PostHash::new(&state.config, post_id, None), mime_type);
            if expected == orphan.path {
                Some("its post uses it now")
            } else if !expected.exists() {
                Some("its post's own file is missing, so this may be the only copy")
            } else {
                None
            }
        }
    }
}

/// Returns the MIME type of every post, keyed by post ID. Streamed so that only the map, not an
/// intermediate list, has to fit in memory.
fn load_post_mime_types(state: &AppState) -> AdminResult<HashMap<i64, MimeType>> {
    let mut conn = state.connection_pool.get_blocking()?;
    let mut mime_types = HashMap::new();
    for row in post::table
        .select((post::id, post::mime_type))
        .load_iter::<(i64, MimeType), DefaultLoadingMode>(&mut conn)?
    {
        let (post_id, mime_type) = row?;
        mime_types.insert(post_id, mime_type);
    }
    Ok(mime_types)
}

/// Returns the MIME type of each post in `post_ids` that exists, keyed by post ID.
fn load_mime_types_of(state: &AppState, post_ids: &[i64]) -> AdminResult<HashMap<i64, MimeType>> {
    const CHUNK_SIZE: usize = 10_000;
    let mut conn = state.connection_pool.get_blocking()?;
    let mut mime_types = HashMap::new();
    for chunk in post_ids.chunks(CHUNK_SIZE) {
        admin::is_cancelled()?;

        let rows: Vec<(i64, MimeType)> = post::table
            .select((post::id, post::mime_type))
            .filter(post::id.eq_any(chunk))
            .load(&mut conn)?;
        mime_types.extend(rows);
    }
    Ok(mime_types)
}

/// Updates database values for thumbnail size.
pub fn reset_thumbnail_sizes(state: &AppState) {
    if let Err(err) = reset_thumbnail_sizes_impl(state) {
        error!("{err}");
    }
}

pub fn reset_thumbnail_sizes_impl(state: &AppState) -> AdminResult<()> {
    let _timer = Timer::new("reset_thumbnail_sizes");
    let mut conn = state.connection_pool.get_blocking()?;
    if state.config.path(Directory::Avatars).try_exists()? {
        let progress = ProgressReporter::new(Level::INFO, "Avatar sizes cached", PRINT_INTERVAL);
        for entry in WalkDir::new(state.config.path(Directory::Avatars)) {
            admin::is_cancelled()?;

            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }

            let Some(username) = path.file_name().map(OsStr::to_string_lossy) else {
                error!("Unable to convert file name of {} to string", path.display());
                continue;
            };

            let file_size = filesystem::file_size(path)
                .map_err(|err| format!("Cannot read size of {}: {err}", path.display()))?;
            diesel::update(user::table)
                .set(user::custom_avatar_size.eq(file_size))
                .filter(user::name.eq(username))
                .execute(&mut conn)?;
            progress.increment();
        }
    }
    if state.config.path(Directory::CustomThumbnails).try_exists()? {
        let progress = ProgressReporter::new(Level::INFO, "Custom thumbnails sizes cached", PRINT_INTERVAL);
        for entry in WalkDir::new(state.config.path(Directory::CustomThumbnails)) {
            admin::is_cancelled()?;

            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }

            let Some(post_id) = admin::get_post_id(path) else {
                error!("Could not find post_id of {}", path.display());
                continue;
            };

            let file_size = filesystem::file_size(path)?;
            diesel::update(post::table)
                .set(post::custom_thumbnail_size.eq(file_size))
                .filter(post::id.eq(post_id))
                .execute(&mut conn)?;
            progress.increment();
        }
    }
    if state.config.path(Directory::GeneratedThumbnails).try_exists()? {
        let progress = ProgressReporter::new(Level::INFO, "Generated thumbnail sizes cached", PRINT_INTERVAL);
        for entry in WalkDir::new(state.config.path(Directory::GeneratedThumbnails)) {
            admin::is_cancelled()?;

            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }

            let Some(post_id) = admin::get_post_id(path) else {
                error!("Cannot determine post ID from file name of {} (expected \"<id>_<hash>.<ext>\"); skipping", path.display());
                continue;
            };

            let file_size = filesystem::file_size(path)
                .map_err(|err| format!("Cannot read size of {}: {err}", path.display()))?;
            diesel::update(post::table)
                .set(post::generated_thumbnail_size.eq(file_size))
                .filter(post::id.eq(post_id))
                .execute(&mut conn)?;
            progress.increment();
        }
    }
    if state.config.path(Directory::CustomThumbnails).try_exists()? {
        let progress = ProgressReporter::new(Level::INFO, "Custom thumbnails sizes cached", PRINT_INTERVAL);
        for entry in WalkDir::new(state.config.path(Directory::CustomThumbnails)) {
            admin::is_cancelled()?;

            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }

            let Some(post_id) = admin::get_post_id(path) else {
                error!("Cannot determine post ID from file name of {} (expected \"<id>_<hash>.<ext>\"); skipping", path.display());
                continue;
            };

            let file_size = filesystem::file_size(path)
                .map_err(|err| format!("Cannot read size of {}: {err}", path.display()))?;
            diesel::update(post::table)
                .set(post::custom_thumbnail_size.eq(file_size))
                .filter(post::id.eq(post_id))
                .execute(&mut conn)?;
            progress.increment();
        }
    }
    Ok(())
}

/// Recomputes database table statistics. Useful for when new statistics are added
/// or a bug is found in statistics updaters.
///
/// Because it computes statistics one row at a time, this function is fairly slow.
/// A much faster version of this is done in `scripts/convert_szuru_database.sql`,
/// but it would be very tricky to implement in Diesel.
pub fn reset_relation_stats(state: &AppState) -> AdminResult<()> {
    let mut conn = state.connection_pool.get_blocking()?;

    let comment_count: i64 = comment::table.count().first(&mut conn)?;
    let pool_count: i64 = pool::table.count().first(&mut conn)?;
    let post_count: i64 = post::table.count().first(&mut conn)?;
    let tag_count: i64 = tag::table.count().first(&mut conn)?;
    let user_count: i64 = user::table.count().first(&mut conn)?;
    diesel::update(database_statistics::table)
        .set((
            database_statistics::comment_count.eq(comment_count),
            database_statistics::pool_count.eq(pool_count),
            database_statistics::post_count.eq(post_count),
            database_statistics::tag_count.eq(tag_count),
            database_statistics::user_count.eq(user_count),
        ))
        .execute(&mut conn)?;

    let comment_stats: Vec<(i64, Option<i64>)> = comment::table
        .left_join(comment_score::table)
        .group_by(comment::id)
        .select((comment::id, sum(comment_score::score).nullable()))
        .load(&mut conn)?;
    let progress = ProgressReporter::new(Level::INFO, "Comment statistics calculated", PRINT_INTERVAL);
    for (comment_id, score) in comment_stats {
        admin::is_cancelled()?;
        diesel::update(comment_statistics::table.find(comment_id))
            .set(comment_statistics::score.eq(score.unwrap_or(0)))
            .execute(&mut conn)?;
        progress.increment();
    }

    let pool_category_stats: Vec<(i64, Option<i64>)> = pool_category::table
        .left_join(pool::table)
        .group_by(pool_category::id)
        .select((pool_category::id, count(pool::id).nullable()))
        .load(&mut conn)?;
    let progress = ProgressReporter::new(Level::INFO, "Pool category statistics calculated", PRINT_INTERVAL);
    for (category_id, usage_count) in pool_category_stats {
        admin::is_cancelled()?;
        diesel::update(pool_category_statistics::table.find(category_id))
            .set(pool_category_statistics::usage_count.eq(usage_count.unwrap_or(0)))
            .execute(&mut conn)?;
        progress.increment();
    }

    let pool_stats: Vec<(i64, Option<i64>)> = pool::table
        .left_join(pool_post::table)
        .group_by(pool::id)
        .select((pool::id, count(pool_post::post_id).nullable()))
        .load(&mut conn)?;
    let progress = ProgressReporter::new(Level::INFO, "Pool statistics calculated", PRINT_INTERVAL);
    for (pool_id, post_count) in pool_stats {
        admin::is_cancelled()?;
        diesel::update(pool_statistics::table.find(pool_id))
            .set(pool_statistics::post_count.eq(post_count.unwrap_or(0)))
            .execute(&mut conn)?;
        progress.increment();
    }

    let tag_category_stats: Vec<(i64, Option<i64>)> = tag_category::table
        .left_join(tag::table)
        .group_by(tag_category::id)
        .select((tag_category::id, count(tag::id).nullable()))
        .load(&mut conn)?;
    let progress = ProgressReporter::new(Level::INFO, "Tag category statistics calculated", PRINT_INTERVAL);
    for (category_id, usage_count) in tag_category_stats {
        admin::is_cancelled()?;
        diesel::update(tag_category_statistics::table.find(category_id))
            .set(tag_category_statistics::usage_count.eq(usage_count.unwrap_or(0)))
            .execute(&mut conn)?;
        progress.increment();
    }

    let tag_ids: Vec<i64> = tag::table.select(tag::id).load(&mut conn)?;
    let progress = ProgressReporter::new(Level::INFO, "Tag statistics calculated", PRINT_INTERVAL);
    for tag_id in tag_ids {
        admin::is_cancelled()?;
        let usage_count: i64 = post_tag::table
            .filter(post_tag::tag_id.eq(tag_id))
            .count()
            .first(&mut conn)?;
        let implication_count: i64 = tag_implication::table
            .filter(tag_implication::child_id.eq(tag_id))
            .count()
            .first(&mut conn)?;
        let suggestion_count: i64 = tag_suggestion::table
            .filter(tag_suggestion::child_id.eq(tag_id))
            .count()
            .first(&mut conn)?;
        diesel::update(tag_statistics::table.find(tag_id))
            .set((
                tag_statistics::usage_count.eq(usage_count),
                tag_statistics::implication_count.eq(implication_count),
                tag_statistics::suggestion_count.eq(suggestion_count),
            ))
            .execute(&mut conn)?;
        progress.increment();
    }

    let user_ids: Vec<i64> = user::table.select(user::id).load(&mut conn)?;
    let progress = ProgressReporter::new(Level::INFO, "User statistics calculated", PRINT_INTERVAL);
    for user_id in user_ids {
        admin::is_cancelled()?;
        let comment_count: i64 = comment::table
            .filter(comment::user_id.eq(user_id))
            .count()
            .first(&mut conn)?;
        let favorite_count: i64 = post_favorite::table
            .filter(post_favorite::user_id.eq(user_id))
            .count()
            .first(&mut conn)?;
        let upload_count: i64 = post::table.filter(post::user_id.eq(user_id)).count().first(&mut conn)?;
        diesel::update(user_statistics::table.find(user_id))
            .set((
                user_statistics::comment_count.eq(comment_count),
                user_statistics::favorite_count.eq(favorite_count),
                user_statistics::upload_count.eq(upload_count),
            ))
            .execute(&mut conn)?;
        progress.increment();
    }

    let post_ids: Vec<i64> = post::table.select(post::id).load(&mut conn)?;
    let progress = ProgressReporter::new(Level::INFO, "Post statistics calculated", PRINT_INTERVAL);
    for post_id in post_ids {
        admin::is_cancelled()?;
        let tag_count: i64 = post_tag::table
            .filter(post_tag::post_id.eq(post_id))
            .count()
            .first(&mut conn)?;
        let pool_count: i64 = pool_post::table
            .filter(pool_post::post_id.eq(post_id))
            .count()
            .first(&mut conn)?;
        let note_count: i64 = post_note::table
            .filter(post_note::post_id.eq(post_id))
            .count()
            .first(&mut conn)?;
        let comment_count: i64 = comment::table
            .filter(comment::post_id.eq(post_id))
            .count()
            .first(&mut conn)?;
        let relation_count: i64 = post_relation::table
            .filter(post_relation::child_id.eq(post_id))
            .count()
            .first(&mut conn)?;
        let score: Option<i64> = post_score::table
            .select(sum(post_score::score))
            .filter(post_score::post_id.eq(post_id))
            .first(&mut conn)?;
        let favorite_count: i64 = post_favorite::table
            .filter(post_favorite::post_id.eq(post_id))
            .count()
            .first(&mut conn)?;
        let feature_count: i64 = post_feature::table
            .filter(post_feature::post_id.eq(post_id))
            .count()
            .first(&mut conn)?;
        let last_comment_time: Option<DateTime> = comment::table
            .select(max(comment::creation_time))
            .filter(comment::post_id.eq(post_id))
            .first(&mut conn)?;
        let last_favorite_time: Option<DateTime> = post_favorite::table
            .select(max(post_favorite::time))
            .filter(post_favorite::post_id.eq(post_id))
            .first(&mut conn)?;
        let last_feature_time: Option<DateTime> = post_feature::table
            .select(max(post_feature::time))
            .filter(post_feature::post_id.eq(post_id))
            .first(&mut conn)?;
        diesel::update(post_statistics::table.find(post_id))
            .set((
                post_statistics::tag_count.eq(tag_count),
                post_statistics::pool_count.eq(pool_count),
                post_statistics::note_count.eq(note_count),
                post_statistics::comment_count.eq(comment_count),
                post_statistics::relation_count.eq(relation_count),
                post_statistics::score.eq(score.unwrap_or(0)),
                post_statistics::favorite_count.eq(favorite_count),
                post_statistics::feature_count.eq(feature_count),
                post_statistics::last_comment_time.eq(last_comment_time),
                post_statistics::last_favorite_time.eq(last_favorite_time),
                post_statistics::last_feature_time.eq(last_feature_time),
            ))
            .execute(&mut conn)?;
        progress.increment();
    }
    Ok(())
}

/// Recalculates cached file sizes, row counts, and table statistics.
/// Useful for when the statistics become inconsistent with database
/// or when migrating from an older version without statistics.
pub fn reset_statistics(state: &AppState) {
    if let Err(err) = reset_statistics_impl(state) {
        error!("{err}");
    }
}

pub fn reset_statistics_impl(state: &AppState) -> AdminResult<()> {
    let _timer = Timer::new("reset_statistics");

    // Disk usage will automatically be incremented via triggers as we calculate
    // content, thumbnail, and avatar sizes
    let mut conn = state.connection_pool.get_blocking()?;
    diesel::update(database_statistics::table)
        .set(database_statistics::disk_usage.eq(0))
        .execute(&mut conn)?;

    if state.config.path(Directory::Posts).try_exists()? {
        let progress = ProgressReporter::new(Level::INFO, "Posts content sizes cached", PRINT_INTERVAL);
        for entry in WalkDir::new(state.config.path(Directory::Posts)) {
            admin::is_cancelled()?;

            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }

            let Some(post_id) = admin::get_post_id(path) else {
                error!("Cannot determine post ID from file name of {} (expected \"<id>_<hash>.<ext>\"); skipping", path.display());
                continue;
            };

            let file_size = filesystem::file_size(path)
                .map_err(|err| format!("Cannot read size of {}: {err}", path.display()))?;
            diesel::update(post::table)
                .set(post::file_size.eq(file_size))
                .filter(post::id.eq(post_id))
                .execute(&mut conn)?;
            progress.increment();
        }
    }
    reset_thumbnail_sizes_impl(state)?;
    reset_relation_stats(state)
}

#[cfg(test)]
mod test {
    use super::{OrphanKind, delete_orphans, find_orphan_files_impl, parse_orphan_kinds};
    use crate::admin::{self, AdminResult};
    use crate::content::hash::PostHash;
    use crate::filesystem::Directory;
    use crate::model::enums::MimeType;
    use crate::test::*;
    use serial_test::{parallel, serial};
    use std::fs::File;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    /// Creates a file at `path`, backdated by `age` so the deletion age guard can be exercised.
    fn plant(path: &Path, age: Duration) -> std::io::Result<()> {
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new("")))?;
        std::fs::write(path, b"orphan")?;
        File::options()
            .write(true)
            .open(path)?
            .set_modified(SystemTime::now() - age)
    }

    #[test]
    #[parallel]
    fn parse_kinds() {
        assert!(parse_orphan_kinds("").is_ok_and(|kinds| kinds.is_empty()));
        assert!(
            parse_orphan_kinds(" no-post , stale,no-post ")
                .is_ok_and(|kinds| kinds == [OrphanKind::NoPost, OrphanKind::Stale])
        );
        assert!(parse_orphan_kinds("stale, all").is_ok_and(|kinds| kinds == OrphanKind::ALL));
        assert_eq!(parse_orphan_kinds("no-post, orphans").err().as_deref(), Some("orphans"));
    }

    #[test]
    #[serial]
    fn find_orphan_files() -> AdminResult<()> {
        const OLD: Duration = Duration::from_secs(2 * 60 * 60);
        let state = get_state();
        let config = &state.config;

        // Post 1 is a JPEG, so a PNG beside it is the kind of file a JXL conversion leaves behind.
        let stale = PostHash::new(config, 1, None).content_path(MimeType::Png);
        let no_post = PostHash::new(config, 999, None).content_path(MimeType::Jpeg);
        let no_post_thumbnail = PostHash::new(config, 999, None).generated_thumbnail_path();
        let unrecognized = config.path(Directory::Posts).join("notes.txt");
        for path in [&stale, &no_post, &no_post_thumbnail, &unrecognized] {
            plant(path, OLD)?;
        }

        let report_path = config.data_dir.join("orphans.txt");
        let orphans = find_orphan_files_impl(&state, Some(&report_path))?;
        assert_eq!(orphans.len(), 4);

        let report = std::fs::read_to_string(&report_path)?;
        let mut lines: Vec<&str> = report.lines().collect();
        lines.sort_unstable();
        let mut expected = vec![
            format!("stale\t{}", stale.display()),
            format!("no-post\t{}", no_post.display()),
            format!("no-post\t{}", no_post_thumbnail.display()),
            format!("unrecognized-name\t{}", unrecognized.display()),
        ];
        expected.sort_unstable();
        assert_eq!(lines, expected, "files used by existing posts must not be reported");

        // Non-interactive runs answer the deletion prompt with "done", which must keep every file.
        super::find_orphan_files(&state, &mut admin::mock_editor());
        for path in [&stale, &no_post, &no_post_thumbnail, &unrecognized] {
            assert!(path.exists(), "non-interactive run must not delete {}", path.display());
        }

        reset_database();
        Ok(())
    }

    #[test]
    #[serial]
    fn delete_orphan_files() -> AdminResult<()> {
        const OLD: Duration = Duration::from_secs(2 * 60 * 60);
        let state = get_state();
        let config = &state.config;

        // Deleted: post 1's own JPEG exists, and post 999 doesn't exist.
        let stale = PostHash::new(config, 1, None).content_path(MimeType::Png);
        let no_post = PostHash::new(config, 999, None).content_path(MimeType::Jpeg);
        // Kept: modified too recently.
        let recent = PostHash::new(config, 998, None).content_path(MimeType::Jpeg);
        // Kept: post 3 has no custom thumbnail in the configured format, so this one in another
        // format may be the only copy.
        let only_copy = PostHash::new(config, 3, None).custom_thumbnail_path_with_ext("png");
        // Kept: not a selected kind.
        let unrecognized = config.path(Directory::Posts).join("notes.txt");
        for path in [&stale, &no_post, &only_copy, &unrecognized] {
            plant(path, OLD)?;
        }
        plant(&recent, Duration::ZERO)?;
        let used_content = PostHash::new(config, 1, None).content_path(MimeType::Jpeg);
        assert!(used_content.exists());

        let orphans = find_orphan_files_impl(&state, None)?;
        let selected: Vec<_> = orphans
            .iter()
            .filter(|orphan| matches!(orphan.kind, OrphanKind::NoPost | OrphanKind::Stale))
            .collect();
        delete_orphans(&state, &selected)?;

        assert!(!stale.exists(), "stale file whose post has its own file must be deleted");
        assert!(!no_post.exists(), "file without a post must be deleted");
        assert!(recent.exists(), "recently modified file must be kept");
        assert!(only_copy.exists(), "stale file whose post lacks its own file must be kept");
        assert!(unrecognized.exists(), "unselected kind must be kept");
        assert!(used_content.exists(), "file used by a post must be kept");

        reset_database();
        Ok(())
    }
}
