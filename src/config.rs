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
}

impl Config {
    /// Charge et parse la config depuis le fichier au chemin donné.
    ///
    /// Retourne `Err` avec message précis si le fichier est absent ou malformé.
    /// Le binaire doit traiter cette erreur comme fatale (exit non-zero).
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!("impossible de lire le fichier config '{}': {}", path, e)
        })?;
        let config: Config = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("config TOML invalide dans '{}': {}", path, e))?;
        config.validate()?;
        Ok(config)
    }

    /// Valide la cohérence de la config après parsing.
    ///
    /// Vérifie que chaque alias référence un provider déclaré dans `[providers]`.
    fn validate(&self) -> anyhow::Result<()> {
        for (alias, target) in &self.aliases {
            if !self.providers.contains_key(&target.provider) {
                anyhow::bail!(
                    "alias '{}' référence le provider '{}' qui n'est pas déclaré dans [providers]",
                    alias,
                    target.provider
                );
            }
        }
        Ok(())
    }

    /// Retourne les noms des providers configurés (pour le health endpoint).
    pub fn provider_names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}
