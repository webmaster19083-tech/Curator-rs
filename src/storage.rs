//! Storage accounting, download admission, and conservative cleanup helpers.
//!
//! This module deliberately treats originals differently from derived cache
//! files. Cache eviction is safe once a person enables a limit; deleting an
//! original is only ever done by an explicit retention rule or a confirmed
//! local action, and always leaves its database metadata in place.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::params;
use serde_json::{json, Value};

use crate::db::{now_iso, save_settings, Settings};
use crate::AppState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncPause {
    SourceQuota { used_bytes: u64, allowed_bytes: u64 },
    LowDisk { free_bytes: u64, reserve_bytes: u64 },
}

impl SyncPause {
    pub fn status(&self) -> &'static str {
        match self {
            Self::SourceQuota { .. } => "storage_limit",
            Self::LowDisk { .. } => "low_disk",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::SourceQuota {
                used_bytes,
                allowed_bytes,
            } => format!(
                "Storage limit: {} used / {} allowed. Increase the source storage limit, remove old media, or permit one sync.",
                human_bytes(*used_bytes),
                human_bytes(*allowed_bytes)
            ),
            Self::LowDisk {
                free_bytes,
                reserve_bytes,
            } => format!(
                "Low disk space: {} free / {} reserve. New downloads are paused before the disk becomes dangerously full.",
                human_bytes(*free_bytes),
                human_bytes(*reserve_bytes)
            ),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CleanupOutcome {
    pub removed_items: u64,
    pub removed_bytes: u64,
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Count regular files without following links. Curator owns these directories
/// and must never let an accounting pass escape through a junction/symlink.
pub fn directory_usage(path: &Path) -> u64 {
    if !path.is_dir() {
        return 0;
    }
    walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| entry.metadata().ok().map(|meta| meta.len()))
        .sum()
}

pub fn source_usage(library_dir: &Path, slug: &str) -> u64 {
    directory_usage(&library_dir.join(slug))
}

pub fn available_disk_bytes(path: &Path) -> Option<u64> {
    fs2::available_space(path).ok()
}

pub fn total_disk_bytes(path: &Path) -> Option<u64> {
    fs2::total_space(path).ok()
}

/// Check before a source process is started. A quota uses `>=` here so an
/// already-full source never even starts gallery-dl. During an active sync we
/// use `>` (see `active_sync_pause`) to allow exactly one file to cross it.
pub fn source_sync_admission(
    settings: &Settings,
    data_dir: &Path,
    library_dir: &Path,
    slug: &str,
    apply_source_limits: bool,
) -> Option<SyncPause> {
    if let Some(reserve) = settings.minimum_free_disk_bytes {
        if let Some(free) = available_disk_bytes(data_dir) {
            if free < reserve {
                return Some(SyncPause::LowDisk {
                    free_bytes: free,
                    reserve_bytes: reserve,
                });
            }
        }
    }
    if apply_source_limits {
        if let Some(limit) = settings.max_source_storage_bytes {
            let used = source_usage(library_dir, slug);
            if used >= limit {
                return Some(SyncPause::SourceQuota {
                    used_bytes: used,
                    allowed_bytes: limit,
                });
            }
        }
    }
    None
}

/// Called after indexing a newly finished file. The source can therefore
/// exceed its configured quota by one permitted file, never by a planned
/// batch. gallery-dl is then stopped and the durable source state explains why.
pub fn active_sync_pause(
    settings: &Settings,
    data_dir: &Path,
    library_dir: &Path,
    slug: &str,
    apply_source_limits: bool,
) -> Option<SyncPause> {
    if let Some(reserve) = settings.minimum_free_disk_bytes {
        if let Some(free) = available_disk_bytes(data_dir) {
            if free < reserve {
                return Some(SyncPause::LowDisk {
                    free_bytes: free,
                    reserve_bytes: reserve,
                });
            }
        }
    }
    if apply_source_limits {
        if let Some(limit) = settings.max_source_storage_bytes {
            let used = source_usage(library_dir, slug);
            if used > limit {
                return Some(SyncPause::SourceQuota {
                    used_bytes: used,
                    allowed_bytes: limit,
                });
            }
        }
    }
    None
}

fn is_media_path(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    crate::downloader::image_exts().contains(&ext.as_str())
        || crate::downloader::video_exts().contains(&ext.as_str())
}

fn is_sidecar_path(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("json"))
}

fn safe_library_file(library_dir: &Path, relative: &str) -> Option<PathBuf> {
    let candidate = Path::new(relative);
    if candidate.is_absolute()
        || candidate.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let root = dunce::canonicalize(library_dir).ok()?;
    let resolved = dunce::canonicalize(library_dir.join(candidate)).ok()?;
    resolved.starts_with(root).then_some(resolved)
}

fn source_rows(pool: &crate::db::DbPool) -> Vec<(i64, String, String, String, Option<i64>, bool)> {
    let Ok(conn) = pool.get() else {
        return Vec::new();
    };
    let Ok(mut statement) = conn.prepare(
        "SELECT id,name,slug,status,retention_keep_newest,storage_override_once FROM sources",
    ) else {
        return Vec::new();
    };
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, bool>(5)?,
            ))
        })
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .collect()
}

