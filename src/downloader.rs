use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use futures::{Stream, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncReadExt, BufReader};

use tracing::{info, warn};

use crate::db::now_iso;
use crate::slug::pending_filepath;
use crate::AppState;

// ─── Extension sets ──────────────────────────────────────────────────────────

pub fn image_exts() -> &'static [&'static str] {
    &[
        "jpg", "jpeg", "png", "gif", "webp", "bmp", "jfif", "avif", "tiff",
    ]
}

pub fn video_exts() -> &'static [&'static str] {
    &["mp4", "webm", "mov", "avi", "mkv", "m4v"]
}

fn is_image_path(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    image_exts().contains(&ext.as_str())
}

fn is_video_path(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    video_exts().contains(&ext.as_str())
}

// ─── gallery-dl stderr filter ────────────────────────────────────────────────

pub fn filter_gdl_stderr(raw: &str) -> String {
    raw.lines()
        .filter(|l| !l.contains("RequestsDependencyWarning"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ─── Kill a process by PID ───────────────────────────────────────────────────
//
// Windows taskkill must include /T; killing only the parent leaves ffmpeg children alive.
pub async fn kill_pid(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(target_os = "windows")]
    {
        match crate::process::command("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output()
            .await
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => warn!(
                "Process-tree termination for PID {pid} returned {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(e) => warn!("Could not terminate process tree for PID {pid}: {e}"),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
}

// ─── spawn_gallery_dl — shared subprocess-streaming primitive ────────────────
//
// Both `populate_placeholders` and `/api/preview/scan` (live browse) go
// through this single entry point, matching the Tier 1 contract: one place
// that spawns gallery-dl, registers/deregisters its PID, and drains stderr
// concurrently with stdout so a chatty extractor can't deadlock the pipe
// (stdout fills → gallery-dl blocks writing → if we're not *also* reading
// stderr at the same time and its OS pipe buffer fills, gallery-dl blocks
// on that too, and everything hangs forever).
//
// A note on the stream shape: `gallery-dl -j` does not emit newline-delimited
// JSON the way some other gallery-dl invocations do — it collects every
// discovered item internally and prints the whole thing as a single JSON
// array only once, at the end of the run. There is no way to get
// item-by-item output any earlier than that without patching gallery-dl
// itself. So this function reads stdout to completion, parses it as one
// JSON value, and yields the top-level array's elements one at a time —
// each element is one `[type, url, metadata]` entry, exactly the shape
// `preview_walk` already expects. This still gives real value on the SSE
// side (see routes/preview.rs): the HTTP connection stays open with
// keep-alive pings while gallery-dl runs, and once the result lands, items
// stream to the client one event at a time instead of one giant response
// the browser has to deserialize all at once.
pub fn spawn_gallery_dl(
    args: Vec<String>,
    source_id: Option<i64>,
    state: Arc<AppState>,
) -> impl Stream<Item = Result<Value>> {
    async_stream::stream! {
        let mut cmd = crate::process::command(&state.gallery_dl_bin);
        cmd.args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()).kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                yield Err(anyhow::anyhow!(
                    "gallery-dl could not be launched: {}. Is it on your PATH?", e
                ));
                return;
            }
        };

        let pid = child.id();
        if let (Some(id), Some(pid)) = (source_id, pid) {
            state.active_processes.lock().await.insert(id, pid);
        }

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        // Drain stderr concurrently on its own task so it can never back up
        // and block the process while we're reading stdout.
        let stderr_handle = tokio::spawn(async move {
            drain_tail(stderr).await
        });

        let output_task = tokio::spawn(async move {
            let mut raw=String::new();
            let result=BufReader::new(stdout).take(16*1024*1024+1).read_to_string(&mut raw).await;
            (raw,result)
        });
        let stopped = tokio::select! {
            _ = state.shutdown.cancelled() => true,
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => true,
            _ = child.wait() => false,
        };
        if stopped {
            if let Some(pid)=child.id() { kill_pid(pid).await; }
            let _=child.kill().await;
            let _=child.wait().await;
        }
        let (raw,read_result) = output_task.await.unwrap_or_else(|_| (String::new(), Ok(0)));
        // Deregister immediately after the child has stopped, before any error return.
        // Timeout/cancellation used to return above the old cleanup path and leave a
        // stale source -> PID entry forever.
        if let Some(id) = source_id {
            state.active_processes.lock().await.remove(&id);
        }
        if stopped || raw.len()>16*1024*1024 {
            let _=stderr_handle.await;
            yield Err(anyhow::anyhow!("Listing stopped (30s deadline, cancellation, or 16 MiB output limit)"));
            return;
        }
        if let Err(e) = read_result {
            let _=stderr_handle.await;
            yield Err(anyhow::anyhow!("Could not read gallery-dl output: {e}"));
            return;
        }

        let stderr_text = stderr_handle.await.unwrap_or_default();
        let raw = raw.trim();

        if raw.is_empty() {
            let err = filter_gdl_stderr(&stderr_text);
            let err = if err.is_empty() {
                "gallery-dl returned nothing. Is the URL supported / does the extractor need login cookies?".to_string()
            } else {
                let tail: String = err.chars().rev().take(1500).collect::<String>().chars().rev().collect();
                tail
            };
            yield Err(anyhow::anyhow!(err));
            return;
        }

        match serde_json::from_str::<Value>(raw) {
            Ok(Value::Array(items)) => {
                for item in items {
                    yield Ok(item);
                }
            }
            Ok(other) => {
                yield Ok(other);
            }
            Err(e) => {
                yield Err(anyhow::anyhow!("Could not parse gallery-dl output: {}", e));
            }
        }
    }
}

// ─── Preview walk — extract file entries from gallery-dl -j output ───────────

#[derive(serde::Serialize, Debug, Clone)]
pub struct PreviewItem {
    pub url: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub creator: String,
    pub title: String,
    pub poster: Option<String>,
    pub source: String,
}

pub fn preview_ext_from(meta: &Value, url: &str) -> String {
    if let Some(ext) = meta.get("extension").and_then(|v| v.as_str()) {
        if !ext.is_empty() {
            return ext.to_lowercase();
        }
    }
    let tail = url.split('?').next().unwrap_or(url);
    let tail = tail.rsplit('/').next().unwrap_or("");
    if tail.contains('.') {
        tail.rsplit('.').next().unwrap_or("").to_lowercase()
    } else {
        String::new()
    }
}

pub fn preview_walk(
    node: &Value,
    source_url: &str,
    results: &mut Vec<PreviewItem>,
    seen: &mut std::collections::HashSet<String>,
) {
    let arr = match node.as_array() {
        Some(a) => a,
        None => return,
    };

    // gallery-dl -j produces arrays: [type, url, metadata_dict]
    if arr.len() >= 2 {
        if let Some(url_str) = arr[1].as_str() {
            if url_str.starts_with("http") {
                let meta = arr.last().and_then(|v| v.as_object());
                let meta_val = arr.last().cloned().unwrap_or(Value::Null);
                let ext = preview_ext_from(&meta_val, url_str);

                let kind = if image_exts().contains(&ext.as_str()) {
                    Some("image")
                } else if video_exts().contains(&ext.as_str()) {
                    Some("video")
                } else {
                    None
                };

                if let Some(kind) = kind {
                    if !seen.contains(url_str) {
                        seen.insert(url_str.to_string());

                        let creator = meta
                            .and_then(|m| {
                                m.get("username")
                                    .or_else(|| m.get("author"))
                                    .or_else(|| m.get("user"))
                                    .or_else(|| m.get("artist"))
                            })
                            .and_then(|v| v.as_str())
                            .unwrap_or(source_url)
                            .to_string();

                        let title = meta
                            .and_then(|m| {
                                m.get("title")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                                    .or_else(|| m.get("id").map(|v| v.to_string()))
                            })
                            .unwrap_or_default();

                        let poster = meta
                            .and_then(|m| m.get("thumbnail").or_else(|| m.get("preview")))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        results.push(PreviewItem {
                            url: url_str.to_string(),
                            kind: kind.to_string(),
                            creator,
                            title,
                            poster,
                            source: source_url.to_string(),
                        });
                    }
                }
            }
        }
    }

    // Recurse into nested arrays
    for item in arr {
        if item.is_array() {
            preview_walk(item, source_url, results, seen);
        }
    }
}

// ─── collect_gallery_dl_items ─────────────────────────────────────────────────
// Non-streaming convenience wrapper over spawn_gallery_dl for callers (namely
// populate_placeholders) that just want the final Vec<PreviewItem> with a
// timeout, and don't care about incremental delivery the way the SSE preview
// endpoint does.

pub async fn collect_gallery_dl_items(
    url: &str,
    state: Arc<AppState>,
) -> std::result::Result<Vec<PreviewItem>, String> {
    let args = vec![
        "-j".into(),
        "--no-download".into(),
        "--range".into(),
        "1-1000".into(),
        url.to_string(),
    ];
    let url_owned = url.to_string();

    let collect_fut = async move {
        let stream = spawn_gallery_dl(args, None, state);
        futures::pin_mut!(stream);

        let mut items: Vec<PreviewItem> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut first_err: Option<String> = None;

        while let Some(node_result) = stream.next().await {
            match node_result {
                Ok(node) => preview_walk(&node, &url_owned, &mut items, &mut seen),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e.to_string());
                    }
                }
            }
        }

        (items, first_err)
    };

    let (items, first_err) = collect_fut.await;
    if items.is_empty() {
        if let Some(e) = first_err {
            return Err(e);
        }
    }
    Ok(items)
}

