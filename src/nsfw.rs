//! Fail-soft local media classification.
//!
//! NudeNet is deliberately limited to anatomical exposure evidence and can
//! only produce SFW (1), Slow (2), or Medium (3). A separate optional P-HAR
//! worker may suggest Fast (4) for a short video after two consecutive
//! explicit-action windows. Neither worker can ever assign Cum (5).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

const NUDE_BATCH_SIZE: usize = 12;
const DETECTION_MIN_SCORE: f32 = 0.20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Detection {
    #[serde(alias = "class")]
    pub label: String,
    pub score: f32,
    #[serde(default, alias = "box", skip_serializing_if = "Option::is_none")]
    pub box_: Option<[i32; 4]>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NudeNetResult {
    #[serde(default = "default_nudenet_model")]
    pub model: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub score: f32,
    #[serde(default)]
    pub detections: Vec<Detection>,
}

fn default_nudenet_model() -> String { "NudeNet-320".to_string() }

#[derive(Serialize)]
struct NudeRequest<'a> { id: i64, paths: Vec<&'a str> }

#[derive(Deserialize)]
struct NudeResponse {
    id: Option<i64>,
    results: Option<Vec<NudeNetResult>>,
    // A stale worker lets us surface a useful upgrade error rather than
    // silently accepting the removed legacy score protocol.
    score: Option<f32>,
    error: Option<String>,
    ready: Option<bool>,
}

struct NudeJob {
    paths: Vec<PathBuf>,
    reply: oneshot::Sender<Result<Vec<NudeNetResult>, String>>,
}

/// Handle to the persistent NudeNet worker. A batch request maps naturally
/// onto NudeNet's `detect_batch` API while keeping the model warm.
#[derive(Clone)]
pub struct NsfwClassifier {
    tx: mpsc::Sender<NudeJob>,
    pub ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    task: std::sync::Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl NsfwClassifier {
    pub fn spawn(python_bin: String, worker_script: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel::<NudeJob>(32);
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = tokio::spawn(nude_supervisor_loop(python_bin, worker_script, rx, ready.clone()));
        Self { tx, ready, task: std::sync::Arc::new(tokio::sync::Mutex::new(Some(task))) }
    }

    pub async fn shutdown(&self) {
        if let Some(task) = self.task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
        self.ready.store(false, Ordering::Release);
    }

    pub async fn classify(&self, path: PathBuf) -> anyhow::Result<NudeNetResult> {
        let mut results = self.classify_batch(vec![path]).await?;
        results.pop().ok_or_else(|| anyhow::anyhow!("NudeNet worker returned no result"))
    }

    pub async fn classify_batch(&self, paths: Vec<PathBuf>) -> anyhow::Result<Vec<NudeNetResult>> {
        anyhow::ensure!(!paths.is_empty(), "classification batch is empty");
        anyhow::ensure!(paths.len() <= NUDE_BATCH_SIZE, "classification batch is too large");
        anyhow::ensure!(paths.iter().all(|path| path.is_file()), "classification source is unavailable");
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx.send(NudeJob { paths, reply: reply_tx }).await
            .map_err(|_| anyhow::anyhow!("NudeNet worker is not running"))?;
        reply_rx.await
            .map_err(|_| anyhow::anyhow!("NudeNet worker dropped the request"))?
            .map_err(|error| anyhow::anyhow!(error))
    }
}

async fn nude_supervisor_loop(
    python_bin: String,
    worker_script: PathBuf,
    mut rx: mpsc::Receiver<NudeJob>,
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let mut failures = 0_u32;
    'restart: loop {
        ready.store(false, Ordering::Release);
        if rx.is_closed() { return; }
        let mut child = match crate::process::command(&python_bin)
            .arg(&worker_script).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .kill_on_drop(true).spawn() {
            Ok(child) => child,
            Err(error) => {
                warn!("NudeNet worker could not launch '{}': {}", python_bin, error);
                drain_nude_jobs(&mut rx, "NudeNet worker could not be launched");
                backoff(&mut failures).await;
                continue;
            }
        };
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let mut lines = BufReader::new(stdout).lines();
        tokio::spawn(async move {
            let mut stderr_lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = stderr_lines.next_line().await { warn!("NudeNet worker stderr: {line}"); }
        });
        match wait_nude_ready(&mut lines).await {
            Ok(()) => { failures = 0; ready.store(true, Ordering::Release); info!("NudeNet classifier worker ready"); }
            Err(error) => {
                warn!("NudeNet worker failed to start: {error}");
                let _ = child.kill().await;
                drain_nude_jobs(&mut rx, "NudeNet worker unavailable (install nudenet and its model dependencies)");
                backoff(&mut failures).await;
                continue;
            }
        }
        loop {
            let Some(job) = rx.recv().await else { let _ = child.kill().await; return; };
            if job.reply.is_closed() { continue; }
            if job.paths.iter().any(|path| !path.is_file()) {
                let _ = job.reply.send(Err("classification source is unavailable".into()));
                continue;
            }
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(45), run_nude_batch(&mut stdin, &mut lines, &job.paths))
                .await.unwrap_or_else(|_| Err("NudeNet classification timed out".to_string()));
            match outcome {
                Ok(result) => { let _ = job.reply.send(result); }
                Err(error) => {
                    let _ = job.reply.send(Err(error.clone()));
                    warn!("NudeNet worker protocol failed: {error}; restarting");
                    let _ = child.kill().await;
                    backoff(&mut failures).await;
                    continue 'restart;
                }
            }
        }
    }
}

