# evo-gateway

API aggregator for the Evo self-evolution agent system. Provides a unified API interface over multiple LLM providers (OpenAI, Anthropic, Cursor, Claude Code, Codex CLI, local LLMs via Ollama/vLLM), so agents in the Evo system make all external API calls through a single, configurable gateway.

Supports **SSE streaming** for AI coding tools (Codex CLI, Cursor, Claude Code) and optional **API key authentication** for shared deployments.

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
      +---> GET  /health                  --> Health check (no auth)
      |
      +---> POST /v1/chat/completions     --> Provider Router (streaming supported)
      |                                         |
      +---> POST /v1/messages             -->   +---> OpenAI proxy
      |                                         |
      +---> POST /v1/embeddings           -->   +---> Anthropic proxy
      |                                         |
      +---> GET  /v1/models               -->   +---> Cursor proxy (via cursor-agent CLI)
      +---> GET  /v1/models/:provider     -->   |
      |                                         +---> Claude Code proxy (via claude CLI)
      +---> POST /api/generate            -->   |
      +---> POST /api/chat                -->   +---> Codex CLI proxy (via codex CLI)
                                                |     + PTY model discovery (portable-pty)
                                                |
                                                +---> Local LLM proxy (Ollama / vLLM)
                                          (routing by config / model prefix)

Middleware stack (applied to API routes):
  - Authentication (optional, via EVO_GATEWAY_AUTH)
  - Request logging (tracing)
```

Agents never call provider APIs directly. All outbound LLM traffic flows through this gateway, which injects the correct API keys, enforces rate limits, and can be reconfigured at runtime without changing agent code.

---

## Authentication

The gateway supports optional API key authentication, controlled by environment variables. When disabled (the default), all requests pass through — preserving backward compatibility.

### Enabling Auth

```bash
# Enable authentication
export EVO_GATEWAY_AUTH=true

# Optional: custom keys file path (default: auth.json)
export EVO_GATEWAY_AUTH_KEYS=/path/to/auth.json

cargo run --release
```

### Managing Keys with the CLI

The `evo-gateway-cli` binary manages authentication keys stored in a local JSON file.

```bash
# Generate a new API key
evo-gateway-cli auth generate --name my-key

# Output:
# Generated API key for 'my-key':
#
#   evo-a1b2c3d4e5f6...
#
# Save this key — it cannot be recovered.

# List all keys
evo-gateway-cli auth list

# Output:
# NAME                 PREFIX               CREATED
# ------------------------------------------------------------
# my-key               evo-a1b2c3d4e5f6     2026-02-23 10:30:00 UTC

# Revoke a key
evo-gateway-cli auth revoke --name my-key

# Use a custom keys file
evo-gateway-cli auth generate --name dev-key --keys-file /etc/evo/auth.json
```

### How Auth Works

- Keys are `evo-` prefixed with 48 hex characters (192 bits of entropy)
- Only SHA-256 hashes are stored in `auth.json` — raw keys are shown once at generation time
- The gateway accepts keys via `Authorization: Bearer <key>` or `x-api-key: <key>` headers
- `/health` is always unauthenticated for monitoring
- Invalid or missing keys return `401 Unauthorized` with a JSON error body

### Cursor Authentication

The Cursor provider uses the `cursor-agent` CLI for OAuth-based authentication instead of API keys. Authenticate via the built-in subcommand:

```bash
# Authenticate with Cursor (opens browser for OAuth)
cargo run -- auth cursor

# Or with a release build
./target/release/evo-gateway auth cursor
```

This runs `cursor-agent login`, then saves the auth status to a local database (`gateway.db`). On server startup, the gateway checks and logs the cursor auth status.

### Sending Authenticated Requests

```bash
# OpenAI-style (Authorization: Bearer)
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer evo-your-key-here" \
  -H "Content-Type: application/json" \
  -d '{"model": "openai:gpt-4o", "messages": [{"role": "user", "content": "hi"}]}'

# Anthropic-style (x-api-key)
curl -X POST http://localhost:8080/v1/messages \
  -H "x-api-key: evo-your-key-here" \
  -H "Content-Type: application/json" \
  -d '{"model": "claude-sonnet-4-20250514", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 100}'
```

---

## Streaming

All chat/messages endpoints support SSE streaming when `"stream": true` is set in the request body. Streaming responses are piped directly from the upstream provider without buffering.

```bash
# OpenAI streaming
curl -N -X POST http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer evo-your-key-here" \
  -H "Content-Type: application/json" \
  -d '{"model": "openai:gpt-4o", "messages": [{"role": "user", "content": "hi"}], "stream": true}'

