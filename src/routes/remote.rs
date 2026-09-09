use std::sync::Arc;

use axum::{extract::State, Json};

use crate::AppState;

/// Runtime network diagnostics for Settings → Remote Access.  This is read
/// only; it does not create a second listener or expose a new control plane.
pub async fn status(State(state): State<Arc<AppState>>) -> Json<crate::remote::RemoteAccessInfo> {
    Json(crate::remote::remote_access_info(&state).await)
}
