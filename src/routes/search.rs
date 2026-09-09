//! Provider-normalized discovery search.
//!
//! Curator never downloads search results itself. Providers return source or
//! gallery URLs and the selected compatible URLs are handed to the established
//! gallery-dl source queue in `routes::sources`.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use futures::StreamExt;
use once_cell::sync::Lazy;
use regex::Regex;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::media::db_err;
use crate::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub creator: Option<String>,
    pub thumbnail: Option<String>,
    pub source: String,
    pub source_url: String,
    pub provider: String,
    pub result_type: String,
    pub item_count: Option<i64>,
    pub date: Option<String>,
    pub gallery_dl_compatible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relevance: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct SearchQuery {
    #[serde(alias = "q")]
    pub query: Option<String>,
    pub provider: Option<String>,
    pub result_type: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<usize>,
}

/// Provider implementations are intentionally backend-only. The UI receives
/// only the common `SearchResult` shape and does not accumulate one-off
/// site-specific controls or downloader code.
trait SearchProvider: Send + Sync {
    fn id(&self) -> &'static str;

    /// Local catalog work is deliberately synchronous: a rusqlite connection
    /// must never be held across a network await in an Axum handler.
    fn search_catalog(
        &self,
        _conn: &Connection,
        _query: &str,
    ) -> rusqlite::Result<Vec<SearchResult>> {
        Ok(Vec::new())
    }

    /// Remote providers get a bounded async hook, while keeping their
    /// provider-specific protocol out of the UI and out of the downloader.
    fn search_remote<'a>(
        &'a self,
        _query: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchResult>, String>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

struct CuratorCatalogProvider;

impl SearchProvider for CuratorCatalogProvider {
    fn id(&self) -> &'static str {
        "gallery-dl"
    }

    fn search_catalog(
        &self,
        conn: &Connection,
        query: &str,
    ) -> rusqlite::Result<Vec<SearchResult>> {
        let needle = format!("%{}%", query.trim());
        let mut results = Vec::new();
        let mut sources = conn.prepare(
            "SELECT s.name,s.url,s.item_count,s.added_at,
                    (SELECT sm.creator FROM source_metadata sm
                     JOIN media m ON m.id=sm.media_id WHERE m.source_id=s.id
                     ORDER BY sm.id DESC LIMIT 1) AS creator
             FROM sources s WHERE s.name LIKE ?1 COLLATE NOCASE OR s.url LIKE ?1 COLLATE NOCASE
             ORDER BY s.item_count DESC,s.id DESC LIMIT 80",
        )?;
        for result in sources
            .query_map([&needle], |row| {
                Ok(SearchResult {
                    title: row.get(0)?,
                    creator: row.get(4)?,
                    thumbnail: None,
                    source: "Curator library".to_string(),
                    source_url: row.get(1)?,
                    provider: "gallery-dl".to_string(),
                    result_type: "album".to_string(),
                    item_count: row.get(2)?,
                    date: row.get(3)?,
                    gallery_dl_compatible: true,
                    relevance: Some(100),
                })
            })?
            .flatten()
        {
            results.push(result);
        }

        let mut creators = conn.prepare(
            "SELECT sm.creator,COUNT(DISTINCT m.id),MIN(s.url),MAX(sm.captured_at)
             FROM source_metadata sm JOIN media m ON m.id=sm.media_id
             JOIN sources s ON s.id=m.source_id
             WHERE sm.creator IS NOT NULL AND sm.creator<>''
               AND sm.creator LIKE ?1 COLLATE NOCASE
             GROUP BY sm.creator ORDER BY COUNT(DISTINCT m.id) DESC,sm.creator COLLATE NOCASE LIMIT 80",
        )?;
        for result in creators
            .query_map([&needle], |row| {
                let title: String = row.get(0)?;
                Ok(SearchResult {
                    creator: Some(title.clone()),
                    title,
                    thumbnail: None,
                    source: "Curator library".to_string(),
                    source_url: row.get(2)?,
                    provider: "gallery-dl".to_string(),
                    result_type: "creator".to_string(),
                    item_count: row.get(1)?,
                    date: row.get(3)?,
                    gallery_dl_compatible: true,
                    relevance: Some(90),
                })
            })?
            .flatten()
        {
            results.push(result);
        }
        Ok(results)
    }
}

/// Public Bunkr-album index. It only discovers pages; downloading every
/// selected result still goes through Curator's normal gallery-dl source
/// queue. The request is fixed to the provider's own HTTPS origin, so search
/// text cannot turn this into an arbitrary server-side fetch.
struct BalbumsIndexProvider;

impl SearchProvider for BalbumsIndexProvider {
    fn id(&self) -> &'static str {
        "balbums"
    }

    fn search_remote<'a>(
        &'a self,
        query: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchResult>, String>> + Send + 'a>> {
        Box::pin(async move { search_balbums(query).await })
    }
}

const BALBUMS_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