pub fn dashboard_snapshot(
    pool: &crate::db::DbPool,
    data_dir: &Path,
    library_dir: &Path,
    archives_dir: &Path,
    thumbs_dir: &Path,
    settings: &Settings,
    sort: &str,
) -> Value {
    let mut original_media = 0_u64;
    let mut metadata_sidecars = 0_u64;
    if library_dir.is_dir() {
        for entry in walkdir::WalkDir::new(library_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
        {
            let bytes = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            if is_media_path(entry.path()) {
                original_media = original_media.saturating_add(bytes);
            } else if is_sidecar_path(entry.path()) {
                metadata_sidecars = metadata_sidecars.saturating_add(bytes);
            }
        }
    }

    let mut sources = source_rows(pool)
        .into_iter()
        .map(|(id, name, slug, status, retention_keep_newest, storage_override_once)| {
            let used_bytes = source_usage(library_dir, &slug);
            json!({
                "id": id,
                "name": name,
                "slug": slug,
                "status": status,
                "used_bytes": used_bytes,
                "allowed_bytes": settings.max_source_storage_bytes,
                "at_or_over_limit": settings.max_source_storage_bytes.is_some_and(|limit| used_bytes >= limit),
                "retention_keep_newest": retention_keep_newest,
                "storage_override_once": storage_override_once,
            })
        })
        .collect::<Vec<_>>();
    match sort {
        "usage_asc" => sources.sort_by_key(|value| value["used_bytes"].as_u64().unwrap_or(0)),
        "name" => {
            sources.sort_by_key(|value| value["name"].as_str().unwrap_or("").to_ascii_lowercase())
        }
        _ => sources
            .sort_by_key(|value| std::cmp::Reverse(value["used_bytes"].as_u64().unwrap_or(0))),
    }

    json!({
        "categories": {
            "original_media": original_media,
            "thumbnail_cache": thumbnail_cache_usage(thumbs_dir),
            "metadata_sidecars": metadata_sidecars,
            "backups": directory_usage(&data_dir.join("backups")),
            "phar_environment": directory_usage(&data_dir.join("phar")),
            "gallery_dl_archives": directory_usage(archives_dir),
        },
        "disk": {
            "free_bytes": available_disk_bytes(data_dir),
            "total_bytes": total_disk_bytes(data_dir),
            "minimum_free_disk_bytes": settings.minimum_free_disk_bytes,
        },
        "sources": sources,
        "note": "An active remote source sync can exceed its source quota by up to one permitted file before Curator pauses it.",
    })
}

fn protected_cleanup_candidates(
    state: &AppState,
    source_id: i64,
    keep_newest: u32,
) -> Result<Vec<(i64, String, u64)>> {
    let conn = state.pool.get()?;
    // A source's first available image is its current cover in the Explorer;
    // never make that cover disappear as a side effect of retention.
    let mut statement = conn.prepare(
        "SELECT m.id,m.filepath,COALESCE(m.file_size_bytes,0)
         FROM media m
         WHERE m.source_id=?1 AND m.downloaded=1 AND m.missing=0
           AND COALESCE(m.retention_deleted,0)=0
           AND COALESCE(m.favorite,0)=0
           AND m.human_rating IS NULL AND m.rating_reviewed=0 AND m.rating_source<>'human'
           AND NOT EXISTS(SELECT 1 FROM media_groups mg WHERE mg.media_id=m.id)
           AND m.id<>COALESCE((SELECT cover.id FROM media cover
                                WHERE cover.source_id=m.source_id AND cover.type='image'
                                  AND cover.downloaded=1 AND cover.missing=0
                                ORDER BY cover.id LIMIT 1),-1)
           AND m.id NOT IN (SELECT newest.id FROM media newest
                            WHERE newest.source_id=?1 AND newest.downloaded=1
                              AND newest.missing=0 AND COALESCE(newest.retention_deleted,0)=0
                            ORDER BY COALESCE(newest.downloaded_at,newest.added_at) DESC,newest.id DESC
                            LIMIT ?2)
         ORDER BY COALESCE(m.downloaded_at,m.added_at) ASC,m.id ASC",
    )?;
    let rows = statement.query_map(params![source_id, keep_newest as i64], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?.max(0) as u64,
        ))
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

/// Delete only confirmed/opted-in unprotected originals and retain the media
/// row, sidecar metadata, annotations, tags, and provenance as a placeholder.
pub fn cleanup_source_keep_newest(
    state: &AppState,
    source_id: i64,
    keep_newest: u32,
) -> Result<CleanupOutcome> {
    let mut outcome = CleanupOutcome::default();
    for (media_id, filepath, saved_bytes) in
        protected_cleanup_candidates(state, source_id, keep_newest)?
    {
        let Some(path) = safe_library_file(&state.library_dir, &filepath) else {
            continue;
        };
        if path.is_file() {
            let actual_bytes = std::fs::metadata(&path)
                .map(|meta| meta.len())
                .unwrap_or(saved_bytes);
            std::fs::remove_file(&path)?;
            outcome.removed_bytes = outcome.removed_bytes.saturating_add(actual_bytes);
        }
        let conn = state.pool.get()?;
        conn.execute(
            "UPDATE media SET downloaded=0,missing=1,retention_deleted=1,
                 skip_reason='Removed by retention; annotations and metadata remain as an unavailable placeholder.',
                 skipped_at=?1 WHERE id=?2",
            params![now_iso(), media_id],
        )?;
        outcome.removed_items = outcome.removed_items.saturating_add(1);
    }
    Ok(outcome)
}

fn thumbnail_unit(path: &Path) -> Option<(PathBuf, Vec<PathBuf>, u64, SystemTime)> {
    if !path.is_file()
        || !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("jpg"))
    {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    let stem = path.file_stem()?.to_str()?;
    if stem.parse::<i64>().is_err() {
        return None;
    }
    let parent = path.parent()?;
    let extras = [
        parent.join(format!("{stem}.stamp")),
        parent.join(format!("{stem}.failed")),
    ]
    .into_iter()
    .filter(|extra| extra.is_file())
    .collect::<Vec<_>>();
    let bytes = metadata.len()
        + extras
            .iter()
            .filter_map(|extra| std::fs::metadata(extra).ok().map(|meta| meta.len()))
            .sum::<u64>();
    Some((
        path.to_path_buf(),
        extras,
        bytes,
        metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    ))
}

fn thumbnail_cache_usage(thumbs_dir: &Path) -> u64 {
    std::fs::read_dir(thumbs_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| thumbnail_unit(&entry.path()).map(|unit| unit.2))
        .sum()
}

/// Least-recently-used thumbnail eviction. Thumbnail mtime is touched on each
/// cache hit (see `thumb_worker`) so this remains deterministic on filesystems
/// where atime is disabled.
pub fn trim_thumbnail_cache(thumbs_dir: &Path, maximum_bytes: u64) -> Result<CleanupOutcome> {
    let mut units = std::fs::read_dir(thumbs_dir)?
        .filter_map(Result::ok)
        .filter_map(|entry| thumbnail_unit(&entry.path()))
        .collect::<Vec<_>>();
    // Only recognized thumbnail units count toward this limit. Sidecar files
    // belonging to other features may share the cache directory and must not
    // cause an unrelated thumbnail to be evicted.
    let mut total = units.iter().map(|unit| unit.2).sum::<u64>();
    if maximum_bytes == 0 || total <= maximum_bytes {
        return Ok(CleanupOutcome::default());
    }
    units.sort_by_key(|unit| unit.3);
    let mut outcome = CleanupOutcome::default();
    for (main, extras, bytes, _) in units {
        if total <= maximum_bytes {
            break;
        }
        if main.is_file() {
            std::fs::remove_file(&main)?;
        }
        for extra in extras {
            let _ = std::fs::remove_file(extra);
        }
        total = total.saturating_sub(bytes);
        outcome.removed_items = outcome.removed_items.saturating_add(1);
        outcome.removed_bytes = outcome.removed_bytes.saturating_add(bytes);
    }
    Ok(outcome)
}

pub fn clear_thumbnail_cache(thumbs_dir: &Path) -> Result<CleanupOutcome> {
    let mut outcome = CleanupOutcome::default();
    if !thumbs_dir.is_dir() {
        return Ok(outcome);
    }
    for entry in std::fs::read_dir(thumbs_dir)?.filter_map(Result::ok) {
        let path = entry.path();
        let is_artifact = path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| matches!(value, "jpg" | "stamp" | "failed"));
        if is_artifact && path.is_file() {
            let bytes = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            std::fs::remove_file(path)?;
            outcome.removed_items = outcome.removed_items.saturating_add(1);
            outcome.removed_bytes = outcome.removed_bytes.saturating_add(bytes);
        }
    }
    crate::thumb_worker::clear_failure_cache();
    Ok(outcome)
}