async fn wait_nude_ready(lines: &mut Lines<BufReader<ChildStdout>>) -> Result<(), String> {
    let line = tokio::time::timeout(std::time::Duration::from_secs(60), lines.next_line()).await
        .map_err(|_| "startup timed out".to_string())?
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "worker closed stdout during startup".to_string())?;
    let response: NudeResponse = serde_json::from_str(&line).map_err(|error| error.to_string())?;
    if response.ready == Some(true) { Ok(()) }
    else { Err(response.error.unwrap_or_else(|| "unknown startup error".to_string())) }
}

async fn run_nude_batch(
    stdin: &mut ChildStdin,
    lines: &mut Lines<BufReader<ChildStdout>>,
    paths: &[PathBuf],
) -> Result<Result<Vec<NudeNetResult>, String>, String> {
    static NEXT_ID: AtomicI64 = AtomicI64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path_strings = paths.iter().map(|path| path.to_string_lossy().to_string()).collect::<Vec<_>>();
    let request = NudeRequest { id, paths: path_strings.iter().map(String::as_str).collect() };
    let mut payload = serde_json::to_string(&request).map_err(|error| error.to_string())?;
    payload.push('\n');
    stdin.write_all(payload.as_bytes()).await.map_err(|error| error.to_string())?;
    stdin.flush().await.map_err(|error| error.to_string())?;
    let line = lines.next_line().await.map_err(|error| error.to_string())?
        .ok_or_else(|| "NudeNet worker closed stdout".to_string())?;
    let response: NudeResponse = serde_json::from_str(&line).map_err(|error| error.to_string())?;
    if response.id != Some(id) { return Err(format!("NudeNet response id mismatch (expected {id}, got {:?})", response.id)); }
    if let Some(error) = response.error { return Ok(Err(error)); }
    if let Some(results) = response.results {
        if results.len() != paths.len() { return Err(format!("NudeNet returned {} results for {} paths", results.len(), paths.len())); }
        return Ok(Ok(results));
    }
    if response.score.is_some() { return Err("stale legacy score-worker protocol detected; restart Curator".to_string()); }
    Err("NudeNet returned neither results nor an error".to_string())
}

fn drain_nude_jobs(rx: &mut mpsc::Receiver<NudeJob>, message: &str) {
    while let Ok(job) = rx.try_recv() { let _ = job.reply.send(Err(message.to_string())); }
}

async fn backoff(failures: &mut u32) {
    *failures = failures.saturating_add(1);
    tokio::time::sleep(std::time::Duration::from_secs((u64::from(*failures).min(24)) * 5)).await;
}

