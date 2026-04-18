//! Handler GET /health
//!
//! Retourne le statut du gateway et la liste des providers configurés.
//! Alpha.1 : pas de probing backend — répond toujours 200 si le service tourne.
//!
//! Réponse :
//! ```json
//! { "status": "ok", "version": "0.1.0-alpha.1", "providers": ["llmcore"] }
//! ```

use axum::{extract::State, Json};
use serde::Serialize;
use tracing::instrument;

use crate::AppState;

/// Réponse du health endpoint.
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub providers: Vec<String>,
}

/// Handler GET /health
///
/// Retourne 200 + JSON avec statut "ok" et liste des providers configurés.
#[instrument(skip(state))]
pub async fn handler(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        providers: state.config.provider_names(),
    })
}
