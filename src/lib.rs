//! Crate lib — expose les types publics pour les tests d'intégration.
//!
//! Le binaire (`main.rs`) et les tests (`tests/`) importent depuis ici.
//! Tout ce qui est public ici est testable sans spawner le binaire complet.

pub mod config;
pub mod error;
pub mod handlers;
pub mod providers;

use config::Config;
use std::sync::Arc;

/// État partagé entre handlers (identique à celui de main.rs).
///
/// Exposé publiquement pour que les tests puissent construire un `AppState`
/// avec une config de test sans passer par le parsing de fichier TOML.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
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
        .route(
            "/v1/chat/completions",
            routing::post(handlers::chat::handler),
        )
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
