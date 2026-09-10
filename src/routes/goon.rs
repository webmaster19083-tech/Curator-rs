//! GOON session planning and soundtrack metadata.
//!
//! The server returns one authoritative beat timeline.  The browser starts a
//! monotonic Web Audio clock after an explicit user interaction and derives
//! music, metronome, visuals, persona prompts, and local speech from that one
//! timeline instead of layering independent stage timers.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use axum::{extract::{Path, State}, http::StatusCode, Json};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::db::now_iso;
use crate::routes::media::{db_err, EFFECTIVE_RATING_SQL};
use crate::AppState;

const METER: u8 = 4;
const COUNT_IN_BEATS: u32 = 4;

#[derive(Debug, Clone, Serialize)]
pub struct SessionStage {
    pub id: String,
    pub title: String,
    pub pace: String,
    pub duration_s: f64,
    pub start_beat: u32,
    pub end_beat: u32,
    pub intensity: u8,
    pub media_rating: Option<u8>,
    pub prompt: String,
    pub event: String,
    pub transition: String,
    pub visual_change_beats: Vec<u32>,
    pub media: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaceStageInput {
    pub id: Option<String>,
    pub pace: String,
    pub beats: Option<u32>,
    pub duration_s: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SoundtrackInput {
    pub provider: Option<String>,
    pub playlist_id: Option<i64>,
    pub url: Option<String>,
    pub track_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct MetronomeInput {
    pub enabled: Option<bool>,
    pub volume: Option<f64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct StartSessionBody {
    #[serde(default)]
    pub media_ids: Vec<i64>,
    /// Legacy compatibility field. New sessions use exact pace stages rather
    /// than choosing the nearest arbitrary intensity.
    pub intensity: Option<u8>,
    #[serde(default)]
    pub soundtrack: SoundtrackInput,
    pub persona: Option<String>,
    pub bpm: Option<f64>,
    #[serde(alias = "offset", alias = "beat_offset")]
    pub beat_offset_secs: Option<f64>,
    #[serde(default)]
    pub metronome: MetronomeInput,
    #[serde(default, alias = "stages")]
    pub pace_stages: Vec<PaceStageInput>,
}

#[derive(Debug, Deserialize, Default)]
pub struct CompleteSessionBody {
    pub duration_s: Option<i64>,
    pub stages_completed: Option<i64>,
    pub ended_state: Option<String>,
    pub events: Option<Value>,
    pub soundtrack_provider: Option<String>,
    pub bpm: Option<f64>,
    pub beat_offset_secs: Option<f64>,
    pub timing_corrections: Option<Value>,
    pub rating_phases: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct PlaylistBody {
    pub name: String,
    pub provider: String,
    pub source_url: Option<String>,
    #[serde(default)]
    pub tracks: Value,
}

#[derive(Debug, Deserialize)]
pub struct BeatMapAnalyzeBody {
    pub track_path: String,
    pub playlist_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct BeatMapUpdateBody {
    pub bpm: f64,
    pub beat_offset_secs: f64,
    #[serde(default)]
    pub markers: Vec<f64>,
    #[serde(default)]
    pub confirmed: bool,
}

#[derive(Debug, Deserialize)]
pub struct OAuthCallbackBody {
    pub provider: String,
    pub code: Option<String>,
    pub state: Option<String>,
}

fn pace_rating(pace: &str) -> Option<u8> {
    match pace.trim().to_ascii_lowercase().as_str() {
        "slow" => Some(2),
        "medium" => Some(3),
        "fast" => Some(4),
        "cum" => Some(5),
        "succubus" => None,
        _ => Some(0),
    }
}

fn pace_title(pace: &str) -> &'static str {
    match pace {
        "slow" => "Slow",
        "medium" => "Medium",
        "fast" => "Fast",
        "cum" => "Cum",
        "succubus" => "Succubus",
        _ => "Stage",
    }
}

fn valid_persona(value: &str) -> bool {
    matches!(value, "neutral" | "mommy" | "dom" | "brat")
}

fn persona_prompt(persona: &str, pace: &str) -> &'static str {
    // Fixed stage-specific dialogue only.  No external model or generated
    // dialogue is involved, which keeps the TTS behavior predictable.
    match (persona, pace) {
        ("mommy", "slow") => "Settle into a gentle, steady rhythm.",
        ("mommy", "medium") => "Keep the rhythm calm and intentional.",
        ("mommy", "fast") => "Hold the tempo until the next cue.",
        ("mommy", "cum") => "Follow your own boundaries and finish when ready.",
        ("mommy", "succubus") => "Stay with the beat; this phase has no media.",
        ("dom", "slow") => "Begin on the count and keep the tempo controlled.",
        ("dom", "medium") => "Maintain the assigned rhythm.",
        ("dom", "fast") => "Do not rush ahead of the beat.",
        ("dom", "cum") => "Use the final counts deliberately.",
        ("dom", "succubus") => "Watch the beat map only; no media in this phase.",
        ("brat", "slow") => "Easy counts first. Do not skip the warm-up.",
        ("brat", "medium") => "Keep up with the meter, one beat at a time.",
        ("brat", "fast") => "The beat is in charge now.",
        ("brat", "cum") => "Use the last counts how you choose.",
        ("brat", "succubus") => "No shortcuts: follow the empty beat phase.",
        (_, "slow") => "Start slowly and follow the count-in.",
        (_, "medium") => "Keep a consistent beat-aligned pace.",
        (_, "fast") => "Keep the pace steady until the next marker.",
        (_, "cum") => "Use the final phase at your own pace.",
        (_, "succubus") => "Follow the rapid beat map; this phase deliberately has no media.",
        _ => "Follow the beat map.",
    }
}

fn default_stage_inputs() -> Vec<PaceStageInput> {
    [
        ("slow", 32_u32), ("medium", 48), ("fast", 48), ("succubus", 16), ("cum", 8),
    ].into_iter().map(|(pace, beats)| PaceStageInput { id: Some(pace.into()), pace: pace.into(), beats: Some(beats), duration_s: None }).collect()
}

fn build_stages(inputs: &[PaceStageInput], bpm: f64, persona: &str) -> Result<Vec<SessionStage>, String> {
    let mut cursor = COUNT_IN_BEATS;
    let mut stages = Vec::new();
    for (index, input) in inputs.iter().enumerate() {
        let pace = input.pace.trim().to_ascii_lowercase();
        // `None` is the intentional Succubus no-media phase.  A zero value
        // is only the sentinel returned for an unknown label.
        let media_rating = pace_rating(&pace);
        if media_rating == Some(0) { return Err(format!("Unknown pace stage: {}", input.pace)); }
        let beats = input.beats.or_else(|| input.duration_s.map(|seconds| (seconds * bpm / 60.0).round().max(1.0) as u32)).unwrap_or(16).clamp(1, 4_096);
        let start_beat = cursor;
        cursor = cursor.saturating_add(beats);
        let visual_change_beats = (start_beat..cursor).filter(|beat| (beat - start_beat) % METER as u32 == 0).collect();
        let is_last = index + 1 == inputs.len();
        stages.push(SessionStage {
            id: input.id.clone().filter(|value| !value.trim().is_empty()).unwrap_or_else(|| format!("{pace}-{index}")),
            title: pace_title(&pace).into(), pace: pace.clone(), duration_s: beats as f64 * 60.0 / bpm,
            start_beat, end_beat: cursor, intensity: media_rating.unwrap_or(0), media_rating,
            prompt: persona_prompt(persona, &pace).into(), event: pace.clone(),
            transition: if is_last { "end".into() } else { "next".into() },
            visual_change_beats, media: Vec::new(),
        });
    }
    if stages.is_empty() { return Err("At least one pace stage is required".into()); }
    Ok(stages)
}

fn validate_soundtrack(input: &SoundtrackInput, fallback: &str) -> Result<Value, String> {
    let provider = input.provider.as_deref().unwrap_or(fallback).trim().to_ascii_lowercase();
    if !matches!(provider.as_str(), "local" | "youtube" | "soundcloud" | "apple_music" | "spotify") {
        return Err("Unknown soundtrack provider".into());
    }
    let url = input.url.as_deref().map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned);
    if url.as_deref().is_some_and(|value| !(value.starts_with("https://") || value.starts_with("http://"))) {
        return Err("Soundtrack URL must be an http(s) URL".into());
    }
    Ok(json!({
        "provider":provider,"playlist_id":input.playlist_id,"url":url,"track_key":input.track_key,
        // Spotify can play independently but must never be used as a visual
        // synchronization driver under its playback policy.
        "beat_synchronization": provider != "spotify",
        "spotify_synchronization_disabled": provider == "spotify",
    }))
}

fn stage_query_params(media_ids: &[i64], max_clip_length: i64, limit: i64) -> (String, Vec<rusqlite::types::Value>) {
    let mut params: Vec<rusqlite::types::Value> = vec![max_clip_length.into()];
    let selected = if media_ids.is_empty() { String::new() } else {
        let placeholders = media_ids.iter().map(|id| { params.push((*id).into()); format!("?{}", params.len()) }).collect::<Vec<_>>().join(",");
        format!(" AND m.id IN ({placeholders})")
    };
    params.push(limit.into());
    (selected, params)
}

fn fetch_stage_media(state: &AppState, media_ids: &[i64], max_clip_length: i64, limit: i64) -> Result<(Vec<Value>, i64), (StatusCode, Json<Value>)> {
    let (selected_sql, params) = stage_query_params(media_ids, max_clip_length, limit);
    let skipped_sfw = if media_ids.is_empty() { 0 } else {
        let placeholders = media_ids.iter().enumerate().map(|(index, _)| format!("?{}", index + 1)).collect::<Vec<_>>().join(",");
        let conn = state.pool.get().map_err(db_err)?;
        conn.query_row(&format!("SELECT COUNT(*) FROM media m WHERE m.id IN ({placeholders}) AND {EFFECTIVE_RATING_SQL}=1"), rusqlite::params_from_iter(media_ids.iter()), |row| row.get(0)).unwrap_or(0)
    };
    let query = format!(
        "SELECT m.id,m.filepath,m.filename,m.type,{EFFECTIVE_RATING_SQL} AS rating,m.human_rating,m.auto_rating,m.action_rating,m.duration_secs,m.origin_url,
                (SELECT creator FROM source_metadata sm WHERE sm.media_id=m.id ORDER BY sm.id DESC LIMIT 1) AS creator
         FROM media m JOIN sources s ON s.id=m.source_id
         WHERE m.downloaded=1 AND m.missing=0 AND s.included=1
           AND {EFFECTIVE_RATING_SQL} BETWEEN 2 AND 5
           AND (m.clip_start_secs IS NULL OR EXISTS(SELECT 1 FROM media parent WHERE parent.id=m.clip_parent_id AND parent.downloaded=1 AND parent.missing=0))
           AND (m.type<>'video' OR m.duration_secs IS NULL OR m.duration_secs<=?1)
           {selected_sql} ORDER BY RANDOM() LIMIT ?{}", params.len()
    );
    let conn = state.pool.get().map_err(db_err)?;
    let mut statement = conn.prepare(&query).map_err(db_err)?;
    let values = statement.query_map(rusqlite::params_from_iter(params.iter()), |row| Ok(json!({
        "id":row.get::<_,i64>(0)?,"filepath":row.get::<_,String>(1)?,"filename":row.get::<_,String>(2)?,"type":row.get::<_,String>(3)?,
        "rating":row.get::<_,i64>(4)?,"pace_label":crate::nsfw::pace_label(row.get::<_,i64>(4)?),"human_rating":row.get::<_,Option<i64>>(5)?,"auto_rating":row.get::<_,i64>(6)?,"action_rating":row.get::<_,i64>(7)?,
        "duration_secs":row.get::<_,Option<f64>>(8)?,"origin_url":row.get::<_,Option<String>>(9)?,"creator":row.get::<_,Option<String>>(10)?,
    }))).map_err(db_err)?.filter_map(Result::ok).collect::<Vec<_>>();
    Ok((values, skipped_sfw))
}

/// POST /api/goon/session.  This only plans; an explicit Start gesture in the
/// browser is required to unlock Web Audio, embedded players, and local TTS.
pub async fn start(
    State(state): State<Arc<AppState>>,
    Json(body): Json<StartSessionBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if body.media_ids.len() > 5_000 { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"At most 5,000 selected media items can start one session"})))); }
    if body.media_ids.iter().any(|id| *id <= 0) { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Media IDs must be positive"})))); }
    let settings = state.settings.read().await;
    let persona = body.persona.as_deref().unwrap_or(&settings.goon_persona).trim().to_ascii_lowercase();
    if !valid_persona(&persona) { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Unknown GOON persona"})))); }
    let bpm = body.bpm.unwrap_or(120.0);
    if !bpm.is_finite() || !(40.0..=300.0).contains(&bpm) { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"BPM must be between 40 and 300"})))); }
    let beat_offset_secs = body.beat_offset_secs.unwrap_or(0.0);
    if !beat_offset_secs.is_finite() || !(-30.0..=30.0).contains(&beat_offset_secs) { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid beat offset"})))); }
    let soundtrack = validate_soundtrack(&body.soundtrack, &settings.soundtrack_provider)
        .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({"error":error}))))?;
    let metronome_enabled = body.metronome.enabled.unwrap_or(settings.metronome_enabled);
    let metronome_volume = body.metronome.volume.unwrap_or(settings.metronome_volume);
    if !metronome_volume.is_finite() { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid metronome volume"})))); }
    let max_clip_length = i64::from(settings.max_clip_length_secs);
    let limit = i64::from(settings.goon_default_limit.clamp(1, 5_000));
    drop(settings);
    let inputs = if body.pace_stages.is_empty() { default_stage_inputs() } else { body.pace_stages.clone() };
    let mut stages = build_stages(&inputs, bpm, &persona)
        .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({"error":error}))))?;
    let (mut candidates, skipped_sfw) = fetch_stage_media(&state, &body.media_ids, max_clip_length, limit)?;
    candidates.shuffle(&mut rand::thread_rng());
    let mut pools: HashMap<i64, Vec<Value>> = HashMap::new();
    for item in candidates {
        if let Some(rating) = item["rating"].as_i64() { pools.entry(rating).or_default().push(item); }
    }
    let mut flattened = Vec::new();
    let mut seen = HashSet::new();
    for stage in &mut stages {
        stage.media = stage.media_rating.map(|rating| pools.get(&(rating as i64)).cloned().unwrap_or_default()).unwrap_or_default();
        for item in &stage.media {
            if let Some(id) = item["id"].as_i64() { if seen.insert(id) { flattened.push(item.clone()); } }
        }
    }
    if let Some(id) = flattened.first().and_then(|item| item["id"].as_i64()) { state.remember_playback(id).await; }
    let total_beats = stages.last().map(|stage| stage.end_beat).unwrap_or(COUNT_IN_BEATS);
    let visual_markers = stages.iter().flat_map(|stage| stage.visual_change_beats.iter().map(|beat| json!({"beat":beat,"stage":stage.id}))).collect::<Vec<_>>();
    Ok(Json(json!({
        "mode":"goon","requires_explicit_start":true,"persona":persona,"soundtrack":soundtrack,
        "metronome":{"enabled":metronome_enabled,"volume":metronome_volume.clamp(0.0,1.0)},
        "timeline":{"bpm":bpm,"beat_offset_secs":beat_offset_secs,"meter":METER,"count_in_beats":COUNT_IN_BEATS,"total_beats":total_beats,"visual_markers":visual_markers},
        "stages":stages,"media":flattened,
        "selection":{"uses_effective_rating":true,"exact_stage_ratings":true,"sfw_excluded":true,"skipped_sfw_count":skipped_sfw,"long_videos_excluded":true,"missing_pools_do_not_substitute":true},
        "end_states":["cooldown","completed","cancelled"],
    })))
}

