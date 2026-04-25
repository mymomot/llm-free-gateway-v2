//! Tests intégration — Rejet strict des aliases inconnus (fix Phase D 2026-04-25).
//!
//! Vérifie que la gateway retourne HTTP 404 + body OpenAI-compat pour tout alias
//! absent du config TOML, et que les aliases valides passent normalement.
//!
//! Contexte : avant le fix, un alias inconnu était silencieusement forwardé vers
//! l'alias "default" (si présent). Cela masquait les consumers sur des aliases legacy
//! supprimés (ex: qwen3.5-122b après la refonte Phase C 2026-04-25).
//!
//! Cas testés :
//! 1. POST chat avec alias inconnu → HTTP 404 + body structuré
//! 2. POST chat avec alias valide → HTTP non-404 (régression)
//! 3. POST embeddings avec alias inconnu → HTTP 404
//! 4. POST embeddings avec alias valide → HTTP non-404 (régression)
//! 5. Body d'erreur 404 : champs error.type, error.code, error.message + liste aliases
//! 6. Alias "default" connu (si configuré) → non rejeté
//! 7. Alias legacy supprimé ("qwen3.5-122b") → HTTP 404 strictement

use axum_test::TestServer;
use llm_free_gateway_v2::{build_router, AppState};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Config de test avec un alias chat ("default") et un alias embed ("bge-m3").
fn test_config_two_aliases() -> llm_free_gateway_v2::config::Config {
    use llm_free_gateway_v2::config::{
        AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig,
    };
    use std::collections::HashMap;

    let mut providers = HashMap::new();
    providers.insert(
        "chat-backend".to_string(),
        ProviderConfig {
            endpoint: "http://127.0.0.1:1".to_string(), // port 1 — toujours refusé
            timeout_secs: 2,
            api_key_env: None,
        },
    );
    providers.insert(
        "embed-backend".to_string(),
        ProviderConfig {
            endpoint: "http://127.0.0.1:1".to_string(),
            timeout_secs: 2,
            api_key_env: None,
        },
    );

    let mut aliases = HashMap::new();
    aliases.insert(
        "default".to_string(),
        AliasTarget::simple("chat-backend", "Qwen3.6-35B-A3B"),
    );
    aliases.insert(
        "bge-m3".to_string(),
        AliasTarget::simple("embed-backend", "bge-m3-Q8_0"),
    );

    Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".to_string(),
            registry_db: None,
            bearer_token_env: None,
            rate_limit_per_minute: 0,
            circuit_threshold: 5,
            circuit_window_secs: 60,
            circuit_cooldown_secs: 30,
            max_total_tokens: 0, // cap désactivé — pas l'objet de ces tests
        },
        logging: LoggingConfig {
            level: "error".to_string(),
        },
        providers,
        aliases,
    }
}

fn build_server() -> TestServer {
    let state = AppState::for_test(test_config_two_aliases());
    let router = build_router(state);
    TestServer::new(router)
}

// ---------------------------------------------------------------------------
// Tests chat — alias inconnu
// ---------------------------------------------------------------------------

/// Alias inconnu → HTTP 404.
///
/// Avant le fix : "qwen3.5-122b" était forwardé sur l'alias "default" → 200.
/// Après le fix : rejeté avec HTTP 404.
#[tokio::test]
async fn test_chat_alias_inconnu_retourne_404() {
    let server = build_server();
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "qwen3.5-122b",
            "messages": [{"role": "user", "content": "reply OK"}],
            "max_tokens": 5
        }))
        .await;
    response.assert_status(axum::http::StatusCode::NOT_FOUND);
}

/// Alias inconnu quelconque → HTTP 404.
#[tokio::test]
async fn test_chat_alias_totalement_inconnu_retourne_404() {
    let server = build_server();
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "modele-qui-nexiste-pas",
            "messages": [{"role": "user", "content": "test"}]
        }))
        .await;
    response.assert_status(axum::http::StatusCode::NOT_FOUND);
}

