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

fn default_max_concurrent() -> u32 {
    6
}
fn default_slideshow_speed() -> f64 {
    3000.0
}
fn default_slideshow_loop() -> bool {
    true
}
fn default_slideshow_shuffle() -> bool {
    false
}
fn default_theme() -> String {
    "system".into()
}
fn default_export_reminder_days() -> u32 {
    30
}
fn default_goon_default_interval() -> f64 {
    5.0
}
fn default_goon_default_limit() -> u32 {
    200
}
fn default_goon_default_shuffle() -> bool {
    true
}
fn default_goon_default_media_type() -> String {
    "image".into()
}
fn default_max_clip_length_secs() -> u32 {
    60
}
fn default_last_play_mode() -> String {
    "slideshow".into()
}
fn default_keep_running_in_tray() -> bool {
    true
}

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

    // Curator-native interactive-session defaults. `alias` keeps existing
    // settings files readable while new writes shed the legacy feature name.
    #[serde(default, alias = "ch_log_sessions")]
    pub goon_log_sessions: bool,

    #[serde(
        default = "default_goon_default_interval",
        alias = "ch_default_interval"
    )]
    pub goon_default_interval: f64,

    #[serde(default = "default_goon_default_limit", alias = "ch_default_limit")]
    pub goon_default_limit: u32,

    #[serde(default = "default_goon_default_shuffle", alias = "ch_default_shuffle")]
    pub goon_default_shuffle: bool,

    #[serde(
        default = "default_goon_default_media_type",
        alias = "ch_default_media_type"
    )]
    pub goon_default_media_type: String,

    /// A video at or below this duration is classified as a short clip in
    /// views and short-form workflows. The source file itself is never
    /// changed when this value moves.
    #[serde(default = "default_max_clip_length_secs")]
    pub max_clip_length_secs: u32,

    /// The primary Play button reopens the last mode the person used. Keep
    /// this in the same durable settings document as the other play defaults
    /// so it survives both desktop and headless restarts.
    #[serde(default = "default_last_play_mode")]
    pub last_play_mode: String,

    /// Per-user Windows Run registration. The actual registry write happens
    /// through the shared Settings/OOBE path, not in the frontend.
    #[serde(default)]
    pub start_with_windows: bool,

    /// Closing the main window normally hides it and leaves the background
    /// server, downloads, and tray controls alive.
    #[serde(default = "default_keep_running_in_tray")]
    pub keep_running_in_tray: bool,

    // NSFW auto-rating (opt-in, requires the Python worker's dependencies
    // to be installed — see nsfw_worker.py). Enabling/disabling takes effect
    // on next restart, since it decides whether the worker process gets
    // started at all. Recommendations and raw scores are retained separately;
    // automation changes the effective rating only until a human reviews it.
    #[serde(default)]
    pub nsfw_filter_enabled: bool,

    // First-run OOBE (out-of-box setup wizard — see oobe.rs). `false` here
    // means "show the wizard instead of the normal UI". This field alone is
    // NOT the whole story for whether an existing installation gets forced
    // through it — see `load_settings` below and `oobe::existing_installation_has_data`
    // for the self-healing logic that protects upgraders.
    #[serde(default)]
    pub oobe_completed: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_concurrent: default_max_concurrent(),
            default_slideshow_speed: default_slideshow_speed(),
            default_slideshow_loop: default_slideshow_loop(),
            default_slideshow_shuffle: default_slideshow_shuffle(),
            theme: default_theme(),
            export_reminder_days: default_export_reminder_days(),
            last_export_at: None,
            export_reminder_snoozed_until: None,
            goon_log_sessions: false,
            goon_default_interval: default_goon_default_interval(),
            goon_default_limit: default_goon_default_limit(),
            goon_default_shuffle: default_goon_default_shuffle(),
            goon_default_media_type: default_goon_default_media_type(),
            max_clip_length_secs: default_max_clip_length_secs(),
            last_play_mode: default_last_play_mode(),
            start_with_windows: false,
            keep_running_in_tray: default_keep_running_in_tray(),
            nsfw_filter_enabled: false,
            // A brand new Settings::default() (no settings.json on disk at
            // all) means a genuinely fresh install — OOBE should run. See
            // load_settings for how an *existing* settings.json that
            // predates this field is handled differently.
            oobe_completed: false,
        }
    }
}

