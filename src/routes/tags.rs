use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};

use crate::AppState;
use crate::db::build_group_effective_tags_map;
use crate::routes::media::db_err;

// ─── GET /api/tags ────────────────────────────────────────────────────────────

pub async fn list(State(state): State<Arc<AppState>>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;

    // Real tags with their group usage count
    struct RealTag { id: i64, name: String, group_count: i64 }
    let real_tags: Vec<RealTag> = {
        let mut stmt = conn.prepare(
            "SELECT t.id, t.name, \
                (SELECT COUNT(*) FROM group_tags gt WHERE gt.tag_id = t.id) AS group_count \
             FROM tags t ORDER BY t.name COLLATE NOCASE"
        ).map_err(db_err)?;
        let rows = stmt.query_map([], |r| Ok(RealTag {
            id:          r.get(0)?,
            name:        r.get(1)?,
            group_count: r.get(2)?,
        })).map_err(db_err)?
        .filter_map(|r| r.ok())
        .collect();
        rows
    };

    // Effective tags map (read from cache or rebuild)
    let group_effective_tags: HashMap<i64, std::collections::HashSet<String>> = {
        let cache_read = state.group_tag_cache.read().await;
        if let Some(ref cached) = *cache_read {
            cached.clone()
        } else {
            drop(cache_read);
            let rebuilt = build_group_effective_tags_map(&conn);
            let mut cache_write = state.group_tag_cache.write().await;
            *cache_write = Some(rebuilt.clone());
            rebuilt
        }
    };

    // Media rows for effective count computation
    struct MediaRow { source_group_id: Option<i64>, tags_csv: Option<String> }
    let media_rows: Vec<MediaRow> = {
        let mut stmt = conn.prepare(
            "SELECT s.group_id AS source_group_id, \
                (SELECT GROUP_CONCAT(t.name, ',') FROM media_tags mt \
                 JOIN tags t ON t.id = mt.tag_id WHERE mt.media_id = m.id) AS tags_csv \
             FROM media m JOIN sources s ON s.id = m.source_id"
        ).map_err(db_err)?;
        let rows = stmt.query_map([], |r| Ok(MediaRow {
            source_group_id: r.get(0)?,
            tags_csv:        r.get(1)?,
        })).map_err(db_err)?
        .filter_map(|r| r.ok())
        .collect();
        rows
    };

    // Count media_id sets per effective tag name
    let mut counts: HashMap<String, std::collections::HashSet<usize>> = HashMap::new();
    for (idx, row) in media_rows.iter().enumerate() {
        let own: std::collections::HashSet<String> = row.tags_csv.as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(|t| t.to_string()).collect())
            .unwrap_or_default();

        let inherited = row.source_group_id
            .and_then(|gid| group_effective_tags.get(&gid).cloned())
            .unwrap_or_default();

        let effective: std::collections::HashSet<String> = own.union(&inherited).cloned().collect();
        for tag_name in effective {
            counts.entry(tag_name).or_default().insert(idx);
        }
    }

    // Build result — real tags + "virtual" group-name tags that appear in counts but not in tags table
    let by_name: HashMap<String, (i64, i64)> = real_tags.iter()
        .map(|t| (t.name.clone(), (t.id, t.group_count)))
        .collect();

    let mut all_names: std::collections::HashSet<String> = by_name.keys().cloned().collect();
    all_names.extend(counts.keys().cloned());

    let mut result: Vec<Value> = all_names.into_iter().map(|name| {
        let (id, gc) = by_name.get(&name).copied().unwrap_or((0, 0));
        let mc = counts.get(&name).map(|s| s.len()).unwrap_or(0) as i64;
        json!({
            "id":          if id == 0 { Value::Null } else { json!(id) },
            "name":        name,
            "group_count": gc,
            "media_count": mc,
        })
    }).collect();

    result.sort_by(|a, b| {
        let na = a["name"].as_str().unwrap_or("").to_lowercase();
        let nb = b["name"].as_str().unwrap_or("").to_lowercase();
        na.cmp(&nb)
    });

    Ok(Json(json!({ "tags": result })))
}

// ─── DELETE /api/tags/:id ────────────────────────────────────────────────────

pub async fn delete_tag(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let exists: bool = conn.query_row("SELECT COUNT(*) FROM tags WHERE id=?1", [id], |r| r.get::<_,i64>(0))
        .unwrap_or(0) > 0;
    if !exists { return Err((StatusCode::NOT_FOUND, Json(json!({"error": "Tag not found"})))); }
    conn.execute("DELETE FROM tags WHERE id=?1", [id]).map_err(db_err)?;
    *state.group_tag_cache.write().await = None;
    Ok(Json(json!({ "status": "deleted" })))
}
