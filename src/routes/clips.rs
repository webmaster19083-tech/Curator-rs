//! Creates independent MP4 clips; the source is only ever opened for reading.
use super::media::db_err;
use crate::{db::now_iso, AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;
static CLIP_SLOT: once_cell::sync::Lazy<Arc<tokio::sync::Semaphore>> =
    once_cell::sync::Lazy::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

#[derive(Deserialize)]
pub struct ClipBody {
    pub seconds: u32,
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<ClipBody>,
) -> ApiResult {
    if !(15..=60).contains(&body.seconds) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Clip length must be 15 to 60 seconds"})),
        ));
    }
    let permit = CLIP_SLOT.clone().try_acquire_owned().map_err(|_| {
        (
            StatusCode::CONFLICT,
            Json(json!({"error":"Another video is being split. Please wait for it to finish."})),
        )
    })?;
    let (filepath, job_id) = {
        let conn = state.pool.get().map_err(db_err)?;
        let filepath: Option<String> = conn.query_row("SELECT filepath FROM media WHERE id=?1 AND type='video' AND downloaded=1 AND missing=0 AND clip_parent_id IS NULL", [id], |r| r.get(0)).optional().map_err(db_err)?;
        let filepath = filepath.ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"Original downloaded video not found"})),
            )
        })?;
        let existing: Option<i64> = conn.query_row("SELECT id FROM clip_jobs WHERE media_id=?1 AND seconds=?2 AND status='done' ORDER BY id DESC LIMIT 1", rusqlite::params![id,body.seconds], |r| r.get(0)).optional().map_err(db_err)?;
        if let Some(job_id) = existing {
            return Ok(Json(json!({"job_id":job_id,"status":"done"})));
        }
        conn.execute(
            "INSERT INTO clip_jobs(media_id,seconds,status,added_at) VALUES(?1,?2,'running',?3)",
            rusqlite::params![id, body.seconds, now_iso()],
        )
        .map_err(db_err)?;
        (filepath, conn.last_insert_rowid())
    };
    let worker = state.clone();
    state.download_tasks.spawn(async move {
        let _permit = permit;
        let result = split_video(worker.clone(), id, job_id, filepath, body.seconds).await;
        if let Ok(conn) = worker.pool.get() {
            match result {
                Ok(count) => {
                    let _ = conn.execute(
                        "UPDATE clip_jobs SET status='done',clip_count=?1 WHERE id=?2",
                        rusqlite::params![count, job_id],
                    );
                }
                Err(error) => {
                    let _ = conn.execute(
                        "UPDATE clip_jobs SET status='failed',error=?1 WHERE id=?2",
                        rusqlite::params![error.to_string(), job_id],
                    );
                }
            }
        }
    });
    Ok(Json(json!({"job_id":job_id,"status":"running"})))
}

