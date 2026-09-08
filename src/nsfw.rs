//! Background NSFW classification.
//!
//! Talks to a small persistent Python worker process (`nsfw_worker.py`,
//! embedded in the binary and written out to the data dir on startup — see
//! `main.rs`) over stdin/stdout, so the comparatively expensive model load
//! happens once per app run rather than once per image.
//!
//! This whole feature is opt-in and fails soft: if Python or the
//! `opennsfw-onnx` package aren't available, the worker keeps failing
//! to start, `classify()` keeps returning errors, and the rest of the app
//! is completely unaffected — media just stays unscored.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

#[derive(Serialize)]
struct Request<'a> {
    id: i64,
    path: &'a str,
}

#[derive(Deserialize)]
struct Response {
    id: Option<i64>,
    score: Option<f32>,
    error: Option<String>,
    ready: Option<bool>,
}

struct Job {
    path: PathBuf,
    reply: oneshot::Sender<Result<f32, String>>,
}

/// Handle to the classifier. Cheap to clone — every clone just shares the
/// same channel to the one supervised worker process.
#[derive(Clone)]
pub struct NsfwClassifier {
    tx: mpsc::Sender<Job>,
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    task: std::sync::Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl NsfwClassifier {
    /// Starts the supervisor task and returns immediately — spawning never
    /// fails outright. If the worker can't actually be started (Python
    /// missing, package missing, etc.), every `classify()` call will just
    /// return an error, same as if the feature were switched off.
    pub fn spawn(python_bin: String, worker_script: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel::<Job>(32);
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = tokio::spawn(supervisor_loop(
            python_bin,
            worker_script,
            rx,
            ready.clone(),
        ));
        Self { tx, ready, task: std::sync::Arc::new(tokio::sync::Mutex::new(Some(task))) }
    }

    pub async fn shutdown(&self) {
        if let Some(task) = self.task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        self.ready.store(false, Ordering::Release);
    }

    pub async fn classify(&self, path: PathBuf) -> anyhow::Result<f32> {
        anyhow::ensure!(path.is_file(), "classification source is unavailable");
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Job {
                path,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("nsfw worker is not running"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("nsfw worker dropped the request"))?
            .map_err(|e| anyhow::anyhow!(e))
    }
}

/// Owns the worker process's lifetime for as long as the app runs: starts
/// it, feeds it jobs one at a time, and restarts it if the pipe itself
/// breaks. Backs off between restart attempts so a permanently-missing
/// dependency doesn't spam the log every few milliseconds.
async fn supervisor_loop(
    python_bin: String,
    worker_script: PathBuf,
    mut rx: mpsc::Receiver<Job>,
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let mut consecutive_failures: u32 = 0;

    'restart: loop {
        ready.store(false, Ordering::Release);
        if rx.is_closed() {
            return;
        }
        let mut child = match crate::process::command(&python_bin)
            .arg(&worker_script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("nsfw worker: could not launch '{}': {}", python_bin, e);
                drain_with_error(&mut rx, "nsfw worker could not be launched");
                backoff(&mut consecutive_failures).await;
                continue 'restart;
            }
        };

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let mut stdin = stdin;
        let mut lines = BufReader::new(stdout).lines();

        // Forward the worker's stderr into our own log so a Python
        // traceback (missing dependency, etc.) is visible without needing
        // to run the worker by hand to debug it.
        tokio::spawn(async move {
            let mut err_lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = err_lines.next_line().await {
                warn!("nsfw worker stderr: {}", l);
            }
        });

        match tokio::time::timeout(std::time::Duration::from_secs(60), lines.next_line()).await {
            Ok(Ok(Some(line))) => match serde_json::from_str::<Response>(&line) {
                Ok(r) if r.ready == Some(true) => {
                    info!("nsfw worker ready");
                    consecutive_failures = 0;
                    ready.store(true, Ordering::Release);
                }
                Ok(r) => {
                    warn!(
                        "nsfw worker failed to start: {}",
                        r.error.unwrap_or_else(|| "unknown error".into())
                    );
                    let _ = child.kill().await;
                    drain_with_error(
                        &mut rx,
                        "nsfw worker failed to start (is opennsfw-onnx installed?)",
                    );
                    backoff(&mut consecutive_failures).await;
                    continue 'restart;
                }
                Err(e) => {
                    warn!("nsfw worker: unexpected startup line ({}): {}", e, line);
                    let _ = child.kill().await;
                    drain_with_error(&mut rx, "nsfw worker sent an unexpected startup response");
                    backoff(&mut consecutive_failures).await;
                    continue 'restart;
                }
            },
            _ => {
                warn!("nsfw worker exited before it was ready — is Python on PATH, and is opennsfw-onnx installed?");
                let _ = child.kill().await;
                drain_with_error(
                    &mut rx,
                    "nsfw worker exited on startup (missing Python or opennsfw-onnx?)",
                );
                backoff(&mut consecutive_failures).await;
                continue 'restart;
            }
        }

        // Serve jobs one at a time until the worker dies or the channel closes.
        loop {
            let job = match rx.recv().await {
                Some(j) => j,
                None => {
                    // Channel closed — app is shutting down.
                    let _ = child.kill().await;
                    return;
                }
            };

            if job.reply.is_closed() {
                continue;
            }
            if !job.path.is_file() {
                let _ = job
                    .reply
                    .send(Err("classification source is unavailable".into()));
                continue;
            }
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                run_one(&mut stdin, &mut lines, &job.path),
            )
            .await
            .unwrap_or_else(|_| Err("classification timed out".into()))
            {
                // Per-image outcome (score or a "this file failed" error) —
                // the worker itself is fine, just report it and move on.
                Ok(outcome) => {
                    let _ = job.reply.send(outcome);
                }
                // Transport/protocol failure — the worker is no longer
                // trustworthy, restart it. Fail this job too, obviously.
                Err(transport_err) => {
                    let _ = job.reply.send(Err(transport_err.clone()));
                    warn!("nsfw worker: {} — restarting", transport_err);
                    let _ = child.kill().await;
                    backoff(&mut consecutive_failures).await;
                    continue 'restart;
                }
            }
        }
    }
}

