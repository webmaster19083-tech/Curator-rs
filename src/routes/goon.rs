//! Curator's data-driven interactive playback session.
//!
//! This deliberately owns only the plan, selection policy, and session log.
//! The browser continues to use Curator's existing slideshow/feed media
//! elements, so there is one playback implementation for every Play mode.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::db::now_iso;
use crate::routes::media::{db_err, EFFECTIVE_RATING_SQL};
use crate::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct SessionStage {
    pub id: &'static str,
    pub title: &'static str,
    pub duration_s: u32,
    pub intensity: u8,
    pub prompt: &'static str,
    pub event: &'static str,
    pub transition: &'static str,
}

#[derive(Debug, Deserialize, Default)]
pub struct StartSessionBody {
    #[serde(default)]
    pub media_ids: Vec<i64>,
    /// A desired 1–5 media intensity. The effective human-first rating is
    /// used when choosing a matching starting order.
    pub intensity: Option<u8>,
}

#[derive(Debug, Deserialize, Default)]
pub struct CompleteSessionBody {
    pub duration_s: Option<i64>,
    pub stages_completed: Option<i64>,
    pub ended_state: Option<String>,
    pub events: Option<Value>,
}

fn plan(interval: f64) -> Vec<SessionStage> {
    // Keep stage lengths legible and bounded even when an older settings file
    // has an unusual interval. The data is returned to the frontend, rather
    // than hard-coded there, so presets can be added without a playback fork.
    let beat = interval.round().clamp(5.0, 120.0) as u32;
    vec![
        SessionStage {
            id: "arrival",
            title: "Arrival",
            duration_s: beat.saturating_mul(6),
            intensity: 1,
            prompt: "Settle in, check the controls, and choose a comfortable pace.",
            event: "begin",
            transition: "build",
        },
        SessionStage {
            id: "build",
            title: "Build",
            duration_s: beat.saturating_mul(10),
            intensity: 2,
            prompt: "Stay present and let the media rotate at a steady pace.",
            event: "increase",
            transition: "focus",
        },
        SessionStage {
            id: "focus",
            title: "Focus",
            duration_s: beat.saturating_mul(12),
            intensity: 4,
            prompt: "Keep the pace consistent; skip anything that is not a fit.",
            event: "hold",
            transition: "cooldown",
        },
        SessionStage {
            id: "cooldown",
            title: "Cooldown",
            duration_s: beat.saturating_mul(6),
            intensity: 1,
            prompt: "Slow down, take a breath, and end when ready.",
            event: "cooldown",
            transition: "end",
        },
    ]
}

