//! Durable source-metadata evidence and reviewable tags.
//!
//! Gallery-dl metadata can be useful but noisy.  This module preserves it,
//! presents tag-like fields for review, and never lets an adapter silently
//! overwrite a person's visible organization tags.

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
        .to_ascii_lowercase();
    (!normalized.is_empty() && normalized.len() <= 160).then_some(normalized)
}

fn provider_from_url(url: Option<&str>) -> String {
    let Some(url) = url else { return String::new(); };
    url.split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or_default()
        .split('@')
        .next_back()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_matches('.')
        .to_ascii_lowercase()
}

fn first_string(metadata: &Value, keys: &[&str]) -> Option<String> {
    let object = metadata.as_object()?;
    keys.iter().find_map(|key| {
        object
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn provider_from_metadata(metadata: &Value, source_url: Option<&str>) -> String {
    first_string(metadata, &["extractor", "category", "site", "basecategory"])
        .map(|value| value.to_ascii_lowercase())
        .filter(|value| value.len() <= 120)
        .unwrap_or_else(|| provider_from_url(source_url))
}

pub fn get_or_create_tag(conn: &Connection, name: &str) -> rusqlite::Result<i64> {
    let Some(name) = normalize_tag(name) else { return Err(rusqlite::Error::InvalidQuery); };
    conn.execute(
        "INSERT OR IGNORE INTO tags(name,added_at) VALUES(?1,?2)",
        params![name, now_iso()],
    )?;
    conn.query_row("SELECT id FROM tags WHERE name=?1", [name], |row| row.get(0))
}

/// Add a visible tag and record why it exists.  The primary-key provenance
/// table is intentionally compact for compatibility with earlier installs;
/// the first durable reason wins rather than duplicating the visible tag.
pub fn attach_tag(
    conn: &Connection,
    media_id: i64,
    name: &str,
    provenance: &str,
    _source_tag_id: Option<i64>,
) -> Result<i64> {
    if !matches!(provenance, HUMAN | HUMAN_EDITED | SOURCE_APPROVED | AUTOMATIC | LEGACY) {
        bail!("Unknown tag provenance");
    }
    let tag_id = get_or_create_tag(conn, name)?;
    conn.execute(
        "INSERT OR IGNORE INTO media_tags(media_id,tag_id) VALUES(?1,?2)",
        params![media_id, tag_id],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO media_tag_provenance(media_id,tag_id,provenance,added_at)
         VALUES(?1,?2,?3,?4)",
        params![media_id, tag_id, provenance, now_iso()],
    )?;
    Ok(tag_id)
}

fn collect_tag_values(value: &Value, output: &mut BTreeSet<String>) {
    match value {
        Value::String(raw) => {
            if let Some(tag) = normalize_tag(raw) { output.insert(tag); }
        }
        Value::Array(values) => values.iter().for_each(|value| collect_tag_values(value, output)),
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

fn extract_source_tags(metadata: &Value) -> Vec<String> {
    let mut tags = BTreeSet::new();
    if let Some(object) = metadata.as_object() {
        for key in ["tags", "tag", "categories", "labels", "keywords"] {
            if let Some(value) = object.get(key) { collect_tag_values(value, &mut tags); }
        }
    }
    tags.into_iter().collect()
}

fn matching_rule(conn: &Connection, provider: &str, raw_name: &str) -> rusqlite::Result<Option<(String, Option<String>)>> {
    conn.query_row(
        "SELECT action,normalized_name FROM source_tag_rules
         WHERE raw_name=?1 AND provider IN (?2,'')
         ORDER BY CASE WHEN provider=?2 THEN 0 ELSE 1 END LIMIT 1",
        params![raw_name, provider],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

fn candidate_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceTagCandidate> {
    Ok(SourceTagCandidate {
        id: row.get(0)?, media_id: row.get(1)?, filename: row.get(2)?,
        source_id: row.get(3)?, source_name: row.get(4)?, source_url: row.get(5)?,
        provider: row.get(6)?, raw_name: row.get(7)?, normalized_name: row.get(8)?,
        state: row.get(9)?, added_at: row.get(10)?,
    })
}

fn load_candidate(conn: &Connection, id: i64) -> Result<SourceTagCandidate> {
    conn.query_row(
        "SELECT st.id,st.media_id,m.filename,s.id,s.name,s.url,st.provider,st.raw_name,
                st.normalized_name,st.state,st.added_at
         FROM source_tags st JOIN media m ON m.id=st.media_id
         JOIN sources s ON s.id=m.source_id WHERE st.id=?1",
        [id], candidate_from_row,
    )
    .map_err(Into::into)
}

fn apply_rule_if_any(conn: &Connection, source_tag_id: i64) -> Result<()> {
    let candidate = load_candidate(conn, source_tag_id)?;
    if candidate.state != "pending" { return Ok(()); }
    let Some((action, normalized_name)) = matching_rule(conn, &candidate.provider, &candidate.raw_name)? else {
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
                .or_else(|| normalize_tag(&candidate.raw_name))
                .ok_or_else(|| anyhow::anyhow!("Invalid remembered source-tag rule"))?;
            conn.execute(
                "UPDATE source_tags SET state='approved',normalized_name=?1,reviewed_at=?2 WHERE id=?3",
                params![name, now_iso(), source_tag_id],
            )?;
            attach_tag(conn, candidate.media_id, &name, SOURCE_APPROVED, Some(source_tag_id))?;
        }
        _ => bail!("Invalid remembered source-tag action"),
    };
    Ok(())
}

/// Store source JSON and create review candidates. A later sidecar refresh
/// updates evidence but never resets an existing approval or skip.
pub fn capture_source_metadata(
    conn: &Connection,
    media_id: i64,
    source_url: Option<&str>,
    metadata: &Value,
) -> Result<()> {
    let provider = provider_from_metadata(metadata, source_url);
    let source_url = source_url.unwrap_or("");
    let raw_json = serde_json::to_string(metadata)?;
    let creator = first_string(metadata, &["creator", "username", "author", "user", "artist", "uploader"]);
    let title = first_string(metadata, &["title", "gallery_title", "post_title", "name"]);
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

pub fn pending_source_tags(conn: &Connection, provider: Option<&str>, limit: u32) -> Result<Vec<SourceTagCandidate>> {
    let mut query = String::from(
        "SELECT st.id,st.media_id,m.filename,s.id,s.name,s.url,st.provider,st.raw_name,
                st.normalized_name,st.state,st.added_at
         FROM source_tags st JOIN media m ON m.id=st.media_id
         JOIN sources s ON s.id=m.source_id WHERE st.state='pending'",
    );
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(provider) = provider.filter(|provider| !provider.trim().is_empty()) {
        values.push(provider.trim().to_ascii_lowercase().into());
        query.push_str(&format!(" AND st.provider=?{}", values.len()));
    }
    values.push(i64::from(limit.clamp(1, 250)).into());
    query.push_str(&format!(" ORDER BY st.id LIMIT ?{}", values.len()));
    let mut statement = conn.prepare(&query)?;
    let rows = statement.query_map(rusqlite::params_from_iter(values.iter()), candidate_from_row)?;
    Ok(rows.filter_map(Result::ok).collect())
}

pub fn review_source_tag(conn: &Connection, id: i64, action: &str, normalized_name: Option<&str>) -> Result<SourceTagCandidate> {
    let candidate = load_candidate(conn, id)?;
    if candidate.state != "pending" { bail!("This source tag was already reviewed"); }
    match action {
        "skip" => { conn.execute("UPDATE source_tags SET state='skipped',reviewed_at=?1 WHERE id=?2", params![now_iso(), id])?; }
        "add" | "edit" => {
            let name = normalized_name
                .and_then(normalize_tag)
                .or_else(|| candidate.normalized_name.as_deref().and_then(normalize_tag))
                .or_else(|| normalize_tag(&candidate.raw_name))
                .ok_or_else(|| anyhow::anyhow!("A normalized tag name is required"))?;
            conn.execute(
                "UPDATE source_tags SET state='approved',normalized_name=?1,reviewed_at=?2 WHERE id=?3",
                params![name, now_iso(), id],
            )?;
            attach_tag(conn, candidate.media_id, &name, if action == "edit" { HUMAN_EDITED } else { SOURCE_APPROVED }, Some(id))?;
        }
        _ => bail!("Review action must be add, edit, or skip"),
    }
    load_candidate(conn, id)
}

pub fn upsert_rule(conn: &Connection, input: &RuleInput) -> Result<TagRule> {
    let raw_name = normalize_tag(&input.raw_name).ok_or_else(|| anyhow::anyhow!("A source tag is required"))?;
    if !matches!(input.action.as_str(), "add" | "normalize" | "skip") { bail!("Rule action must be add, normalize, or skip"); }
    let normalized_name = input.normalized_name.as_deref().and_then(normalize_tag);
    if input.action == "normalize" && normalized_name.is_none() { bail!("A normalized tag name is required for a normalize rule"); }
    let provider = input.provider.as_deref().map(|value| value.trim().to_ascii_lowercase()).filter(|value| !value.is_empty()).unwrap_or_default();
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
                id: row.get(0)?, provider: (!provider.is_empty()).then_some(provider),
                raw_name: row.get(2)?, action: row.get(3)?, normalized_name: row.get(4)?,
                added_at: row.get(5)?, updated_at: row.get(6)?,
            })
        },
    ).map_err(Into::into)
}

pub fn list_rules(conn: &Connection) -> Result<Vec<TagRule>> {
    let mut statement = conn.prepare(
        "SELECT id,provider,raw_name,action,normalized_name,added_at,updated_at
         FROM source_tag_rules ORDER BY provider,raw_name",
    )?;
    let rows = statement.query_map([], |row| {
        let provider: String = row.get(1)?;
        Ok(TagRule {
            id: row.get(0)?, provider: (!provider.is_empty()).then_some(provider),
            raw_name: row.get(2)?, action: row.get(3)?, normalized_name: row.get(4)?,
            added_at: row.get(5)?, updated_at: row.get(6)?,
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

pub fn delete_rule(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM source_tag_rules WHERE id=?1", [id])? > 0)
}
