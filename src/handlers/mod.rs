//! Handlers Axum du gateway v2.
//!
//! Routes :
//! - `GET /health` → `health::handler`
//! - `POST /v1/chat/completions` → `chat::handler`

pub mod chat;
pub mod health;