pub fn remove_archives_older_than(
    archives_dir: &Path,
    age_days: Option<u32>,
) -> Result<CleanupOutcome> {
    let mut outcome = CleanupOutcome::default();
    if !archives_dir.is_dir() {
        return Ok(outcome);
    }
    let cutoff =
        age_days.map(|days| SystemTime::now() - Duration::from_secs(u64::from(days) * 86_400));
    for entry in std::fs::read_dir(archives_dir)?.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(cutoff) = cutoff {
            let modified = entry.metadata().ok().and_then(|meta| meta.modified().ok());
            if modified.is_some_and(|modified| modified > cutoff) {
                continue;
            }
        }
        let bytes = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        std::fs::remove_file(path)?;
        outcome.removed_items = outcome.removed_items.saturating_add(1);
        outcome.removed_bytes = outcome.removed_bytes.saturating_add(bytes);
    }
    Ok(outcome)
}

fn weekly_due(last_run: Option<&str>) -> bool {
    let Some(last_run) = last_run else {
        return true;
    };
    DateTime::parse_from_rfc3339(last_run)
        .ok()
        .map(|timestamp| {
            Utc::now()
                .signed_duration_since(timestamp.with_timezone(&Utc))
                .num_days()
                >= 7
        })
        .unwrap_or(true)
}

