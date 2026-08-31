use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::AppState;
use crate::db::{build_group_effective_tags_map, now_iso};

// ─── Sort orders ─────────────────────────────────────────────────────────────

fn sort_order(sort: &str) -> &'static str {
    match sort {
        "rating_desc"   => "m.rating DESC, m.id ASC",
        "rating_asc"    => "m.rating ASC, m.id ASC",
        "date_desc"     => "m.added_at DESC, m.id DESC",
        "date_asc"      => "m.added_at ASC, m.id ASC",
        "filename_asc"  => "m.filename COLLATE NOCASE ASC",
        "filename_desc" => "m.filename COLLATE NOCASE DESC",
        _               => "m.id ASC",
    }
}

// ─── GET /api/media ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MediaQuery {
    source_id:     Option<i64>,
    group_id:      Option<i64>,
    only_included: Option<bool>,
    tag:           Option<String>,
    sort:          Option<String>,
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    Query(q):     Query<MediaQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let order = sort_order(q.sort.as_deref().unwrap_or("default"));

    // Get group effective tags — read from cache or rebuild
    let group_effective_tags: HashMap<i64, HashSet<String>> = {
        let cache_read = state.group_tag_cache.read().await;
        if let Some(ref cached) = *cache_read {
            cached.clone()
        } else {
            drop(cache_read);
            // rusqlite's Connection is !Send, so it must not be held across
            // the `.await` calls in this function — acquire and drop it in
            // its own scope for this synchronous rebuild step.
            let rebuilt = {
                let conn = state.pool.get().map_err(|e| (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                ))?;
                build_group_effective_tags_map(&conn)
            };
            let mut cache_write = state.group_tag_cache.write().await;
            *cache_write = Some(rebuilt.clone());
            rebuilt
        }
    };

    // Build scope SQL. Constructed here (after the awaits above) rather than
    // at the top of the function, since `scope_params` holds `Box<dyn ToSql>`
    // trait objects which are !Send and must not be live across an `.await`.
    let (scope_sql, scope_params): (&str, Vec<Box<dyn rusqlite::ToSql>>) =
        if let Some(sid) = q.source_id {
            ("SELECT id FROM media WHERE source_id=?1", vec![Box::new(sid)])
        } else if let Some(gid) = q.group_id {
            if gid == 0 {
                ("SELECT m.id FROM media m JOIN sources s ON s.id = m.source_id WHERE s.group_id IS NULL",
                 vec![])
            } else {
                ("WITH RECURSIVE subtree(id) AS (\
                    SELECT ?1 \
                    UNION ALL \
                    SELECT g.id FROM groups g JOIN subtree st ON g.parent_id = st.id\
                  ) \
                  SELECT m.id FROM media m \
                  JOIN sources s ON s.id = m.source_id \
                  WHERE s.group_id IN (SELECT id FROM subtree)",
                 vec![Box::new(gid)])
            }
        } else if q.only_included.unwrap_or(false) {
            ("SELECT m.id FROM media m JOIN sources s ON s.id = m.source_id WHERE s.included = 1",
             vec![])
        } else {
            ("SELECT id FROM media", vec![])
        };

    let query = format!(
        "SELECT m.*, s.group_id AS _source_group_id, \
            (SELECT GROUP_CONCAT(t.name, ',') FROM media_tags mt \
             JOIN tags t ON t.id = mt.tag_id WHERE mt.media_id = m.id) AS tags_csv \
         FROM media m \
         JOIN sources s ON s.id = m.source_id \
         WHERE m.id IN ({}) \
         ORDER BY {}",
        scope_sql, order
    );

    let rows: Vec<HashMap<String, Value>> = {
        let conn = state.pool.get().map_err(|e| (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ))?;
        let mut stmt = conn.prepare(&query).map_err(|e| (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        ))?;

        let params_refs: Vec<&dyn rusqlite::ToSql> =
            scope_params.iter().map(|b| b.as_ref()).collect();

        let out = stmt.query_map(params_refs.as_slice(), |row| {
            let col_count = row.as_ref().column_count();
            let col_names: Vec<String> = (0..col_count)
                .map(|i| row.as_ref().column_name(i).unwrap_or("?").to_string())
                .collect();

            let mut map = HashMap::new();
            for (i, name) in col_names.iter().enumerate() {
                let val: Value = match row.get_ref(i) {
                    Ok(rusqlite::types::ValueRef::Null)    => Value::Null,
                    Ok(rusqlite::types::ValueRef::Integer(n)) => json!(n),
                    Ok(rusqlite::types::ValueRef::Real(f))    => json!(f),
                    Ok(rusqlite::types::ValueRef::Text(s))    =>
                        json!(std::str::from_utf8(s).unwrap_or("")),
                    Ok(rusqlite::types::ValueRef::Blob(b))    =>
                        json!(std::str::from_utf8(b).unwrap_or("")),
                    Err(_) => Value::Null,
                };
                map.insert(name.clone(), val);
            }
            Ok(map)
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?
        .filter_map(|r| r.ok())
        .collect();
        out
    };

    let tag_filter = q.tag.as_deref().map(|t| t.trim().to_lowercase());

    let mut media = Vec::new();
    for mut r in rows {
        let source_group_id: Option<i64> = r.remove("_source_group_id")
            .and_then(|v| v.as_i64());
        let tags_csv = r.remove("tags_csv")
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

        if let Some(ref filter) = tag_filter {
            if !effective_tags.contains(filter.as_str()) { continue; }
        }

        let mut own_sorted: Vec<String> = own_tags.iter().cloned().collect();
        own_sorted.sort();
        let mut inh_sorted: Vec<String> = (effective_tags.difference(&own_tags)).cloned().collect();
        inh_sorted.sort();

        r.insert("tags".into(),            json!(own_sorted));
        r.insert("inherited_tags".into(),  json!(inh_sorted));
        media.push(r);
    }

    Ok(Json(json!({ "media": media })))
}

// ─── PUT /api/media/:id/rating ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RatingBody { pub rating: i64 }

pub async fn set_rating(
    State(state): State<Arc<AppState>>,
    Path(id):     Path<i64>,
    Json(body):   Json<RatingBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !(0..=5).contains(&body.rating) {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "Rating must be between 0 and 5"}))));
    }
    let conn = state.pool.get().map_err(db_err)?;
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) FROM media WHERE id=?1", [id], |r| r.get::<_, i64>(0)
    ).unwrap_or(0) > 0;
    if !exists {
        return Err((StatusCode::NOT_FOUND, Json(json!({"error": "Media not found"}))));
    }
    conn.execute("UPDATE media SET rating=?1 WHERE id=?2", rusqlite::params![body.rating, id])
        .map_err(db_err)?;
    Ok(Json(json!({ "id": id, "rating": body.rating })))
}

