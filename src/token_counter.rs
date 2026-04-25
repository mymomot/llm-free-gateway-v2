//! Comptage heuristique de tokens pour la validation du cap de contexte.
//!
//! Utilise une heuristique chars/4 calibrée sur Qwen3.6 (ratio observé 3.7-4.0
//! chars/token sur corpus FR+EN). Cette approche est intentionnellement conservative
//! par rapport à un tokenizer exact : mieux vaut rejeter légèrement tôt que risquer
//! le freeze backend sur cache-bf16 + corpus >200K.
//!
//! Décision ADR-C4 (council homelab-gouvernance MAJOR 2026-04-25) :
//! - Cap hard : 180000 tokens
//! - Slot ctx réel Qwen3.6 : 262K (YARN, bug freeze connu sur cache-bf16 >200K)
//! - Heuristique préférée à tiktoken-rs (dépendance lourde, inutile à ±5% près)
//!
//! Formule appliquée :
//! - Chaque message : `ceil(contenu_chars / 4)` + overhead rôle constant (4 tokens)
//! - `max_tokens` demandé : ajouté tel quel au total
//!
//! Usage :
//! ```ignore
//! use llm_free_gateway_v2::token_counter::estimate_total_tokens;
//!
//! // total = tokens entrée + max_tokens demandé
//! let total = estimate_total_tokens(&request);
//! ```

use llm_commons::openai::chat::ChatCompletionRequest;

/// Tokens d'overhead par message (rôle + structure JSON approximative).
const TOKENS_PER_MESSAGE_OVERHEAD: u64 = 4;

/// Diviseur chars→tokens : 4 chars ≈ 1 token sur Qwen3.6 (ratio observé 3.7-4.0).
const CHARS_PER_TOKEN: u64 = 4;

/// Estime le nombre total de tokens pour une requête chat (heuristique chars/4).
///
/// Total = Σ(tokens_par_message) + max_tokens_demandé
///
/// Chaque message compte : `ceil(chars_contenu / 4) + 4` (overhead rôle).
///
/// Si `max_tokens` est absent ou nul, seuls les tokens d'entrée sont comptés.
#[must_use]
pub fn estimate_total_tokens(request: &ChatCompletionRequest) -> u64 {
    let input_tokens = estimate_input_tokens(request);
    let output_tokens = request.max_tokens.map(|n| n as u64).unwrap_or(0);
    input_tokens.saturating_add(output_tokens)
}

/// Estime les tokens d'entrée uniquement (sans max_tokens).
///
/// Utilisé pour distinguer tokens entrée / sortie dans les logs.
#[must_use]
pub fn estimate_input_tokens(request: &ChatCompletionRequest) -> u64 {
    request
        .messages
        .iter()
        .map(|msg| {
            let content_chars = msg.content.chars().count() as u64;
            let content_tokens = content_chars.div_ceil(CHARS_PER_TOKEN);
            content_tokens.saturating_add(TOKENS_PER_MESSAGE_OVERHEAD)
        })
        .fold(0u64, |acc, t| acc.saturating_add(t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_commons::openai::chat::{ChatCompletionRequest, Message};

    fn make_request(messages: Vec<Message>, max_tokens: Option<u32>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "test-model".to_string(),
            messages,
            max_tokens,
            stream: None,
            temperature: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            chat_template_kwargs: None,
        }
    }

    // -----------------------------------------------------------------------
    // Tests unitaires tokenizer
    // -----------------------------------------------------------------------

    #[test]
    fn test_vide_retourne_zero_input() {
        let req = make_request(vec![], None);
        assert_eq!(estimate_input_tokens(&req), 0);
    }

    #[test]
    fn test_message_simple_calcul_correct() {
        // 40 chars → 10 tokens + 4 overhead = 14 tokens
        let content = "a".repeat(40);
        let req = make_request(vec![Message::user(&content)], None);
        let tokens = estimate_input_tokens(&req);
        assert_eq!(tokens, 14, "40 chars / 4 + 4 overhead = 14");
    }

    #[test]
    fn test_max_tokens_ajouté_au_total() {
        // 40 chars → 10 tokens + 4 overhead = 14 input
        let content = "a".repeat(40);
        let req = make_request(vec![Message::user(&content)], Some(100));
        let total = estimate_total_tokens(&req);
        assert_eq!(total, 114, "14 input + 100 max_tokens = 114");
    }

    #[test]
    fn test_plusieurs_messages_cumulés() {
        // Message 1 : 40 chars → 10 + 4 = 14 tokens
        // Message 2 : 80 chars → 20 + 4 = 24 tokens
        let req = make_request(
            vec![
                Message::user("a".repeat(40)),
                Message::assistant("b".repeat(80)),
            ],
            None,
        );
        let tokens = estimate_input_tokens(&req);
        assert_eq!(tokens, 38, "14 + 24 = 38");
    }

    #[test]
    fn test_précision_heuristique_sur_texte_fr_en() {
        // Texte réaliste FR+EN. L'heuristique doit être déterministe
        // et dans un ordre de grandeur raisonnable.
        let content = "Voici une phrase en français. Here is an English sentence. \
                       Les modèles de langage sont fascinants. \
                       Language models are fascinating and complex. Merci !";
        let chars = content.chars().count() as u64;
        let req = make_request(vec![Message::user(content)], None);
        let estimated = estimate_input_tokens(&req);
        let expected = chars.div_ceil(CHARS_PER_TOKEN) + TOKENS_PER_MESSAGE_OVERHEAD;
        assert_eq!(
            estimated, expected,
            "heuristique chars/4 doit être déterministe"
        );
        // Vérification ordre de grandeur (pas hors sol).
        assert!(
            estimated > 40 && estimated < 100,
            "estimation {} hors plage raisonnable pour {} chars",
            estimated,
            chars
        );
    }

    #[test]
    fn test_sans_max_tokens_total_égale_input() {
        let req = make_request(vec![Message::user("bonjour")], None);
        assert_eq!(estimate_total_tokens(&req), estimate_input_tokens(&req));
    }

    #[test]
    fn test_saturation_pas_de_panique_sur_grandes_valeurs() {
        // Vérifier que saturating_add ne panic pas sur très grands corpus.
        let content = "x".repeat(1_000_000);
        let req = make_request(vec![Message::user(&content)], Some(u32::MAX));
        // Ne doit pas paniquer.
        let _ = estimate_total_tokens(&req);
    }

    #[test]
    fn test_system_message_comptabilisé() {
        // Le system message doit être compté au même titre que les autres.
        let req = make_request(
            vec![
                Message::system("s".repeat(400)), // 100 tokens + 4 overhead = 104
                Message::user("u".repeat(40)),    // 10 tokens + 4 overhead = 14
            ],
            None,
        );
        let tokens = estimate_input_tokens(&req);
        assert_eq!(tokens, 118, "104 + 14 = 118");
    }
}
