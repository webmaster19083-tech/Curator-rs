use std::sync::Arc;
use axum::{extract::{Path, State}, response::IntoResponse, Json};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{db, state::AppState};
use super::{bad_request, not_found, AppError};
use crate::routes::media::get_or_create_tag;

#[derive(Deserialize)]
pub struct GroupCreateRequest {
    pub name: String,
    pub parent_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct GroupUpdateRequest {
    pub name: Option<String>,
    pub parent_id: Option<i64>,
    #[serde(default)]
    pub clear_parent: bool,
}

#[derive(Deserialize)]
pub struct TagBody { pub name: String }

pub async fn list(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let mut stmt = conn.prepare(
        "SELECT g.*,
            (SELECT COUNT(*) FROM sources s WHERE s.group_id=g.id) AS source_count,
            (SELECT GROUP_CONCAT(t.name, ',') FROM group_tags gt
             JOIN tags t ON t.id=gt.tag_id WHERE gt.group_id=g.id) AS tags_csv
         FROM groups g ORDER BY g.added_at",
    )?;
    let groups: Vec<Value> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_,i64>(0)?,
                r.get::<_,String>(1)?,
                r.get::<_,Option<i64>>(2)?,
                r.get::<_,String>(3)?,
                r.get::<_,i64>(4)?,
                r.get::<_,Option<String>>(5)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .map(|(id, name, parent_id, added_at, source_count, tags_csv)| {
            let tags: Vec<&str> = tags_csv.as_deref().unwrap_or("")
                .split(',').filter(|s| !s.is_empty()).collect();
            json!({
                "id": id, "name": name, "parent_id": parent_id,
                "added_at": added_at, "source_count": source_count,
                "tags": tags,
            })
        })
        .collect();
    Ok(Json(json!({"groups": groups})))
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Json(body): Json<GroupCreateRequest>,
) -> Result<impl IntoResponse, AppError> {
    let name = body.name.trim().to_string();
    if name.is_empty() { return Err(bad_request("Group name is required")); }
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    if let Some(pid) = body.parent_id {
        let ok = conn.query_row("SELECT id FROM groups WHERE id=?", [pid], |_| Ok(())).is_ok();
        if !ok { return Err(bad_request("Parent group not found")); }
    }
    let id: i64 = conn.query_row(
        "INSERT INTO groups (name, parent_id, added_at) VALUES (?,?,?) RETURNING id",
        params![name, body.parent_id, db::now_iso()],
        |r| r.get(0),
    ).map_err(anyhow::Error::from)?;
    // Invalidate cache
    state.group_tag_cache.write().await.map.clear();
    let row = conn.query_row("SELECT * FROM groups WHERE id=?", [id], |r| {
        Ok(json!({
            "id": r.get::<_,i64>(0)?,
            "name": r.get::<_,String>(1)?,
            "parent_id": r.get::<_,Option<i64>>(2)?,
            "added_at": r.get::<_,String>(3)?,
        }))
    }).map_err(anyhow::Error::from)?;
    Ok(Json(row))
}

fn do_group_update(
    conn: &rusqlite::Connection,
    id: i64,
    body: &GroupUpdateRequest,
) -> Result<(), AppError> {
    let mut sets: Vec<String> = Vec::new();
    let mut vals: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(name) = &body.name {
        let name = name.trim().to_string();
        if name.is_empty() { return Err(bad_request("Group name is required")); }
        sets.push("name=?".into()); vals.push(Box::new(name));
    }
    if body.clear_parent {
        sets.push("parent_id=NULL".into());
    } else if let Some(pid) = body.parent_id {
        if is_self_or_descendant(conn, pid, id) {
            return Err(bad_request("Can't move a group into itself or one of its own subgroups"));
        }
        let ok = conn.query_row("SELECT id FROM groups WHERE id=?", [pid], |_| Ok(())).is_ok();
        if !ok { return Err(bad_request("Parent group not found")); }
        sets.push("parent_id=?".into()); vals.push(Box::new(pid));
    }
    if sets.is_empty() { return Err(bad_request("Nothing to update")); }

    let sql = format!("UPDATE groups SET {} WHERE id=?", sets.join(", "));
    vals.push(Box::new(id));
    let p: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|v| v.as_ref()).collect();
    conn.execute(&sql, p.as_slice())?;
    Ok(())
}

pub async fn update(
    Path(id): Path<i64>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<GroupUpdateRequest>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let exists = conn.query_row("SELECT id FROM groups WHERE id=?", [id], |_| Ok(())).is_ok();
    if !exists { return Err(not_found("Group not found")); }

    do_group_update(&conn, id, &body)?;
    drop(conn);
    state.group_tag_cache.write().await.map.clear();
    let conn = state.pool.get().map_err(anyhow::Error::from)?;

    let row = conn.query_row("SELECT * FROM groups WHERE id=?", [id], |r| {
        Ok(json!({
            "id": r.get::<_,i64>(0)?,
            "name": r.get::<_,String>(1)?,
            "parent_id": r.get::<_,Option<i64>>(2)?,
            "added_at": r.get::<_,String>(3)?,
        }))
    }).map_err(anyhow::Error::from)?;
    Ok(Json(row))
}

pub async fn delete(
    Path(id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let parent_id: Option<i64> = conn
        .query_row("SELECT parent_id FROM groups WHERE id=?", [id], |r| r.get(0))
        .map_err(|_| not_found("Group not found"))?;
    // Promote children up to this group's parent
    conn.execute("UPDATE groups SET parent_id=? WHERE parent_id=?", params![parent_id, id])?;
    conn.execute("DELETE FROM groups WHERE id=?", [id])?;
    state.group_tag_cache.write().await.map.clear();
    Ok(Json(json!({"status": "deleted"})))
}

pub async fn add_tag(
    Path(id): Path<i64>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<TagBody>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let exists = conn.query_row("SELECT id FROM groups WHERE id=?", [id], |_| Ok(())).is_ok();
    if !exists { return Err(not_found("Group not found")); }
    let tag_id = get_or_create_tag(&conn, &body.name)?;
    conn.execute(
        "INSERT OR IGNORE INTO group_tags (group_id, tag_id) VALUES (?,?)",
        params![id, tag_id],
    )?;
    state.group_tag_cache.write().await.map.clear();
    let tags = get_group_tags(&conn, id)?;
    Ok(Json(json!({"group_id": id, "tags": tags})))
}

pub async fn remove_tag(
    Path((group_id, tag_id)): Path<(i64, i64)>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    conn.execute(
        "DELETE FROM group_tags WHERE group_id=? AND tag_id=?",
        params![group_id, tag_id],
    )?;
    state.group_tag_cache.write().await.map.clear();
    Ok(Json(json!({"status": "removed"})))
}

fn is_self_or_descendant(conn: &rusqlite::Connection, candidate: i64, of: i64) -> bool {
    if candidate == of { return true; }
    let children: Vec<i64> = conn
        .prepare("SELECT id FROM groups WHERE parent_id=?").ok()
        .map(|mut s| s.query_map([of], |r| r.get(0)).ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default())
        .unwrap_or_default();
    children.iter().any(|&c| is_self_or_descendant(conn, candidate, c))
}

fn get_group_tags(conn: &rusqlite::Connection, group_id: i64) -> Result<Vec<String>, AppError> {
    let mut stmt = conn.prepare(
        "SELECT t.name FROM group_tags gt JOIN tags t ON t.id=gt.tag_id
         WHERE gt.group_id=? ORDER BY t.name COLLATE NOCASE",
    ).map_err(anyhow::Error::from)?;
    let names: Vec<String> = stmt.query_map([group_id], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(names)
}
