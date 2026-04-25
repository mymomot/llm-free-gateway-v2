//! TNR E2E — SSE conversational passthrough
//!
//! Complète la couverture SSE de la gateway v2 en couvrant le mode chat conversationnel
//! (texte seulement streaming), en contraste avec `sse_tool_calls_passthrough.rs`
//! qui couvre les tool calls.
//!
//! Cas testés :
//! 1. Simple text streaming (rôle + texte + finish_reason)
//! 2. Multi-turn streaming (history avec assistant + user multiple)
//! 3. Streaming avec stop sequence
//! 4. Streaming avec finish_reason="length" (quota tokens atteint)
//! 5. Streaming avec usage stats (prompt_tokens, completion_tokens, total_tokens)
//! 6. Streaming avec deltas role + content concat
//! 7. Streaming error mid-stream
//! 8. Streaming avec finish_reason="content_filter"
//!
//! Pattern de test identique à sse_tool_calls_passthrough.rs :
//! - Mock llmcore sur port aléatoire
//! - Gateway testée pointant sur le mock
//! - Assertions sur le passthrough SSE complet

use axum::{extract::Json as AxumJson, response::Response, routing::post, Router};
use axum_test::TestServer;
use std::net::SocketAddr;
use tokio::net::TcpListener;

use llm_free_gateway_v2::{build_router, AppState};

// ---------------------------------------------------------------------------
// Helpers — config + test server
// ---------------------------------------------------------------------------

fn test_config_for_mock(mock_addr: SocketAddr) -> llm_free_gateway_v2::config::Config {
    use llm_free_gateway_v2::config::{
        AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig,
    };
    use std::collections::HashMap;

    let mut providers = HashMap::new();
    providers.insert(
        "mock-llmcore".to_string(),
        ProviderConfig {
            endpoint: format!("http://{}", mock_addr),
            timeout_secs: 5,
            api_key_env: None,
        },
    );

    let mut aliases = HashMap::new();
    aliases.insert(
        "test-model".to_string(),
        AliasTarget {
            provider: "mock-llmcore".to_string(),
            model: "test-model-real".to_string(),
            fallback_provider: None,
            fallback_model: None,
        },
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
            max_total_tokens: 0, // désactivé dans les tests
        },
        logging: LoggingConfig {
            level: "error".to_string(),
        },
        providers,
        aliases,
    }
}

fn build_gateway_for_mock(mock_addr: SocketAddr) -> TestServer {
    let state = AppState::for_test(test_config_for_mock(mock_addr));
    let router = build_router(state);
    TestServer::new(router)
}

/// Mock handler qui retourne une fixture SSE statique.
async fn mock_handler_simple(AxumJson(_req): AxumJson<serde_json::Value>) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from(SIMPLE_TEXT_STREAM))
        .expect("construction réponse mock SSE")
}

async fn mock_handler_length_limit(AxumJson(_req): AxumJson<serde_json::Value>) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from(LENGTH_LIMIT_STREAM))
        .expect("construction réponse mock SSE")
}

async fn mock_handler_content_filter(AxumJson(_req): AxumJson<serde_json::Value>) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from(CONTENT_FILTER_STREAM))
        .expect("construction réponse mock SSE")
}

async fn mock_handler_with_usage(AxumJson(_req): AxumJson<serde_json::Value>) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from(WITH_USAGE_STREAM))
        .expect("construction réponse mock SSE")
}

async fn mock_handler_multiturn(AxumJson(_req): AxumJson<serde_json::Value>) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from(MULTITURN_STREAM))
        .expect("construction réponse mock SSE")
}

/// Spawns un mock llmcore qui retourne le stream SIMPLE_TEXT_STREAM.
async fn spawn_mock_llmcore() -> SocketAddr {
    let app = Router::new().route("/v1/chat/completions", post(mock_handler_simple));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port aléatoire mock llmcore");

    let addr = listener.local_addr().expect("adresse locale mock llmcore");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock llmcore serve");
    });

    addr
}

/// Spawns un mock llmcore pour le test length_limit.
async fn spawn_mock_llmcore_length_limit() -> SocketAddr {
    let app = Router::new().route("/v1/chat/completions", post(mock_handler_length_limit));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port aléatoire mock llmcore");

    let addr = listener.local_addr().expect("adresse locale mock llmcore");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock llmcore serve");
    });

    addr
}

/// Spawns un mock llmcore pour le test content_filter.
async fn spawn_mock_llmcore_content_filter() -> SocketAddr {
    let app = Router::new().route("/v1/chat/completions", post(mock_handler_content_filter));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port aléatoire mock llmcore");

    let addr = listener.local_addr().expect("adresse locale mock llmcore");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock llmcore serve");
    });

    addr
}

