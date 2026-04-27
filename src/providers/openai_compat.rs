//! Provider HTTP OpenAI-compat — implémente `LlmProvider` pour tout endpoint
//! conforme `/v1/chat/completions` (llama.cpp, vLLM, llmcore, OpenRouter, etc.).
//!
//! Alpha.1 : `complete()` et `stream()` sont implémentés.
//! Le streaming utilise un forward byte-level du flux SSE du backend.
//!
//! Conforme ADR-020 (standards-first) et Q3 (agnosticité modèle) :
//! aucun branchement sur le nom de modèle dans cette implémentation.

use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;
use llm_commons::{
    capabilities::Capabilities,
    error::{LlmError, LlmResult},
    openai::chat::{ChatCompletionRequest, ChatCompletionResponse},
    openai::streaming::ChatCompletionChunk,
    provider::{ChatCompletionStream, LlmProvider},
};
use reqwest::Client;
use tracing::instrument;

/// Provider HTTP conforme OpenAI Chat Completions spec.
pub struct OpenAiCompatProvider {
    /// Nom du provider (pour logging et messages d'erreur).
    name: String,
    /// URL du endpoint `/v1/chat/completions` (endpoint_base + "/v1/chat/completions").
    chat_url: String,
    /// Client HTTP partagé avec timeout total configuré (pour `complete()` et `health_check()`).
    client: Client,
    /// Capabilities déclarées — configurées à la construction.
    capabilities: Capabilities,
    /// Clé API optionnelle (lue depuis variable d'env au moment de la construction).
    api_key: Option<String>,
    /// Timeout en secondes issu de la config — utilisé comme `connect_timeout` pour
    /// les requêtes streaming (pas de timeout total sur le flux).
    timeout_secs: u64,
}

impl OpenAiCompatProvider {
    /// Construit un nouveau provider OpenAI-compat.
    ///
    /// `endpoint_base` : URL de base (ex: "http://192.168.10.118:8080").
    /// Le path "/v1/chat/completions" est ajouté automatiquement.
    ///
    /// `timeout_secs` : timeout HTTP global pour les requêtes non-streaming.
    /// Pour le streaming, le timeout couvre l'établissement de connexion (premier byte).
    ///
    /// `api_key_env` : si fourni, la valeur de la variable d'env nommée est lue
    /// et utilisée comme Bearer token. Si la variable est absente, le provider
    /// fonctionne sans Authorization header (adapté pour llmcore local).
    pub fn new(
        name: impl Into<String>,
        endpoint_base: &str,
        timeout_secs: u64,
        api_key_env: Option<&str>,
        capabilities: Capabilities,
    ) -> anyhow::Result<Self> {
        let chat_url = format!(
            "{}/v1/chat/completions",
            endpoint_base.trim_end_matches('/')
        );

        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| anyhow::anyhow!("erreur construction client HTTP: {}", e))?;

        let api_key = api_key_env.and_then(|env_name| std::env::var(env_name).ok());

