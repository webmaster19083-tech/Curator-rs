use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use rand::seq::SliceRandom;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::{build_group_effective_tags_map, now_iso};
use crate::routes::media::db_err;
use crate::AppState;

// ─── GET /api/ch/playlist ────────────────────────────────────────────────────
// Returns a filtered, optionally shuffled list of downloaded media for a session.

#[derive(Deserialize)]
pub struct PlaylistQuery {
    /// Comma-separated list of tags to require (AND). Empty = no tag filter.
    tags:         Option<String>,
    /// Comma-separated list of tags to exclude (ANY match drops the item).
    exclude_tags: Option<String>,
    /// "image", "video", or omit for both. Accepts `type` as an alias so
    /// the Tier 3 static/ch/ frontend (which sends `type=`) keeps working
    /// without a JS change alongside the plan's `media_type` naming.
    #[serde(alias = "type")]
    media_type:   Option<String>,
    /// Comma-separated source IDs. Combined with `groups` as an OR/union
    /// of scopes — set either, both, or neither (neither = all included
    /// sources, the previous default behavior).
    sources:      Option<String>,
    /// Comma-separated group IDs (each includes its subgroups). `0` means
    /// "ungrouped" within the list, same as the old singular group_id=0.
    groups:       Option<String>,
    /// Maximum items to return. Default pulled from settings.
    limit:        Option<u32>,
    /// Whether to shuffle. Default pulled from settings.
    shuffle:      Option<bool>,
    /// Minimum rating (1–5 inclusive). 0 or omit = any.
    min_rating:   Option<i64>,
}

fn parse_id_list(raw: &str) -> Vec<i64> {
    raw.split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect()
}

pub async fn get_playlist(
    State(state): State<Arc<AppState>>,
    Query(q):     Query<PlaylistQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;

    let settings     = state.settings.read().await;
    let limit        = q.limit.unwrap_or(settings.ch_default_limit) as usize;
    let do_shuffle   = q.shuffle.unwrap_or(settings.ch_default_shuffle);
    let default_kind = settings.ch_default_media_type.clone();
    drop(settings);

    let kind_filter: Option<String> = q.media_type
        .as_deref()
        .filter(|s| !s.is_empty() && *s != "all")
        .map(|s| s.to_string())
        .or_else(|| if default_kind != "all" { Some(default_kind.clone()) } else { None });

    // Resolve effective tag map (cache hit preferred)
    let group_effective_tags: HashMap<i64, HashSet<String>> = {
        let read = state.group_tag_cache.read().await;
        if let Some(ref cached) = *read {
            cached.clone()
        } else {
            drop(read);
            let rebuilt = build_group_effective_tags_map(&conn);
            let mut write = state.group_tag_cache.write().await;
            *write = Some(rebuilt.clone());
            rebuilt
        }
    };

    let source_ids: Vec<i64> = q.sources.as_deref().map(parse_id_list).unwrap_or_default();
    let group_ids:  Vec<i64> = q.groups.as_deref().map(parse_id_list).unwrap_or_default();

    // Build the scope predicate. Explicit sources/groups are unioned with OR;
    // with neither set, fall back to "every included source" (previous default).
    let mut scope_clauses: Vec<String> = Vec::new();

    if !source_ids.is_empty() {
        let list = source_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        scope_clauses.push(format!("m.source_id IN ({})", list));
    }

    // `groups=0` (ungrouped) and real group ids need different SQL shapes,
    // so split them out; a real group id also needs its subtree included.
    let ungrouped_requested = group_ids.contains(&0);
    let real_group_ids: Vec<i64> = group_ids.iter().copied().filter(|&g| g != 0).collect();

    let with_clause = if real_group_ids.is_empty() {
        String::new()
    } else {
        // SQLite doesn't accept `SELECT 1,2 AS id` for multiple seed rows,
        // so build the seed as a UNION of one-row SELECTs regardless of count.
        let seed_union = real_group_ids.iter()
            .map(|i| format!("SELECT {} AS id", i))
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        scope_clauses.push("s.group_id IN (SELECT id FROM subtree)".to_string());
        format!(
            "WITH RECURSIVE subtree(id) AS (\
                {seed_union} \
                UNION ALL \
                SELECT g.id FROM groups g JOIN subtree st ON g.parent_id = st.id\
            ) "
        )
    };

    if ungrouped_requested {
        scope_clauses.push("s.group_id IS NULL".to_string());
    }

    let scope_sql = if scope_clauses.is_empty() {
        String::new() // no explicit scope — every included source
    } else {
        format!(" AND ({})", scope_clauses.join(" OR "))
    };

    let base_sql = format!(
        "{with_clause}\
         SELECT m.id, m.filepath, m.filename, m.type, m.rating, s.group_id, \
            (SELECT GROUP_CONCAT(t.name, ',') FROM media_tags mt JOIN tags t ON t.id=mt.tag_id WHERE mt.media_id=m.id) AS tags_csv \
         FROM media m JOIN sources s ON s.id=m.source_id \
         WHERE m.downloaded=1 AND s.included=1{scope_sql}"
    );

    struct Row {
        id:       i64,
        filepath: String,
        filename: String,
        kind:     String,
        rating:   i64,
        group_id: Option<i64>,
        tags_csv: Option<String>,
    }

    let rows: Vec<Row> = {
        let mut stmt = conn.prepare(&base_sql).map_err(db_err)?;
        stmt.query_map([], |r| Ok(Row {
            id:       r.get(0)?,
            filepath: r.get(1)?,
            filename: r.get(2)?,
            kind:     r.get(3)?,
            rating:   r.get(4)?,
            group_id: r.get(5)?,
            tags_csv: r.get(6)?,
        })).map_err(db_err)?
        .filter_map(|r| r.ok())
        .collect()
    };

    let require_tags: Vec<String> = q.tags.as_deref()
        .map(|s| s.split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).collect())
        .unwrap_or_default();

    let exclude_tags: Vec<String> = q.exclude_tags.as_deref()
        .map(|s| s.split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).collect())
        .unwrap_or_default();

    let min_rating = q.min_rating.unwrap_or(0);

    let mut items: Vec<Value> = rows.into_iter()
        .filter_map(|row| {
            // Type filter
            if let Some(ref k) = kind_filter {
                if &row.kind != k { return None; }
            }

            // Rating filter
            if min_rating > 0 && row.rating < min_rating { return None; }

            let own_tags: HashSet<String> = row.tags_csv.as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| s.split(',').map(|t| t.to_string()).collect())
                .unwrap_or_default();

            let inherited: HashSet<String> = row.group_id
                .and_then(|gid| group_effective_tags.get(&gid).cloned())
                .unwrap_or_default();

            let effective: HashSet<String> = own_tags.union(&inherited).cloned().collect();

            // Required tags (all must be present)
            for req in &require_tags {
                if !effective.contains(req.as_str()) { return None; }
            }

            // Excluded tags (any match drops the item)
            for excl in &exclude_tags {
                if effective.contains(excl.as_str()) { return None; }
            }

            let tags_vec: Vec<String> = effective.into_iter().collect();

            Some(json!({
                "id":       row.id,
                "filepath": row.filepath,
                "filename": row.filename,
                "type":     row.kind,
                "rating":   row.rating,
                "tags":     tags_vec,
                "url":      format!("/library/{}", urlencoding::encode(&row.filepath).replace("%2F", "/")),
            }))
        })
        .collect();

    if do_shuffle {
        let mut rng = rand::thread_rng();
        items.shuffle(&mut rng);
    }

    let total = items.len();
    if items.len() > limit { items.truncate(limit); }
    let count = items.len();

    Ok(Json(json!({
        "items":       items,
        "count":       count,
        "total_avail": total,
    })))
}

