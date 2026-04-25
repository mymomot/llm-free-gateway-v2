# Dépendances — llm-free-gateway-v2

> Généré le : 2026-04-25
> Commit ref : c3cc70b

Runtime : **Rust** | Édition : 2021 | Version crate : `0.1.0-alpha.3`
Registry personnalisé : `kellnr` (Forgejo Kellnr local) pour `llm-commons`.

---

## Dépendances directes (Cargo.toml)

### Production

| Crate | Version | Usage |
|---|---|---|
| `axum` | 0.8 | Framework HTTP async |
| `tokio` | 1 (full) | Runtime async |
| `serde` + `serde_json` | 1 | Sérialisation JSON |
| `reqwest` | 0.12 (json, stream) | Client HTTP providers + embeddings proxy |
| `tracing` + `tracing-subscriber` | 0.1 / 0.3 | Journalisation structurée |
| `tower-http` | 0.6 (trace, cors) | Middlewares HTTP |
| `futures` + `futures-core` | 0.3 | Streams async (SSE passthrough) |
| `toml` | 0.8 | Parsing config TOML |
| `bytes` | 1 | Buffers bytes SSE |
| `anyhow` | 1 | Gestion erreurs (startup) |
| `thiserror` | 1 | Erreurs typées (ApiError, LlmError) |
| `async-trait` | 0.1 | Trait LlmProvider async |
| `rusqlite` | 0.32 (bundled) | Registry SQLite journalisation |
| `llm-commons` | 0.5.0-alpha.5 (kellnr) | Traits LLM, CB, streaming, types partagés |
| `subtle` | 2 | Comparaison temps-constant Bearer token |
| `tempfile` | 3 | Tests uniquement (dev) |
| `axum-test` | 20 | Tests d'intégration (dev) |

### Transitive notables

| Crate | Version | Notes |
|---|---|---|
| `hyper` | 1.9.0 | Transport HTTP/1+HTTP/2 sous reqwest/axum |
| `h2` | 0.4.13 | HTTP/2 support |
| `native-tls` + `openssl` | 0.2.18 / 0.10.77 | TLS via reqwest (OpenRouter HTTPS) |
| `rusqlite` bundled | libsqlite3-sys 0.30.1 | SQLite compilé statiquement |
| `utoipa` | 5.4.0 | OpenAPI specs via llm-commons |
| `matchit` | 0.8.4 | Router trie Axum |
| `tower` | 0.5.3 | Middleware layer framework |

---

## Arbre complet (cargo tree --workspace)

```
llm-free-gateway-v2 v0.1.0-alpha.3
├── anyhow v1.0.102
├── async-trait v0.1.89 (proc-macro)
├── axum v0.8.9
│   ├── axum-core v0.5.6
│   ├── bytes v1.11.1
│   ├── hyper v1.9.0
│   │   └── h2 v0.4.13
│   ├── hyper-util v0.1.20
│   ├── tower v0.5.3
│   └── tracing v0.1.44
├── bytes v1.11.1
├── futures v0.3.32
├── futures-core v0.3.32
├── llm-commons v0.5.0-alpha.5 (registry `kellnr`)
│   ├── async-trait v0.1.89 (*)
│   ├── axum v0.8.9 (*)
│   ├── serde v1.0.228
│   ├── serde_json v1.0.149
│   ├── tokio v1.52.1 (*)
│   └── utoipa v5.4.0
├── reqwest v0.12.28
│   ├── hyper-tls v0.6.0
│   │   └── native-tls v0.2.18
│   │       └── openssl v0.10.77
│   ├── tokio v1.52.1
│   └── tower-http v0.6.8
├── rusqlite v0.32.1 (bundled — libsqlite3-sys 0.30.1)
├── serde v1.0.228
├── serde_json v1.0.149
├── subtle v2.6.1
├── thiserror v1.0.69
├── tokio v1.52.1 (full)
├── toml v0.8.23
│   └── toml_edit v0.22.27
├── tower-http v0.6.8 (trace, cors)
├── tracing v0.1.44
└── tracing-subscriber v0.3.23
[dev-dependencies]
├── axum-test v20.0.0
└── tempfile v3.27.0
```

(Arbre complet 521 lignes — version raccourcie pour lisibilité. Générer via `cargo tree --workspace`.)

---

## Points d'attention

- **`llm-commons` sur registry `kellnr`** : dépendance interne Forgejo local. Si le registry est inaccessible → `cargo build` échoue. Mirror GitHub ne publie pas sur Kellnr.
- **`rusqlite` bundled** : SQLite compilé statiquement (~3MB de code C linkés). Avantage : pas de dépendance système libsqlite3-dev. Inconvénient : temps de compilation plus long.
- **`native-tls` + `openssl`** : TLS system (OpenSSL) via reqwest pour les appels HTTPS vers OpenRouter. Si `openssl-dev` absent → build failure.
- **`subtle` v2** : comparaison temps-constant pour le Bearer token inbound — protège contre les timing attacks.
- **`axum-test` v20** : version majeure récente — API potentiellement breaking vs v16-19. Épinglé à "20".
- **Doublons de version** : `thiserror` v1.0.69 (prod) vs v2.0.18 (via axum-test/expect-json) — deux versions coexistent dans le lockfile.
