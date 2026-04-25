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
//! Alpha.4 : fallback provider par alias — si le primary échoue (erreur backend ou CB ouvert),
//!           le handler tente le `fallback_provider` configuré dans l'alias.
//!           Le fallback est transparent : le circuit breaker du primary n'est pas impacté
//!           par les erreurs récupérées via fallback. Seul le fallback lui-même alimente
//!           son propre circuit breaker.
//!           Le streaming utilise le même mécanisme de fallback pre-connect.
//!
//! Modes :
//! - `stream: true`  → réponse SSE passthrough (Content-Type: text/event-stream)
//! - `stream: false` → réponse JSON (Content-Type: application/json)
//!
//! Codes d'erreur :
//! - 400 : model inconnu (pas dans aliases)
//! - 413 : dépassement cap tokens (input + max_tokens > server.max_total_tokens)
//! - 429 : rate limit dépassé
//! - 500 : provider absent de config (incohérence — normalement catchée à la validation)
//! - 502 : erreur backend (timeout, réseau, upstream 5xx) — tous fallbacks épuisés
//! - 503 : circuit breaker ouvert — tous fallbacks épuisés
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

use crate::{
    error::ApiError, rate_limit::extract_client_ip, registry::RequestLogEntry,
    token_counter::estimate_total_tokens, AppState,
};

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

    // Cap hard tokens (C4 — council homelab-gouvernance MAJOR 2026-04-25).
    // Vérification AVANT la résolution d'alias et tout appel réseau.
    // `max_total_tokens == 0` désactive le cap (non recommandé en prod Qwen3.6).
    let cap = state.config.server.max_total_tokens;
    if cap > 0 {
        let total = estimate_total_tokens(&body);
        if total > cap {
            tracing::warn!(
                consumer = %client_ip,
                total_tokens = total,
                cap = cap,
                model = %body.model,
                "cap tokens dépassé — requête rejetée HTTP 413"
            );
            return Err(ApiError::ContextLengthExceeded { total, cap });
        }
    }

    // Résolution de l'alias : cherche d'abord le model exact, puis "default".
    let alias = state
        .config
        .aliases
        .get(&body.model)
        .or_else(|| state.config.aliases.get("default"))
        .ok_or_else(|| ApiError::UnknownModel(body.model.clone()))?
        .clone();

    // Adapter la requête : remplacer le model par le model réel du provider.
    let model_alias = body.model.clone();
    let is_stream = body.stream == Some(true);
    let start = Instant::now();

    // Dispatch avec fallback : tente le primary, puis le fallback si configuré.
    // Le fallback est tenté dans deux cas :
    //   1. Circuit breaker du primary est ouvert
    //   2. Appel au primary retourne ApiError::Backend (réseau, timeout, 5xx)
    // Les erreurs non-backend (alias inconnu, provider absent) ne déclenchent pas le fallback.
    let (result, effective_provider) =
        dispatch_with_fallback(&state, body, &alias, is_stream).await;

    let latency = start.elapsed();

    // Enregistrement circuit breaker selon le résultat de l'appel — sur le provider effectif.
    match &result {
        Ok(_) => state
            .providers
            .circuit_breakers
            .record_success(&effective_provider),
        Err(ApiError::Backend(llm_err)) => {
            state
                .providers
                .circuit_breakers
                .record_failure(&effective_provider, llm_err);
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
        &effective_provider,
        status_code,
        Some(latency),
    );

    // Journal asynchrone SQLite — fire and forget.
    let registry = state.registry.clone();
    let alias_name = model_alias.clone();
    let real_model = alias.model.clone();
    let error_msg = result.as_ref().err().map(|e| e.to_string());

    tokio::spawn(async move {
        let entry = RequestLogEntry {
            model_alias: alias_name,
            provider_real: effective_provider,
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

/// Dispatch une requête chat vers le primary provider, avec fallback automatique.
///
/// Retourne `(résultat, nom_provider_effectif)`.
/// Le provider effectif est le primary si la requête y aboutit (succès ou erreur non-backend),
/// ou le fallback si le primary a échoué et qu'un fallback est configuré.
///
/// Logique de fallback :
/// - Circuit breaker primary ouvert → tenter fallback directement (sans appel réseau primary)
/// - Appel primary échoue avec ApiError::Backend → tenter fallback
/// - Appel primary échoue avec autre erreur (400, 500 config) → pas de fallback
async fn dispatch_with_fallback(
    state: &crate::AppState,
    body: llm_commons::openai::chat::ChatCompletionRequest,
    alias: &crate::config::AliasTarget,
    is_stream: bool,
) -> (Result<Response, ApiError>, String) {
    // Tentative sur le primary provider.
    let primary_result = try_provider(
        state,
        body.clone(),
        &alias.provider,
        &alias.model,
        is_stream,
    )
    .await;

    match primary_result {
        // Succès primary — retourner directement.
        Ok(resp) => (Ok(resp), alias.provider.clone()),

        // Erreur non-backend (config) — pas de fallback, retourner l'erreur.
        Err(ref e) if !is_backend_error(e) => (primary_result, alias.provider.clone()),

        // Erreur backend — tenter le fallback si configuré.
        Err(primary_err) => {
            // Alimenter le circuit breaker du primary même quand le fallback récupère.
            // Raison : si le primary est down, son CB doit s'ouvrir pour éviter des
            // tentatives réseau coûteuses sur les requêtes suivantes.
            if let ApiError::Backend(ref llm_err) = primary_err {
                state
                    .providers
                    .circuit_breakers
                    .record_failure(&alias.provider, llm_err);
            }

            let Some(fb_provider) = &alias.fallback_provider else {
                // Pas de fallback configuré — retourner l'erreur primary.
                return (Err(primary_err), alias.provider.clone());
            };

            tracing::warn!(
                primary = %alias.provider,
                fallback = %fb_provider,
                error = %primary_err,
                "primary provider échoué — tentative fallback"
            );

            // Model fallback : utiliser fallback_model si défini, sinon le même model.
            let fb_model = alias.fallback_model.as_deref().unwrap_or(&alias.model);
            let fb_result = try_provider(state, body, fb_provider, fb_model, is_stream).await;

            match fb_result {
                Ok(resp) => {
                    tracing::info!(
                        fallback = %fb_provider,
                        "fallback provider OK"
                    );
                    // Provider effectif = fallback (succès). Le CB du fallback sera alimenté
                    // avec record_success dans le handler principal.
                    (Ok(resp), fb_provider.clone())
                }
                Err(fb_err) => {
                    tracing::warn!(
                        fallback = %fb_provider,
                        error = %fb_err,
                        "fallback provider également échoué"
                    );
                    // Retourner l'erreur du fallback (plus récente) avec le nom du fallback.
                    // Le CB du fallback sera alimenté avec record_failure dans le handler principal.
                    (Err(fb_err), fb_provider.clone())
                }
            }
        }
    }
}

/// Retourne `true` si l'erreur est une erreur backend (réseau, timeout, upstream 5xx)
/// qui justifie de tenter un fallback.
/// Les erreurs de configuration (alias inconnu, provider absent) ne déclenchent pas le fallback.
fn is_backend_error(e: &ApiError) -> bool {
    matches!(e, ApiError::Backend(_))
}

/// Tente un appel vers un provider spécifique (primary ou fallback).
///
/// Vérifie le circuit breaker, résout le provider depuis le pool, et dispatch
/// selon le mode streaming ou non-streaming.
/// Retourne une `ApiError::Backend` si le circuit breaker est ouvert
/// (traité comme erreur backend pour déclencher le fallback).
async fn try_provider(
    state: &crate::AppState,
    mut body: llm_commons::openai::chat::ChatCompletionRequest,
    provider_name: &str,
    model: &str,
    is_stream: bool,
) -> Result<Response, ApiError> {
    // Circuit breaker : traiter l'ouverture comme une erreur backend (déclencheur fallback).
    if !state.providers.circuit_breakers.should_allow(provider_name) {
        tracing::warn!(
            provider = %provider_name,
            "circuit breaker ouvert — traité comme erreur backend pour fallback"
        );
        return Err(ApiError::Backend(
            llm_commons::error::LlmError::ProviderUnavailable {
                provider: provider_name.to_string(),
                reason: "circuit breaker ouvert".to_string(),
            },
        ));
    }

    // Résolution du provider depuis le pool partagé.
    let provider = state
        .providers
        .get(provider_name)
        .ok_or_else(|| ApiError::ProviderNotFound(provider_name.to_string()))?;

    // Injecter le model réel du provider.
    body.model = model.to_string();

    if is_stream {
        let chunk_stream = provider.stream(body).await?;
        let circuit_breakers = state.providers.circuit_breakers.clone();
        let provider_id = provider_name.to_string();
        let sse_body = sse_stream_from_chunks(chunk_stream, circuit_breakers, provider_id);

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
        let completion = provider.complete(body).await?;
        Ok((StatusCode::OK, Json(completion)).into_response())
    }
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