// ── Optional temporal action worker ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionWindow { pub start_secs: f64, pub end_secs: f64, pub label: String, pub score: f32 }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionResult {
    #[serde(default = "default_phar_model")]
    pub model: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub windows: Vec<ActionWindow>,
}

fn default_phar_model() -> String { "P-HAR".to_string() }

#[derive(Serialize)]
struct ActionRequest<'a> { id: i64, path: &'a str, windows: &'a [(f64, f64)] }

#[derive(Deserialize)]
struct ActionResponse { id: Option<i64>, result: Option<ActionResult>, error: Option<String>, ready: Option<bool> }

struct ActionJob { path: PathBuf, windows: Vec<(f64, f64)>, reply: oneshot::Sender<Result<ActionResult, String>> }

/// Independently supervised because an unavailable/heavy action model must
/// never hold up NudeNet's much lighter image pipeline.
#[derive(Clone)]
pub struct ActionClassifier {
    tx: mpsc::Sender<ActionJob>,
    pub ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    task: std::sync::Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl ActionClassifier {
    pub fn spawn(python_bin: String, worker_script: PathBuf, model_path: String) -> Self {
        let (tx, rx) = mpsc::channel::<ActionJob>(8);
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = tokio::spawn(action_supervisor_loop(python_bin, worker_script, model_path, rx, ready.clone()));
        Self { tx, ready, task: std::sync::Arc::new(tokio::sync::Mutex::new(Some(task))) }
    }

    pub async fn shutdown(&self) {
        if let Some(task) = self.task.lock().await.take() { task.abort(); let _ = task.await; }
        self.ready.store(false, Ordering::Release);
    }

    pub async fn classify(&self, path: PathBuf, windows: Vec<(f64, f64)>) -> anyhow::Result<ActionResult> {
        anyhow::ensure!(path.is_file(), "action-classification source is unavailable");
        anyhow::ensure!(!windows.is_empty(), "action-classification windows are empty");
        let (reply, receive) = oneshot::channel();
        self.tx.send(ActionJob { path, windows, reply }).await
            .map_err(|_| anyhow::anyhow!("P-HAR worker is not running"))?;
        receive.await.map_err(|_| anyhow::anyhow!("P-HAR worker dropped the request"))?
            .map_err(|error| anyhow::anyhow!(error))
    }
}

async fn action_supervisor_loop(
    python_bin: String,
    worker_script: PathBuf,
    model_path: String,
    mut rx: mpsc::Receiver<ActionJob>,
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let mut failures = 0_u32;
    'restart: loop {
        ready.store(false, Ordering::Release);
        let mut child = match crate::process::command(&python_bin)
            .arg(&worker_script).arg("--model").arg(&model_path)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .kill_on_drop(true).spawn() {
            Ok(child) => child,
            Err(error) => {
                warn!("P-HAR worker could not launch: {error}");
                drain_action_jobs(&mut rx, "P-HAR worker could not be launched");
                backoff(&mut failures).await;
                continue;
            }
        };
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let mut lines = BufReader::new(stdout).lines();
        tokio::spawn(async move {
            let mut stderr_lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = stderr_lines.next_line().await { warn!("P-HAR worker stderr: {line}"); }
        });
        let ready_line = tokio::time::timeout(std::time::Duration::from_secs(60), lines.next_line()).await;
        let started = ready_line.ok().and_then(Result::ok).flatten()
            .and_then(|line| serde_json::from_str::<ActionResponse>(&line).ok())
            .filter(|response| response.ready == Some(true));
        if started.is_none() {
            let _ = child.kill().await;
            drain_action_jobs(&mut rx, "P-HAR model unavailable; clip needs manual review");
            backoff(&mut failures).await;
            continue;
        }
        failures = 0;
        ready.store(true, Ordering::Release);
        info!("P-HAR action worker ready");
        loop {
            let Some(job) = rx.recv().await else { let _ = child.kill().await; return; };
            if !job.path.is_file() { let _ = job.reply.send(Err("action-classification source is unavailable".into())); continue; }
            match tokio::time::timeout(std::time::Duration::from_secs(90), run_action(&mut stdin, &mut lines, &job.path, &job.windows)).await
                .unwrap_or_else(|_| Err("P-HAR analysis timed out".to_string())) {
                Ok(result) => { let _ = job.reply.send(result); }
                Err(error) => {
                    let _ = job.reply.send(Err(error.clone()));
                    warn!("P-HAR worker protocol failed: {error}; restarting");
                    let _ = child.kill().await;
                    backoff(&mut failures).await;
                    continue 'restart;
                }
            }
        }
    }
}

