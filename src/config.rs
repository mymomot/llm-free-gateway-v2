//! Configuration du gateway v2.
//!
//! Chargée au démarrage depuis un fichier TOML (chemin via `--config PATH`
//! ou variable d'environnement `CONFIG_PATH`). Si absente ou malformée,
//! le binaire quitte avec un message d'erreur clair et code non-zéro.
//!
//! Structure :
//! - `[server]` : adresse d'écoute
//! - `[logging]` : niveau tracing
//! - `[providers.<nom>]` : endpoint HTTP + timeout par provider
//! - `[aliases]` : map model_id → { provider, model }
//!
//! Le dispatch par alias est la seule logique de routage : aucun branchement
//! conditionnel par nom de modèle dans le code (ADR-020 standards-first, Q3).

use std::collections::HashMap;

use serde::Deserialize;

/// Config racine du gateway.
#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Providers indexés par nom (ex: "llmcore").
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    /// Aliases model_id → route provider.
    #[serde(default)]
    pub aliases: HashMap<String, AliasTarget>,
}

/// Config serveur.
#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    /// Adresse d'écoute (ex: "127.0.0.1:8435").
    pub listen: String,
    /// Chemin du fichier SQLite pour le registre (défaut: "./registry.db").
    /// Peut être ":memory:" pour les tests.
    #[serde(default)]
    pub registry_db: Option<String>,
    /// Nom de la variable d'environnement contenant le Bearer token inbound.
    ///
    /// Si absent ou `None` : aucune authentification requise (mode local/test).
    /// Si présent : tous les endpoints sauf `/health` exigent `Authorization: Bearer <token>`.
    #[serde(default)]
    pub bearer_token_env: Option<String>,
    /// Limite de requêtes par minute par IP (sur les endpoints POST uniquement).
    ///
    /// Défaut : 60 req/min. `0` désactive le rate limiting.
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_minute: u32,
    /// Nombre d'échecs consécutifs avant d'ouvrir le circuit breaker par provider.
    ///
    /// Défaut : 5.
    #[serde(default = "default_circuit_threshold")]
    pub circuit_threshold: u32,
    /// Fenêtre temporelle des échecs circuit breaker (secondes).
    ///
    /// Défaut : 60s.
    #[serde(default = "default_circuit_window_secs")]
    pub circuit_window_secs: u64,
    /// Durée du cooldown circuit breaker après ouverture (secondes).
    ///
    /// Défaut : 30s.
    #[serde(default = "default_circuit_cooldown_secs")]
    pub circuit_cooldown_secs: u64,
    /// Cap hard total de tokens (input + max_tokens demandé) par requête chat.
    ///
    /// Si le total estimé dépasse ce seuil, le gateway retourne HTTP 413
    /// avec code `context_length_exceeded` AVANT d'envoyer la requête au backend.
    ///
    /// Motivation : Qwen3.6 slot ctx réel = 262K (YARN), mais bug freeze llama-server
    /// connu sur cache-bf16 + corpus >200K. Le cap 180K est imposé par le council
    /// homelab-gouvernance MAJOR 2026-04-25 (condition C4).
    ///
    /// Défaut : 180000. Peut être abaissé via `MAX_TOTAL_TOKENS` ou directement
    /// en TOML. `0` désactive le cap (non recommandé en prod avec Qwen3.6).
    #[serde(default = "default_max_total_tokens")]
    pub max_total_tokens: u64,
}

fn default_rate_limit() -> u32 {
    60
}

fn default_circuit_threshold() -> u32 {
    5
}

fn default_circuit_window_secs() -> u64 {
    60
}

fn default_circuit_cooldown_secs() -> u64 {
    30
}

/// Cap hard tokens par défaut — council homelab-gouvernance MAJOR 2026-04-25 (C4).
/// Slot ctx réel Qwen3.6 = 262K, cap 180K pour éviter freeze cache-bf16 >200K.
fn default_max_total_tokens() -> u64 {
    // Surridable via variable d'env MAX_TOTAL_TOKENS au chargement de config.
    // La lecture de l'env est gérée dans `Config::load` après parsing TOML.
    180_000
}

/// Config logging.
#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    /// Niveau tracing (ex: "info", "debug", "warn").
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

