use std::{sync::Arc, sync::atomic::Ordering};
use axum::{extract::State, response::IntoResponse, Json};
use serde_json::json;

use crate::state::AppState;
use super::AppError;

pub async fn status(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let paused = state.downloads_paused.load(Ordering::SeqCst);
    let active: Vec<i64> = state.active_processes.lock().unwrap().keys().cloned().collect();
    Ok(Json(json!({"paused": paused, "active_sources": active})))
}

pub async fn pause(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    state.downloads_paused.store(true, Ordering::SeqCst);

    // Kill all in-flight gallery-dl processes immediately
    let pids: Vec<(i64, u32)> = state.active_processes.lock().unwrap()
        .iter().map(|(&sid, &pid)| (sid, pid)).collect();

    let mut paused_ids = state.paused_source_ids.lock().unwrap();
    for (sid, pid) in &pids {
        paused_ids.insert(*sid);
        kill_by_pid(*pid);
    }
    drop(paused_ids);

    tracing::info!("Downloads paused — terminated {} in-flight process(es)", pids.len());
    Ok(Json(json!({"status": "paused"})))
}

pub async fn resume(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    state.downloads_paused.store(false, Ordering::SeqCst);

    // Re-queue everything still at 'paused' status
    let ids: Vec<i64> = {
        let conn = state.pool.get().map_err(anyhow::Error::from)?;
        let mut stmt = conn.prepare("SELECT id FROM sources WHERE status='paused'")?;
        let ids: Vec<i64> = stmt.query_map([], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        ids
    };

    let count = ids.len();
    for id in ids {
        tokio::spawn(crate::downloader::run_download(Arc::clone(&state), id));
    }
    tracing::info!("Downloads resumed — re-queued {count} source(s)");
    Ok(Json(json!({"status": "resumed", "queued": count})))
}

fn kill_by_pid(pid: u32) {
    #[cfg(target_os = "windows")]
    { let _ = std::process::Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).spawn(); }
    #[cfg(not(target_os = "windows"))]
    { let _ = std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).spawn(); }
}
