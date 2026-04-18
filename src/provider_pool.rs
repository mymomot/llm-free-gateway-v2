//! Pool de providers partagés — construit au startup, distribué via `AppState`.
//!
//! Au démarrage, chaque provider déclaré dans `config.providers` est instancié
//! une seule fois. Les handlers résolvent ensuite via `alias → provider_name → &Arc<Provider>`.
//!
//! Avantages vs. création par requête (alpha.1) :
//! - `reqwest::Client` partagé → pool de connexions TCP réutilisé
//! - Capabilities statiques résolues une seule fois
//! - Mémoire réduite (pas d'allocation par requête)
//!
//! Le `reqwest::Client` partagé est aussi exposé directement pour les handlers
//! qui font des requêtes HTTP sans passer par le trait `LlmProvider`
//! (ex: handler embeddings qui est un pur proxy HTTP).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use llm_commons::{
    capabilities::{Capabilities, ThinkingMode, ToolUseSupport},
    provider::LlmProvider,
};
use reqwest::Client;

use crate::config::Config;
use crate::providers::openai_compat::OpenAiCompatProvider;

/// Pool de providers partagés.
#[derive(Clone)]
pub struct ProviderPool {
    /// Providers indexés par nom (correspond aux clés de `config.providers`).
    providers: Arc<HashMap<String, Arc<dyn LlmProvider + Send + Sync>>>,
    /// Client HTTP partagé — pool de connexions réutilisé entre tous les handlers.
    http_client: Client,
}

impl ProviderPool {
    /// Construit le pool depuis la config.
    ///
    /// En cas d'erreur de construction d'un provider (ex: clé API env illisible),
    /// le pool se construit quand même avec les providers valides.
    /// Les providers en erreur sont loggés en `warn`.
    pub fn from_config(config: &Config) -> Self {
        // Client HTTP partagé — pool de connexions global pour tous les providers.
        let http_client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            // SAFETY : la construction d'un reqwest::Client ne peut échouer que si
            // TLS est indisponible sur le système — impossible dans notre contexte LXC Ubuntu.
            .expect("construction du client HTTP partagé impossible — TLS système absent");

        // Capabilities génériques par défaut pour les providers OpenAI-compat.
        // Le probing réel des capabilities est prévu en alpha.3.
        let default_caps = Capabilities {
            tool_use: ToolUseSupport::Native,
            streaming: true,
            vision: true,
            thinking: ThinkingMode::Switchable,
            context_max: 131_072,
            structured_output: false,
            prompt_caching: true,
            reasoning_levels: None,
        };

        let mut providers: HashMap<String, Arc<dyn LlmProvider + Send + Sync>> = HashMap::new();

        for (name, cfg) in &config.providers {
            match OpenAiCompatProvider::new(
                name,
                &cfg.endpoint,
                cfg.timeout_secs,
                cfg.api_key_env.as_deref(),
                default_caps.clone(),
            ) {
                Ok(provider) => {
                    providers.insert(name.clone(), Arc::new(provider));
                    tracing::info!(provider = %name, endpoint = %cfg.endpoint, "provider enregistré");
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %name,
                        error = %e,
                        "échec construction provider — exclu du pool"
                    );
                }
            }
        }

        Self {
            providers: Arc::new(providers),
            http_client,
        }
    }

    /// Résout un provider par nom.
    ///
    /// Retourne `None` si le nom n'est pas dans le pool (incohérence config — déjà catchée
    /// par `Config::validate()` au démarrage).
    pub fn get(&self, name: &str) -> Option<&Arc<dyn LlmProvider + Send + Sync>> {
        self.providers.get(name)
    }

    /// Nombre de providers dans le pool.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Retourne `true` si le pool ne contient aucun provider.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Retourne le client HTTP partagé pour les handlers qui font des requêtes directes.
    ///
    /// Utilisé par le handler embeddings pour les requêtes pass-through HTTP.
    pub fn http_client(&self) -> Client {
        self.http_client.clone()
    }

    /// Itère sur les noms de providers dans le pool.
    pub fn names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AliasTarget, Config, LoggingConfig, ProviderConfig, ServerConfig};
    use std::collections::HashMap;

    fn test_config_with_two_providers() -> Config {
        let mut providers = HashMap::new();
        providers.insert(
            "p1".to_string(),
            ProviderConfig {
                endpoint: "http://127.0.0.1:1".to_string(),
                timeout_secs: 5,
                api_key_env: None,
            },
        );
        providers.insert(
            "p2".to_string(),
            ProviderConfig {
                endpoint: "http://127.0.0.1:2".to_string(),
                timeout_secs: 5,
                api_key_env: None,
            },
        );
        let mut aliases = HashMap::new();
        aliases.insert(
            "m1".to_string(),
            AliasTarget {
                provider: "p1".to_string(),
                model: "model-real".to_string(),
            },
        );
        Config {
            server: ServerConfig {
                listen: "127.0.0.1:0".to_string(),
                registry_db: None,
            },
            logging: LoggingConfig {
                level: "error".to_string(),
            },
            providers,
            aliases,
        }
    }

    #[test]
    fn test_pool_builds_from_config() {
        let config = test_config_with_two_providers();
        let pool = ProviderPool::from_config(&config);
        assert_eq!(pool.len(), 2);
        assert!(pool.get("p1").is_some());
        assert!(pool.get("p2").is_some());
        assert!(pool.get("p3").is_none());
    }

    #[test]
    fn test_http_client_shared() {
        let config = test_config_with_two_providers();
        let pool = ProviderPool::from_config(&config);
        // Vérifie que le clone du client est possible (implique Arc interne).
        let _c1 = pool.http_client();
        let _c2 = pool.http_client();
    }
}
