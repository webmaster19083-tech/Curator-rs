use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::{bad_request, not_found, AppError};
use crate::{
    db,
    downloader::run_download,
    slug::{derive_name_from_url, normalize_for_compare, normalize_url, slugify},
    state::AppState,
};

#[derive(Deserialize)]
pub struct AddSourcesRequest {
    #[serde(default)]
    pub urls: Vec<String>,
    pub text: Option<String>,
}

#[derive(Deserialize)]
pub struct PatchSourceRequest {
    pub name: Option<String>,
    pub included: Option<bool>,
}

#[derive(Deserialize)]
pub struct SourceGroupRequest {
    pub group_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct DeleteSourceQuery {
    #[serde(default)]
    pub delete_files: bool,
}

pub async fn list(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let mut stmt = conn.prepare(
        "SELECT s.*,
            (SELECT id FROM media m WHERE m.source_id = s.id AND m.type = 'image'
             ORDER BY m.id LIMIT 1) AS thumbnail_id
         FROM sources s ORDER BY s.added_at DESC",
    )?;
    let rows: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_,i64>(0)?,
                "name": r.get::<_,String>(1)?,
                "url": r.get::<_,String>(2)?,
                "slug": r.get::<_,String>(3)?,
                "status": r.get::<_,String>(4)?,
                "item_count": r.get::<_,i64>(5)?,
                "included": r.get::<_,i64>(6)? != 0,
                "group_id": r.get::<_,Option<i64>>(7)?,
                "error_message": r.get::<_,Option<String>>(8)?,
                "log": r.get::<_,Option<String>>(9)?,
                "added_at": r.get::<_,String>(10)?,
                "synced_at": r.get::<_,Option<String>>(11)?,
                "thumbnail_id": r.get::<_,Option<i64>>(12)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(json!({"sources": rows})))
}