async fn run_action(
    stdin: &mut ChildStdin,
    lines: &mut Lines<BufReader<ChildStdout>>,
    path: &Path,
    windows: &[(f64, f64)],
) -> Result<Result<ActionResult, String>, String> {
    static NEXT_ID: AtomicI64 = AtomicI64::new(1_000_000);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let request = ActionRequest { id, path: &path.to_string_lossy(), windows };
    let mut payload = serde_json::to_string(&request).map_err(|error| error.to_string())?;
    payload.push('\n');
    stdin.write_all(payload.as_bytes()).await.map_err(|error| error.to_string())?;
    stdin.flush().await.map_err(|error| error.to_string())?;
    let line = lines.next_line().await.map_err(|error| error.to_string())?
        .ok_or_else(|| "P-HAR worker closed stdout".to_string())?;
    let response: ActionResponse = serde_json::from_str(&line).map_err(|error| error.to_string())?;
    if response.id != Some(id) { return Err(format!("P-HAR response id mismatch (expected {id}, got {:?})", response.id)); }
    if let Some(error) = response.error { return Ok(Err(error)); }
    response.result.ok_or_else(|| "P-HAR returned neither result nor error".to_string()).map(Ok)
}

fn drain_action_jobs(rx: &mut mpsc::Receiver<ActionJob>, message: &str) {
    while let Ok(job) = rx.try_recv() { let _ = job.reply.send(Err(message.to_string())); }
}

// ── Rating policy and persistence ──────────────────────────────────────────

fn normalized_label(label: &str) -> String { label.trim().to_ascii_uppercase().replace(['-', ' '], "_") }

fn exposed_label(label: &str) -> bool {
    matches!(normalized_label(label).as_str(),
        "FEMALE_BREAST_EXPOSED" | "BUTTOCKS_EXPOSED" |
        "FEMALE_GENITALIA_EXPOSED" | "MALE_GENITALIA_EXPOSED" |
        "ANUS_EXPOSED" | "MALE_BREAST_EXPOSED")
}

fn slow_label(label: &str) -> bool {
    matches!(normalized_label(label).as_str(),
        "FEMALE_BREAST_COVERED" | "BUTTOCKS_COVERED" |
        "FEMALE_GENITALIA_COVERED" | "MALE_GENITALIA_COVERED" |
        "BELLY_EXPOSED" | "ARMPITS_EXPOSED")
}

/// NudeNet evidence has no activity or climax signal. A confidence value is
/// only evidence for a qualifying anatomical label; it never promotes to 4
/// or 5.
pub fn nudenet_rating(detections: &[Detection]) -> i64 {
    let mut qualifying = detections.iter().filter(|d| d.score.is_finite() && d.score >= DETECTION_MIN_SCORE);
    if qualifying.clone().any(|d| exposed_label(&d.label)) { 3 }
    else if qualifying.any(|d| slow_label(&d.label)) { 2 }
    else { 1 }
}

pub fn pace_label(rating: i64) -> &'static str {
    match rating { 1 => "sfw", 2 => "slow", 3 => "medium", 4 => "fast", 5 => "cum", _ => "unrated" }
}

fn compact_detections(detections: &[Detection]) -> Value {
    json!(detections.iter().filter(|d| d.score.is_finite()).take(16).map(|d| {
        let mut value = json!({"label":normalized_label(&d.label),"score":(f64::from(d.score)*1000.0).round()/1000.0});
        if let Some(box_) = d.box_ { value["box"] = json!(box_); }
        value
    }).collect::<Vec<_>>())
}

