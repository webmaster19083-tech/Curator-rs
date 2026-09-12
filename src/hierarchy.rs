//! Bulk library statistics used by the compact source/group hierarchy.
//! The endpoint deliberately reads the database only; no filesystem work is
//! performed while the navigation tree is being rendered.

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

pub fn summary(conn: &Connection) -> Result<Value> {
    let mut sources = conn.prepare(
        "SELECT s.id,COUNT(m.id),COALESCE(SUM(m.file_size_bytes),0),
                COUNT(m.id)-COUNT(m.file_size_bytes)
         FROM sources s LEFT JOIN media m
           ON m.source_id=s.id AND m.downloaded=1 AND m.missing=0
         GROUP BY s.id",
    )?;
    let sources = sources
        .query_map([], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "items": row.get::<_, i64>(1)?,
                "bytes": row.get::<_, i64>(2)?,
                "unknown": row.get::<_, i64>(3)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // UNION makes malformed legacy cycles harmless while summing all nested
    // groups below each root.
    let mut groups = conn.prepare(
        "WITH RECURSIVE tree(root,id) AS (
             SELECT id,id FROM groups
             UNION
             SELECT tree.root,g.id FROM tree JOIN groups g ON g.parent_id=tree.id
         ), totals AS (
             SELECT s.id,s.group_id,COUNT(m.id) items,COALESCE(SUM(m.file_size_bytes),0) bytes,
                    COUNT(m.id)-COUNT(m.file_size_bytes) unknown
             FROM sources s LEFT JOIN media m
               ON m.source_id=s.id AND m.downloaded=1 AND m.missing=0
             GROUP BY s.id
         )
         SELECT tree.root,COALESCE(SUM(t.items),0),COALESCE(SUM(t.bytes),0),
                COALESCE(SUM(t.unknown),0),COUNT(t.id)
         FROM tree LEFT JOIN totals t ON t.group_id=tree.id
         GROUP BY tree.root",
    )?;
    let groups = groups
        .query_map([], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "items": row.get::<_, i64>(1)?,
                "bytes": row.get::<_, i64>(2)?,
                "unknown": row.get::<_, i64>(3)?,
                "sources": row.get::<_, i64>(4)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(json!({"groups":groups,"sources":sources}))
}

pub async fn endpoint(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<crate::AppState>>,
) -> Result<axum::Json<Value>, (axum::http::StatusCode, axum::Json<Value>)> {
    let pool = state.pool.clone();
    tokio::task::spawn_blocking(move || summary(&*pool.get()?))
        .await
        .map_err(crate::routes::media::db_err)?
        .map(axum::Json)
        .map_err(crate::routes::media::db_err)
}
