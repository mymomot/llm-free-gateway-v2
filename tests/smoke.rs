//! Tests smoke — gateway v2 alpha.1.
//!
//! Ces tests vérifient que le serveur démarre et répond correctement
//! sans appels réels vers un backend LLM.
//!
//! Stratégie : port aléatoire + config temporaire en mémoire.

use std::sync::Arc;

use axum_test::TestServer;
use llm_free_gateway_v2::{build_router, AppState};

/// Crée une config minimale pour les tests (provider fictif, pas d'accès réseau).
fn test_config() -> llm_free_gateway_v2::config::Config {
    use llm_free_gateway_v2::config::{
        AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig,
    };
    use std::collections::HashMap;

    let mut providers = HashMap::new();
    providers.insert(
        "test-backend".to_string(),
        ProviderConfig {
            endpoint: "http://127.0.0.1:1".to_string(), // port 1 — toujours refusé
            timeout_secs: 5,
            api_key_env: None,
        },
    );

    let mut aliases = HashMap::new();
    aliases.insert(
        "test-model".to_string(),
        AliasTarget {
            provider: "test-backend".to_string(),
            model: "test-model-real".to_string(),
        },
    );

    Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".to_string(),
        },
        logging: LoggingConfig {
            level: "error".to_string(),
        },
        providers,
        aliases,
    }
}

/// Construit un `TestServer` axum-test avec la config de test.
fn build_test_server() -> TestServer {
    let state = AppState {
        config: Arc::new(test_config()),
    };
    let router = build_router(state);
    TestServer::new(router)
}

#[tokio::test]
async fn test_health_returns_200() {
    let server = build_test_server();
    let response = server.get("/health").await;
    response.assert_status_ok();
}

#[tokio::test]
async fn test_health_body_status_ok() {
    let server = build_test_server();
    let response = server.get("/health").await;
    response.assert_status_ok();
    let body: serde_json::Value = response.json();
    assert_eq!(body["status"], "ok", "champ 'status' attendu 'ok'");
}

#[tokio::test]
async fn test_health_body_version_present() {
    let server = build_test_server();
    let response = server.get("/health").await;
    let body: serde_json::Value = response.json();
    assert!(
        body["version"].is_string(),
        "champ 'version' doit être présent et string"
    );
    // Vérifie que la version contient "alpha.1"
    assert!(
        body["version"].as_str().unwrap_or("").contains("alpha.1"),
        "version doit contenir 'alpha.1'"
    );
}

#[tokio::test]
async fn test_health_body_providers_list() {
    let server = build_test_server();
    let response = server.get("/health").await;
    let body: serde_json::Value = response.json();
    let providers = body["providers"]
        .as_array()
        .expect("'providers' doit être un tableau");
    assert_eq!(
        providers.len(),
        1,
        "un provider configuré dans la config de test"
    );
    assert_eq!(providers[0], "test-backend");
}

#[tokio::test]
async fn test_chat_unknown_model_returns_400() {
    let server = build_test_server();
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "modele-inconnu-xyz",
            "messages": [{"role": "user", "content": "bonjour"}]
        }))
        .await;
    response.assert_status_bad_request();
    let body: serde_json::Value = response.json();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("modele-inconnu-xyz"),
        "message d'erreur doit mentionner le nom du modèle"
    );
}

#[tokio::test]
async fn test_chat_unknown_model_error_format() {
    // Vérifie que le format d'erreur est conforme OpenAI-compat.
    let server = build_test_server();
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "inexistant",
            "messages": [{"role": "user", "content": "test"}]
        }))
        .await;
    let body: serde_json::Value = response.json();
    // Format OpenAI-compat : { "error": { "message": "...", "type": "...", "code": "..." } }
    assert!(
        body["error"].is_object(),
        "réponse doit avoir un champ 'error' objet"
    );
    assert!(
        body["error"]["message"].is_string(),
        "'error.message' doit être string"
    );
    assert!(
        body["error"]["type"].is_string(),
        "'error.type' doit être string"
    );
    assert!(
        body["error"]["code"].is_string(),
        "'error.code' doit être string"
    );
}
