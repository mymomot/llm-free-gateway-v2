//! TNR E2E — SSE tool_calls passthrough
//!
//! Protège le fix llm-commons alpha.5 : `ChunkDelta.tool_calls: Option<Vec<ChunkToolCall>>`
//! contre toute régression future (retrait du champ, modification de la sérialisation, etc.).
//!
//! Architecture du test :
//! 1. Mock llmcore : serveur axum sur port aléatoire, retourne un SSE canned avec
//!    des chunks tool_calls progressifs (id + name + arguments fragmentés).
//! 2. Gateway v2 : `TestServer` axum-test avec config pointant sur le mock.
//! 3. Assertions : le body SSE passthrough contient bien tous les deltas tool_calls.
//!
//! Avant ce test, le fix alpha.3→alpha.5 n'était couvert que par des tests unitaires
//! serde dans llm-commons/tests/streaming_serde.rs — aucun test E2E ne couvrait
//! le chemin complet llmcore → gateway → client.

use axum::{extract::Json as AxumJson, response::Response, routing::post, Router};
use axum_test::TestServer;
use std::net::SocketAddr;
use tokio::net::TcpListener;

use llm_free_gateway_v2::{build_router, AppState};

// ---------------------------------------------------------------------------
// Helpers — config + test server
// ---------------------------------------------------------------------------

/// Construit une config gateway pointant sur le mock llmcore.
///
/// Le timeout est volontairement court (5s) — le mock répond immédiatement.
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
        AliasTarget::simple("mock-llmcore", "test-model-real"),
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

/// Construit un `TestServer` axum-test gateway pointant sur le mock llmcore.
fn build_gateway_for_mock(mock_addr: SocketAddr) -> TestServer {
    let state = AppState::for_test(test_config_for_mock(mock_addr));
    let router = build_router(state);
    TestServer::new(router)
}

// ---------------------------------------------------------------------------
// Mock llmcore — SSE canned avec tool_calls progressifs
// ---------------------------------------------------------------------------

/// SSE canned représentant un appel outil `get_weather(city="Paris")` streamé
/// progressivement selon le format OpenAI/llama.cpp :
///
/// - Chunk 1 : rôle assistant (delta initial)
/// - Chunk 2 : premier delta tool_call (index=0, id, type, function.name + debut arguments)
/// - Chunk 3 : delta arguments fragment ("\"city\":\"Paris\"}")
/// - Chunk 4 : chunk final (delta vide, finish_reason="tool_calls")
/// - [DONE]
///
/// Cette séquence couvre le chemin complet du fix alpha.5 :
/// `ChunkDelta.tool_calls` doit survivre au round-trip parse → sérialise dans la gateway.
const MOCK_SSE_TOOL_CALLS: &str = concat!(
    // Chunk 1 : delta rôle
    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    // Chunk 2 : premier delta tool_call (id + type + function.name + debut arguments)
    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"tc-123\",",
    "\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\"}}]}}]}\n\n",
    // Chunk 3 : fragment arguments
    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test\",",
    "\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,",
    "\"function\":{\"arguments\":\"\\\"city\\\":\\\"Paris\\\"}\"}}]}}]}\n\n",
    // Chunk 4 : fin (finish_reason)
    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test\",",
    "\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    // Terminateur SSE
    "data: [DONE]\n\n",
);

/// Handler du mock llmcore : retourne le SSE canned sans interroger de vrai modèle.
///
/// Accepte n'importe quelle requête POST (corps ignoré) et retourne le flux SSE.
async fn mock_llmcore_handler(AxumJson(_req): AxumJson<serde_json::Value>) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from(MOCK_SSE_TOOL_CALLS))
        .expect("construction réponse mock SSE — paramètres statiques valides")
}

/// Démarre le mock llmcore sur un port aléatoire et retourne son `SocketAddr`.
///
/// Le serveur tourne dans un task tokio détaché — il s'arrête quand le test termine.
async fn spawn_mock_llmcore() -> SocketAddr {
    let app = Router::new().route("/v1/chat/completions", post(mock_llmcore_handler));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind port aléatoire mock llmcore");

    let addr = listener.local_addr().expect("adresse locale mock llmcore");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock llmcore serve");
    });

    // Bind sync + local_addr() = socket prêt. axum::serve est scheduled avant
    // la première requête TestServer dans le runtime tokio single-threaded test.
    // Pas besoin de sleep (anti-pattern flaky).

    addr
}