pub fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.json")
}

pub fn load_settings(data_dir: &Path) -> Settings {
    let path = settings_path(data_dir);
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(mut s) = serde_json::from_str::<Settings>(&text) {
            // Self-heal: a settings.json that exists on disk but predates
            // the `oobe_completed` field belongs to an installation that
            // was already set up and running before OOBE existed in this
            // codebase. `#[serde(default)]` would otherwise silently give
            // it `false` and force a real, already-configured user through
            // first-run setup — which the spec explicitly forbids. Detect
            // that case by checking the raw JSON (not just the parsed
            // struct, since `false` is indistinguishable from "missing"
            // once deserialized) and repair the file once, the same way
            // db::run_migrations self-heals older databases.
            if !text.contains("\"oobe_completed\"") {
                s.oobe_completed = true;
                save_settings(data_dir, &s);
            }
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
    let manager = SqliteConnectionManager::file(&db_path).with_init(|conn| {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
                 PRAGMA busy_timeout=5000;
                 PRAGMA foreign_keys=ON;",
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
    let tx = conn.unchecked_transaction()?;
    let conn = &*tx;
    // ── groups ────────────────────────────────────────────────────────────────
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS groups (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            name      TEXT NOT NULL,
            parent_id INTEGER REFERENCES groups(id) ON DELETE SET NULL,
            added_at  TEXT NOT NULL
        );
    ",
    )?;

    // ALTER TABLE additions for existing DBs
    let group_cols: HashSet<String> = column_names(conn, "groups");
    if !group_cols.contains("parent_id") {
        conn.execute_batch("ALTER TABLE groups ADD COLUMN parent_id INTEGER REFERENCES groups(id) ON DELETE SET NULL;")?;
    }

    // ── sources ───────────────────────────────────────────────────────────────
    conn.execute_batch(
        "
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
    ",
    )?;

    let src_cols: HashSet<String> = column_names(conn, "sources");
    if !src_cols.contains("group_id") {
        conn.execute_batch("ALTER TABLE sources ADD COLUMN group_id INTEGER REFERENCES groups(id) ON DELETE SET NULL;")?;
    }
    // Retry scheduling belongs to a source, rather than to an in-memory
    // task, so a background process can safely recover its queue after a
    // desktop-window or machine restart.
    if !src_cols.contains("retry_attempts") {
        conn.execute_batch(
            "ALTER TABLE sources ADD COLUMN retry_attempts INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    if !src_cols.contains("retry_at") {
        conn.execute_batch("ALTER TABLE sources ADD COLUMN retry_at INTEGER NOT NULL DEFAULT 0;")?;
    }

    // ── media ─────────────────────────────────────────────────────────────────
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS media (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            source_id  INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
            filepath   TEXT NOT NULL UNIQUE,
            filename   TEXT NOT NULL,
            type       TEXT NOT NULL,
            added_at   TEXT NOT NULL,
            rating     INTEGER NOT NULL DEFAULT 0
        );
    ",
    )?;

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
    if !media_cols.contains("duration_secs") {
        // NULL = not a video, or a video whose duration isn't known yet
        // (ffprobe not installed, or not yet backfilled — see duration.rs).
        conn.execute_batch("ALTER TABLE media ADD COLUMN duration_secs REAL;")?;
    }

    for (name, definition) in [
        ("file_size_bytes", "INTEGER CHECK(file_size_bytes >= 0)"),
        ("clip_start_secs", "REAL CHECK(clip_start_secs >= 0)"),
        (
            "clip_end_secs",
            "REAL CHECK(clip_end_secs > clip_start_secs)",
        ),
        ("missing", "INTEGER NOT NULL DEFAULT 0"),
        ("file_stamp", "TEXT"),
        // Timestamps are retained independently: importing/reindexing a
        // file must not rewrite its original download date just because its
        // metadata was refreshed.
        ("downloaded_at", "TEXT"),
        ("modified_at", "TEXT"),
        ("nsfw_state", "TEXT NOT NULL DEFAULT 'pending'"),
        ("nsfw_attempts", "INTEGER NOT NULL DEFAULT 0"),
        ("nsfw_retry_at", "INTEGER NOT NULL DEFAULT 0"),
        ("duration_attempted", "INTEGER NOT NULL DEFAULT 0"),
        (
            "clip_parent_id",
            "INTEGER REFERENCES media(id) ON DELETE SET NULL",
        ),
        ("auto_rating", "INTEGER NOT NULL DEFAULT 0"),
        ("auto_rating_score", "REAL"),
        // NULL means that no human has reviewed this item. Zero clears a
        // prior human decision, so persisted human ratings are 1..5 only.
        (
            "human_rating",
            "INTEGER CHECK(human_rating BETWEEN 1 AND 5)",
        ),
        ("rating_source", "TEXT NOT NULL DEFAULT 'none'"),
        ("rating_reviewed", "INTEGER NOT NULL DEFAULT 0"),
        ("rating_reviewed_at", "TEXT"),
    ] {
        if !media_cols.contains(name) {
            conn.execute_batch(&format!(
                "ALTER TABLE media ADD COLUMN {name} {definition};"
            ))?;
        }
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_groups_parent ON groups(parent_id);
        CREATE INDEX IF NOT EXISTS idx_sources_group ON sources(group_id);
        CREATE INDEX IF NOT EXISTS idx_sources_retry ON sources(retry_at, retry_attempts);
        CREATE INDEX IF NOT EXISTS idx_media_size ON media(file_size_bytes);",
    )?;
    // Backfill upgraded databases with a stable approximation. New imports
    // use the actual completed-download and filesystem-modified timestamps.
    conn.execute_batch(
        "UPDATE media SET downloaded_at=COALESCE(downloaded_at,added_at) WHERE downloaded=1;
         UPDATE media SET modified_at=COALESCE(modified_at,added_at);",
    )?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS clip_jobs (
        id INTEGER PRIMARY KEY AUTOINCREMENT, media_id INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
        seconds INTEGER NOT NULL, status TEXT NOT NULL, clip_count INTEGER NOT NULL DEFAULT 0,
        error TEXT, added_at TEXT NOT NULL);
        UPDATE clip_jobs SET status='failed',error='Interrupted by restart; original preserved' WHERE status='running';")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_media_review ON media(rating_reviewed, auto_rating, id);",
    )?;
    if !column_names(conn, "clip_jobs").contains("virtual") {
        conn.execute_batch("ALTER TABLE clip_jobs ADD COLUMN virtual INTEGER NOT NULL DEFAULT 0;")?;
    }
    conn.execute_batch("CREATE TABLE IF NOT EXISTS placeholder_scans (
        source_id INTEGER PRIMARY KEY REFERENCES sources(id) ON DELETE CASCADE,
        url TEXT NOT NULL, retry_at INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_media_nsfw ON media(nsfw_state, downloaded, rating, type, nsfw_retry_at, id);
        CREATE INDEX IF NOT EXISTS idx_media_probe ON media(duration_attempted, downloaded, type, id);
        CREATE INDEX IF NOT EXISTS idx_media_filename ON media(filename COLLATE NOCASE, id);
        CREATE INDEX IF NOT EXISTS idx_media_rating_id ON media(rating DESC, id ASC);
        CREATE TRIGGER IF NOT EXISTS media_count_insert AFTER INSERT ON media WHEN NEW.downloaded=1 BEGIN
          UPDATE sources SET item_count=item_count+1 WHERE id=NEW.source_id;
        END;
        CREATE TRIGGER IF NOT EXISTS media_count_delete AFTER DELETE ON media WHEN OLD.downloaded=1 BEGIN
          UPDATE sources SET item_count=MAX(0,item_count-1) WHERE id=OLD.source_id;
        END;
        CREATE TRIGGER IF NOT EXISTS media_count_update AFTER UPDATE OF downloaded,source_id ON media
        WHEN OLD.downloaded<>NEW.downloaded OR OLD.source_id<>NEW.source_id BEGIN
          UPDATE sources SET item_count=MAX(0,item_count-OLD.downloaded) WHERE id=OLD.source_id;
          UPDATE sources SET item_count=item_count+NEW.downloaded WHERE id=NEW.source_id;
        END;")?;

    // ── tags + junction tables ────────────────────────────────────────────────
    conn.execute_batch(
        "
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
        -- A source may still have a default group, while individual media can
        -- belong to additional organization groups without moving files.
        CREATE TABLE IF NOT EXISTS media_groups (
            media_id INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
            group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
            added_at TEXT NOT NULL,
            PRIMARY KEY (media_id, group_id)
        );
        CREATE INDEX IF NOT EXISTS idx_media_groups_group ON media_groups(group_id, media_id);
    ",
    )?;

    // Raw metadata is intentionally separate from the curated tag relation:
    // source metadata is evidence, not a user-visible decision. The review
    // table lets an installation retain it even when a tag is skipped.
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS source_metadata (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            media_id    INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
            provider    TEXT NOT NULL DEFAULT '',
            source_url  TEXT,
            raw_json    TEXT NOT NULL,
            creator     TEXT,
            title       TEXT,
            captured_at TEXT NOT NULL,
            UNIQUE(media_id, provider, source_url)
        );
        CREATE TABLE IF NOT EXISTS source_tags (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            media_id            INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
            source_metadata_id  INTEGER REFERENCES source_metadata(id) ON DELETE SET NULL,
            provider            TEXT NOT NULL DEFAULT '',
            raw_name            TEXT NOT NULL,
            normalized_name     TEXT,
            state               TEXT NOT NULL DEFAULT 'pending'
                                CHECK(state IN ('pending','approved','skipped')),
            reviewed_at         TEXT,
            added_at            TEXT NOT NULL,
            UNIQUE(media_id, provider, raw_name)
        );
        CREATE TABLE IF NOT EXISTS media_tag_provenance (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            media_id      INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
            tag_id        INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
            source_tag_id INTEGER REFERENCES source_tags(id) ON DELETE SET NULL,
            provenance    TEXT NOT NULL CHECK(provenance IN ('human','human_edited','source_approved','automatic','legacy')),
            added_at      TEXT NOT NULL,
            UNIQUE(media_id, tag_id, provenance, source_tag_id)
        );
        CREATE TABLE IF NOT EXISTS source_tag_rules (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            -- Empty string is the global scope. Avoid NULL here because
            -- SQLite considers NULL values distinct in a UNIQUE constraint.
            provider        TEXT NOT NULL DEFAULT '',
            raw_name        TEXT NOT NULL COLLATE NOCASE,
            action          TEXT NOT NULL CHECK(action IN ('add','normalize','skip')),
            normalized_name TEXT,
            added_at        TEXT NOT NULL,
            updated_at      TEXT NOT NULL,
            UNIQUE(provider, raw_name)
        );
        CREATE INDEX IF NOT EXISTS idx_source_metadata_media ON source_metadata(media_id);
        CREATE INDEX IF NOT EXISTS idx_source_tags_review ON source_tags(state, provider, id);
        CREATE INDEX IF NOT EXISTS idx_source_tags_media ON source_tags(media_id);
        CREATE INDEX IF NOT EXISTS idx_media_tag_provenance_media ON media_tag_provenance(media_id, tag_id);
        CREATE INDEX IF NOT EXISTS idx_source_tag_rules_lookup ON source_tag_rules(provider, raw_name);
        ",
    )?;

    // `last_used_at` is only a presentation aid for quick tag buttons. It is
    // nullable for existing tags, which preserves their original timestamps.
    let tag_cols: HashSet<String> = column_names(conn, "tags");
    if !tag_cols.contains("last_used_at") {
        conn.execute_batch("ALTER TABLE tags ADD COLUMN last_used_at TEXT;")?;
    }

    // ── Curator interactive-session log (opt-in) ─────────────────────────────
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS interactive_sessions (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at  TEXT NOT NULL,
            duration_s  INTEGER NOT NULL,
            item_count  INTEGER NOT NULL,
            plan        TEXT,
            events      TEXT,
            ended_state TEXT NOT NULL DEFAULT 'completed'
        );
    ",
    )?;

    // ── Indexes ───────────────────────────────────────────────────────────────
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_media_source    ON media(source_id);
        CREATE INDEX IF NOT EXISTS idx_media_rating    ON media(rating);
        CREATE INDEX IF NOT EXISTS idx_media_effective_rating ON media(human_rating, auto_rating, id);
        CREATE INDEX IF NOT EXISTS idx_media_added_at  ON media(added_at);
        CREATE INDEX IF NOT EXISTS idx_media_duration  ON media(duration_secs);
        CREATE INDEX IF NOT EXISTS idx_media_downloaded_at ON media(downloaded_at);
        CREATE INDEX IF NOT EXISTS idx_media_modified_at ON media(modified_at);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_media_source_origin
            ON media(source_id, origin_url) WHERE origin_url IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_groups_parent   ON groups(parent_id);
        CREATE INDEX IF NOT EXISTS idx_sources_group   ON sources(group_id);
        CREATE INDEX IF NOT EXISTS idx_media_tags_tag  ON media_tags(tag_id);
        CREATE INDEX IF NOT EXISTS idx_group_tags_tag  ON group_tags(tag_id);
    ",
    )?;

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
        repair_slug_names(c)
    })?;

    // One-time policy change: all pre-review-era effective ratings are
    // automated recommendations. Later explicit reviews remain protected.
    run_migration_once(conn, "0002_existing_ratings_are_automated", |c| {
        c.execute_batch(
            "UPDATE media SET
            auto_rating_score=CASE WHEN rating=0 OR rating=auto_rating THEN auto_rating_score ELSE NULL END,
            auto_rating=CASE WHEN rating>0 THEN rating ELSE auto_rating END,
            rating_source=CASE WHEN rating>0 OR auto_rating>0 THEN 'auto' ELSE 'none' END,
            rating_reviewed=0, rating_reviewed_at=NULL;",
        )?;
        Ok(())
    })?;

    // Before `human_rating` existed, `rating` was an overloaded effective
    // cache. The reviewed bit is the only reliable signal that an old value
    // was a human decision. Preserve it exactly, including reviewed zero.
    run_migration_once(
        conn,
        "0003_separate_human_ratings_and_tag_provenance",
        |c| {
            c.execute_batch(
                "UPDATE media
             SET human_rating=CASE WHEN rating BETWEEN 1 AND 5 THEN rating ELSE NULL END
             WHERE rating_reviewed=1 AND human_rating IS NULL;
             UPDATE media
             SET rating=CASE
                 WHEN human_rating IS NOT NULL THEN human_rating
                 WHEN auto_rating BETWEEN 1 AND 5 THEN auto_rating
                 ELSE 0
             END,
             rating_source=CASE
                 WHEN human_rating IS NOT NULL THEN 'human'
                 WHEN auto_rating BETWEEN 1 AND 5 THEN 'auto'
                 ELSE 'none'
             END,
             rating_reviewed=CASE WHEN human_rating IS NULL THEN 0 ELSE 1 END;
             INSERT OR IGNORE INTO media_tag_provenance(media_id,tag_id,provenance,added_at)
             SELECT media_id,tag_id,'legacy',datetime('now') FROM media_tags;",
            )?;
            Ok(())
        },
    )?;

    // Preserve optional history created by an early experimental session
    // screen. It is deliberately copied rather than deleted so downgrading an
    // installation cannot destroy the person's own activity records.
    run_migration_once(conn, "0004_migrate_legacy_interactive_sessions", |c| {
        let has_legacy: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='ch_sessions')",
            [],
            |row| row.get(0),
        )?;
        if has_legacy {
            c.execute_batch(
                "INSERT OR IGNORE INTO interactive_sessions(id,started_at,duration_s,item_count,plan,events,ended_state)
                 SELECT id,started_at,duration_s,item_count,filters,notes,'completed' FROM ch_sessions;",
            )?;
        }
        Ok(())
    })?;

    tx.commit()?;
    Ok(())
}

