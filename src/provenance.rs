//! Durable metadata provenance for Curator's media library.
//!
//! Source extractors are allowed to be noisy and change their field names.
//! This module preserves their original JSON, turns only explicit tag-like
//! fields into reviewable candidates, and never treats those candidates as a
//! human decision until they are approved.

use std::collections::BTreeSet;

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::now_iso;

pub const HUMAN: &str = "human";
pub const HUMAN_EDITED: &str = "human_edited";
pub const SOURCE_APPROVED: &str = "source_approved";
pub const AUTOMATIC: &str = "automatic";
pub const LEGACY: &str = "legacy";

#[derive(Debug, Clone, Serialize)]
pub struct SourceTagCandidate {
    pub id: i64,
    pub media_id: i64,
    pub filename: String,
    pub source_id: i64,
    pub source_name: String,
    pub source_url: String,
    pub provider: String,
    pub raw_name: String,
    pub normalized_name: Option<String>,
    pub state: String,
    pub added_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuleInput {
    pub raw_name: String,
    pub action: String,
    pub normalized_name: Option<String>,
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TagRule {
    pub id: i64,
    pub provider: Option<String>,
    pub raw_name: String,
    pub action: String,
    pub normalized_name: Option<String>,
    pub added_at: String,
    pub updated_at: String,
}

pub fn normalize_tag(raw: &str) -> Option<String> {
    let normalized = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_lowercase();
    if normalized.is_empty() || normalized.len() > 160 {
        None
    } else {
        Some(normalized)
    }
}

pub fn provider_from_url(url: Option<&str>) -> String {
    let Some(url) = url else {
        return String::new();
    };
    let authority = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    authority
        .split('/')
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .trim_matches('.')
        .to_lowercase()
}

fn provider_from_metadata(metadata: &Value, source_url: Option<&str>) -> String {
    // gallery-dl sidecars usually carry `category` / `extractor`; prefer
    // those stable site identities over a CDN hostname embedded in the media
    // URL. The host remains the safe fallback for sparse sidecars.
    first_string(metadata, &["extractor", "category", "site", "basecategory"])
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty() && value.len() <= 120)
        .unwrap_or_else(|| provider_from_url(source_url))
}

pub fn get_or_create_tag(conn: &Connection, name: &str) -> rusqlite::Result<i64> {
    let Some(name) = normalize_tag(name) else {
        return Err(rusqlite::Error::InvalidQuery);
    };
    let now = now_iso();
    conn.execute(
        "INSERT INTO tags(name,added_at,last_used_at) VALUES(?1,?2,?2)
         ON CONFLICT(name) DO UPDATE SET last_used_at=excluded.last_used_at",
        params![name, now],
    )?;
    conn.query_row("SELECT id FROM tags WHERE name=?1", [name], |row| {
        row.get(0)
    })
}

/// Adds the canonical relation plus an immutable explanation of why it is
/// present. Several provenance rows can support one visible tag.
pub fn attach_tag(
    conn: &Connection,
    media_id: i64,
    name: &str,
    provenance: &str,
    source_tag_id: Option<i64>,
) -> Result<i64> {
    if !matches!(
        provenance,
        HUMAN | HUMAN_EDITED | SOURCE_APPROVED | AUTOMATIC | LEGACY
    ) {
        bail!("Unknown tag provenance");
    }
    let tag_id = get_or_create_tag(conn, name)?;
    conn.execute(
        "INSERT OR IGNORE INTO media_tags(media_id,tag_id) VALUES(?1,?2)",
        params![media_id, tag_id],
    )?;
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM media_tag_provenance
          WHERE media_id=?1 AND tag_id=?2 AND provenance=?3 AND source_tag_id IS ?4)",
        params![media_id, tag_id, provenance, source_tag_id],
        |row| row.get(0),
    )?;
    if !exists {
        conn.execute(
            "INSERT INTO media_tag_provenance(media_id,tag_id,source_tag_id,provenance,added_at)
             VALUES(?1,?2,?3,?4,?5)",
            params![media_id, tag_id, source_tag_id, provenance, now_iso()],
        )?;
    }
    Ok(tag_id)
}

fn string_from(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn first_string(metadata: &Value, keys: &[&str]) -> Option<String> {
    let object = metadata.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(string_from))
}

fn collect_tag_values(value: &Value, output: &mut BTreeSet<String>) {
    match value {
        Value::String(raw) => {
            if let Some(tag) = normalize_tag(raw) {
                output.insert(tag);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_tag_values(value, output);
            }
        }
        Value::Object(values) => {
            for key in ["name", "tag", "label", "title"] {
                if let Some(value) = values.get(key) {
                    collect_tag_values(value, output);
                    break;
                }
            }
        }
        _ => {}
    }
}

