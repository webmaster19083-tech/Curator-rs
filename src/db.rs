use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub type DbPool = Pool<SqliteConnectionManager>;

// ─── Settings ────────────────────────────────────────────────────────────────

fn default_max_concurrent()        -> u32   { 6 }
fn default_slideshow_speed()       -> f64   { 3000.0 }
fn default_slideshow_loop()        -> bool  { true }
fn default_slideshow_shuffle()     -> bool  { false }
fn default_theme()                 -> String { "system".into() }
fn default_export_reminder_days()  -> u32   { 30 }
fn default_ch_default_interval()   -> f64   { 5.0 }
fn default_ch_default_limit()      -> u32   { 200 }
fn default_ch_default_shuffle()    -> bool  { true }
fn default_ch_default_media_type() -> String { "image".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,

    #[serde(default = "default_slideshow_speed")]
    pub default_slideshow_speed: f64,

    #[serde(default = "default_slideshow_loop")]
    pub default_slideshow_loop: bool,

    #[serde(default = "default_slideshow_shuffle")]
    pub default_slideshow_shuffle: bool,

    #[serde(default = "default_theme")]
    pub theme: String,

    #[serde(default = "default_export_reminder_days")]
    pub export_reminder_days: u32,

    pub last_export_at: Option<String>,
    pub export_reminder_snoozed_until: Option<String>,

    // Tier 3 — Cock Hero settings
    #[serde(default)]
    pub ch_log_sessions: bool,

    #[serde(default = "default_ch_default_interval")]
    pub ch_default_interval: f64,

    #[serde(default = "default_ch_default_limit")]
    pub ch_default_limit: u32,

    #[serde(default = "default_ch_default_shuffle")]
    pub ch_default_shuffle: bool,

    #[serde(default = "default_ch_default_media_type")]
    pub ch_default_media_type: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_concurrent:                 default_max_concurrent(),
            default_slideshow_speed:        default_slideshow_speed(),
            default_slideshow_loop:         default_slideshow_loop(),
            default_slideshow_shuffle:      default_slideshow_shuffle(),
            theme:                          default_theme(),
            export_reminder_days:           default_export_reminder_days(),
            last_export_at:                 None,
            export_reminder_snoozed_until:  None,
            ch_log_sessions:                false,
            ch_default_interval:            default_ch_default_interval(),
            ch_default_limit:               default_ch_default_limit(),
            ch_default_shuffle:             default_ch_default_shuffle(),
            ch_default_media_type:          default_ch_default_media_type(),
        }
    }
}

pub fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.json")
}

pub fn load_settings(data_dir: &Path) -> Settings {
    let path = settings_path(data_dir);
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(s) = serde_json::from_str::<Settings>(&text) {
            return s;
        }
    }
    Settings::default()
}

pub fn save_settings(data_dir: &Path, settings: &Settings) {
    let path = settings_path(data_dir);
    if let Ok(text) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(&path, text);
    }
}

// ─── Pool init ────────────────────────────────────────────────────────────────

pub fn init_pool(data_dir: &Path) -> Result<DbPool> {
    let db_path = data_dir.join("data.db");
    let manager = SqliteConnectionManager::file(&db_path)
        .with_init(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA busy_timeout=5000;
                 PRAGMA foreign_keys=ON;"
            )
        });
    let pool = r2d2::Pool::builder()
        .max_size(8)
        .build(manager)
        .context("building SQLite connection pool")?;

    let migration_conn = pool.get().context("getting migration connection")?;
    run_migrations(&migration_conn)?;
    Ok(pool)
}

// ─── Migrations ───────────────────────────────────────────────────────────────