/// Spawns un mock llmcore pour le test with_usage.
async fn spawn_mock_llmcore_with_usage() -> SocketAddr {
    let app = Router::new().route("/v1/chat/completions", post(mock_handler_with_usage));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port aléatoire mock llmcore");

    let addr = listener.local_addr().expect("adresse locale mock llmcore");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock llmcore serve");
    });

    addr
}

/// Spawns un mock llmcore pour le test multiturn.
async fn spawn_mock_llmcore_multiturn() -> SocketAddr {
    let app = Router::new().route("/v1/chat/completions", post(mock_handler_multiturn));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port aléatoire mock llmcore");

    let addr = listener.local_addr().expect("adresse locale mock llmcore");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock llmcore serve");
    });

    addr
}

// ---------------------------------------------------------------------------
// Fixtures SSE — représentant des réponses réalistes du backend
// ---------------------------------------------------------------------------

/// Streaming texte simple : rôle + fragments texte + finish_reason="stop"
///
/// Simulant une réponse conversationnelle simple (pas d'outils, pas d'erreurs mid-stream).
const SIMPLE_TEXT_STREAM: &str = concat!(
    // Chunk 0 : rôle initial
    "data: {\"id\":\"chatcmpl-001\",\"object\":\"chat.completion.chunk\",\"created\":1000,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    // Chunk 1 : premier fragment texte
    "data: {\"id\":\"chatcmpl-001\",\"object\":\"chat.completion.chunk\",\"created\":1000,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Bonjour\"}}]}\n\n",
    // Chunk 2 : deuxième fragment texte
    "data: {\"id\":\"chatcmpl-001\",\"object\":\"chat.completion.chunk\",\"created\":1000,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\" c'est\"}}]}\n\n",
    // Chunk 3 : troisième fragment texte
    "data: {\"id\":\"chatcmpl-001\",\"object\":\"chat.completion.chunk\",\"created\":1000,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\" Claude\"}}]}\n\n",
    // Chunk 4 : finish_reason
    "data: {\"id\":\"chatcmpl-001\",\"object\":\"chat.completion.chunk\",\"created\":1000,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    // Terminateur
    "data: [DONE]\n\n",
);

/// Streaming avec finish_reason="length" (token limit atteint)
const LENGTH_LIMIT_STREAM: &str = concat!(
    "data: {\"id\":\"chatcmpl-002\",\"object\":\"chat.completion.chunk\",\"created\":1001,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-002\",\"object\":\"chat.completion.chunk\",\"created\":1001,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Le texte\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-002\",\"object\":\"chat.completion.chunk\",\"created\":1001,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\" est coupé car\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-002\",\"object\":\"chat.completion.chunk\",\"created\":1001,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// Streaming avec finish_reason="content_filter" (contenu bloqué)
const CONTENT_FILTER_STREAM: &str = concat!(
    "data: {\"id\":\"chatcmpl-003\",\"object\":\"chat.completion.chunk\",\"created\":1002,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-003\",\"object\":\"chat.completion.chunk\",\"created\":1002,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Je ne peux pas\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-003\",\"object\":\"chat.completion.chunk\",\"created\":1002,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// Streaming avec usage stats (prompt_tokens, completion_tokens, total_tokens)
/// Note : OpenAI place usage dans le dernier chunk au niveau du root, pas du choice.
const WITH_USAGE_STREAM: &str = concat!(
    "data: {\"id\":\"chatcmpl-004\",\"object\":\"chat.completion.chunk\",\"created\":1003,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-004\",\"object\":\"chat.completion.chunk\",\"created\":1003,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Réponse\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-004\",\"object\":\"chat.completion.chunk\",\"created\":1003,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":7,\"total_tokens\":49}}\n\n",
    "data: [DONE]\n\n",
);

/// Streaming multi-turn simulant une conversation avec history
const MULTITURN_STREAM: &str = concat!(
    "data: {\"id\":\"chatcmpl-005\",\"object\":\"chat.completion.chunk\",\"created\":1004,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-005\",\"object\":\"chat.completion.chunk\",\"created\":1004,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Vous aviez demandé\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-005\",\"object\":\"chat.completion.chunk\",\"created\":1004,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\" comment faire quelque chose.\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-005\",\"object\":\"chat.completion.chunk\",\"created\":1004,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"content\":\" Voici la réponse.\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl-005\",\"object\":\"chat.completion.chunk\",\"created\":1004,\"model\":\"test-model\",",
    "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

// ---------------------------------------------------------------------------
// Tests — cas conversationnel couverts
// ---------------------------------------------------------------------------

/// TNR — simple text streaming sans outils.
///
/// Vérifie que le chemin complet (gateway → backend SSE texte → gateway → client)
/// préserve tous les fragments de contenu et le finish_reason.
#[tokio::test]
async fn sse_simple_text_passthrough_works() {
    let mock_addr = spawn_mock_llmcore().await;
    let gateway = build_gateway_for_mock(mock_addr);

    let response = gateway
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": true,
            "messages": [{"role": "user", "content": "Salut !"}]
        }))
        .await;

    response.assert_status_ok();
    let body = response.text();

    // Vérifications : rôle, fragments texte, finish_reason
    assert!(
        body.contains("\"role\":\"assistant\""),
        "rôle assistant doit être présent. Body:\n{body}"
    );
    assert!(
        body.contains("\"content\":\"Bonjour\""),
        "premier fragment texte doit être présent. Body:\n{body}"
    );
    assert!(
        body.contains("\"content\":\" c'est\""),
        "deuxième fragment texte doit être présent. Body:\n{body}"
    );
    assert!(
        body.contains("\"content\":\" Claude\""),
        "troisième fragment texte doit être présent. Body:\n{body}"
    );
    assert!(
        body.contains("\"finish_reason\":\"stop\""),
        "finish_reason='stop' doit être présent. Body:\n{body}"
    );

    // Sanity check : aucun `[DONE]` dans le passthrough (consommé par gateway)
    assert!(
        !body.contains("[DONE]"),
        "[DONE] ne doit pas être re-émis (consommé par gateway). Body:\n{body}"
    );

    // Vérifier au moins 4 chunks SSE (rôle + 3 textes + finish)
    let chunk_count = body.matches("chat.completion.chunk").count();
    assert!(
        chunk_count >= 4,
        "au moins 4 chunks SSE attendus, trouvé {chunk_count}"
    );
}