/// Sends one request and waits for its matching response.
///
/// Outer `Result` is transport/protocol-level (pipe broke, bad JSON, id
/// mismatch) — callers should restart the worker on `Err`. Inner `Result`
/// is this specific image's classification outcome and is always safe to
/// hand back to the caller without touching the worker.
async fn run_one(
    stdin: &mut ChildStdin,
    lines: &mut Lines<BufReader<ChildStdout>>,
    path: &Path,
) -> Result<Result<f32, String>, String> {
    static NEXT_ID: AtomicI64 = AtomicI64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

    let req = Request {
        id,
        path: &path.to_string_lossy(),
    };
    let mut line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    line.push('\n');

    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stdin.flush().await.map_err(|e| e.to_string())?;

    let resp_line = lines
        .next_line()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "nsfw worker closed its stdout".to_string())?;

    let resp: Response = serde_json::from_str(&resp_line).map_err(|e| e.to_string())?;
    if resp.id != Some(id) {
        return Err(format!(
            "nsfw worker response id mismatch (expected {}, got {:?})",
            id, resp.id
        ));
    }
    if let Some(err) = resp.error {
        return Ok(Err(err));
    }
    match resp.score {
        Some(s) => Ok(Ok(s)),
        None => Err("nsfw worker returned neither a score nor an error".to_string()),
    }
}

/// Fails every job currently waiting in the channel with `msg`, so callers
/// don't hang forever on a worker that isn't going to come up this attempt.
fn drain_with_error(rx: &mut mpsc::Receiver<Job>, msg: &str) {
    while let Ok(job) = rx.try_recv() {
        let _ = job.reply.send(Err(msg.to_string()));
    }
}

async fn backoff(consecutive_failures: &mut u32) {
    *consecutive_failures = consecutive_failures.saturating_add(1);
    // 5s, 10s, 15s ... capped at 2 minutes, so a permanently-missing
    // dependency settles into occasional retries rather than a busy loop.
    let secs = (*consecutive_failures).min(24) as u64 * 5;
    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
}

