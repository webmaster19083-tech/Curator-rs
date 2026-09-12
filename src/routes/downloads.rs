use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::{extract::{Path, State}, Json};
use serde_json::{json, Value};

use crate::AppState;

// ─── GET /api/downloads/status ───────────────────────────────────────────────

pub async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let paused = state.downloads_paused.load(Ordering::SeqCst);
    let active_ids: HashSet<i64> = state.active_processes.lock().await.keys().copied().collect();
    let active = active_ids.len();
    let paused_ids: Vec<i64> = state
        .paused_source_ids
        .lock()
        .await
        .iter()
        .cloned()
        .collect();
    let mut sources = state.pool.get().ok().and_then(|conn| {
        let mut statement = conn.prepare(
            "SELECT s.id,s.name,s.status,s.item_count,s.known_total,
                    (SELECT COUNT(*) FROM media m WHERE m.source_id=s.id AND m.downloaded=1 AND m.missing=0) AS indexed_count,
                    s.completed_count,s.current_filename,s.retry_at,s.error_message,s.queued_at,s.started_at,s.completed_at,s.progress_updated_at
             FROM sources s ORDER BY COALESCE(s.queued_at,s.added_at),s.id",
        ).ok()?;
        let mapped = statement.query_map([], |row| Ok((
            row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?, row.get::<_, Option<i64>>(4)?, row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?, row.get::<_, Option<String>>(7)?, row.get::<_, i64>(8)?,
            row.get::<_, Option<String>>(9)?, row.get::<_, Option<String>>(10)?, row.get::<_, Option<String>>(11)?,
            row.get::<_, Option<String>>(12)?, row.get::<_, Option<String>>(13)?,
        ))).ok()?;
        let rows = mapped.collect::<rusqlite::Result<Vec<_>>>().ok()?;
        Some(rows)
    }).unwrap_or_default();
    let mut queue_position = 0_i64;
    let source_rows = sources.drain(..).map(|row| {
        let (id,name,status,item_count,known_total,indexed_count,persisted_completed,current_filename,retry_at,error,queued_at,started_at,completed_at,updated_at) = row;
        let phase = match status.as_str() {
            "pending" => "queued",
            "downloading" if active_ids.contains(&id) => "active",
            "downloading" => "queued",
            "indexing" => "indexing",
            "retrying" => "retrying",
            "paused" => "paused",
            "done" => "completed",
            "error" => "failed",
            _ => "queued",
        };
        let position = if phase == "queued" { queue_position += 1; Some(queue_position) } else { None };
        let completed = indexed_count.max(persisted_completed).max(item_count.min(indexed_count));
        let percentage = known_total.map(|total| {
            if total == 0 { if phase == "completed" { 100.0 } else { 0.0 } }
            else { ((completed as f64 / total as f64) * 100.0).clamp(0.0, 100.0) }
        });
        json!({
            "id":id,"name":name,"status":status,"phase":phase,"queue_position":position,
            "known_total":known_total,"completed_count":completed,"percentage":percentage,
            "indeterminate":known_total.is_none(),"current_filename":current_filename,
            "retry_at":if retry_at>0 { Some(retry_at) } else { None },"error":error,
            "queued_at":queued_at,"started_at":started_at,"completed_at":completed_at,"updated_at":updated_at,
        })
    }).collect::<Vec<_>>();
    let queued_count = source_rows.iter().filter(|row| row["phase"] == "queued").count() as i64;
    let retrying_count = source_rows.iter().filter(|row| row["phase"] == "retrying").count() as i64;

    Json(json!({
        "paused":       paused,
        "active_count": active,
        "queued_count": queued_count,
        "retrying_count": retrying_count,
        "paused_source_ids": paused_ids,
        "sources": source_rows,
    }))
}

// ─── POST /api/downloads/pause ───────────────────────────────────────────────

