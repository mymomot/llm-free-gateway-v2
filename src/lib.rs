//! Crate lib — expose les types publics pour les tests d'intégration.
//!
//! Le binaire (`main.rs`) et les tests (`tests/`) importent depuis ici.
//! Tout ce qui est public ici est testable sans spawner le binaire complet.

pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod metrics;
pub mod provider_pool;
pub mod providers;
pub mod rate_limit;
pub mod registry;
pub mod token_counter;

use config::Config;
use metrics::Metrics;
use provider_pool::ProviderPool;
use rate_limit::RateLimiter;
use registry::Registry;
use std::path::Path;
use std::sync::Arc;

/// État partagé entre handlers (injecté par Axum via `State<AppState>`).
///
/// Alpha.3 additions :
/// - `bearer_token` : token Bearer inbound pré-résolu depuis env var au startup
/// - `rate_limiter` : rate limiter par IP (maison, stdlib uniquement)
///
/// Alpha.2 :
/// - `providers` : pool de providers construits au startup (plus de création par requête)
/// - `registry`  : journalisation SQLite + statuts providers
/// - `metrics`   : export Prometheus
///
/// Exposé publiquement pour que les tests puissent construire un `AppState`
/// avec une config de test sans passer par le parsing de fichier TOML.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Pool de providers partagés — accès O(1) par nom.
    pub providers: ProviderPool,
    /// Registre SQLite — journalisation et statuts providers.
    pub registry: Registry,
    /// Métriques Prometheus — export via /metrics.
    pub metrics: Metrics,
    /// Bearer token inbound pré-résolu depuis env var au startup.
    ///
    /// `None` = pas d'authentification requise (mode local/test).
    /// `Some(token)` = tous les endpoints sauf /health exigent ce token.
    pub bearer_token: Option<Arc<String>>,
    /// Rate limiter par IP — appliqué sur les endpoints POST uniquement.
    pub rate_limiter: Arc<RateLimiter>,
}

impl AppState {
    /// Construit l'état depuis une config — version production.
    ///
    /// `registry_path` : chemin du fichier SQLite. Si `None`, utilise `"./registry.db"`.
    pub fn new(config: Config, registry_path: Option<&Path>) -> Self {
        // Résolution du bearer token depuis env var au startup.
        // Logué en info si configuré (sans révéler la valeur).
        let bearer_token = config
            .server
            .bearer_token_env
            .as_deref()
            .and_then(|env_name| match std::env::var(env_name) {
                Ok(token) if !token.is_empty() => {
                    tracing::info!(
                        env_var = %env_name,
                        "authentification Bearer inbound activée"
                    );
                    Some(Arc::new(token))
                }
                Ok(_) => {
                    tracing::warn!(
                        env_var = %env_name,
                        "variable env bearer_token_env présente mais vide — auth désactivée"
                    );
                    None
                }
                Err(_) => {
                    tracing::warn!(
                        env_var = %env_name,
                        "variable env bearer_token_env non définie — auth désactivée"
                    );
                    None
                }
            });

        let providers = ProviderPool::from_config(&config);
        let providers_count = providers.len();

        let db_path = registry_path
            .map(|p| p.to_owned())
            .unwrap_or_else(|| Path::new("./registry.db").to_owned());

        let registry = Registry::new(&db_path).unwrap_or_else(|e| {
            tracing::warn!(
                path = %db_path.display(),
                error = %e,
                "impossible d'ouvrir le registre SQLite — registry désactivé, utilisation d'un db en mémoire"
            );
            // Fallback sur :memory: pour ne pas bloquer le démarrage.
            Registry::new(Path::new(":memory:"))
                .expect("Registry en mémoire impossible — environnement Rust incorrect")
        });

        let metrics = Metrics::new(providers_count);
        let rate_limiter = Arc::new(RateLimiter::new(config.server.rate_limit_per_minute));

        Self {
            config: Arc::new(config),
            providers,
            registry,
            metrics,
            bearer_token,
            rate_limiter,
        }
    }

    /// Construit l'état de test — registry en mémoire, pas d'auth.
    ///
    /// Respecte `config.server.rate_limit_per_minute` pour permettre de tester
    /// le rate limiting avec des configs dédiées. Les tests généraux passent
    /// `rate_limit_per_minute: 0` pour désactiver.
    ///
    /// Utilisé dans `tests/smoke.rs` pour les tests d'intégration sans I/O disque.
    pub fn for_test(config: Config) -> Self {
        let providers = ProviderPool::from_config(&config);
        let providers_count = providers.len();
        let registry = Registry::new(Path::new(":memory:"))
            .expect("Registry en mémoire pour tests impossible");
        let metrics = Metrics::new(providers_count);
        let rate_limiter = Arc::new(RateLimiter::new(config.server.rate_limit_per_minute));
        Self {
            config: Arc::new(config),
            providers,
            registry,
            metrics,
            bearer_token: None,
            rate_limiter,
        }
    }
}

/// Construit le router Axum — partagé entre main.rs et les tests.
///
/// Alpha.3 additions :
/// - `DefaultBodyLimit::max(4MB)` sur tous les endpoints (FINDING-M1)
/// - Middleware auth Bearer (FINDING-C1) sur tous les endpoints sauf /health
/// - Rate limit par IP sur les endpoints POST (FINDING-M1, impl maison stdlib)
///
/// Exposé ici pour que `tests/smoke.rs` puisse construire un `TestServer`
/// sans spawner un vrai TcpListener.
pub fn build_router(state: AppState) -> axum::Router {
    use axum::extract::DefaultBodyLimit;
    use axum::middleware;
    use axum::routing;
    use tower_http::{cors::CorsLayer, trace::TraceLayer};

    axum::Router::new()
        .route("/health", routing::get(handlers::health::handler))
        .route("/metrics", routing::get(handlers::metrics_handler::handler))
        .route("/v1/models", routing::get(handlers::models::handler))
        .route(
            "/v1/chat/completions",
            routing::post(handlers::chat::handler),
        )
        .route(
            "/v1/embeddings",
            routing::post(handlers::embeddings::handler),
        )
        // Body limit 4MB — protège contre les payloads excessifs (FINDING-M1).
        // Appliqué avant le middleware auth pour éviter de lire un body énorme
        // avant d'authentifier.
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
        // Auth Bearer inbound — bypass automatique pour /health.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::bearer_auth,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
