//! `config.json` — the small, pre-database bootstrap file that decides where
//! everything else lives (`data_dir`) and which external executables Curator
//! shells out to (`gallery_dl_bin`, `python_bin`, `ffprobe_bin`).
//!
//! This file is deliberately separate from `db::Settings` (`settings.json`):
//! everything in here is read once, at process startup, before the database
//! or logging exist yet — `data_dir` in particular has to be known before
//! `Settings` can even be loaded. Changing a value here (via the OOBE
//! settings endpoint or by hand) takes effect on the *next* launch, same as
//! it always has — there is no in-process hot-reload for any of these
//! fields, so OOBE surfaces that plainly rather than pretending otherwise.
//!
//! Extracted out of `main.rs` (where this used to live as private items) so
//! `oobe.rs` can read and write the exact same file through the exact same
//! resolution rules instead of re-implementing them — see the OOBE build
//! notes: "Do not duplicate existing configuration logic."

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub data_dir:       Option<String>,
    pub gallery_dl_bin: Option<String>,
    pub python_bin: Option<String>,
    pub ffprobe_bin: Option<String>,
    /// ffmpeg is deliberately separate from ffprobe.  The latter is enough
    /// for the ordinary clips/videos split; sampling video frames and
    /// decoding a local soundtrack require the actual encoder binary.
    pub ffmpeg_bin: Option<String>,
    /// Optional path to a P-HAR-compatible temporal action model.  Leaving
    /// this unset keeps NudeNet image classification available and routes
    /// clips to manual review instead of repeatedly trying to load a model.
    pub action_model_path: Option<String>,
}

/// `config.json` always lives next to the running executable (not in
/// `data_dir` — it has to be readable before `data_dir` is even resolved).
pub fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config.json")))
        .unwrap_or_else(|| PathBuf::from("config.json"))
}

pub fn load_config() -> Config {
    let path = config_path();
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(cfg) = serde_json::from_str::<Config>(&text) {
            return cfg;
        }
    }
    Config::default()
}

/// Writes `cfg` to `config.json` next to the executable. Used both by the
/// first-launch bootstrap (`ensure_config_json`, which only ever writes
/// `data_dir` once) and by the OOBE settings endpoint (which merges in
/// whichever fields the person actually changed via `load_config` + mutate
/// + this).
pub fn save_config(cfg: &Config) -> std::io::Result<()> {
    let path = config_path();
    let text = serde_json::to_string_pretty(cfg)
        .map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}

pub fn resolve_data_dir(cfg: &Config) -> PathBuf {
    // 1. Environment variable
    if let Ok(env_val) = std::env::var("CURATOR_DATA_DIR") {
        if !env_val.is_empty() {
            return PathBuf::from(env_val);
        }
    }
    // 2. config.json data_dir
    if let Some(ref configured) = cfg.data_dir {
        if !configured.is_empty() {
            return PathBuf::from(configured);
        }
    }
    // 3. ~/Curator default
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Curator")
}

/// First-launch bootstrap: if `config.json` doesn't exist at all yet, seed
/// it with just the resolved `data_dir` so future runs (and diagnostics
/// like `tools/diagnose_nsfw.py`, which looks for this exact file) find the
/// same place without the person ever having to hand-edit JSON. Never
/// overwrites an existing file — OOBE's settings endpoint (`save_config`)
/// is the only thing that updates an already-present config.json.
pub fn ensure_config_json(data_dir: &std::path::Path) {
    let path = config_path();
    if !path.exists() {
        let content = serde_json::json!({ "data_dir": data_dir.to_string_lossy() });
        let _ = std::fs::write(&path, serde_json::to_string_pretty(&content).unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The process environment is global.  Keep these precedence tests from
    // racing when the suite is deliberately run with multiple test threads.
    static DATA_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn resolve_data_dir_prefers_env_var_over_config() {
        let _guard = DATA_DIR_ENV_LOCK.lock().unwrap();
        // Isolate from whatever the real environment/config might have —
        // this only asserts precedence, not the literal default path.
        std::env::set_var("CURATOR_DATA_DIR", "/tmp/curator-env-test-dir");
        let cfg = Config { data_dir: Some("/tmp/curator-config-test-dir".into()), ..Default::default() };
        let resolved = resolve_data_dir(&cfg);
        std::env::remove_var("CURATOR_DATA_DIR");
        assert_eq!(resolved, PathBuf::from("/tmp/curator-env-test-dir"));
    }

    #[test]
    fn resolve_data_dir_falls_back_to_home_curator() {
        let _guard = DATA_DIR_ENV_LOCK.lock().unwrap();
        std::env::remove_var("CURATOR_DATA_DIR");
        let cfg = Config::default();
        let resolved = resolve_data_dir(&cfg);
        assert!(resolved.ends_with("Curator"));
    }
}
