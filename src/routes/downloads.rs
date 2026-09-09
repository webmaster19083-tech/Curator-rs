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
    let (queued_count, retrying_count) = state
        .pool
        .get()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT SUM(status='pending'), SUM(status='retrying') FROM sources",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    ))
                },
            )
            .ok()
        })
        .unwrap_or((0, 0));

    Json(json!({
        "paused":       paused,
        "active_count": active,
        "queued_count": queued_count,
        "retrying_count": retrying_count,
        "paused_source_ids": paused_ids,
    }))
}

// ─── POST /api/downloads/pause ───────────────────────────────────────────────

pub async fn pause(State(state): State<Arc<AppState>>) -> Json<Value> {
    let _control = state.download_control.lock().await;
    state.downloads_paused.store(true, Ordering::SeqCst);

    // Delayed retry timers have no child PID to kill.  Persist their paused
    // state now so a later Resume picks them up immediately rather than
    // waiting for a stale timer (or losing them across a restart).
    if let Ok(conn) = state.pool.get() {
        let _ = conn.execute(
            "UPDATE sources SET status='paused' WHERE status IN ('pending','retrying')",
            [],
        );
    }

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
    let _control = state.download_control.lock().await;
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
    // Claim paused rows before spawning. A second Resume call then sees zero
    // paused rows instead of reporting/requeueing the same work again.
    if !paused_ids.is_empty() {
        let conn = match state.pool.get() {
            Ok(c) => c,
            Err(_) => return Json(json!({"error":"Database unavailable"})),
        };
        let tx = match conn.unchecked_transaction() {
            Ok(tx) => tx,
            Err(_) => return Json(json!({"error":"Database unavailable"})),
        };
        for id in &paused_ids {
            if tx
                .execute(
                    "UPDATE sources SET status='pending' WHERE id=?1 AND status='paused'",
                    [id],
                )
                .is_err()
            {
                return Json(json!({"error":"Database unavailable"}));
            }
        }
        if tx.commit().is_err() {
            return Json(json!({"error":"Database unavailable"}));
        }
    }
    state.paused_source_ids.lock().await.clear();
    state.downloads_paused.store(false, Ordering::SeqCst);

    for id in &paused_ids {
        tokio::spawn(crate::downloader::run_download(Arc::clone(&state), *id));
    }

    Json(json!({ "paused": false, "requeued": paused_ids.len() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_resume_keeps_all_sources_paused() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(dir.path());
        crate::test_support::source(&state);
        state.downloads_paused.store(true, Ordering::SeqCst);
        let conn = state.pool.get().unwrap();
        conn.execute("UPDATE sources SET status='paused' WHERE id=1", [])
            .unwrap();
        conn.execute("INSERT INTO sources(id,name,url,slug,status,added_at) VALUES(2,'second','https://example.org/second','second','paused','now')", []).unwrap();
        conn.execute_batch("CREATE TRIGGER fail_second_resume BEFORE UPDATE OF status ON sources WHEN NEW.id=2 AND NEW.status='pending' BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
        let response = resume(State(state.clone())).await;
        assert!(response.0.get("error").is_some());
        let paused: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sources WHERE status='paused'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(paused, 2);
        assert!(state.downloads_paused.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn repeated_resume_claims_paused_source_once() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(dir.path());
        crate::test_support::source(&state);
        state
            .pool
            .get()
            .unwrap()
            .execute("UPDATE sources SET status='paused' WHERE id=1", [])
            .unwrap();

        let (a, b) = tokio::join!(resume(State(state.clone())), resume(State(state.clone())));
        let total = a.0["requeued"].as_u64().unwrap_or(0) + b.0["requeued"].as_u64().unwrap_or(0);
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn pause_converts_delayed_retries_to_resumable_work() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(dir.path());
        crate::test_support::source(&state);
        let conn = state.pool.get().unwrap();
        conn.execute(
            "UPDATE sources SET status='retrying',retry_attempts=2,retry_at=unixepoch()+600 WHERE id=1",
            [],
        )
        .unwrap();

        let response = pause(State(state.clone())).await.0;
        assert_eq!(response["paused"], true);
        let status: String = conn
            .query_row("SELECT status FROM sources WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(status, "paused");
    }
}
