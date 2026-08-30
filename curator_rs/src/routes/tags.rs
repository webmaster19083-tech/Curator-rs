use std::{collections::{HashMap, HashSet}, sync::Arc};
use axum::{extract::{Path, State}, response::IntoResponse, Json};
use serde_json::json;

use crate::{db, state::AppState};
use super::{not_found, AppError};

pub async fn list(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;

    // Rebuild cache if empty
    {
        let cache = state.group_tag_cache.read().await;
        if cache.map.is_empty() {
            drop(cache);
            let new_map = db::build_group_effective_tags_map(&conn)?;
            state.group_tag_cache.write().await.map = new_map;
        }
    }
    let cache = state.group_tag_cache.read().await;

    let real_tags: Vec<(i64, String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT t.id, t.name,
                (SELECT COUNT(*) FROM group_tags gt WHERE gt.tag_id=t.id) AS group_count
             FROM tags t ORDER BY t.name COLLATE NOCASE",
        )?;
        let real_tags: Vec<(i64, String, i64)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .filter_map(|r| r.ok())
            .collect();
        real_tags
    };

    // Compute effective media counts per tag name
    let mut counts: HashMap<String, HashSet<i64>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT m.id, s.group_id,
                (SELECT GROUP_CONCAT(t.name,',') FROM media_tags mt
                 JOIN tags t ON t.id=mt.tag_id WHERE mt.media_id=m.id) AS tags_csv
             FROM media m JOIN sources s ON s.id=m.source_id",
        )?;
        for row in stmt.query_map([], |r| {
            Ok((r.get::<_,i64>(0)?, r.get::<_,Option<i64>>(1)?, r.get::<_,Option<String>>(2)?))
        })? {
            let (mid, gid, tags_csv) = row?;
            let own: HashSet<String> = tags_csv.as_deref().unwrap_or("")
                .split(',').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
            let empty = HashSet::new();
            let group_tags = gid.and_then(|g| cache.map.get(&g)).unwrap_or(&empty);
            for t in own.iter().chain(group_tags.iter()) {
                counts.entry(t.clone()).or_default().insert(mid);
            }
        }
    }

    let by_name: HashMap<&str, (i64, i64)> = real_tags.iter()
        .map(|(id, name, gc)| (name.as_str(), (*id, *gc)))
        .collect();

    let all_names: HashSet<String> = by_name.keys().map(|s| s.to_string())
        .chain(counts.keys().cloned()).collect();

    let mut result: Vec<serde_json::Value> = all_names.into_iter().map(|name| {
        let (id, gc) = by_name.get(name.as_str()).copied().unwrap_or((0, 0));
        json!({
            "id": if id == 0 { serde_json::Value::Null } else { json!(id) },
            "name": name,
            "group_count": gc,
            "media_count": counts.get(&name).map(|s| s.len()).unwrap_or(0),
        })
    }).collect();
    result.sort_by(|a, b| {
        a["name"].as_str().unwrap_or("").to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });

    Ok(Json(json!({"tags": result})))
}

pub async fn delete(
    Path(id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let exists = conn.query_row("SELECT id FROM tags WHERE id=?", [id], |_| Ok(())).is_ok();
    if !exists { return Err(not_found("Tag not found")); }
    conn.execute("DELETE FROM tags WHERE id=?", [id])?;
    Ok(Json(json!({"status": "deleted"})))
}