pub fn run_migrations(conn: &Connection) -> Result<()> {
    // ── groups ────────────────────────────────────────────────────────────────
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS groups (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            name      TEXT NOT NULL,
            parent_id INTEGER REFERENCES groups(id) ON DELETE SET NULL,
            added_at  TEXT NOT NULL
        );
    ")?;

    // ALTER TABLE additions for existing DBs
    let group_cols: HashSet<String> = column_names(conn, "groups");
    if !group_cols.contains("parent_id") {
        conn.execute_batch("ALTER TABLE groups ADD COLUMN parent_id INTEGER REFERENCES groups(id) ON DELETE SET NULL;")?;
    }

    // ── sources ───────────────────────────────────────────────────────────────
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS sources (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            name          TEXT NOT NULL,
            url           TEXT NOT NULL,
            slug          TEXT NOT NULL,
            status        TEXT NOT NULL DEFAULT 'pending',
            item_count    INTEGER NOT NULL DEFAULT 0,
            included      INTEGER NOT NULL DEFAULT 1,
            group_id      INTEGER REFERENCES groups(id) ON DELETE SET NULL,
            error_message TEXT,
            log           TEXT,
            added_at      TEXT NOT NULL,
            synced_at     TEXT
        );
    ")?;

    let src_cols: HashSet<String> = column_names(conn, "sources");
    if !src_cols.contains("group_id") {
        conn.execute_batch("ALTER TABLE sources ADD COLUMN group_id INTEGER REFERENCES groups(id) ON DELETE SET NULL;")?;
    }

    // ── media ─────────────────────────────────────────────────────────────────
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS media (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            source_id  INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
            filepath   TEXT NOT NULL UNIQUE,
            filename   TEXT NOT NULL,
            type       TEXT NOT NULL,
            added_at   TEXT NOT NULL,
            rating     INTEGER NOT NULL DEFAULT 0
        );
    ")?;

    let media_cols: HashSet<String> = column_names(conn, "media");
    if !media_cols.contains("rating") {
        conn.execute_batch("ALTER TABLE media ADD COLUMN rating INTEGER NOT NULL DEFAULT 0;")?;
    }
    if !media_cols.contains("origin_url") {
        conn.execute_batch("ALTER TABLE media ADD COLUMN origin_url TEXT;")?;
    }
    if !media_cols.contains("downloaded") {
        // DEFAULT 1 so all pre-existing rows are treated as real files
        conn.execute_batch("ALTER TABLE media ADD COLUMN downloaded INTEGER NOT NULL DEFAULT 1;")?;
    }

    // ── tags + junction tables ────────────────────────────────────────────────
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS tags (
            id       INTEGER PRIMARY KEY AUTOINCREMENT,
            name     TEXT NOT NULL UNIQUE,
            added_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS media_tags (
            media_id INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
            tag_id   INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
            PRIMARY KEY (media_id, tag_id)
        );
        CREATE TABLE IF NOT EXISTS group_tags (
            group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
            tag_id   INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
            PRIMARY KEY (group_id, tag_id)
        );
    ")?;

    // ── Cock Hero session log (Tier 3, opt-in) ────────────────────────────────
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS ch_sessions (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at  TEXT NOT NULL,
            duration_s  INTEGER NOT NULL,
            item_count  INTEGER NOT NULL,
            filters     TEXT,
            notes       TEXT
        );
    ")?;

    // ── Indexes ───────────────────────────────────────────────────────────────
    conn.execute_batch("
        CREATE INDEX IF NOT EXISTS idx_media_source    ON media(source_id);
        CREATE INDEX IF NOT EXISTS idx_media_rating    ON media(rating);
        CREATE INDEX IF NOT EXISTS idx_media_added_at  ON media(added_at);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_media_source_origin
            ON media(source_id, origin_url) WHERE origin_url IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_groups_parent   ON groups(parent_id);
        CREATE INDEX IF NOT EXISTS idx_sources_group   ON sources(group_id);
        CREATE INDEX IF NOT EXISTS idx_media_tags_tag  ON media_tags(tag_id);
        CREATE INDEX IF NOT EXISTS idx_group_tags_tag  ON group_tags(tag_id);
    ")?;

    // ── Self-healing slug repair migration ────────────────────────────────────
    // Detects sources named by the old pre-skip-list logic (which collapsed every
    // bunkr /a/<code> and every kemono /<service>/user/<id> into a single shared
    // name) and renames them to what the current derive_name_from_url would produce.
    //
    // This mutates data rather than schema, so it's gated behind the _migrations
    // tracking table below and only ever runs once per database — not on every
    // startup. The content-comparison guard inside repair_slug_names (only touch
    // a row if its current name exactly matches the old bug's output) stays in
    // place too, as defense in depth: even if _migrations were ever lost or
    // tampered with, a re-run still can't clobber a name the user deliberately
    // set themselves.
    ensure_migrations_table(conn)?;
    run_migration_once(conn, "0001_repair_bunkr_kemono_slugs", |c| {
        repair_slug_names(c);
        Ok(())
    })?;

    Ok(())
}

// ─── Migration tracking ────────────────────────────────────────────────────────

fn ensure_migrations_table(conn: &Connection) -> Result<()> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS _migrations (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT NOT NULL UNIQUE,
            applied_at TEXT NOT NULL
        );
    ")?;
    Ok(())
}

fn migration_applied(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM _migrations WHERE name=?1",
        params![name],
        |r| r.get::<_, i64>(0),
    ).unwrap_or(0) > 0
}

fn mark_migration_applied(conn: &Connection, name: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO _migrations (name, applied_at) VALUES (?1, ?2)",
        params![name, now_iso()],
    )?;
    Ok(())
}

/// Runs a one-shot, non-schema data migration exactly once per database,
/// tracked by name in `_migrations`. Schema changes (CREATE TABLE IF NOT
/// EXISTS, the column-presence ALTER TABLEs above) are naturally idempotent
/// and don't need this — it's specifically for migrations that mutate rows,
/// where re-running on every startup would be wasteful or, for a less
/// carefully-guarded migration than this one, actively unsafe.
fn run_migration_once(
    conn: &Connection,
    name: &str,
    body: impl FnOnce(&Connection) -> Result<()>,
) -> Result<()> {
    if migration_applied(conn, name) {
        return Ok(());
    }
    body(conn)?;
    mark_migration_applied(conn, name)?;
    info!("migration: applied {}", name);
    Ok(())
}

