use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::AppState;
use crate::db::now_iso;
use crate::routes::media::{db_err, get_or_create_tag, TagBody};

// ─── Models ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateGroupBody {
    pub name:      String,
    pub parent_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct UpdateGroupBody {
    pub name:         Option<String>,
    pub parent_id:    Option<i64>,
    #[serde(default)]
    pub clear_parent: bool,
}

// ─── GET /api/groups ─────────────────────────────────────────────────────────

pub async fn list(State(state): State<Arc<AppState>>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let mut stmt = conn.prepare(
        "SELECT g.*, \
            (SELECT COUNT(*) FROM sources s WHERE s.group_id = g.id) AS source_count, \
            (SELECT GROUP_CONCAT(t.name, ',') FROM group_tags gt \
             JOIN tags t ON t.id = gt.tag_id WHERE gt.group_id = g.id) AS tags_csv \
         FROM groups g ORDER BY g.added_at"
    ).map_err(db_err)?;

    let groups: Vec<Value> = stmt.query_map([], |row| {
        let count = row.as_ref().column_count();
        let mut map = serde_json::Map::new();
        for i in 0..count {
            let name = row.as_ref().column_name(i).unwrap_or("?").to_string();
            let val: Value = match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null       => Value::Null,
                rusqlite::types::ValueRef::Integer(n) => json!(n),
                rusqlite::types::ValueRef::Real(f)    => json!(f),
                rusqlite::types::ValueRef::Text(s)    => json!(std::str::from_utf8(s).unwrap_or("")),
                rusqlite::types::ValueRef::Blob(b)    => json!(std::str::from_utf8(b).unwrap_or("")),
            };
            map.insert(name, val);
        }
        Ok(Value::Object(map))
    }).map_err(db_err)?
    .filter_map(|r| r.ok())
    .map(|mut v| {
        // Convert tags_csv to an array
        let tags_csv = v["tags_csv"].as_str().unwrap_or("").to_string();
        v.as_object_mut().unwrap().remove("tags_csv");
        let tags: Vec<Value> = if tags_csv.is_empty() {
            vec![]
        } else {
            tags_csv.split(',').map(|t| json!(t)).collect()
        };
        v.as_object_mut().unwrap().insert("tags".to_string(), json!(tags));
        v
    })
    .collect();

    Ok(Json(json!({ "groups": groups })))
}

// ─── POST /api/groups ────────────────────────────────────────────────────────

pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<CreateGroupBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "Group name is required"}))));
    }

    let conn = state.pool.get().map_err(db_err)?;

    if let Some(pid) = body.parent_id {
        let exists: bool = conn.query_row("SELECT COUNT(*) FROM groups WHERE id=?1", [pid], |r| r.get::<_,i64>(0))
            .unwrap_or(0) > 0;
        if !exists { return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "Parent group not found"})))); }
    }

    conn.execute(
        "INSERT INTO groups (name, parent_id, added_at) VALUES (?1,?2,?3)",
        rusqlite::params![name, body.parent_id, now_iso()],
    ).map_err(db_err)?;
    let new_id = conn.last_insert_rowid();

    // Invalidate group tag cache
    *state.group_tag_cache.write().await = None;

    let row = conn.query_row("SELECT * FROM groups WHERE id=?1", [new_id], row_to_json)
        .map_err(db_err)?;
    Ok(Json(row))
}

// ─── PATCH /api/groups/:id ───────────────────────────────────────────────────

pub async fn update(
    State(state): State<Arc<AppState>>,
    Path(id):     Path<i64>,
    Json(body):   Json<UpdateGroupBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // All rusqlite types (Connection/Statement/ToSql trait objects) are !Send,
    // so they must not be held across an `.await`. Do the DB work in its own
    // scope, extract a plain (Send) JSON value, then drop the connection
    // before awaiting the cache lock below.
    let group_json: Value = {
        let conn = state.pool.get().map_err(db_err)?;

        let exists: bool = conn.query_row("SELECT COUNT(*) FROM groups WHERE id=?1", [id], |r| r.get::<_,i64>(0))
            .unwrap_or(0) > 0;
        if !exists { return Err((StatusCode::NOT_FOUND, Json(json!({"error": "Group not found"})))); }

        let mut fields: Vec<String> = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(name) = body.name {
            let name = name.trim().to_string();
            if name.is_empty() { return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "Group name is required"})))); }
            fields.push("name=?".into());
            values.push(Box::new(name));
        }

        if body.clear_parent {
            fields.push("parent_id=?".into());
            values.push(Box::new(Option::<i64>::None));
        } else if let Some(pid) = body.parent_id {
            // Prevent moving a group into itself or its own descendants
            if group_is_self_or_descendant(&conn, pid, id) {
                return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "Can't move a group into itself or one of its own subgroups"}))));
            }
            let exists: bool = conn.query_row("SELECT COUNT(*) FROM groups WHERE id=?1", [pid], |r| r.get::<_,i64>(0))
                .unwrap_or(0) > 0;
            if !exists { return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "Parent group not found"})))); }
            fields.push("parent_id=?".into());
            values.push(Box::new(pid));
        }

        if fields.is_empty() { return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "Nothing to update"})))); }

        values.push(Box::new(id));
        let sql = format!("UPDATE groups SET {} WHERE id=?", fields.join(", "));
        let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        conn.execute(&sql, refs.as_slice()).map_err(db_err)?;

        conn.query_row("SELECT * FROM groups WHERE id=?1", [id], row_to_json).map_err(db_err)?
    };

    *state.group_tag_cache.write().await = None;

    Ok(Json(group_json))
}