/// One bounded producer; persistent claims survive crashes and suppress duplicate work.
pub fn spawn_backfill_loop(
    pool: crate::db::DbPool,
    classifier: NsfwClassifier,
    library_dir: PathBuf,
) {
    tokio::spawn(async move {
        let mut last_warning = std::time::Instant::now() - std::time::Duration::from_secs(60);
        loop {
            if !classifier.ready.load(Ordering::Acquire) {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
            let rows = match fetch_unrated_batch(&pool, 25) {
                Ok(rows) => rows,
                Err(e) => {
                    warn!("NSFW backfill query failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    continue;
                }
            };
            if rows.is_empty() {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }
            for (id, filepath) in rows {
                let path = library_dir.join(&filepath);
                if !path.is_file() {
                    if let Ok(conn) = pool.get() {
                        let _ = crate::media_files::mark_missing(&conn, id);
                    }
                    continue;
                }
                let claimed = pool.get().ok().and_then(|conn| conn.execute(
                    "UPDATE media SET nsfw_state='working', nsfw_retry_at=unixepoch()+300, nsfw_attempts=nsfw_attempts+1
                     WHERE id=?1 AND downloaded=1 AND missing=0 AND nsfw_state IN ('pending','working')
                     AND nsfw_retry_at<=unixepoch()", [id]).ok()) == Some(1);
                if !claimed {
                    continue;
                }
                let result = classifier.classify(path.clone()).await;
                if let Ok(conn) = pool.get() {
                    match result {
                        Ok(score) => {
                            let _ = persist_score(&conn, id, score);
                        }
                        Err(e) => {
                            if !path.is_file() {
                                let _ = crate::media_files::mark_missing(&conn, id);
                                continue;
                            }
                            let _ = conn.execute("UPDATE media SET nsfw_state=CASE WHEN nsfw_attempts>=3 THEN 'failed' ELSE 'pending' END,
                                nsfw_retry_at=unixepoch()+3600 WHERE id=?1", [id]);
                            if last_warning.elapsed().as_secs() >= 60 {
                                warn!("NSFW classification failed for media {id}: {e}; retry delayed, at most 3 attempts per file version (similar failures suppressed for 60s)");
                                last_warning = std::time::Instant::now();
                            }
                        }
                    }
                }
            }
        }
    });
}

// Atomic with respect to concurrent human reviews.
pub(crate) fn persist_score(
    conn: &rusqlite::Connection,
    id: i64,
    score: f32,
) -> rusqlite::Result<usize> {
    if !score.is_finite() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    conn.execute(
        "UPDATE media SET auto_rating=?1, auto_rating_score=?2,
        rating=CASE WHEN rating_reviewed=0 THEN ?1 ELSE rating END,
        rating_source=CASE WHEN rating_reviewed=0 THEN 'auto' ELSE rating_source END,
        nsfw_state='done' WHERE id=?3 AND downloaded=1 AND missing=0",
        rusqlite::params![score_to_rating(score), score, id],
    )
}

/// Buckets a 0..1 NSFW probability into a 1-5 star rating: 1 = clothed/safe,
/// 3 = explicit, 5 = extremely lewd/vulgar — five equal-width bands across
/// the full [0,1] range.
fn score_to_rating(score: f32) -> i64 {
    let score = score.clamp(0.0, 1.0);
    (((score * 5.0).floor() as i64) + 1).clamp(1, 5)
}

fn fetch_unrated_batch(pool: &crate::db::DbPool, limit: i64) -> anyhow::Result<Vec<(i64, String)>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare("SELECT id, filepath FROM media WHERE missing=0 AND downloaded=1 AND type='image'
        AND nsfw_state IN ('pending','working') AND nsfw_retry_at<=unixepoch() AND nsfw_attempts<3 ORDER BY id LIMIT ?1")?;
    let rows = stmt
        .query_map([limit], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn queue_is_bounded_and_missing_file_never_enqueued() {
        let (tx, mut rx) = mpsc::channel(1);
        let classifier = NsfwClassifier {
            tx,
            task: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            ready: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        assert!(classifier
            .classify(PathBuf::from("nonexistent-classifier-test.jpg"))
            .await
            .is_err());
        assert!(rx.try_recv().is_err());
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.jpg");
        std::fs::write(&path, b"test").unwrap();
        let (reply, _) = oneshot::channel();
        classifier
            .tx
            .try_send(Job {
                path: path.clone(),
                reply,
            })
            .unwrap();
        let (reply, _) = oneshot::channel();
        assert!(classifier.tx.try_send(Job { path, reply }).is_err());
    }
    #[test]
    fn failed_and_claimed_rows_do_not_starve_pending_rows_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        crate::test_support::source(&state);
        let conn = state.pool.get().unwrap();
        conn.execute_batch("INSERT INTO media(id,source_id,filepath,filename,type,added_at,nsfw_state) VALUES(1,1,'a','a','image','2026','failed'),(2,1,'b','b','image','2026','pending');
            INSERT INTO media(id,source_id,filepath,filename,type,added_at,nsfw_state,nsfw_retry_at) VALUES(3,1,'c','c','image','2026','working',unixepoch()+300);").unwrap();
        assert_eq!(
            fetch_unrated_batch(&state.pool, 1).unwrap(),
            vec![(2, "b".into())]
        );
        conn.execute("UPDATE media SET nsfw_retry_at=0 WHERE id=3", [])
            .unwrap();
        assert_eq!(fetch_unrated_batch(&state.pool, 25).unwrap().len(), 2);
    }
}
