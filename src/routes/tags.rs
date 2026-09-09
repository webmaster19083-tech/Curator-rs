use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};

use crate::routes::media::db_err;
use crate::routes::media::effective_tags;
use crate::AppState;

// ─── GET /api/tags ────────────────────────────────────────────────────────────

pub async fn list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Real tags with their group usage count
    struct RealTag {
        id: i64,
        name: String,
        group_count: i64,
    }
    let real_tags: Vec<RealTag> = {
        let conn = state.pool.get().map_err(db_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT t.id, t.name, \
                (SELECT COUNT(*) FROM group_tags gt WHERE gt.tag_id = t.id) AS group_count \
             FROM tags t ORDER BY t.name COLLATE NOCASE",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(RealTag {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    group_count: r.get(2)?,
                })
            })
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    // Effective tags map (read from cache or rebuild)
    let group_effective_tags = effective_tags(&state).await?;
    let conn = state.pool.get().map_err(db_err)?;

    // Media rows for effective count computation
    struct MediaRow {
        source_group_id: Option<i64>,
        tags_csv: Option<String>,
    }
    let media_rows: Vec<MediaRow> = {
        let mut stmt = conn
            .prepare(
                "SELECT s.group_id AS source_group_id, \
                (SELECT GROUP_CONCAT(t.name, ',') FROM media_tags mt \
                 JOIN tags t ON t.id = mt.tag_id WHERE mt.media_id = m.id) AS tags_csv \
             FROM media m JOIN sources s ON s.id = m.source_id",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(MediaRow {
                    source_group_id: r.get(0)?,
                    tags_csv: r.get(1)?,
                })
            })
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    // Count media_id sets per effective tag name
    let mut counts: HashMap<String, std::collections::HashSet<usize>> = HashMap::new();
    for (idx, row) in media_rows.iter().enumerate() {
        let own: std::collections::HashSet<String> = row
            .tags_csv
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(|t| t.to_string()).collect())
            .unwrap_or_default();

        let inherited = row
            .source_group_id
            .and_then(|gid| group_effective_tags.get(&gid).cloned())
            .unwrap_or_default();

        let effective: std::collections::HashSet<String> = own.union(&inherited).cloned().collect();
        for tag_name in effective {
            counts.entry(tag_name).or_default().insert(idx);
        }
    }

    // Build result — real tags + "virtual" group-name tags that appear in counts but not in tags table
    let by_name: HashMap<String, (i64, i64)> = real_tags
        .iter()
        .map(|t| (t.name.clone(), (t.id, t.group_count)))
        .collect();

    let mut all_names: std::collections::HashSet<String> = by_name.keys().cloned().collect();
    all_names.extend(counts.keys().cloned());

    let mut result: Vec<Value> = all_names
        .into_iter()
        .map(|name| {
            let (id, gc) = by_name.get(&name).copied().unwrap_or((0, 0));
            let mc = counts.get(&name).map(|s| s.len()).unwrap_or(0) as i64;
            json!({
                "id":          if id == 0 { Value::Null } else { json!(id) },
                "name":        name,
                "group_count": gc,
                "media_count": mc,
            })
        })
        .collect();

    result.sort_by(|a, b| {
        let na = a["name"].as_str().unwrap_or("").to_lowercase();
        let nb = b["name"].as_str().unwrap_or("").to_lowercase();
        na.cmp(&nb)
    });

    Ok(Json(json!({ "tags": result })))
}

/// Compact tag suggestions for the library toolbar. The full tag endpoint is
/// intentionally more expensive because it calculates inherited-group usage;
/// quick buttons should remain responsive on a large collection.
pub async fn quick(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let mut statement = conn
        .prepare(
            "SELECT t.id,t.name,COUNT(DISTINCT mt.media_id) AS media_count,t.last_used_at,
                    COALESCE(MAX(CASE p.provenance
                        WHEN 'human' THEN 4 WHEN 'human_edited' THEN 4
                        WHEN 'source_approved' THEN 3 WHEN 'automatic' THEN 2
                        ELSE 1 END),1) AS provenance_priority
             FROM tags t LEFT JOIN media_tags mt ON mt.tag_id=t.id
             LEFT JOIN media_tag_provenance p ON p.media_id=mt.media_id AND p.tag_id=mt.tag_id
             GROUP BY t.id,t.name,t.last_used_at",
        )
        .map_err(db_err)?;
    let rows: Vec<Value> = statement
        .query_map([], |row| {
            Ok(json!({
                "id":row.get::<_,i64>(0)?,"name":row.get::<_,String>(1)?,
                "media_count":row.get::<_,i64>(2)?,"last_used_at":row.get::<_,Option<String>>(3)?,
                "provenance_priority":row.get::<_,i64>(4)?,
            }))
        })
        .map_err(db_err)?
        .filter_map(Result::ok)
        .collect();
    let mut common = rows.clone();
    common.sort_by(|a, b| {
        b["provenance_priority"]
            .as_i64()
            .cmp(&a["provenance_priority"].as_i64())
            .then_with(|| b["media_count"].as_i64().cmp(&a["media_count"].as_i64()))
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    common.truncate(12);
    let mut recent = rows;
    recent.sort_by(|a, b| {
        b["provenance_priority"]
            .as_i64()
            .cmp(&a["provenance_priority"].as_i64())
            .then_with(|| b["last_used_at"].as_str().cmp(&a["last_used_at"].as_str()))
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    recent.truncate(12);
    Ok(Json(json!({"common":common,"recent":recent})))
}

// ─── DELETE /api/tags/:id ────────────────────────────────────────────────────

pub async fn delete_tag(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let exists: bool = conn
        .query_row("SELECT COUNT(*) FROM tags WHERE id=?1", [id], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0)
        > 0;
    if !exists {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Tag not found"})),
        ));
    }
    conn.execute("DELETE FROM tags WHERE id=?1", [id])
        .map_err(db_err)?;
    drop(conn);
    *state.group_tag_cache.write().await = None;
    Ok(Json(json!({ "status": "deleted" })))
}
