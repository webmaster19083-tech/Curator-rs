use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::now_iso;
use crate::AppState;

// ─── Sort orders ─────────────────────────────────────────────────────────────

fn sort_order(sort: &str) -> &'static str {
    match sort {
        "rating_desc" => "m.rating DESC, m.id ASC",
        "rating_asc" => "m.rating ASC, m.id ASC",
        "date_desc" => "m.added_at DESC, m.id DESC",
        "date_asc" => "m.added_at ASC, m.id ASC",
        "filename_asc" => "m.filename COLLATE NOCASE ASC, m.id ASC",
        "filename_desc" => "m.filename COLLATE NOCASE DESC, m.id ASC",
        _ => "m.id ASC",
    }
}

// ─── GET /api/media ───────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct MediaQuery {
    limit: Option<usize>,
    after_id: Option<i64>,
    cursor: Option<String>,
    shuffle_seed: Option<i64>,
    media_type: Option<String>,
    tags: Option<String>,
    any_tags: Option<String>,
    exclude_tags: Option<String>,
    source_id: Option<i64>,
    group_id: Option<i64>,
    only_included: Option<bool>,
    tag: Option<String>,
    sort: Option<String>,
    /// Hide anything rated above this (0 = unrated is always shown
    /// regardless, since it hasn't been rated — manually or by the NSFW
    /// auto-rater — yet).
    max_rating: Option<i64>,
    rating_status: Option<String>,
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MediaQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let seed = q.shuffle_seed.unwrap_or(1).clamp(1, 2147483646);
    let sort = q.sort.as_deref().unwrap_or("default");
    let (key, descending, id_desc) = match sort {
        "rating_desc" => ("m.rating".to_string(), true, false),
        "rating_asc" => ("m.rating".to_string(), false, false),
        "date_desc" => ("m.added_at".to_string(), true, true),
        "date_asc" => ("m.added_at".to_string(), false, false),
        "filename_asc" => ("m.filename COLLATE NOCASE".to_string(), false, false),
        "filename_desc" => ("m.filename COLLATE NOCASE".to_string(), true, false),
        "shuffle" => (format!("((m.id * {seed}) % 2147483647)"), false, false),
        _ => ("m.id".to_string(), false, false),
    };
    let order = if sort == "shuffle" {
        format!("{key}, m.id")
    } else {
        sort_order(sort).to_string()
    };

    // Get group effective tags — read from cache or rebuild
    let group_effective_tags = effective_tags(&state).await?;

    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    let scope_sql = if let Some(sid) = q.source_id {
        params.push(sid.into());
        "m.source_id=?1"
    } else if let Some(gid) = q.group_id {
        if gid == 0 {
            "s.group_id IS NULL"
        } else {
            params.push(gid.into());
            "s.group_id IN (WITH RECURSIVE subtree(id) AS (SELECT ?1 UNION SELECT g.id FROM groups g JOIN subtree st ON g.parent_id=st.id) SELECT id FROM subtree)"
        }
    } else if q.only_included.unwrap_or(false) {
        "s.included=1"
    } else {
        "1=1"
    };
    let rating_clause = if let Some(max) = q.max_rating {
        params.push(max.into());
        format!(" AND (m.rating=0 OR m.rating<=?{})", params.len())
    } else {
        String::new()
    };

    let mut extra = String::from(" AND m.missing=0");
    extra.push_str(match q.rating_status.as_deref().unwrap_or("") {
        "" | "all" => "",
        "unrated" => " AND m.rating=0 AND m.auto_rating=0",
        "auto" => " AND m.rating_source='auto'",
        "needs_review" => " AND m.auto_rating>0 AND m.rating_reviewed=0",
        "reviewed" => " AND m.rating_reviewed=1",
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"Invalid rating_status"})),
            ))
        }
    });
    if let Some(kind) = q.media_type.as_deref() {
        extra.push_str(match kind {
            "image" => " AND m.type='image'",
            "clip" => " AND m.type='video' AND m.duration_secs IS NOT NULL AND m.duration_secs<=90",
            "video" => " AND m.type='video' AND (m.duration_secs IS NULL OR m.duration_secs>90)",
            _ => "",
        });
    }
    if let Some(tag) = q.tag.as_deref() {
        extra.push_str(" AND ");
        extra.push_str(&tag_predicate(
            tag.trim().to_lowercase().as_str(),
            &group_effective_tags,
            &mut params,
        ));
    }
    for (raw, mode) in [
        (&q.tags, "AND"),
        (&q.any_tags, "OR"),
        (&q.exclude_tags, "NOT"),
    ] {
        let predicates: Vec<String> = raw
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| tag_predicate(&t.to_lowercase(), &group_effective_tags, &mut params))
            .collect();
        if !predicates.is_empty() {
            extra.push_str(&format!(
                " AND {}({})",
                if mode == "NOT" { "NOT " } else { "" },
                predicates.join(if mode == "AND" { " AND " } else { " OR " })
            ));
        }
    }
    let anchor: Option<(Value, i64)> = if let Some(cursor) = q.cursor.as_ref() {
        Some(
            hex::decode(cursor)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .ok_or_else(|| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error":"Invalid media cursor"})),
                    )
                })?,
        )
    } else if let Some(id) = q.after_id {
        let conn = state.pool.get().map_err(db_err)?;
        let sql = format!("SELECT {key} FROM media m WHERE m.id=?1");
        let value = conn
            .query_row(&sql, [id], |r| Ok(sql_value(r.get_ref(0)?)))
            .map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error":"Cursor media no longer exists; use next_cursor"})),
                )
            })?;
        Some((value, id))
    } else {
        None
    };
    if let Some((value, id)) = anchor {
        let v = match value {
            Value::String(s) => rusqlite::types::Value::Text(s),
            Value::Number(n) => rusqlite::types::Value::Integer(
                n.as_i64().ok_or_else(|| db_err("Invalid cursor value"))?,
            ),
            _ => return Err(db_err("Invalid cursor value")),
        };
        params.push(v);
        let n = params.len();
        params.push(id.into());
        let i = params.len();
        if key == "m.id" {
            extra.push_str(&format!(" AND m.id>?{i} AND m.id>=?{n}"));
        } else {
            extra.push_str(&format!(
                " AND ({key} {} ?{n} OR ({key} = ?{n} AND m.id {} ?{i}))",
                if descending { "<" } else { ">" },
                if id_desc { "<" } else { ">" }
            ));
        }
    }
    params.push(((limit + 1) as i64).into());
    let limit_param = params.len();
    let query = format!(
        "SELECT m.id,m.source_id,m.filepath,m.filename,m.type,m.added_at,m.rating,m.auto_rating,m.auto_rating_score,m.rating_source,m.rating_reviewed,m.rating_reviewed_at,m.origin_url,m.downloaded,m.duration_secs,m.clip_parent_id, {key} AS _cursor_key, s.group_id AS _source_group_id, \
            (SELECT GROUP_CONCAT(t.name, ',') FROM media_tags mt \
             JOIN tags t ON t.id = mt.tag_id WHERE mt.media_id = m.id) AS tags_csv \
         FROM media m \
         JOIN sources s ON s.id = m.source_id \
         WHERE ({}){}{extra} \
         ORDER BY {} LIMIT ?{limit_param}",
        scope_sql, rating_clause, order
    );

    let mut rows: Vec<HashMap<String, Value>> = {
        let conn = state.pool.get().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;
        let mut stmt = conn.prepare(&query).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?;

        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(|b| b as &dyn rusqlite::ToSql).collect();

        let out = stmt
            .query_map(params_refs.as_slice(), |row| {
                let col_count = row.as_ref().column_count();
                let col_names: Vec<String> = (0..col_count)
                    .map(|i| row.as_ref().column_name(i).unwrap_or("?").to_string())
                    .collect();

                let mut map = HashMap::new();
                for (i, name) in col_names.iter().enumerate() {
                    let val: Value = match row.get_ref(i) {
                        Ok(rusqlite::types::ValueRef::Null) => Value::Null,
                        Ok(rusqlite::types::ValueRef::Integer(n)) => json!(n),
                        Ok(rusqlite::types::ValueRef::Real(f)) => json!(f),
                        Ok(rusqlite::types::ValueRef::Text(s)) => {
                            json!(std::str::from_utf8(s).unwrap_or(""))
                        }
                        Ok(rusqlite::types::ValueRef::Blob(b)) => {
                            json!(std::str::from_utf8(b).unwrap_or(""))
                        }
                        Err(_) => Value::Null,
                    };
                    map.insert(name.clone(), val);
                }
                Ok(map)
            })
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
            })?
            .filter_map(|r| r.ok())
            .collect();
        out
    };

    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let next_cursor = if has_more {
        rows.last().map(|r| {
            hex::encode(
                serde_json::to_vec(&(r["_cursor_key"].clone(), r["id"].as_i64().unwrap_or(0)))
                    .unwrap_or_default(),
            )
        })
    } else {
        None
    };
    let next_after_id = if has_more {
        rows.last().and_then(|r| r["id"].as_i64())
    } else {
        None
    };

    let mut media = Vec::new();
    for mut r in rows {
        r.remove("_cursor_key");
        r.insert("rating_reviewed".into(), json!(r["rating_reviewed"] == 1));
        let source_group_id: Option<i64> = r.remove("_source_group_id").and_then(|v| v.as_i64());
        let tags_csv = r
            .remove("tags_csv")
            .and_then(|v| v.as_str().map(|s| s.to_string()));

        let own_tags: HashSet<String> = tags_csv
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(|t| t.to_string()).collect())
            .unwrap_or_default();

        let inherited: HashSet<String> = source_group_id
            .and_then(|gid| group_effective_tags.get(&gid).cloned())
            .unwrap_or_default();

        let effective_tags = own_tags.union(&inherited).cloned().collect::<HashSet<_>>();

        let mut own_sorted: Vec<String> = own_tags.iter().cloned().collect();
        own_sorted.sort();
        let mut inh_sorted: Vec<String> = (effective_tags.difference(&own_tags)).cloned().collect();
        inh_sorted.sort();

        r.insert("tags".into(), json!(own_sorted));
        r.insert("inherited_tags".into(), json!(inh_sorted));
        media.push(r);
    }

    Ok(Json(
        json!({ "media": media, "has_more":has_more, "next_cursor":next_cursor, "next_after_id":next_after_id, "limit":limit }),
    ))
}

