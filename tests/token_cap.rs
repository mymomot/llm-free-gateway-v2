//! Tests intégration — Cap hard tokens (C4 council homelab-gouvernance MAJOR 2026-04-25).
//!
//! Vérifie que le gateway retourne HTTP 413 quand `input + max_tokens > max_total_tokens`.
//! Et que les requêtes dans la limite passent normalement (200 ou 502 selon le backend).
//!
//! Cas testés :
//! 1. Requête ~190K tokens estimés → HTTP 413 (cap 180K)
//! 2. Requête ~150K tokens estimés → HTTP non-413 (passthrough — 502 car provider fictif)
//! 3. Config override max_total_tokens=50000 → cap plus strict (rejet à ~60K tokens)
//! 4. Cap désactivé (max_total_tokens=0) → pas de rejet quelle que soit la taille
//! 5. Body JSON conforme OpenAI-compat : champs error.type, error.code, error.message
//!
//! Pattern : provider fictif → les requêtes qui passent le cap retournent 502 (backend unreachable),
//! pas 200. C'est attendu et documenté dans chaque test.

use axum_test::TestServer;
use llm_free_gateway_v2::{build_router, AppState};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_config_with_cap(max_total_tokens: u64) -> llm_free_gateway_v2::config::Config {
    use llm_free_gateway_v2::config::{
        AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig,
    };
    use std::collections::HashMap;

    let mut providers = HashMap::new();
    providers.insert(
        "fictif".to_string(),
        ProviderConfig {
            // Port 1 — toujours refusé (connection refused), jamais atteint si cap bloque.
            endpoint: "http://127.0.0.1:1".to_string(),
            timeout_secs: 2,
            api_key_env: None,
        },
    );

    let mut aliases = HashMap::new();
    aliases.insert(
        "cap-test-model".to_string(),
        AliasTarget::simple("fictif", "cap-test-real"),
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
            max_total_tokens,
        },
        logging: LoggingConfig {
            level: "error".to_string(),
        },
        providers,
        aliases,
    }
}

fn build_test_server_with_cap(max_total_tokens: u64) -> TestServer {
    let state = AppState::for_test(test_config_with_cap(max_total_tokens));
    let router = build_router(state);
    TestServer::new(router)
}

/// Génère un contenu de ~N tokens estimés (heuristique chars/4).
/// N tokens → N*4 caractères ASCII.
fn content_of_approx_tokens(n_tokens: u64) -> String {
    "x".repeat((n_tokens * 4) as usize)
}

// ---------------------------------------------------------------------------
// Tests cap tokens
// ---------------------------------------------------------------------------

/// Requête avec ~190K tokens estimés → HTTP 413 (cap 180K).
///
/// Input : 190K tokens ASCII + max_tokens=100 → total ~190100 > 180000.
/// Le gateway doit rejeter AVANT d'appeler le provider fictif.
#[tokio::test]
async fn test_cap_190k_retourne_413() {
    let server = build_test_server_with_cap(180_000);

    // 189900 tokens input ASCII + 100 max_tokens = 190000 > 180000
    let content = content_of_approx_tokens(189_900);
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "cap-test-model",
            "messages": [{"role": "user", "content": content}],
            "max_tokens": 100
        }))
        .await;

    response.assert_status(axum::http::StatusCode::PAYLOAD_TOO_LARGE);
}

/// Requête avec ~150K tokens estimés → pas de 413 (cap 180K).
///
/// Le provider fictif (:1) est inaccessible → 502 Backend, mais pas de rejet cap.
/// Ce test vérifie que le chemin "sous le cap → passthrough" fonctionne.
#[tokio::test]
async fn test_cap_150k_passe_cap_retourne_non_413() {
    let server = build_test_server_with_cap(180_000);

    // 149900 tokens input + 100 max_tokens = 150000 < 180000 → cap OK
    let content = content_of_approx_tokens(149_900);
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "cap-test-model",
            "messages": [{"role": "user", "content": content}],
            "max_tokens": 100
        }))
        .await;

    // Le provider fictif est inaccessible → 502, mais PAS 413.
    assert_ne!(
        response.status_code(),
        axum::http::StatusCode::PAYLOAD_TOO_LARGE,
        "requête sous le cap ne doit pas retourner 413"
    );
}

