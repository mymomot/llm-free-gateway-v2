//! Handler POST /v1/embeddings
//!
//! Forward une requête d'embedding vers le provider résolu via alias.
//! Le routing suit exactement le même pattern que `/v1/chat/completions` :
//! alias → provider_name → endpoint.
//!
//! La requête est forwardée telle quelle vers `{provider.endpoint}/v1/embeddings`.
//! La réponse du backend est retournée telle quelle (pass-through JSON).
//!
//! Codes d'erreur :
//! - 400 : alias inconnu (modèle non configuré)
//! - 500 : provider absent de config (incohérence — normalement catchée à la validation)
//! - 502 : erreur backend (timeout, réseau, upstream 5xx)
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

use crate::{error::ApiError, registry::RequestLogEntry, AppState};

/// Handler POST /v1/embeddings
///
/// Résout l'alias via `model` dans le body, forward vers le provider, retourne
/// la réponse JSON du backend sans transformation.
///
/// `model` est obligatoire dans la requête — l'alias est la seule clé de dispatch.
#[instrument(skip(state, body), fields(model))]
pub async fn handler(
    State(state): State<AppState>,
    Json(body): Json<EmbeddingRequest>,
) -> Result<Response, ApiError> {
    // Extraire le model alias avant de consommer body.
    let model_alias_owned: String = body.model.clone().unwrap_or_default();
    let model = model_alias_owned.as_str();
    tracing::Span::current().record("model", model);

    // Résolution de l'alias — pas de fallback "default" pour les embeddings :
    // un embedding vers le mauvais modèle est silencieusement incorrect.
    let alias = state
        .config
        .aliases
        .get(model)
        .ok_or_else(|| ApiError::UnknownModel(model_alias_owned.clone()))?
        .clone();

    // Récupération du provider config.
    let provider_cfg = state
        .config
        .providers
        .get(&alias.provider)
        .ok_or_else(|| ApiError::ProviderNotFound(alias.provider.clone()))?
        .clone();

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

    // Résolution clé API si configurée.
    let mut req = client.post(&embed_url).json(&forward_body);
    if let Some(env_name) = &provider_cfg.api_key_env {
        if let Ok(key) = std::env::var(env_name) {
            req = req.bearer_auth(key);
        }
    }

    let response = req.send().await.map_err(|e| {
        if e.is_timeout() {
            ApiError::Backend(llm_commons::error::LlmError::Timeout {
                elapsed_secs: provider_cfg.timeout_secs as f64,
            })
        } else {
            ApiError::Backend(llm_commons::error::LlmError::Network {
                source: Box::new(e),
            })
        }
    })?;

    let latency = start.elapsed();
    let status = response.status();
    let status_code = status.as_u16();

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

/// Timeout pour les requêtes d'embedding (valeur si non spécifiée dans config).
///
/// Les embeddings sont plus rapides que les completions — 60s est généreux.
pub const DEFAULT_EMBED_TIMEOUT_SECS: u64 = 60;

impl AppState {
    /// Expose les dimensions pour le handler embeddings.
    pub fn embed_timeout(&self) -> Duration {
        Duration::from_secs(DEFAULT_EMBED_TIMEOUT_SECS)
    }
}
