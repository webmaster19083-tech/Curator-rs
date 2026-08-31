use once_cell::sync::Lazy;
use regex::Regex;
use sha1::{Digest, Sha1};
use std::collections::HashSet;

// ─── URL skip segments ────────────────────────────────────────────────────────
// Must be ported verbatim — the self-healing repair migration depends on the
// current logic producing a different result than the old buggy one.

static URL_SKIP_SEGMENTS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "users", "user", "channel", "c", "artist", "profile", "gallery", "art",
        "en", "en-us", "ja", "de", "fr", "es", "member",
        // bunkr.cr: /a/<code> — "a" is site structure, not a handle
        "a",
        // kemono.su / coomer.su: /<service>/user/<id>
        "patreon", "fanbox", "fantia", "subscribestar", "gumroad", "boosty",
        "dlsite", "discord", "onlyfans", "fansly",
    ]
    .iter()
    .copied()
    .collect()
});

// ─── derive_name_from_url ─────────────────────────────────────────────────────

pub fn derive_name_from_url(url: &str) -> String {
    // Extract host and path without using an external URL crate
    let without_scheme = if let Some(idx) = url.find("://") {
        &url[idx + 3..]
    } else {
        url
    };

    let (host_part, path_part) = if let Some(slash) = without_scheme.find('/') {
        (&without_scheme[..slash], &without_scheme[slash..])
    } else {
        (without_scheme, "")
    };

    // Strip port if present
    let host_part = host_part.split(':').next().unwrap_or(host_part);

    let host = host_part.to_lowercase();
    let host = host.trim_start_matches("www.");
    let site = host.split('.').next().unwrap_or("site");

    let segments: Vec<&str> = path_part.split('/')
        .filter(|s| !s.is_empty())
        .collect();

    // Walk segments, skipping known structural path components
    let mut handle: Option<&str> = None;
    for seg in &segments {
        if URL_SKIP_SEGMENTS.contains(seg.to_lowercase().as_str()) {
            continue;
        }
        handle = Some(seg);
        break;
    }

    // Fallback: first segment, or the host itself
    let handle = handle.unwrap_or_else(|| {
        segments.first().copied().unwrap_or(host_part)
    });

    let handle = handle.trim_start_matches('@');

    if handle.is_empty() {
        host.to_string()
    } else {
        format!("{} ({})", handle, site)
    }
}

// ─── slugify ──────────────────────────────────────────────────────────────────

static NON_SLUG_RE:  Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\w\-]+").unwrap());
static MULTI_DASH_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"-+").unwrap());

pub fn slugify(text: &str) -> String {
    let lowered = text.trim().to_lowercase();
    let s = NON_SLUG_RE.replace_all(&lowered, "-");
    let s = MULTI_DASH_RE.replace_all(&s, "-");
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "source".into() } else { s }
}

// ─── URL normalization ────────────────────────────────────────────────────────

pub fn normalize_url(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() { return String::new(); }
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{}", raw)
    };
    with_scheme.trim_end_matches('/').to_string()
}

pub fn normalize_for_compare(url: &str) -> String {
    normalize_url(url).to_lowercase()
}

pub fn split_bulk_input(raw: &str) -> Vec<String> {
    static SPLIT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\n,]+").unwrap());
    let mut urls = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for part in SPLIT_RE.split(raw) {
        let u = normalize_url(part);
        if !u.is_empty() && !seen.contains(&u) {
            seen.insert(u.clone());
            urls.push(u);
        }
    }
    urls
}

// ─── Pending filepath (synthetic, unique, never dereferenced) ────────────────

pub fn pending_filepath(source_id: i64, origin_url: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(origin_url.as_bytes());
    let digest = hex::encode(&hasher.finalize()[..8]);
    format!("__pending__/{}/{}", source_id, digest)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bunkr_album_skips_a() {
        let url = "https://bunkr.cr/a/ABCDEF123";
        let name = derive_name_from_url(url);
        assert_eq!(name, "ABCDEF123 (bunkr)");
    }

    #[test]
    fn kemono_skips_patreon_user() {
        let url = "https://kemono.su/patreon/user/someartist";
        let name = derive_name_from_url(url);
        assert_eq!(name, "someartist (kemono)");
    }

    #[test]
    fn plain_twitter() {
        let url = "https://twitter.com/someuser";
        let name = derive_name_from_url(url);
        assert_eq!(name, "someuser (twitter)");
    }

    #[test]
    fn at_handle_stripped() {
        let url = "https://twitter.com/@someuser";
        let name = derive_name_from_url(url);
        assert_eq!(name, "someuser (twitter)");
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("  --  "), "source");
    }
}
