//! Handler POST /v1/chat/completions
//!
//! Dispatch basé sur la table d'aliases de config :
//! model demandé → lookup aliases → provider_name + real_model → forward HTTP.
//!
//! Alpha.2 : le provider est résolu depuis le pool partagé (`AppState.providers`)
//! sans recréation par requête. Le `reqwest::Client` est aussi partagé.
//!
//! Alpha.3 : circuit breaker par provider — 503 si circuit ouvert.
//!           Rate limit par IP — 429 si quota dépassé.
//!
//! Modes :
//! - `stream: true`  → réponse SSE passthrough (Content-Type: text/event-stream)
//! - `stream: false` → réponse JSON (Content-Type: application/json)
//!
//! Codes d'erreur :
//! - 400 : model inconnu (pas dans aliases)
//! - 429 : rate limit dépassé
//! - 500 : provider absent de config (incohérence — normalement catchée à la validation)
//! - 502 : erreur backend (timeout, réseau, upstream 5xx)
//! - 503 : circuit breaker ouvert
//! - 4xx passthrough : erreurs client renvoyées depuis le backend

use std::{sync::Arc, time::Instant};

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use llm_commons::{circuit_breaker::CircuitBreakerRegistry, openai::chat::ChatCompletionRequest};
use tracing::instrument;

use crate::{error::ApiError, rate_limit::extract_client_ip, registry::RequestLogEntry, AppState};

/// Handler POST /v1/chat/completions
///
/// Résout l'alias model depuis le pool de providers partagé (construit au startup),
/// et dispatch vers `complete()` ou `stream()` selon le flag `stream`.
///
/// Alpha.3 : circuit breaker par provider + rate limit par IP.
#[instrument(skip(state, headers, body), fields(model))]
pub async fn handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ChatCompletionRequest>,
) -> Result<Response, ApiError> {
    // Rate limit par IP — extrait depuis X-Forwarded-For / X-Real-IP ou fallback localhost.
    let client_ip = extract_client_ip(&headers);

    if !state.rate_limiter.check_and_increment(client_ip) {
        return Ok(Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("Retry-After", "60")
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .body(Body::from(
                r#"{"error":{"message":"rate limit exceeded","type":"rate_limit_error","code":"too_many_requests"}}"#,
            ))
            .unwrap_or_else(|_| StatusCode::TOO_MANY_REQUESTS.into_response()));
    }

    tracing::Span::current().record("model", body.model.as_str());

    // Résolution de l'alias : cherche d'abord le model exact, puis "default".
    let alias = state
        .config
        .aliases
        .get(&body.model)
        .or_else(|| state.config.aliases.get("default"))
        .ok_or_else(|| ApiError::UnknownModel(body.model.clone()))?
        .clone();

    // Circuit breaker : 503 si circuit ouvert pour ce provider.
    if !state
        .providers
        .circuit_breakers
        .should_allow(&alias.provider)
    {
        tracing::warn!(
            provider = %alias.provider,
            "circuit breaker ouvert — requête chat rejetée"
        );
        return Ok(Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .body(Body::from(format!(
                r#"{{"error":{{"message":"provider '{}' temporairement indisponible (circuit ouvert)","type":"server_error","code":"provider_unavailable"}}}}"#,
                alias.provider
            )))
            .unwrap_or_else(|_| StatusCode::SERVICE_UNAVAILABLE.into_response()));
    }

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
        // CB-1 : le circuit breaker est alimenté par les erreurs mid-stream via record_failure.
        let circuit_breakers = state.providers.circuit_breakers.clone();
        let provider_id_for_stream = alias.provider.clone();
        let sse_body = sse_stream_from_chunks(chunk_stream, circuit_breakers, provider_id_for_stream);

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

    // Enregistrement circuit breaker selon le résultat de l'appel.
    match &result {
        Ok(_) => state
            .providers
            .circuit_breakers
            .record_success(&alias.provider),
        Err(ApiError::Backend(llm_err)) => {
            state
                .providers
                .circuit_breakers
                .record_failure(&alias.provider, llm_err);
        }
        Err(_) => {
            // Erreur non-backend (alias inconnu, provider absent) — pas d'impact circuit breaker.
        }
    }

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
///
/// CB-1 : chaque `Err` issu du stream upstream alimente le circuit breaker via
/// `circuit_breakers.record_failure(provider_id, &err)`. Le streaming n'est pas
/// interrompu — le client reçoit `[DONE]` et le circuit breaker est mis à jour.
fn sse_stream_from_chunks(
    chunks: llm_commons::provider::ChatCompletionStream,
    circuit_breakers: Arc<CircuitBreakerRegistry>,
    provider_id: String,
) -> impl futures::Stream<Item = Result<bytes::Bytes, std::convert::Infallible>> {
    use futures::StreamExt;

    chunks.map(move |result| {
        let line = match result {
            Ok(chunk) => match serde_json::to_string(&chunk) {
                Ok(json) => format!("data: {}\n\n", json),
                Err(e) => {
                    tracing::warn!("erreur sérialisation chunk SSE: {}", e);
                    return Ok(bytes::Bytes::new());
                }
            },
            Err(e) => {
                // Erreur upstream durant le streaming — alimente le circuit breaker (CB-1)
                // puis termine le flux proprement.
                tracing::error!("erreur stream backend: {}", e);
                circuit_breakers.record_failure(&provider_id, &e);
                "data: [DONE]\n\n".to_string()
            }
        };
        Ok(bytes::Bytes::from(line))
    })
}