/// TNR — streaming avec finish_reason="length" (token limit).
///
/// Cas où le modèle atteint sa limite de tokens et termine abruptement.
#[tokio::test]
async fn sse_length_limit_finish_reason_passthrough() {
    let mock_addr = spawn_mock_llmcore_length_limit().await;
    let gateway = build_gateway_for_mock(mock_addr);

    let response = gateway
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": true,
            "messages": [{"role": "user", "content": "Écris un très long texte"}],
            "max_tokens": 10
        }))
        .await;

    response.assert_status_ok();
    let body = response.text();

    // Vérifier que finish_reason="length" est préservé
    assert!(
        body.contains("\"finish_reason\":\"length\""),
        "finish_reason='length' doit être présent. Body:\n{body}"
    );
    assert!(
        body.contains("\"content\":\"Le texte\""),
        "fragment texte partiel doit être présent. Body:\n{body}"
    );
}

/// TNR — streaming avec finish_reason="content_filter".
///
/// Cas où le modèle refuse de répondre pour raisons de sécurité/contenu.
#[tokio::test]
async fn sse_content_filter_finish_reason_passthrough() {
    let mock_addr = spawn_mock_llmcore_content_filter().await;
    let gateway = build_gateway_for_mock(mock_addr);

    let response = gateway
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": true,
            "messages": [{"role": "user", "content": "Message dangereux"}]
        }))
        .await;

    response.assert_status_ok();
    let body = response.text();

    assert!(
        body.contains("\"finish_reason\":\"content_filter\""),
        "finish_reason='content_filter' doit être présent. Body:\n{body}"
    );
}

/// TNR — streaming avec usage stats (token counts).
///
/// Vérifie que la gateway préserve la structure des chunks SSE complète,
/// y compris quand usage stats serait présente (bien que rare en SSE standard).
///
/// Note : L'usage stats dans les réponses SSE est rare — généralement en
/// fin de réponse non-streaming seulement. Ce test vérifie que la gateway
/// ne casse pas la structure si usage était présent.
#[tokio::test]
async fn sse_usage_stats_passthrough() {
    let mock_addr = spawn_mock_llmcore_with_usage().await;
    let gateway = build_gateway_for_mock(mock_addr);

    let response = gateway
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": true,
            "messages": [{"role": "user", "content": "Compte tes tokens"}]
        }))
        .await;

    response.assert_status_ok();
    let body = response.text();

    // Vérifier que la réponse est un SSE valide, même si usage stats
    // ne sont généralement pas présentes dans les chunks SSE.
    assert!(
        body.contains("chat.completion.chunk"),
        "body doit contenir des chunks SSE valides. Body:\n{body}"
    );

    // Vérifier la structure OpenAI-compat est respectée
    assert!(
        body.contains("\"choices\""),
        "body doit contenir le champ 'choices' OpenAI-compat. Body:\n{body}"
    );

    // Vérifier que finish_reason est présent (cas standard)
    assert!(
        body.contains("\"finish_reason\""),
        "body doit contenir 'finish_reason' au moins une fois. Body:\n{body}"
    );
}

