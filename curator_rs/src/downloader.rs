use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
};
use anyhow::Result;
use once_cell::sync::Lazy;
use tokio::{process::Command, sync::Semaphore};
use tracing::{info, warn};

use crate::{db, state::AppState};

static IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff", "tif", "avif", "heic", "heif",
];
static VIDEO_EXTS: &[&str] = &["mp4", "mkv", "webm", "avi", "mov", "wmv", "flv", "m4v", "ts"];

fn is_image(ext: &str) -> bool { IMAGE_EXTS.contains(&ext) }
fn is_video(ext: &str) -> bool { VIDEO_EXTS.contains(&ext) }

fn filter_gdl_stderr(raw: &str) -> String {
    raw.lines()
        .filter(|l| !l.contains("RequestsDependencyWarning"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn short_error_summary(log: &str) -> String {
    // Pull the first non-empty, non-warning line from the tail
    log.lines()
        .rev()
        .filter(|l| {
            let l = l.trim();
            !l.is_empty()
                && !l.contains("RequestsDependencyWarning")
                && !l.starts_with("  ")
        })
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Walk dest for new media files and index them. Returns (total_downloaded, newly_added).
/// Retires placeholder rows in-place (same id) so ratings/tags survive download.
pub fn scan_and_index(
    pool: &db::DbPool,
    source_id: i64,
    dest: &Path,
    library_dir: &Path,
) -> Result<(i64, i64)> {
    let conn = pool.get()?;

    // Existing real filepaths (relative to library_dir)
    let real_existing: HashSet<String> = {
        let mut stmt = conn.prepare(
            "SELECT filepath FROM media WHERE source_id=? AND downloaded=1",
        )?;
        let real_existing: HashSet<String> = stmt.query_map([source_id], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        real_existing
    };

    // Placeholder rows keyed by origin_url
    let mut placeholders_by_url: HashMap<String, i64> = {
        let mut stmt = conn.prepare(
            "SELECT id, origin_url FROM media WHERE source_id=? AND downloaded=0",
        )?;
        let placeholders: HashMap<String, i64> = stmt.query_map([source_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .map(|(id, url)| (url, id))
            .collect();
        placeholders
    };

    // Walk dest
    let mut new_rows: Vec<(i64, String, String, &'static str, String, Option<String>)> = Vec::new();
    let mut retired: Vec<(i64, String, String)> = Vec::new(); // (media_id, rel_path, filename)
    let mut sidecars_to_clean: Vec<PathBuf> = Vec::new();

    if dest.exists() {
        for entry in walkdir::WalkDir::new(dest)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            let ext = path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            if !is_image(&ext) && !is_video(&ext) {
                continue;
            }

            let rel = path
                .strip_prefix(library_dir)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();

            if real_existing.contains(&rel) {
                continue;
            }

            // Try sidecar for origin_url matching
            let sidecar = path.with_file_name(format!(
                "{}.json",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            let mut origin_url: Option<String> = None;
            if sidecar.exists() {
                if let Ok(raw) = std::fs::read_to_string(&sidecar) {
                    if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&raw) {
                        origin_url = meta["url"].as_str().map(|s| s.to_string());
                    }
                }
                sidecars_to_clean.push(sidecar);
            }

            let media_type: &'static str = if is_video(&ext) { "video" } else { "image" };
            let filename = path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();

            if let Some(url) = &origin_url {
                if let Some(media_id) = placeholders_by_url.remove(url) {
                    retired.push((media_id, rel, filename));
                    continue;
                }
            }

            new_rows.push((source_id, rel, filename, media_type, db::now_iso(), origin_url));
        }
    }

    // Bulk write
    conn.execute("BEGIN", [])?;
    if !retired.is_empty() {
        let mut stmt = conn.prepare(
            "UPDATE media SET filepath=?, filename=?, downloaded=1 WHERE id=?",
        )?;
        for (id, rel, name) in &retired {
            stmt.execute(rusqlite::params![rel, name, id])?;
        }
    }
    if !new_rows.is_empty() {
        let mut stmt = conn.prepare(
            "INSERT OR IGNORE INTO media \
             (source_id, filepath, filename, type, added_at, origin_url, downloaded) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
        )?;
        for (sid, rel, name, mtype, ts, url) in &new_rows {
            stmt.execute(rusqlite::params![sid, rel, name, mtype, ts, url])?;
        }
    }
    conn.execute("COMMIT", [])?;

    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM media WHERE source_id=? AND downloaded=1",
        [source_id],
        |r| r.get(0),
    )?;

    // Clean sidecars
    for sc in sidecars_to_clean {
        let _ = std::fs::remove_file(sc);
    }

    let added = new_rows.len() as i64 + retired.len() as i64;
    Ok((total, added))
}

/// Best-effort: pre-populate placeholder rows via gallery-dl -j so the UI
/// can show items before they're downloaded. Silently non-fatal.
pub async fn populate_placeholders(state: Arc<AppState>, source_id: i64) {
    static PLACEHOLDER_SEM: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(3));

    let (url, status) = {
        let conn = match state.pool.get() {
            Ok(c) => c, Err(_) => return,
        };
        match conn.query_row(
            "SELECT url, status FROM sources WHERE id=?",
            [source_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        ) {
            Ok(v) => v, Err(_) => return,
        }
    };

    if status == "done" { return; }

    // Guard: if any real rows exist with NULL origin_url, skip (duplicate risk)
    {
        let conn = match state.pool.get() {
            Ok(c) => c, Err(_) => return,
        };
        let unmatched: i64 = conn.query_row(
            "SELECT COUNT(*) FROM media WHERE source_id=? AND downloaded=1 AND origin_url IS NULL",
            [source_id], |r| r.get(0),
        ).unwrap_or(1);
        if unmatched > 0 { return; }
    }

    let _permit = match PLACEHOLDER_SEM.try_acquire() {
        Ok(p) => p,
        Err(_) => return, // busy — skip, not fatal
    };

    let output = match run_gallery_dl_j(&url, 120).await {
        Ok(o) => o, Err(_) => return,
    };

    let items = parse_gdl_j_output(&output, &url);
    if items.is_empty() { return; }

    let conn = match state.pool.get() {
        Ok(c) => c, Err(_) => return,
    };

    if let Ok(mut stmt) = conn.prepare(
        "INSERT OR IGNORE INTO media \
         (source_id, filepath, filename, type, added_at, origin_url, downloaded) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
    ) {
        let _ = conn.execute("BEGIN", []);
        for item in &items {
            let fp = db::pending_filepath(source_id, &item.url);
            let fname = item.url.split('?').next().unwrap_or(&item.url)
                .rsplit('/').next().unwrap_or("item");
            let _ = stmt.execute(rusqlite::params![
                source_id, fp, fname, item.media_type, db::now_iso(), item.url
            ]);
        }
        let _ = conn.execute("COMMIT", []);
    }
    info!("Placeholder pre-scan for source {source_id}: {} candidate item(s)", items.len());
}

struct GdlItem {
    url: String,
    media_type: &'static str,
}

/// Recursively walk gallery-dl -j output (nested arrays) and collect file entries.
fn parse_gdl_j_output(raw: &str, _source_url: &str) -> Vec<GdlItem> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(raw) else { return vec![] };
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    walk_gdl_node(&root, &mut items, &mut seen);
    items
}

fn walk_gdl_node(node: &serde_json::Value, out: &mut Vec<GdlItem>, seen: &mut HashSet<String>) {
    let Some(arr) = node.as_array() else { return };

    // Pattern: [int_or_str, "http://...", {...metadata}]
    if arr.len() >= 2 {
        if let Some(url_str) = arr[1].as_str() {
            if url_str.starts_with("http") && !seen.contains(url_str) {
                let meta = arr.last().and_then(|v| v.as_object());
                let ext = meta
                    .and_then(|m| m.get("extension"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                let ext = if ext.is_empty() {
                    url_str.split('?').next().unwrap_or("")
                        .rsplit('.').next().unwrap_or("").to_lowercase()
                } else { ext };

                let media_type: Option<&'static str> = if is_image(&ext) {
                    Some("image")
                } else if is_video(&ext) {
                    Some("video")
                } else {
                    None
                };

                if let Some(mt) = media_type {
                    seen.insert(url_str.to_string());
                    out.push(GdlItem { url: url_str.to_string(), media_type: mt });
                }
            }
        }
    }

    for item in arr {
        if item.is_array() {
            walk_gdl_node(item, out, seen);
        }
    }
}

/// Run gallery-dl -j --no-download <url> with timeout. Returns raw stdout.
pub async fn run_gallery_dl_j(url: &str, timeout_s: u64) -> Result<String> {
    use tokio::io::AsyncReadExt;

    let gdl = gallery_dl_bin();
    let mut child = Command::new(&gdl)
        .args(["-j", "--no-download", url])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    // Take pipes up front so `child` itself stays owned/usable (e.g. to kill on timeout).
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let read_task = async {
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        if let Some(mut s) = stdout_pipe.take() {
            let _ = s.read_to_end(&mut stdout_buf).await;
        }
        if let Some(mut s) = stderr_pipe.take() {
            let _ = s.read_to_end(&mut stderr_buf).await;
        }
        (stdout_buf, stderr_buf)
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_s),
        read_task,
    ).await;

    match result {
        Ok((stdout_bytes, stderr_bytes)) => {
            let _ = child.wait().await;
            let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
            if stdout.trim().is_empty() {
                let stderr = filter_gdl_stderr(&String::from_utf8_lossy(&stderr_bytes));
                Err(anyhow::anyhow!("{}", if stderr.is_empty() {
                    "gallery-dl returned no output".to_string()
                } else { stderr }))
            } else {
                Ok(stdout)
            }
        }
        Err(_) => {
            let _ = child.kill().await;
            Err(anyhow::anyhow!("Timed out scanning URL (>{timeout_s}s)"))
        }
    }
}

/// Fire-and-forget download task. Mirrors Python's run_download exactly.
pub async fn run_download(state: Arc<AppState>, source_id: i64) {
    // Spawn placeholder task first (non-blocking)
    {
        let s = Arc::clone(&state);
        tokio::spawn(async move { populate_placeholders(s, source_id).await });
    }

    if state.downloads_paused.load(Ordering::SeqCst) {
        let conn = state.pool.get().unwrap();
        let _ = conn.execute(
            "UPDATE sources SET status='paused' WHERE id=?",
            [source_id],
        );
        return;
    }

    // Claim status before waiting for semaphore slot
    {
        let conn = state.pool.get().unwrap();
        let _ = conn.execute(
            "UPDATE sources SET status='downloading', error_message=NULL WHERE id=?",
            [source_id],
        );
    }

    // Bounded concurrency — static semaphore, default 3 slots.
    // Changing max_concurrent takes effect for the next queued download
    // (same behaviour as the Python version).
    static SEM: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(3));
    let _permit = SEM.acquire().await;

    if state.downloads_paused.load(Ordering::SeqCst) {
        let conn = state.pool.get().unwrap();
        let _ = conn.execute("UPDATE sources SET status='paused' WHERE id=?", [source_id]);
        return;
    }

    run_download_inner(Arc::clone(&state), source_id).await;
}

async fn run_download_inner(state: Arc<AppState>, source_id: i64) {
    let (url, slug, name) = {
        let conn = match state.pool.get() { Ok(c) => c, Err(_) => return };
        match conn.query_row(
            "SELECT url, slug, name FROM sources WHERE id=?",
            [source_id],
            |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,String>(2)?)),
        ) { Ok(v) => v, Err(_) => return }
    };

    let library_dir = state.data_dir.join("library");
    let archives_dir = state.data_dir.join("archives");
    let dest = dunce::simplified(&library_dir.join(&slug)).to_path_buf();
    let archive_path = dunce::simplified(&archives_dir.join(format!("{slug}.sqlite3"))).to_path_buf();

    let _ = std::fs::create_dir_all(&dest);
    let _ = std::fs::create_dir_all(&archives_dir);

    info!("Starting sync for source {source_id} ({name}): {url}");

    let gdl = gallery_dl_bin();
    let mut child = match Command::new(&gdl)
        .args([
            &url,
            "-D", &dest.to_string_lossy(),
            "--download-archive", &archive_path.to_string_lossy(),
            "--no-part",
            "--write-metadata",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped()) // read separately, merged into the same log buffer below
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("gallery-dl could not be launched: {e}");
            let conn = state.pool.get().unwrap();
            let _ = conn.execute(
                "UPDATE sources SET status='error', error_message=?, synced_at=? WHERE id=?",
                rusqlite::params![msg, db::now_iso(), source_id],
            );
            return;
        }
    };

    // Track PID for pause/delete
    if let Some(pid) = child.id() {
        state.active_processes.lock().unwrap().insert(source_id, pid);
    }

    // Progressive indexer — scan every 4s while download runs
    let pool_clone = state.pool.clone();
    let dest_clone = dest.clone();
    let lib_clone = library_dir.clone();
    let sid = source_id;
    let indexer = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            let pool = pool_clone.clone();
            let dest = dest_clone.clone();
            let lib = lib_clone.clone();
            let _ = tokio::task::spawn_blocking(move || {
                scan_and_index(&pool, sid, &dest, &lib)
            }).await;
        }
    });

    // Collect output (rolling 500-line buffer, stdout+stderr merged)
    let log_lines: Arc<std::sync::Mutex<std::collections::VecDeque<String>>> =
        Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let read_stdout_task = {
        let log_lines = Arc::clone(&log_lines);
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            if let Some(stdout) = stdout {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let mut lines = log_lines.lock().unwrap();
                    lines.push_back(line);
                    if lines.len() > 500 { lines.pop_front(); }
                }
            }
        })
    };
    let read_stderr_task = {
        let log_lines = Arc::clone(&log_lines);
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            if let Some(stderr) = stderr {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    let mut lines = log_lines.lock().unwrap();
                    lines.push_back(line);
                    if lines.len() > 500 { lines.pop_front(); }
                }
            }
        })
    };

    let exit_status = child.wait().await;
    indexer.abort();
    let _ = tokio::join!(read_stdout_task, read_stderr_task);
    let log_text: String = log_lines.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n");
    let returncode = exit_status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);

    state.active_processes.lock().unwrap().remove(&source_id);

    // Final index pass
    let (total, new_count) = tokio::task::spawn_blocking({
        let pool = state.pool.clone();
        let dest = dest.clone();
        let lib = library_dir.clone();
        move || scan_and_index(&pool, sid, &dest, &lib).unwrap_or((0, 0))
    }).await.unwrap_or((0, 0));

    let was_paused = state.paused_source_ids.lock().unwrap().remove(&source_id);
    let interrupted = log_text.contains("KeyboardInterrupt");
    let status = if was_paused {
        "paused"
    } else if returncode == 0 || total > 0 {
        "done"
    } else {
        "error"
    };

    let error_message: Option<String> = if was_paused {
        info!("Source {source_id} ({name}) paused after {new_count} new item(s), {total} total");
        None
    } else if interrupted {
        let s = if total > 0 { "done" } else { "error" };
        info!("Source {source_id} ({name}) interrupted by shutdown after {new_count} new, {total} total");
        Some("Interrupted by Curator shutting down mid-download — not a real failure. Resync to pick up where it left off.".into())
    } else if status == "error" {
        let summary = short_error_summary(&log_text);
        warn!("Source {source_id} ({name}) failed to sync: {summary}");
        Some(if summary.is_empty() { log_text.chars().rev().take(4000).collect::<String>().chars().rev().collect() } else { summary })
    } else {
        if new_count > 0 {
            info!("Finished syncing source {source_id} ({name}): {new_count} new item(s), {total} total");
        } else {
            info!("Finished syncing source {source_id} ({name}): nothing new ({total} total)");
        }
        None
    };

    let log_tail: String = {
        let chars: Vec<char> = log_text.chars().collect();
        chars[chars.len().saturating_sub(4000)..].iter().collect()
    };

    let conn = state.pool.get().unwrap();
    let _ = conn.execute(
        "UPDATE sources SET status=?, item_count=?, error_message=?, log=?, synced_at=? WHERE id=?",
        rusqlite::params![
            if was_paused { "paused" } else if interrupted && total == 0 { "error" } else { status },
            total,
            error_message,
            log_tail,
            db::now_iso(),
            source_id,
        ],
    );
}

fn gallery_dl_bin() -> String {
    // Use sys.executable equivalent — check if gallery-dl is on PATH
    // Fall back to `python -m gallery_dl` is not straightforward in Rust;
    // user must have gallery-dl on PATH or set GALLERY_DL_BIN env var.
    std::env::var("GALLERY_DL_BIN").unwrap_or_else(|_| "gallery-dl".to_string())
}