/// Body d'erreur 404 conforme OpenAI-compat avec liste d'aliases disponibles.
#[tokio::test]
async fn test_chat_404_body_format_et_liste_aliases() {
    let server = build_server();
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "alias-inexistant",
            "messages": [{"role": "user", "content": "test"}]
        }))
        .await;

    response.assert_status(axum::http::StatusCode::NOT_FOUND);

    let body: serde_json::Value = response.json();

    // Format OpenAI-compat.
    assert_eq!(
        body["error"]["type"].as_str().unwrap_or(""),
        "invalid_request_error",
        "error.type doit être 'invalid_request_error'"
    );
    assert_eq!(
        body["error"]["code"].as_str().unwrap_or(""),
        "model_not_found",
        "error.code doit être 'model_not_found'"
    );

    let msg = body["error"]["message"].as_str().unwrap_or("");

    // Le message doit mentionner l'alias demandé.
    assert!(
        msg.contains("alias-inexistant"),
        "message doit mentionner l'alias demandé — reçu: {msg}"
    );

    // Le message doit lister les aliases disponibles.
    assert!(
        msg.contains("default"),
        "message doit lister l'alias 'default' disponible — reçu: {msg}"
    );
    assert!(
        msg.contains("bge-m3"),
        "message doit lister l'alias 'bge-m3' disponible — reçu: {msg}"
    );
}

/// Alias "default" connu → pas de 404 (régression).
///
/// Le provider fictif (:1) est inaccessible → 502, mais pas 404.
#[tokio::test]
async fn test_chat_alias_default_connu_non_rejeté() {
    let server = build_server();
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "default",
            "messages": [{"role": "user", "content": "test"}]
        }))
        .await;
    // Le provider fictif est inaccessible → 502, mais PAS 404.
    assert_ne!(
        response.status_code(),
        axum::http::StatusCode::NOT_FOUND,
        "alias 'default' connu ne doit pas retourner 404 — reçu: {}",
        response.status_code()
    );
}

// ---------------------------------------------------------------------------
// Tests embeddings — alias inconnu
// ---------------------------------------------------------------------------

/// Alias embed inconnu → HTTP 404.
#[tokio::test]
async fn test_embeddings_alias_inconnu_retourne_404() {
    let server = build_server();
    let response = server
        .post("/v1/embeddings")
        .json(&serde_json::json!({
            "model": "invalid-embed",
            "input": "hello world"
        }))
        .await;
    response.assert_status(axum::http::StatusCode::NOT_FOUND);
}

/// Body d'erreur embeddings 404 conforme OpenAI-compat avec alias mentionné.
#[tokio::test]
async fn test_embeddings_404_body_mention_alias() {
    let server = build_server();
    let response = server
        .post("/v1/embeddings")
        .json(&serde_json::json!({
            "model": "invalid-embed",
            "input": "test"
        }))
        .await;

    response.assert_status(axum::http::StatusCode::NOT_FOUND);

    let body: serde_json::Value = response.json();
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("invalid-embed"),
        "message doit mentionner l'alias demandé — reçu: {msg}"
    );
}

/// Alias embed "bge-m3" connu → pas de 404 (régression).
///
/// Le provider fictif (:1) est inaccessible → 502, mais pas 404.
#[tokio::test]
async fn test_embeddings_alias_bge_m3_connu_non_rejeté() {
    let server = build_server();
    let response = server
        .post("/v1/embeddings")
        .json(&serde_json::json!({
            "model": "bge-m3",
            "input": "hello world"
        }))
        .await;
    // Provider fictif inaccessible → 502 ou 503, mais PAS 404.
    assert_ne!(
        response.status_code(),
        axum::http::StatusCode::NOT_FOUND,
        "alias 'bge-m3' connu ne doit pas retourner 404 — reçu: {}",
        response.status_code()
    );
}
