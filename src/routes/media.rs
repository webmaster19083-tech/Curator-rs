use std::{collections::HashSet, sync::Arc};
use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{db, state::AppState};
use super::{bad_request, not_found, AppError};

#[derive(Deserialize)]
pub struct MediaQuery {
    pub source_id: Option<i64>,
    pub group_id: Option<i64>,
    #[serde(default)]
    pub only_included: bool,
    pub tag: Option<String>,
    #[serde(default = "default_sort")]
    pub sort: String,
}

fn default_sort() -> String { "default".into() }

fn sort_sql(sort: &str) -> &'static str {
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

pub async fn list(
    Query(q): Query<MediaQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let order = sort_sql(&q.sort);

    // Build scope SQL
    let (scope_sql, scope_params): (&str, Vec<i64>) = if let Some(sid) = q.source_id {
        ("SELECT id FROM media WHERE source_id=?", vec![sid])
    } else if let Some(gid) = q.group_id {
        if gid == 0 {
            ("SELECT m.id FROM media m JOIN sources s ON s.id=m.source_id WHERE s.group_id IS NULL", vec![])
        } else {
            ("WITH RECURSIVE subtree(id) AS (
                SELECT ?
                UNION ALL
                SELECT g.id FROM groups g JOIN subtree st ON g.parent_id = st.id
              )
              SELECT m.id FROM media m JOIN sources s ON s.id=m.source_id
              WHERE s.group_id IN (SELECT id FROM subtree)",
             vec![gid])
        }
    } else if q.only_included {
        ("SELECT m.id FROM media m JOIN sources s ON s.id=m.source_id WHERE s.included=1", vec![])
    } else {
        ("SELECT id FROM media", vec![])
    };

    let query = format!(
        "SELECT m.*, s.group_id AS _src_gid,
            (SELECT GROUP_CONCAT(t.name, ',') FROM media_tags mt
             JOIN tags t ON t.id=mt.tag_id WHERE mt.media_id=m.id) AS tags_csv
         FROM media m JOIN sources s ON s.id=m.source_id
         WHERE m.id IN ({scope_sql})
         ORDER BY {order}"
    );

    let conn = state.pool.get().map_err(anyhow::Error::from)?;

    // Rebuild cache if empty (first request or invalidated)
    {
        let cache = state.group_tag_cache.read().await;
        if cache.map.is_empty() {
            drop(cache);
            let new_map = db::build_group_effective_tags_map(&conn)?;
            let mut w = state.group_tag_cache.write().await;
            w.map = new_map;
        }
    }
    let cache = state.group_tag_cache.read().await;

    let tag_filter: Option<String> = q.tag.as_deref().map(|t| t.trim().to_lowercase());

    let mut stmt = conn.prepare(&query)?;
    let rows: Vec<Value> = stmt
        .query_map(rusqlite::params_from_iter(scope_params.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,       // id
                r.get::<_, i64>(1)?,       // source_id
                r.get::<_, String>(2)?,    // filepath
                r.get::<_, String>(3)?,    // filename
                r.get::<_, String>(4)?,    // type
                r.get::<_, String>(5)?,    // added_at
                r.get::<_, i64>(6)?,       // rating
                r.get::<_, Option<String>>(7)?, // origin_url
                r.get::<_, i64>(8)?,       // downloaded
                r.get::<_, Option<i64>>(9)?,   // _src_gid
                r.get::<_, Option<String>>(10)?, // tags_csv
            ))
        })?
        .filter_map(|r| r.ok())
        .filter_map(|(id, source_id, filepath, filename, mtype, added_at,
                       rating, origin_url, downloaded, src_gid, tags_csv)| {
            let own_tags: HashSet<String> = tags_csv
                .as_deref()
                .unwrap_or("")
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();

            let empty = HashSet::new();
            let group_tags = src_gid
                .and_then(|gid| cache.map.get(&gid))
                .unwrap_or(&empty);

            let effective: HashSet<&String> = own_tags.iter().chain(group_tags.iter()).collect();

            if let Some(ref tf) = tag_filter {
                if !effective.contains(tf) { return None; }
            }

            let inherited: Vec<String> = group_tags
                .iter()
                .filter(|t| !own_tags.contains(*t))
                .cloned()
                .collect();
            let mut own_sorted: Vec<String> = own_tags.into_iter().collect();
            own_sorted.sort();
            let mut inh_sorted = inherited;
            inh_sorted.sort();

            Some(json!({
                "id": id,
                "source_id": source_id,
                "filepath": filepath,
                "filename": filename,
                "type": mtype,
                "added_at": added_at,
                "rating": rating,
                "origin_url": origin_url,
                "downloaded": downloaded != 0,
                "tags": own_sorted,
                "inherited_tags": inh_sorted,
            }))
        })
        .collect();

    Ok(Json(json!({"media": rows})))
}

#[derive(Deserialize)]
pub struct RatingBody { pub rating: i64 }

pub async fn set_rating(
    Path(id): Path<i64>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<RatingBody>,
) -> Result<impl IntoResponse, AppError> {
    if !(0..=5).contains(&body.rating) {
        return Err(bad_request("Rating must be between 0 and 5"));
    }
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let exists: bool = conn.query_row("SELECT id FROM media WHERE id=?", [id], |_| Ok(())).is_ok();
    if !exists { return Err(not_found("Media not found")); }
    conn.execute("UPDATE media SET rating=? WHERE id=?", params![body.rating, id])?;
    Ok(Json(json!({"id": id, "rating": body.rating})))
}

#[derive(Deserialize)]
pub struct TagBody { pub name: String }

pub async fn add_tag(
    Path(id): Path<i64>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<TagBody>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let exists: bool = conn.query_row("SELECT id FROM media WHERE id=?", [id], |_| Ok(())).is_ok();
    if !exists { return Err(not_found("Media not found")); }
    let tag_id = get_or_create_tag(&conn, &body.name)?;
    conn.execute(
        "INSERT OR IGNORE INTO media_tags (media_id, tag_id) VALUES (?,?)",
        params![id, tag_id],
    )?;
    let tags = get_media_tags(&conn, id)?;
    Ok(Json(json!({"media_id": id, "tags": tags})))
}

pub async fn remove_tag(
    Path((media_id, tag_id)): Path<(i64, i64)>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    conn.execute(
        "DELETE FROM media_tags WHERE media_id=? AND tag_id=?",
        params![media_id, tag_id],
    )?;
    Ok(Json(json!({"status": "removed"})))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub fn get_or_create_tag(conn: &rusqlite::Connection, name: &str) -> Result<i64, AppError> {
    let name = name.trim().to_lowercase();
    if name.is_empty() { return Err(bad_request("Tag name is required")); }
    if let Ok(id) = conn.query_row("SELECT id FROM tags WHERE name=?", [&name], |r| r.get::<_,i64>(0)) {
        return Ok(id);
    }
    let id = conn.query_row(
        "INSERT INTO tags (name, added_at) VALUES (?,?) RETURNING id",
        params![name, db::now_iso()],
        |r| r.get(0),
    ).map_err(anyhow::Error::from)?;
    Ok(id)
}

fn get_media_tags(conn: &rusqlite::Connection, media_id: i64) -> Result<Vec<String>, AppError> {
    let mut stmt = conn.prepare(
        "SELECT t.name FROM media_tags mt JOIN tags t ON t.id=mt.tag_id
         WHERE mt.media_id=? ORDER BY t.name COLLATE NOCASE",
    ).map_err(anyhow::Error::from)?;
    let names: Vec<String> = stmt.query_map([media_id], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(names)
}
