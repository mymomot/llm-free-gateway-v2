//! Tests smoke — gateway v2 alpha.2.
//!
//! Ces tests vérifient que le serveur démarre et répond correctement
//! sans appels réels vers un backend LLM.
//!
//! Stratégie : port aléatoire + config temporaire en mémoire.
//! `AppState::for_test()` utilise un registre SQLite :memory: — pas de fichier disque.

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
        AliasTarget::simple("test-backend", "test-model-real"),
    );

    Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".to_string(),
            registry_db: None,
            bearer_token_env: None,
            rate_limit_per_minute: 0, // désactivé dans les tests généraux
            circuit_threshold: 5,
            circuit_window_secs: 60,
            circuit_cooldown_secs: 30,
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
    let state = AppState::for_test(test_config());
    let router = build_router(state);
    TestServer::new(router)
}

// ---------------------------------------------------------------------------
// Tests health
// ---------------------------------------------------------------------------

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
    // Vérifie que la version contient "alpha.3"
    assert!(
        body["version"].as_str().unwrap_or("").contains("alpha.3"),
        "version doit contenir 'alpha.3'"
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

// ---------------------------------------------------------------------------
// Tests chat
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Tests models
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_models_returns_200() {
    let server = build_test_server();
    let response = server.get("/v1/models").await;
    response.assert_status_ok();
}

#[tokio::test]
async fn test_models_body_format_openai_compat() {
    let server = build_test_server();
    let response = server.get("/v1/models").await;
    let body: serde_json::Value = response.json();

    assert_eq!(body["object"], "list", "champ 'object' doit être 'list'");
    let data = body["data"]
        .as_array()
        .expect("'data' doit être un tableau");
    assert_eq!(data.len(), 1, "un alias configuré dans la config de test");

    let model = &data[0];
    assert_eq!(
        model["id"], "test-model",
        "id doit être l'alias 'test-model'"
    );
    assert_eq!(model["object"], "model", "objet doit être 'model'");
    assert_eq!(
        model["owned_by"], "test-backend",
        "owned_by doit être le provider"
    );
    assert!(
        model["created"].is_number(),
        "'created' doit être un nombre"
    );
}

// ---------------------------------------------------------------------------
// Tests metrics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_metrics_returns_200() {
    let server = build_test_server();
    let response = server.get("/metrics").await;
    response.assert_status_ok();
}

#[tokio::test]
async fn test_metrics_contains_type_counter() {
    let server = build_test_server();
    let response = server.get("/metrics").await;
    let body = response.text();
    assert!(
        body.contains("# TYPE gateway_requests_total counter"),
        "ligne TYPE counter attendue dans:\n{}",
        body
    );
}

#[tokio::test]
async fn test_metrics_contains_providers_configured() {
    let server = build_test_server();
    let response = server.get("/metrics").await;
    let body = response.text();
    assert!(
        body.contains("gateway_providers_configured"),
        "gauge providers_configured attendue dans:\n{}",
        body
    );
}

// ---------------------------------------------------------------------------
// Tests embeddings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_embeddings_unknown_model_returns_400() {
    let server = build_test_server();
    let response = server
        .post("/v1/embeddings")
        .json(&serde_json::json!({
            "model": "unknown-embed-model",
            "input": "hello world"
        }))
        .await;
    response.assert_status_bad_request();
    let body: serde_json::Value = response.json();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("unknown-embed-model"),
        "message d'erreur doit mentionner le modèle inconnu"
    );
}

#[tokio::test]
async fn test_embeddings_missing_model_returns_400() {
    // model non fourni → None → alias "" → inconnu → 400
    let server = build_test_server();
    let response = server
        .post("/v1/embeddings")
        .json(&serde_json::json!({
            "input": "hello world"
        }))
        .await;
    response.assert_status_bad_request();
}

// ---------------------------------------------------------------------------
// Tests auth Bearer (FINDING-C1)
// ---------------------------------------------------------------------------

/// Construit un serveur de test avec authentification Bearer activée.
fn build_auth_test_server(token: &str) -> TestServer {
    use llm_free_gateway_v2::config::{
        AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig,
    };
    use std::collections::HashMap;
    use std::sync::Arc;

    let mut providers = HashMap::new();
    providers.insert(
        "test-backend".to_string(),
        ProviderConfig {
            endpoint: "http://127.0.0.1:1".to_string(),
            timeout_secs: 5,
            api_key_env: None,
        },
    );
    let mut aliases = HashMap::new();
    aliases.insert(
        "test-model".to_string(),
        AliasTarget::simple("test-backend", "test-model-real"),
    );

    let config = Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".to_string(),
            registry_db: None,
            bearer_token_env: None, // env var non utilisée ici
            rate_limit_per_minute: 0,
            circuit_threshold: 5,
            circuit_window_secs: 60,
            circuit_cooldown_secs: 30,
        },
        logging: LoggingConfig {
            level: "error".to_string(),
        },
        providers,
        aliases,
    };

    let mut state = AppState::for_test(config);
    // Inject directement le token dans l'état pour les tests
    state.bearer_token = Some(Arc::new(token.to_string()));

    let router = build_router(state);
    TestServer::new(router)
}