// ─── Migration tracking ────────────────────────────────────────────────────────

fn ensure_migrations_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS _migrations (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT NOT NULL UNIQUE,
            applied_at TEXT NOT NULL
        );
    ",
    )?;

    // CREATE TABLE IF NOT EXISTS is a no-op if a _migrations table already
    // exists from an older, differently-shaped version of this app — seen in
    // the wild: one missing the `name` column entirely, which then makes
    // every query below (and every startup) fail with "no column named
    // name" forever, since nothing here ever repairs an already-existing
    // table. Detect that and move the old table aside instead of touching it
    // further, then create a correctly-shaped one in its place. The one
    // migration this tracks (0001_repair_bunkr_kemono_slugs) only ever
    // touches rows that still exactly match the bug it's fixing, so
    // re-running it once more against a legacy database is safe regardless.
    let cols = column_names(conn, "_migrations");
    if !cols.contains("name") || !cols.contains("applied_at") {
        warn!(
            "_migrations table exists with an incompatible schema — moving it aside and recreating"
        );
        let mut legacy = "_migrations_legacy".to_string();
        while !column_names(conn, &legacy).is_empty() {
            legacy.push('_');
        }
        conn.execute_batch(&format!(
            "
            ALTER TABLE _migrations RENAME TO {legacy};
            CREATE TABLE _migrations (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                name       TEXT NOT NULL UNIQUE,
                applied_at TEXT NOT NULL
            );
        "
        ))?;
    }

    Ok(())
}