/// Config d'un provider HTTP OpenAI-compat.
#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    /// URL de base (ex: "http://192.168.10.118:8080").
    /// Le gateway append "/v1/chat/completions" pour les requêtes chat.
    pub endpoint: String,
    /// Timeout HTTP en secondes (défaut : 120).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Nom de la variable d'env contenant la clé API (optionnel).
    /// Si absent : aucun header Authorization n'est envoyé.
    pub api_key_env: Option<String>,
}

fn default_timeout_secs() -> u64 {
    120
}

/// Cible d'un alias : provider nommé + model réel à envoyer.
#[derive(Debug, Deserialize, Clone)]
pub struct AliasTarget {
    /// Nom du provider dans `[providers]`.
    pub provider: String,
    /// Identifiant de modèle réel à transmettre au backend.
    pub model: String,
    /// Provider de fallback (optionnel) — tenté si le provider primary retourne une erreur
    /// backend (réseau, timeout, 5xx) ou si son circuit breaker est ouvert.
    ///
    /// Doit référencer un provider déclaré dans `[providers]`.
    /// Si absent : aucun fallback, le handler retourne l'erreur directement.
    #[serde(default)]
    pub fallback_provider: Option<String>,
    /// Identifiant de modèle à envoyer au provider de fallback.
    ///
    /// Si absent, le même `model` que le primary est utilisé.
    #[serde(default)]
    pub fallback_model: Option<String>,
}

impl AliasTarget {
    /// Construit un alias simple sans fallback — utile dans les tests.
    pub fn simple(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            fallback_provider: None,
            fallback_model: None,
        }
    }

    /// Construit un alias avec fallback provider.
    ///
    /// `fallback_model` : si `None`, le même `model` que le primary est utilisé.
    pub fn with_fallback(
        provider: impl Into<String>,
        model: impl Into<String>,
        fallback_provider: impl Into<String>,
        fallback_model: Option<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            fallback_provider: Some(fallback_provider.into()),
            fallback_model,
        }
    }
}

impl Config {
    /// Charge et parse la config depuis le fichier au chemin donné.
    ///
    /// Retourne `Err` avec message précis si le fichier est absent ou malformé.
    /// Le binaire doit traiter cette erreur comme fatale (exit non-zero).
    ///
    /// Override possible par variable d'environnement après parsing TOML :
    /// - `MAX_TOTAL_TOKENS` : surride `server.max_total_tokens` (u64, 0 = désactivé)
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!("impossible de lire le fichier config '{}': {}", path, e)
        })?;
        let mut config: Config = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("config TOML invalide dans '{}': {}", path, e))?;

        // Override MAX_TOTAL_TOKENS depuis env var (C4 — surridable sans redéploiement config).
        if let Ok(val) = std::env::var("MAX_TOTAL_TOKENS") {
            match val.parse::<u64>() {
                Ok(n) => {
                    tracing::info!(
                        max_total_tokens = n,
                        "cap tokens surchargé via MAX_TOTAL_TOKENS"
                    );
                    config.server.max_total_tokens = n;
                }
                Err(_) => {
                    anyhow::bail!(
                        "MAX_TOTAL_TOKENS='{}' invalide — doit être un entier positif",
                        val
                    );
                }
            }
        }

        config.validate()?;
        Ok(config)
    }

    /// Valide la cohérence de la config après parsing.
    ///
    /// Vérifie que chaque alias référence un provider déclaré dans `[providers]`,
    /// et que le fallback_provider (si présent) est également déclaré.
    fn validate(&self) -> anyhow::Result<()> {
        for (alias, target) in &self.aliases {
            if !self.providers.contains_key(&target.provider) {
                anyhow::bail!(
                    "alias '{}' référence le provider '{}' qui n'est pas déclaré dans [providers]",
                    alias,
                    target.provider
                );
            }
            if let Some(fb) = &target.fallback_provider {
                if !self.providers.contains_key(fb) {
                    anyhow::bail!(
                        "alias '{}' référence le fallback_provider '{}' qui n'est pas déclaré dans [providers]",
                        alias,
                        fb
                    );
                }
            }
        }
        Ok(())
    }

    /// Retourne les noms des providers configurés (pour le health endpoint).
    pub fn provider_names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}
