use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures::stream::{self, BoxStream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::downloader::{preview_walk, spawn_gallery_dl};
use crate::slug::normalize_url;
use crate::AppState;

// ─── GET /api/preview/scan ───────────────────────────────────────────────────
//
// Streams results as Server-Sent Events instead of one blocking JSON
// response. Each discovered media item is emitted as its own `item` event
// the moment it's parsed out of gallery-dl's output; a final `done` event
// closes out the scan, or an `error` event carries a failure message.
//
// Note on why this doesn't emit events the instant gallery-dl finds each
// file: `gallery-dl -j` collects everything internally and only prints the
// complete JSON array once, at the very end of the run — there's no
// intermediate flush to read incrementally. What streams here is still
// real: the SSE connection stays alive (with keep-alive pings) for the
// full duration of the scan rather than the client sitting on a bare,
// timeout-prone `fetch()`, and once gallery-dl's output does land, items
// are pushed to the browser one at a time instead of as a single large
// payload the client has to deserialize in one shot.
#[derive(Deserialize)]
pub struct ScanQuery {
    pub url: String,
}

pub async fn scan(
    State(state): State<Arc<AppState>>,
    Query(q):     Query<ScanQuery>,
) -> Sse<BoxStream<'static, Result<Event, Infallible>>> {
    let url = normalize_url(&q.url);

    let stream: BoxStream<'static, Result<Event, Infallible>> = if url.is_empty() {
        let evt = Ok(Event::default().event("error").data("No URL provided"));
        Box::pin(stream::once(async move { evt }))
    } else {
        let args = vec!["-j".into(), "--no-download".into(), url.clone()];
        let inner = spawn_gallery_dl(args, None, Arc::clone(&state));

        Box::pin(async_stream::stream! {
            let mut seen      = std::collections::HashSet::new();
            let mut found_any = false;
            let mut had_error = false;

            futures::pin_mut!(inner);
            while let Some(node_result) = inner.next().await {
                match node_result {
                    Ok(node) => {
                        let mut items = Vec::new();
                        preview_walk(&node, &url, &mut items, &mut seen);
                        for item in items {
                            found_any = true;
                            let data = serde_json::to_string(&item).unwrap_or_default();
                            yield Ok(Event::default().event("item").data(data));
                        }
                    }
                    Err(e) => {
                        had_error = true;
                        yield Ok(Event::default().event("error").data(e.to_string()));
                    }
                }
            }

            if !found_any && !had_error {
                yield Ok(Event::default().event("error").data(
                    "gallery-dl returned nothing. Is the URL supported / does the extractor need login cookies?"
                ));
            }

            yield Ok(Event::default().event("done").data(json!({ "count": 0 }).to_string()));
        })
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ─── POST /api/preview/search ─────────────────────────────────────────────────
// Filters a pre-scanned item list client-side by type/keyword. No gallery-dl
// invocation here — the client sends us back the items from /api/preview/scan.
// Unchanged by the SSE conversion above: this endpoint never talked to
// gallery-dl in the first place.

#[derive(Deserialize)]
pub struct SearchBody {
    pub items:   Vec<Value>,
    pub query:   Option<String>,
    #[serde(rename = "type")]
    pub kind:    Option<String>,
    pub creator: Option<String>,
}

pub async fn search(
    Json(body): Json<SearchBody>,
) -> Json<Value> {
    let q_lc = body.query.as_deref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let kind_filter  = body.kind.as_deref().filter(|s| !s.is_empty() && *s != "all");
    let creator_filter = body.creator.as_deref()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());

    let filtered: Vec<&Value> = body.items.iter().filter(|item| {
        if let Some(k) = kind_filter {
            if item.get("type").and_then(|v| v.as_str()) != Some(k) {
                return false;
            }
        }
        if let Some(ref c) = creator_filter {
            let creator = item.get("creator").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            if !creator.contains(c.as_str()) { return false; }
        }
        if let Some(ref q) = q_lc {
            let title   = item.get("title").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            let url     = item.get("url").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            let creator = item.get("creator").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            if !title.contains(q.as_str()) && !url.contains(q.as_str()) && !creator.contains(q.as_str()) {
                return false;
            }
        }
        true
    }).collect();

    let count = filtered.len();
    Json(json!({
        "items": filtered,
        "count": count,
    }))
}