/// Automatic cleanup is intentionally conservative: it only applies an
/// already-enabled per-source retention rule, cache eviction, and an optional
/// archive age policy. It never recompresses originals and never invents a
/// deletion candidate for a source that has no retention rule.
pub async fn maybe_run_automatic_cleanup(state: std::sync::Arc<AppState>) {
    let settings = state.settings.read().await.clone();
    let trigger = match settings.automatic_cleanup_mode.as_str() {
        "weekly" => weekly_due(settings.last_automatic_cleanup_at.as_deref()),
        "low_disk" => settings
            .automatic_cleanup_low_disk_bytes
            .zip(available_disk_bytes(&state.data_dir))
            .is_some_and(|(threshold, free)| free < threshold),
        _ => false,
    };
    if !trigger || state.maintenance.is_active() {
        return;
    }
    let Some(worker_lease) = state.maintenance.try_acquire_background_worker() else {
        return;
    };
    let source_state = state.clone();
    let settings_for_work = settings.clone();
    let worked = tokio::task::spawn_blocking(move || {
        let _worker_lease = worker_lease;
        for (id, _, _, _, retention, _) in source_rows(&source_state.pool) {
            if let Some(keep_newest) = retention.and_then(|value| u32::try_from(value).ok()) {
                let _ = cleanup_source_keep_newest(&source_state, id, keep_newest);
            }
        }
        if let Some(limit) = settings_for_work.thumbnail_cache_max_bytes {
            let _ = trim_thumbnail_cache(&source_state.thumbs_dir, limit);
        }
        if settings_for_work.archive_retention_days.is_some() {
            let _ = remove_archives_older_than(
                &source_state.archives_dir,
                settings_for_work.archive_retention_days,
            );
        }
    })
    .await
    .is_ok();
    if worked {
        let mut current = state.settings.write().await;
        if current.automatic_cleanup_mode == settings.automatic_cleanup_mode {
            current.last_automatic_cleanup_at = Some(now_iso());
            save_settings(&state.data_dir, &current);
        }
    }
}

