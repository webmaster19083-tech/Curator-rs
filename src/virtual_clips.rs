//! Database-only clips. No transcoding and no media files are written.
use crate::{db::now_iso, AppState};
use rusqlite::OptionalExtension;

pub fn create(
    state: &AppState,
    id: i64,
    job: i64,
    filepath: String,
    seconds: u32,
) -> anyhow::Result<i64> {
    let root = dunce::canonicalize(&state.library_dir)?;
    let original = dunce::canonicalize(root.join(&filepath))?;
    anyhow::ensure!(
        original.starts_with(&root) && original.is_file(),
        "Original video is unavailable"
    );
    let conn = state.pool.get()?;
    let cached: Option<f64> =
        conn.query_row("SELECT duration_secs FROM media WHERE id=?1", [id], |r| {
            r.get(0)
        })?;
    let duration = cached
        .filter(|d| d.is_finite() && *d > 0.0)
        .or_else(|| crate::downloader::probe_video_duration(&state.ffprobe_bin, &original))
        .ok_or_else(|| {
            anyhow::anyhow!("Duration is unknown; play the video once or install ffprobe")
        })?;
    anyhow::ensure!(
        duration.is_finite() && duration > 90.0,
        "Only videos longer than 90 seconds need clips"
    );
    let count = (duration / f64::from(seconds)).ceil() as i64;
    anyhow::ensure!(
        count <= 10000,
        "Too many clips; choose a longer clip duration"
    );
    let tx = conn.unchecked_transaction()?;
    for index in 0..count {
        anyhow::ensure!(
            !state.shutdown.is_cancelled(),
            "Clip creation interrupted; retry"
        );
        let start = index as f64 * f64::from(seconds);
        let end = (start + f64::from(seconds)).min(duration);
        let key = format!("virtual-clips/{id}/{job}/{index}");
        let existing: Option<i64> = tx
            .query_row("SELECT id FROM media WHERE filepath=?1", [&key], |r| {
                r.get(0)
            })
            .optional()?;
        if existing.is_some() {
            continue;
        }
        tx.execute("INSERT INTO media(source_id,filepath,filename,type,added_at,downloaded,duration_secs,duration_attempted,clip_parent_id,clip_start_secs,clip_end_secs,file_size_bytes,rating,auto_rating,auto_rating_score,rating_source,rating_reviewed,rating_reviewed_at,nsfw_state)
          SELECT source_id,?1,filename || ' [' || ?2 || '-' || ?3 || 's]','video',?4,1,?5,1,id,?2,?3,0,rating,auto_rating,auto_rating_score,rating_source,rating_reviewed,rating_reviewed_at,'done' FROM media WHERE id=?6",
          rusqlite::params![key,start,end,now_iso(),end-start,id])?;
        let clip = tx.last_insert_rowid();
        tx.execute("INSERT INTO media_tags(media_id,tag_id) SELECT ?1,tag_id FROM media_tags WHERE media_id=?2",rusqlite::params![clip,id])?;
    }
    tx.commit()?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn creates_ranges_without_media_files_and_preserves_annotations() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(dir.path());
        crate::test_support::source(&state);
        let original = state.library_dir.join("test/original.mp4");
        std::fs::write(&original, b"original preserved").unwrap();
        let conn = state.pool.get().unwrap();
        conn.execute_batch("INSERT INTO media(id,source_id,filepath,filename,type,added_at,duration_secs,rating) VALUES(1,1,'test/original.mp4','original.mp4','video','now',95,4);
          INSERT INTO tags(id,name,added_at) VALUES(1,'keep','now');INSERT INTO media_tags VALUES(1,1);").unwrap();
        assert_eq!(
            create(&state, 1, 1, "test/original.mp4".into(), 30).unwrap(),
            4
        );
        let ranges:Vec<(f64,f64)>=conn.prepare("SELECT clip_start_secs,clip_end_secs FROM media WHERE clip_start_secs IS NOT NULL ORDER BY id").unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(
            ranges,
            vec![(0.0, 30.0), (30.0, 60.0), (60.0, 90.0), (90.0, 95.0)]
        );
        crate::media_files::reconcile(&state.pool, &state.library_dir).unwrap();
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM media WHERE clip_start_secs IS NOT NULL AND missing=0 AND file_size_bytes=0",[],|r|r.get::<_,i64>(0)).unwrap(),4);
        assert_eq!(
            std::fs::read_dir(original.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
        assert_eq!(std::fs::read(original).unwrap(), b"original preserved");
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM media_tags", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            5
        );
        assert_eq!(
            create(&state, 1, 1, "test/original.mp4".into(), 30).unwrap(),
            4
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM media", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            5
        );
    }
}
