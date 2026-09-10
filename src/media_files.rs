//! Persistent filesystem reconciliation. Missing rows retain their identity and annotations.
use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::Path;

pub fn stamp(path: &Path) -> Option<String> {
    let m = path.metadata().ok()?;
    if !m.is_file() {
        return None;
    }
    Some(format!("{}:{:?}", m.len(), m.modified().ok()))
}

pub fn mark_missing(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("UPDATE media SET missing=1, downloaded=0, nsfw_state='missing' WHERE id=?1 AND downloaded=1", [id])?;
    Ok(())
}

pub fn reconcile(pool: &crate::db::DbPool, library: &Path) -> Result<usize> {
    let mut after = 0;
    let mut missing = 0;
    loop {
        let rows = {
            let conn = pool.get()?;
            let mut stmt = conn.prepare(
                "SELECT id, filepath, file_stamp, missing FROM media
                WHERE id>?1 AND (downloaded=1 OR missing=1) ORDER BY id LIMIT 256",
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
                if old_stamp.as_ref() != Some(&current) || was_missing {
                    tx.execute("UPDATE media SET downloaded=1, missing=0, file_stamp=?1,
                        nsfw_state='pending', nsfw_attempts=0, nsfw_retry_at=0, duration_attempted=0,
<<<<<<< Updated upstream
                        duration_secs=CASE WHEN file_stamp IS NULL THEN duration_secs ELSE NULL END WHERE id=?2", params![current, id])?;
=======
                        action_rating=0,action_model=NULL,action_model_version=NULL,action_score=NULL,action_evidence=NULL,
                        classifier_model=NULL,classifier_version=NULL,classifier_score=NULL,classifier_evidence=NULL,
                        classification_label='unclassified',manual_review_required=0,manual_review_reason=NULL,
                        duration_secs=CASE WHEN file_stamp IS NULL THEN duration_secs ELSE NULL END WHERE id=?4",
                        params![current,modified_at(&path),crate::db::now_iso(),id])?;
>>>>>>> Stashed changes
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
    conn.execute("UPDATE sources SET item_count=(SELECT COUNT(*) FROM media WHERE source_id=sources.id AND downloaded=1)", [])?;
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use super::*;
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
