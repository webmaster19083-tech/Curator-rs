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
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

#[derive(Serialize)]
struct Request<'a> {
    id:   i64,
    path: &'a str,
}

#[derive(Deserialize)]
struct Response {
    id:    Option<i64>,
    score: Option<f32>,
    error: Option<String>,
    ready: Option<bool>,
}

struct Job {
    path:  PathBuf,
    reply: oneshot::Sender<Result<f32, String>>,
}

/// Handle to the classifier. Cheap to clone — every clone just shares the
/// same channel to the one supervised worker process.
#[derive(Clone)]
pub struct NsfwClassifier {
    tx: mpsc::UnboundedSender<Job>,
}

impl NsfwClassifier {
    /// Starts the supervisor task and returns immediately — spawning never
    /// fails outright. If the worker can't actually be started (Python
    /// missing, package missing, etc.), every `classify()` call will just
    /// return an error, same as if the feature were switched off.
    pub fn spawn(python_bin: String, worker_script: PathBuf) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<Job>();
        tokio::spawn(supervisor_loop(python_bin, worker_script, rx));
        Self { tx }
    }

    pub async fn classify(&self, path: PathBuf) -> anyhow::Result<f32> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Job { path, reply: reply_tx })
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
    mut rx: mpsc::UnboundedReceiver<Job>,
) {
    let mut consecutive_failures: u32 = 0;

    'restart: loop {
        let mut child = match Command::new(&python_bin)
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

        let stdin  = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let mut stdin  = stdin;
        let mut lines  = BufReader::new(stdout).lines();

        // Forward the worker's stderr into our own log so a Python
        // traceback (missing dependency, etc.) is visible without needing
        // to run the worker by hand to debug it.
        tokio::spawn(async move {
            let mut err_lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = err_lines.next_line().await {
                warn!("nsfw worker stderr: {}", l);
            }
        });

        match lines.next_line().await {
            Ok(Some(line)) => match serde_json::from_str::<Response>(&line) {
                Ok(r) if r.ready == Some(true) => {
                    info!("nsfw worker ready");
                    consecutive_failures = 0;
                }
                Ok(r) => {
                    warn!(
                        "nsfw worker failed to start: {}",
                        r.error.unwrap_or_else(|| "unknown error".into())
                    );
                    let _ = child.kill().await;
                    drain_with_error(&mut rx, "nsfw worker failed to start (is opennsfw-onnx installed?)");
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
                drain_with_error(&mut rx, "nsfw worker exited on startup (missing Python or opennsfw-onnx?)");
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

            match run_one(&mut stdin, &mut lines, &job.path).await {
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

    let req = Request { id, path: &path.to_string_lossy() };
    let mut line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    line.push('\n');

    stdin.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
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
fn drain_with_error(rx: &mut mpsc::UnboundedReceiver<Job>, msg: &str) {
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

/// Periodically finds unrated, downloaded images and runs them through the
/// classifier, writing the result straight into the existing `rating`
/// column (1 = clothed .. 5 = extremely explicit — see `score_to_rating`).
/// Only ever touches rows still at `rating=0`, so a rating you set yourself
/// — before or after this runs — is never overwritten. Runs for the life of
/// the app; returns (stopping the task) only if this instance is unused.
///
/// A permanently-broken file (corrupt/undecodable) is remembered in an
/// in-memory set for the rest of this run so it isn't retried on every pass
/// forever — same lesson as the thumbnail cache, but kept in memory rather
/// than in the DB since `rating=0` needs to keep meaning "not yet rated" and
/// not gain a second "tried and failed" meaning. It resets on restart,
/// which is fine: at worst a permanently-broken file gets one more retry.
pub fn spawn_backfill_loop(
    pool: crate::db::DbPool,
    classifier: NsfwClassifier,
    library_dir: std::path::PathBuf,
) {
    tokio::spawn(async move {
        let mut known_bad: std::collections::HashSet<i64> = std::collections::HashSet::new();

        loop {
            let batch = fetch_unrated_batch(&pool, &known_bad, 25);
            let rows = match batch {
                Ok(rows) => rows,
                Err(e) => {
                    warn!("nsfw backfill: query failed: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    continue;
                }
            };

            if rows.is_empty() {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }

            for (id, filepath) in rows {
                // filepath is stored relative to library_dir (see thumb.rs's
                // identical join) — passing it straight to the classifier
                // without this would have it opening a path relative to
                // whatever the process's CWD happens to be, not the library.
                let abs_path = library_dir.join(&filepath);
                let result = classifier.classify(abs_path).await;
                match result {
                    Ok(score) => {
                        let rating = score_to_rating(score);
                        if let Ok(conn) = pool.get() {
                            // Guard against a race with the user rating it
                            // themselves while this was in flight.
                            let _ = conn.execute(
                                "UPDATE media SET rating=?1 WHERE id=?2 AND rating=0",
                                rusqlite::params![rating, id],
                            );
                        } else {
                            warn!("nsfw backfill: could not get db connection");
                        }
                    }
                    Err(e) => {
                        warn!("nsfw classify failed for media {}: {} — skipping for this run", id, e);
                        known_bad.insert(id);
                    }
                }
            }
        }
    });
}

/// Buckets a 0..1 NSFW probability into a 1-5 star rating: 1 = clothed/safe,
/// 3 = explicit, 5 = extremely lewd/vulgar — five equal-width bands across
/// the full [0,1] range.
fn score_to_rating(score: f32) -> i64 {
    let score = score.clamp(0.0, 1.0);
    (((score * 5.0).floor() as i64) + 1).clamp(1, 5)
}

fn fetch_unrated_batch(
    pool: &crate::db::DbPool,
    known_bad: &std::collections::HashSet<i64>,
    limit: i64,
) -> anyhow::Result<Vec<(i64, String)>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare(
        "SELECT id, filepath FROM media
         WHERE rating=0 AND downloaded=1 AND type='image'
         ORDER BY id
         LIMIT ?1",
    )?;
    // Over-fetch a bit and filter known_bad in Rust — simplest way to skip
    // them without a dynamically-sized SQL IN(...) list every pass.
    let rows = stmt
        .query_map(rusqlite::params![limit + known_bad.len() as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .filter(|(id, _)| !known_bad.contains(id))
        .take(limit as usize)
        .collect();
    Ok(rows)
}

