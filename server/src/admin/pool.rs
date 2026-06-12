use crate::admin::input::PostEditor;
use crate::admin::{self, input};
use crate::app::{AppState, Context};
use crate::comic;
use crate::model::enums::PostSafety;
use crate::time::Timer;
use std::path::PathBuf;
use std::sync::Arc;

/// Imports a CBZ archive as a pool. Pages are matched against existing posts
/// by exact checksum first and perceptual similarity second; pages without a
/// match are uploaded as new posts through the regular content pipeline.
pub fn import_cbz_as_pool(state: &AppState, editor: &mut PostEditor) {
    input::user_input_loop(state, editor, |state: &AppState, editor: &mut PostEditor| {
        let path_input = input::read("Path to CBZ file: ", editor)?;
        let cbz_path = PathBuf::from(path_input.trim());
        if !cbz_path.is_file() {
            return Err(format!("{} is not a file", cbz_path.display()).into());
        }

        let default_name = cbz_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().replace(char::is_whitespace, "_"))
            .unwrap_or_default();
        let name_input = input::read(&format!("Pool name [{default_name}]: "), editor)?;
        let pool_name = if name_input.trim().is_empty() {
            default_name
        } else {
            name_input.trim().to_owned()
        };

        let safety_input = input::read("Safety for newly created posts (safe/sketchy/unsafe) [safe]: ", editor)?;
        let safety = match safety_input.trim().to_ascii_lowercase().as_str() {
            "" | "safe" => PostSafety::Safe,
            "sketchy" => PostSafety::Sketchy,
            "unsafe" => PostSafety::Unsafe,
            other => return Err(format!("Invalid safety: {other}").into()),
        };

        // Admin tasks have no request context, so build one for the upload pipeline.
        let context = Context {
            client: admin::client(),
            config: Arc::clone(&state.config),
            content_cache: Arc::clone(&state.content_cache),
            av1_supported: state.av1_supported,
        };

        let _timer = Timer::new("import_cbz_as_pool");
        let mut conn = state.connection_pool.get_blocking()?;
        let import_result =
            comic::import_archive_as_pool(&context, &mut conn, &cbz_path, &pool_name, safety, &|| {
                admin::is_cancelled().is_err()
            });

        // Convert a Ctrl+C during the import into a graceful cancellation.
        admin::is_cancelled()?;
        import_result?;
        Ok(())
    });
}