/// Qualifying P-HAR class must appear in two adjacent temporal windows at
/// >= .70. Idle/nonsexual/transition labels are explicitly insufficient.
pub fn phar_fast_suggestion(windows: &[ActionWindow]) -> Option<f32> {
    let mut previous_end: Option<f64> = None;
    let mut previous_score = 0.0_f32;
    for window in windows {
        let label = window.label.to_ascii_lowercase();
        let excluded = label.contains("nonsexual") || label.contains("non-sexual") || label.contains("idle") || label.contains("transition");
        let explicit = ["sex", "masturb", "intercourse", "oral", "fellatio", "handjob", "penetrat", "explicit"]
            .iter().any(|needle| label.contains(needle));
        let qualified = !excluded && explicit && window.score.is_finite() && window.score >= 0.70;
        let adjacent = previous_end.map(|end| window.start_secs <= end + 0.75).unwrap_or(false);
        if qualified && previous_score >= 0.70 && adjacent { return Some(previous_score.min(window.score)); }
        if qualified { previous_end = Some(window.end_secs); previous_score = window.score; }
        else { previous_end = None; previous_score = 0.0; }
    }
    None
}

pub(crate) fn persist_nudenet(conn: &rusqlite::Connection, id: i64, result: &NudeNetResult, evidence: Value) -> rusqlite::Result<usize> {
    let rating = nudenet_rating(&result.detections);
    let score = result.score.clamp(0.0, 1.0);
    conn.execute(
        "UPDATE media SET auto_rating=?1,auto_rating_score=?2,classifier_model=?3,classifier_version=?4,classifier_score=?2,classifier_evidence=?5,classification_label=?6,manual_review_required=0,manual_review_reason=NULL,rating=CASE WHEN human_rating IS NULL AND action_rating=0 THEN ?1 ELSE COALESCE(human_rating,NULLIF(action_rating,0),rating) END,rating_source=CASE WHEN human_rating IS NOT NULL THEN 'human' WHEN action_rating=4 THEN 'auto_action' ELSE 'auto' END,rating_reviewed=CASE WHEN human_rating IS NULL THEN 0 ELSE 1 END,nsfw_state='done',classification_updated_at=datetime('now') WHERE id=?7 AND downloaded=1 AND missing=0",
        rusqlite::params![rating,score,result.model,result.version,evidence.to_string(),pace_label(rating),id],
    )
}

fn persist_video_nudenet(conn: &rusqlite::Connection, id: i64, results: &[NudeNetResult]) -> rusqlite::Result<usize> {
    let rating = results.iter().map(|result| nudenet_rating(&result.detections)).max().unwrap_or(1);
    let score = results.iter().map(|result| result.score).filter(|score| score.is_finite()).fold(0.0_f32, f32::max).clamp(0.0, 1.0);
    let model = results.first().map(|result| result.model.clone()).unwrap_or_else(default_nudenet_model);
    let version = results.first().map(|result| result.version.clone()).unwrap_or_default();
    let evidence = json!(results.iter().enumerate().map(|(index, result)| json!({"frame":index,"score":(f64::from(result.score)*1000.0).round()/1000.0,"detections":compact_detections(&result.detections)})).collect::<Vec<_>>());
    conn.execute(
        "UPDATE media SET auto_rating=?1,auto_rating_score=?2,classifier_model=?3,classifier_version=?4,classifier_score=?2,classifier_evidence=?5,classification_label=?6,rating=CASE WHEN human_rating IS NULL AND action_rating=0 THEN ?1 ELSE COALESCE(human_rating,NULLIF(action_rating,0),rating) END,rating_source=CASE WHEN human_rating IS NOT NULL THEN 'human' WHEN action_rating=4 THEN 'auto_action' ELSE 'auto' END,rating_reviewed=CASE WHEN human_rating IS NULL THEN 0 ELSE 1 END,nsfw_state='done',classification_updated_at=datetime('now') WHERE id=?7 AND downloaded=1 AND missing=0",
        rusqlite::params![rating,score,model,version,evidence.to_string(),pace_label(rating),id],
    )
}