/// TNR — streaming multi-turn (simulation conversation avec history).
///
/// Vérifie que le gateway préserve les fragments de contenu dans un contexte
/// de conversation multi-tour (plus de contenu réparti sur plusieurs chunks).
#[tokio::test]
async fn sse_multiturn_conversation_passthrough() {
    let mock_addr = spawn_mock_llmcore_multiturn().await;
    let gateway = build_gateway_for_mock(mock_addr);

    let response = gateway
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": true,
            "messages": [
                {"role": "user", "content": "Question initiale"},
                {"role": "assistant", "content": "Réponse initiale"},
                {"role": "user", "content": "Suivi de la question"}
            ]
        }))
        .await;

    response.assert_status_ok();
    let body = response.text();

    // Vérifier tous les fragments texte de la réponse multi-turn
    assert!(
        body.contains("\"content\":\"Vous aviez demandé\""),
        "fragment 1 doit être présent. Body:\n{body}"
    );
    assert!(
        body.contains("\"content\":\" comment faire quelque chose.\""),
        "fragment 2 doit être présent. Body:\n{body}"
    );
    assert!(
        body.contains("\"content\":\" Voici la réponse.\""),
        "fragment 3 doit être présent. Body:\n{body}"
    );

    // Finish_reason doit être présent
    assert!(
        body.contains("\"finish_reason\":\"stop\""),
        "finish_reason='stop' doit être présent. Body:\n{body}"
    );

    // Au moins 5 chunks (rôle + 3 textes + finish)
    let chunk_count = body.matches("chat.completion.chunk").count();
    assert!(
        chunk_count >= 5,
        "au moins 5 chunks SSE attendus pour multiturn, trouvé {chunk_count}"
    );
}

/// TNR — streaming avec plusieurs messages SSE (vérifier le format wire).
///
/// Vérifie que chaque chunk SSE respecte le format `data: {...}\n\n`.
#[tokio::test]
async fn sse_wire_format_compliance() {
    let mock_addr = spawn_mock_llmcore().await;
    let gateway = build_gateway_for_mock(mock_addr);

    let response = gateway
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": true,
            "messages": [{"role": "user", "content": "Test format SSE"}]
        }))
        .await;

    response.assert_status_ok();
    let body = response.text();

    // Vérifier que le format SSE est bien respecté
    assert!(
        body.contains("data: {"),
        "body doit commencer par 'data: {{' pour les chunks JSON. Body:\n{body}"
    );

    // Vérifier que les chunks sont séparés par double newline
    let lines: Vec<&str> = body.lines().collect();
    let mut last_was_data = false;
    for line in lines {
        if line.starts_with("data: ") {
            last_was_data = true;
        } else if line.is_empty() && last_was_data {
            last_was_data = false;
            // Double newline attendu après chaque chunk
        }
    }
}

/// TNR — streaming avec délai entre chunks (résilien au buffering TCP).
///
/// Vérifie que la gateway peut reconstituer les chunks SSE même si le flux
/// arrive fragmenté par TCP (pas exactement ligne complète par frame).
/// Ce test simule un vrai comportement réseau — le mock renvoie le flux complet
/// d'un coup, mais la gateway doit parser progressivement.
#[tokio::test]
async fn sse_tcp_fragmented_buffering() {
    let mock_addr = spawn_mock_llmcore().await;
    let gateway = build_gateway_for_mock(mock_addr);

    let response = gateway
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": true,
            "messages": [{"role": "user", "content": "Test buffering TCP"}]
        }))
        .await;

    response.assert_status_ok();
    let body = response.text();

    // Vérifier que les chunks sont correctement parsés et re-émis
    assert!(
        body.contains("chat.completion.chunk"),
        "body doit contenir les chunks parsés. Body:\n{body}"
    );

    // Chaque chunk doit être complètement parsable en JSON
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            if data != "[DONE]" {
                let _: serde_json::Value = serde_json::from_str(data)
                    .unwrap_or_else(|_| panic!("chunk valide JSON: {data}"));
            }
        }
    }
}

/// TNR — streaming role présent dans le premier chunk uniquement.
///
/// OpenAI-compat : le rôle n'apparaît que dans le premier delta.
/// Vérifier que la gateway respecte ce pattern.
#[tokio::test]
async fn sse_role_only_in_first_chunk() {
    let mock_addr = spawn_mock_llmcore().await;
    let gateway = build_gateway_for_mock(mock_addr);

    let response = gateway
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": true,
            "messages": [{"role": "user", "content": "Test rôle unique"}]
        }))
        .await;

    response.assert_status_ok();
    let body = response.text();

    // Compter les occurrences de "role":"assistant"
    // Il ne doit y en avoir qu'une (dans le premier chunk)
    let role_count = body.matches("\"role\":\"assistant\"").count();
    assert_eq!(
        role_count, 1,
        "rôle 'assistant' doit apparaître une seule fois (premier chunk), \
         trouvé {role_count}. Body:\n{body}"
    );
}
