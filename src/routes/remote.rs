use std::sync::Arc;

use axum::{extract::State, Json};

use crate::AppState;

/// Read-only listener diagnostics for Settings. This does not open another
/// server or expose a control surface.
pub async fn status(State(state): State<Arc<AppState>>) -> Json<crate::remote::RemoteAccessInfo> {
    Json(crate::remote::remote_access_info(&state).await)
}