/// POST /api/goon/session
///
/// Long videos are intentionally excluded from rapid, timed sessions. The
/// source file remains in the normal Video library and can be clipped first.
pub async fn start(
    State(state): State<Arc<AppState>>,
    Json(body): Json<StartSessionBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if body.media_ids.len() > 5_000 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"At most 5,000 selected media items can start one session"})),
        ));
    }
    if body.media_ids.iter().any(|id| *id <= 0) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Media IDs must be positive"})),
        ));
    }
    let desired = i64::from(body.intensity.unwrap_or(3).clamp(1, 5));
    let settings = state.settings.read().await;
    let max_clip_length = i64::from(settings.max_clip_length_secs);
    let stages = plan(settings.goon_default_interval);
    let default_limit = i64::from(settings.goon_default_limit.clamp(1, 5_000));
    drop(settings);

    let mut params: Vec<rusqlite::types::Value> = vec![desired.into(), max_clip_length.into()];
    let selected_sql = if body.media_ids.is_empty() {
        String::new()
    } else {
        let placeholders = body
            .media_ids
            .iter()
            .map(|id| {
                params.push((*id).into());
                format!("?{}", params.len())
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(" AND m.id IN ({placeholders})")
    };
    params.push(default_limit.into());
    let limit_parameter = params.len();
    let query = format!(
        "SELECT m.id,m.filepath,m.filename,m.type,{EFFECTIVE_RATING_SQL},m.human_rating,
                m.auto_rating,m.duration_secs,m.origin_url,
                (SELECT creator FROM source_metadata sm WHERE sm.media_id=m.id
                 ORDER BY sm.id DESC LIMIT 1) AS creator
         FROM media m JOIN sources s ON s.id=m.source_id
         WHERE m.downloaded=1 AND m.missing=0 AND s.included=1
           AND (m.clip_start_secs IS NULL OR EXISTS(SELECT 1 FROM media parent
                WHERE parent.id=m.clip_parent_id AND parent.downloaded=1 AND parent.missing=0))
           AND (m.type<>'video' OR m.duration_secs IS NULL OR m.duration_secs<=?2)
           {selected_sql}
         ORDER BY ABS({EFFECTIVE_RATING_SQL}-?1), RANDOM() LIMIT ?{limit_parameter}"
    );
    // Keep the synchronous SQLite handles inside their own scope. The next
    // step awaits the shared playback-history mutex, and Axum handlers must
    // not carry a non-Send database handle across that await point.
    let mut media: Vec<Value> = {
        let conn = state.pool.get().map_err(db_err)?;
        let mut statement = conn.prepare(&query).map_err(db_err)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok(json!({
                    "id":row.get::<_,i64>(0)?,
                    "filepath":row.get::<_,String>(1)?,
                    "filename":row.get::<_,String>(2)?,
                    "type":row.get::<_,String>(3)?,
                    "rating":row.get::<_,i64>(4)?,
                    "human_rating":row.get::<_,Option<i64>>(5)?,
                    "auto_rating":row.get::<_,i64>(6)?,
                    "duration_secs":row.get::<_,Option<f64>>(7)?,
                    "origin_url":row.get::<_,Option<String>>(8)?,
                    "creator":row.get::<_,Option<String>>(9)?,
                }))
            })
            .map_err(db_err)?
            .filter_map(Result::ok)
            .collect();
        rows
    };

    if media.is_empty() {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error":"No ready short-form media matches this session"})),
        ));
    }

    // The database has already applied rating preference. Shuffle equally
    // suitable items, then protect the session boundary from a visually
    // obvious immediate repeat. A single eligible item is the sole exception.
    media.shuffle(&mut rand::thread_rng());
    let last_id = state.playback_history.lock().await.back().copied();
    if media.len() > 1 && media.first().and_then(|item| item["id"].as_i64()) == last_id {
        media.swap(0, 1);
    }
    if let Some(id) = media.first().and_then(|item| item["id"].as_i64()) {
        state.remember_playback(id).await;
    }

    Ok(Json(json!({
        "mode":"goon",
        "stages":stages,
        "media":media,
        "selection":{
            "desired_intensity":desired,
            "uses_effective_rating":true,
            "long_videos_excluded":true,
        },
        "end_states":["cooldown","completed","cancelled"],
    })))
}

/// POST /api/goon/session/complete
pub async fn complete(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CompleteSessionBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ended_state = body.ended_state.unwrap_or_else(|| "completed".to_string());
    if !matches!(ended_state.as_str(), "cooldown" | "completed" | "cancelled") {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Unknown session end state"})),
        ));
    }
    let duration_s = body.duration_s.unwrap_or(0).clamp(0, 86_400);
    let item_count = body.stages_completed.unwrap_or(0).clamp(0, 128);
    let should_log = state.settings.read().await.goon_log_sessions;
    if !should_log {
        return Ok(Json(json!({"logged":false,"ended_state":ended_state})));
    }
    let conn = state.pool.get().map_err(db_err)?;
    conn.execute(
        "INSERT INTO interactive_sessions(started_at,duration_s,item_count,plan,events,ended_state)
         VALUES(?1,?2,?3,?4,?5,?6)",
        rusqlite::params![
            now_iso(),
            duration_s,
            item_count,
            "goon-default-v1",
            body.events.map(|events| events.to_string()),
            ended_state,
        ],
    )
    .map_err(db_err)?;
    Ok(Json(
        json!({"logged":true,"id":conn.last_insert_rowid(),"ended_state":ended_state}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;

    #[tokio::test]
    async fn uses_effective_human_rating_and_excludes_long_videos() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        crate::test_support::source(&state);
        state.pool.get().unwrap().execute_batch(
            "INSERT INTO media(id,source_id,filepath,filename,type,added_at,downloaded,human_rating,auto_rating,rating,duration_secs)
             VALUES (1,1,'one','one','image','2026',1,5,1,5,NULL),
                    (2,1,'two','two','video','2026',1,NULL,5,5,300.0);",
        ).unwrap();
        let result = start(
            State(state),
            Json(StartSessionBody {
                media_ids: vec![],
                intensity: Some(5),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(result["media"].as_array().unwrap().len(), 1);
        assert_eq!(result["media"][0]["id"], 1);
        assert_eq!(result["media"][0]["rating"], 5);
        assert_eq!(result["stages"][3]["transition"], "end");
    }
}
