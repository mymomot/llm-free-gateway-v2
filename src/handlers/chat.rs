//! Handler POST /v1/chat/completions
//!
//! Dispatch basé sur la table d'aliases de config :
//! model demandé → lookup aliases → provider_name + real_model → forward HTTP.
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

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use llm_commons::{
    capabilities::{Capabilities, ThinkingMode, ToolUseSupport},
    openai::chat::ChatCompletionRequest,
    provider::LlmProvider,
};
use tracing::instrument;

use crate::{error::ApiError, providers::openai_compat::OpenAiCompatProvider, AppState};

/// Handler POST /v1/chat/completions
///
/// Lit le body JSON, résout l'alias model, construit le provider à la demande
/// (sans état partagé entre requêtes pour alpha.1 — pool de providers en alpha.2),
/// et dispatch vers complete() ou stream() selon le flag `stream`.
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

    // Récupération du provider config.
    let provider_cfg = state
        .config
        .providers
        .get(&alias.provider)
        .ok_or_else(|| ApiError::ProviderNotFound(alias.provider.clone()))?
        .clone();

    // Construction du provider (stateless pour alpha.1).
    // La capabilities est générique maximale — le backend n'est pas interrogé
    // à ce stade pour ses capabilities réelles (probing en alpha.2).
    let caps = Capabilities {
        tool_use: ToolUseSupport::Native,
        streaming: true,
        vision: true,
        thinking: ThinkingMode::Switchable,
        context_max: 131_072,
        structured_output: false,
        prompt_caching: true,
        reasoning_levels: None,
    };

    let provider = OpenAiCompatProvider::new(
        &alias.provider,
        &provider_cfg.endpoint,
        provider_cfg.timeout_secs,
        provider_cfg.api_key_env.as_deref(),
        caps,
    )
    .map_err(|e| {
        ApiError::Backend(llm_commons::error::LlmError::Custom {
            message: e.to_string(),
        })
    })?;

    // Adapter la requête : remplacer le model par le model réel du provider.
    let mut request = body;
    request.model = alias.model.clone();

    let is_stream = request.stream == Some(true);

    if is_stream {
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
    }
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
