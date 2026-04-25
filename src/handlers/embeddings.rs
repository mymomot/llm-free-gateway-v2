//! Handler POST /v1/embeddings
//!
//! Forward une requête d'embedding vers le provider résolu via alias.
//! Le routing suit exactement le même pattern que `/v1/chat/completions` :
//! alias → provider_name → endpoint.
//!
//! La requête est forwardée telle quelle vers `{provider.endpoint}/v1/embeddings`.
//! La réponse du backend est retournée telle quelle (pass-through JSON).
//!
//! Alpha.3 :
//! - Clé API lue depuis `resolved_api_keys` (pré-résolue au startup, FINDING-M4)
//! - Circuit breaker : 503 si circuit ouvert, record_failure/success autour de l'appel
//! - Rate limit par IP : 429 si quota dépassé
//!
//! Codes d'erreur :
//! - 400 : alias inconnu (modèle non configuré)
//! - 429 : rate limit dépassé
//! - 500 : provider absent de config (incohérence — normalement catchée à la validation)
//! - 502 : erreur backend (timeout, réseau, upstream 5xx)
//! - 503 : circuit breaker ouvert (provider temporairement indisponible)
//! - 4xx passthrough : erreurs client depuis le backend

use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use llm_commons::openai::embeddings::EmbeddingRequest;
use tracing::instrument;

use crate::{error::ApiError, rate_limit::extract_client_ip, registry::RequestLogEntry, AppState};

