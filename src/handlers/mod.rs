//! Handlers Axum du gateway v2.
//!
//! Routes :
//! - `GET /health`              → `health::handler`
//! - `GET /metrics`             → `metrics_handler::handler`
//! - `GET /v1/models`           → `models::handler`
//! - `POST /v1/chat/completions` → `chat::handler`
//! - `POST /v1/embeddings`      → `embeddings::handler`

pub mod chat;
pub mod embeddings;
pub mod health;
pub mod metrics_handler;
pub mod models;