# Anthropic streaming
curl -N -X POST http://localhost:8080/v1/messages \
  -H "x-api-key: evo-your-key-here" \
  -H "Content-Type: application/json" \
  -d '{"model": "claude-sonnet-4-20250514", "messages": [{"role": "user", "content": "hi"}], "stream": true, "max_tokens": 100}'
```

Non-streaming requests (`"stream": false` or omitted) continue to return buffered JSON as before.

---

## Using with AI Coding Tools

### Codex CLI

```bash
export OPENAI_BASE_URL=http://localhost:8080/v1
export OPENAI_API_KEY=evo-<generated-key>
codex
```

### Cursor

Settings > Models > OpenAI API Base URL: `http://localhost:8080/v1`
API Key: `evo-<generated-key>`

### Claude Code

```bash
export ANTHROPIC_BASE_URL=http://localhost:8080
export ANTHROPIC_API_KEY=evo-<generated-key>
claude
```

---

## Configuration

The gateway reads `gateway.json` at startup (auto-generated with defaults if missing). This file is managed by `evo-king`; do not edit it while the gateway is running in production.

### Example `gateway.json`

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
    },
    {
      "name": "anthropic",
      "base_url": "https://api.anthropic.com/v1",
      "api_key_envs": ["ANTHROPIC_API_KEY"],
      "enabled": true,
      "provider_type": "anthropic",
      "extra_headers": {}
    },
    {
      "name": "cursor",
      "base_url": "",
      "api_key_envs": [],
      "enabled": false,
      "provider_type": "cursor",
      "extra_headers": {}
    },
    {
      "name": "claude-code",
      "base_url": "",
      "api_key_envs": [],
      "enabled": false,
      "provider_type": "claude_code",
      "extra_headers": {}
    },
    {
      "name": "codex-cli",
      "base_url": "",
      "api_key_envs": [],
      "enabled": false,
      "provider_type": "codex_cli",
      "extra_headers": {}
    }
  ]
}
```

Multiple `api_key_envs` entries enable token rotation (round-robin, lock-free with `AtomicUsize`).

### Config Lifecycle (managed by evo-king)

1. King detects change via file watcher.
2. King spawns a test gateway instance on an ephemeral port with the new config.
3. King runs health checks against the test instance.
4. **Pass** — King copies the new config to `gateway-running.conf`, backs up the previous config as `gateway-backup-{datetime}.config`, then gracefully restarts the production gateway.
5. **Fail** — King reverts `gateway.config` from `gateway-running.conf` and logs the failure. The production gateway continues running on the last known-good config.

---

## API Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Returns status of the gateway and all configured providers (no auth) |
| `POST` | `/v1/chat/completions` | OpenAI-compatible chat completions (streaming supported) |
| `POST` | `/v1/messages` | Anthropic native Messages API (streaming supported) |
| `POST` | `/v1/embeddings` | Embeddings endpoint |
| `GET` | `/v1/models` | List all available models across enabled providers (`provider:model` format) |
| `GET` | `/v1/models/:provider` | List models for a specific provider |
| `POST` | `/api/generate` | Local LLM generate (Ollama-compatible) |
| `POST` | `/api/chat` | Local LLM chat (Ollama-compatible) |

### Provider:Model Routing

The OpenAI-compatible endpoint supports `"model": "provider:model"` syntax:

```json
{ "model": "openai:gpt-4o", "messages": [...] }
{ "model": "anthropic:claude-opus-4-5", "messages": [...] }
{ "model": "openrouter:meta-llama/llama-3.3-70b-instruct", "messages": [...] }
{ "model": "cursor:auto", "messages": [...] }
{ "model": "claude-code:sonnet", "messages": [...] }
{ "model": "codex-cli:o4-mini", "messages": [...] }
```

If no provider prefix is given, the first enabled provider is used by default.

### Model Discovery

The `/v1/models` endpoint aggregates models from all enabled providers:

- **OpenAI-compatible providers** — fetches from upstream `{base_url}/models` if no models are declared in config
- **CLI providers (Codex CLI)** — discovers models dynamically via PTY-based introspection (see below)
- **Other providers** — returns a `"default"` model entry if no models are configured

Model IDs are returned in `provider:model` format (e.g., `"openai:gpt-4o"`, `"codex-cli:gpt-5.3-codex"`), matching the routing syntax used in chat requests.

### Dynamic CLI Model Discovery (Codex CLI)

For the Codex CLI provider, the gateway discovers available models at startup by spawning `codex --dangerously-bypass-approvals-and-sandbox` in a pseudo-terminal (PTY), sending the `/model` slash command keystroke-by-keystroke, and parsing the TUI menu output. This approach is necessary because the Codex CLI does not expose a programmatic model listing API.

The discovery process:
1. Opens a PTY with `portable-pty` and spawns the Codex CLI in interactive mode
2. Waits for initialization (prompt indicator + MCP servers finishing)
3. Sends `/model` character-by-character (the TUI uses raw-mode keystroke processing)
4. Captures and parses the model selection menu (strips ANSI escape codes, extracts model names via regex)
5. Cleans up: dismisses the menu with Escape and kills the process

Discovered models are cached in `AppState` with a **1-hour TTL**. The `/v1/models` and `/v1/models/codex-cli` endpoints serve from this cache. Discovery runs in a background task after server startup so it does not block request handling.

---

## Source File Layout

```
evo-gateway/
  Cargo.toml
  test-gateway.sh         # Comprehensive curl-based test suite (health, models, all providers)
  src/
    lib.rs                # Library root — exports auth module
    main.rs               # Axum server entry point; loads config, wires auth + routes
    state.rs              # AppState, ProviderPool, TokenPool, CLI models cache
    stream.rs             # SSE streaming passthrough (proxy_streaming, is_streaming)
    error.rs              # GatewayError → HTTP response mapping
    health.rs             # GET /health handler
    auth.rs               # AuthStore — key generation, hashing, verification
    cli_common.rs         # Shared helpers for CLI-subprocess providers (prompt, response, SSE)
    cursor.rs             # Cursor provider — cursor-agent CLI integration (chat + streaming)
    claude_code.rs        # Claude Code provider — claude CLI integration (chat + streaming)
    codex_cli.rs          # Codex CLI provider — codex CLI integration (chat + streaming + PTY model discovery)
    db.rs                 # Local libSQL database for credential storage (cursor auth)
    routes/
      mod.rs              # Route registry
      openai.rs           # POST /v1/chat/completions, /v1/embeddings, GET /v1/models, GET /v1/models/:provider
      anthropic.rs        # POST /v1/messages
      local_llm.rs        # POST /api/generate, /api/chat
    middleware/
      mod.rs              # Request logging middleware
      auth.rs             # Authentication middleware (Bearer + x-api-key)
    bin/
      cli.rs              # evo-gateway-cli binary (auth key management)
