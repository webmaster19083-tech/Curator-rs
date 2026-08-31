use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use tempfile::NamedTempFile;
use zip::{write::FileOptions, ZipWriter};

// ─── Rating → speed tag ──────────────────────────────────────────────────────

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

static SPEED_VALUES: &[&str] = &["slow", "medium", "fast", "cum", "succubus"];

pub fn is_speed_tag(tag: &str) -> bool {
    SPEED_VALUES.contains(&tag)
}

// ─── Manifest types ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ManifestEntry {
    file: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Serialize)]
struct Manifest {
    version:            &'static str,
    name:               String,
    author:             String,
    description:        String,
    preview:            &'static str,
    unlock_cost:        i64,
    required_challenge: &'static str,
    patreon_exclusive:  bool,
    media:              Vec<ManifestEntry>,
    social_links:       SocialLinks,
}

#[derive(Serialize)]
struct SocialLinks {
    onlyfans:     String,
    fansly:       String,
    twitter:      String,
    linktree:     String,
    manyvids:     String,
    redgifs:      String,
    discord:      String,
    patreon:      String,
    subscribestar: String,
    kofi:         String,
}

impl Default for SocialLinks {
    fn default() -> Self {
        SocialLinks {
            onlyfans: String::new(), fansly: String::new(), twitter: String::new(),
            linktree: String::new(), manyvids: String::new(), redgifs: String::new(),
            discord: String::new(), patreon: String::new(),
            subscribestar: String::new(), kofi: String::new(),
        }
    }
}

// ─── Media row for export ─────────────────────────────────────────────────────

pub struct ExportRow {
    pub filepath: String,
    pub kind:     String,   // "image" or "video"
    pub rating:   i64,
    pub tags:     Vec<String>,
}

// ─── build_chpack ─────────────────────────────────────────────────────────────
// Streams files directly into a NamedTempFile — no full-library RAM bomb.

pub fn build_chpack(
    pack_name:   String,
    author:      String,
    description: String,
    unlock_cost: i64,
    rows:        Vec<ExportRow>,
    library_dir: &Path,
) -> Result<NamedTempFile> {
    let tmp = NamedTempFile::new().context("creating temp file for .chpack")?;
    let tmp_file = tmp.reopen().context("reopening temp file")?;
    let mut zip = ZipWriter::new(tmp_file);

    let options: FileOptions<()> = FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(6));

    let mut media_entries = Vec::new();
    let mut idx = 0usize;

    for row in &rows {
        let src_path = dunce::simplified(&library_dir.join(&row.filepath)).to_path_buf();
        if !src_path.exists() { continue; }

        let ext = src_path.extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e.to_lowercase()))
            .unwrap_or_default();

        let speed_tag = rating_to_speed(row.rating);

        // Strip curator tags that would collide with speed tag names
        let own_tags: Vec<String> = row.tags.iter()
            .filter(|t| !is_speed_tag(t.as_str()))
            .cloned()
            .collect();

        let mut parts = vec![idx.to_string()];
        parts.extend(own_tags.iter().cloned());
        if let Some(spd) = speed_tag { parts.push(spd.to_string()); }

        let archive_filename = format!("{}{}", parts.join("_"), ext);
        let arc_path = format!("media/{}", archive_filename);

        zip.start_file(&arc_path, options)?;
        let mut f = std::fs::File::open(&src_path)
            .with_context(|| format!("opening {:?}", src_path))?;
        std::io::copy(&mut f, &mut zip)?;

        let file_kind = if row.kind == "video" { "video" } else { "image" };
        media_entries.push(ManifestEntry { file: archive_filename, kind: file_kind.to_string() });

        idx += 1;
    }

    if media_entries.is_empty() {
        anyhow::bail!("No accessible files on disk for this selection");
    }

    let manifest = Manifest {
        version:            "0.02a",
        name:               pack_name,
        author,
        description,
        preview:            "",
        unlock_cost,
        required_challenge: "",
        patreon_exclusive:  false,
        media:              media_entries,
        social_links:       SocialLinks::default(),
    };

    zip.start_file("manifest.json", options)?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;

    zip.finish()?;
    Ok(tmp)
}

// ─── Safe filename for download header ───────────────────────────────────────

static UNSAFE_RE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new(r"[^\w\-. ]").unwrap());

pub fn safe_pack_filename(name: &str) -> String {
    let s = UNSAFE_RE.replace_all(name.trim(), "_");
    format!("{}.chpack", s.replace(' ', "_"))
}
