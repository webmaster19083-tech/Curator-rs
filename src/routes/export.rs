use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::{now_iso, save_settings};
use crate::routes::media::db_err;
use crate::routes::sources::create_sources_from_urls;
use crate::slug::normalize_for_compare;
use crate::AppState;

/// GET /api/export
pub async fn export_sources(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // rusqlite's Connection/Statement are !Send, so they must be dropped
    // before the `.await` below rather than held across it.
    let sources: Vec<Value> = {
        let conn = state.pool.get().map_err(db_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT s.name, s.url, s.included, g.name AS group_name \
                 FROM sources s LEFT JOIN groups g ON g.id = s.group_id \
                 ORDER BY s.added_at",
            )
            .map_err(db_err)?;

        let out = stmt
            .query_map([], |r| {
                Ok(json!({
                    "name": r.get::<_, String>(0)?,
                    "url": r.get::<_, String>(1)?,
                    "included": r.get::<_, i64>(2)? != 0,
                    "group": r.get::<_, Option<String>>(3)?,
                }))
            })
            .map_err(db_err)?
            .filter_map(|r| r.ok())
            .collect();
        out
    };

    let exported_at = now_iso();
    {
        let mut settings = state.settings.write().await;
        settings.last_export_at = Some(exported_at.clone());
        settings.export_reminder_snoozed_until = None;
        save_settings(&state.data_dir, &settings);
    }

    Ok(Json(
        json!({ "exported_at": exported_at, "sources": sources }),
    ))
}

/// POST /api/import
#[derive(Deserialize)]
pub struct ImportBody {
    pub sources: Vec<Value>,
}

pub async fn import_sources(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ImportBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let urls: Vec<String> = body
        .sources
        .iter()
        .filter_map(|s| s.get("url").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();

    if urls.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "No valid entries to import"})),
        ));
    }

    let result = create_sources_from_urls(Arc::clone(&state), urls).await?;

    // Best-effort: restore group assignments from the import file.
    if result["sources"]
        .as_array()
        .is_some_and(|sources| !sources.is_empty())
    {
        let entries_by_url: std::collections::HashMap<String, String> = body
            .sources
            .iter()
            .filter_map(|s| {
                let url = s.get("url")?.as_str()?.to_owned();
                let group = s.get("group")?.as_str()?.to_owned();
                (!group.is_empty()).then_some((normalize_for_compare(&url), group))
            })
            .collect();

        if !entries_by_url.is_empty() {
            let conn = state.pool.get().map_err(db_err)?;
            let mut group_by_name: std::collections::HashMap<String, i64> = {
                let mut stmt = conn
                    .prepare("SELECT id, name FROM groups")
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(0)?)))
                    .map_err(db_err)?
                    .filter_map(|r| r.ok())
                    .collect();
                rows
            };

            if let Some(created) = result["sources"].as_array() {
                for src in created {
                    let url = src["url"].as_str().unwrap_or_default();
                    let src_id = src["id"].as_i64().unwrap_or_default();
                    if src_id == 0 {
                        continue;
                    }

                    let Some(gname) = entries_by_url.get(&normalize_for_compare(url)).cloned()
                    else {
                        continue;
                    };

                    let gid = if let Some(&id) = group_by_name.get(&gname) {
                        id
                    } else {
                        conn.execute(
                            "INSERT INTO groups (name, added_at) VALUES (?1,?2)",
                            rusqlite::params![gname, now_iso()],
                        )
                        .map_err(db_err)?;
                        let new_id = conn.last_insert_rowid();
                        group_by_name.insert(gname, new_id);
                        new_id
                    };

                    let _ = conn.execute(
                        "UPDATE sources SET group_id=?1 WHERE id=?2",
                        rusqlite::params![gid, src_id],
                    );
                }
            }

            drop(conn);
            *state.group_tag_cache.write().await = None;
        }
    }

    Ok(Json(result))
}
