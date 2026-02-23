# evo-gateway

API aggregator for the Evo self-evolution agent system. Provides a unified API interface over multiple LLM providers (OpenAI, Anthropic, local LLMs via Ollama/vLLM), so agents in the Evo system make all external API calls through a single, configurable gateway.

**Status:** Skeleton — Phase 3 pending implementation.

---

## Part of the Evo System

| Crate | Role |
|---|---|
| `evo-common` | Shared types and utilities (dependency of all crates) |
| `evo-gateway` | **(this)** API aggregator, listens on port 8080 |
| `evo-king` | Orchestrator, manages gateway config lifecycle and restarts |
| `evo-agents` | Runner binary + agent folders, agents call APIs through this gateway |

---

## Architecture

```
Client Request
      |
      v
 evo-gateway (Axum)
      |
      +---> GET  /health                  --> Health check (all providers)
      |
      +---> POST /v1/chat/completions     --> Provider Router
      |                                         |
      +---> POST /v1/messages             -->   +---> OpenAI proxy
      |                                         |
      +---> POST /v1/embeddings           -->   +---> Anthropic proxy
                                                |
                                                +---> Local LLM proxy (Ollama / vLLM)
                                          (routing by config / model prefix)

Middleware stack (applied to all routes):
  - Request logging (tracing)
  - Authentication
  - Per-provider rate limiting
```

Agents never call provider APIs directly. All outbound LLM traffic flows through this gateway, which injects the correct API keys, enforces rate limits, and can be reconfigured at runtime without changing agent code.

---

## Configuration

The gateway reads `gateway.config` (TOML) at startup. This file is managed by `evo-king`; do not edit it while the gateway is running in production — use the config lifecycle described below.

### Full example: `gateway.config`

```toml
[server]
host = "0.0.0.0"
port = 8080

[[providers]]
name = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
enabled = true

[providers.rate_limit]
requests_per_minute = 60
burst_size = 10

[[providers]]
name = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key_env = "ANTHROPIC_API_KEY"
enabled = true

[[providers]]
name = "ollama"
base_url = "http://localhost:11434"
api_key_env = ""
enabled = true
```

### Config fields

| Field | Type | Description |
|---|---|---|
| `server.host` | string | Bind address |
| `server.port` | integer | Bind port |
| `providers[].name` | string | Provider identifier used in routing |
| `providers[].base_url` | string | Provider API base URL |
| `providers[].api_key_env` | string | Environment variable name holding the API key (empty string for unauthenticated providers) |
| `providers[].enabled` | bool | Whether this provider is active |
| `providers[].rate_limit.requests_per_minute` | integer | Token bucket refill rate |
| `providers[].rate_limit.burst_size` | integer | Token bucket burst capacity |

---

## Config Lifecycle (managed by evo-king)

`evo-king` owns the gateway config lifecycle. Edits to `gateway.config` (made manually or by the Skill Manage agent) trigger the following sequence:

1. King detects change via file watcher.
2. King spawns a test gateway instance on an ephemeral port with the new config.
3. King runs health checks against the test instance.
4. **Pass** — King copies the new config to `gateway-running.conf`, backs up the previous config as `gateway-backup-{datetime}.config`, then gracefully restarts the production gateway.
5. **Fail** — King reverts `gateway.config` from `gateway-running.conf` and logs the failure. The production gateway continues running on the last known-good config.

This means the gateway itself does not perform hot-reload; `evo-king` is responsible for safe config transitions.

---

## API Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Returns status of the gateway and all configured providers |
| `POST` | `/v1/chat/completions` | OpenAI-compatible chat completions, routed to the configured provider |
| `POST` | `/v1/messages` | Anthropic-compatible messages endpoint |
| `POST` | `/v1/embeddings` | Embeddings endpoint |

All provider-facing endpoints are transparent proxies: the gateway forwards the request body unmodified, injects the provider API key from the environment, and returns the provider response to the caller.

---

## Source File Layout

```
evo-gateway/
  Cargo.toml
  gateway.config          # Active config (managed by evo-king)
  gateway-running.conf    # Last known-good config (written by evo-king on successful restart)
  src/
    main.rs               # Axum server entry point; loads gateway.config, binds routes, starts server
    config.rs             # Parses gateway.config (TOML) using evo_common::GatewayConfig
    health.rs             # GET /health handler; checks reachability of all enabled providers
    routes/
      mod.rs              # Route registry; assembles the Axum Router and dispatches to providers
      openai.rs           # OpenAI API proxy (chat completions, embeddings)
      anthropic.rs        # Anthropic API proxy (messages)
      local_llm.rs        # Local LLM proxy (Ollama and vLLM-compatible endpoints)
    middleware/
      mod.rs              # Composes middleware layers: auth, rate limiting, request logging
```

---

## Dependencies

| Crate | Version | Purpose |
|---|---|---|
| `axum` | 0.7 | HTTP framework and routing |
| `tokio` | 1 (full) | Async runtime |
| `reqwest` | 0.12 (json) | HTTP client for proxying to providers |
| `serde` | 1.0 | Serialization / deserialization |
| `toml` | 0.8 | TOML config parsing |
| `tracing` | 0.1 | Structured logging instrumentation |
| `tracing-subscriber` | 0.3 | Log output formatting and filtering |
| `evo-common` | git | Shared types (`GatewayConfig`, `ProviderConfig`, etc.) and `init_logging` |

`evo-common` is a git dependency from the `ai-evo-agents` GitHub organization.

---

## Shared Types from evo-common

The gateway relies on the following types from `evo-common`:

- `GatewayConfig` — top-level config struct
- `ServerConfig` — host/port settings
- `ProviderConfig` — per-provider settings including `api_key_env` and `enabled`
- `RateLimitConfig` — `requests_per_minute` and `burst_size`

Logging is initialized via:

```rust
evo_common::logging::init_logging("gateway");
```

This writes structured logs to the directory specified by `EVO_LOG_DIR` and to stdout.

---

## Build and Run

### Build

```sh
cargo build
```

### Run (release)

```sh
cargo run --release
```

### Run with debug logging

```sh
RUST_LOG=debug cargo run
```

### Run with per-crate log filtering

```sh
RUST_LOG=evo_gateway=debug,reqwest=warn cargo run
```

The gateway expects `gateway.config` to be present in the working directory at startup.

---

## Environment Variables

| Variable | Required | Description |
|---|---|---|
| `OPENAI_API_KEY` | If OpenAI provider enabled | API key for OpenAI |
| `ANTHROPIC_API_KEY` | If Anthropic provider enabled | API key for Anthropic |
| `RUST_LOG` | No | Log level filter (e.g. `debug`, `info`, `evo_gateway=debug`) |
| `EVO_LOG_DIR` | No | Directory for structured log output (used by `evo-common` logging) |

API key environment variable names are configured per-provider in `gateway.config` via the `api_key_env` field. The names above match the defaults in the example config.

---

## License

MIT