/// Handler POST /v1/embeddings
///
/// Résout l'alias via `model` dans le body, forward vers le provider, retourne
/// la réponse JSON du backend sans transformation.
///
/// `model` est obligatoire dans la requête — l'alias est la seule clé de dispatch.
#[instrument(skip(state, headers, body), fields(model))]
pub async fn handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<EmbeddingRequest>,
) -> Result<Response, ApiError> {
    // Rate limit par IP — extrait depuis X-Forwarded-For / X-Real-IP ou fallback localhost.
    let client_ip = extract_client_ip(&headers);

    if !state.rate_limiter.check_and_increment(client_ip) {
        // RL-1 : valeur statique 60s — cohérence avec chat.rs:56.
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

    // Extraire le model alias avant de consommer body.
    let model_alias_owned: String = body.model.clone().unwrap_or_default();
    let model = model_alias_owned.as_str();
    tracing::Span::current().record("model", model);

    // Résolution stricte de l'alias — rejet HTTP 404 si absent du config TOML.
    // Pas de fallback silencieux : un embedding vers le mauvais modèle produit des
    // vecteurs incompatibles. Fix Phase D 2026-04-25 — cohérence avec chat handler.
    let alias = state.config.aliases.get(model).ok_or_else(|| {
        let mut available: Vec<String> = state.config.aliases.keys().cloned().collect();
        available.sort();
        tracing::warn!(
            consumer = %client_ip,
            model = %model,
            available = ?available,
            "alias inconnu (embeddings) — requête rejetée HTTP 404"
        );
        ApiError::AliasNotFound {
            alias: model_alias_owned.clone(),
            available,
        }
    })?
    .clone();

    // Récupération du provider config.
    let provider_cfg = state
        .config
        .providers
        .get(&alias.provider)
        .ok_or_else(|| ApiError::ProviderNotFound(alias.provider.clone()))?
        .clone();

    // Circuit breaker : 503 si circuit ouvert pour ce provider.
    if !state
        .providers
        .circuit_breakers
        .should_allow(&alias.provider)
    {
        tracing::warn!(
            provider = %alias.provider,
            "circuit breaker ouvert — requête embeddings rejetée"
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

    let embed_url = format!(
        "{}/v1/embeddings",
        provider_cfg.endpoint.trim_end_matches('/')
    );

    // Client HTTP — timeout configurable.
    let client = state.providers.http_client();

    // Adapter la requête : injecter le model réel.
    let mut forward_body = body;
    forward_body.model = Some(alias.model.clone());

    let start = Instant::now();

    // Clé API pré-résolue depuis resolved_api_keys (FINDING-M4 — pas d'env::var() par requête).
    let mut req = client.post(&embed_url).json(&forward_body);
    if let Some(Some(key)) = state.providers.resolved_api_keys.get(&alias.provider) {
        req = req.bearer_auth(key);
    }

    // TMO-1 : override du timeout per-provider — le client partagé a un timeout global,
    // mais chaque requête peut avoir son propre timeout depuis provider_cfg.timeout_secs.
    let response = req
        .timeout(Duration::from_secs(provider_cfg.timeout_secs))
        .send()
        .await
        .map_err(|e| {
            // Enregistrer l'échec réseau dans le circuit breaker avant de retourner l'erreur.
            let llm_err = if e.is_timeout() {
                llm_commons::error::LlmError::Timeout {
                    elapsed_secs: provider_cfg.timeout_secs as f64,
                }
            } else {
                llm_commons::error::LlmError::Network {
                    source: Box::new(e),
                }
            };
            state
                .providers
                .circuit_breakers
                .record_failure(&alias.provider, &llm_err);
            ApiError::Backend(llm_err)
        })?;

    let latency = start.elapsed();
    let status = response.status();
    let status_code = status.as_u16();

    // Enregistrement circuit breaker selon le statut HTTP.
    if status.is_server_error() {
        // 5xx backend → failure circuit breaker.
        state.providers.circuit_breakers.record_failure(
            &alias.provider,
            &llm_commons::error::LlmError::UpstreamError {
                status: status_code,
                message: format!("upstream HTTP {}", status_code),
            },
        );
    } else {
        // Succès ou 4xx (erreur client, pas du provider) → success circuit breaker.
        state
            .providers
            .circuit_breakers
            .record_success(&alias.provider);
    }

    // Journalisation asynchrone non-bloquante — fire and forget.
    // Les erreurs de log ne doivent pas impacter la réponse au client.
    let registry = state.registry.clone();
    let alias_str = model_alias_owned.clone();
    let provider_str = alias.provider.clone();
    let real_model_str = alias.model.clone();
    let is_err = !status.is_success();

    tokio::spawn(async move {
        let entry = RequestLogEntry {
            model_alias: alias_str,
            provider_real: provider_str,
            real_model: real_model_str,
            route: "/v1/embeddings".to_owned(),
            latency_ms: Some(latency.as_millis() as u64),
            status_code,
            streamed: false,
            error_message: if is_err {
                Some(format!("upstream HTTP {}", status_code))
            } else {
                None
            },
        };
        if let Err(e) = registry.log_request(entry).await {
            tracing::warn!("erreur journalisation requête embeddings: {}", e);
        }
    });

    // Enregistrement métriques.
    state.metrics.record_request(
        "/v1/embeddings",
        &model_alias_owned,
        &alias.provider,
        status_code,
        Some(latency),
    );

    // Passthrough de la réponse backend : status + body JSON.
    if !status.is_success() {
        let body_bytes = response.bytes().await.unwrap_or_default();
        return Ok(Response::builder()
            .status(status)
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .body(Body::from(body_bytes))
            .map_err(|e| {
                ApiError::Backend(llm_commons::error::LlmError::Custom {
                    message: format!("erreur construction réponse passthrough: {}", e),
                })
            })?
            .into_response());
    }

    let body_bytes = response.bytes().await.map_err(|e| {
        ApiError::Backend(llm_commons::error::LlmError::Network {
            source: Box::new(e),
        })
    })?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        .body(Body::from(body_bytes))
        .map_err(|e| {
            ApiError::Backend(llm_commons::error::LlmError::Custom {
                message: format!("erreur construction réponse embeddings: {}", e),
            })
        })?
        .into_response())
}

// DT-1 : DEFAULT_EMBED_TIMEOUT_SECS et embed_timeout() supprimés — dead code confirmé.
// Le timeout per-provider est appliqué directement via provider_cfg.timeout_secs (TMO-1).
