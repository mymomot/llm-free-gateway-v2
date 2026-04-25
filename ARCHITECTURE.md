# Architecture — llm-free-gateway-v2

> Généré le : 2026-04-25
> Commit ref : 4aabb8e

Gateway HTTP OpenAI-compatible v2. Proxy unifié avec pool de providers, circuit breaker per-provider, fallback déclaratif par alias TOML, auth Bearer inbound, rate limiting par IP, journalisation SQLite et export Prometheus.

---

## Arbre fonctionnel

```
[CORE] AppState — état partagé injecté par Axum via State<AppState>
  ├── [CORE] Config — config TOML racine chargée au startup
  │   ├── [SUB] ServerConfig — adresse, auth, rate limit, circuit breaker params
  │   ├── [SUB] LoggingConfig — niveau tracing
  │   ├── [SUB] ProviderConfig — endpoint, timeout, api_key_env par provider
  │   └── [SUB] AliasTarget — provider + model + fallback_provider + fallback_model
  ├── [CORE] ProviderPool — pool de providers construits au startup
  │   ├── [SUB] OpenAiCompatProvider — provider HTTP OpenAI-compat générique
  │   │   └── utilise LlmProvider (trait llm-commons)
  │   ├── [SUB] CircuitBreakerRegistry — CB per-provider (llm-commons)
  │   └── [SUB] resolved_api_keys — clés API pré-résolues depuis env vars au startup
  ├── [CORE] Registry — journalisation SQLite
  │   └── persiste dans registry.db (ou :memory: en tests)
  ├── [CORE] Metrics — export Prometheus
  └── [CORE] RateLimiter — rate limit par IP (maison, stdlib)

[CORE] build_router — construction du router Axum
  ├── [FEATURE] /health GET → health::handler
  │   └── [UTILITY] probe LlmProvider.health_check() par provider
  ├── [FEATURE] /metrics GET → metrics_handler::handler
  │   └── dépend de Metrics
  ├── [FEATURE] /v1/models GET → models::handler
  │   └── retourne liste aliases configurés
  ├── [FEATURE] /v1/chat/completions POST → chat::handler
  │   ├── [SUB] dispatch_with_fallback — résolution alias + try primary + try fallback
  │   │   ├── [SUB] try_provider — CB check + pool.get() + stream ou complete
  │   │   │   ├── vérifie CircuitBreakerRegistry.should_allow(provider)
  │   │   │   ├── → ProviderUnavailable (ApiError::Backend) si CB ouvert
  │   │   │   ├── appelle LlmProvider.stream() ou LlmProvider.complete()
  │   │   │   └── record_failure/record_success sur le CB primary
  │   │   └── [UTILITY] is_backend_error — détecte ApiError::Backend pour trigger fallback
  │   ├── [HOOK] rate_limit par IP (RateLimiter) — avant dispatch
  │   └── [HOOK] registry.log_request() — journalise provider effectif + latence
  ├── [FEATURE] /v1/embeddings POST → embeddings::handler
  │   └── pass-through HTTP direct via ProviderPool.http_client()
  ├── [UTILITY] auth::bearer_auth — middleware Bearer inbound (FINDING-C1)
  │   └── bypass automatique pour /health
  ├── [UTILITY] DefaultBodyLimit 4MB — protection payload excessif
  ├── [UTILITY] TraceLayer — tracing HTTP via tower-http
  └── [UTILITY] CorsLayer — CORS permissif (LAN only)
```

---

## Table des relations

| De | Type | Vers |
|---|---|---|
| chat::handler | utilise | dispatch_with_fallback |
| dispatch_with_fallback | utilise | try_provider × 2 (primary + fallback) |
| try_provider | vérifie | CircuitBreakerRegistry |
| try_provider | utilise | ProviderPool.get(provider_name) |
| try_provider | appelle | LlmProvider.stream() / complete() |
| ProviderPool | construit | OpenAiCompatProvider × n |
| ProviderPool | contient | CircuitBreakerRegistry |
| AppState | contient | ProviderPool, Registry, Metrics, RateLimiter |
| build_router | injecte | AppState via State<> |
| Registry | persiste dans | SQLite registry.db |
| auth::bearer_auth | dépend de | AppState.bearer_token |
| embeddings::handler | utilise | ProviderPool.http_client() (reqwest direct) |
| chat::handler | utilise | RateLimiter (par IP) |
| chat::handler | persiste dans | Registry (log_request) |
| Metrics | utilisé par | /metrics handler |
| AliasTarget | consommé par | dispatch_with_fallback (résolution alias) |

---

## Fichiers critiques par fonctionnalité

| Fonctionnalité | Fichiers |
|---|---|
| Routing principal | `src/lib.rs` (build_router), `src/main.rs` |
| Config + validation | `src/config.rs` |
| Provider pool + CB | `src/provider_pool.rs` |
| Provider HTTP | `src/providers/openai_compat.rs` |
| Chat + fallback | `src/handlers/chat.rs` |
| Embeddings proxy | `src/handlers/embeddings.rs` |
| Auth Bearer | `src/auth.rs` |
| Rate limiting | `src/rate_limit.rs` |
| Registry SQLite | `src/registry.rs` |
| Métriques Prometheus | `src/metrics.rs`, `src/handlers/metrics_handler.rs` |
| Health check | `src/handlers/health.rs` |
| Liste modèles | `src/handlers/models.rs` |
| Erreurs | `src/error.rs` |
| Tests intégration | `tests/smoke.rs`, `tests/fallback_openrouter.rs`, `tests/sse_tool_calls_passthrough.rs`, `tests/sse_conversational_passthrough.rs` |

---

## Services externes

### Consomme

| Service | URL/Port | Variable env | Usage |
|---|---|---|---|
| llmcore (primary) | `http://192.168.10.118:8080` | — (local LAN) | Chat completions LLM lourds (Qwen3.6-35B-A3B) |
| llmcore-embed (primary) | `http://192.168.10.118:8432` | — (local LAN) | Embeddings bge-m3 GPU |
| OpenRouter (fallback) | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` | Fallback cloud si llmcore down |

### Fournit

| Endpoint | Port | Consommé par | Usage |
|---|---|---|---|
| `/v1/chat/completions` | `:8435` | vault-mem (flush), nexus, vox-avatar, hubmq | Chat completions OpenAI-compat |
| `/v1/embeddings` | `:8435` | nexus, llm-embedding | Embeddings vectoriels |
| `/v1/models` | `:8435` | Claude Code, diagnostic | Liste des modèles configurés |
| `/health` | `:8435` | monitoring, CI | Health check par provider |
| `/metrics` | `:8435` | Prometheus/Grafana | Export métriques |