```

---

## Build and Run

```sh
# Build all binaries
cargo build --release

# Run the gateway
cargo run --release

# Run with auth enabled
EVO_GATEWAY_AUTH=true cargo run --release

# Run the CLI
cargo run --bin evo-gateway-cli -- auth generate --name my-key

# Run with debug logging
RUST_LOG=debug cargo run

# Run unit tests
cargo test

# Run integration tests against a running gateway (requires curl + jq)
./test-gateway.sh                              # default: http://localhost:8080
./test-gateway.sh http://host:port             # custom target
AUTH_KEY=evo-xxx ./test-gateway.sh             # with authentication

# Lint
cargo clippy -- -D warnings
```

---

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `GATEWAY_CONFIG` | `gateway.json` | Path to JSON config file |
| `OPENAI_API_KEY` | — | OpenAI API key |
| `ANTHROPIC_API_KEY` | — | Anthropic API key |
| `OPENROUTER_API_KEY` | — | OpenRouter API key |
| `EVO_GATEWAY_AUTH` | `false` | Enable API key authentication (`true` or `1`) |
| `EVO_GATEWAY_AUTH_KEYS` | `auth.json` | Path to auth keys JSON file |
| `CURSOR_AGENT_BINARY` | `cursor-agent` | Path to the cursor-agent binary |
| `CURSOR_MAX_CONCURRENT` | `4` | Max concurrent cursor-agent processes |
| `CURSOR_TIMEOUT_SECS` | `120` | Per-request timeout for cursor-agent (seconds) |
| `CLAUDE_CODE_BINARY` | `claude` | Path to the Claude Code CLI binary |
| `CLAUDE_CODE_MAX_CONCURRENT` | `4` | Max concurrent claude processes |
| `CLAUDE_CODE_TIMEOUT_SECS` | `300` | Per-request timeout for claude (seconds) |
| `CODEX_CLI_BINARY` | `codex` | Path to the Codex CLI binary |
| `CODEX_CLI_MAX_CONCURRENT` | `4` | Max concurrent codex processes |
| `CODEX_CLI_TIMEOUT_SECS` | `300` | Per-request timeout for codex (seconds) |
| `EVO_GATEWAY_DB_PATH` | `gateway.db` | Path to local libSQL database for credentials |
| `RUST_LOG` | `info` | Log level filter |
| `EVO_LOG_DIR` | `./logs` | Structured log output directory |
| `EVO_OTLP_ENDPOINT` | `http://localhost:3300` | OTLP HTTP endpoint for distributed tracing (evo-king) |

---

## License

MIT