pub async fn get_one(
    Path(id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let row = conn.query_row("SELECT * FROM sources WHERE id=?", [id], |r| {
        Ok(source_row_to_json(r))
    });
    match row {
        Ok(Ok(v)) => Ok(Json(v)),
        _ => Err(not_found("Source not found")),
    }
}

pub async fn add(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AddSourcesRequest>,
) -> Result<impl IntoResponse, AppError> {
    let mut candidates = payload.urls;
    if let Some(text) = payload.text {
        candidates.extend(split_bulk_input(&text));
    }
    if candidates.is_empty() {
        return Err(bad_request("No valid URLs provided"));
    }
    let result = create_sources_from_urls(Arc::clone(&state), candidates).await?;
    if result["sources"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(true)
        && result["duplicates"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true)
    {
        return Err(bad_request("No valid URLs provided"));
    }
    Ok(Json(result))
}

pub async fn patch(
    Path(id): Path<i64>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<PatchSourceRequest>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let mut fields = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(name) = body.name {
        fields.push("name=?");
        values.push(Box::new(name));
    }
    if let Some(inc) = body.included {
        fields.push("included=?");
        values.push(Box::new(if inc { 1i64 } else { 0i64 }));
    }
    if fields.is_empty() {
        return Err(bad_request("Nothing to update"));
    }
    let sql = format!("UPDATE sources SET {} WHERE id=?", fields.join(", "));
    values.push(Box::new(id));
    let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    let n = conn.execute(&sql, params.as_slice())?;
    if n == 0 {
        return Err(not_found("Source not found"));
    }
    let row = conn.query_row("SELECT * FROM sources WHERE id=?", [id], |r| {
        Ok(source_row_to_json(r))
    })??;
    Ok(Json(row))
}

pub async fn set_group(
    Path(id): Path<i64>,
    State(state): State<Arc<AppState>>,
    Json(body): Json<SourceGroupRequest>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let exists: bool = conn
        .query_row("SELECT id FROM sources WHERE id=?", [id], |_| Ok(()))
        .is_ok();
    if !exists {
        return Err(not_found("Source not found"));
    }
    if let Some(gid) = body.group_id {
        let g_exists: bool = conn
            .query_row("SELECT id FROM groups WHERE id=?", [gid], |_| Ok(()))
            .is_ok();
        if !g_exists {
            return Err(bad_request("Group not found"));
        }
    }
    conn.execute(
        "UPDATE sources SET group_id=? WHERE id=?",
        params![body.group_id, id],
    )?;
    let row = conn.query_row("SELECT * FROM sources WHERE id=?", [id], |r| {
        Ok(source_row_to_json(r))
    })??;
    Ok(Json(row))
}

pub async fn delete(
    Path(id): Path<i64>,
    Query(q): Query<DeleteSourceQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let row = conn.query_row("SELECT slug FROM sources WHERE id=?", [id], |r| {
        r.get::<_, String>(0)
    });
    let slug = match row {
        Ok(s) => s,
        Err(_) => return Err(not_found("Source not found")),
    };
    conn.execute("DELETE FROM sources WHERE id=?", [id])?;

    // Kill in-flight process before touching files
    let pid = state.active_processes.lock().unwrap().remove(&id);
    if let Some(pid) = pid {
        kill_by_pid(pid);
    }

    if q.delete_files {
        let library_dir = state.data_dir.join("library");
        let dest = dunce::simplified(&library_dir.join(&slug)).to_path_buf();
        if dest.exists() {
            let _ = std::fs::remove_dir_all(&dest);
        }
        let archive = dunce::simplified(
            &state
                .data_dir
                .join("archives")
                .join(format!("{slug}.sqlite3")),
        )
        .to_path_buf();
        let _ = std::fs::remove_file(&archive);
    }

    Ok(Json(json!({"status": "deleted"})))
}

pub async fn resync(
    Path(id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let row = conn.query_row("SELECT status FROM sources WHERE id=?", [id], |r| {
        r.get::<_, String>(0)
    });
    let status = match row {
        Ok(s) => s,
        Err(_) => return Err(not_found("Source not found")),
    };
    if status == "pending" || status == "downloading" {
        return Ok(Json(json!({"status": "already_syncing"})));
    }
    if state.downloads_paused.load(Ordering::SeqCst) {
        return Ok(Json(json!({"status": "paused"})));
    }
    tokio::spawn(run_download(Arc::clone(&state), id));
    Ok(Json(json!({"status": "queued"})))
}

pub async fn resync_all(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    if state.downloads_paused.load(Ordering::SeqCst) {
        return Ok(Json(json!({"queued": 0, "paused": true})));
    }
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let ids: Vec<i64> = {
        let mut stmt =
            conn.prepare("SELECT id FROM sources WHERE status NOT IN ('pending','downloading')")?;
        let ids: Vec<i64> = stmt
            .query_map([], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        ids
    };
    let count = ids.len();
    for id in ids {
        tokio::spawn(run_download(Arc::clone(&state), id));
    }
    tracing::info!("Resync all: queued {count} source(s)");
    Ok(Json(json!({"queued": count})))
}

pub async fn get_log(
    Path(id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let log: Option<String> = conn
        .query_row("SELECT log FROM sources WHERE id=?", [id], |r| r.get(0))
        .map_err(|_| not_found("Source not found"))?;
    Ok(Json(json!({"log": log.unwrap_or_default()})))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn source_row_to_json(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_,i64>(0)?,
        "name": r.get::<_,String>(1)?,
        "url": r.get::<_,String>(2)?,
        "slug": r.get::<_,String>(3)?,
        "status": r.get::<_,String>(4)?,
        "item_count": r.get::<_,i64>(5)?,
        "included": r.get::<_,i64>(6)? != 0,
        "group_id": r.get::<_,Option<i64>>(7)?,
        "error_message": r.get::<_,Option<String>>(8)?,
        "log": r.get::<_,Option<String>>(9)?,
        "added_at": r.get::<_,String>(10)?,
        "synced_at": r.get::<_,Option<String>>(11)?,
    }))
}

async fn create_sources_from_urls(
    state: Arc<AppState>,
    candidates: Vec<String>,
) -> Result<Value, AppError> {
    let mut normalized: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for c in candidates {
        let u = normalize_url(&c);
        if !u.is_empty() && seen.insert(u.clone()) {
            normalized.push(u);
        }
    }
    if normalized.is_empty() {
        return Ok(json!({"sources": [], "duplicates": []}));
    }

    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let existing: std::collections::HashMap<String, String> = {
        let mut stmt = conn.prepare("SELECT url, name FROM sources")?;
        let existing: std::collections::HashMap<String, String> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .map(|(url, name)| (normalize_for_compare(&url), name))
            .collect();
        existing
    };

    let mut to_create: Vec<String> = Vec::new();
    let mut duplicates: Vec<Value> = Vec::new();
    let mut guard = existing.clone();

    for url in &normalized {
        let key = normalize_for_compare(url);
        if let Some(name) = guard.get(&key) {
            duplicates.push(json!({"url": url, "name": name}));
        } else {
            guard.insert(key, String::new());
            to_create.push(url.clone());
        }
    }

    let mut created: Vec<Value> = Vec::new();
    for url in to_create {
        let name = derive_name_from_url(&url);
        let ts = db::now_iso();
        let id: i64 = conn.query_row(
            "INSERT INTO sources (name, url, slug, status, added_at) VALUES (?1,?2,?3,'pending',?4) RETURNING id",
            params![name, url, "__tmp__", ts],
            |r| r.get(0),
        )?;
        let slug = format!("{id}-{}", slugify(&name));
        conn.execute("UPDATE sources SET slug=? WHERE id=?", params![slug, id])?;
        created
            .push(json!({"id": id, "name": name, "url": url, "slug": slug, "status": "pending"}));
        tokio::spawn(run_download(Arc::clone(&state), id));
    }

    Ok(json!({"sources": created, "duplicates": duplicates}))
}

fn split_bulk_input(text: &str) -> Vec<String> {
    text.split(['\n', ','])
        .map(|p| normalize_url(p))
        .filter(|u| !u.is_empty())
        .collect()
}

fn kill_by_pid(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .spawn();
    }
}
