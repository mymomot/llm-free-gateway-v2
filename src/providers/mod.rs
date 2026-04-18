//! Module providers — implémentations de `LlmProvider` pour le gateway v2.
//!
//! Alpha.1 : un seul provider disponible — `openai_compat` (HTTP OpenAI-compat).
//! Les providers futurs (Anthropic natif, Google Gemini) viendront en alpha.2+.

pub mod openai_compat;
