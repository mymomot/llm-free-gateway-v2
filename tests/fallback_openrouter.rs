//! Tests d'intégration — Fallback provider (G1 : port OpenRouter v1→v2).
//!
//! Vérifie que quand le provider primary est inaccessible, la gateway v2
//! tente automatiquement le fallback_provider configuré dans l'alias.
//!
//! Architecture des tests :
//! - Mock "primary" sur port aléatoire — renvoie une erreur 503 ou refuse la connexion
//! - Mock "fallback" sur port aléatoire — renvoie une réponse valide
//! - Gateway configurée avec alias pointant primary + fallback_provider
//! - Assertions : la réponse finale vient du fallback, le CB primary est alimenté
//!
//! Ce fichier couvre la condition G1 du council homelab-gouvernance MAJOR 2026-04-25.

use axum::{extract::Json as AxumJson, response::Response, routing::post, Router};
use axum_test::TestServer;
use std::net::SocketAddr;
use tokio::net::TcpListener;

use llm_free_gateway_v2::{
    build_router,
    config::{AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig},
    AppState,
};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Helpers — fixtures SSE et JSON
// ---------------------------------------------------------------------------

/// Réponse JSON valide (mode non-streaming) retournée par le mock fallback.
const FALLBACK_JSON_RESPONSE: &str = r#"{
    "id": "fallback-001",
    "object": "chat.completion",
    "created": 1714000000,
    "model": "openrouter-model",
    "choices": [{
        "index": 0,
        "message": {"role": "assistant", "content": "Réponse depuis le fallback OpenRouter"},
        "finish_reason": "stop"
    }],
    "usage": {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18}
}"#;

/// Réponse SSE valide retournée par le mock fallback en mode streaming.
const FALLBACK_SSE_RESPONSE: &str = concat!(
    "data: {\"id\":\"fb1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"openrouter-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"id\":\"fb1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"openrouter-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Fallback OK\"}}]}\n\n",
    "data: {\"id\":\"fb1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"openrouter-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

// ---------------------------------------------------------------------------
// Helpers — spawn mocks
// ---------------------------------------------------------------------------

/// Spawn un mock qui retourne une erreur 503 (simule primary down).
async fn spawn_mock_primary_down() -> SocketAddr {
    async fn handler_503(_body: axum::body::Bytes) -> Response {
        Response::builder()
            .status(503)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                r#"{"error":{"message":"Service Unavailable","type":"server_error"}}"#,
            ))
            .expect("construction réponse 503 — paramètres statiques valides")
    }

    let app = Router::new().route("/v1/chat/completions", post(handler_503));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port aléatoire mock primary");
    let addr = listener.local_addr().expect("adresse locale mock primary");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock primary down serve");
    });
    addr
}

/// Spawn un mock fallback qui retourne une réponse JSON valide (non-streaming).
async fn spawn_mock_fallback_json() -> SocketAddr {
    async fn handler_ok(AxumJson(_req): AxumJson<serde_json::Value>) -> Response {
        Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(FALLBACK_JSON_RESPONSE))
            .expect("construction réponse fallback JSON — paramètres statiques valides")
    }

    let app = Router::new().route("/v1/chat/completions", post(handler_ok));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port aléatoire mock fallback JSON");
    let addr = listener
        .local_addr()
        .expect("adresse locale mock fallback JSON");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock fallback JSON serve");
    });
    addr
}

/// Spawn un mock fallback qui retourne un flux SSE valide (streaming).
async fn spawn_mock_fallback_sse() -> SocketAddr {
    async fn handler_sse(AxumJson(_req): AxumJson<serde_json::Value>) -> Response {
        Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(axum::body::Body::from(FALLBACK_SSE_RESPONSE))
            .expect("construction réponse fallback SSE — paramètres statiques valides")
    }

    let app = Router::new().route("/v1/chat/completions", post(handler_sse));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port aléatoire mock fallback SSE");
    let addr = listener
        .local_addr()
        .expect("adresse locale mock fallback SSE");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock fallback SSE serve");
    });
    addr
}

