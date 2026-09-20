//! Local folders use ordinary sources and the existing media indexer.

use anyhow::Result;
use rusqlite::OptionalExtension;
use std::path::Path;

const STORAGE_PAUSE_PREFIX: &str = "CURATOR_STORAGE_PAUSE:";

fn relative_library_path(state: &crate::AppState, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(dunce::simplified(&state.library_dir))?
        .to_string_lossy()
        .replace('\\', "/"))
}

fn record_local_size_skip(
    state: &crate::AppState,
    source_id: i64,
    target: &Path,
    limit: u64,
    size: u64,
) -> Result<()> {
    let extension = target
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let kind = if crate::downloader::video_exts().contains(&extension.as_str()) {
        "video"
    } else {
        "image"
    };
    let relative = relative_library_path(state, target)?;
    let reason = format!(
        "Skipped: local file is {} bytes, above the configured {} byte download limit. Raise or remove the limit and re-sync to import it.",
        size, limit
    );
    let conn = state.pool.get()?;
    conn.execute(
        "INSERT INTO media(source_id,filepath,filename,type,added_at,downloaded,file_size_bytes,skip_reason,skip_limit_bytes,skipped_at)
         VALUES(?1,?2,?3,?4,?5,0,?6,?7,?8,?5)
         ON CONFLICT(filepath) DO UPDATE SET downloaded=0,missing=0,file_size_bytes=excluded.file_size_bytes,
            skip_reason=excluded.skip_reason,skip_limit_bytes=excluded.skip_limit_bytes,skipped_at=excluded.skipped_at,
            retention_deleted=0",
        rusqlite::params![
            source_id,
            relative,
            target.file_name().unwrap_or_default().to_string_lossy(),
            kind,
            crate::db::now_iso(),
            i64::try_from(size).unwrap_or(i64::MAX),
            reason,
            i64::try_from(limit).unwrap_or(i64::MAX),
        ],
    )?;
    Ok(())
}

pub fn import_folder(state: &crate::AppState, folder: &Path, group_id: Option<i64>) -> Result<i64> {
    let folder = dunce::canonicalize(folder)?;
    anyhow::ensure!(folder.is_dir(), "Select a folder");
    let library = dunce::canonicalize(&state.library_dir)?;
    anyhow::ensure!(
        !library.starts_with(&folder) && !folder.starts_with(&library),
        "Select a folder outside Curator's managed library"
    );
    let url = format!("local:{}", folder.to_string_lossy());
    let conn = state.pool.get()?;
    let existing: Option<i64> = conn
        .query_row("SELECT id FROM sources WHERE url=?1", [&url], |row| {
            row.get(0)
        })
        .optional()?;
    let id = if let Some(id) = existing {
        id
    } else {
        let name = folder.file_name().unwrap_or_default().to_string_lossy();
        let slug = format!("local-{:032x}", rand::random::<u128>());
        conn.execute(
            "INSERT INTO sources(name,url,slug,status,group_id,added_at)
             VALUES(?1,?2,?3,'pending',?4,?5)",
            rusqlite::params![name, url, slug, group_id, crate::db::now_iso()],
        )?;
        conn.last_insert_rowid()
    };
    drop(conn);
    sync_folder(state, id, &folder)?;
    Ok(id)
}

pub fn sync_folder(state: &crate::AppState, id: i64, folder: &Path) -> Result<()> {
    sync_folder_with_cancel(state, id, folder, None)
}