#[tokio::test]
async fn test_auth_health_always_public() {
    let server = build_auth_test_server("secret123");
    // /health doit toujours passer sans token
    server.get("/health").await.assert_status_ok();
}

#[tokio::test]
async fn test_auth_models_without_token_returns_401() {
    let server = build_auth_test_server("secret123");
    let resp = server.get("/v1/models").await;
    resp.assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_auth_models_with_correct_token_returns_200() {
    let server = build_auth_test_server("secret123");
    server
        .get("/v1/models")
        .add_header("Authorization", "Bearer secret123")
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn test_auth_models_with_wrong_token_returns_401() {
    let server = build_auth_test_server("secret123");
    let resp = server
        .get("/v1/models")
        .add_header("Authorization", "Bearer wrongtoken")
        .await;
    resp.assert_status(axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_auth_metrics_protected() {
    let server = build_auth_test_server("mytoken");
    // Sans token → 401
    server
        .get("/metrics")
        .await
        .assert_status(axum::http::StatusCode::UNAUTHORIZED);
    // Avec token → 200
    server
        .get("/metrics")
        .add_header("Authorization", "Bearer mytoken")
        .await
        .assert_status_ok();
}

// ---------------------------------------------------------------------------
// Tests rate limit (FINDING-M1)
// ---------------------------------------------------------------------------

/// Construit un serveur de test avec rate limit très bas (2 req/min).
fn build_rate_limited_server() -> TestServer {
    use llm_free_gateway_v2::config::{
        AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig,
    };
    use std::collections::HashMap;

    let mut providers = HashMap::new();
    providers.insert(
        "test-backend".to_string(),
        ProviderConfig {
            endpoint: "http://127.0.0.1:1".to_string(),
            timeout_secs: 5,
            api_key_env: None,
        },
    );
    let mut aliases = HashMap::new();
    aliases.insert(
        "test-model".to_string(),
        AliasTarget::simple("test-backend", "test-model-real"),
    );

    let config = Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".to_string(),
            registry_db: None,
            bearer_token_env: None,
            rate_limit_per_minute: 2, // seuil très bas pour forcer le 429
            circuit_threshold: 5,
            circuit_window_secs: 60,
            circuit_cooldown_secs: 30,
        },
        logging: LoggingConfig {
            level: "error".to_string(),
        },
        providers,
        aliases,
    };

    let state = AppState::for_test(config);
    let router = build_router(state);
    TestServer::new(router)
}

#[tokio::test]
async fn test_rate_limit_post_enforced() {
    let server = build_rate_limited_server();

    // axum-test n'a pas de vrai TcpListener → ConnectInfo absente → toutes les requêtes
    // partagent la même IP fictive (127.0.0.1). Donc le quota de 2 sera atteint.

    // Les 2 premières requêtes peuvent échouer pour d'autres raisons (pas de backend réel)
    // mais ne doivent pas retourner 429.
    let r1 = server
        .post("/v1/chat/completions")
        .json(
            &serde_json::json!({"model":"test-model","messages":[{"role":"user","content":"hi"}]}),
        )
        .await;
    assert_ne!(
        r1.status_code(),
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        "1ère requête ne doit pas être rate-limitée"
    );

    let r2 = server
        .post("/v1/chat/completions")
        .json(
            &serde_json::json!({"model":"test-model","messages":[{"role":"user","content":"hi"}]}),
        )
        .await;
    assert_ne!(
        r2.status_code(),
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        "2ème requête ne doit pas être rate-limitée"
    );

    // La 3ème dépasse la limite → 429
    let r3 = server
        .post("/v1/chat/completions")
        .json(
            &serde_json::json!({"model":"test-model","messages":[{"role":"user","content":"hi"}]}),
        )
        .await;
    r3.assert_status(axum::http::StatusCode::TOO_MANY_REQUESTS);

    // Le header Retry-After doit être présent
    let retry_after = r3.headers().get("Retry-After");
    assert!(retry_after.is_some(), "Retry-After header attendu sur 429");
}

#[tokio::test]
async fn test_rate_limit_get_not_enforced() {
    // Les endpoints GET (/health, /metrics, /v1/models) ne sont pas soumis au rate limit.
    // Note : le rate limit est dans les handlers POST eux-mêmes, pas dans un layer global.
    // Les GET ne passent jamais par les handlers chat/embeddings.
    let server = build_rate_limited_server();

    // Dépasser le quota en GET — ne doit jamais retourner 429.
    for _ in 0..10 {
        let resp = server.get("/health").await;
        assert_ne!(
            resp.status_code(),
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
    }
}

// ---------------------------------------------------------------------------
// Tests body limit (FINDING-M1 — DefaultBodyLimit 4MB)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_body_limit_large_payload_rejected() {
    let server = build_test_server();

    // Génère un payload > 4MB
    let large_content = "x".repeat(5 * 1024 * 1024); // 5MB
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": large_content}]
        }))
        .await;

    // 413 Payload Too Large attendu
    assert_eq!(
        response.status_code().as_u16(),
        413,
        "payload > 4MB doit retourner 413 — reçu : {}",
        response.status_code()
    );
}
