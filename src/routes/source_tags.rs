//! API surface for reviewing source-owned metadata without losing it.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{provenance, AppState};

use super::media::db_err;

#[derive(Debug, Deserialize, Default)]
pub struct ReviewQuery {
    pub provider: Option<String>,
    pub limit: Option<u32>,
}

pub async fn review_list(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ReviewQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let tags = provenance::pending_source_tags(
        &conn,
        query.provider.as_deref(),
        query.limit.unwrap_or(50),
    )
    .map_err(db_err)?;
    Ok(Json(json!({"source_tags":tags,"tags":tags})))
}

#[derive(Debug, Deserialize)]
pub struct ReviewBody {
    pub id: i64,
    pub action: String,
    pub normalized_name: Option<String>,
    #[serde(default)]
    pub remember: bool,
    /// `provider` (the safe default) or `global`.
    pub scope: Option<String>,
}

pub async fn review(
    State(state): State<Arc<AppState>>,
    Json(body): Json<ReviewBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let reviewed = provenance::review_source_tag(
        &conn,
        body.id,
        &body.action,
        body.normalized_name.as_deref(),
    )
    .map_err(|error| {
        let status = if error.to_string().contains("already reviewed") {
            StatusCode::CONFLICT
        } else if error.to_string().contains("Query returned no rows") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        };
        (status, Json(json!({"error":error.to_string()})))
    })?;

    let rule = if body.remember {
        let action = match body.action.as_str() {
            "skip" => "skip",
            "edit" => "normalize",
            "add" => "add",
            _ => return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Review action must be add, edit, or skip"})))),
        };
        let scope = body.scope.as_deref().unwrap_or("provider");
        if !matches!(scope, "provider" | "global") {
            return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Rule scope must be provider or global"}))));
        }
        Some(
            provenance::upsert_rule(
                &conn,
                &provenance::RuleInput {
                    raw_name: reviewed.raw_name.clone(),
                    action: action.to_string(),
                    normalized_name: reviewed.normalized_name.clone(),
                    provider: (scope == "provider").then_some(reviewed.provider.clone()),
                },
            )
            .map_err(db_err)?,
        )
    } else {
        None
    };
    Ok(Json(json!({"source_tag":reviewed,"rule":rule})))
}

pub async fn list_rules(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    Ok(Json(json!({"rules":provenance::list_rules(&conn).map_err(db_err)?})))
}

pub async fn save_rule(
    State(state): State<Arc<AppState>>,
    Json(input): Json<provenance::RuleInput>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    Ok(Json(json!({"rule":provenance::upsert_rule(&conn, &input).map_err(db_err)?})))
}

pub async fn delete_rule(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    if !provenance::delete_rule(&conn, id).map_err(db_err)? {
        return Err((StatusCode::NOT_FOUND, Json(json!({"error":"Tag rule not found"}))));
    }
    Ok(Json(json!({"status":"deleted"})))
}
