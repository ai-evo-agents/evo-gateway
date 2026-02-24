# evo-gateway

Multi-provider LLM API aggregator. Proxies requests to OpenAI, Anthropic, OpenRouter, and local models (Ollama/vLLM) with a single consistent API surface. Supports SSE streaming and optional API key authentication.

## Quick Commands

```bash
# Run (default port 8080)
cargo run

# Run with custom config
GATEWAY_CONFIG=/path/to/gateway.json cargo run

# Run with auth enabled
EVO_GATEWAY_AUTH=true cargo run

# Build release
cargo build --release

# Test
cargo test

# Lint
cargo clippy -- -D warnings

# Health check (once running)
curl http://localhost:8080/health

# Generate an auth key
cargo run --bin evo-gateway-cli -- auth generate --name my-key

# List auth keys
cargo run --bin evo-gateway-cli -- auth list

# Revoke an auth key
cargo run --bin evo-gateway-cli -- auth revoke --name my-key
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `GATEWAY_CONFIG` | `gateway.json` | Path to JSON config file |
| `OPENAI_API_KEY` | — | OpenAI API key |
| `ANTHROPIC_API_KEY` | — | Anthropic API key |
| `OPENROUTER_API_KEY` | — | OpenRouter API key |
| `EVO_GATEWAY_AUTH` | `false` | Enable API key authentication (`true` or `1`) |
| `EVO_GATEWAY_AUTH_KEYS` | `auth.json` | Path to auth keys JSON file |
| `CLAUDE_CODE_BINARY` | `claude` | Path to the Claude Code CLI binary |
| `CLAUDE_CODE_MAX_CONCURRENT` | `4` | Max concurrent claude processes |
| `CLAUDE_CODE_TIMEOUT_SECS` | `300` | Per-request timeout for claude (seconds) |
| `CODEX_CLI_BINARY` | `codex` | Path to the Codex CLI binary |
| `CODEX_CLI_MAX_CONCURRENT` | `4` | Max concurrent codex processes |
| `CODEX_CLI_TIMEOUT_SECS` | `300` | Per-request timeout for codex (seconds) |
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
| `GET` | `/health` | Health check — no auth required |
| `POST` | `/v1/chat/completions` | OpenAI-compatible chat (streaming supported) |
| `POST` | `/v1/messages` | Anthropic native Messages API (streaming supported) |
| `POST` | `/v1/embeddings` | Embeddings endpoint |
| `POST` | `/api/generate` | Local LLM generate (Ollama-compatible) |
| `POST` | `/api/chat` | Local LLM chat (Ollama-compatible) |

### Provider:Model Routing

The OpenAI-compatible endpoint supports `"model": "provider:model"` syntax:

```json
{ "model": "openai:gpt-4o", "messages": [...] }
{ "model": "anthropic:claude-opus-4-5", "messages": [...] }
{ "model": "openrouter:meta-llama/llama-3.3-70b-instruct", "messages": [...] }
```

If no provider prefix is given, the first enabled provider is used by default.

## Authentication

Auth is disabled by default (backward compatible). Enable with `EVO_GATEWAY_AUTH=true`.

- Keys managed via `evo-gateway-cli auth generate|list|revoke`
- Accepts `Authorization: Bearer <key>` or `x-api-key: <key>` headers
- Keys are SHA-256 hashed before storage in `auth.json`
- `/health` is always unauthenticated

## Streaming

Chat and messages endpoints support SSE streaming with `"stream": true`. Responses are piped directly from upstream without buffering. Non-streaming requests return buffered JSON as before.

## AI Coding Tool Configuration

**Codex CLI:**
```bash
export OPENAI_BASE_URL=http://localhost:8080/v1
export OPENAI_API_KEY=evo-<generated-key>
```

**Cursor:** Settings > Models > OpenAI API Base URL: `http://localhost:8080/v1`, API Key: `evo-<key>`

**Claude Code:**
```bash
export ANTHROPIC_BASE_URL=http://localhost:8080
export ANTHROPIC_API_KEY=evo-<generated-key>
```

## Architecture

```
src/
├── lib.rs           — library root (exports auth module)
├── main.rs          — startup, config load, Axum server, auth wiring
├── state.rs         — AppState, ProviderPool, TokenPool (lock-free round-robin)
├── stream.rs        — SSE streaming passthrough (proxy_streaming, is_streaming)
├── error.rs         — GatewayError → HTTP response mapping
├── health.rs        — GET /health handler
├── auth.rs          — AuthStore (key generation, hashing, verification)
├── cli_common.rs    — shared helpers for CLI-subprocess providers
├── cursor.rs        — Cursor provider (cursor-agent CLI)
├── claude_code.rs   — Claude Code provider (claude CLI)
├── codex_cli.rs     — Codex CLI provider (codex CLI)
├── middleware/
│   ├── mod.rs       — request logging middleware (tracing)
│   └── auth.rs      — authentication middleware (Bearer + x-api-key)
├── routes/
│   ├── mod.rs       — route registry
│   ├── openai.rs    — POST /v1/chat/completions, /v1/embeddings, /v1/models
│   ├── anthropic.rs — POST /v1/messages
│   └── local_llm.rs — POST /api/generate, /api/chat
└── bin/
    └── cli.rs       — evo-gateway-cli (auth key management)
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