fn migration_applied(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM _migrations WHERE name=?1",
        params![name],
        |r| r.get::<_, i64>(0),
    )? > 0)
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
    if migration_applied(conn, name)? {
        return Ok(());
    }
    body(conn)?;
    mark_migration_applied(conn, name)?;
    info!("migration: applied {}", name);
    Ok(())
}

fn repair_slug_names(conn: &Connection) -> Result<()> {
    struct Row {
        id: i64,
        name: String,
        url: String,
    }
    let rows: Vec<Row> = {
        let mut stmt = conn.prepare("SELECT id, name, url FROM sources")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    url: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    for row in rows {
        let old_guess = old_buggy_name(&row.url);
        if row.name != old_guess {
            continue; // doesn't match old bug's output — leave it alone
        }
        let new_name = crate::slug::derive_name_from_url(&row.url);
        if new_name != row.name {
            conn.execute(
                "UPDATE sources SET name=?1 WHERE id=?2",
                params![new_name, row.id],
            )?;
            info!(
                "repair: renamed source {} from {:?} to {:?}",
                row.id, row.name, new_name
            );
        }
    }
    Ok(())
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

    let segments: Vec<&str> = path_part.split('/').filter(|s| !s.is_empty()).collect();

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
pub fn build_group_ancestry_map(conn: &Connection) -> rusqlite::Result<HashMap<i64, Vec<i64>>> {
    struct G {
        id: i64,
        parent_id: Option<i64>,
    }
    let rows: Vec<G> = {
        let mut stmt = conn.prepare("SELECT id, parent_id FROM groups")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(G {
                    id: r.get(0)?,
                    parent_id: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    let parents: HashMap<i64, Option<i64>> = rows.iter().map(|g| (g.id, g.parent_id)).collect();

    Ok(parents
        .keys()
        .map(|&gid| {
            let mut chain = vec![gid];
            let mut seen = HashSet::from([gid]);
            let mut cur = parents[&gid];
            while let Some(pid) = cur {
                if seen.contains(&pid) {
                    break;
                }
                chain.push(pid);
                seen.insert(pid);
                cur = parents.get(&pid).copied().flatten();
            }
            (gid, chain)
        })
        .collect())
}

/// group_id → set of tag names (group's own name + explicit tags + all ancestors' names+tags)
pub fn build_group_effective_tags_map(
    conn: &Connection,
) -> rusqlite::Result<HashMap<i64, HashSet<String>>> {
    let ancestry = build_group_ancestry_map(conn)?;

    let names: HashMap<i64, String> = {
        let mut stmt = conn.prepare("SELECT id, name FROM groups")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, name)| (id, name.trim().to_lowercase()))
            .collect()
    };

    let mut own_tags: HashMap<i64, HashSet<String>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT gt.group_id, t.name FROM group_tags gt JOIN tags t ON t.id = gt.tag_id",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (gid, tag) in rows {
            own_tags.entry(gid).or_default().insert(tag);
        }
    }

    Ok(ancestry
        .into_iter()
        .map(|(gid, chain)| {
            let mut tags = HashSet::new();
            for ancestor_id in &chain {
                if let Some(name) = names.get(ancestor_id) {
                    if !name.is_empty() {
                        tags.insert(name.clone());
                    }
                }
                if let Some(t) = own_tags.get(ancestor_id) {
                    tags.extend(t.iter().cloned());
                }
            }
            (gid, tags)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_preserves_multiple_legacy_tables_and_is_repeatable() {
        for schema in [
            "version INTEGER",
            "name TEXT",
            "id INTEGER, applied_at TEXT",
        ] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(&format!("CREATE TABLE _migrations({schema}); CREATE TABLE _migrations_legacy(keep TEXT); INSERT INTO _migrations_legacy VALUES('preserve');")).unwrap();
            run_migrations(&conn).unwrap();
            run_migrations(&conn).unwrap();
            assert_eq!(
                conn.query_row("SELECT keep FROM _migrations_legacy", [], |r| r
                    .get::<_, String>(0))
                    .unwrap(),
                "preserve"
            );
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM _migrations", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                // Current schema has four durable, one-time migrations. The
                // important part of this regression test is that a second
                // startup does not duplicate any of them.
                4
            );
        }
    }

    #[test]
    fn failed_migration_rolls_back_schema_and_tracker_repair() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE _migrations(version INTEGER); CREATE VIEW _migrations_legacy AS SELECT 1 AS preserved;").unwrap();
        // Force a failure after schema work starts, then verify nothing was committed.
        conn.execute_batch("CREATE VIEW sources AS SELECT 1 AS id;")
            .unwrap();
        assert!(run_migrations(&conn).is_err());
        assert!(column_names(&conn, "_migrations").contains("version"));
        assert!(column_names(&conn, "groups").is_empty());
    }

    /// Reproduces the exact failure seen in the wild: a pre-existing
    /// `_migrations` table from an older, differently-shaped version of the
    /// app (no `name` column) must not make `run_migrations` — and therefore
    /// the whole app's startup — fail with "no column named name".
    #[test]
    fn run_migrations_repairs_legacy_migrations_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE _migrations (id INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);",
        )
        .unwrap();

        run_migrations(&conn).expect("run_migrations should self-heal, not fail");

        assert!(column_names(&conn, "_migrations").contains("name"));
        // Old data preserved, not silently dropped.
        assert!(column_names(&conn, "_migrations_legacy").contains("applied_at"));
        // The migration still gets recorded in the new table.
        assert!(migration_applied(&conn, "0001_repair_bunkr_kemono_slugs").unwrap());
    }

    /// A normal, already-correct database (the common case) shouldn't be
    /// touched by the repair path at all.
    #[test]
    fn run_migrations_is_a_noop_on_a_healthy_db() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).expect("running migrations twice must stay safe");
        assert!(column_names(&conn, "_migrations").contains("name"));
        assert!(!column_names(&conn, "_migrations_legacy").contains("applied_at"));
    }

    /// A legacy settings.json written before OOBE existed in this codebase
    /// must not force an already-configured installation through first-run
    /// setup just because the new field defaults to `false`.
    #[test]
    fn load_settings_self_heals_legacy_file_as_oobe_completed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            settings_path(dir.path()),
            r#"{"max_concurrent":4,"theme":"dark"}"#,
        )
        .unwrap();

        let loaded = load_settings(dir.path());
        assert!(
            loaded.oobe_completed,
            "legacy settings.json should self-heal to completed"
        );

        // And the repair should have been persisted, not just held in memory.
        let raw = std::fs::read_to_string(settings_path(dir.path())).unwrap();
        let repaired: Settings = serde_json::from_str(&raw).unwrap();
        assert!(repaired.oobe_completed);
    }

    /// A genuinely fresh install (no settings.json on disk at all) should
    /// need OOBE.
    #[test]
    fn load_settings_defaults_oobe_incomplete_for_fresh_install() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_settings(dir.path());
        assert!(!loaded.oobe_completed);
    }

    /// A settings.json that already has the field (any prior OOBE run,
    /// complete or reset) must be respected as-is, not re-healed.
    #[test]
    fn load_settings_respects_explicit_oobe_completed_false() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            settings_path(dir.path()),
            r#"{"max_concurrent":4,"theme":"dark","oobe_completed":false}"#,
        )
        .unwrap();
        let loaded = load_settings(dir.path());
        assert!(!loaded.oobe_completed);
    }
}