fn persist_action(conn: &rusqlite::Connection, id: i64, result: &ActionResult) -> rusqlite::Result<usize> {
    let fast = phar_fast_suggestion(&result.windows);
    let evidence = json!(result.windows.iter().take(48).map(|window| json!({"start_secs":window.start_secs,"end_secs":window.end_secs,"label":window.label,"score":(f64::from(window.score)*1000.0).round()/1000.0})).collect::<Vec<_>>());
    conn.execute(
        "UPDATE media SET action_model=?1,action_model_version=?2,action_score=?3,action_evidence=?4,action_rating=?5,classification_label=CASE WHEN ?5=4 THEN 'fast' ELSE classification_label END,rating=CASE WHEN human_rating IS NULL AND ?5=4 THEN 4 ELSE COALESCE(human_rating,NULLIF(action_rating,0),NULLIF(auto_rating,0),0) END,rating_source=CASE WHEN human_rating IS NOT NULL THEN 'human' WHEN ?5=4 THEN 'auto_action' WHEN auto_rating>0 THEN 'auto' ELSE 'none' END,rating_reviewed=CASE WHEN human_rating IS NULL THEN 0 ELSE 1 END,nsfw_state='done',classification_updated_at=datetime('now') WHERE id=?6 AND downloaded=1 AND missing=0",
        rusqlite::params![result.model,result.version,fast,evidence.to_string(),if fast.is_some(){4}else{0},id],
    )
}

fn mark_manual_review(conn: &rusqlite::Connection, id: i64, reason: &str) {
    let _ = conn.execute(
        "UPDATE media SET manual_review_required=1,manual_review_reason=?1,nsfw_state='manual_review',classification_label=CASE WHEN classification_label='unclassified' THEN 'manual_review' ELSE classification_label END,classification_updated_at=datetime('now'),nsfw_retry_at=9223372036854775807 WHERE id=?2 AND downloaded=1 AND missing=0",
        rusqlite::params![reason,id],
    );
}

/// Compatibility helper for old in-tree tests. It is intentionally capped
/// at Medium, unlike the old score buckets.
#[cfg(test)]
pub(crate) fn persist_score(conn: &rusqlite::Connection, id: i64, score: f32) -> rusqlite::Result<usize> {
    let detection = if score >= 0.66 { Detection { label: "FEMALE_BREAST_EXPOSED".into(), score, box_: None } }
        else if score >= 0.33 { Detection { label: "FEMALE_BREAST_COVERED".into(), score, box_: None } }
        else { Detection { label: "NONE".into(), score, box_: None } };
    let result = NudeNetResult { model: default_nudenet_model(), version: "compat".into(), score, detections: vec![detection.clone()] };
    persist_nudenet(conn, id, &result, compact_detections(&[detection]))
}

#[derive(Debug, Clone)]
struct PendingMedia { id: i64, filepath: String, kind: String, duration_secs: Option<f64> }