// ─── PUT /api/media/:id/rating ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RatingBody {
    pub rating: i64,
}

pub async fn set_rating(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<RatingBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !(0..=5).contains(&body.rating) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Rating must be between 0 and 5"})),
        ));
    }
    save_review(&state, id, Some(body.rating))
}

pub async fn approve_rating(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    save_review(&state, id, None)
}

fn save_review(
    state: &AppState,
    id: i64,
    rating: Option<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let result = conn.query_row(
        "UPDATE media SET rating=COALESCE(?1,auto_rating), rating_source='human',
         rating_reviewed=1, rating_reviewed_at=?2 WHERE id=?3 AND (?1 IS NOT NULL OR auto_rating BETWEEN 1 AND 5)
         RETURNING rating,auto_rating,auto_rating_score,rating_source,rating_reviewed,rating_reviewed_at",
        rusqlite::params![rating, now_iso(), id], |r| Ok(json!({
            "id":id, "rating":r.get::<_,i64>(0)?, "auto_rating":r.get::<_,i64>(1)?,
            "auto_rating_score":r.get::<_,Option<f64>>(2)?, "rating_source":r.get::<_,String>(3)?,
            "rating_reviewed":r.get::<_,bool>(4)?, "rating_reviewed_at":r.get::<_,Option<String>>(5)?
        })));
    match result {
        Ok(value) => Ok(Json(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            let exists = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM media WHERE id=?1)",
                    [id],
                    |r| r.get::<_, bool>(0),
                )
                .map_err(db_err)?;
            Err((
                if exists {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::NOT_FOUND
                },
                Json(
                    json!({"error":if exists {"No automated rating to approve"} else {"Media not found"}}),
                ),
            ))
        }
        Err(e) => Err(db_err(e)),
    }
}

