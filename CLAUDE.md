# evo-gateway

Multi-provider LLM API aggregator. Proxies requests to OpenAI, Anthropic, OpenRouter, and local models (Ollama/vLLM) with a single consistent API surface.

## Quick Commands

```bash
# Run (default port 8080)
cargo run

# Run with custom config
GATEWAY_CONFIG=/path/to/gateway.json cargo run

# Build release
cargo build --release

# Test
cargo test

# Lint
cargo clippy -- -D warnings

# Health check (once running)
curl http://localhost:8080/health
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `GATEWAY_CONFIG` | `gateway.json` | Path to JSON config file |
| `OPENAI_API_KEY` | — | OpenAI API key |
| `ANTHROPIC_API_KEY` | — | Anthropic API key |
| `OPENROUTER_API_KEY` | — | OpenRouter API key |
| `EVO_LOG_DIR` | `./logs` | Log output directory |
| `RUST_LOG` | `info` | Log level filter |

## Config File (`gateway.json`)

JSON format — auto-generated with defaults if missing:

```json
{
  "server": { "host": "0.0.0.0", "port": 8080 },
  "providers": [
    {
      "name": "openai",
      "base_url": "https://api.openai.com/v1",
      "api_key_envs": ["OPENAI_API_KEY"],
      "enabled": true,
      "provider_type": "open_ai_compatible",
      "extra_headers": {}
    }
  ]
}
```

Multiple `api_key_envs` entries enable token rotation (round-robin, lock-free with `AtomicUsize`).

## API Routes

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Health check — lists enabled providers |
| `POST` | `/v1/chat/completions` | OpenAI-compatible chat (provider:model routing) |
| `POST` | `/v1/messages` | Anthropic native Messages API |
| `POST` | `/v1/local` | Local LLM proxy (Ollama / vLLM) |

### Provider:Model Routing

The OpenAI-compatible endpoint supports `"model": "provider:model"` syntax:

```json
{ "model": "openai:gpt-4o", "messages": [...] }
{ "model": "anthropic:claude-opus-4-5", "messages": [...] }
{ "model": "openrouter:meta-llama/llama-3.3-70b-instruct", "messages": [...] }
```

If no provider prefix is given, `openai` is used by default.

## Architecture

```
src/
├── main.rs          — startup, config load, Axum server
├── state.rs         — AppState, ProviderPool, TokenPool (lock-free round-robin)
├── config.rs        — config parsing helpers
├── error.rs         — GatewayError → HTTP response mapping
├── health.rs        — GET /health handler
├── middleware/
│   └── mod.rs       — request logging middleware (tracing)
└── routes/
    ├── mod.rs        — route registry
    ├── openai.rs     — POST /v1/chat/completions
    ├── anthropic.rs  — POST /v1/messages
    └── local_llm.rs  — POST /v1/local
```

### Key Types (`state.rs`)

```rust
struct TokenPool { tokens: Vec<String>, next: AtomicUsize }  // lock-free round-robin
struct ProviderPool { config: ProviderConfig, token_pool: TokenPool }
struct AppState { pools: Arc<RwLock<HashMap<String, Arc<ProviderPool>>>>, http_client }
```

## Logging

Logs write to `logs/gateway.log` (rolling daily JSON) and stdout (human-readable).

```bash
# See all requests
RUST_LOG=debug cargo run

# Production
RUST_LOG=info cargo run
```

## evo-king Integration

evo-king watches `gateway.json` for changes. When the config file is modified:
1. king validates the new JSON
2. Creates a timestamped backup in `backups/`
3. Broadcasts `king:config_update` to all connected runners

To trigger a config reload from king's side: simply write an updated `gateway.json`.
