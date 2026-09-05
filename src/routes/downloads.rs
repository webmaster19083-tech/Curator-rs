use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::AppState;

// ─── GET /api/downloads/status ───────────────────────────────────────────────

pub async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let paused = state.downloads_paused.load(Ordering::SeqCst);
    let active = state.active_processes.lock().await.len();
    let paused_ids: Vec<i64> = state
        .paused_source_ids
        .lock()
        .await
        .iter()
        .cloned()
        .collect();

    Json(json!({
        "paused":       paused,
        "active_count": active,
        "paused_source_ids": paused_ids,
    }))
}

// ─── POST /api/downloads/pause ───────────────────────────────────────────────

pub async fn pause(State(state): State<Arc<AppState>>) -> Json<Value> {
    state.downloads_paused.store(true, Ordering::SeqCst);

    // Kill all running gallery-dl processes and mark their sources as paused
    let procs: Vec<(i64, u32)> = {
        let guard = state.active_processes.lock().await;
        guard.iter().map(|(&sid, &pid)| (sid, pid)).collect()
    };

    for (source_id, pid) in procs {
        state.paused_source_ids.lock().await.insert(source_id);
        crate::downloader::kill_pid(pid).await;
    }

    Json(json!({ "paused": true }))
}

// ─── POST /api/downloads/resume ──────────────────────────────────────────────

pub async fn resume(State(state): State<Arc<AppState>>) -> Json<Value> {
    // Wait for killed children and their final index pass before requeueing.
    while state.downloads_paused.load(Ordering::SeqCst) {
        if state.running_sources.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let paused_ids: Vec<i64> = {
        let conn = match state.pool.get() {
            Ok(c) => c,
            Err(_) => return Json(json!({"error":"Database unavailable"})),
        };
        let mut stmt = match conn.prepare("SELECT id FROM sources WHERE status='paused'") {
            Ok(s) => s,
            Err(_) => return Json(json!({"error":"Database unavailable"})),
        };
        let ids = stmt
            .query_map([], |r| r.get(0))
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default();
        ids
    };
    state.paused_source_ids.lock().await.clear();
    state.downloads_paused.store(false, Ordering::SeqCst);

    for id in &paused_ids {
        tokio::spawn(crate::downloader::run_download(Arc::clone(&state), *id));
    }

    Json(json!({ "paused": false, "requeued": paused_ids.len() }))
}
