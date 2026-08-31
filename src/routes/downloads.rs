use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::AppState;

// ─── GET /api/downloads/status ───────────────────────────────────────────────

pub async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let paused  = state.downloads_paused.load(Ordering::SeqCst);
    let active  = state.active_processes.lock().await.len();
    let paused_ids: Vec<i64> = state.paused_source_ids.lock().await.iter().cloned().collect();

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
    state.downloads_paused.store(false, Ordering::SeqCst);

    // Re-queue sources that were paused mid-download
    let paused_ids: Vec<i64> = {
        let mut guard = state.paused_source_ids.lock().await;
        let ids: Vec<i64> = guard.iter().cloned().collect();
        guard.clear();
        ids
    };

    for id in &paused_ids {
        tokio::spawn(crate::downloader::run_download(Arc::clone(&state), *id));
    }

    Json(json!({ "paused": false, "requeued": paused_ids.len() }))
}