fn fetch_pending_batch(pool: &crate::db::DbPool, limit: i64) -> anyhow::Result<Vec<PendingMedia>> {
    let conn = pool.get()?;
    let mut statement = conn.prepare("SELECT id,filepath,type,duration_secs FROM media WHERE missing=0 AND downloaded=1 AND nsfw_state IN ('pending','working') AND nsfw_retry_at<=unixepoch() AND nsfw_attempts<3 ORDER BY CASE WHEN manual_review_required=1 THEN 0 ELSE 1 END,id LIMIT ?1")?;
    let rows = statement.query_map([limit], |row| Ok(PendingMedia { id:row.get(0)?, filepath:row.get(1)?, kind:row.get(2)?, duration_secs:row.get(3)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn claim(pool: &crate::db::DbPool, id: i64) -> bool {
    pool.get().ok().and_then(|conn| conn.execute("UPDATE media SET nsfw_state='working',nsfw_retry_at=unixepoch()+300,nsfw_attempts=nsfw_attempts+1 WHERE id=?1 AND downloaded=1 AND missing=0 AND nsfw_state IN ('pending','working') AND nsfw_retry_at<=unixepoch()",[id]).ok()) == Some(1)
}

fn frame_times(duration: f64) -> Vec<f64> {
    (0..6).map(|index| { let fraction=(f64::from(index)+0.5)/6.0; (duration*fraction).clamp(0.05,(duration-0.05).max(0.05)) }).collect()
}

fn action_windows(duration: f64) -> Vec<(f64, f64)> {
    let width=duration.min(4.0).max(1.0); let stride=(width/2.0).max(0.5); let mut output=Vec::new(); let mut start=0.0;
    while start<duration { output.push((start,(start+width).min(duration))); if start+width>=duration { break; } start+=stride; }
    output
}

fn sample_video_frames(ffmpeg_bin: &str, source: &Path, duration: f64) -> Result<(tempfile::TempDir, Vec<PathBuf>), String> {
    if !source.is_file() { return Err("video file is unavailable".to_string()); }
    let directory=tempfile::tempdir().map_err(|error| error.to_string())?; let mut paths=Vec::new();
    for (index,seconds) in frame_times(duration).into_iter().enumerate() {
        let output=directory.path().join(format!("frame-{index}.jpg"));
        let status=std::process::Command::new(ffmpeg_bin).args(["-hide_banner","-loglevel","error","-ss",&format!("{seconds:.3}"),"-i"]).arg(source).args(["-frames:v","1","-vf","scale=320:320:force_original_aspect_ratio=decrease","-y"]).arg(&output).status().map_err(|error| format!("ffmpeg is unavailable: {error}"))?;
        if !status.success() || !output.is_file() { return Err("ffmpeg could not sample this clip".to_string()); }
        paths.push(output);
    }
    Ok((directory,paths))
}

/// Starts one bounded producer. A permanently missing action model never
/// creates a retry storm: clips are marked for manual review once while image
/// work continues through the independent NudeNet worker.
pub fn spawn_backfill_loop(
    pool: crate::db::DbPool,
    classifier: NsfwClassifier,
    action_classifier: Option<ActionClassifier>,
    library_dir: PathBuf,
    ffmpeg_bin: String,
    max_clip_length_secs: u32,
) {
    tokio::spawn(async move {
        let mut last_warning=std::time::Instant::now()-std::time::Duration::from_secs(60);
        loop {
            if !classifier.ready.load(Ordering::Acquire) { tokio::time::sleep(std::time::Duration::from_secs(5)).await; continue; }
            let rows=match fetch_pending_batch(&pool,12) { Ok(rows)=>rows, Err(error)=>{warn!("classification queue query failed: {error}");tokio::time::sleep(std::time::Duration::from_secs(30)).await;continue;} };
            if rows.is_empty() { tokio::time::sleep(std::time::Duration::from_secs(30)).await; continue; }
            for row in rows {
                let source=library_dir.join(&row.filepath);
                if !source.is_file() { if let Ok(conn)=pool.get(){let _=crate::media_files::mark_missing(&conn,row.id);} continue; }
                if !claim(&pool,row.id) { continue; }
                if row.kind=="video" {
                    let Some(duration)=row.duration_secs.filter(|value| value.is_finite()&&*value>0.0) else { if let Ok(conn)=pool.get(){mark_manual_review(&conn,row.id,"Video duration is unknown; review manually");} continue; };
                    if duration>f64::from(max_clip_length_secs) { if let Ok(conn)=pool.get(){mark_manual_review(&conn,row.id,"Video exceeds the automatic clip-length limit");} continue; }
                    let sampled=tokio::task::spawn_blocking({let bin=ffmpeg_bin.clone();let source=source.clone();move||sample_video_frames(&bin,&source,duration)}).await.unwrap_or_else(|_|Err("ffmpeg frame sampling task failed".to_string()));
                    let (_temporary,frames)=match sampled { Ok(value)=>value, Err(reason)=>{if let Ok(conn)=pool.get(){mark_manual_review(&conn,row.id,&reason);}continue;} };
                    match classifier.classify_batch(frames).await {
                        Ok(results)=>{
                            if let Ok(conn)=pool.get(){let _=persist_video_nudenet(&conn,row.id,&results);}
                            match action_classifier.as_ref() {
                                Some(action) if action.ready.load(Ordering::Acquire)=>match action.classify(source.clone(),action_windows(duration)).await { Ok(result)=>if let Ok(conn)=pool.get(){let _=persist_action(&conn,row.id,&result);}, Err(error)=>if let Ok(conn)=pool.get(){mark_manual_review(&conn,row.id,&format!("P-HAR unavailable: {error}"));}, },
                                _=>if let Ok(conn)=pool.get(){mark_manual_review(&conn,row.id,"P-HAR action model is unavailable; review Fast manually");},
                            }
                        }
                        Err(error)=>if let Ok(conn)=pool.get(){mark_manual_review(&conn,row.id,&format!("NudeNet could not read sampled clip: {error}"));},
                    }
                } else {
                    match classifier.classify(source).await {
                        Ok(result)=>if let Ok(conn)=pool.get(){let evidence=compact_detections(&result.detections);let _=persist_nudenet(&conn,row.id,&result,evidence);},
                        Err(error)=>{
                            if let Ok(conn)=pool.get(){let _=conn.execute("UPDATE media SET nsfw_state=CASE WHEN nsfw_attempts>=3 THEN 'failed' ELSE 'pending' END,nsfw_retry_at=unixepoch()+3600 WHERE id=?1",[row.id]);}
                            if last_warning.elapsed().as_secs()>=60 {warn!("NudeNet classification failed for media {}: {}; retry delayed",row.id,error);last_warning=std::time::Instant::now();}
                        }
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nudenet_never_promotes_anatomy_to_fast_or_cum() {
        assert_eq!(nudenet_rating(&[]),1);
        assert_eq!(nudenet_rating(&[Detection{label:"BELLY_EXPOSED".into(),score:0.99,box_:None}]),2);
        assert_eq!(nudenet_rating(&[Detection{label:"FEMALE_BREAST_EXPOSED".into(),score:0.999,box_:None}]),3);
    }
    #[test]
    fn phar_requires_two_consecutive_explicit_windows() {
        let one=vec![ActionWindow{start_secs:0.0,end_secs:2.0,label:"explicit_sex".into(),score:0.9}];assert_eq!(phar_fast_suggestion(&one),None);
        let pair=vec![ActionWindow{start_secs:0.0,end_secs:2.0,label:"explicit_sex".into(),score:0.9},ActionWindow{start_secs:1.0,end_secs:3.0,label:"explicit_sex".into(),score:0.8}];assert_eq!(phar_fast_suggestion(&pair),Some(0.8));
        let idle=vec![ActionWindow{start_secs:0.0,end_secs:2.0,label:"idle".into(),score:1.0},ActionWindow{start_secs:1.0,end_secs:3.0,label:"idle".into(),score:1.0}];assert_eq!(phar_fast_suggestion(&idle),None);
    }
    #[test]
    fn video_sampling_plan_is_six_frames_and_overlapping_windows() { assert_eq!(frame_times(12.0).len(),6);let windows=action_windows(10.0);assert!(windows.windows(2).all(|pair|pair[1].0<pair[0].1)); }

    #[test]
    fn persisted_action_suggestion_is_fast_only_and_never_cum() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::run_migrations(&conn).unwrap();
        conn.execute("INSERT INTO sources(id,name,url,slug,added_at) VALUES(1,'test','test','test','2026')", []).unwrap();
        conn.execute("INSERT INTO media(id,source_id,filepath,filename,type,added_at,downloaded) VALUES(1,1,'a','a','video','2026',1)", []).unwrap();
        let result = ActionResult { model: "P-HAR".into(), version: "fixture".into(), windows: vec![
            ActionWindow { start_secs: 0.0, end_secs: 2.0, label: "explicit_sex".into(), score: 0.9 },
            ActionWindow { start_secs: 1.0, end_secs: 3.0, label: "explicit_sex".into(), score: 0.8 },
        ]};
        persist_action(&conn, 1, &result).unwrap();
        let row: (i64, i64, String) = conn.query_row("SELECT action_rating,auto_rating,classification_label FROM media WHERE id=1", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!(row, (4, 0, "fast".into()));
    }
}
