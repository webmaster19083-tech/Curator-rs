use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use anyhow::Result;

fn default_max_concurrent() -> u32 { 3 }
fn default_slideshow_speed() -> f64 { 3000.0 }
fn default_false() -> bool { false }
fn default_theme() -> String { "dark".into() }
fn default_export_reminder_days() -> u32 { 30 }
fn default_ch_default_interval() -> f64 { 5.0 }
fn default_ch_default_limit() -> u32 { 200 }
fn default_true() -> bool { true }
fn default_ch_media_type() -> String { "image".into() }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    #[serde(default = "default_slideshow_speed")]
    pub default_slideshow_speed: f64,
    #[serde(default = "default_false")]
    pub default_slideshow_loop: bool,
    #[serde(default = "default_false")]
    pub default_slideshow_shuffle: bool,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_export_reminder_days")]
    pub export_reminder_days: u32,
    #[serde(default)]
    pub last_export_at: Option<String>,
    #[serde(default)]
    pub export_reminder_snoozed_until: Option<String>,
    // Cock hero feed settings
    #[serde(default = "default_false")]
    pub ch_log_sessions: bool,
    #[serde(default = "default_ch_default_interval")]
    pub ch_default_interval: f64,
    #[serde(default = "default_ch_default_limit")]
    pub ch_default_limit: u32,
    #[serde(default = "default_true")]
    pub ch_default_shuffle: bool,
    #[serde(default = "default_ch_media_type")]
    pub ch_default_media_type: String,
}

impl Default for Settings {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl Settings {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("settings.json")
    }

    pub fn load(data_dir: &Path) -> Result<Self> {
        let p = Self::path(data_dir);
        if !p.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&p)?;
        // Missing keys get defaults via serde(default)
        let s: Self = serde_json::from_str(&raw).unwrap_or_default();
        Ok(s)
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let p = Self::path(data_dir);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(p, json)?;
        Ok(())
    }
}