// ─── scan_and_index ───────────────────────────────────────────────────────────
/// Probe on a blocking thread with a five-second deadline. Indexing itself does
/// not probe; the bounded duration backfill persists the result or failure.
pub(crate) fn probe_video_duration(ffprobe_bin: &str, path: &Path) -> Option<f64> {
    if !path.is_file() {
        return None;
    }
    let output = crate::process::output_timeout(
        std::process::Command::new(ffprobe_bin)
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(path),
        std::time::Duration::from_secs(5),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|d| d.is_finite() && *d >= 0.0)
}

pub fn index_file(state: &AppState, source_id: i64, path: &Path) -> Result<bool> {
    use rusqlite::OptionalExtension;
    let path = dunce::simplified(path);
    if !path.is_file() || (!is_image_path(path) && !is_video_path(path)) {
        return Ok(false);
    }
    // gallery-dl writes .part files and atomically renames them on completion.
    let mut part = path.as_os_str().to_os_string();
    part.push(".part");
    if Path::new(&part).exists() {
        return Ok(false);
    }
    let rel = path
        .strip_prefix(dunce::simplified(&state.library_dir))?
        .to_string_lossy()
        .replace('\\', "/");
    let stamp = crate::media_files::stamp(path);
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(".json");
    let origin: Option<String> = std::fs::read_to_string(Path::new(&sidecar))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("url").and_then(Value::as_str).map(str::to_owned));
    let conn = state.pool.get()?;
    let existing: Option<(i64, Option<String>, Option<String>, bool)> = conn
        .query_row(
            "SELECT id, file_stamp, origin_url, downloaded FROM media WHERE filepath=?1",
            [&rel],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if existing
        .as_ref()
        .is_some_and(|r| r.1 == stamp && r.3 && (origin.is_none() || r.2 == origin))
    {
        return Ok(false);
    }
    let tx = conn.unchecked_transaction()?;
    // A metadata event can follow the file event. Merge an early real row into
    // its placeholder, retaining the placeholder ID, ratings and both tag sets.
    if let (Some((real_id, _, _, _)), Some(url)) = (&existing, &origin) {
        let placeholder: Option<i64> = tx
            .query_row(
                "SELECT id FROM media WHERE source_id=?1 AND origin_url=?2 AND id<>?3",
                rusqlite::params![source_id, url, real_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = placeholder {
            tx.execute("INSERT OR IGNORE INTO media_tags SELECT ?1, tag_id FROM media_tags WHERE media_id=?2", rusqlite::params![id,real_id])?;
            tx.execute("UPDATE media SET (rating,rating_source,rating_reviewed,rating_reviewed_at)=
                (SELECT rating,rating_source,rating_reviewed,rating_reviewed_at FROM media WHERE id=?2)
                WHERE id=?1 AND rating_reviewed=0 AND (rating=0 OR (SELECT rating_reviewed FROM media WHERE id=?2)=1)", rusqlite::params![id,real_id])?;
            tx.execute(
                "UPDATE media SET (auto_rating,auto_rating_score)=
                (SELECT auto_rating,auto_rating_score FROM media WHERE id=?2)
                WHERE id=?1 AND auto_rating=0",
                rusqlite::params![id, real_id],
            )?;
            tx.execute("DELETE FROM media WHERE id=?1", [real_id])?;
        }
    }
    let kind = if is_video_path(path) {
        "video"
    } else {
        "image"
    };
    tx.execute("INSERT INTO media(source_id,filepath,filename,type,added_at,origin_url,downloaded,file_stamp)
        VALUES(?1,?2,?3,?4,?5,?6,1,?7)
        ON CONFLICT(source_id,origin_url) WHERE origin_url IS NOT NULL DO UPDATE SET
          filepath=excluded.filepath, filename=excluded.filename, downloaded=1, missing=0,
          file_stamp=excluded.file_stamp, nsfw_state='pending', nsfw_attempts=0, nsfw_retry_at=0, duration_attempted=0,
          duration_secs=CASE WHEN media.file_stamp=excluded.file_stamp THEN media.duration_secs ELSE NULL END
        ON CONFLICT(filepath) DO UPDATE SET downloaded=1, missing=0, file_stamp=excluded.file_stamp,
          origin_url=COALESCE(excluded.origin_url,media.origin_url), nsfw_state='pending', nsfw_attempts=0,
          nsfw_retry_at=0, duration_attempted=0, duration_secs=CASE WHEN media.file_stamp=excluded.file_stamp THEN media.duration_secs ELSE NULL END",
        rusqlite::params![source_id,rel,path.file_name().unwrap_or_default().to_string_lossy(),kind,now_iso(),origin,stamp])?;
    tx.execute(
        "UPDATE media SET file_size_bytes=?1 WHERE filepath=?2",
        rusqlite::params![crate::media_files::file_size(path), rel],
    )?;
    tx.commit()?;
    // Keep sidecars: restart recovery and late metadata events need their URL.
    Ok(true)
}

pub fn scan_and_index(state: &AppState, source_id: i64, dest: &Path) -> Result<(i64, i64)> {
    let mut added = 0;
    for entry in walkdir::WalkDir::new(dest).into_iter() {
        if state.shutdown.is_cancelled() {
            break;
        }
        let entry = entry?;
        if entry.file_type().is_file() && index_file(state, source_id, entry.path())? {
            added += 1;
        }
    }
    let conn = state.pool.get()?;
    let total = conn.query_row(
        "SELECT COUNT(*) FROM media WHERE source_id=?1 AND downloaded=1",
        [source_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "UPDATE sources SET item_count=?1 WHERE id=?2",
        rusqlite::params![total, source_id],
    )?;
    Ok((total, added))
}

/// A single consumer serializes event batches, recovery scans and the final scan.
async fn index_download(
    state: Arc<AppState>,
    source_id: i64,
    dest: PathBuf,
    done: tokio_util::sync::CancellationToken,
) {
    use notify::Watcher;
    use std::sync::atomic::{AtomicBool, Ordering};
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PathBuf>(1024);
    let dirty = Arc::new(AtomicBool::new(false));
    let dirty_event = dirty.clone();
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(e) => {
                if e.need_rescan() {
                    dirty_event.store(true, Ordering::Relaxed);
                }
                if matches!(e.kind, notify::EventKind::Access(_)) {
                    return;
                }
                for path in e.paths {
                    if tx.try_send(path).is_err() {
                        dirty_event.store(true, Ordering::Relaxed);
                    }
                }
            }
            Err(_) => dirty_event.store(true, Ordering::Relaxed),
        })
        .and_then(|mut w| {
            w.watch(&dest, notify::RecursiveMode::Recursive)?;
            Ok(w)
        })
        .ok();
    if watcher.is_none() {
        warn!(
            "File notifications unavailable for source {source_id}; using 5-minute recovery scans"
        );
    }
    let mut recovery = tokio::time::interval(std::time::Duration::from_secs(300));
    loop {
        let mut paths = std::collections::HashSet::new();
        let final_scan;
        let scan;
        tokio::select! {
            _ = done.cancelled() => { final_scan=true; scan=true; }
            _ = recovery.tick() => { final_scan=false; scan=true; }
            path = rx.recv(), if watcher.is_some() => {
                final_scan=false; scan=false;
                if let Some(p)=path { paths.insert(p); }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                for _ in 0..1024 { if let Ok(p)=rx.try_recv() { paths.insert(p); } else { break; } }
            }
        }
        // Overflow recovers on the next scheduled scan, never a continuous scan storm.
        if scan && dirty.swap(false, Ordering::Relaxed) {
            info!("Recovering missed filesystem events for source {source_id}");
        }
        let s = state.clone();
        let d = dest.clone();
        let result=tokio::task::spawn_blocking(move || -> Result<()> {
            if scan { scan_and_index(&s,source_id,&d)?; }
            else {
                for mut path in paths {
                    if path.extension().is_some_and(|e| e=="json") { path.set_extension(""); }
                    if path.is_file() { index_file(&s,source_id,&path)?; }
                    else if let Ok(rel)=path.strip_prefix(&s.library_dir) {
                        let conn=s.pool.get()?;
                        conn.execute("UPDATE media SET missing=1,downloaded=0,nsfw_state='missing' WHERE filepath=?1 AND downloaded=1",[rel.to_string_lossy().replace('\\',"/")])?;
                    }
                }
            }
            Ok(())
        }).await;
        if let Ok(Err(e)) = result {
            warn!("Indexing source {source_id} failed: {e}");
        }
        if scan {
            // Even a scan that takes longer than the interval must leave a quiet gap.
            recovery.reset_after(std::time::Duration::from_secs(300));
        }
        if final_scan {
            watcher.take();
            break;
        }
    }
}

// ─── populate_placeholders ───────────────────────────────────────────────────
//
// Pre-scans a source via `gallery-dl -j` and inserts downloaded=0 placeholder
// rows so the UI can show "coming soon" tiles before the real download
// finishes. Deliberately stays on plain INSERT OR IGNORE rather than the
// ON CONFLICT ... DO UPDATE upsert scan_and_index uses: if a real download
// already retired this origin_url into a downloaded=1 row, a placeholder
// re-insert for the same (source_id, origin_url) must be silently dropped,
// never regress that row back to downloaded=0. INSERT OR IGNORE guarantees
// that unconditionally, for any conflict, without needing to reason about
// which specific column changed.

pub async fn populate_placeholders(state: Arc<AppState>, source_id: i64) {
    let (url, status) = {
        let conn = match state.pool.get() {
            Ok(c) => c,
            Err(_) => return,
        };
        match conn.query_row(
            "SELECT url, status FROM sources WHERE id=?1",
            [source_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        ) {
            Ok(v) => v,
            Err(_) => return,
        }
    };

    if status == "done" {
        return;
    }

    // Guard: if any real file predates origin_url tracking, skip to avoid duplicates
    let unmatched_real: i64 = {
        let conn = match state.pool.get() {
            Ok(c) => c,
            Err(_) => return,
        };
        conn.query_row(
            "SELECT COUNT(*) FROM media WHERE source_id=?1 AND downloaded=1 AND origin_url IS NULL",
            [source_id],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    if unmatched_real > 0 {
        return;
    }

    // Rate-limit placeholder scans (3 concurrent max)
    let _permit = tokio::select! {
        _=state.shutdown.cancelled()=>return,
        p=state.placeholder_semaphore.acquire()=>match p {Ok(p)=>p,Err(_)=>return},
    };

    if state.shutdown.is_cancelled() {
        return;
    }
    let claimed = {
        let conn = match state.pool.get() {
            Ok(c) => c,
            Err(_) => return,
        };
        conn.execute(
            "INSERT INTO placeholder_scans(source_id,url,retry_at) VALUES(?1,?2,unixepoch()+21600)
          ON CONFLICT(source_id) DO UPDATE SET url=excluded.url,retry_at=excluded.retry_at
          WHERE placeholder_scans.url<>excluded.url OR placeholder_scans.retry_at<=unixepoch()",
            rusqlite::params![source_id, url],
        )
        .unwrap_or(0)
            > 0
    };
    if !claimed {
        return;
    }
    let items = match collect_gallery_dl_items(&url, Arc::clone(&state)).await {
        Ok(items) => items,
        Err(err) => {
            info!(
                "Placeholder pre-scan skipped for source {}: {}",
                source_id, err
            );
            return;
        }
    };
    if items.is_empty() {
        return;
    }

    let conn = match state.pool.get() {
        Ok(c) => c,
        Err(_) => return,
    };
    let now = now_iso();

    let _ = conn.execute("BEGIN", []);
    {
        let mut stmt = match conn.prepare(
            "INSERT OR IGNORE INTO media (source_id, filepath, filename, type, added_at, origin_url, downloaded)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)"
        ) { Ok(s) => s, Err(_) => { let _ = conn.execute("ROLLBACK", []); return; } };

        for item in &items {
            let fname = item
                .url
                .split('?')
                .next()
                .unwrap_or(&item.url)
                .rsplit('/')
                .next()
                .unwrap_or("item")
                .to_string();
            let fname = if fname.is_empty() {
                "item".to_string()
            } else {
                fname
            };
            let fp = pending_filepath(source_id, &item.url);
            let _ = stmt.execute(rusqlite::params![
                source_id, fp, fname, item.kind, now, item.url
            ]);
        }
    }
    let _ = conn.execute("COMMIT", []);

    info!(
        "Placeholder pre-scan for source {}: {} candidate item(s)",
        source_id,
        items.len()
    );
}

// ─── run_download ─────────────────────────────────────────────────────────────

pub fn run_download(
    state: Arc<AppState>,
    source_id: i64,
) -> impl std::future::Future<Output = ()> + Send {
    let token = state.download_tasks.token();
    async move {
        let _token = token;
        if state.shutdown.is_cancelled() || !state.running_sources.lock().await.insert(source_id) {
            return;
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        state
            .source_cancellations
            .lock()
            .await
            .insert(source_id, cancel.clone());
        run_download_impl(state.clone(), source_id, cancel).await;
        state.source_cancellations.lock().await.remove(&source_id);
        state.running_sources.lock().await.remove(&source_id);
    }
}

async fn run_download_impl(
    state: Arc<AppState>,
    source_id: i64,
    cancel: tokio_util::sync::CancellationToken,
) {
    // Fire placeholder scan concurrently — never gates the real download
    let state2 = Arc::clone(&state);
    state
        .download_tasks
        .spawn(async move { populate_placeholders(state2, source_id).await });

    if state
        .downloads_paused
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        if let Ok(conn) = state.pool.get() {
            let _ = conn.execute(
                "UPDATE sources SET status='paused' WHERE id=?1",
                [source_id],
            );
        }
        return;
    }

    // Claim status NOW (before the semaphore wait) to avoid concurrent duplicate downloads
    {
        if let Ok(conn) = state.pool.get() {
            let _ = conn.execute(
                "UPDATE sources SET status='downloading', error_message=NULL WHERE id=?1",
                [source_id],
            );
        } else {
            warn!("Source {source_id} could not claim downloading status: database unavailable");
            return;
        }
    }

    let sem = {
        let guard = state.download_semaphore.lock().await;
        Arc::clone(&*guard)
    };
    let _permit = tokio::select! {
        _ = state.shutdown.cancelled() => {
            if let Ok(conn)=state.pool.get() { let _=conn.execute("UPDATE sources SET status='paused' WHERE id=?1",[source_id]); }
            return;
        }
        _ = cancel.cancelled() => return,
        permit = sem.acquire() => match permit { Ok(p)=>p,Err(_)=>return },
    };

    if state
        .downloads_paused
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        if let Ok(conn) = state.pool.get() {
            let _ = conn.execute(
                "UPDATE sources SET status='paused' WHERE id=?1",
                [source_id],
            );
        }
        return;
    }

    run_download_inner(Arc::clone(&state), source_id, cancel).await;
}

async fn run_download_inner(
    state: Arc<AppState>,
    source_id: i64,
    cancel: tokio_util::sync::CancellationToken,
) {
    let (url, slug, name) = {
        let conn = match state.pool.get() {
            Ok(c) => c,
            Err(_) => return,
        };
        match conn.query_row(
            "SELECT url, slug, name FROM sources WHERE id=?1",
            [source_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        ) {
            Ok(v) => v,
            Err(_) => return,
        }
    };

    if let Some(folder) = url.strip_prefix("local:") {
        let folder = PathBuf::from(folder);
        let local_state = state.clone();
        let _ = tokio::task::spawn_blocking(move || {
            crate::local_import::sync_folder(&local_state, source_id, &folder)
        })
        .await;
        return;
    }
    let dest = dunce::simplified(&state.library_dir.join(&slug)).to_path_buf();
    let archive_path =
        dunce::simplified(&state.archives_dir.join(format!("{}.sqlite3", slug))).to_path_buf();
    let _ = std::fs::create_dir_all(&dest);

    let initial_count: i64 = state
        .pool
        .get()
        .ok()
        .and_then(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM media WHERE source_id=?1 AND downloaded=1",
                [source_id],
                |r| r.get(0),
            )
            .ok()
        })
        .unwrap_or(0);
    info!("Starting sync for source {} ({}): {}", source_id, name, url);

    let args = vec![
        url.clone(),
        "-D".into(),
        dest.to_string_lossy().to_string(),
        "--download-archive".into(),
        archive_path.to_string_lossy().to_string(),
        "--write-metadata".into(),
    ];

    let mut child = match crate::process::command(&state.gallery_dl_bin)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let msg = format!(
                "gallery-dl could not be launched: {}. Is it on your PATH?",
                e
            );
            if let Ok(conn) = state.pool.get() {
                let _ = conn.execute(
                    "UPDATE sources SET status='error', error_message=?1, synced_at=?2 WHERE id=?3",
                    rusqlite::params![msg, now_iso(), source_id],
                );
            } else {
                warn!("Source {source_id} failed to launch gallery-dl and database was unavailable: {msg}");
            }
            return;
        }
    };

    if let Some(pid) = child.id() {
        state.active_processes.lock().await.insert(source_id, pid);
    }
    if state
        .downloads_paused
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        state.paused_source_ids.lock().await.insert(source_id);
        if let Some(pid) = child.id() {
            kill_pid(pid).await;
        }
    }
    let index_done = tokio_util::sync::CancellationToken::new();
    let idx_task = tokio::spawn(index_download(
        state.clone(),
        source_id,
        dest.clone(),
        index_done.clone(),
    ));

    // Drain raw bytes even if an external tool emits invalid UTF-8. A decoding
    // error must never close the pipe while the child is still writing.
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stderr_task = tokio::spawn(async move { drain_tail(stderr).await });
    let stdout_task = tokio::spawn(async move { drain_tail(stdout).await });
    let interrupted;
    let returncode;
    tokio::select! {
        _ = async { tokio::select! { _=state.shutdown.cancelled()=>{}, _=cancel.cancelled()=>{} } } => {
            interrupted=true;
            if let Some(pid)=child.id() { kill_pid(pid).await; }
            let _=child.kill().await;
            returncode=child.wait().await.map(|s|s.code().unwrap_or(-1)).unwrap_or(-1);
        }
        status=child.wait() => { interrupted=false; returncode=status.map(|s|s.code().unwrap_or(-1)).unwrap_or(-1); }
    }
    index_done.cancel();
    let _ = idx_task.await; // never abort a running blocking scan
    let log_text = stdout_task.await.unwrap_or_default();
    let stderr_text = stderr_task.await.unwrap_or_default();

    // Keep both diagnostic streams; cancellation comes from application state.
    let combined_text = if stderr_text.trim().is_empty() {
        log_text.clone()
    } else if log_text.is_empty() {
        stderr_text.clone()
    } else {
        format!("{}\n{}", log_text, stderr_text)
    };

    {
        let mut procs = state.active_processes.lock().await;
        procs.remove(&source_id);
    }

    let total: i64 = state
        .pool
        .get()
        .ok()
        .and_then(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM media WHERE source_id=?1 AND downloaded=1",
                [source_id],
                |r| r.get(0),
            )
            .ok()
        })
        .unwrap_or(0);
    let new_count = (total - initial_count).max(0);

    let was_paused = {
        let ids = state.paused_source_ids.lock().await;
        ids.contains(&source_id)
    };

    let mut status = if returncode == 0 { "done" } else { "error" };
    let mut error_msg: Option<String> = None;

    if was_paused {
        status = "paused";
        info!(
            "Source {} ({}) paused after {} new item(s), {} total",
            source_id, name, new_count, total
        );
    } else if interrupted {
        status = "paused";
        error_msg = Some(
            if cancel.is_cancelled() {
                "Cancelled by user; resync to continue."
            } else {
                "Interrupted by Curator shutdown; resync to continue."
            }
            .to_string(),
        );
        info!(
            "Source {} ({}) was interrupted after {} new item(s), {} total",
            source_id, name, new_count, total
        );
    } else if status == "error" {
        let summary = short_error_summary(&combined_text);
        warn!(
            "Source {} ({}) failed to sync: {}",
            source_id, name, summary
        );
        error_msg = Some(if summary.is_empty() {
            combined_text
                .chars()
                .rev()
                .take(4000)
                .collect::<String>()
                .chars()
                .rev()
                .collect()
        } else {
            summary
        });
    } else if new_count > 0 {
        info!(
            "Finished syncing source {} ({}): {} new item(s), {} total",
            source_id, name, new_count, total
        );
    } else {
        info!(
            "Finished syncing source {} ({}): nothing new ({} total)",
            source_id, name, total
        );
    }

    let log_tail: String = {
        let chars: Vec<char> = combined_text.chars().collect();
        chars
            .iter()
            .rev()
            .take(4000)
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    };

    if let Ok(conn) = state.pool.get() {
        let _ = conn.execute(
            "UPDATE sources SET status=?1, item_count=?2, error_message=?3, log=?4, synced_at=?5 WHERE id=?6",
            rusqlite::params![status, total, error_msg, log_tail, now_iso(), source_id],
        );
    } else {
        warn!("Source {source_id} finished but final status could not be persisted: database unavailable");
    }
}

