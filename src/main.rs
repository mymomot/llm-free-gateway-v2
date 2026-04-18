//! Gateway LLM v2 — binaire principal.
//!
//! Démarre un serveur Axum sur le port configuré (default: 127.0.0.1:8435).
//! Routes :
//! - `GET /health`               → status + providers
//! - `GET /metrics`              → export Prometheus text format
//! - `GET /v1/models`            → liste des aliases configurés
//! - `POST /v1/chat/completions` → proxy vers backend OpenAI-compat
//! - `POST /v1/embeddings`       → proxy embedding vers backend OpenAI-compat
//!
//! Configuration :
//! - `--config PATH` (argument CLI)
//! - `CONFIG_PATH=/chemin/config.toml` (variable d'environnement)
//! - `./config.toml` (défaut local)
//!
//! Conformité ADR-019 (Monarch) : service parallèle v2 sur port 8435,
//! sans impact sur gateway v1 port 8430.

use std::path::Path;
use std::time::Duration;

use llm_free_gateway_v2::config::Config;
use llm_free_gateway_v2::{build_router, AppState};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Résolution du chemin de config : --config > CONFIG_PATH > ./config.toml
    let config_path = resolve_config_path();

    // Chargement et validation config — fatal si absente ou malformée.
    let config = Config::load(&config_path).unwrap_or_else(|e| {
        eprintln!("ERREUR configuration : {}", e);
        std::process::exit(1);
    });

    // Init tracing avec niveau depuis config (env RUST_LOG prime si présent).
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.logging.level));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();

    let listen_addr = config.server.listen.clone();

    // Chemin du registre SQLite : depuis config ou défaut relatif à la config.
    let registry_path = config
        .server
        .registry_db
        .as_deref()
        .map(|p| Path::new(p).to_owned());

    let state = AppState::new(config, registry_path.as_deref());

    // Background task : purge TTL des statuts providers périmés (toutes les 5 min).
    // Les statuts rate_limited/down non mis à jour depuis 1h sont supprimés.
    {
        let registry = state.registry.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                match registry
                    .purge_stale_statuses(Duration::from_secs(3600))
                    .await
                {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(purged = n, "statuts providers périmés purgés"),
                    Err(e) => tracing::warn!(error = %e, "échec purge statuts providers"),
                }
            }
        });
    }

    let router = build_router(state);

    // Bind et démarrage.
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    tracing::info!(
        addr = %listen_addr,
        version = env!("CARGO_PKG_VERSION"),
        "gateway v2 listening"
    );

    axum::serve(listener, router).await?;
    Ok(())
}

/// Résout le chemin de config depuis les sources disponibles (dans l'ordre de priorité).
///
/// Priorité : `--config <path>` > `CONFIG_PATH` env > `./config.toml`.
fn resolve_config_path() -> String {
    // 1. Argument CLI --config <path>
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--config") {
        if let Some(path) = args.get(pos + 1) {
            return path.clone();
        }
    }

    // 2. Variable d'environnement CONFIG_PATH
    if let Ok(path) = std::env::var("CONFIG_PATH") {
        return path;
    }

    // 3. Défaut local
    "./config.toml".to_string()
}
