use crate::settings::Settings;
use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc, Mutex},
};
use tokio::sync::RwLock;

/// Effective tag set for a group_id: own tags ∪ ancestor tags ∪ ancestor names.
/// Rebuilt on any group or group_tag write; read lock held during list_media.
#[derive(Default, Clone)]
pub struct GroupTagCache {
    /// group_id → set of effective tag names
    pub map: HashMap<i64, HashSet<String>>,
}

pub struct AppState {
    pub pool: Pool<SqliteConnectionManager>,
    pub data_dir: PathBuf,
    pub group_tag_cache: Arc<RwLock<GroupTagCache>>,
    /// Mirrors Python's _downloads_paused. AtomicBool — no lock needed.
    pub downloads_paused: Arc<AtomicBool>,
    /// source_id → child PID (u32). Kill by PID via taskkill/kill.
    /// Stores PID only so the map is Send across await points.
    pub active_processes: Arc<Mutex<HashMap<i64, u32>>>,
    /// source_ids terminated via pause (used to detect deliberate stop vs real error)
    pub paused_source_ids: Arc<Mutex<HashSet<i64>>>,
    pub settings: Arc<RwLock<Settings>>,
}

impl AppState {
    pub fn new(pool: Pool<SqliteConnectionManager>, data_dir: PathBuf) -> Result<Self> {
        let settings = Settings::load(&data_dir)?;
        Ok(Self {
            pool,
            data_dir,
            group_tag_cache: Arc::new(RwLock::new(GroupTagCache::default())),
            downloads_paused: Arc::new(AtomicBool::new(false)),
            active_processes: Arc::new(Mutex::new(HashMap::new())),
            paused_source_ids: Arc::new(Mutex::new(HashSet::new())),
            settings: Arc::new(RwLock::new(settings)),
        })
    }
}