pub async fn status(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> ApiResult {
    let conn = state.pool.get().map_err(db_err)?;
    conn.query_row("SELECT status,clip_count,error FROM clip_jobs WHERE id=?1", [id], |r| Ok(Json(json!({
        "job_id":id,"status":r.get::<_,String>(0)?,"clip_count":r.get::<_,i64>(1)?,"error":r.get::<_,Option<String>>(2)?
    })))).map_err(|e| if matches!(e,rusqlite::Error::QueryReturnedNoRows) {(StatusCode::NOT_FOUND,Json(json!({"error":"Clip job not found"})))} else {db_err(e)})
}

fn ffmpeg_path(probe: &str) -> PathBuf {
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let sibling = std::path::Path::new(probe).with_file_name(name);
    if sibling.is_file() {
        sibling
    } else {
        PathBuf::from(name)
    }
}

async fn split_video(
    state: Arc<AppState>,
    id: i64,
    job: i64,
    filepath: String,
    seconds: u32,
) -> anyhow::Result<i64> {
    let library = state.library_dir.canonicalize()?;
    let original = library.join(&filepath).canonicalize()?;
    anyhow::ensure!(
        original.starts_with(&library) && original.is_file(),
        "Video is outside the library or unavailable"
    );
    let probe = state.ffprobe_bin.clone();
    let input = original.clone();
    let duration = tokio::task::spawn_blocking(move || {
        crate::downloader::probe_video_duration(&probe, &input)
    })
    .await?
    .ok_or_else(|| anyhow::anyhow!("Could not read video duration with ffprobe"))?;
    anyhow::ensure!(
        duration > 90.0,
        "Only videos longer than 90 seconds need splitting"
    );
    let original_stamp = crate::media_files::stamp(&original);
    let staging = tempfile::Builder::new()
        .prefix("clip-work-")
        .tempdir_in(&state.data_dir)?;
    let output_pattern = staging.path().join("clip-%04d.mp4");
    let log = tempfile::tempfile()?;
    let mut child = tokio::process::Command::new(ffmpeg_path(&state.ffprobe_bin))
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-n", "-i"])
        .arg(&original)
        .args([
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-sn",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "22",
            "-threads",
            "2",
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-force_key_frames",
        ])
        .arg(format!("expr:gte(t,n_forced*{seconds})"))
        .args(["-f", "segment", "-segment_time"])
        .arg(seconds.to_string())
        .args([
            "-reset_timestamps",
            "1",
            "-segment_format_options",
            "movflags=+faststart",
        ])
        .arg(&output_pattern)
        .stdout(std::process::Stdio::null())
        .stderr(log.try_clone()?)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("Could not start FFmpeg: {e}"))?;
    let result = tokio::select! {
        result = child.wait() => result?,
        _ = state.shutdown.cancelled() => { child.kill().await?; anyhow::bail!("Clip creation interrupted by shutdown; original preserved"); },
        _ = tokio::time::sleep(std::time::Duration::from_secs(21600)) => { child.kill().await?; anyhow::bail!("Clip creation exceeded six hours; original preserved"); }
    };
    if !result.success() {
        use std::io::{Read, Seek, SeekFrom};
        let mut log = log;
        log.seek(SeekFrom::Start(0))?;
        let mut error = String::new();
        log.take(2000).read_to_string(&mut error)?;
        anyhow::bail!("FFmpeg failed: {}", error.trim());
    }
    anyhow::ensure!(
        crate::media_files::stamp(&original) == original_stamp,
        "Original changed during clip creation; retry"
    );
    tokio::task::spawn_blocking(move || -> anyhow::Result<i64> {
        let mut clips: Vec<_> = std::fs::read_dir(staging.path())?.collect::<Result<Vec<_>,_>>()?;
        clips.sort_by_key(|entry| entry.file_name());
        anyhow::ensure!(!clips.is_empty(), "FFmpeg produced no clips");
        let mut durations = Vec::new();
        for clip in &clips {
            let duration = crate::downloader::probe_video_duration(&state.ffprobe_bin, &clip.path())
                .ok_or_else(|| anyhow::anyhow!("Could not verify generated clip"))?;
            anyhow::ensure!(duration > 0.0 && duration <= 90.0, "Generated clip exceeds the clip category limit");
            durations.push(duration);
        }
        let destination = original.parent().unwrap().join(format!("curator-clips-{id}-{job}"));
        anyhow::ensure!(!destination.exists(), "Clip output directory already exists; nothing overwritten");
        std::fs::rename(staging.path(), &destination)?;
        let db_result = (|| -> anyhow::Result<i64> {
            let conn = state.pool.get()?;
            let tx = conn.unchecked_transaction()?;
            for (clip,duration) in clips.iter().zip(durations) {
                let path = destination.join(clip.file_name());
                let relative = path.strip_prefix(&library)?.to_string_lossy().replace('\\',"/");
                tx.execute("INSERT INTO media(source_id,filepath,filename,type,added_at,downloaded,duration_secs,duration_attempted,file_stamp,clip_parent_id,auto_rating,auto_rating_score,rating,rating_source)
                    SELECT source_id,?1,?2,'video',?3,1,?4,1,?5,id,auto_rating,auto_rating_score,auto_rating,
                    CASE WHEN auto_rating>0 THEN 'auto' ELSE 'none' END FROM media WHERE id=?6
                    ON CONFLICT(filepath) DO UPDATE SET clip_parent_id=excluded.clip_parent_id,
                    duration_secs=excluded.duration_secs,duration_attempted=1,
                    auto_rating=excluded.auto_rating,auto_rating_score=excluded.auto_rating_score,
                    rating=CASE WHEN media.rating_reviewed=0 THEN excluded.rating ELSE media.rating END,
                    rating_source=CASE WHEN media.rating_reviewed=0 THEN excluded.rating_source ELSE media.rating_source END", rusqlite::params![relative,clip.file_name().to_string_lossy(),now_iso(),duration,crate::media_files::stamp(&path),id])?;
                tx.execute("INSERT OR IGNORE INTO media_tags(media_id,tag_id) SELECT m.id,mt.tag_id FROM media m JOIN media_tags mt ON mt.media_id=?1 WHERE m.filepath=?2", rusqlite::params![id,relative])?;
            }
            tx.commit()?;
            Ok(clips.len() as i64)
        })();
        if db_result.is_err() {
            let _ = std::fs::remove_dir_all(&destination);
        }
        db_result
    }).await?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn real_split_preserves_original_and_indexes_short_mp4_copies() {
        let Ok(probe) = std::env::var("CURATOR_TEST_FFPROBE") else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let mut state = crate::test_support::state(root.path());
        Arc::get_mut(&mut state).unwrap().ffprobe_bin = probe.clone();
        crate::test_support::source(&state);
        let original = state.library_dir.join("test/original.mp4");
        let generated = std::process::Command::new(ffmpeg_path(&probe))
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=32x32:r=2:d=95",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&original)
            .status()
            .unwrap();
        assert!(generated.success());
        let before = std::fs::read(&original).unwrap();
        state.pool.get().unwrap().execute_batch("INSERT INTO media(id,source_id,filepath,filename,type,added_at,duration_secs,auto_rating,rating,rating_source) VALUES(1,1,'test/original.mp4','original.mp4','video','2026',95,4,4,'auto');").unwrap();
        let job = create(
            State(state.clone()),
            Path(1),
            Json(ClipBody { seconds: 30 }),
        )
        .await
        .unwrap()
        .0;
        let id = job["job_id"].as_i64().unwrap();
        let done = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let job = status(State(state.clone()), Path(id)).await.unwrap().0;
                if job["status"] != "running" {
                    break job;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(done["status"], "done", "{done}");
        assert_eq!(done["clip_count"], 4);
        assert_eq!(std::fs::read(&original).unwrap(), before);
        assert_eq!(state.pool.get().unwrap().query_row("SELECT COUNT(*) FROM media WHERE clip_parent_id=1 AND duration_secs<=90 AND duration_secs>0",[],|r|r.get::<_,i64>(0)).unwrap(),4);
        let repeated = create(State(state), Path(1), Json(ClipBody { seconds: 30 }))
            .await
            .unwrap()
            .0;
        assert_eq!(repeated["job_id"], id);
    }
}