#[cfg(test)]
mod rating_migration_tests {
    #[test]
    fn existing_provenance_is_reset_once_and_future_reviews_survive() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        super::run_migrations(&conn).unwrap();
        conn.execute_batch("DELETE FROM _migrations WHERE name='0002_existing_ratings_are_automated';
            INSERT INTO sources(id,name,url,slug,added_at) VALUES(1,'test','test','test','2026');
            INSERT INTO media(id,source_id,filepath,filename,type,added_at,rating,auto_rating,auto_rating_score,rating_source,rating_reviewed,rating_reviewed_at)
            VALUES(1,1,'a','a','image','2026',3,4,0.72,'human',1,'2026');").unwrap();
        super::run_migrations(&conn).unwrap();
        let row: (i64,i64,Option<f64>,String,bool,Option<String>) = conn.query_row("SELECT rating,auto_rating,auto_rating_score,rating_source,rating_reviewed,rating_reviewed_at FROM media", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
        assert_eq!(row, (3, 3, None, "auto".into(), false, None));
        conn.execute_batch("UPDATE media SET rating=2,rating_source='human',rating_reviewed=1,rating_reviewed_at='2027';").unwrap();
        super::run_migrations(&conn).unwrap();
        assert!(conn
            .query_row("SELECT rating_reviewed FROM media", [], |r| r
                .get::<_, bool>(0))
            .unwrap());
    }

    #[test]
    fn legacy_ratings_are_preserved_and_migration_is_idempotent() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE media(id INTEGER PRIMARY KEY, source_id INTEGER NOT NULL, filepath TEXT UNIQUE NOT NULL, filename TEXT NOT NULL, type TEXT NOT NULL, added_at TEXT NOT NULL, rating INTEGER NOT NULL DEFAULT 0);
            INSERT INTO media VALUES(1,1,'a','a','image','2026',3),(2,1,'b','b','image','2026',0);").unwrap();
        super::run_migrations(&conn).unwrap();
        let row: (i64,String,bool,Option<String>) = conn.query_row("SELECT rating,rating_source,rating_reviewed,rating_reviewed_at FROM media WHERE id=1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(row, (3, "auto".into(), false, None));
        conn.execute(
            "UPDATE media SET rating=4,auto_rating=3,rating_source='human',rating_reviewed=1,rating_reviewed_at='2026' WHERE id=2",
            [],
        )
        .unwrap();
        super::run_migrations(&conn).unwrap();
        assert!(conn
            .query_row("SELECT rating_reviewed FROM media WHERE id=2", [], |r| r
                .get::<_, bool>(
                0
            ))
            .unwrap());
    }
}
