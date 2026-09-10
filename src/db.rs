use anyhow::{Context, Result};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Connection};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use crate::slug::derive_name_from_url;

pub type DbPool = Pool<SqliteConnectionManager>;

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
        .context("failed to create DB pool")?;
    let conn = pool.get().context("failed to get DB connection")?;
    run_migrations(&conn).context("DB migration failed")?;
    Ok(pool)
}

pub fn run_migrations(conn: &Connection) -> Result<()> {
    // ── Schema ──────────────────────────────────────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS groups (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            name      TEXT NOT NULL,
            parent_id INTEGER REFERENCES groups(id) ON DELETE SET NULL,
            added_at  TEXT NOT NULL
        );
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
        CREATE TABLE IF NOT EXISTS media (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            source_id  INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
            filepath   TEXT NOT NULL UNIQUE,
            filename   TEXT NOT NULL,
            type       TEXT NOT NULL,
            added_at   TEXT NOT NULL,
            rating     INTEGER NOT NULL DEFAULT 0
        );
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
        );",
    )?;

    // ── ADD MISSING COLUMNS (safe on existing DBs) ───────────────────────
    add_column_if_missing(
        conn,
        "sources",
        "group_id",
        "INTEGER REFERENCES groups(id) ON DELETE SET NULL",
    )?;
    add_column_if_missing(
        conn,
        "groups",
        "parent_id",
        "INTEGER REFERENCES groups(id) ON DELETE SET NULL",
    )?;
    add_column_if_missing(conn, "media", "rating", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "media", "origin_url", "TEXT")?;
    add_column_if_missing(conn, "media", "downloaded", "INTEGER NOT NULL DEFAULT 1")?;

    // ── INDEXES ──────────────────────────────────────────────────────────
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_media_source    ON media(source_id);
         CREATE INDEX IF NOT EXISTS idx_media_rating    ON media(rating);
         CREATE INDEX IF NOT EXISTS idx_media_added_at  ON media(added_at);
         CREATE UNIQUE INDEX IF NOT EXISTS idx_media_source_origin
             ON media(source_id, origin_url) WHERE origin_url IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_groups_parent   ON groups(parent_id);
         CREATE INDEX IF NOT EXISTS idx_sources_group   ON sources(group_id);
         CREATE INDEX IF NOT EXISTS idx_media_tags_tag  ON media_tags(tag_id);
         CREATE INDEX IF NOT EXISTS idx_group_tags_tag  ON group_tags(tag_id);",
    )?;

    // ── SELF-HEALING NAME REPAIR ─────────────────────────────────────────
    // Repairs sources auto-named before _URL_SKIP_SEGMENTS existed.
    // Only touches a row if its current name exactly matches what the
    // pre-skip-list logic would have produced for its URL — anything
    // deliberately renamed is left alone.
    let rows: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, name, url FROM sources")?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    for (id, name, url) in rows {
        let old_guess = old_buggy_name(&url);
        if name != old_guess {
            continue; // deliberately renamed — leave alone
        }
        let new_guess = derive_name_from_url(&url);
        if new_guess != name {
            conn.execute(
                "UPDATE sources SET name=? WHERE id=?",
                params![new_guess, id],
            )?;
        }
    }

    Ok(())
}

fn add_column_if_missing(conn: &Connection, table: &str, col: &str, def: &str) -> Result<()> {
    let exists: bool = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == col);
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {def};"))?;
    }
    Ok(())
}

/// Reconstructs what the pre-skip-list naming logic produced:
/// take the very first path segment, lstrip '@', format "{seg} ({site})".
fn old_buggy_name(url: &str) -> String {
    use url::Url;
    let Ok(parsed) = Url::parse(url) else {
        return String::new();
    };
    let host = parsed.host_str().unwrap_or("").to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let site = host.split('.').next().unwrap_or("site");
    let seg = parsed
        .path_segments()
        .and_then(|mut s| s.next())
        .unwrap_or("")
        .trim_start_matches('@')
        .to_string();
    if seg.is_empty() {
        host.to_string()
    } else {
        format!("{seg} ({site})")
    }
}

// ── Group tag cache helpers ──────────────────────────────────────────────────

/// group_id → vec of ancestor group_ids (self first, then parent, …, root)
pub fn build_group_ancestry_map(conn: &Connection) -> Result<HashMap<i64, Vec<i64>>> {
    let parents: HashMap<i64, Option<i64>> = {
        let mut stmt = conn.prepare("SELECT id, parent_id FROM groups")?;
        let parents: HashMap<i64, Option<i64>> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        parents
    };

    fn chain(id: i64, parents: &HashMap<i64, Option<i64>>) -> Vec<i64> {
        let mut out = vec![id];
        let mut seen = HashSet::from([id]);
        let mut cur = parents.get(&id).copied().flatten();
        while let Some(p) = cur {
            if seen.contains(&p) {
                break;
            }
            out.push(p);
            seen.insert(p);
            cur = parents.get(&p).copied().flatten();
        }
        out
    }

    Ok(parents
        .keys()
        .map(|&id| (id, chain(id, &parents)))
        .collect())
}

/// group_id → set of effective tag names (own tags ∪ ancestor tags ∪ group names)
pub fn build_group_effective_tags_map(conn: &Connection) -> Result<HashMap<i64, HashSet<String>>> {
    let ancestry = build_group_ancestry_map(conn)?;

    let names: HashMap<i64, String> = {
        let mut stmt = conn.prepare("SELECT id, name FROM groups")?;
        let names: HashMap<i64, String> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .map(|(id, name)| (id, name.trim().to_lowercase()))
            .collect();
        names
    };

    let mut own_tags: HashMap<i64, HashSet<String>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT gt.group_id, t.name FROM group_tags gt JOIN tags t ON t.id = gt.tag_id",
        )?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (gid, tag) = row?;
            own_tags.entry(gid).or_default().insert(tag);
        }
    }

    let mut effective: HashMap<i64, HashSet<String>> = HashMap::new();
    for (&gid, chain) in &ancestry {
        let mut tags = HashSet::new();
        for &aid in chain {
            if let Some(n) = names.get(&aid) {
                tags.insert(n.clone());
            }
            if let Some(t) = own_tags.get(&aid) {
                tags.extend(t.iter().cloned());
            }
        }
        effective.insert(gid, tags);
    }
    Ok(effective)
}

// ── Misc helpers ─────────────────────────────────────────────────────────────

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn pending_filepath(source_id: i64, origin_url: &str) -> String {
    use sha1::Digest;
    let mut h = sha1::Sha1::new();
    h.update(origin_url.as_bytes());
    let digest = hex::encode(&h.finalize()[..8]);
    format!("__pending__/{source_id}/{digest}")
}