        Ok(Self {
            name: name.into(),
            chat_url,
            client,
            capabilities,
            api_key,
            timeout_secs,
        })
    }

    /// Ajoute le header Authorization si une clé API est configurée.
    fn add_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Complétion non-streaming.
    ///
    /// Forward la requête vers le backend, parse la réponse `ChatCompletionResponse`.
    /// Les erreurs HTTP sont converties en `LlmError` via `from_http_status`.
    #[instrument(skip(self, request), fields(provider = %self.name, model = %request.model))]
    async fn complete(&self, request: ChatCompletionRequest) -> LlmResult<ChatCompletionResponse> {
        let mut req = request;
        // Forcer stream=false pour le mode non-streaming (le champ est optionnel).
        req.stream = Some(false);

        let builder = self.client.post(&self.chat_url).json(&req);
        let builder = self.add_auth(builder);

        let response = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                LlmError::Timeout {
                    elapsed_secs: self.capabilities.context_max as f64, // valeur symbolique
                }
            } else {
                LlmError::Network {
                    source: Box::new(e),
                }
            }
        })?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::from_http_status(status, body));
        }

        let completion: ChatCompletionResponse =
            response.json().await.map_err(|e| LlmError::Serialization {
                source: serde_json::Error::custom(e.to_string()),
            })?;

        Ok(completion)
    }

    /// Complétion streaming — forward byte-level du flux SSE du backend.
    ///
    /// Chaque ligne SSE `data: {...}` est parsée en `ChatCompletionChunk`
    /// et émise dans le stream retourné. La ligne `data: [DONE]` termine le flux.
    ///
    /// Effets de bord : le stream retourné est `Send` et peut être consommé
    /// dans un contexte async multi-thread. Chaque item est un `LlmResult<ChatCompletionChunk>`.
    #[instrument(skip(self, request), fields(provider = %self.name, model = %request.model))]
    async fn stream(&self, request: ChatCompletionRequest) -> LlmResult<ChatCompletionStream> {
        let mut req = request;
        req.stream = Some(true);

        // Client dédié au streaming : uniquement connect_timeout (temps jusqu'au premier byte),
        // PAS de timeout total. Pour les gros prompts (ex: 17K tokens à 133 tok/s = ~128s de
        // prompt-processing avant le 1er byte), un timeout total couperait la requête à tort.
        //
        // Le connect_timeout utilise self.timeout_secs (config TOML du provider) au lieu de
        // la valeur 30s hardcodée qui était la cause du bug timeout sur Monarch/BB analyze.
        let client_stream = Client::builder()
            .connect_timeout(Duration::from_secs(self.timeout_secs))
            .build()
            .map_err(|e| LlmError::Network {
                source: Box::new(e),
            })?;

        let builder = client_stream.post(&self.chat_url).json(&req);
        let builder = self.add_auth(builder);

        let response = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                LlmError::Timeout {
                    elapsed_secs: self.timeout_secs as f64,
                }
            } else {
                LlmError::Network {
                    source: Box::new(e),
                }
            }
        })?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::from_http_status(status, body));
        }

        // Convertit le stream d'octets en stream de chunks parsés.
        let stream = sse_bytes_to_chunks(response);
        Ok(Box::pin(stream))
    }

    /// Health check : vérifie que le backend répond sur son endpoint chat.
    ///
    /// Envoie une requête minimale et considère que tout code HTTP non-5xx
    /// indique un backend vivant (même un 400 = le backend est joignable).
    async fn health_check(&self) -> LlmResult<()> {
        // HEAD sur le chat endpoint — certains backends ne supportent pas HEAD,
        // on utilise une requête OPTIONS qui est moins coûteuse qu'une vraie complétion.
        let result = self
            .client
            .get(format!(
                "{}/health",
                self.chat_url.trim_end_matches("/v1/chat/completions")
            ))
            .send()
            .await;

        match result {
            Ok(resp) if !resp.status().is_server_error() => Ok(()),
            Ok(resp) => Err(LlmError::UpstreamError {
                status: resp.status().as_u16(),
                message: "health check failed".to_string(),
            }),
            Err(e) => Err(LlmError::Network {
                source: Box::new(e),
            }),
        }
    }
}

/// Convertit un flux de bytes HTTP en stream de `LlmResult<ChatCompletionChunk>`.
///
/// Parse le protocole SSE : chaque ligne `data: <json>` est désérialisée.
/// `data: [DONE]` termine le flux proprement. Les lignes vides sont ignorées.
///
/// Effets de bord : alloue un buffer par ligne SSE. Les chunks malformés
/// sont retournés comme `LlmError::Serialization` au lieu de paniquer.
fn sse_bytes_to_chunks(
    response: reqwest::Response,
) -> impl Stream<Item = LlmResult<ChatCompletionChunk>> {
    use futures::StreamExt;

    let byte_stream = response.bytes_stream();

    // Utilise un buffer pour reconstruire les lignes complètes à travers les chunks TCP.
    futures::stream::unfold(
        (byte_stream, String::new()),
        |(mut stream, mut buf)| async move {
            loop {
                // Cherche une ligne complète dans le buffer courant.
                if let Some(newline_pos) = buf.find('\n') {
                    let line = buf[..newline_pos].trim_end_matches('\r').to_string();
                    buf = buf[newline_pos + 1..].to_string();

                    if let Some(data) = line.strip_prefix("data: ") {
                        if data == "[DONE]" {
                            // Fin du flux SSE — on s'arrête proprement.
                            return None;
                        }
                        let result = serde_json::from_str::<ChatCompletionChunk>(data)
                            .map_err(|e| LlmError::Serialization { source: e });
                        return Some((result, (stream, buf)));
                    }
                    // Ligne vide ou commentaire SSE — continuer la boucle.
                    continue;
                }

                // Buffer incomplet — lire le prochain chunk HTTP.
                match stream.next().await {
                    Some(Ok(bytes)) => {
                        // SAFETY : llama.cpp envoie du UTF-8 valide; on remplace
                        // les séquences invalides plutôt que de paniquer.
                        buf.push_str(&String::from_utf8_lossy(&bytes));
                    }
                    Some(Err(e)) => {
                        return Some((
                            Err(LlmError::Network {
                                source: Box::new(e),
                            }),
                            (stream, buf),
                        ));
                    }
                    None => {
                        // Flux TCP terminé sans [DONE] — cas backend qui ferme proprement.
                        return None;
                    }
                }
            }
        },
    )
}

