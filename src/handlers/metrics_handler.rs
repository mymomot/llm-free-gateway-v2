//! Handler GET /metrics
//!
//! Expose les métriques Prometheus en text format 0.0.4.
//! Content-Type : `text/plain; version=0.0.4; charset=utf-8`
//!
//! Métriques disponibles :
//! - `gateway_requests_total` — counter par (route, model_alias, provider, status_code)
//! - `gateway_request_duration_seconds` — summary (sum + count) par (route, model_alias)
//! - `gateway_providers_configured` — gauge
//! - `gateway_uptime_seconds` — gauge

use axum::{
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use tracing::instrument;

use crate::AppState;

/// Handler GET /metrics
///
/// Rend l'export Prometheus complet. Retourne toujours 200.
#[instrument(skip(state))]
pub async fn handler(State(state): State<AppState>) -> Response {
    let body = state.metrics.render();

    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )
        .body(axum::body::Body::from(body))
        // SAFETY : les headers sont des valeurs statiques — l'erreur est impossible.
        .expect("construction réponse /metrics impossible avec headers statiques")
        .into_response()
}
