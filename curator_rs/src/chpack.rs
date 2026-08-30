use std::{
    collections::HashSet,
    io::Write,
    path::{Path, PathBuf},
};
use anyhow::Result;
use tempfile::NamedTempFile;
use zip::{write::FileOptions, ZipWriter, CompressionMethod};

static SPEED_TAGS: &[(&str, &str)] = &[
    // rating value → speed tag
];

/// Map Curator rating (1-5) to CockHero speed tag.
/// 0 = unrated → no tag (game distributes randomly).
pub fn rating_to_speed(rating: i64) -> Option<&'static str> {
    match rating {
        1 => Some("slow"),
        2 => Some("medium"),
        3 => Some("fast"),
        4 => Some("cum"),
        5 => Some("succubus"),
        _ => None,
    }
}

static SPEED_TAG_SET: once_cell::sync::Lazy<HashSet<&'static str>> =
    once_cell::sync::Lazy::new(|| {
        ["slow", "medium", "fast", "cum", "succubus"].iter().copied().collect()
    });

pub struct MediaRow {
    pub filepath: String,
    pub rating: i64,
    pub tags_csv: Option<String>,
    pub media_type: String, // "image" | "video"
}

pub struct ChpackOptions {
    pub name: String,
    pub author: String,
    pub description: String,
    pub unlock_cost: i64,
}

/// Build a .chpack (ZIP) in a NamedTempFile. Returns the tempfile to stream back.
/// Streams each media file — never buffers the full archive in RAM.
pub fn build_chpack(
    entries: &[MediaRow],
    library_dir: &Path,
    opts: &ChpackOptions,
) -> Result<NamedTempFile> {
    let tmp = NamedTempFile::new()?;
    let file = tmp.reopen()?;
    let mut zip = ZipWriter::new(file);
    let file_opts: FileOptions<()> = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(6));

    let mut media_entries = Vec::new();

    for (idx, row) in entries.iter().enumerate() {
        let src = dunce::simplified(&library_dir.join(&row.filepath)).to_path_buf();
        if !src.exists() { continue; }

        let ext = PathBuf::from(&row.filepath)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        // Build filename: {idx}_{curator_tags}_{speed}.{ext}
        // Curator tags that collide with speed tag set are stripped.
        let own_tags: Vec<String> = row.tags_csv
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty() && !SPEED_TAG_SET.contains(t.as_str()))
            .collect();

        let mut parts: Vec<String> = vec![idx.to_string()];
        parts.extend(own_tags);
        if let Some(speed) = rating_to_speed(row.rating) {
            parts.push(speed.to_string());
        }
        let archive_name = format!("{}.{ext}", parts.join("_"));
        let arc_path = format!("media/{archive_name}");

        zip.start_file(&arc_path, file_opts)?;
        let mut f = std::fs::File::open(&src)?;
        std::io::copy(&mut f, &mut zip)?;

        let file_type = if row.media_type == "video" { "video" } else { "image" };
        media_entries.push(serde_json::json!({
            "file": archive_name,
            "type": file_type,
        }));
    }

    if media_entries.is_empty() {
        anyhow::bail!("No accessible files on disk for this selection");
    }

    // Write manifest.json
    let manifest = serde_json::json!({
        "version": "0.02a",
        "name": opts.name,
        "author": opts.author,
        "description": opts.description,
        "preview": "",
        "unlock_cost": opts.unlock_cost,
        "required_challenge": "",
        "patreon_exclusive": false,
        "media": media_entries,
        "social_links": {
            "onlyfans": "", "fansly": "", "twitter": "",
            "linktree": "", "manyvids": "", "redgifs": "",
            "discord": "", "patreon": "", "subscribestar": "", "kofi": ""
        }
    });

    zip.start_file("manifest.json", file_opts)?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;
    zip.finish()?;

    Ok(tmp)
}