// Nécessaire pour convertir reqwest::Error en serde_json::Error via message.
// Le trait est implémenté par serde_json::Error::custom() via serde::de::Error.
trait SerdeErrorCustom {
    fn custom(msg: impl std::fmt::Display) -> Self;
}

impl SerdeErrorCustom for serde_json::Error {
    fn custom(msg: impl std::fmt::Display) -> Self {
        // Seul moyen de construire un serde_json::Error depuis un message arbitraire.
        serde_json::from_str::<serde_json::Value>(&format!("\"{}\"", msg)).unwrap_err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_commons::capabilities::{ThinkingMode, ToolUseSupport};

    fn default_caps() -> Capabilities {
        Capabilities {
            tool_use: ToolUseSupport::Native,
            streaming: true,
            vision: false,
            thinking: ThinkingMode::None,
            context_max: 131_072,
            structured_output: false,
            prompt_caching: false,
            reasoning_levels: None,
        }
    }

    /// Vérifie que timeout_secs est correctement stocké dans la struct.
    ///
    /// Régression : avant le fix, timeout_secs n'était pas un champ de la struct,
    /// ce qui empêchait stream() de l'utiliser pour le connect_timeout.
    #[test]
    fn timeout_secs_stored_in_struct() {
        let provider = OpenAiCompatProvider::new(
            "test-provider",
            "http://127.0.0.1:9999",
            600,
            None,
            default_caps(),
        )
        .expect("construction OpenAiCompatProvider échouée");

        assert_eq!(
            provider.timeout_secs, 600,
            "timeout_secs doit être 600 tel que passé à new()"
        );
    }

    /// Vérifie que le timeout_secs par défaut (120s) est aussi bien stocké.
    #[test]
    fn timeout_secs_default_stored() {
        let provider = OpenAiCompatProvider::new(
            "test-default",
            "http://127.0.0.1:9999",
            120,
            None,
            default_caps(),
        )
        .expect("construction OpenAiCompatProvider échouée");

        assert_eq!(
            provider.timeout_secs, 120,
            "timeout_secs doit être 120 (valeur défaut config)"
        );
    }

    /// Vérifie que des valeurs extrêmes (très grand timeout pour streaming lent) sont acceptées.
    ///
    /// Cas réel : prompt 17K tokens à 133 tok/s = ~128s avant 1er byte.
    /// Un timeout de 600s doit être stocké et utilisable sans troncature.
    #[test]
    fn timeout_secs_large_value_stored() {
        let provider = OpenAiCompatProvider::new(
            "test-large-timeout",
            "http://127.0.0.1:9999",
            600,
            None,
            default_caps(),
        )
        .expect("construction OpenAiCompatProvider échouée");

        // 600s = 10 minutes — couvre un prompt de ~80K tokens à 133 tok/s
        assert_eq!(provider.timeout_secs, 600);
        // Vérifier aussi la conversion f64 sans perte (utilisée dans LlmError::Timeout)
        assert_eq!(
            provider.timeout_secs as f64, 600.0_f64,
            "conversion u64→f64 sans perte pour elapsed_secs"
        );
    }
}