/// POST /api/goon/session/complete
pub async fn complete(State(state): State<Arc<AppState>>, Json(body): Json<CompleteSessionBody>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let ended_state = body.ended_state.unwrap_or_else(|| "completed".to_string());
    if !matches!(ended_state.as_str(), "cooldown" | "completed" | "cancelled") { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Unknown session end state"})))); }
    let duration_s = body.duration_s.unwrap_or(0).clamp(0, 86_400);
    let item_count = body.stages_completed.unwrap_or(0).clamp(0, 128);
    if !state.settings.read().await.goon_log_sessions { return Ok(Json(json!({"logged":false,"ended_state":ended_state}))); }
    let provider = body.soundtrack_provider.unwrap_or_else(|| "local".into());
    let bpm = body.bpm.filter(|value| value.is_finite()).unwrap_or(0.0);
    let offset = body.beat_offset_secs.filter(|value| value.is_finite()).unwrap_or(0.0);
    let conn = state.pool.get().map_err(db_err)?;
    conn.execute(
        "INSERT INTO interactive_sessions(started_at,duration_s,item_count,plan,events,ended_state,soundtrack_provider,bpm,beat_offset_secs,timing_corrections,rating_phases) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        rusqlite::params![now_iso(),duration_s,item_count,"goon-beat-v2",body.events.map(|value|value.to_string()),ended_state,provider,bpm,offset,body.timing_corrections.map(|value|value.to_string()),body.rating_phases.map(|value|value.to_string())],
    ).map_err(db_err)?;
    Ok(Json(json!({"logged":true,"id":conn.last_insert_rowid(),"ended_state":ended_state})))
}

pub async fn connector_status() -> Json<Value> {
    Json(json!({"connectors":[
        {"provider":"local","available":true,"authorization":"none","beat_sync":true},
        {"provider":"youtube","available":true,"authorization":"iframe_api","beat_sync":true},
        {"provider":"soundcloud","available":true,"authorization":"widget_api","beat_sync":true},
        {"provider":"apple_music","available":true,"authorization":"musickit","beat_sync":"manual_bpm_offset"},
        {"provider":"spotify","available":true,"authorization":"oauth_pkce","beat_sync":false,"reason":"Spotify playback is independent and never synchronizes visual media"}
    ]}))
}

pub async fn list_playlists(State(state): State<Arc<AppState>>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(db_err)?;
    let mut statement = conn.prepare("SELECT id,name,provider,source_url,tracks,added_at,updated_at FROM goon_playlists ORDER BY updated_at DESC,id DESC").map_err(db_err)?;
    let playlists = statement.query_map([], |row| Ok(json!({"id":row.get::<_,i64>(0)?,"name":row.get::<_,String>(1)?,"provider":row.get::<_,String>(2)?,"source_url":row.get::<_,Option<String>>(3)?,"tracks":serde_json::from_str::<Value>(&row.get::<_,String>(4)?).unwrap_or_else(|_|json!([])),"added_at":row.get::<_,String>(5)?,"updated_at":row.get::<_,String>(6)?}))).map_err(db_err)?.filter_map(Result::ok).collect::<Vec<_>>();
    Ok(Json(json!({"playlists":playlists})))
}

pub async fn save_playlist(State(state): State<Arc<AppState>>, Json(body): Json<PlaylistBody>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let name = body.name.trim();
    if name.is_empty() || name.len() > 160 { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Playlist name is required"})))); }
    let provider = body.provider.trim().to_ascii_lowercase();
    if !matches!(provider.as_str(), "local" | "youtube" | "soundcloud" | "apple_music" | "spotify") { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Unknown playlist provider"})))); }
    if !body.tracks.is_array() { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Playlist tracks must be an array"})))); }
    let conn = state.pool.get().map_err(db_err)?;
    conn.execute("INSERT INTO goon_playlists(name,provider,source_url,tracks,added_at,updated_at) VALUES(?1,?2,?3,?4,?5,?5)",rusqlite::params![name,provider,body.source_url,body.tracks.to_string(),now_iso()]).map_err(db_err)?;
    Ok(Json(json!({"id":conn.last_insert_rowid(),"status":"saved"})))
}

fn allowed_track_path(state: &AppState, raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    let canonical = dunce::canonicalize(&path).map_err(|_| "Track path is unavailable".to_string())?;
    let library = dunce::canonicalize(&state.library_dir).map_err(|_| "Library is unavailable".to_string())?;
    let data = dunce::canonicalize(&state.data_dir).map_err(|_| "Data directory is unavailable".to_string())?;
    if !canonical.starts_with(&library) && !canonical.starts_with(&data) { return Err("Track must be inside Curator's library or data directory".into()); }
    Ok(canonical)
}

pub async fn analyze_beat_map(State(state): State<Arc<AppState>>, Json(body): Json<BeatMapAnalyzeBody>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let track = allowed_track_path(&state, &body.track_path).map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({"error":error}))))?;
    let bin = state.ffmpeg_bin.clone();
    let analysis = tokio::task::spawn_blocking(move || crate::beat::analyze_local_audio(&bin, &track)).await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":"Beat analysis task failed"}))))?
        .map_err(|error| (StatusCode::BAD_REQUEST, Json(json!({"error":error}))))?;
    let track_key = body.track_path;
    let conn = state.pool.get().map_err(db_err)?;
    conn.execute("INSERT INTO beat_maps(playlist_id,track_key,bpm,beat_offset_secs,confidence,markers,confirmed,added_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,0,?7,?7) ON CONFLICT(playlist_id,track_key) DO UPDATE SET bpm=excluded.bpm,beat_offset_secs=excluded.beat_offset_secs,confidence=excluded.confidence,markers=excluded.markers,confirmed=0,updated_at=excluded.updated_at",rusqlite::params![body.playlist_id,track_key,analysis.bpm,analysis.first_beat_offset_secs,analysis.confidence,serde_json::to_string(&analysis.markers).unwrap_or_else(|_|"[]".into()),now_iso()]).map_err(db_err)?;
    Ok(Json(json!({"analysis":analysis,"id":conn.last_insert_rowid()})))
}

pub async fn update_beat_map(State(state): State<Arc<AppState>>, Path(id): Path<i64>, Json(body): Json<BeatMapUpdateBody>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !body.bpm.is_finite() || !(40.0..=300.0).contains(&body.bpm) || !body.beat_offset_secs.is_finite() { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid beat-map values"})))); }
    if body.markers.len() > 4_096 || body.markers.iter().any(|marker| !marker.is_finite() || *marker < 0.0) { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Invalid beat markers"})))); }
    let conn = state.pool.get().map_err(db_err)?;
    let changed=conn.execute("UPDATE beat_maps SET bpm=?1,beat_offset_secs=?2,markers=?3,confirmed=?4,updated_at=?5 WHERE id=?6",rusqlite::params![body.bpm,body.beat_offset_secs,serde_json::to_string(&body.markers).unwrap_or_else(|_|"[]".into()),body.confirmed,now_iso(),id]).map_err(db_err)?;
    if changed==0 { return Err((StatusCode::NOT_FOUND, Json(json!({"error":"Beat map not found"})))); }
    Ok(Json(json!({"id":id,"updated":true})))
}

/// OAuth callbacks intentionally retain no token in SQLite. Desktop builds
/// may attach an OS credential-store bridge; without one Curator reports the
/// safe session-only fallback rather than persisting a secret in a data file.
pub async fn oauth_callback(Json(body): Json<OAuthCallbackBody>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let provider = body.provider.trim().to_ascii_lowercase();
    if !matches!(provider.as_str(), "spotify" | "apple_music" | "youtube" | "soundcloud") { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Unknown OAuth provider"})))); }
    if body.code.as_deref().is_none_or(str::is_empty) { return Err((StatusCode::BAD_REQUEST, Json(json!({"error":"Authorization code is required"})))); }
    Ok(Json(json!({"provider":provider,"authorized":true,"credential_storage":"session_only","state":body.state})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;

    #[test]
    fn succubus_has_no_rating_or_media_and_regular_stages_are_exact() {
        let stages=build_stages(&default_stage_inputs(),120.0,"neutral").unwrap();
        assert_eq!(stages[0].media_rating,Some(2));
        assert_eq!(stages[1].media_rating,Some(3));
        assert_eq!(stages[2].media_rating,Some(4));
        assert_eq!(stages[3].pace,"succubus");
        assert_eq!(stages[3].media_rating,None);
        assert_eq!(stages[4].media_rating,Some(5));
    }

    #[tokio::test]
    async fn excludes_sfw_and_never_substitutes_a_stage_pool() {
        let root=tempfile::tempdir().unwrap(); let state=crate::test_support::state(root.path()); crate::test_support::source(&state);
        state.pool.get().unwrap().execute_batch("INSERT INTO media(id,source_id,filepath,filename,type,added_at,downloaded,human_rating,rating) VALUES(1,1,'one','one','image','2026',1,1,1),(2,1,'two','two','image','2026',1,3,3);").unwrap();
        let value=start(State(state),Json(StartSessionBody{media_ids:vec![1,2],..Default::default()})).await.unwrap().0;
        assert_eq!(value["selection"]["skipped_sfw_count"],1);
        assert_eq!(value["media"].as_array().unwrap().len(),1);
        assert!(value["stages"].as_array().unwrap().iter().any(|stage|stage["pace"]=="fast"&&stage["media"].as_array().unwrap().is_empty()));
        assert!(value["stages"].as_array().unwrap().iter().find(|stage|stage["pace"]=="succubus").unwrap()["media"].as_array().unwrap().is_empty());
    }
}
