use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use futures::{Stream, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tracing::{info, warn};

use crate::AppState;
use crate::db::now_iso;
use crate::slug::pending_filepath;

// ─── Extension sets ──────────────────────────────────────────────────────────

pub fn image_exts() -> &'static [&'static str] {
    &["jpg", "jpeg", "png", "gif", "webp", "bmp", "jfif", "avif", "tiff"]
}

pub fn video_exts() -> &'static [&'static str] {
    &["mp4", "webm", "mov", "avi", "mkv", "m4v"]
}

pub fn path_image_exts() -> &'static [&'static str] {
    &[".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".jfif", ".avif", ".tiff"]
}

pub fn path_video_exts() -> &'static [&'static str] {
    &[".mp4", ".webm", ".mov", ".avi", ".mkv", ".m4v"]
}

fn is_image_path(path: &Path) -> bool {
    let ext = path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    image_exts().contains(&ext.as_str())
}

fn is_video_path(path: &Path) -> bool {
    let ext = path.extension()
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
// Windows: `taskkill /F /PID` — deliberately NOT winapi/TerminateProcess FFI.
// taskkill is available on every Windows target without unsafe bindings and
// correctly handles child-process trees; a raw TerminateProcess call on the
// gallery-dl PID alone can leave orphaned grandchild processes behind.

pub async fn kill_pid(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        unsafe { libc::kill(pid as i32, libc::SIGTERM); }
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
    args:       Vec<String>,
    source_id:  Option<i64>,
    state:      Arc<AppState>,
) -> impl Stream<Item = Result<Value>> {
    async_stream::stream! {
        let mut cmd = Command::new(&state.gallery_dl_bin);
        cmd.args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

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
            let mut buf = String::new();
            let mut reader = BufReader::new(stderr);
            let _ = reader.read_to_string(&mut buf).await;
            filter_gdl_stderr(&buf)
        });

        let mut raw = String::new();
        {
            let mut reader = BufReader::new(stdout);
            let _ = reader.read_to_string(&mut raw).await;
        }

        let _ = child.wait().await;
        if let (Some(id), Some(_)) = (source_id, pid) {
            state.active_processes.lock().await.remove(&id);
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
    pub url:     String,
    #[serde(rename = "type")]
    pub kind:    String,
    pub creator: String,
    pub title:   String,
    pub poster:  Option<String>,
    pub source:  String,
}

pub fn preview_ext_from(meta: &Value, url: &str) -> String {
    if let Some(ext) = meta.get("extension").and_then(|v| v.as_str()) {
        if !ext.is_empty() { return ext.to_lowercase(); }
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
    node:       &Value,
    source_url: &str,
    results:    &mut Vec<PreviewItem>,
    seen:       &mut std::collections::HashSet<String>,
) {
    let arr = match node.as_array() {
        Some(a) => a,
        None    => return,
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

                        let creator = meta.and_then(|m| {
                            m.get("username").or_else(|| m.get("author"))
                             .or_else(|| m.get("user")).or_else(|| m.get("artist"))
                        })
                        .and_then(|v| v.as_str())
                        .unwrap_or(source_url)
                        .to_string();

                        let title = meta.and_then(|m| {
                            m.get("title").and_then(|v| v.as_str()).map(|s| s.to_string())
                                .or_else(|| m.get("id").map(|v| v.to_string()))
                        }).unwrap_or_default();

                        let poster = meta.and_then(|m| {
                            m.get("thumbnail").or_else(|| m.get("preview"))
                        }).and_then(|v| v.as_str()).map(|s| s.to_string());

                        results.push(PreviewItem {
                            url:     url_str.to_string(),
                            kind:    kind.to_string(),
                            creator,
                            title,
                            poster,
                            source:  source_url.to_string(),
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
    url:          &str,
    timeout_secs: u64,
    state:        Arc<AppState>,
) -> std::result::Result<Vec<PreviewItem>, String> {
    let args = vec!["-j".into(), "--no-download".into(), url.to_string()];
    let url_owned = url.to_string();

    let collect_fut = async move {
        let stream = spawn_gallery_dl(args, None, state);
        futures::pin_mut!(stream);

        let mut items: Vec<PreviewItem> = Vec::new();
        let mut seen  = std::collections::HashSet::new();
        let mut first_err: Option<String> = None;

        while let Some(node_result) = stream.next().await {
            match node_result {
                Ok(node) => preview_walk(&node, &url_owned, &mut items, &mut seen),
                Err(e)   => { if first_err.is_none() { first_err = Some(e.to_string()); } }
            }
        }

        (items, first_err)
    };

    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), collect_fut).await {
        Err(_) => Err(format!("Timed out scanning that URL (took > {}s).", timeout_secs)),
        Ok((items, first_err)) => {
            if items.is_empty() {
                if let Some(e) = first_err {
                    return Err(e);
                }
            }
            Ok(items)
        }
    }
}

// ─── scan_and_index ───────────────────────────────────────────────────────────
// Returns (total_downloaded_count, newly_added_count).
//
// Uses a single SQL-level UPSERT per the plan rather than an application-logic
// select-then-branch: `ON CONFLICT(source_id, origin_url) ... DO UPDATE` retires
// a matching placeholder row in place (never regresses downloaded 1→0 — it
// unconditionally sets downloaded=1 on the SAME row, so existing ratings/tags
// on that media_id survive the transition). `ON CONFLICT(filepath) DO NOTHING`
// is the second conflict target: media.filepath carries its own UNIQUE
// constraint independent of origin_url, and without a second explicit target
// here that path would raise a constraint-violation error instead of the old
// INSERT-OR-IGNORE-style silent skip on a re-scan race.
//
// Every path constructed from a DB `filepath` value or walked off disk goes
// through `dunce::simplified` — this user's filenames embed long base64
// URLs, and raw `\\?\`-prefixed long paths on Windows confuse non-WinAPI
// callers (including SQLite's own file layer in some configurations).
pub fn scan_and_index(
    state:     &AppState,
    source_id: i64,
    dest:      &Path,
) -> Result<(i64, i64)> {
    let conn = state.pool.get()?;

    let dest        = dunce::simplified(dest).to_path_buf();
    let library_dir = dunce::simplified(&state.library_dir).to_path_buf();

    // Files already indexed as real (downloaded=1) — skip re-processing them
    // on every 4-second poll cycle during a long-running download.
    let mut stmt = conn.prepare(
        "SELECT filepath FROM media WHERE source_id=?1 AND downloaded=1"
    )?;
    let real_existing: std::collections::HashSet<String> = stmt
        .query_map([source_id], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    struct Candidate {
        filepath:   String,
        filename:   String,
        kind:       String,
        origin_url: Option<String>,
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut sidecars:   Vec<PathBuf>   = Vec::new();

    if dest.exists() {
        for entry in walkdir::WalkDir::new(&dest)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = dunce::simplified(entry.path()).to_path_buf();
            if !is_image_path(&path) && !is_video_path(&path) { continue; }

            // filepath stored relative to library_dir
            let rel = match path.strip_prefix(&library_dir) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };

            if real_existing.contains(&rel) { continue; }

            // Check for --write-metadata sidecar
            let sidecar_path = {
                let mut s = path.clone();
                let mut name = s.file_name().unwrap_or_default().to_os_string();
                name.push(".json");
                s.set_file_name(name);
                dunce::simplified(&s).to_path_buf()
            };

            let origin_url: Option<String> = if sidecar_path.exists() {
                sidecars.push(sidecar_path.clone());
                std::fs::read_to_string(&sidecar_path)
                    .ok()
                    .and_then(|txt| serde_json::from_str::<Value>(&txt).ok())
                    .and_then(|v| v.get("url").and_then(|u| u.as_str()).map(|s| s.to_string()))
            } else {
                None
            };

            let kind = if is_video_path(&path) { "video" } else { "image" };
            let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();

            candidates.push(Candidate { filepath: rel, filename, kind: kind.to_string(), origin_url });
        }
    }

    let now = now_iso();
    let mut added: i64 = 0;

    conn.execute("BEGIN", [])?;
    {
        let mut stmt = conn.prepare_cached(
            "INSERT INTO media (source_id, filepath, filename, type, added_at, origin_url, downloaded)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)
             ON CONFLICT(source_id, origin_url) WHERE origin_url IS NOT NULL
                DO UPDATE SET filepath=excluded.filepath, filename=excluded.filename, downloaded=1
             ON CONFLICT(filepath) DO NOTHING"
        )?;
        for c in &candidates {
            let changed = stmt.execute(rusqlite::params![
                source_id, c.filepath, c.filename, c.kind, now, c.origin_url
            ])?;
            added += changed as i64;
        }
    }
    conn.execute("COMMIT", [])?;

    // Clean up sidecars now that their data has been persisted
    for sidecar in sidecars {
        let _ = std::fs::remove_file(sidecar);
    }

    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM media WHERE source_id=?1 AND downloaded=1",
        [source_id],
        |r| r.get(0),
    )?;

    Ok((total, added))
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
            Ok(c) => c, Err(_) => return,
        };
        match conn.query_row(
            "SELECT url, status FROM sources WHERE id=?1",
            [source_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        ) {
            Ok(v)  => v,
            Err(_) => return,
        }
    };

    if status == "done" { return; }

    // Guard: if any real file predates origin_url tracking, skip to avoid duplicates
    let unmatched_real: i64 = {
        let conn = match state.pool.get() { Ok(c) => c, Err(_) => return };
        conn.query_row(
            "SELECT COUNT(*) FROM media WHERE source_id=?1 AND downloaded=1 AND origin_url IS NULL",
            [source_id],
            |r| r.get(0),
        ).unwrap_or(0)
    };
    if unmatched_real > 0 { return; }

    // Rate-limit placeholder scans (3 concurrent max)
    let _permit = match state.placeholder_semaphore.acquire().await {
        Ok(p) => p, Err(_) => return,
    };

    let items = match collect_gallery_dl_items(&url, 180, Arc::clone(&state)).await {
        Ok(items) => items,
        Err(err)  => {
            info!("Placeholder pre-scan skipped for source {}: {}", source_id, err);
            return;
        }
    };
    if items.is_empty() { return; }

    let conn = match state.pool.get() { Ok(c) => c, Err(_) => return };
    let now = now_iso();

    let _ = conn.execute("BEGIN", []);
    {
        let mut stmt = match conn.prepare(
            "INSERT OR IGNORE INTO media (source_id, filepath, filename, type, added_at, origin_url, downloaded)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)"
        ) { Ok(s) => s, Err(_) => { let _ = conn.execute("ROLLBACK", []); return; } };

        for item in &items {
            let fname = item.url.split('?').next().unwrap_or(&item.url)
                .rsplit('/').next().unwrap_or("item").to_string();
            let fname = if fname.is_empty() { "item".to_string() } else { fname };
            let fp    = pending_filepath(source_id, &item.url);
            let _ = stmt.execute(rusqlite::params![source_id, fp, fname, item.kind, now, item.url]);
        }
    }
    let _ = conn.execute("COMMIT", []);

    info!("Placeholder pre-scan for source {}: {} candidate item(s)", source_id, items.len());
}

// ─── run_download ─────────────────────────────────────────────────────────────

pub async fn run_download(state: Arc<AppState>, source_id: i64) {
    // Fire placeholder scan concurrently — never gates the real download
    let state2 = Arc::clone(&state);
    tokio::spawn(async move { populate_placeholders(state2, source_id).await });

    if state.downloads_paused.load(std::sync::atomic::Ordering::SeqCst) {
        let conn = state.pool.get().unwrap();
        let _ = conn.execute("UPDATE sources SET status='paused' WHERE id=?1", [source_id]);
        return;
    }

    // Claim status NOW (before the semaphore wait) to avoid concurrent duplicate downloads
    {
        let conn = state.pool.get().unwrap();
        let _ = conn.execute(
            "UPDATE sources SET status='downloading', error_message=NULL WHERE id=?1",
            [source_id],
        );
    }

    let sem = {
        let guard = state.download_semaphore.lock().await;
        Arc::clone(&*guard)
    };
    let _permit = match sem.acquire().await {
        Ok(p) => p, Err(_) => return,
    };

    if state.downloads_paused.load(std::sync::atomic::Ordering::SeqCst) {
        let conn = state.pool.get().unwrap();
        let _ = conn.execute("UPDATE sources SET status='paused' WHERE id=?1", [source_id]);
        return;
    }

    run_download_inner(Arc::clone(&state), source_id).await;
}

async fn run_download_inner(state: Arc<AppState>, source_id: i64) {
    let (url, slug, name) = {
        let conn = match state.pool.get() { Ok(c) => c, Err(_) => return };
        match conn.query_row(
            "SELECT url, slug, name FROM sources WHERE id=?1",
            [source_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        ) {
            Ok(v) => v, Err(_) => return,
        }
    };

    let dest         = dunce::simplified(&state.library_dir.join(&slug)).to_path_buf();
    let archive_path = dunce::simplified(&state.archives_dir.join(format!("{}.sqlite3", slug))).to_path_buf();
    let _ = std::fs::create_dir_all(&dest);

    info!("Starting sync for source {} ({}): {}", source_id, name, url);

    let args = vec![
        url.clone(),
        "-D".into(), dest.to_string_lossy().to_string(),
        "--download-archive".into(), archive_path.to_string_lossy().to_string(),
        "--no-part".into(),
        "--write-metadata".into(),
    ];

    let mut child = match Command::new(&state.gallery_dl_bin)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("gallery-dl could not be launched: {}. Is it on your PATH?", e);
            let conn = state.pool.get().unwrap();
            let _ = conn.execute(
                "UPDATE sources SET status='error', error_message=?1, synced_at=?2 WHERE id=?3",
                rusqlite::params![msg, now_iso(), source_id],
            );
            return;
        }
    };

    let pid = child.id().unwrap_or(0);
    {
        let mut procs = state.active_processes.lock().await;
        procs.insert(source_id, pid);
    }

    // Progressive indexing while gallery-dl runs
    let state_idx   = Arc::clone(&state);
    let dest_idx    = dest.clone();
    let idx_task    = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            let _ = tokio::task::spawn_blocking({
                let s = Arc::clone(&state_idx);
                let d = dest_idx.clone();
                move || scan_and_index(&s, source_id, &d)
            }).await;
        }
    });

    // Drain stdout (keep last 500 lines)
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let mut lines_buf: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut stdout_reader = BufReader::new(stdout).lines();

    let stderr_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut s).await;
        filter_gdl_stderr(&s)
    });

    while let Ok(Some(line)) = stdout_reader.next_line().await {
        lines_buf.push_back(line);
        if lines_buf.len() > 500 { lines_buf.pop_front(); }
    }

    let returncode = child.wait().await.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);

    idx_task.abort();
    let _ = idx_task.await;

    let log_text = lines_buf.into_iter().collect::<Vec<_>>().join("\n");
    let _stderr_text = stderr_task.await.unwrap_or_default();

    {
        let mut procs = state.active_processes.lock().await;
        procs.remove(&source_id);
    }

    let (total, new_count) = tokio::task::spawn_blocking({
        let s = Arc::clone(&state);
        let d = dest.clone();
        move || scan_and_index(&s, source_id, &d).unwrap_or((0, 0))
    }).await.unwrap_or((0, 0));

    let was_paused = {
        let mut ids = state.paused_source_ids.lock().await;
        ids.remove(&source_id)
    };

    let interrupted   = log_text.contains("KeyboardInterrupt");
    let mut status    = if returncode == 0 || total > 0 { "done" } else { "error" };
    let mut error_msg: Option<String> = None;

    if was_paused {
        status = "paused";
        info!("Source {} ({}) paused after {} new item(s), {} total", source_id, name, new_count, total);
    } else if interrupted {
        status = if total > 0 { "done" } else { "error" };
        error_msg = Some("Interrupted by Curator shutting down mid-download — not a real failure. Resync to pick up where it left off.".to_string());
        info!("Source {} ({}) was interrupted after {} new item(s), {} total", source_id, name, new_count, total);
    } else if status == "error" {
        let summary = short_error_summary(&log_text);
        warn!("Source {} ({}) failed to sync: {}", source_id, name, summary);
        error_msg = Some(if summary.is_empty() { log_text.chars().rev().take(4000).collect::<String>().chars().rev().collect() } else { summary });
    } else if new_count > 0 {
        info!("Finished syncing source {} ({}): {} new item(s), {} total", source_id, name, new_count, total);
    } else {
        info!("Finished syncing source {} ({}): nothing new ({} total)", source_id, name, total);
    }

    let log_tail: String = {
        let chars: Vec<char> = log_text.chars().collect();
        chars.iter().rev().take(4000).collect::<String>().chars().rev().collect()
    };

    let conn = state.pool.get().unwrap();
    let _ = conn.execute(
        "UPDATE sources SET status=?1, item_count=?2, error_message=?3, log=?4, synced_at=?5 WHERE id=?6",
        rusqlite::params![status, total, error_msg, log_tail, now_iso(), source_id],
    );
}

fn short_error_summary(log_text: &str) -> String {
    let filtered: Vec<&str> = log_text.lines()
        .filter(|l| !l.contains("RequestsDependencyWarning"))
        .collect();
    let joined = filtered.join("\n");
    let tail: String = joined.chars().rev().take(500).collect::<String>().chars().rev().collect();
    tail.trim().to_string()
}
