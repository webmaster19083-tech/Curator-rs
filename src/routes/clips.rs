//! Creates virtual time ranges backed by the original video.
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
use std::sync::Arc;

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
            Json(json!({"error":"Another video is having virtual clips created. Please wait for it to finish."})),
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
        let existing: Option<i64> = conn.query_row("SELECT id FROM clip_jobs WHERE media_id=?1 AND seconds=?2 AND status='done' AND virtual=1 ORDER BY id DESC LIMIT 1", rusqlite::params![id,body.seconds], |r| r.get(0)).optional().map_err(db_err)?;
        if let Some(job_id) = existing {
            return Ok(Json(json!({"job_id":job_id,"status":"done"})));
        }
        conn.execute(
            "INSERT INTO clip_jobs(media_id,seconds,status,added_at,virtual) VALUES(?1,?2,'running',?3,1)",
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

async fn split_video(
    state: Arc<AppState>,
    id: i64,
    job: i64,
    filepath: String,
    seconds: u32,
) -> anyhow::Result<i64> {
    tokio::task::spawn_blocking(move || {
        crate::virtual_clips::create(&state, id, job, filepath, seconds)
    })
    .await?
}