/// Extract only conventional tag-like fields. We do not recursively scour all
/// strings in source JSON; doing that turns filenames, descriptions, and URLs
/// into surprising tags.
pub fn extract_source_tags(metadata: &Value) -> Vec<String> {
    let mut tags = BTreeSet::new();
    if let Some(object) = metadata.as_object() {
        for key in [
            "tags",
            "tag",
            "categories",
            "category",
            "labels",
            "label",
            "keywords",
        ] {
            if let Some(value) = object.get(key) {
                collect_tag_values(value, &mut tags);
            }
        }
    }
    tags.into_iter().collect()
}

fn matching_rule(
    conn: &Connection,
    provider: &str,
    raw_name: &str,
) -> rusqlite::Result<Option<(String, Option<String>)>> {
    conn.query_row(
        "SELECT action,normalized_name FROM source_tag_rules
         WHERE raw_name=?1 AND provider IN (?2,'')
         ORDER BY CASE WHEN provider=?2 THEN 0 ELSE 1 END LIMIT 1",
        params![raw_name, provider],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

fn apply_rule_if_any(conn: &Connection, source_tag_id: i64) -> Result<()> {
    let row: Option<(i64, String, String, String)> = conn
        .query_row(
            "SELECT media_id,provider,raw_name,state FROM source_tags WHERE id=?1",
            [source_tag_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((media_id, provider, raw_name, state)) = row else {
        return Ok(());
    };
    if state != "pending" {
        return Ok(());
    }
    let Some((action, normalized_name)) = matching_rule(conn, &provider, &raw_name)? else {
        return Ok(());
    };
    match action.as_str() {
        "skip" => {
            conn.execute(
                "UPDATE source_tags SET state='skipped',reviewed_at=?1 WHERE id=?2",
                params![now_iso(), source_tag_id],
            )?;
        }
        "add" | "normalize" => {
            let name = normalized_name
                .as_deref()
                .and_then(normalize_tag)
                .or_else(|| normalize_tag(&raw_name))
                .ok_or_else(|| anyhow::anyhow!("Invalid remembered source-tag rule"))?;
            conn.execute(
                "UPDATE source_tags SET state='approved',normalized_name=?1,reviewed_at=?2 WHERE id=?3",
                params![name, now_iso(), source_tag_id],
            )?;
            attach_tag(conn, media_id, &name, SOURCE_APPROVED, Some(source_tag_id))?;
        }
        _ => bail!("Invalid remembered source-tag action"),
    }
    Ok(())
}

/// Stores exact source JSON and creates pending source-tag candidates. The
/// routine is idempotent: a late sidecar event updates its evidence without
/// undoing a prior approval/skip.
pub fn capture_source_metadata(
    conn: &Connection,
    media_id: i64,
    source_url: Option<&str>,
    metadata: &Value,
) -> Result<()> {
    let provider = provider_from_metadata(metadata, source_url);
    let raw_json = serde_json::to_string(metadata)?;
    let creator = first_string(
        metadata,
        &[
            "creator", "username", "author", "user", "artist", "uploader",
        ],
    );
    let title = first_string(metadata, &["title", "gallery_title", "post_title", "name"]);
    let source_url = source_url.unwrap_or("");
    conn.execute(
        "INSERT INTO source_metadata(media_id,provider,source_url,raw_json,creator,title,captured_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(media_id,provider,source_url) DO UPDATE SET
           raw_json=excluded.raw_json,creator=COALESCE(excluded.creator,source_metadata.creator),
           title=COALESCE(excluded.title,source_metadata.title),captured_at=excluded.captured_at",
        params![media_id, provider, source_url, raw_json, creator, title, now_iso()],
    )?;
    let metadata_id: i64 = conn.query_row(
        "SELECT id FROM source_metadata WHERE media_id=?1 AND provider=?2 AND source_url=?3",
        params![media_id, provider, source_url],
        |row| row.get(0),
    )?;
    for raw_name in extract_source_tags(metadata) {
        conn.execute(
            "INSERT INTO source_tags(media_id,source_metadata_id,provider,raw_name,added_at)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(media_id,provider,raw_name) DO UPDATE SET
               source_metadata_id=excluded.source_metadata_id",
            params![media_id, metadata_id, provider, raw_name, now_iso()],
        )?;
        let source_tag_id: i64 = conn.query_row(
            "SELECT id FROM source_tags WHERE media_id=?1 AND provider=?2 AND raw_name=?3",
            params![media_id, provider, raw_name],
            |row| row.get(0),
        )?;
        apply_rule_if_any(conn, source_tag_id)?;
    }
    Ok(())
}

pub fn pending_source_tags(
    conn: &Connection,
    provider: Option<&str>,
    limit: u32,
) -> Result<Vec<SourceTagCandidate>> {
    let limit = i64::from(limit.clamp(1, 250));
    let mut query = String::from(
        "SELECT st.id,st.media_id,m.filename,s.id,s.name,s.url,st.provider,st.raw_name,
                st.normalized_name,st.state,st.added_at
         FROM source_tags st JOIN media m ON m.id=st.media_id
         JOIN sources s ON s.id=m.source_id WHERE st.state='pending'",
    );
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(provider) = provider.filter(|provider| !provider.trim().is_empty()) {
        values.push(provider.trim().to_lowercase().into());
        query.push_str(&format!(" AND st.provider=?{}", values.len()));
    }
    values.push(limit.into());
    query.push_str(&format!(" ORDER BY st.id LIMIT ?{}", values.len()));
    let mut statement = conn.prepare(&query)?;
    let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), |row| {
        Ok(SourceTagCandidate {
            id: row.get(0)?,
            media_id: row.get(1)?,
            filename: row.get(2)?,
            source_id: row.get(3)?,
            source_name: row.get(4)?,
            source_url: row.get(5)?,
            provider: row.get(6)?,
            raw_name: row.get(7)?,
            normalized_name: row.get(8)?,
            state: row.get(9)?,
            added_at: row.get(10)?,
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

pub fn review_source_tag(
    conn: &Connection,
    id: i64,
    action: &str,
    normalized_name: Option<&str>,
) -> Result<SourceTagCandidate> {
    let row: SourceTagCandidate = conn.query_row(
        "SELECT st.id,st.media_id,m.filename,s.id,s.name,s.url,st.provider,st.raw_name,
                st.normalized_name,st.state,st.added_at
         FROM source_tags st JOIN media m ON m.id=st.media_id
         JOIN sources s ON s.id=m.source_id WHERE st.id=?1",
        [id],
        |row| {
            Ok(SourceTagCandidate {
                id: row.get(0)?,
                media_id: row.get(1)?,
                filename: row.get(2)?,
                source_id: row.get(3)?,
                source_name: row.get(4)?,
                source_url: row.get(5)?,
                provider: row.get(6)?,
                raw_name: row.get(7)?,
                normalized_name: row.get(8)?,
                state: row.get(9)?,
                added_at: row.get(10)?,
            })
        },
    )?;
    if row.state != "pending" {
        bail!("This source tag was already reviewed");
    }
    match action {
        "skip" => {
            conn.execute(
                "UPDATE source_tags SET state='skipped',reviewed_at=?1 WHERE id=?2",
                params![now_iso(), id],
            )?;
        }
        "add" | "edit" => {
            let name = normalized_name
                .and_then(normalize_tag)
                .or_else(|| row.normalized_name.as_deref().and_then(normalize_tag))
                .or_else(|| normalize_tag(&row.raw_name))
                .ok_or_else(|| anyhow::anyhow!("A normalized tag name is required"))?;
            conn.execute(
                "UPDATE source_tags SET state='approved',normalized_name=?1,reviewed_at=?2 WHERE id=?3",
                params![name, now_iso(), id],
            )?;
            attach_tag(
                conn,
                row.media_id,
                &name,
                if action == "edit" {
                    HUMAN_EDITED
                } else {
                    SOURCE_APPROVED
                },
                Some(id),
            )?;
        }
        _ => bail!("Review action must be add, edit, or skip"),
    }
    pending_source_tags(conn, None, 250)?
        .into_iter()
        .find(|candidate| candidate.id == id)
        .or_else(|| {
            // The just-reviewed item intentionally disappears from the
            // pending list; fetch its durable form for the response.
            conn.query_row(
                "SELECT st.id,st.media_id,m.filename,s.id,s.name,s.url,st.provider,st.raw_name,
                        st.normalized_name,st.state,st.added_at
                 FROM source_tags st JOIN media m ON m.id=st.media_id
                 JOIN sources s ON s.id=m.source_id WHERE st.id=?1",
                [id],
                |row| {
                    Ok(SourceTagCandidate {
                        id: row.get(0)?,
                        media_id: row.get(1)?,
                        filename: row.get(2)?,
                        source_id: row.get(3)?,
                        source_name: row.get(4)?,
                        source_url: row.get(5)?,
                        provider: row.get(6)?,
                        raw_name: row.get(7)?,
                        normalized_name: row.get(8)?,
                        state: row.get(9)?,
                        added_at: row.get(10)?,
                    })
                },
            )
            .ok()
        })
        .ok_or_else(|| anyhow::anyhow!("Reviewed source tag disappeared"))
}

pub fn upsert_rule(conn: &Connection, input: &RuleInput) -> Result<TagRule> {
    let raw_name = normalize_tag(&input.raw_name)
        .ok_or_else(|| anyhow::anyhow!("A source tag is required"))?;
    if !matches!(input.action.as_str(), "add" | "normalize" | "skip") {
        bail!("Rule action must be add, normalize, or skip");
    }
    let normalized_name = input.normalized_name.as_deref().and_then(normalize_tag);
    if input.action == "normalize" && normalized_name.is_none() {
        bail!("A normalized tag name is required for a normalize rule");
    }
    let provider = input
        .provider
        .as_deref()
        .map(|provider| provider.trim().to_lowercase())
        .filter(|provider| !provider.is_empty())
        .unwrap_or_default();
    let now = now_iso();
    conn.execute(
        "INSERT INTO source_tag_rules(provider,raw_name,action,normalized_name,added_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,?5)
         ON CONFLICT(provider,raw_name) DO UPDATE SET action=excluded.action,
           normalized_name=excluded.normalized_name,updated_at=excluded.updated_at",
        params![provider, raw_name, input.action, normalized_name, now],
    )?;
    conn.query_row(
        "SELECT id,provider,raw_name,action,normalized_name,added_at,updated_at
         FROM source_tag_rules WHERE provider=?1 AND raw_name=?2",
        params![provider, raw_name],
        |row| {
            let provider: String = row.get(1)?;
            Ok(TagRule {
                id: row.get(0)?,
                provider: (!provider.is_empty()).then_some(provider),
                raw_name: row.get(2)?,
                action: row.get(3)?,
                normalized_name: row.get(4)?,
                added_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        },
    )
    .map_err(Into::into)
}

pub fn list_rules(conn: &Connection) -> Result<Vec<TagRule>> {
    let mut statement = conn.prepare(
        "SELECT id,provider,raw_name,action,normalized_name,added_at,updated_at
         FROM source_tag_rules ORDER BY provider,raw_name",
    )?;
    let rows = statement.query_map([], |row| {
        let provider: String = row.get(1)?;
        Ok(TagRule {
            id: row.get(0)?,
            provider: (!provider.is_empty()).then_some(provider),
            raw_name: row.get(2)?,
            action: row.get(3)?,
            normalized_name: row.get(4)?,
            added_at: row.get(5)?,
            updated_at: row.get(6)?,
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

pub fn delete_rule(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM source_tag_rules WHERE id=?1", [id])? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::run_migrations(&conn).unwrap();
        conn.execute_batch("INSERT INTO sources(id,name,url,slug,added_at) VALUES(1,'source','https://example.test/a','s','now');
            INSERT INTO media(id,source_id,filepath,filename,type,added_at) VALUES(1,1,'a.jpg','a.jpg','image','now');").unwrap();
        conn
    }

    #[test]
    fn source_metadata_stays_pending_until_reviewed() {
        let conn = conn();
        capture_source_metadata(
            &conn,
            1,
            Some("https://kemono.su/post/1"),
            &serde_json::json!({
                "tags":["Blue Hair", "creator tag"], "creator":"Ada"
            }),
        )
        .unwrap();
        let pending = pending_source_tags(&conn, Some("kemono.su"), 10).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM media_tags", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let approved =
            review_source_tag(&conn, pending[0].id, "edit", Some("curated blue")).unwrap();
        assert_eq!(approved.state, "approved");
        assert_eq!(
            conn.query_row("SELECT name FROM tags", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "curated blue"
        );
        assert_eq!(
            conn.query_row("SELECT provenance FROM media_tag_provenance", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            HUMAN_EDITED
        );
    }

    #[test]
    fn remembered_provider_rule_auto_approves_only_that_provider() {
        let conn = conn();
        upsert_rule(
            &conn,
            &RuleInput {
                raw_name: "Raw Tag".into(),
                action: "normalize".into(),
                normalized_name: Some("clean tag".into()),
                provider: Some("bunkr.cr".into()),
            },
        )
        .unwrap();
        capture_source_metadata(
            &conn,
            1,
            Some("https://bunkr.cr/a/x"),
            &serde_json::json!({"tags":["raw tag"]}),
        )
        .unwrap();
        assert_eq!(pending_source_tags(&conn, None, 10).unwrap().len(), 0);
        assert_eq!(
            conn.query_row("SELECT name FROM tags", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "clean tag"
        );
    }
}