static BALBUMS_CARD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)<a\s+href="(?P<url>[^"]+)"(?P<attrs>[^>]*)>(?P<body>.*?)</a>"#)
        .expect("valid balbums card matcher")
});
static BALBUMS_TITLE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)<h3[^>]*>(?P<title>.*?)</h3>"#).expect("valid balbums title matcher")
});
static BALBUMS_IMAGE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)<img\s+[^>]*src="(?P<url>[^"]+)"[^>]*>"#)
        .expect("valid balbums image matcher")
});
static BALBUMS_COUNT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)>\s*(?P<count>\d+)\s+files?\s*</span>"#)
        .expect("valid balbums file-count matcher")
});
static HTML_TAGS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)<[^>]+>").expect("valid HTML tag matcher"));

fn is_balbums_url(value: &str) -> bool {
    value
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_ascii_lowercase()
        .starts_with("balbums.st/")
}

fn is_bunkr_album_url(value: &str) -> bool {
    let Some((_, authority_and_path)) = value.split_once("://") else {
        return false;
    };
    let mut parts = authority_and_path.split('/');
    let host = parts
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    let path = parts.collect::<Vec<_>>().join("/");
    (host.eq_ignore_ascii_case("bunkr.cr") || host.to_ascii_lowercase().ends_with(".bunkr.cr"))
        && path.starts_with("a/")
}

fn html_text(value: &str) -> String {
    HTML_TAGS
        .replace_all(value, " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_balbums_html(html: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    for card in BALBUMS_CARD.captures_iter(html) {
        let url = card
            .name("url")
            .map(|value| value.as_str())
            .unwrap_or_default();
        let attrs = card
            .name("attrs")
            .map(|value| value.as_str())
            .unwrap_or_default();
        if !attrs.to_ascii_lowercase().contains("card") || !is_bunkr_album_url(url) {
            continue;
        }
        let body = card
            .name("body")
            .map(|value| value.as_str())
            .unwrap_or_default();
        let Some(title) = BALBUMS_TITLE
            .captures(body)
            .and_then(|value| value.name("title"))
            .map(|value| html_text(value.as_str()))
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        // The provider's decorative Bunkr logo appears before a real cover,
        // so prefer the last absolute image in a card.
        let thumbnail = BALBUMS_IMAGE
            .captures_iter(body)
            .filter_map(|value| value.name("url").map(|url| url.as_str()))
            .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
            .last()
            .map(ToOwned::to_owned);
        let item_count = BALBUMS_COUNT
            .captures(body)
            .and_then(|value| value.name("count"))
            .and_then(|value| value.as_str().parse::<i64>().ok());
        results.push(SearchResult {
            title,
            creator: None,
            thumbnail,
            source: "balbums.st".to_string(),
            source_url: url.to_string(),
            provider: "balbums".to_string(),
            result_type: "album".to_string(),
            item_count,
            date: None,
            gallery_dl_compatible: true,
            relevance: Some(80),
        });
    }
    results
}

