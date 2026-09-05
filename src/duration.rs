//! Backfills `duration_secs` for videos that were indexed before duration
//! probing existed (scan_and_index now populates it for anything indexed
//! going forward — see downloader.rs). Runs for the life of the app,
//! working through existing videos a small batch at a time.
//!
//! Entirely best-effort: if ffprobe isn't installed, this checks once at
//! startup, logs a single note, and never runs the loop at all — no
//! per-file failures, no repeated wasted process-spawn attempts.

use std::path::PathBuf;

use tracing::warn;

use crate::db::DbPool;
use crate::downloader::probe_video_duration;

/// True if `ffprobe_bin` actually runs. Checked once at startup so the
/// backfill loop either runs for real or doesn't start at all, rather than
/// silently failing on every single video.
pub fn ffprobe_available(ffprobe_bin: &str) -> bool {
    crate::process::output_timeout(
        std::process::Command::new(ffprobe_bin).arg("-version"),
        std::time::Duration::from_secs(5),
    )
    .map(|o| o.status.success())
    .unwrap_or(false)
}

pub fn spawn_backfill_loop(pool: DbPool, ffprobe_bin: String, library_dir: PathBuf) {
    tokio::spawn(async move {
        loop {
            let batch = tokio::task::spawn_blocking({
                let pool = pool.clone();
                move || fetch_undurationed_batch(&pool, 25)
            })
            .await;

            let rows = match batch {
                Ok(Ok(rows)) => rows,
                Ok(Err(e)) => {
                    warn!("duration backfill: query failed: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    continue;
                }
                Err(e) => {
                    warn!("duration backfill: task panicked: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    continue;
                }
            };

            if rows.is_empty() {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }

            for (id, filepath) in rows {
                let ffprobe_bin = ffprobe_bin.clone();
                let pool = pool.clone();
                // filepath is stored relative to library_dir (see routes/thumb.rs's
                // identical join) — ffprobe needs a real, absolute path.
                let abs_path = library_dir.join(&filepath);
                let _ = tokio::task::spawn_blocking(move || {
                    if !abs_path.is_file() {
                        if let Ok(conn) = pool.get() { let _ = crate::media_files::mark_missing(&conn, id); }
                        return;
                    }
                    let duration = probe_video_duration(&ffprobe_bin, &abs_path);
                    if let Ok(conn) = pool.get() {
                        let _ = conn.execute(
                            "UPDATE media SET duration_secs=?1, duration_attempted=1 WHERE id=?2 AND duration_secs IS NULL",
                            rusqlite::params![duration, id],
                        );
                    }
                    // Both success and failure persist until file metadata changes.
                }).await;
            }
        }
    });
}

fn fetch_undurationed_batch(pool: &DbPool, limit: i64) -> anyhow::Result<Vec<(i64, String)>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare(
        "SELECT id, filepath FROM media
         WHERE type='video' AND duration_secs IS NULL AND downloaded=1 AND duration_attempted=0
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![limit], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}