// ─── POST /api/ch/session ────────────────────────────────────────────────────
// Logs a completed Cock Hero session when ch_log_sessions is enabled.

#[derive(Deserialize)]
pub struct LogSessionBody {
    pub duration_s: i64,
    pub item_count: i64,
    pub filters:    Option<String>,
    pub notes:      Option<String>,
}

pub async fn log_session(
    State(state): State<Arc<AppState>>,
    Json(body):   Json<LogSessionBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let log_enabled = state.settings.read().await.ch_log_sessions;
    if !log_enabled {
        return Ok(Json(json!({ "logged": false })));
    }

    let conn = state.pool.get().map_err(db_err)?;
    conn.execute(
        "INSERT INTO ch_sessions (started_at, duration_s, item_count, filters, notes) \
         VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![now_iso(), body.duration_s, body.item_count, body.filters, body.notes],
    ).map_err(db_err)?;

    Ok(Json(json!({ "logged": true, "id": conn.last_insert_rowid() })))
}

// ─── GET /api/ch/sessions ────────────────────────────────────────────────────

pub async fn get_sessions(State(state): State<Arc<AppState>>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let mut stmt = conn.prepare(
        "SELECT id, started_at, duration_s, item_count, filters, notes \
         FROM ch_sessions ORDER BY started_at DESC LIMIT 100"
    ).map_err(db_err)?;

    let sessions: Vec<Value> = stmt.query_map([], |r| Ok(json!({
        "id":          r.get::<_,i64>(0)?,
        "started_at":  r.get::<_,String>(1)?,
        "duration_s":  r.get::<_,i64>(2)?,
        "item_count":  r.get::<_,i64>(3)?,
        "filters":     r.get::<_,Option<String>>(4)?,
        "notes":       r.get::<_,Option<String>>(5)?,
    }))).map_err(db_err)?
    .filter_map(|r| r.ok())
    .collect();

    Ok(Json(json!({ "sessions": sessions })))
}
