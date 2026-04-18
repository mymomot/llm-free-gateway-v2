//! Middleware d'authentification Bearer inbound (FINDING-C1 SecurityAuditor).
//!
//! Protège tous les endpoints sauf `/health` (doit rester public pour le monitoring).
//!
//! Comportement :
//! - Si `bearer_token` est `None` dans l'`AppState` → pas d'auth, compat mode local/test.
//! - Si `bearer_token` est `Some(token)` → exige `Authorization: Bearer <token>` sur tous
//!   les endpoints sauf `/health`.
//! - `/health` reste TOUJOURS public, quel que soit la configuration.
//!
//! Retour sur échec : 401 Unauthorized (body vide — ne pas exposer d'info sur le token).

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use subtle::ConstantTimeEq;

use crate::AppState;

/// Middleware Axum vérifiant le Bearer token inbound.
///
/// Signature sans générique de body — Axum 0.8 exige `Request<Body>` pour `Next::run()`.
///
/// # Effets de bord
/// - Lit le header `Authorization` de la requête — ne modifie pas la requête.
/// - Ne logue jamais le token (ni attendu, ni fourni).
pub async fn bearer_auth(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // /health est toujours public — bypass auth.
    if req.uri().path() == "/health" {
        return Ok(next.run(req).await);
    }

    // Si aucun token configuré → mode ouvert (local / test).
    let expected = match &state.bearer_token {
        Some(t) => t.clone(),
        None => return Ok(next.run(req).await),
    };

    // Extraction du token depuis le header Authorization.
    let provided = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_owned());

    match provided {
        // Comparaison en temps constant — protège contre les timing oracles (FINDING-CT-1).
        Some(token) if bool::from(token.as_bytes().ct_eq(expected.as_bytes())) => {
            Ok(next.run(req).await)
        }
        // Token absent ou invalide → 401. Ne jamais loguer le token fourni.
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use axum_test::TestServer;
    use std::sync::Arc;

    /// Construit un router minimal pour tester le middleware auth.
    fn auth_test_server(token: Option<&str>) -> TestServer {
        use crate::config::{Config, LoggingConfig, ServerConfig};
        use crate::AppState;
        use axum::middleware;
        use std::collections::HashMap;

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
            providers: HashMap::new(),
            aliases: HashMap::new(),
        };

        let mut state = AppState::for_test(config);
        state.bearer_token = token.map(|t| Arc::new(t.to_string()));

        let router = Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/v1/models", get(|| async { "models" }))
            .layer(middleware::from_fn_with_state(state.clone(), bearer_auth))
            .with_state(state);

        TestServer::new(router)
    }

    #[tokio::test]
    async fn test_health_always_public_no_auth_configured() {
        let server = auth_test_server(None);
        server.get("/health").await.assert_status_ok();
    }

    #[tokio::test]
    async fn test_health_always_public_with_auth_configured() {
        let server = auth_test_server(Some("secret123"));
        // /health doit passer sans token
        server.get("/health").await.assert_status_ok();
    }

    #[tokio::test]
    async fn test_no_auth_configured_allows_all() {
        let server = auth_test_server(None);
        server.get("/v1/models").await.assert_status_ok();
    }

    #[tokio::test]
    async fn test_correct_token_allows() {
        let server = auth_test_server(Some("secret123"));
        server
            .get("/v1/models")
            .add_header("Authorization", "Bearer secret123")
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn test_wrong_token_returns_401() {
        let server = auth_test_server(Some("secret123"));
        let resp = server
            .get("/v1/models")
            .add_header("Authorization", "Bearer wrongtoken")
            .await;
        resp.assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_missing_token_returns_401() {
        let server = auth_test_server(Some("secret123"));
        let resp = server.get("/v1/models").await;
        resp.assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_malformed_auth_header_returns_401() {
        let server = auth_test_server(Some("secret123"));
        // Header présent mais pas au format "Bearer ..."
        let resp = server
            .get("/v1/models")
            .add_header("Authorization", "Basic secret123")
            .await;
        resp.assert_status(StatusCode::UNAUTHORIZED);
    }

    /// Vérifie que ct_eq gère correctement les tokens de longueur différente (CT-1).
    /// Un token plus court qui serait un préfixe du token attendu doit quand même être rejeté.
    #[tokio::test]
    async fn test_token_different_length_returns_401() {
        let server = auth_test_server(Some("secret123"));
        // Token trop court — même début mais longueur différente.
        let resp = server
            .get("/v1/models")
            .add_header("Authorization", "Bearer secret")
            .await;
        resp.assert_status(StatusCode::UNAUTHORIZED);

        // Token trop long — préfixe correct mais suffixe en trop.
        let server2 = auth_test_server(Some("secret123"));
        let resp2 = server2
            .get("/v1/models")
            .add_header("Authorization", "Bearer secret123extra")
            .await;
        resp2.assert_status(StatusCode::UNAUTHORIZED);
    }
}