fn short_error_summary(log_text: &str) -> String {
    let filtered: Vec<&str> = log_text
        .lines()
        .filter(|l| !l.contains("RequestsDependencyWarning"))
        .collect();
    let joined = filtered.join("\n");
    let tail: String = joined
        .chars()
        .rev()
        .take(500)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    tail.trim().to_string()
}

async fn drain_tail(mut reader: impl tokio::io::AsyncRead + Unpin) -> String {
    let mut tail = std::collections::VecDeque::new();
    let mut bytes = [0u8; 4096];
    while let Ok(n) = reader.read(&mut bytes).await {
        if n == 0 {
            break;
        }
        tail.extend(&bytes[..n]);
        while tail.len() > 65536 {
            tail.pop_front();
        }
    }
    filter_gdl_stderr(&String::from_utf8_lossy(
        &tail.into_iter().collect::<Vec<_>>(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_drain_survives_invalid_utf8_and_bounds_long_lines() {
        let mut bytes = vec![b'x'; 100_000];
        bytes.push(0xff);
        bytes.extend_from_slice(b"still draining");
        let tail = drain_tail(bytes.as_slice()).await;
        assert!(tail.ends_with("still draining"));
        assert!(tail.contains('\u{fffd}'));
        assert!(tail.len() <= 65_538);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn listing_shutdown_removes_registered_process() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = crate::test_support::state(dir.path());
        Arc::get_mut(&mut state).unwrap().gallery_dl_bin = "sh".into();
        let stream = spawn_gallery_dl(
            vec!["-c".into(), "sleep 30".into()],
            Some(77),
            state.clone(),
        );
        futures::pin_mut!(stream);
        let cancel = async {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if state.active_processes.lock().await.contains_key(&77) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            state.shutdown.cancel();
        };
        let consume = async { while stream.next().await.is_some() {} };
        tokio::join!(consume, cancel);
        assert!(state.active_processes.lock().await.is_empty());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn placeholder_cache_survives_repeated_calls_and_refreshes_changed_url() {
        let root = tempfile::tempdir().unwrap();
        let mut state = crate::test_support::state(root.path());
        Arc::get_mut(&mut state).unwrap().gallery_dl_bin = crate::test_support::fake_downloader();
        crate::test_support::source(&state);
        populate_placeholders(state.clone(), 1).await;
        state
            .pool
            .get()
            .unwrap()
            .execute(
                "UPDATE placeholder_scans SET retry_at=unixepoch()+100000",
                [],
            )
            .unwrap();
        let before: i64 = state
            .pool
            .get()
            .unwrap()
            .query_row("SELECT retry_at FROM placeholder_scans", [], |r| r.get(0))
            .unwrap();
        populate_placeholders(state.clone(), 1).await;
        assert_eq!(
            state
                .pool
                .get()
                .unwrap()
                .query_row("SELECT retry_at FROM placeholder_scans", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            before
        );
        state
            .pool
            .get()
            .unwrap()
            .execute("UPDATE sources SET url='https://example.test/changed'", [])
            .unwrap();
        populate_placeholders(state.clone(), 1).await;
        assert_eq!(
            state
                .pool
                .get()
                .unwrap()
                .query_row("SELECT url FROM placeholder_scans", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "https://example.test/changed"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn explicit_shutdown_and_real_failure_are_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = crate::test_support::state(dir.path());
        Arc::get_mut(&mut state).unwrap().gallery_dl_bin = crate::test_support::fake_downloader();
        crate::test_support::source(&state);
        for (url, expected) in [
            ("https://fixture/success", "done"),
            ("https://fixture/failure", "error"),
        ] {
            state
                .pool
                .get()
                .unwrap()
                .execute("UPDATE sources SET url=?1", [url])
                .unwrap();
            run_download(state.clone(), 1).await;
            assert_eq!(
                state
                    .pool
                    .get()
                    .unwrap()
                    .query_row("SELECT status FROM sources", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                expected
            );
        }
        state
            .pool
            .get()
            .unwrap()
            .execute("UPDATE sources SET url='https://fixture/wait'", [])
            .unwrap();
        let task = tokio::spawn(run_download(state.clone(), 1));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !state.library_dir.join("test/child.pid").is_file() {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        state.shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert!(state.active_processes.lock().await.is_empty());
        assert_eq!(
            state
                .pool
                .get()
                .unwrap()
                .query_row("SELECT status FROM sources", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "paused"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn pause_kills_windows_descendants_and_resume_requeues() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = crate::test_support::state(dir.path());
        Arc::get_mut(&mut state).unwrap().gallery_dl_bin = crate::test_support::fake_downloader();
        crate::test_support::source(&state);
        state
            .pool
            .get()
            .unwrap()
            .execute("UPDATE sources SET url='https://fixture/wait'", [])
            .unwrap();
        let task = tokio::spawn(run_download(state.clone(), 1));
        let pid_path = state.library_dir.join("test/child.pid");
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !pid_path.is_file() {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let pid: u32 = std::fs::read_to_string(pid_path).unwrap().parse().unwrap();
        let _ = crate::routes::downloads::pause(axum::extract::State(state.clone())).await;
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let output = crate::process::command("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 1 }}"),
            ])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "grandchild must be terminated too");
        state
            .pool
            .get()
            .unwrap()
            .execute("UPDATE sources SET url='https://fixture/success'", [])
            .unwrap();
        let result = crate::routes::downloads::resume(axum::extract::State(state.clone())).await;
        assert_eq!(result.0["requeued"], 1);
        state.download_tasks.close();
        state.download_tasks.wait().await;
        assert_eq!(
            state
                .pool
                .get()
                .unwrap()
                .query_row("SELECT status FROM sources", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "done"
        );
        kill_pid(0).await;
    }

    #[test]
    fn late_metadata_merges_placeholder_without_losing_ratings_or_tags() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(dir.path());
        crate::test_support::source(&state);
        let conn = state.pool.get().unwrap();
        conn.execute_batch("INSERT INTO media(id,source_id,filepath,filename,type,added_at,downloaded,origin_url,rating) VALUES(10,1,'pending','pending','image','2026',0,'https://example.test/item.jpg',4);
            INSERT INTO tags(id,name,added_at) VALUES(1,'keep','2026'),(2,'also keep','2026'); INSERT INTO media_tags VALUES(10,1);").unwrap();
        conn.execute("UPDATE media SET rating=0,rating_source='human',rating_reviewed=1,rating_reviewed_at='2026' WHERE id=10", []).unwrap();
        let path = state.library_dir.join("test/item.jpg");
        std::fs::write(&path, b"downloaded").unwrap();
        assert!(index_file(&state, 1, &path).unwrap());
        let real_id: i64 = conn
            .query_row("SELECT id FROM media WHERE downloaded=1", [], |r| r.get(0))
            .unwrap();
        crate::nsfw::persist_score(&conn, real_id, 0.72).unwrap();
        conn.execute("INSERT INTO media_tags VALUES(?1,2)", [real_id])
            .unwrap();
        std::fs::write(
            state.library_dir.join("test/item.jpg.json"),
            r#"{"url":"https://example.test/item.jpg"}"#,
        )
        .unwrap();
        assert!(index_file(&state, 1, &path).unwrap());
        assert!(!index_file(&state, 1, &path).unwrap());
        assert_eq!(
            conn.query_row("SELECT id,rating,downloaded FROM media", [], |r| Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?
            )))
            .unwrap(),
            (10, 0, 1)
        );
        let provenance: (String, bool, i64) = conn
            .query_row(
                "SELECT rating_source,rating_reviewed,auto_rating FROM media",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(provenance, ("human".into(), true, 4));
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM media_tags WHERE media_id=10",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM media", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn partial_files_are_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(dir.path());
        crate::test_support::source(&state);
        let path = state.library_dir.join("test/item.jpg");
        std::fs::write(&path, b"incomplete").unwrap();
        std::fs::write(state.library_dir.join("test/item.jpg.part"), b"partial").unwrap();
        assert!(!index_file(&state, 1, &path).unwrap());
    }

    #[tokio::test]
    async fn native_events_index_completed_file_before_final_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(dir.path());
        crate::test_support::source(&state);
        let done = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(index_download(
            state.clone(),
            1,
            state.library_dir.join("test"),
            done.clone(),
        ));
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let path = state.library_dir.join("test/event.jpg");
        std::fs::write(&path, b"complete").unwrap();
        let observed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let count: i64 = state
                    .pool
                    .get()
                    .unwrap()
                    .query_row(
                        "SELECT COUNT(*) FROM media WHERE filepath='test/event.jpg'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                if count == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;
        done.cancel();
        task.await.unwrap();
        assert!(
            observed.is_ok(),
            "native filesystem event should index without a recovery scan"
        );
    }

    #[test]
    fn probe_video_duration_fails_soft_on_a_missing_binary() {
        // Never a panic or Err — just None, same as "duration not known yet".
        assert_eq!(
            probe_video_duration(
                "definitely-not-a-real-binary-xyz",
                Path::new("/nonexistent.mp4")
            ),
            None
        );
    }

    #[test]
    fn probe_video_duration_fails_soft_on_a_missing_file() {
        assert_eq!(
            probe_video_duration("ffprobe", Path::new("/nonexistent.mp4")),
            None
        );
    }

    #[test]
    fn probe_video_duration_reads_a_real_file_correctly() {
        // Skips (doesn't fail) on machines without ffmpeg/ffprobe installed —
        // this checks the parsing logic is correct, not that ffmpeg exists.
        if !std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            eprintln!("skipping: ffmpeg not available on this machine");
            return;
        }

        let dir = std::env::temp_dir().join(format!("curator_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.mp4");

        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=3:size=64x64:rate=5",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "ffmpeg failed to generate the test fixture"
        );

        let duration = probe_video_duration("ffprobe", &path);
        let _ = std::fs::remove_dir_all(&dir);

        let d =
            duration.expect("ffprobe should have reported a duration for a file ffmpeg just made");
        assert!((d - 3.0).abs() < 0.5, "expected ~3s, got {}", d);
    }
}