// ─── DELETE /api/groups/:id ──────────────────────────────────────────────────

pub async fn delete(
    State(state): State<Arc<AppState>>,
    Path(id):     Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;

    let row = conn.query_row("SELECT parent_id FROM groups WHERE id=?1", [id],
        |r| r.get::<_, Option<i64>>(0)
    );
    let parent_id = match row {
        Ok(p)  => p,
        Err(_) => return Err((StatusCode::NOT_FOUND, Json(json!({"error": "Group not found"})))),
    };

    // Promote subgroups up to this group's parent (not top-level)
    conn.execute("UPDATE groups SET parent_id=?1 WHERE parent_id=?2", rusqlite::params![parent_id, id])
        .map_err(db_err)?;
    conn.execute("DELETE FROM groups WHERE id=?1", [id]).map_err(db_err)?;

    *state.group_tag_cache.write().await = None;

    Ok(Json(json!({ "status": "deleted" })))
}

// ─── POST /api/groups/:id/tags ───────────────────────────────────────────────

pub async fn add_tag(
    State(state): State<Arc<AppState>>,
    Path(id):     Path<i64>,
    Json(body):   Json<TagBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let exists: bool = conn.query_row("SELECT COUNT(*) FROM groups WHERE id=?1", [id], |r| r.get::<_,i64>(0))
        .unwrap_or(0) > 0;
    if !exists { return Err((StatusCode::NOT_FOUND, Json(json!({"error": "Group not found"})))); }

    let tag_id = get_or_create_tag(&conn, &body.name).map_err(db_err)?;
    conn.execute("INSERT OR IGNORE INTO group_tags (group_id, tag_id) VALUES (?1,?2)", rusqlite::params![id, tag_id])
        .map_err(db_err)?;

    let tags: Vec<String> = {
        let mut stmt = conn.prepare("SELECT t.name FROM group_tags gt JOIN tags t ON t.id=gt.tag_id WHERE gt.group_id=?1 ORDER BY t.name COLLATE NOCASE").map_err(db_err)?;
        let out = stmt.query_map([id], |r| r.get(0)).map_err(db_err)?
            .filter_map(|r| r.ok()).collect();
        out
    };

    *state.group_tag_cache.write().await = None;

    Ok(Json(json!({ "group_id": id, "tags": tags })))
}

// ─── DELETE /api/groups/:id/tags/:tag_id ─────────────────────────────────────

pub async fn remove_tag(
    State(state): State<Arc<AppState>>,
    Path((id, tag_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    conn.execute("DELETE FROM group_tags WHERE group_id=?1 AND tag_id=?2", rusqlite::params![id, tag_id])
        .map_err(db_err)?;
    *state.group_tag_cache.write().await = None;
    Ok(Json(json!({ "status": "removed" })))
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn group_is_self_or_descendant(conn: &rusqlite::Connection, candidate: i64, of: i64) -> bool {
    if candidate == of { return true; }
    let children: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM groups WHERE parent_id=?1").unwrap();
        stmt.query_map([of], |r| r.get(0)).unwrap()
            .filter_map(|r| r.ok()).collect()
    };
    children.iter().any(|&c| group_is_self_or_descendant(conn, candidate, c))
}

fn row_to_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let count = row.as_ref().column_count();
    let mut map = serde_json::Map::new();
    for i in 0..count {
        let name = row.as_ref().column_name(i).unwrap_or("?").to_string();
        let val: Value = match row.get_ref(i)? {
            rusqlite::types::ValueRef::Null       => Value::Null,
            rusqlite::types::ValueRef::Integer(n) => json!(n),
            rusqlite::types::ValueRef::Real(f)    => json!(f),
            rusqlite::types::ValueRef::Text(s)    => json!(std::str::from_utf8(s).unwrap_or("")),
            rusqlite::types::ValueRef::Blob(b)    => json!(std::str::from_utf8(b).unwrap_or("")),
        };
        map.insert(name, val);
    }
    Ok(Value::Object(map))
}