// ---------------------------------------------------------------------------
// Test principal
// ---------------------------------------------------------------------------

/// TNR E2E — tool_calls passthrough complet gateway v2.
///
/// Vérifie que le chemin :
///   client → gateway v2 → mock llmcore (SSE tool_calls) → gateway v2 → client
///
/// préserve tous les champs `tool_calls` dans le stream SSE retourné.
///
/// Régression ciblée : avant llm-commons alpha.5, `ChunkDelta` ne contenait pas
/// `tool_calls` → les deltas tool_calls étaient droppés silencieusement lors
/// du round-trip parse (`sse_bytes_to_chunks`) → sérialise (`sse_stream_from_chunks`).
#[tokio::test]
async fn sse_tool_calls_passthrough_works() {
    // Étape 1 : démarrer le mock llmcore.
    let mock_addr = spawn_mock_llmcore().await;

    // Étape 2 : construire la gateway pointant sur le mock.
    let gateway = build_gateway_for_mock(mock_addr);

    // Étape 3 : envoyer une requête streaming avec tools.
    //
    // Le body SSE est lu intégralement via `.text()` — fonctionne parce que le mock
    // ferme le flux après `[DONE]` (body statique → connexion fermée après envoi).
    let response = gateway
        .post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "test-model",
            "stream": true,
            "messages": [{"role": "user", "content": "Quel temps fait-il à Paris ?"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Retourne la météo pour une ville",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "city": {"type": "string", "description": "Nom de la ville"}
                        },
                        "required": ["city"]
                    }
                }
            }]
        }))
        .await;

    response.assert_status_ok();

    let body = response.text();

    // --- Assertions anti-régression llm-commons alpha.5 ---

    // Le champ tool_calls doit traverser le round-trip parse/sérialise de la gateway.
    // Avant alpha.5 : ChunkDelta sans tool_calls → drop silencieux lors de la sérialisation.
    assert!(
        body.contains("\"tool_calls\""),
        "REGRESSION alpha.5 : body doit contenir les deltas 'tool_calls' — \
         vérifier ChunkDelta.tool_calls dans llm-commons. Body:\n{body}"
    );

    // Nom de la fonction sur le premier delta tool_call.
    assert!(
        body.contains("\"name\":\"get_weather\""),
        "nom de fonction 'get_weather' doit passer en passthrough. Body:\n{body}"
    );

    // Identifiant du tool call.
    assert!(
        body.contains("\"id\":\"tc-123\""),
        "id du tool call 'tc-123' doit passer en passthrough. Body:\n{body}"
    );

    // Type du tool call.
    assert!(
        body.contains("\"type\":\"function\""),
        "type 'function' du tool call doit passer en passthrough. Body:\n{body}"
    );

    // Le champ arguments doit être présent dans les deltas tool_calls.
    assert!(
        body.contains("\"arguments\""),
        "body must contain 'arguments' field from tool_calls deltas. Body:\n{body}"
    );

    // Fragment d'arguments progressif contenant la ville.
    assert!(
        body.contains("Paris"),
        "fragment arguments 'Paris' doit passer en passthrough. Body:\n{body}"
    );

    // Finish reason du chunk final.
    assert!(
        body.contains("\"finish_reason\":\"tool_calls\""),
        "finish_reason 'tool_calls' doit passer en passthrough. Body:\n{body}"
    );

    // Sanity check : le body doit être du SSE (lignes `data: ...`).
    assert!(
        body.contains("data: "),
        "body doit contenir des lignes SSE 'data: ...'. Body:\n{body}"
    );

    // Note : la gateway consomme `data: [DONE]` dans sse_bytes_to_chunks() pour terminer
    // le stream proprement (retour None dans le stream unfold). Le [DONE] n'est donc PAS
    // re-émis dans le flux passthrough — comportement correct par design OpenAI-compat.
    // Le terminateur effectif côté client est la fermeture de la connexion HTTP.
    //
    // Vérification de complétude : au moins 4 chunks SSE (role + 2 tool_calls + finish).
    let chunk_count = body.matches("chat.completion.chunk").count();
    assert!(
        chunk_count >= 4,
        "au moins 4 chunks SSE attendus (role + 2 tool_calls + finish_reason), \
         trouvé {chunk_count}. Body:\n{body}"
    );
}
