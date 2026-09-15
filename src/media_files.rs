//! Persistent filesystem reconciliation. Missing rows retain their identity and annotations.
use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

pub fn file_size(path: &Path) -> Option<i64> {
    path.metadata()
        .ok()
        .filter(|metadata| metadata.is_file())
        .and_then(|metadata| i64::try_from(metadata.len()).ok())
}

/// Durable rows are processed in short background batches after startup.
/// Completed means inspected, not necessarily populated: unreadable and
/// absent files intentionally retain NULL so a later reconciliation can retry.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SizeBackfillProgress {
    pub running: bool,
    pub completed: u64,
    pub total: u64,
    pub updated: u64,
    pub unavailable: u64,
    pub error: Option<String>,
}

fn count_missing_sizes(pool: &crate::db::DbPool) -> Result<u64> {
    let conn = pool.get()?;
    let count = conn.query_row(
        "SELECT COUNT(*) FROM media
         WHERE downloaded=1 AND missing=0 AND clip_start_secs IS NULL
           AND file_size_bytes IS NULL",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(count.max(0) as u64)
}

/// Inspect at most limit rows without holding a SQLite transaction across
/// filesystem metadata calls. It is public for deterministic migration tests.
pub fn backfill_size_batch(
    pool: &crate::db::DbPool,
    library: &Path,
    after_id: i64,
    limit: usize,
) -> Result<(i64, u64, u64, u64, bool)> {
    let rows = {
        let conn = pool.get()?;
        let mut statement = conn.prepare(
            "SELECT id,filepath FROM media
             WHERE id>?1 AND downloaded=1 AND missing=0
               AND clip_start_secs IS NULL AND file_size_bytes IS NULL
             ORDER BY id LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![after_id, limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    if rows.is_empty() {
        return Ok((after_id, 0, 0, 0, true));
    }

    let inspected = rows.len() as u64;
    let next_after_id = rows.last().map(|(id, _)| *id).unwrap_or(after_id);
    let measured: Vec<(i64, Option<i64>)> = rows
        .into_iter()
        .map(|(id, filepath)| (id, file_size(&library.join(filepath))))
        .collect();
    let unavailable = measured.iter().filter(|(_, size)| size.is_none()).count() as u64;
    let updated = measured.iter().filter(|(_, size)| size.is_some()).count() as u64;

    let conn = pool.get()?;
    let tx = conn.unchecked_transaction()?;
    for (id, size) in measured {
        if let Some(size) = size {
            tx.execute(
                "UPDATE media SET file_size_bytes=?1
                 WHERE id=?2 AND file_size_bytes IS NULL",
                params![size, id],
            )?;
        }
    }
    tx.commit()?;
    Ok((next_after_id, inspected, updated, unavailable, false))
}

/// Start a resumable, interruptible size backfill. It deliberately does not
/// run during migrations and it never treats a failed metadata lookup as a
/// missing-file verdict.
pub async fn backfill_missing_file_sizes(
    pool: crate::db::DbPool,
    library: PathBuf,
    shutdown: tokio_util::sync::CancellationToken,
    progress: Arc<RwLock<SizeBackfillProgress>>,
    maintenance: Arc<crate::maintenance::MaintenanceController>,
) {
    {
        let mut status = progress.write().await;
        *status = SizeBackfillProgress {
            running: true,
            ..SizeBackfillProgress::default()
        };
    }
    // Counting is read-only, but it still uses SQLite.  Treat it as an
    // in-flight worker so an Admin job cannot begin its exclusive window in
    // the middle of this startup phase.
    let count_lease = loop {
        if shutdown.is_cancelled() {
            progress.write().await.running = false;
            return;
        }
        if let Some(lease) = maintenance.try_acquire_background_worker() {
            break lease;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    };
    let total = match tokio::task::spawn_blocking({
        let pool = pool.clone();
        move || count_missing_sizes(&pool)
    })
    .await
    {
        Ok(Ok(total)) => total,
        Ok(Err(error)) => {
            let mut status = progress.write().await;
            status.running = false;
            status.error = Some(error.to_string());
            return;
        }
        Err(error) => {
            let mut status = progress.write().await;
            status.running = false;
            status.error = Some(format!("Size backfill worker stopped: {error}"));
            return;
        }
    };
    drop(count_lease);
    progress.write().await.total = total;

    let mut after_id = 0;
    loop {
        if shutdown.is_cancelled() {
            progress.write().await.running = false;
            return;
        }
        if maintenance.is_active() {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            continue;
        }
        let Some(_worker) = maintenance.try_acquire_background_worker() else {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            continue;
        };
        let batch = tokio::task::spawn_blocking({
            let pool = pool.clone();
            let library = library.clone();
            move || backfill_size_batch(&pool, &library, after_id, 128)
        })
        .await;
        let (next, inspected, updated, unavailable, done) = match batch {
            Ok(Ok(batch)) => batch,
            Ok(Err(error)) => {
                let mut status = progress.write().await;
                status.running = false;
                status.error = Some(error.to_string());
                return;
            }
            Err(error) => {
                let mut status = progress.write().await;
                status.running = false;
                status.error = Some(format!("Size backfill worker stopped: {error}"));
                return;
            }
        };
        {
            let mut status = progress.write().await;
            status.completed += inspected;
            status.updated += updated;
            status.unavailable += unavailable;
            if done {
                status.running = false;
            }
        }
        if done {
            return;
        }
        after_id = next;
    }
}

fn modified_at(path: &Path) -> Option<String> {
    let modified = path.metadata().ok()?.modified().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from(modified).to_rfc3339())
}

pub fn stamp(path: &Path) -> Option<String> {
    let m = path.metadata().ok()?;
    if !m.is_file() {
        return None;
    }
    Some(format!("{}:{:?}", m.len(), m.modified().ok()))
}

pub fn mark_missing(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("UPDATE media SET missing=1, downloaded=0, file_size_bytes=NULL, nsfw_state='missing' WHERE id=?1 AND downloaded=1", [id])?;
    Ok(())
}

#[allow(dead_code)]
pub fn reconcile(pool: &crate::db::DbPool, library: &Path) -> Result<usize> {
    reconcile_cancellable(pool, library, None)
}

/// Reconcile physical files without treating database-only virtual clips as
/// missing.  A cancellation means the next startup resumes from the durable
/// database state; it is never an error or a reason to erase annotations.
pub fn reconcile_cancellable(
    pool: &crate::db::DbPool,
    library: &Path,
    shutdown: Option<&tokio_util::sync::CancellationToken>,
) -> Result<usize> {
    let mut after = 0;
    let mut missing = 0;
    loop {
        if shutdown.is_some_and(|token| token.is_cancelled()) {
            break;
        }
        let rows = {
            let conn = pool.get()?;
            let mut stmt = conn.prepare(
                "SELECT id, filepath, file_stamp, missing FROM media
                WHERE id>?1 AND clip_start_secs IS NULL
                  AND (downloaded=1 OR missing=1) ORDER BY id LIMIT 256",
            )?;
            let rows = stmt
                .query_map([after], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, bool>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        if rows.is_empty() {
            break;
        }
        let conn = pool.get()?;
        let tx = conn.unchecked_transaction()?;
        for (id, filepath, old_stamp, was_missing) in rows {
            after = id;
            let path = library.join(filepath);
            if let Some(current) = stamp(&path) {
                tx.execute(
                    "UPDATE media SET file_size_bytes=?1
                     WHERE id=?2 AND file_size_bytes IS NOT ?1",
                    params![file_size(&path), id],
                )?;
                if old_stamp.as_ref() != Some(&current) || was_missing {
                    tx.execute("UPDATE media SET downloaded=1, missing=0, file_stamp=?1,
                        modified_at=COALESCE(?2,modified_at),downloaded_at=COALESCE(downloaded_at,?3),
                        nsfw_state='pending', nsfw_attempts=0, nsfw_retry_at=0, duration_attempted=0,
                        action_rating=0,action_model=NULL,action_model_version=NULL,action_score=NULL,action_evidence=NULL,
                        classifier_model=NULL,classifier_version=NULL,classifier_score=NULL,classifier_evidence=NULL,
                        classification_label='unclassified',manual_review_required=0,manual_review_reason=NULL,
                        duration_secs=CASE WHEN file_stamp IS NULL THEN duration_secs ELSE NULL END WHERE id=?4",
                        params![current,modified_at(&path),crate::db::now_iso(),id])?;
                }
            } else if !was_missing && !path.is_file() {
                // Permission failures are not evidence that a file was deleted.
                if path.metadata().is_ok_and(|m| !m.is_file())
                    || path
                        .metadata()
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                {
                    mark_missing(&tx, id)?;
                    missing += 1;
                }
            }
        }
        tx.commit()?;
    }
    let conn = pool.get()?;
    // Virtual clips are playable ranges in their parent's file.  Their
    // availability is derived from that parent rather than from a synthetic
    // filepath that reconciliation would otherwise (correctly) not find.
    conn.execute(
        "UPDATE media AS clip
         SET downloaded=CASE WHEN EXISTS(
               SELECT 1 FROM media parent
               WHERE parent.id=clip.clip_parent_id AND parent.downloaded=1 AND parent.missing=0
             ) THEN 1 ELSE 0 END,
             missing=CASE WHEN EXISTS(
               SELECT 1 FROM media parent
               WHERE parent.id=clip.clip_parent_id AND parent.downloaded=1 AND parent.missing=0
             ) THEN 0 ELSE 1 END
         WHERE clip.clip_start_secs IS NOT NULL",
        [],
    )?;
    conn.execute("UPDATE sources SET item_count=(SELECT COUNT(*) FROM media WHERE source_id=sources.id AND downloaded=1)", [])?;
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_backfill_is_batched_and_leaves_unavailable_rows_null() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        crate::test_support::source(&state);
        std::fs::write(state.library_dir.join("test/present.jpg"), b"12345").unwrap();
        let conn = state.pool.get().unwrap();
        conn.execute_batch(
            "INSERT INTO media(id,source_id,filepath,filename,type,added_at,downloaded) VALUES
             (1,1,'test/present.jpg','present.jpg','image','now',1),
             (2,1,'test/absent.jpg','absent.jpg','image','now',1)",
        )
        .unwrap();
        let (after, inspected, updated, unavailable, done) =
            backfill_size_batch(&state.pool, &state.library_dir, 0, 1).unwrap();
        assert_eq!(
            (after, inspected, updated, unavailable, done),
            (1, 1, 1, 0, false)
        );
        let (_, inspected, updated, unavailable, done) =
            backfill_size_batch(&state.pool, &state.library_dir, after, 1).unwrap();
        assert_eq!((inspected, updated, unavailable, done), (1, 0, 1, false));
        assert_eq!(
            conn.query_row("SELECT file_size_bytes FROM media WHERE id=1", [], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .unwrap(),
            Some(5)
        );
        assert_eq!(
            conn.query_row("SELECT file_size_bytes FROM media WHERE id=2", [], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .unwrap(),
            None
        );
    }

    #[test]
    fn missing_is_persistent_and_restoration_preserves_annotations() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        crate::test_support::source(&state);
        let conn = state.pool.get().unwrap();
        conn.execute("INSERT INTO media(id,source_id,filepath,filename,type,added_at,rating) VALUES(1,1,'test/gone.jpg','gone.jpg','image','2026',4)",[]).unwrap();
        conn.execute(
            "INSERT INTO tags(id,name,added_at) VALUES(1,'keep','2026')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO media_tags VALUES(1,1)", [])
            .unwrap();
        assert_eq!(reconcile(&state.pool, &state.library_dir).unwrap(), 1);
        assert_eq!(reconcile(&state.pool, &state.library_dir).unwrap(), 0);
        assert_eq!(
            conn.query_row(
                "SELECT downloaded,missing,rating FROM media WHERE id=1",
                [],
                |r| Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?
                ))
            )
            .unwrap(),
            (0, 1, 4)
        );
        std::fs::write(state.library_dir.join("test/gone.jpg"), b"restored").unwrap();
        reconcile(&state.pool, &state.library_dir).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT downloaded,missing,rating FROM media WHERE id=1",
                [],
                |r| Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?
                ))
            )
            .unwrap(),
            (1, 0, 4)
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM media_tags", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        conn.execute(
            "UPDATE media SET nsfw_state='failed',duration_attempted=1",
            [],
        )
        .unwrap();
        reconcile(&state.pool, &state.library_dir).unwrap();
        assert_eq!(
            conn.query_row("SELECT nsfw_state FROM media", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "failed"
        );
        std::fs::write(
            state.library_dir.join("test/gone.jpg"),
            b"changed file version",
        )
        .unwrap();
        reconcile(&state.pool, &state.library_dir).unwrap();
        assert_eq!(
            conn.query_row("SELECT nsfw_state FROM media", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "pending"
        );
    }
}