/// Synchronous folder scan used by the downloader's blocking worker.  The
/// optional cancellation token is checked between files so a source-level
/// pause can stop a large import without interrupting a filesystem operation.
pub fn sync_folder_with_cancel(
    state: &crate::AppState,
    id: i64,
    folder: &Path,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<()> {
    let slug: String =
        state
            .pool
            .get()?
            .query_row("SELECT slug FROM sources WHERE id=?1", [id], |row| {
                row.get(0)
            })?;
    let destination = state.library_dir.join(&slug);
    std::fs::create_dir_all(&destination)?;
    // Local imports retain their historical unrestricted behavior unless the
    // person explicitly opts into applying remote download limits to them.
    // `try_read` is non-blocking and safe in this worker thread; a transient
    // lock contention falls back to the conservative existing behavior.
    let settings = state
        .settings
        .try_read()
        .map(|settings| settings.clone())
        .unwrap_or_default();
    let apply_download_limits = settings.apply_download_limits_to_local_imports;
    let result = (|| -> Result<()> {
        for entry in walkdir::WalkDir::new(folder).follow_links(false) {
            anyhow::ensure!(
                !state.shutdown.is_cancelled()
                    && !cancel.as_ref().is_some_and(|token| token.is_cancelled()),
                "Import interrupted; sync the source to resume"
            );
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let extension = entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if !crate::downloader::image_exts().contains(&extension.as_str())
                && !crate::downloader::video_exts().contains(&extension.as_str())
            {
                continue;
            }
            let target = destination.join(entry.path().strip_prefix(folder)?);
            let source_size = entry.metadata()?.len();
            if apply_download_limits
                && !target.exists()
                && settings
                    .max_download_file_size_bytes
                    .is_some_and(|limit| source_size > limit)
            {
                record_local_size_skip(
                    state,
                    id,
                    &target,
                    settings.max_download_file_size_bytes.unwrap_or_default(),
                    source_size,
                )?;
                continue;
            }
            std::fs::create_dir_all(target.parent().unwrap())?;
            if !target.exists() {
                // Copy to a sibling temporary first: interrupted copies are
                // never indexed as completed media.
                let mut temporary = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
                std::io::copy(
                    &mut std::fs::File::open(entry.path())?,
                    temporary.as_file_mut(),
                )?;
                temporary.persist_noclobber(&target)?;
            }
            crate::downloader::index_file(state, id, &target)?;
            if let Some(pause) = crate::storage::active_sync_pause(
                &settings,
                &state.data_dir,
                &state.library_dir,
                &slug,
                apply_download_limits,
            ) {
                return Err(anyhow::anyhow!(
                    "{}{}:{}",
                    STORAGE_PAUSE_PREFIX,
                    pause.status(),
                    pause.message()
                ));
            }
        }
        Ok(())
    })();
    let conn = state.pool.get()?;
    let now = crate::db::now_iso();
    let storage_pause = result.as_ref().err().and_then(|error| {
        error
            .to_string()
            .strip_prefix(STORAGE_PAUSE_PREFIX)
            .map(str::to_owned)
    });
    let (status, error_message) = if let Some(pause) = storage_pause {
        let mut parts = pause.splitn(2, ':');
        (
            parts.next().unwrap_or("error").to_string(),
            Some(parts.next().unwrap_or("Storage limit reached.").to_string()),
        )
    } else if result.is_ok() {
        ("done".to_string(), None)
    } else {
        (
            "error".to_string(),
            result.as_ref().err().map(ToString::to_string),
        )
    };
    conn.execute(
        "UPDATE sources
         SET status=?1,error_message=?2,synced_at=?3,
             item_count=(SELECT COUNT(*) FROM media WHERE source_id=?4 AND downloaded=1 AND missing=0),
             completed_count=(SELECT COUNT(*) FROM media WHERE source_id=?4 AND downloaded=1 AND missing=0),
             known_total=(SELECT COUNT(*) FROM media WHERE source_id=?4 AND downloaded=1 AND missing=0),
             completed_at=CASE WHEN ?1='done' THEN ?3 ELSE completed_at END,
             progress_updated_at=?3,current_filename=NULL,
             retry_attempts=CASE WHEN ?1 IN ('done','error') THEN 0 ELSE retry_attempts END,
             retry_at=CASE WHEN ?1 IN ('done','error') THEN 0 ELSE retry_at END
         WHERE id=?4",
        rusqlite::params![
            status,
            error_message,
            now,
            id
        ],
    )?;
    result
}