async fn search_balbums(query: &str) -> Result<Vec<SearchResult>, String> {
    let trimmed = query.trim();
    if is_balbums_url(trimmed) {
        let source_url = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            trimmed.to_string()
        } else {
            format!("https://{trimmed}")
        };
        return Ok(vec![SearchResult {
            title: "balbums.st collection".to_string(),
            creator: None,
            thumbnail: None,
            source: "balbums.st".to_string(),
            source_url,
            provider: "balbums".to_string(),
            result_type: "collection".to_string(),
            item_count: None,
            date: None,
            // An index URL must be resolved to its source page before it can
            // enter gallery-dl. The UI keeps it available for preview/open.
            gallery_dl_compatible: false,
            relevance: Some(100),
        }]);
    }
    // URLs belong to the direct gallery-dl provider. Do not send them to an
    // unrelated index search endpoint.
    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        return Ok(Vec::new());
    }
    let url = format!(
        "https://balbums.st/?mode=broad&page=1&per=50&search={}&sort=latest",
        urlencoding::encode(trimmed)
    );
    let client = reqwest::Client::builder()
        .user_agent("Curator/0.1 discovery")
        .connect_timeout(std::time::Duration::from_secs(4))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| format!("could not initialize balbums provider: {error}"))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("balbums.st is unavailable: {error}"))?
        .error_for_status()
        .map_err(|error| format!("balbums.st search failed: {error}"))?;
    if response
        .content_length()
        .is_some_and(|size| size as usize > BALBUMS_MAX_RESPONSE_BYTES)
    {
        return Err("balbums.st returned an unexpectedly large search response".to_string());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("reading balbums.st results: {error}"))?;
        if bytes.len().saturating_add(chunk.len()) > BALBUMS_MAX_RESPONSE_BYTES {
            return Err("balbums.st returned an unexpectedly large search response".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    let html = String::from_utf8(bytes)
        .map_err(|_| "balbums.st returned non-text search data".to_string())?;
    Ok(parse_balbums_html(&html))
}

fn direct_url_result(query: &str) -> Option<SearchResult> {
    let value = query.trim();
    if !(value.starts_with("https://") || value.starts_with("http://")) {
        return None;
    }
    let host = value
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(value)
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(SearchResult {
        title: value.to_string(),
        creator: None,
        thumbnail: None,
        source: host.clone(),
        source_url: value.to_string(),
        provider: "gallery-dl".to_string(),
        result_type: "post".to_string(),
        item_count: None,
        date: None,
        gallery_dl_compatible: !host.ends_with("balbums.st"),
        relevance: Some(110),
    })
}

pub async fn search(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let text = query.query.unwrap_or_default().trim().to_string();
    if text.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"A search query is required"})),
        ));
    }
    if text.len() > 2_000 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Search query is too long"})),
        ));
    }
    let requested = query
        .provider
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !requested.is_empty() && !["gallery-dl", "balbums"].contains(&requested.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Unknown search provider"})),
        ));
    }
    let catalog = CuratorCatalogProvider;
    let balbums = BalbumsIndexProvider;
    let mut results = Vec::new();
    let mut provider_errors = Vec::new();
    if requested.is_empty() || requested == "gallery-dl" {
        // Keep the SQLite connection inside this scope. External providers
        // are awaited afterwards, so a pooled rusqlite connection is never
        // accidentally retained across asynchronous network work.
        let catalog_results = {
            let conn = state.pool.get().map_err(db_err)?;
            catalog.search_catalog(&conn, &text).map_err(db_err)?
        };
        results.extend(catalog_results);
        if let Some(result) = direct_url_result(&text) {
            results.push(result);
        }
    }
    if requested.is_empty() || requested == balbums.id() {
        match balbums.search_remote(&text).await {
            Ok(external_results) => results.extend(external_results),
            Err(error) => provider_errors.push(json!({
                "provider": balbums.id(),
                "error": error,
            })),
        }
    }

    let requested_type = query.result_type.unwrap_or_default().to_ascii_lowercase();
    if !requested_type.is_empty() {
        results.retain(|result| result.result_type == requested_type);
    }
    let mut seen = HashSet::new();
    results.retain(|result| {
        seen.insert((
            result.provider.clone(),
            result.source_url.clone(),
            result.result_type.clone(),
        ))
    });
    match query.sort.as_deref().unwrap_or("relevance") {
        "relevance" => results.sort_by(|a, b| {
            b.relevance
                .cmp(&a.relevance)
                .then_with(|| a.title.cmp(&b.title))
        }),
        "date_desc" => {
            results.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.title.cmp(&b.title)))
        }
        "date_asc" => {
            results.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.title.cmp(&b.title)))
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"Unknown search sort"})),
            ))
        }
    }
    results.truncate(query.limit.unwrap_or(100).clamp(1, 250));
    Ok(Json(json!({
        "results":results,
        "providers":[catalog.id(),balbums.id()],
        "provider_errors":provider_errors,
    })))
}

#[derive(Debug, Deserialize)]
pub struct DownloadSearchResultsBody {
    #[serde(default)]
    pub results: Vec<SearchResult>,
}

pub async fn download_selected(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DownloadSearchResultsBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if body.results.is_empty() || body.results.len() > 500 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Select between one and 500 compatible search results"})),
        ));
    }
    let urls = body
        .results
        .into_iter()
        .filter(|result| result.gallery_dl_compatible)
        .map(|result| result.source_url.trim().to_string())
        .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
        .collect::<Vec<_>>();
    if urls.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Selected results do not yet resolve to gallery-dl source URLs"})),
        ));
    }
    let queued = super::sources::create_sources_from_urls(state, urls).await?;
    Ok(Json(
        json!({"queued":queued,"status":"queued_with_gallery_dl"}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;

    #[tokio::test]
    async fn direct_urls_use_existing_source_queue_contract() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        let result = search(
            State(state),
            Query(SearchQuery {
                query: Some("https://example.test/post/1".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(result["results"][0]["provider"], "gallery-dl");
        assert_eq!(result["results"][0]["gallery_dl_compatible"], true);
    }

    #[tokio::test]
    async fn balbums_index_entries_are_not_sent_to_the_downloader_until_resolved() {
        let root = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(root.path());
        let result = search(
            State(state),
            Query(SearchQuery {
                query: Some("balbums.st/collection/example".into()),
                provider: Some("balbums".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(result["results"][0]["gallery_dl_compatible"], false);
    }

    #[test]
    fn balbums_cards_are_normalized_to_gallery_dl_album_results() {
        let html = r#"
            <a href="https://bunkr.cr/a/curator-test" target="_blank" class="card search-card">
              <img src="/assets/bunkr.svg" alt="Bunkr">
              <img src="https://cdn.example.test/cover.jpg" alt="Cover">
              <h3>Example &amp; Gallery</h3>
              <span>42 files</span>
            </a>
        "#;

        let results = parse_balbums_html(html);
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(result.title, "Example & Gallery");
        assert_eq!(result.source_url, "https://bunkr.cr/a/curator-test");
        assert_eq!(
            result.thumbnail.as_deref(),
            Some("https://cdn.example.test/cover.jpg")
        );
        assert_eq!(result.item_count, Some(42));
        assert!(result.gallery_dl_compatible);
    }
}
