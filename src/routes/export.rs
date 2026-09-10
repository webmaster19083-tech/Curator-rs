use axum::{extract::State, http::header, response::IntoResponse, Json};
use rusqlite::params;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use super::{bad_request, AppError};
use crate::{
    chpack, db,
    slug::{derive_name_from_url, normalize_for_compare, normalize_url, slugify},
    state::AppState,
};

// ── Source list export ───────────────────────────────────────────────────────

pub async fn export_sources(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let sources: Vec<serde_json::Value> = {
        let mut stmt = conn.prepare(
            "SELECT s.url, s.name, s.slug, s.added_at, s.synced_at, g.name AS group_name,
                (SELECT GROUP_CONCAT(t.name, ',') FROM group_tags gt
                 JOIN tags t ON t.id=gt.tag_id WHERE gt.group_id=s.group_id) AS tags_csv
             FROM sources s LEFT JOIN groups g ON g.id=s.group_id
             ORDER BY s.added_at",
        )?;
        let sources: Vec<serde_json::Value> = stmt
            .query_map([], |r| {
                Ok(json!({
                    "url": r.get::<_,String>(0)?,
                    "name": r.get::<_,String>(1)?,
                    "slug": r.get::<_,String>(2)?,
                    "added_at": r.get::<_,String>(3)?,
                    "synced_at": r.get::<_,Option<String>>(4)?,
                    "group": r.get::<_,Option<String>>(5)?,
                    "tags": r.get::<_,Option<String>>(6)?
                        .as_deref().unwrap_or("")
                        .split(',').filter(|s| !s.is_empty())
                        .collect::<Vec<_>>(),
                }))
            })?
            .filter_map(|r| r.ok())
            .collect();
        sources
    };

    // Update last_export_at + clear snooze
    let now = db::now_iso();
    {
        let mut s = state.settings.write().await;
        s.last_export_at = Some(now.clone());
        s.export_reminder_snoozed_until = None;
        s.save(&state.data_dir).map_err(anyhow::Error::from)?;
    }

    let payload = json!({
        "version": 1,
        "exported_at": now,
        "sources": sources,
    });

    let json_bytes = serde_json::to_vec_pretty(&payload).map_err(anyhow::Error::from)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"curator-sources.json\"",
            ),
        ],
        json_bytes,
    )
        .into_response())
}

// ── Source list import ───────────────────────────────────────────────────────

pub async fn import_sources(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, AppError> {
    let sources = body["sources"]
        .as_array()
        .ok_or_else(|| bad_request("Expected {\"sources\": [...]}"))?;

    let conn = state.pool.get().map_err(anyhow::Error::from)?;
    let existing: std::collections::HashMap<String, ()> = {
        let mut stmt = conn.prepare("SELECT url FROM sources")?;
        let existing: std::collections::HashMap<String, ()> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .map(|url| (normalize_for_compare(&url), ()))
            .collect();
        existing
    };

    let mut added = 0usize;
    let mut skipped = 0usize;

    for entry in sources {
        let url = entry["url"].as_str().unwrap_or("").trim().to_string();
        if url.is_empty() {
            skipped += 1;
            continue;
        }
        let url = normalize_url(&url);
        if existing.contains_key(&normalize_for_compare(&url)) {
            skipped += 1;
            continue;
        }

        let name = entry["name"].as_str().unwrap_or("").trim().to_string();
        let name = if name.is_empty() {
            derive_name_from_url(&url)
        } else {
            name
        };
        let ts = db::now_iso();
        let id: i64 = conn.query_row(
            "INSERT INTO sources (name, url, slug, status, added_at) VALUES (?,?,?,'pending',?) RETURNING id",
            params![name, url, "__tmp__", ts],
            |r| r.get(0),
        ).map_err(anyhow::Error::from)?;
        let slug = format!("{id}-{}", slugify(&name));
        conn.execute("UPDATE sources SET slug=? WHERE id=?", params![slug, id])?;
        tokio::spawn(crate::downloader::run_download(Arc::clone(&state), id));
        added += 1;
    }

    Ok(Json(json!({"added": added, "skipped": skipped})))
}

// ── CockHero .chpack export ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ChpackRequest {
    pub source_id: Option<i64>,
    pub name: Option<String>,
    #[serde(default = "default_author")]
    pub author: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub unlock_cost: i64,
}

fn default_author() -> String {
    "Curator".into()
}

pub async fn export_chpack(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ChpackRequest>,
) -> Result<impl IntoResponse, AppError> {
    let conn = state.pool.get().map_err(anyhow::Error::from)?;

    // Resolve pack name
    let pack_name = if let Some(ref n) = body.name {
        if !n.trim().is_empty() {
            n.trim().to_string()
        } else {
            default_pack_name(&conn, body.source_id)
        }
    } else {
        default_pack_name(&conn, body.source_id)
    };

    // Fetch media rows
    let rows: Vec<chpack::MediaRow> = {
        let (where_sql, id_param): (&str, Option<i64>) = match body.source_id {
            Some(sid) => ("AND m.source_id=?", Some(sid)),
            None => ("", None),
        };
        let sql = format!(
            "SELECT m.filepath, m.rating, m.type,
                (SELECT GROUP_CONCAT(t.name, ',') FROM media_tags mt
                 JOIN tags t ON t.id=mt.tag_id WHERE mt.media_id=m.id) AS tags_csv
             FROM media m WHERE m.downloaded=1 {where_sql} ORDER BY m.id"
        );
        let mut stmt = conn.prepare(&sql).map_err(anyhow::Error::from)?;
        let params_vec: Vec<&dyn rusqlite::ToSql> = match &id_param {
            Some(sid) => vec![sid],
            None => vec![],
        };
        let out: Vec<_> = stmt
            .query_map(params_vec.as_slice(), |r| {
                Ok(chpack::MediaRow {
                    filepath: r.get(0)?,
                    rating: r.get(1)?,
                    media_type: r.get(2)?,
                    tags_csv: r.get(3)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        out
    };

    if rows.is_empty() {
        return Err(bad_request("No downloaded media found for this selection"));
    }

    let library_dir = state.data_dir.join("library");
    let opts = chpack::ChpackOptions {
        name: pack_name.clone(),
        author: body.author.clone(),
        description: body.description.clone(),
        unlock_cost: body.unlock_cost,
    };

    let tmp = tokio::task::spawn_blocking(move || chpack::build_chpack(&rows, &library_dir, &opts))
        .await
        .map_err(anyhow::Error::from)??;

    let bytes = tokio::fs::read(tmp.path())
        .await
        .map_err(anyhow::Error::from)?;
    let safe_name = pack_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let filename = format!("{safe_name}.chpack");

    use axum::http::{HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    Ok((headers, bytes).into_response())
}

fn default_pack_name(conn: &rusqlite::Connection, source_id: Option<i64>) -> String {
    if let Some(sid) = source_id {
        conn.query_row("SELECT name FROM sources WHERE id=?", [sid], |r| r.get(0))
            .unwrap_or_else(|_| "Curator Pack".into())
    } else {
        "Curator Pack".into()
    }
}