/// Override max_total_tokens=50000 → rejet à ~60K tokens.
#[tokio::test]
async fn test_cap_override_50k_rejette_60k() {
    let server = build_test_server_with_cap(50_000);

    // 59900 tokens input + 100 max_tokens = 60000 > 50000
    let content = content_of_approx_tokens(59_900);
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "cap-test-model",
            "messages": [{"role": "user", "content": content}],
            "max_tokens": 100
        }))
        .await;

    response.assert_status(axum::http::StatusCode::PAYLOAD_TOO_LARGE);
}

/// Cap désactivé (max_total_tokens=0) → pas de rejet même sur grand corpus.
#[tokio::test]
async fn test_cap_désactivé_ne_rejette_pas() {
    let server = build_test_server_with_cap(0); // 0 = cap désactivé

    // 500K tokens — ne doit pas être rejeté par le cap.
    let content = content_of_approx_tokens(500_000);
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "cap-test-model",
            "messages": [{"role": "user", "content": content}],
            "max_tokens": 100
        }))
        .await;

    assert_ne!(
        response.status_code(),
        axum::http::StatusCode::PAYLOAD_TOO_LARGE,
        "cap désactivé (0) ne doit jamais retourner 413"
    );
}

/// Format du body d'erreur 413 conforme OpenAI-compat.
///
/// Vérifie les champs : error.type, error.code, error.message.
#[tokio::test]
async fn test_cap_error_body_format_openai_compat() {
    let server = build_test_server_with_cap(180_000);

    // 190K tokens → 413
    let content = content_of_approx_tokens(190_000);
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "cap-test-model",
            "messages": [{"role": "user", "content": content}],
            "max_tokens": 0
        }))
        .await;

    response.assert_status(axum::http::StatusCode::PAYLOAD_TOO_LARGE);

    let body: serde_json::Value = response.json();

    // Format OpenAI-compat : { "error": { "type": "...", "code": "...", "message": "..." } }
    assert_eq!(
        body["error"]["type"].as_str().unwrap_or(""),
        "invalid_request_error",
        "type doit être 'invalid_request_error'"
    );
    assert_eq!(
        body["error"]["code"].as_str().unwrap_or(""),
        "context_length_exceeded",
        "code doit être 'context_length_exceeded'"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("180000"),
        "message doit mentionner le cap"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("262K")
            || body["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("262"),
        "message doit mentionner le slot ctx Qwen3.6 (262K)"
    );
}

/// Requête exactement au seuil (= cap) → acceptée (frontière inclusive).
///
/// Convention : total == cap → OK (seul > cap déclenche le rejet).
#[tokio::test]
async fn test_cap_exactement_au_seuil_passe() {
    let cap = 10_000u64;
    let server = build_test_server_with_cap(cap);

    // Construire un message dont le total estimé = exactement cap.
    // estimate_total_tokens = ceil(chars/4) + 4 overhead + max_tokens
    // On veut : total = cap = 10000
    // max_tokens = 0 → total = ceil(chars/4) + 4
    // chars/4 + 4 = 10000 → chars = (10000 - 4) * 4 = 39984
    let content = "x".repeat(39_984);
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "cap-test-model",
            "messages": [{"role": "user", "content": content}],
            "max_tokens": 0
        }))
        .await;

    // Au seuil exact → accepté (pas de 413).
    assert_ne!(
        response.status_code(),
        axum::http::StatusCode::PAYLOAD_TOO_LARGE,
        "requête exactement au seuil ne doit pas retourner 413"
    );
}