#[derive(Deserialize)]
pub struct DurationBody {
    pub duration_secs: f64,
}

pub async fn set_duration(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<DurationBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !body.duration_secs.is_finite() || body.duration_secs <= 0.0 || body.duration_secs > 604800.0
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Invalid video duration"})),
        ));
    }
    let conn = state.pool.get().map_err(db_err)?;
    conn.execute("UPDATE media SET duration_secs=?1,duration_attempted=1 WHERE id=?2 AND type='video' AND duration_secs IS NULL", rusqlite::params![body.duration_secs,id]).map_err(db_err)?;
    Ok(Json(json!({"id":id})))
}

#[derive(Deserialize)]
pub struct UndoRatingBody {
    pub rating_reviewed_at: String,
}

pub async fn undo_rating(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<UndoRatingBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let result = conn.query_row("UPDATE media SET rating=auto_rating,rating_source='auto',rating_reviewed=0,rating_reviewed_at=NULL
        WHERE id=?1 AND auto_rating>0 AND rating_reviewed=1 AND rating_reviewed_at=?2
        RETURNING rating,auto_rating,auto_rating_score", rusqlite::params![id,body.rating_reviewed_at], |r| Ok(json!({
            "id":id,"rating":r.get::<_,i64>(0)?,"auto_rating":r.get::<_,i64>(1)?,"auto_rating_score":r.get::<_,Option<f64>>(2)?,
            "rating_source":"auto","rating_reviewed":false,"rating_reviewed_at":null
        })));
    match result {
        Ok(value) => Ok(Json(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Err((
            StatusCode::CONFLICT,
            Json(json!({"error":"Rating changed since this review; cannot undo it"})),
        )),
        Err(e) => Err(db_err(e)),
    }
}

#[derive(Deserialize)]
pub struct TagBody {
    pub name: String,
}

pub async fn add_tag(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(body): Json<TagBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let exists: bool = conn
        .query_row("SELECT COUNT(*) FROM media WHERE id=?1", [id], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0)
        > 0;
    if !exists {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Media not found"})),
        ));
    }

    let tag_id = get_or_create_tag(&conn, &body.name).map_err(db_err)?;
    conn.execute(
        "INSERT OR IGNORE INTO media_tags (media_id, tag_id) VALUES (?1,?2)",
        rusqlite::params![id, tag_id],
    )
    .map_err(db_err)?;

    let tags: Vec<String> = {
        let mut stmt = conn.prepare("SELECT t.name FROM media_tags mt JOIN tags t ON t.id=mt.tag_id WHERE mt.media_id=?1 ORDER BY t.name COLLATE NOCASE").map_err(db_err)?;
        let out = stmt
            .query_map([id], |r| r.get(0))
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();
        out
    };

    Ok(Json(json!({ "media_id": id, "tags": tags })))
}

