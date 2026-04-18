//! Handler GET /v1/models
//!
//! Retourne la liste des aliases configurés au format OpenAI list.
//! Source : `config.aliases` — pas de probing backend (source=config, ADR-020).
//!
//! Réponse :
//! ```json
//! {
//!   "object": "list",
//!   "data": [
//!     { "id": "qwen3.5-122b", "object": "model", "created": 0, "owned_by": "llmcore" }
//!   ]
//! }
//! ```
//!
//! `owned_by` = provider_name de l'alias (pas le model réel — c'est l'entité qui "possède"
//! le modèle du point de vue du gateway).

use axum::{extract::State, Json};
use serde::Serialize;
use tracing::instrument;

use crate::AppState;

/// Un modèle dans la liste OpenAI-compat.
#[derive(Serialize)]
pub struct ModelInfo {
    /// Identifiant public du modèle (= clé d'alias dans la config).
    pub id: String,
    /// Toujours "model" (spec OpenAI).
    pub object: &'static str,
    /// Timestamp de création — 0 car la source est la config (pas de metadata provider).
    pub created: u64,
    /// Provider qui sert ce modèle (= `alias.provider`).
    pub owned_by: String,
}

/// Réponse format OpenAI list.
#[derive(Serialize)]
pub struct ModelsResponse {
    pub object: &'static str,
    pub data: Vec<ModelInfo>,
}

/// Handler GET /v1/models
///
/// Construit la liste depuis `config.aliases` — aucun appel réseau.
/// Retourne toujours 200 si le service tourne (même liste vide si config sans aliases).
#[instrument(skip(state))]
pub async fn handler(State(state): State<AppState>) -> Json<ModelsResponse> {
    let mut data: Vec<ModelInfo> = state
        .config
        .aliases
        .iter()
        .map(|(alias_id, target)| ModelInfo {
            id: alias_id.clone(),
            object: "model",
            created: 0,
            owned_by: target.provider.clone(),
        })
        .collect();

    // Tri alphabétique pour une réponse déterministe.
    data.sort_by(|a, b| a.id.cmp(&b.id));

    Json(ModelsResponse {
        object: "list",
        data,
    })
}