fn repair_slug_names(conn: &Connection) {
    struct Row { id: i64, name: String, url: String }
    let rows: Vec<Row> = {
        let mut stmt = match conn.prepare("SELECT id, name, url FROM sources") {
            Ok(s) => s,
            Err(e) => { warn!("repair_slug_names: prepare failed: {}", e); return; }
        };
        stmt.query_map([], |r| Ok(Row {
            id:   r.get(0)?,
            name: r.get(1)?,
            url:  r.get(2)?,
        }))
        .unwrap_or_else(|_| panic!("unreachable"))
        .filter_map(|r| r.ok())
        .collect()
    };

    for row in rows {
        let old_guess = old_buggy_name(&row.url);
        if row.name != old_guess {
            continue; // doesn't match old bug's output — leave it alone
        }
        let new_name = crate::slug::derive_name_from_url(&row.url);
        if new_name != row.name {
            let _ = conn.execute("UPDATE sources SET name=?1 WHERE id=?2", params![new_name, row.id]);
            info!("repair: renamed source {} from {:?} to {:?}", row.id, row.name, new_name);
        }
    }
}

/// Reconstructs what the pre-skip-list naming logic would have produced for a
/// given URL — used only by repair_slug_names to identify candidates for repair.
fn old_buggy_name(url: &str) -> String {
    // Manual URL parsing — no external url crate needed
    // Strip scheme: "https://host/path"
    let without_scheme = if let Some(idx) = url.find("://") {
        &url[idx + 3..]
    } else {
        url
    };

    // host = everything up to first '/'
    let (host_part, path_part) = if let Some(slash) = without_scheme.find('/') {
        (&without_scheme[..slash], &without_scheme[slash..])
    } else {
        (without_scheme, "")
    };

    let host = host_part.to_lowercase();
    let host = host.trim_start_matches("www.");
    let site = host.split('.').next().unwrap_or("site");

    let segments: Vec<&str> = path_part.split('/')
        .filter(|s| !s.is_empty())
        .collect();

    if segments.is_empty() {
        return host.to_string();
    }

    let handle = segments[0].trim_start_matches('@');
    if handle.is_empty() {
        return host.to_string();
    }
    format!("{} ({})", handle, site)
}

// ─── Schema helper ────────────────────────────────────────────────────────────

fn column_names(conn: &Connection, table: &str) -> HashSet<String> {
    let sql = format!("PRAGMA table_info({})", table);
    conn.prepare(&sql)
        .and_then(|mut stmt| {
            stmt.query_map([], |r| r.get::<_, String>(1))
                .map(|iter| iter.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default()
}

// ─── Time helper ─────────────────────────────────────────────────────────────

pub fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// ─── Group tag cache helpers ──────────────────────────────────────────────────

/// group_id → [itself, parent, grandparent, ...] up to root
pub fn build_group_ancestry_map(conn: &Connection) -> HashMap<i64, Vec<i64>> {
    struct G { id: i64, parent_id: Option<i64> }
    let rows: Vec<G> = {
        let mut stmt = conn.prepare("SELECT id, parent_id FROM groups").unwrap();
        stmt.query_map([], |r| Ok(G { id: r.get(0)?, parent_id: r.get(1)? }))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };

    let parents: HashMap<i64, Option<i64>> = rows.iter().map(|g| (g.id, g.parent_id)).collect();

    parents.keys().map(|&gid| {
        let mut chain = vec![gid];
        let mut seen = HashSet::from([gid]);
        let mut cur = parents[&gid];
        while let Some(pid) = cur {
            if seen.contains(&pid) { break; }
            chain.push(pid);
            seen.insert(pid);
            cur = parents.get(&pid).copied().flatten();
        }
        (gid, chain)
    }).collect()
}

/// group_id → set of tag names (group's own name + explicit tags + all ancestors' names+tags)
pub fn build_group_effective_tags_map(conn: &Connection) -> HashMap<i64, HashSet<String>> {
    let ancestry = build_group_ancestry_map(conn);

    let names: HashMap<i64, String> = {
        let mut stmt = conn.prepare("SELECT id, name FROM groups").unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .map(|(id, name)| (id, name.trim().to_lowercase()))
            .collect()
    };

    let mut own_tags: HashMap<i64, HashSet<String>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT gt.group_id, t.name FROM group_tags gt JOIN tags t ON t.id = gt.tag_id"
        ).unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .for_each(|(gid, tag)| { own_tags.entry(gid).or_default().insert(tag); });
    }

    ancestry.into_iter().map(|(gid, chain)| {
        let mut tags = HashSet::new();
        for ancestor_id in &chain {
            if let Some(name) = names.get(ancestor_id) {
                if !name.is_empty() { tags.insert(name.clone()); }
            }
            if let Some(t) = own_tags.get(ancestor_id) {
                tags.extend(t.iter().cloned());
            }
        }
        (gid, tags)
    }).collect()
}