pub async fn pause(State(state): State<Arc<AppState>>) -> Json<Value> {
    let _control = state.download_control.lock().await;
    state.downloads_paused.store(true, Ordering::SeqCst);

    // Delayed retry timers have no child PID to kill. Persist their paused
    // state now so Resume can claim them immediately and a restart cannot
    // lose them.
    if let Ok(conn) = state.pool.get() {
        let _ = conn.execute(
            "UPDATE sources SET status='paused',progress_updated_at=?1 WHERE status IN ('pending','retrying')",
            [crate::db::now_iso()],
        );
    }

    // Kill all running gallery-dl processes and mark their sources as paused
    let procs: Vec<(i64, u32)> = {
        let guard = state.active_processes.lock().await;
        guard.iter().map(|(&sid, &pid)| (sid, pid)).collect()
    };

    for &(source_id, pid) in &procs {
        state.paused_source_ids.lock().await.insert(source_id);
        // The owning task also has to leave its select loop.  Killing a PID
        // alone is not sufficient on Windows when a managed environment
        // rejects taskkill's tree walk, and it cannot wake a queued source.
        if let Some(cancel) = state
            .source_cancellations
            .lock()
            .await
            .get(&source_id)
            .cloned()
        {
            cancel.cancel();
        }
        crate::downloader::kill_pid(pid).await;
    }

    // Local-folder imports and queued workers do not have a gallery-dl PID,
    // but they still register a source cancellation token.  Cancel those
    // tokens as part of the same global transition so pause has one meaning
    // for every source type.
    let cancellations: Vec<(i64, tokio_util::sync::CancellationToken)> = state
        .source_cancellations
        .lock()
        .await
        .iter()
        .filter(|(source_id, _)| !procs.iter().any(|(id, _)| id == *source_id))
        .map(|(source_id, token)| (*source_id, token.clone()))
        .collect();
    for (source_id, cancel) in cancellations {
        state.paused_source_ids.lock().await.insert(source_id);
        cancel.cancel();
        if let Ok(conn) = state.pool.get() {
            let _ = conn.execute(
                "UPDATE sources SET status='paused',progress_updated_at=?1 WHERE id=?2 AND status IN ('downloading','indexing')",
                rusqlite::params![crate::db::now_iso(), source_id],
            );
        }
    }

    Json(json!({ "paused": true }))
}

/// Pause one source without stopping unrelated work.  The owning downloader
/// task reaps its process before a later Resume requeues it.
pub async fn pause_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let _control = state.download_control.lock().await;
    let exists = state.pool.get().ok().and_then(|conn| conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sources WHERE id=?1)", [id], |row| row.get::<_, bool>(0)
    ).ok()).unwrap_or(false);
    if !exists { return Json(json!({"error":"Source not found"})); }
    state.paused_source_ids.lock().await.insert(id);
    if let Some(cancel) = state.source_cancellations.lock().await.get(&id).cloned() { cancel.cancel(); }
    if let Some(pid) = state.active_processes.lock().await.get(&id).copied() { crate::downloader::kill_pid(pid).await; }
    if let Ok(conn) = state.pool.get() {
        let _ = conn.execute("UPDATE sources SET status='paused',progress_updated_at=?1 WHERE id=?2", rusqlite::params![crate::db::now_iso(), id]);
    }
    Json(json!({"id":id,"paused":true}))
}

/// Resume only one paused source.  Global pause still wins so this endpoint
/// cannot accidentally restart downloads behind the user's back.
pub async fn resume_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Json<Value> {
    // Serialize a source-level resume with global pause/resume.  Without the
    // same transition lock, a double-click or a simultaneous global resume
    // could both claim the row and enqueue duplicate downloader tasks.
    let _control = state.download_control.lock().await;
    if state.downloads_paused.load(Ordering::SeqCst) { return Json(json!({"id":id,"error":"Downloads are globally paused"})); }
    let changed = state.pool.get().ok().and_then(|conn| conn.execute(
        "UPDATE sources SET status='pending',queued_at=?1,progress_updated_at=?1,current_filename=NULL WHERE id=?2 AND status IN ('paused','error','done','retrying')",
        rusqlite::params![crate::db::now_iso(),id],
    ).ok()).unwrap_or(0);
    if changed == 0 { return Json(json!({"id":id,"error":"Source is not resumable"})); }
    state.paused_source_ids.lock().await.remove(&id);
    state
        .download_tasks
        .spawn(crate::downloader::run_download(Arc::clone(&state), id));
    Json(json!({"id":id,"paused":false,"status":"queued"}))
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
    // paused rows instead of launching duplicate downloads.
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
                    "UPDATE sources SET status='pending',queued_at=?1,progress_updated_at=?1,current_filename=NULL WHERE id=?2 AND status='paused'",
                    rusqlite::params![crate::db::now_iso(), id],
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
        state
            .download_tasks
            .spawn(crate::downloader::run_download(Arc::clone(&state), *id));
    }

    Json(json!({ "paused": false, "requeued": paused_ids.len() }))
}