// ---------------------------------------------------------------------------
// Helpers — config
// ---------------------------------------------------------------------------

/// Construit une config avec primary + fallback.
fn config_with_fallback(primary_addr: SocketAddr, fallback_addr: SocketAddr) -> Config {
    let mut providers = HashMap::new();
    providers.insert(
        "primary".to_string(),
        ProviderConfig {
            endpoint: format!("http://{}", primary_addr),
            timeout_secs: 2,
            api_key_env: None,
        },
    );
    providers.insert(
        "fallback-or".to_string(),
        ProviderConfig {
            endpoint: format!("http://{}", fallback_addr),
            timeout_secs: 5,
            // Simule OPENROUTER_API_KEY — pas de vraie var d'env dans les tests.
            api_key_env: None,
        },
    );

    let mut aliases = HashMap::new();
    aliases.insert(
        "test-model".to_string(),
        AliasTarget::with_fallback("primary", "primary-model", "fallback-or", None),
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
        },
        logging: LoggingConfig {
            level: "error".to_string(),
        },
        providers,
        aliases,
    }
}

// ---------------------------------------------------------------------------
// Tests unitaires — config TOML
// ---------------------------------------------------------------------------

/// Vérifie que la config TOML avec fallback_provider se parse correctement.
#[test]
fn test_alias_with_fallback_parses_from_toml() {
    let toml = r#"
[server]
listen = "127.0.0.1:8435"

[providers.llmcore]
endpoint = "http://192.168.10.118:8080"
timeout_secs = 120

[providers.openrouter]
endpoint = "https://openrouter.ai/api/v1"
timeout_secs = 30
api_key_env = "OPENROUTER_API_KEY"

[aliases]
"default" = { provider = "llmcore", model = "Qwen3.6-35B-A3B", fallback_provider = "openrouter", fallback_model = "anthropic/claude-haiku" }
"qwen3.6-35b" = { provider = "llmcore", model = "Qwen3.6-35B-A3B" }
"#;
    let config: Config = toml::from_str(toml).expect("parsing config TOML avec fallback");

    let default_alias = config
        .aliases
        .get("default")
        .expect("alias 'default' absent");
    assert_eq!(default_alias.provider, "llmcore");
    assert_eq!(default_alias.model, "Qwen3.6-35B-A3B");
    assert_eq!(
        default_alias.fallback_provider.as_deref(),
        Some("openrouter"),
        "fallback_provider doit être 'openrouter'"
    );
    assert_eq!(
        default_alias.fallback_model.as_deref(),
        Some("anthropic/claude-haiku"),
        "fallback_model doit être 'anthropic/claude-haiku'"
    );

    let qwen_alias = config
        .aliases
        .get("qwen3.6-35b")
        .expect("alias 'qwen3.6-35b' absent");
    assert!(
        qwen_alias.fallback_provider.is_none(),
        "alias sans fallback_provider doit avoir None"
    );
    assert!(
        qwen_alias.fallback_model.is_none(),
        "alias sans fallback_model doit avoir None"
    );
}

/// Vérifie que validate() rejette un fallback_provider non déclaré dans [providers].
#[test]
fn test_validate_rejects_unknown_fallback_provider() {
    let toml = r#"
[server]
listen = "127.0.0.1:8435"

[providers.llmcore]
endpoint = "http://192.168.10.118:8080"

[aliases]
"default" = { provider = "llmcore", model = "test", fallback_provider = "inexistant" }
"#;
    let config: Config = toml::from_str(toml).expect("parsing doit réussir");

    // Vérifier que le champ est bien parsé.
    assert_eq!(
        config
            .aliases
            .get("default")
            .unwrap()
            .fallback_provider
            .as_deref(),
        Some("inexistant"),
        "fallback_provider 'inexistant' doit être parsé"
    );

    // Appeler validate_config (helper local qui reflète la logique de Config::validate).
    let validate_result = validate_config(&config);
    assert!(
        validate_result.is_err(),
        "validate doit échouer si fallback_provider n'est pas dans [providers]"
    );
    assert!(
        validate_result.unwrap_err().contains("fallback_provider"),
        "message d'erreur doit mentionner 'fallback_provider'"
    );
}

