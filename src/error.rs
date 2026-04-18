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
    /// Modèle inconnu — pas dans la table d'aliases.
    #[error("unknown model: {0}")]
    UnknownModel(String),

    /// Provider configuré pour l'alias mais absent de la section `[providers]`.
    #[error("provider '{0}' not found in config")]
    ProviderNotFound(String),

    /// Erreur LLM en provenance du backend (réseau, timeout, upstream error, etc.).
    #[error("backend error: {0}")]
    Backend(#[from] LlmError),

    /// Erreur de désérialisation du body de requête.
    #[error("invalid request body: {0}")]
    InvalidBody(String),
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
            ApiError::UnknownModel(_) => 400,
            ApiError::ProviderNotFound(_) => 500,
            ApiError::InvalidBody(_) => 400,
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
            ApiError::UnknownModel(m) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "model_not_found",
                format!("unknown model: {}", m),
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