// ─── POST /api/media/:id/tags ────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TagBody { pub name: String }

pub async fn add_tag(
    State(state): State<Arc<AppState>>,
    Path(id):     Path<i64>,
    Json(body):   Json<TagBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let exists: bool = conn.query_row("SELECT COUNT(*) FROM media WHERE id=?1", [id], |r| r.get::<_,i64>(0)).unwrap_or(0) > 0;
    if !exists { return Err((StatusCode::NOT_FOUND, Json(json!({"error": "Media not found"})))); }

    let tag_id = get_or_create_tag(&conn, &body.name).map_err(db_err)?;
    conn.execute("INSERT OR IGNORE INTO media_tags (media_id, tag_id) VALUES (?1,?2)", rusqlite::params![id, tag_id])
        .map_err(db_err)?;

    let tags: Vec<String> = {
        let mut stmt = conn.prepare("SELECT t.name FROM media_tags mt JOIN tags t ON t.id=mt.tag_id WHERE mt.media_id=?1 ORDER BY t.name COLLATE NOCASE").map_err(db_err)?;
        let out = stmt.query_map([id], |r| r.get(0)).map_err(db_err)?
            .filter_map(|r| r.ok()).collect();
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
    conn.execute("DELETE FROM media_tags WHERE media_id=?1 AND tag_id=?2", rusqlite::params![id, tag_id])
        .map_err(db_err)?;
    Ok(Json(json!({ "status": "removed" })))
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

pub fn get_or_create_tag(conn: &rusqlite::Connection, name: &str) -> rusqlite::Result<i64> {
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    if let Ok(id) = conn.query_row("SELECT id FROM tags WHERE name=?1", [&name], |r| r.get::<_, i64>(0)) {
        return Ok(id);
    }
    conn.execute("INSERT INTO tags (name, added_at) VALUES (?1, ?2)", rusqlite::params![name, now_iso()])?;
    Ok(conn.last_insert_rowid())
}

pub fn db_err(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})))
}