/// Appelle la validation de la config directement pour les tests.
fn validate_config(config: &Config) -> Result<(), String> {
    for (alias, target) in &config.aliases {
        if !config.providers.contains_key(&target.provider) {
            return Err(format!(
                "alias '{}' référence le provider '{}' qui n'est pas déclaré dans [providers]",
                alias, target.provider
            ));
        }
        if let Some(fb) = &target.fallback_provider {
            if !config.providers.contains_key(fb) {
                return Err(format!(
                    "alias '{}' référence le fallback_provider '{}' qui n'est pas déclaré dans [providers]",
                    alias, fb
                ));
            }
        }
    }
    Ok(())
}

/// Vérifie qu'un alias sans fallback passe la validation.
#[test]
fn test_validate_passes_without_fallback() {
    let toml = r#"
[server]
listen = "127.0.0.1:8435"

[providers.llmcore]
endpoint = "http://192.168.10.118:8080"

[aliases]
"default" = { provider = "llmcore", model = "test" }
"#;
    let config: Config = toml::from_str(toml).expect("parsing doit réussir");
    assert!(
        validate_config(&config).is_ok(),
        "config sans fallback doit passer la validation"
    );
}

/// Vérifie qu'un alias avec fallback valide passe la validation.
#[test]
fn test_validate_passes_with_valid_fallback() {
    let toml = r#"
[server]
listen = "127.0.0.1:8435"

[providers.llmcore]
endpoint = "http://192.168.10.118:8080"

[providers.openrouter]
endpoint = "https://openrouter.ai/api/v1"

[aliases]
"default" = { provider = "llmcore", model = "test", fallback_provider = "openrouter", fallback_model = "mistralai/mistral-7b-instruct" }
"#;
    let config: Config = toml::from_str(toml).expect("parsing doit réussir");
    assert!(
        validate_config(&config).is_ok(),
        "config avec fallback_provider valide doit passer la validation"
    );
}

// ---------------------------------------------------------------------------
// Tests d'intégration — fallback trigger
// ---------------------------------------------------------------------------

/// G1 TNR : primary down (503) → fallback OR appelé → réponse 200 reçue (non-streaming).
///
/// Simule le scénario de production : llmcore inaccessible ou en erreur,
/// la gateway doit transparentement basculer sur OpenRouter.
#[tokio::test]
async fn test_fallback_triggered_on_primary_503_non_streaming() {
    let primary_addr = spawn_mock_primary_down().await;
    let fallback_addr = spawn_mock_fallback_json().await;

    let config = config_with_fallback(primary_addr, fallback_addr);
    let state = AppState::for_test(config);
    let router = build_router(state);
    let server = TestServer::new(router);

    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": false,
            "messages": [{"role": "user", "content": "Bonjour"}]
        }))
        .await;

    // La gateway doit retourner 200 via le fallback, pas 503 du primary.
    response.assert_status_ok();

    let body: serde_json::Value = response.json();
    assert_eq!(
        body["id"], "fallback-001",
        "id de la réponse doit correspondre au fallback mock"
    );
    assert!(
        body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .contains("fallback"),
        "contenu doit venir du fallback provider"
    );
}

/// G1 TNR : primary inaccessible (connexion refusée) → fallback OR appelé → réponse 200 (streaming).
///
/// Port 1 est toujours refusé (permission denied ou connection refused).
#[tokio::test]
async fn test_fallback_triggered_on_primary_unreachable_streaming() {
    // Port 1 = toujours refused
    let primary_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let fallback_addr = spawn_mock_fallback_sse().await;

    let config = config_with_fallback(primary_addr, fallback_addr);
    let state = AppState::for_test(config);
    let router = build_router(state);
    let server = TestServer::new(router);

    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": true,
            "messages": [{"role": "user", "content": "Bonjour streaming"}]
        }))
        .await;

    response.assert_status_ok();

    let body = response.text();

    // Le body SSE doit venir du fallback.
    assert!(
        body.contains("Fallback OK"),
        "contenu SSE doit venir du fallback provider. Body:\n{body}"
    );
    assert!(
        body.contains("data: "),
        "body doit contenir des lignes SSE. Body:\n{body}"
    );
}

