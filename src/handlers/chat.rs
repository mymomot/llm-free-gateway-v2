//! Handler POST /v1/chat/completions
//!
//! Dispatch basé sur la table d'aliases de config :
//! model demandé → lookup aliases → provider_name + real_model → forward HTTP.
//!
//! Alpha.2 : le provider est résolu depuis le pool partagé (`AppState.providers`)
//! sans recréation par requête. Le `reqwest::Client` est aussi partagé.
//!
//! Modes :
//! - `stream: true`  → réponse SSE passthrough (Content-Type: text/event-stream)
//! - `stream: false` → réponse JSON (Content-Type: application/json)
//!
//! Codes d'erreur :
//! - 400 : model inconnu (pas dans aliases)
//! - 500 : provider absent de config (incohérence — normalement catchée à la validation)
//! - 502 : erreur backend (timeout, réseau, upstream 5xx)
//! - 4xx passthrough : erreurs client renvoyées depuis le backend

use std::time::Instant;

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use llm_commons::openai::chat::ChatCompletionRequest;
use tracing::instrument;

use crate::{error::ApiError, registry::RequestLogEntry, AppState};

/// Handler POST /v1/chat/completions
///
/// Résout l'alias model depuis le pool de providers partagé (construit au startup),
/// et dispatch vers `complete()` ou `stream()` selon le flag `stream`.
#[instrument(skip(state, body), fields(model))]
pub async fn handler(
    State(state): State<AppState>,
    Json(body): Json<ChatCompletionRequest>,
) -> Result<Response, ApiError> {
    tracing::Span::current().record("model", body.model.as_str());

    // Résolution de l'alias : cherche d'abord le model exact, puis "default".
    let alias = state
        .config
        .aliases
        .get(&body.model)
        .or_else(|| state.config.aliases.get("default"))
        .ok_or_else(|| ApiError::UnknownModel(body.model.clone()))?
        .clone();

    // Résolution du provider depuis le pool partagé (pas de création par requête).
    let provider = state
        .providers
        .get(&alias.provider)
        .ok_or_else(|| ApiError::ProviderNotFound(alias.provider.clone()))?;

    // Adapter la requête : remplacer le model par le model réel du provider.
    let model_alias = body.model.clone();
    let mut request = body;
    request.model = alias.model.clone();

    let is_stream = request.stream == Some(true);
    let start = Instant::now();

    let result: Result<Response, ApiError> = if is_stream {
        // Mode streaming : forward SSE passthrough.
        let chunk_stream = provider.stream(request).await?;

        // Convertit le stream de ChatCompletionChunk en stream de bytes SSE.
        let sse_body = sse_stream_from_chunks(chunk_stream);

        let response = Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )
            .header(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))
            .header(header::CONNECTION, HeaderValue::from_static("keep-alive"))
            .body(Body::from_stream(sse_body))
            .map_err(|e| {
                ApiError::Backend(llm_commons::error::LlmError::Custom {
                    message: format!("erreur construction réponse SSE: {}", e),
                })
            })?;

        Ok(response)
    } else {
        // Mode non-streaming : réponse JSON complète.
        let completion = provider.complete(request).await?;
        Ok((StatusCode::OK, Json(completion)).into_response())
    };

    let latency = start.elapsed();

    // Journalisation et métriques — non-bloquants, fire and forget.
    let status_code = match &result {
        Ok(resp) => resp.status().as_u16(),
        Err(e) => e.status_code(),
    };

    // Métriques synchrones (atomiques, pas de I/O).
    state.metrics.record_request(
        "/v1/chat/completions",
        &model_alias,
        &alias.provider,
        status_code,
        Some(latency),
    );

    // Journal asynchrone SQLite — fire and forget.
    let registry = state.registry.clone();
    let alias_name = model_alias.clone();
    let provider_name = alias.provider.clone();
    let real_model = alias.model.clone();
    let error_msg = result.as_ref().err().map(|e| e.to_string());

    tokio::spawn(async move {
        let entry = RequestLogEntry {
            model_alias: alias_name,
            provider_real: provider_name,
            real_model,
            route: "/v1/chat/completions".to_owned(),
            latency_ms: Some(latency.as_millis() as u64),
            status_code,
            streamed: is_stream,
            error_message: error_msg,
        };
        if let Err(e) = registry.log_request(entry).await {
            tracing::warn!("erreur journalisation requête chat: {}", e);
        }
    });

    result
}

/// Convertit un stream de `LlmResult<ChatCompletionChunk>` en stream de bytes SSE.
///
/// Format wire : `data: <json>\n\n` par chunk, `data: [DONE]\n\n` pour terminer.
/// Les erreurs de sérialisation des chunks sont loggées et sautées (le flux continue).
fn sse_stream_from_chunks(
    chunks: llm_commons::provider::ChatCompletionStream,
) -> impl futures::Stream<Item = Result<bytes::Bytes, std::convert::Infallible>> {
    use futures::StreamExt;

    chunks.map(|result| {
        let line = match result {
            Ok(chunk) => match serde_json::to_string(&chunk) {
                Ok(json) => format!("data: {}\n\n", json),
                Err(e) => {
                    tracing::warn!("erreur sérialisation chunk SSE: {}", e);
                    return Ok(bytes::Bytes::new());
                }
            },
            Err(e) => {
                // Erreur upstream durant le streaming — termine le flux proprement.
                tracing::error!("erreur stream backend: {}", e);
                "data: [DONE]\n\n".to_string()
            }
        };
        Ok(bytes::Bytes::from(line))
    })
}
