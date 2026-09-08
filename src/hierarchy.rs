//! Bulk library statistics. No filesystem access on the UI request path.
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

pub fn summary(conn: &Connection) -> Result<Value> {
    let mut sources = conn.prepare(
        "SELECT s.id, COUNT(m.id), COALESCE(SUM(m.file_size_bytes),0),
        COUNT(m.id)-COUNT(m.file_size_bytes) FROM sources s LEFT JOIN media m
        ON m.source_id=s.id AND m.downloaded=1 AND m.missing=0 GROUP BY s.id",
    )?;
    let sources = sources
        .query_map([], |r| {
            Ok(json!({
                "id":r.get::<_,i64>(0)?, "items":r.get::<_,i64>(1)?,
                "bytes":r.get::<_,i64>(2)?, "unknown":r.get::<_,i64>(3)?
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // UNION deduplicates ancestry pairs, including in malformed legacy cycles.
    let mut groups = conn.prepare("WITH RECURSIVE tree(root,id) AS (
        SELECT id,id FROM groups UNION SELECT tree.root,g.id FROM tree JOIN groups g ON g.parent_id=tree.id
    ), totals AS (SELECT s.id,s.group_id, COUNT(m.id) items,COALESCE(SUM(m.file_size_bytes),0) bytes,
        COUNT(m.id)-COUNT(m.file_size_bytes) unknown FROM sources s LEFT JOIN media m
        ON m.source_id=s.id AND m.downloaded=1 AND m.missing=0 GROUP BY s.id)
        SELECT tree.root,COALESCE(SUM(t.items),0),COALESCE(SUM(t.bytes),0),
        COALESCE(SUM(t.unknown),0),COUNT(t.id) FROM tree LEFT JOIN totals t ON t.group_id=tree.id GROUP BY tree.root")?;
    let groups = groups
        .query_map([], |r| {
            Ok(json!({
                "id":r.get::<_,i64>(0)?, "items":r.get::<_,i64>(1)?, "bytes":r.get::<_,i64>(2)?,
                "unknown":r.get::<_,i64>(3)?, "sources":r.get::<_,i64>(4)?
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"groups":groups,"sources":sources}))
}

pub async fn endpoint(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<crate::AppState>>,
) -> Result<axum::Json<Value>, (axum::http::StatusCode, axum::Json<Value>)> {
    tokio::task::spawn_blocking(move || summary(&*state.pool.get()?))
        .await
        .map_err(crate::routes::media::db_err)?
        .map(axum::Json)
        .map_err(crate::routes::media::db_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recursive_totals_exclude_missing_and_keep_unknown_and_empty_groups() {
        let root = tempfile::tempdir().unwrap();
        let s = crate::test_support::state(root.path());
        crate::test_support::source(&s);
        let c = s.pool.get().unwrap();
        c.execute_batch("INSERT INTO groups(id,name,parent_id,added_at) VALUES(1,'root',NULL,'now'),(2,'child',1,'now'),(3,'empty',NULL,'now'); UPDATE sources SET group_id=2;
            INSERT INTO media(source_id,filepath,filename,type,added_at,file_size_bytes,missing) VALUES
            (1,'a','a','image','now',123,0),(1,'b','b','image','now',NULL,0),(1,'c','c','image','now',900,1);").unwrap();
        let value = summary(&c).unwrap();
        assert_eq!(value["groups"][0]["items"], 2);
        assert_eq!(value["groups"][0]["bytes"], 123);
        assert_eq!(value["groups"][0]["unknown"], 1);
        assert_eq!(value["groups"][2]["items"], 0);
        c.execute("UPDATE sources SET group_id=NULL", []).unwrap();
        assert_eq!(summary(&c).unwrap()["groups"][0]["items"], 0);
        assert_eq!(summary(&c).unwrap()["sources"][0]["bytes"], 123);
    }
}