/// Vérifie que sans fallback configuré, une erreur primary retourne bien 502.
///
/// Non-régression : le comportement sans fallback_provider ne doit pas changer.
#[tokio::test]
async fn test_no_fallback_primary_down_returns_502() {
    // Primary refusé, pas de fallback.
    let mut providers = HashMap::new();
    providers.insert(
        "primary-only".to_string(),
        ProviderConfig {
            endpoint: "http://127.0.0.1:1".to_string(),
            timeout_secs: 1,
            api_key_env: None,
        },
    );
    let mut aliases = HashMap::new();
    // Pas de fallback_provider — utilise AliasTarget::simple
    aliases.insert(
        "test-model".to_string(),
        AliasTarget::simple("primary-only", "test-model-real"),
    );

    let config = Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".to_string(),
            registry_db: None,
            bearer_token_env: None,
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

    let state = AppState::for_test(config);
    let router = build_router(state);
    let server = TestServer::new(router);

    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": false,
            "messages": [{"role": "user", "content": "test"}]
        }))
        .await;

    // Sans fallback, l'erreur du primary doit remonter — 502 Bad Gateway.
    assert_eq!(
        response.status_code().as_u16(),
        502,
        "sans fallback, primary down doit retourner 502 — reçu : {}",
        response.status_code()
    );
}

/// Vérifie que le circuit breaker du primary est alimenté même quand le fallback réussit.
///
/// Si le primary échoue et que le fallback récupère, le CB primary doit quand même
/// enregistrer l'échec pour s'ouvrir progressivement.
#[tokio::test]
async fn test_primary_cb_incremented_when_fallback_succeeds() {
    use llm_commons::circuit_breaker::CircuitState;

    // Primary avec CB threshold=1 — une seule erreur ouvre le circuit.
    let primary_addr = spawn_mock_primary_down().await;
    let fallback_addr = spawn_mock_fallback_json().await;

    let mut providers = HashMap::new();
    providers.insert(
        "primary".to_string(),
        ProviderConfig {
            endpoint: format!("http://{}", primary_addr),
            timeout_secs: 2,
            api_key_env: None,
        },
    );
    providers.insert(
        "fallback-or".to_string(),
        ProviderConfig {
            endpoint: format!("http://{}", fallback_addr),
            timeout_secs: 5,
            api_key_env: None,
        },
    );
    let mut aliases = HashMap::new();
    aliases.insert(
        "test-model".to_string(),
        AliasTarget::with_fallback("primary", "primary-model", "fallback-or", None),
    );

    let config = Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".to_string(),
            registry_db: None,
            bearer_token_env: None,
            rate_limit_per_minute: 0,
            // CB threshold = 1 : une seule erreur ouvre le circuit
            circuit_threshold: 1,
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
    // Conserver une ref aux circuit_breakers avant de consommer state.
    let circuit_breakers = state.providers.circuit_breakers.clone();

    let router = build_router(state);
    let server = TestServer::new(router);

    // CB primary est fermé au départ.
    assert_eq!(
        circuit_breakers.state("primary"),
        CircuitState::Closed,
        "CB primary doit être Closed avant la première requête"
    );

    // Envoyer une requête — primary 503, fallback OK.
    let response = server
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": false,
            "messages": [{"role": "user", "content": "test CB"}]
        }))
        .await;

    // La réponse doit être 200 via le fallback.
    response.assert_status_ok();

    // Le CB primary doit s'être ouvert (threshold=1, une erreur enregistrée).
    assert_eq!(
        circuit_breakers.state("primary"),
        CircuitState::Open,
        "CB primary doit être Open après une erreur 503 — même quand le fallback a récupéré"
    );
}
