//! ServeDir handles range requests and missing files; reconcile a raced deletion on 404.
use crate::AppState;
use axum::{
    extract::{OriginalUri, Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use std::sync::Arc;

pub async fn reconcile_not_found(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    request: Request,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    if response.status() == StatusCode::NOT_FOUND {
        if let Some(path) = uri.path().strip_prefix("/library/") {
            if let Ok(decoded) = urlencoding::decode(path) {
                let rel = decoded.into_owned();
                if !std::path::Path::new(&rel)
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
                {
                    let _=tokio::task::spawn_blocking(move || {
                        let path=state.library_dir.join(&rel);
                        if path.metadata().is_err_and(|e|e.kind()==std::io::ErrorKind::NotFound) {
                            if let Ok(conn)=state.pool.get() {
                                let _=conn.execute("UPDATE media SET missing=1,downloaded=0,nsfw_state='missing' WHERE filepath=?1 AND downloaded=1",[rel]);
                            }
                        }
                    }).await;
                }
            }
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;
    #[tokio::test]
    async fn missing_library_file_is_404_and_updates_database() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::test_support::state(dir.path());
        crate::test_support::source(&state);
        state.pool.get().unwrap().execute("INSERT INTO media(source_id,filepath,filename,type,added_at) VALUES(1,'test/missing.jpg','missing.jpg','image','2026')",[]).unwrap();
        let app = axum::Router::new().nest(
            "/library",
            axum::Router::new()
                .fallback_service(tower_http::services::ServeDir::new(&state.library_dir))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    reconcile_not_found,
                )),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/library/test/missing.jpg")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            state
                .pool
                .get()
                .unwrap()
                .query_row("SELECT missing FROM media", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
