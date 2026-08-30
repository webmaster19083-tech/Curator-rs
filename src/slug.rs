use once_cell::sync::Lazy;
use std::collections::HashSet;
use url::Url;

/// Path segments that are site structure, not creator handles.
/// Must match Python's _URL_SKIP_SEGMENTS exactly — changing this set
/// silently breaks slug generation and collapses sources.
static SKIP_SEGMENTS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "users", "user", "channel", "c", "artist", "profile", "gallery", "art",
        "en", "en-us", "ja", "de", "fr", "es", "member",
        "a",  // bunkr.cr /a/<code>
        // kemono.su / coomer.su service names
        "patreon", "fanbox", "fantia", "subscribestar", "gumroad", "boosty",
        "dlsite", "discord", "onlyfans", "fansly",
    ]
    .iter()
    .copied()
    .collect()
});

pub fn derive_name_from_url(url: &str) -> String {
    let Ok(parsed) = Url::parse(url) else {
        return url.to_string();
    };
    let host = parsed.host_str().unwrap_or("").to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let site = host.split('.').next().unwrap_or("site");

    let segments: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.filter(|seg| !seg.is_empty()).collect())
        .unwrap_or_default();

    let handle = segments
        .iter()
        .find(|seg| !SKIP_SEGMENTS.contains(seg.to_lowercase().as_str()))
        .map(|s| s.trim_start_matches('@').to_string());

    let handle = handle.unwrap_or_else(|| {
        segments.first().map(|s| s.trim_start_matches('@').to_string())
            .unwrap_or_else(|| host.to_string())
    });

    if handle.is_empty() {
        host.to_string()
    } else {
        format!("{handle} ({site})")
    }
}

/// Slugify text: lowercase, replace non-word chars with `-`, collapse, trim.
/// Must produce the same output as Python's slugify().
pub fn slugify(text: &str) -> String {
    use regex::Regex;
    static RE_NONWORD: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\w\-]+").unwrap());
    static RE_MULTI: Lazy<Regex> = Lazy::new(|| Regex::new(r"-+").unwrap());

    let lower = text.trim().to_lowercase();
    let s = RE_NONWORD.replace_all(&lower, "-");
    let s = RE_MULTI.replace_all(&s, "-");
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "source".to_string() } else { s }
}

pub fn normalize_url(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() { return String::new(); }
    let raw = if raw.contains("://") { raw.to_string() } else { format!("https://{raw}") };
    raw.trim_end_matches('/').to_string()
}

pub fn normalize_for_compare(url: &str) -> String {
    normalize_url(url).to_lowercase()
}
