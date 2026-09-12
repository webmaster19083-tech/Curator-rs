//! Local folders use ordinary sources and the existing media indexer.

use anyhow::Result;
use rusqlite::OptionalExtension;
use std::path::Path;

pub fn import_folder(
    state: &crate::AppState,
    folder: &Path,
    group_id: Option<i64>,
) -> Result<i64> {
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
        .query_row("SELECT id FROM sources WHERE url=?1", [&url], |row| row.get(0))
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
    let slug: String = state
        .pool
        .get()?
        .query_row("SELECT slug FROM sources WHERE id=?1", [id], |row| row.get(0))?;
    let destination = state.library_dir.join(slug);
    std::fs::create_dir_all(&destination)?;
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
        }
        Ok(())
    })();
    let conn = state.pool.get()?;
    let now = crate::db::now_iso();
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
            if result.is_ok() { "done" } else { "error" },
            result.as_ref().err().map(ToString::to_string),
            now,
            id
        ],
    )?;
    result
}
