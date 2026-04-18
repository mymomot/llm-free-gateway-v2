//! Crate lib — expose les types publics pour les tests d'intégration.
//!
//! Le binaire (`main.rs`) et les tests (`tests/`) importent depuis ici.
//! Tout ce qui est public ici est testable sans spawner le binaire complet.

pub mod config;
pub mod error;
pub mod handlers;
pub mod metrics;
pub mod provider_pool;
pub mod providers;
pub mod registry;

use config::Config;
use metrics::Metrics;
use provider_pool::ProviderPool;
use registry::Registry;
use std::path::Path;
use std::sync::Arc;

/// État partagé entre handlers (injecté par Axum via `State<AppState>`).
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
}

impl AppState {
    /// Construit l'état depuis une config — version production.
    ///
    /// `registry_path` : chemin du fichier SQLite. Si `None`, utilise `"./registry.db"`.
    pub fn new(config: Config, registry_path: Option<&Path>) -> Self {
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

        Self {
            config: Arc::new(config),
            providers,
            registry,
            metrics,
        }
    }

    /// Construit l'état de test — registry en mémoire, pas de fichier.
    ///
    /// Utilisé dans `tests/smoke.rs` pour les tests d'intégration sans I/O disque.
    pub fn for_test(config: Config) -> Self {
        let providers = ProviderPool::from_config(&config);
        let providers_count = providers.len();
        let registry = Registry::new(Path::new(":memory:"))
            .expect("Registry en mémoire pour tests impossible");
        let metrics = Metrics::new(providers_count);
        Self {
            config: Arc::new(config),
            providers,
            registry,
            metrics,
        }
    }
}

/// Construit le router Axum — partagé entre main.rs et les tests.
///
/// Exposé ici pour que `tests/smoke.rs` puisse construire un `TestServer`
/// sans spawner un vrai TcpListener.
pub fn build_router(state: AppState) -> axum::Router {
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
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