pub async fn automatic_cleanup_loop(state: std::sync::Arc<AppState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(60 * 60));
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => return,
            _ = interval.tick() => maybe_run_automatic_cleanup(state.clone()).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_and_low_disk_admission_are_conservative() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        std::fs::create_dir_all(library.join("source")).unwrap();
        std::fs::write(library.join("source/item.jpg"), b"12345").unwrap();
        let settings = Settings {
            max_source_storage_bytes: Some(5),
            ..Settings::default()
        };
        assert!(matches!(
            source_sync_admission(&settings, dir.path(), &library, "source", true),
            Some(SyncPause::SourceQuota {
                used_bytes: 5,
                allowed_bytes: 5
            })
        ));
        assert!(active_sync_pause(&settings, dir.path(), &library, "source", true).is_none());
        std::fs::write(library.join("source/next.jpg"), b"6").unwrap();
        assert!(matches!(
            active_sync_pause(&settings, dir.path(), &library, "source", true),
            Some(SyncPause::SourceQuota { .. })
        ));
        // Use a deterministic reserve well above any realistic free-space value;
        // another test may create/delete files between two filesystem probes.
        let reserve_settings = Settings {
            minimum_free_disk_bytes: Some(u64::MAX),
            ..Settings::default()
        };
        assert!(matches!(
            source_sync_admission(&reserve_settings, dir.path(), &library, "source", false),
            Some(SyncPause::LowDisk { .. })
        ));
    }

    #[test]
    fn retention_protects_human_ratings_groups_favorites_and_covers() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(dir.path());
        crate::test_support::source(&state);
        let root = state.library_dir.join("test");
        for name in [
            "cover.jpg",
            "old.jpg",
            "rated.jpg",
            "favorite.jpg",
            "group.jpg",
        ] {
            std::fs::write(root.join(name), b"original").unwrap();
        }
        let conn = state.pool.get().unwrap();
        conn.execute_batch(
            "INSERT INTO media(id,source_id,filepath,filename,type,added_at,downloaded) VALUES
              (1,1,'test/cover.jpg','cover.jpg','image','2026-05-01',1),
              (2,1,'test/old.jpg','old.jpg','image','2026-04-01',1),
              (3,1,'test/rated.jpg','rated.jpg','image','2026-03-01',1),
              (4,1,'test/favorite.jpg','favorite.jpg','image','2026-02-01',1),
              (5,1,'test/group.jpg','group.jpg','image','2026-01-01',1);
              UPDATE media SET human_rating=5,rating_source='human' WHERE id=3;
              UPDATE media SET favorite=1 WHERE id=4;
              INSERT INTO groups(id,name,added_at) VALUES(1,'keep','2026');
              INSERT INTO media_groups(media_id,group_id,added_at) VALUES(5,1,'2026');",
        )
        .unwrap();
        let outcome = cleanup_source_keep_newest(&state, 1, 0).unwrap();
        assert_eq!(outcome.removed_items, 1);
        assert!(!root.join("old.jpg").exists());
        for name in ["cover.jpg", "rated.jpg", "favorite.jpg", "group.jpg"] {
            assert!(root.join(name).exists(), "{name} must be protected");
        }
        let placeholder: (i64, i64, i64) = conn
            .query_row(
                "SELECT downloaded,missing,retention_deleted FROM media WHERE id=2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(placeholder, (0, 1, 1));
    }

    #[test]
    fn thumbnail_trim_is_lru_and_only_touches_cache_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("1.jpg"), vec![1; 8]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        std::fs::write(dir.path().join("2.jpg"), vec![2; 8]).unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"not cache").unwrap();
        let outcome = trim_thumbnail_cache(dir.path(), 10).unwrap();
        assert_eq!(outcome.removed_items, 1);
        assert!(!dir.path().join("1.jpg").exists());
        assert!(dir.path().join("2.jpg").exists());
        assert!(dir.path().join("keep.txt").exists());
    }

    #[test]
    fn archive_cleanup_removes_gallery_dl_sqlite_archives_and_reports_the_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("source.sqlite3"), vec![1; 9]).unwrap();
        std::fs::write(dir.path().join("another.sqlite3"), vec![2; 7]).unwrap();
        let outcome = remove_archives_older_than(dir.path(), None).unwrap();
        assert_eq!(outcome.removed_items, 2);
        assert_eq!(outcome.removed_bytes, 16);
        assert!(!dir.path().join("source.sqlite3").exists());
        assert!(!dir.path().join("another.sqlite3").exists());
    }

    #[test]
    fn dashboard_reports_required_categories_and_sorts_sources_by_usage() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(dir.path());
        crate::test_support::source(&state);
        std::fs::write(state.library_dir.join("test/large.jpg"), vec![1; 12]).unwrap();
        std::fs::write(state.thumbs_dir.join("1.jpg"), vec![1; 5]).unwrap();
        std::fs::write(state.archives_dir.join("test.sqlite3"), vec![1; 3]).unwrap();
        let settings = Settings::default();
        let snapshot = dashboard_snapshot(
            &state.pool,
            &state.data_dir,
            &state.library_dir,
            &state.archives_dir,
            &state.thumbs_dir,
            &settings,
            "usage_desc",
        );
        for key in [
            "original_media",
            "thumbnail_cache",
            "metadata_sidecars",
            "backups",
            "phar_environment",
            "gallery_dl_archives",
        ] {
            assert!(snapshot["categories"].get(key).is_some(), "missing {key}");
        }
        assert_eq!(snapshot["categories"]["original_media"], 12);
        assert_eq!(snapshot["categories"]["thumbnail_cache"], 5);
        assert_eq!(snapshot["categories"]["gallery_dl_archives"], 3);
        assert_eq!(snapshot["sources"][0]["used_bytes"], 12);
    }
}
