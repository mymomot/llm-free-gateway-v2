//! Erreurs API du gateway v2.
//!
//! `ApiError` est le type de retour unifié pour tous les handlers Axum.
//! Il implémente `IntoResponse` pour produire des réponses HTTP avec body JSON
//! au format OpenAI-compat :
//!
//! ```json
//! { "error": { "message": "...", "type": "...", "code": "..." } }
//! ```
//!
//! Toutes les erreurs internes (`LlmError`, config, réseau) sont converties
//! ici en code HTTP approprié avec message structuré.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use llm_commons::error::LlmError;
use thiserror::Error;

/// Erreur retournée par les handlers du gateway.
#[derive(Debug, Error)]
pub enum ApiError {
    /// Alias inconnu — pas dans la table d'aliases du config TOML.
    ///
    /// HTTP 404 — rejet strict : aucun forward silencieux vers un alias par défaut.
    /// La liste des aliases disponibles est incluse dans le message pour faciliter
    /// le diagnostic côté consumer.
    #[error("model alias '{alias}' not found. Available: {}", available.join(", "))]
    AliasNotFound {
        /// Alias demandé par le consumer.
        alias: String,
        /// Liste triée des aliases configurés dans `[aliases]`.
        available: Vec<String>,
    },

    /// Provider configuré pour l'alias mais absent de la section `[providers]`.
    #[error("provider '{0}' not found in config")]
    ProviderNotFound(String),

    /// Erreur LLM en provenance du backend (réseau, timeout, upstream error, etc.).
    #[error("backend error: {0}")]
    Backend(#[from] LlmError),

    /// Erreur de désérialisation du body de requête.
    #[error("invalid request body: {0}")]
    InvalidBody(String),

    /// Dépassement du cap total de tokens (input + max_tokens > seuil configuré).
    ///
    /// HTTP 413 — imposé par le council homelab-gouvernance MAJOR 2026-04-25 (C4).
    /// Raison : Qwen3.6 slot ctx réel = 262K mais bug freeze llama-server
    /// sur cache-bf16 + corpus >200K → cap hard à 180K.
    #[error("context length exceeded: {total} tokens > cap {cap}")]
    ContextLengthExceeded {
        /// Total estimé (input + max_tokens).
        total: u64,
        /// Cap configuré.
        cap: u64,
    },
}

/// Corps d'erreur au format OpenAI-compat.
#[derive(serde::Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(serde::Serialize)]
struct ErrorDetail {
    message: String,
    #[serde(rename = "type")]
    error_type: &'static str,
    code: &'static str,
}

impl ApiError {
    /// Retourne le code HTTP correspondant à l'erreur.
    ///
    /// Utilisé par les handlers pour journaliser le status_code avant de consommer
    /// l'erreur via `into_response()`.
    pub fn status_code(&self) -> u16 {
        match self {
            ApiError::AliasNotFound { .. } => 404,
            ApiError::ProviderNotFound(_) => 500,
            ApiError::InvalidBody(_) => 400,
            ApiError::ContextLengthExceeded { .. } => 413,
            ApiError::Backend(llm_err) => match llm_err {
                LlmError::InvalidRequest { .. } => 400,
                LlmError::Unauthorized { .. } => 401,
                LlmError::Forbidden { .. } => 403,
                LlmError::NotFound { .. } => 404,
                LlmError::RateLimited { .. } => 429,
                LlmError::Timeout { .. }
                | LlmError::Network { .. }
                | LlmError::ProviderUnavailable { .. }
                | LlmError::UpstreamError { .. } => 502,
                _ => 500,
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error_type, code, message) = match &self {
            ApiError::AliasNotFound { alias, available } => (
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "model_not_found",
                format!(
                    "Model alias '{}' is not configured. Available aliases: {}",
                    alias,
                    available.join(", ")
                ),
            ),
            ApiError::ProviderNotFound(p) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "provider_not_configured",
                format!("provider '{}' not found in config", p),
            ),
            ApiError::Backend(llm_err) => {
                let status = match llm_err {
                    LlmError::InvalidRequest { .. } => StatusCode::BAD_REQUEST,
                    LlmError::Unauthorized { .. } => StatusCode::UNAUTHORIZED,
                    LlmError::Forbidden { .. } => StatusCode::FORBIDDEN,
                    LlmError::NotFound { .. } => StatusCode::NOT_FOUND,
                    LlmError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
                    LlmError::Timeout { .. }
                    | LlmError::Network { .. }
                    | LlmError::ProviderUnavailable { .. }
                    | LlmError::UpstreamError { .. } => StatusCode::BAD_GATEWAY,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                (status, "server_error", "backend_error", llm_err.to_string())
            }
            ApiError::InvalidBody(msg) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_body",
                msg.clone(),
            ),
            ApiError::ContextLengthExceeded { total, cap } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "context_length_exceeded",
                format!(
                    "Input + max_tokens exceeds {} token cap \
                     (Qwen3.6 slot 262K, hard cap to avoid freeze on cache-bf16 + corpus >200K). \
                     Estimated total: {} tokens.",
                    cap, total
                ),
            ),
        };

        let body = ErrorBody {
            error: ErrorDetail {
                message,
                error_type,
                code,
            },
        };

        (status, Json(body)).into_response()
    }
}