// ─── DELETE /api/media/:id/tags/:tag_id ──────────────────────────────────────

pub async fn remove_tag(
    State(state): State<Arc<AppState>>,
    Path((id, tag_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    conn.execute(
        "DELETE FROM media_tags WHERE media_id=?1 AND tag_id=?2",
        rusqlite::params![id, tag_id],
    )
    .map_err(db_err)?;
    Ok(Json(json!({ "status": "removed" })))
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

pub fn get_or_create_tag(conn: &rusqlite::Connection, name: &str) -> rusqlite::Result<i64> {
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    if let Ok(id) = conn.query_row("SELECT id FROM tags WHERE name=?1", [&name], |r| {
        r.get::<_, i64>(0)
    }) {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO tags (name, added_at) VALUES (?1, ?2)",
        rusqlite::params![name, now_iso()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn db_err(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": e.to_string()})),
    )
}

pub async fn effective_tags(
    state: &AppState,
) -> Result<Arc<HashMap<i64, HashSet<String>>>, (StatusCode, Json<Value>)> {
    if let Some(cached) = state.group_tag_cache.read().await.as_ref() {
        return Ok(cached.clone());
    }
    let mut cache = state.group_tag_cache.write().await;
    if let Some(cached) = cache.as_ref() {
        return Ok(cached.clone());
    }
    let rebuilt = {
        let conn = state.pool.get().map_err(db_err)?;
        Arc::new(crate::db::build_group_effective_tags_map(&conn))
    };
    *cache = Some(rebuilt.clone());
    Ok(rebuilt)
}

pub fn tag_predicate(
    tag: &str,
    groups: &HashMap<i64, HashSet<String>>,
    params: &mut Vec<rusqlite::types::Value>,
) -> String {
    params.push(tag.to_string().into());
    let t = params.len();
    let ids: Vec<i64> = groups
        .iter()
        .filter(|(_, tags)| tags.contains(tag))
        .map(|(id, _)| *id)
        .collect();
    params.push(
        serde_json::to_string(&ids)
            .unwrap_or_else(|_| "[]".into())
            .into(),
    );
    let g = params.len();
    format!("(EXISTS(SELECT 1 FROM media_tags mt JOIN tags t ON t.id=mt.tag_id WHERE mt.media_id=m.id AND t.name=?{t}) OR s.group_id IN (SELECT value FROM json_each(?{g})))")
}

fn sql_value(value: rusqlite::types::ValueRef<'_>) -> Value {
    match value {
        rusqlite::types::ValueRef::Integer(i) => json!(i),
        rusqlite::types::ValueRef::Text(s) => json!(String::from_utf8_lossy(s)),
        rusqlite::types::ValueRef::Real(f) => json!(f),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_videos_stay_visible_and_browser_duration_moves_clips() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        crate::test_support::source(&state);
        state.pool.get().unwrap().execute_batch("INSERT INTO media(id,source_id,filepath,filename,type,added_at) VALUES(1,1,'v','v','video','2026');").unwrap();
        let videos = list(
            State(state.clone()),
            Query(MediaQuery {
                media_type: Some("video".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(videos["media"].as_array().unwrap().len(), 1);
        let _ = set_duration(
            State(state.clone()),
            Path(1),
            Json(DurationBody {
                duration_secs: 45.0,
            }),
        )
        .await
        .unwrap();
        let clips = list(
            State(state.clone()),
            Query(MediaQuery {
                media_type: Some("clip".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(clips["media"].as_array().unwrap().len(), 1);
        let videos = list(
            State(state.clone()),
            Query(MediaQuery {
                media_type: Some("video".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(videos["media"].as_array().unwrap().is_empty());
        assert_eq!(
            set_duration(
                State(state),
                Path(1),
                Json(DurationBody {
                    duration_secs: -1.0
                })
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn undo_review_restores_auto_queue_and_rejects_stale_undo() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        crate::test_support::source(&state);
        state.pool.get().unwrap().execute_batch("INSERT INTO media(id,source_id,filepath,filename,type,added_at,auto_rating,rating,rating_source) VALUES(1,1,'a','a','image','2026',4,4,'auto');").unwrap();
        let saved = set_rating(
            State(state.clone()),
            Path(1),
            Json(RatingBody { rating: 3 }),
        )
        .await
        .unwrap()
        .0;
        let token = saved["rating_reviewed_at"].as_str().unwrap().to_string();
        let undone = undo_rating(
            State(state.clone()),
            Path(1),
            Json(UndoRatingBody {
                rating_reviewed_at: token.clone(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(undone["rating"], 4);
        assert_eq!(undone["rating_reviewed"], false);
        let queue = list(
            State(state.clone()),
            Query(MediaQuery {
                rating_status: Some("needs_review".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(queue["media"].as_array().unwrap().len(), 1);
        state.pool.get().unwrap().execute_batch("UPDATE media SET rating=2,rating_reviewed=1,rating_source='human',rating_reviewed_at='future';").unwrap();
        assert_eq!(
            undo_rating(
                State(state.clone()),
                Path(1),
                Json(UndoRatingBody {
                    rating_reviewed_at: token
                })
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            state
                .pool
                .get()
                .unwrap()
                .query_row("SELECT rating FROM media WHERE id=1", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn automated_and_human_rating_lifecycle() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        crate::test_support::source(&state);
        let conn = state.pool.get().unwrap();
        conn.execute_batch("INSERT INTO media(id,source_id,filepath,filename,type,added_at) VALUES
            (1,1,'a','a','image','2026'),(2,1,'b','b','image','2026'),(3,1,'c','c','image','2026'),(4,1,'d','d','image','2026');").unwrap();
        crate::nsfw::persist_score(&conn, 1, 0.72).unwrap();
        crate::nsfw::persist_score(&conn, 2, 0.4).unwrap();
        crate::nsfw::persist_score(&conn, 4, 0.9).unwrap();
        conn.execute("UPDATE media SET missing=1 WHERE id=4", [])
            .unwrap();
        let queue = list(
            State(state.clone()),
            Query(MediaQuery {
                rating_status: Some("needs_review".into()),
                limit: Some(1),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(queue["media"][0]["auto_rating"], 4);
        assert_eq!(queue["media"][0]["rating"], 4);
        assert_eq!(queue["media"][0]["rating_source"], "auto");
        assert_eq!(queue["media"][0]["rating_reviewed"], false);
        assert!((queue["media"][0]["auto_rating_score"].as_f64().unwrap() - 0.72).abs() < 0.00001);
        assert_eq!(queue["has_more"], true);
        let manual = set_rating(
            State(state.clone()),
            Path(1),
            Json(RatingBody { rating: 0 }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(manual["rating_source"], "human");
        assert_eq!(manual["rating_reviewed"], true);
        assert!(manual["rating_reviewed_at"].is_string());
        crate::nsfw::persist_score(&conn, 1, 0.99).unwrap();
        let row: (i64, i64, String) = conn
            .query_row(
                "SELECT rating,auto_rating,rating_source FROM media WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, (0, 5, "human".into()));
        let next = list(
            State(state.clone()),
            Query(MediaQuery {
                rating_status: Some("needs_review".into()),
                cursor: queue["next_cursor"].as_str().map(str::to_owned),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(next["media"][0]["id"], 2);
        assert_eq!(next["has_more"], false);
        let approved = approve_rating(State(state.clone()), Path(2))
            .await
            .unwrap()
            .0;
        assert_eq!(approved["rating"], approved["auto_rating"]);
        assert_eq!(approved["auto_rating"], 3);
        assert_eq!(approved["rating_source"], "human");
        assert_eq!(approved["rating_reviewed"], true);
        let queue = list(
            State(state.clone()),
            Query(MediaQuery {
                rating_status: Some("needs_review".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(queue["media"].as_array().unwrap().is_empty());
        assert_eq!(
            approve_rating(State(state.clone()), Path(3))
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            approve_rating(State(state.clone()), Path(999))
                .await
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            set_rating(State(state), Path(1), Json(RatingBody { rating: 6 }))
                .await
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn page_limit_is_capped_and_missing_media_excluded() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        crate::test_support::source(&state);
        state.pool.get().unwrap().execute_batch("WITH RECURSIVE nums(n) AS(SELECT 1 UNION ALL SELECT n+1 FROM nums WHERE n<600)
            INSERT INTO media(id,source_id,filepath,filename,type,added_at) SELECT n,1,'test/'||n,'file','image','2026' FROM nums;
            UPDATE media SET missing=1,downloaded=0 WHERE id=1;").unwrap();
        let result = list(
            State(state),
            Query(MediaQuery {
                limit: Some(usize::MAX),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(result["media"].as_array().unwrap().len(), 500);
        assert_eq!(result["media"][0]["id"], 2);
        assert_eq!(result["has_more"], true);
    }

    #[tokio::test]
    async fn keyset_pages_preserve_every_sort_and_survive_deleted_anchor() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        crate::test_support::source(&state);
        {
            let conn = state.pool.get().unwrap();
            for id in 1..=21 {
                conn.execute("INSERT INTO media(id,source_id,filepath,filename,type,added_at,rating) VALUES(?1,1,?2,?3,'image',?4,?5)",
                    rusqlite::params![id,format!("test/{id}.jpg"),if id%2==0 {"Same"}else{"same"},format!("2026-01-{:02}",id%3+1),id%5]).unwrap();
            }
        }
        for sort in [
            "default",
            "rating_desc",
            "rating_asc",
            "date_desc",
            "date_asc",
            "filename_asc",
            "filename_desc",
            "shuffle",
        ] {
            let whole = list(
                State(state.clone()),
                Query(MediaQuery {
                    sort: Some(sort.into()),
                    limit: Some(500),
                    shuffle_seed: Some(1234567),
                    ..Default::default()
                }),
            )
            .await
            .unwrap()
            .0;
            let expected: Vec<i64> = whole["media"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_i64().unwrap())
                .collect();
            let mut actual = Vec::new();
            let mut cursor = None;
            loop {
                let page = list(
                    State(state.clone()),
                    Query(MediaQuery {
                        sort: Some(sort.into()),
                        limit: Some(4),
                        shuffle_seed: Some(1234567),
                        cursor,
                        ..Default::default()
                    }),
                )
                .await
                .unwrap()
                .0;
                actual.extend(
                    page["media"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|m| m["id"].as_i64().unwrap()),
                );
                cursor = page["next_cursor"].as_str().map(str::to_owned);
                if cursor.is_none() {
                    break;
                }
                assert!(actual.len() <= 21, "cursor must advance");
            }
            assert_eq!(actual, expected, "sort {sort}");
        }
        let first = list(
            State(state.clone()),
            Query(MediaQuery {
                limit: Some(1),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        state
            .pool
            .get()
            .unwrap()
            .execute("DELETE FROM media WHERE id=1", [])
            .unwrap();
        let next = list(
            State(state),
            Query(MediaQuery {
                cursor: first["next_cursor"].as_str().map(str::to_owned),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(next["media"][0]["id"], 2);
    }

    #[tokio::test]
    async fn sql_filters_inherited_and_or_exclusion_before_pagination() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        crate::test_support::source(&state);
        {
            let conn = state.pool.get().unwrap();
            conn.execute_batch("INSERT INTO groups(id,name,added_at) VALUES(1,'Parent','2026');
                INSERT INTO groups(id,name,parent_id,added_at) VALUES(2,'Child',1,'2026');
                UPDATE sources SET group_id=2;
                INSERT INTO tags(id,name,added_at) VALUES(1,'own','2026'),(2,'exclude','2026');
                INSERT INTO media(id,source_id,filepath,filename,type,added_at) VALUES
                (1,1,'test/1','1','image','2026'),(2,1,'test/2','2','image','2026'),(3,1,'test/3','3','image','2026');
                INSERT INTO media_tags VALUES(2,1),(3,1),(3,2);").unwrap();
        }
        let result = list(
            State(state.clone()),
            Query(MediaQuery {
                tags: Some("parent,own".into()),
                exclude_tags: Some("exclude".into()),
                limit: Some(1),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(result["media"][0]["id"], 2);
        assert_eq!(result["has_more"], false);
        let result = list(
            State(state.clone()),
            Query(MediaQuery {
                any_tags: Some("absent,own".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(result["media"].as_array().unwrap().len(), 2);
        let cache1 = effective_tags(&state).await.unwrap();
        let cache2 = effective_tags(&state).await.unwrap();
        assert!(Arc::ptr_eq(&cache1, &cache2));
    }
}
