//! CLI subcommand: `evo-gateway providers`
//!
//! Lists all supported provider types and shows detailed setup instructions
//! for each one (auth method, config fields, example gateway.json snippets).

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum ProvidersAction {
    /// List all supported provider types
    List,
    /// Show detailed setup info for a provider type
    Info {
        /// Provider type (e.g., open_ai_compatible, anthropic, cursor, claude_code,
        /// codex_cli, codex_auth, google, github_copilot)
        #[arg(value_name = "TYPE")]
        provider_type: String,
    },
}

/// Entry point — called from `main.rs` when the user runs `evo-gateway providers`.
pub fn run_providers_command(action: Option<ProvidersAction>) -> Result<()> {
    match action {
        None | Some(ProvidersAction::List) => print_provider_list(),
        Some(ProvidersAction::Info { provider_type }) => print_provider_info(&provider_type),
    }
    Ok(())
}

// ── Provider list ────────────────────────────────────────────────────────────

fn print_provider_list() {
    let rows: &[(&str, &str, &str)] = &[
        (
            "open_ai_compatible",
            "OpenAI Compatible",
            "API key (env var)",
        ),
        ("anthropic", "Anthropic", "API key (env var)"),
        (
            "cursor",
            "Cursor CLI",
            "Browser OAuth (evo-gateway auth cursor)",
        ),
        (
            "claude_code",
            "Claude Code",
            "CLI auth (evo-gateway auth claude)",
        ),
        (
            "codex_cli",
            "Codex CLI",
            "CLI auth (evo-gateway auth codex)",
        ),
        (
            "codex_auth",
            "Codex Auth",
            "OAuth/bearer token (evo-gateway auth codex-auth)",
        ),
        ("google", "Google Gemini", "API key (env var)"),
        ("github_copilot", "GitHub Copilot", "GitHub token (env var)"),
    ];

    println!("Supported Provider Types");
    println!("========================\n");
    println!("  {:<22}  {:<22}  AUTH METHOD", "TYPE", "LABEL");
    println!("  {:<22}  {:<22}  -----------", "----", "-----");
    for (type_name, label, auth) in rows {
        println!("  {type_name:<22}  {label:<22}  {auth}");
    }
    println!();
    println!("Use `evo-gateway providers info <TYPE>` for detailed setup instructions.");
    println!("Common aliases also work: openai, claude, codex, gemini, copilot.");
}

// ── Provider info dispatcher ─────────────────────────────────────────────────

fn print_provider_info(type_name: &str) {
    let resolved = resolve_alias(type_name);
    match resolved {
        "open_ai_compatible" => print_open_ai_compatible(),
        "anthropic" => print_anthropic(),
        "cursor" => print_cursor(),
        "claude_code" => print_claude_code(),
        "codex_cli" => print_codex_cli(),
        "codex_auth" => print_codex_auth(),
        "google" => print_google(),
        "github_copilot" => print_github_copilot(),
        _ => print_unknown(type_name),
    }
}

/// Resolve common aliases to canonical type names.
fn resolve_alias(name: &str) -> &str {
    match name {
        "openai" | "open_ai" | "openai_compatible" => "open_ai_compatible",
        "claude" | "claude-code" => "claude_code",
        "codex" | "codex-cli" => "codex_cli",
        "codex-auth" => "codex_auth",
        "gemini" => "google",
        "copilot" | "github-copilot" | "github_copilot" => "github_copilot",
        other => other,
    }
}

// ── Per-provider info functions ──────────────────────────────────────────────

fn print_open_ai_compatible() {
    println!(
        "\
open_ai_compatible — OpenAI Compatible (HTTP Proxy)
====================================================

How it works:
  Proxies requests to any OpenAI-compatible REST API endpoint.
  Supports /v1/chat/completions, /v1/embeddings, and /v1/models.
  Round-robin load balancing across multiple API keys.

Authentication:
  Set one or more API keys via environment variables.
  The `api_key_envs` array lists which env vars to check (round-robin rotation).

  export OPENAI_API_KEY=\"sk-...\"

Config fields:
  name           Unique provider name (e.g., \"openai\", \"deepseek\", \"groq\")
  base_url       API base URL (e.g., \"https://api.openai.com/v1\")
  api_key_envs   Env var names for API keys [\"OPENAI_API_KEY\"]
  models         Allowed model names [\"gpt-4o\", \"gpt-4o-mini\"]
  extra_headers  Additional HTTP headers (optional)
  enabled        true/false
  rate_limit     Optional rate limit (requests/sec)

Example gateway.json entries:

  OpenAI:
    {{
      \"name\": \"openai\",
      \"provider_type\": \"open_ai_compatible\",
      \"base_url\": \"https://api.openai.com/v1\",
      \"api_key_envs\": [\"OPENAI_API_KEY\"],
      \"models\": [\"gpt-4o\", \"gpt-4o-mini\", \"o3\", \"o4-mini\"],
      \"enabled\": true
    }}

  OpenRouter:
    {{
      \"name\": \"openrouter\",
      \"provider_type\": \"open_ai_compatible\",
      \"base_url\": \"https://openrouter.ai/api/v1\",
      \"api_key_envs\": [\"OPENROUTER_API_KEY\"],
      \"extra_headers\": {{\"HTTP-Referer\": \"https://your-app.com\"}},
      \"models\": [],
      \"enabled\": false
    }}

  Ollama (local):
    {{
      \"name\": \"ollama\",
      \"provider_type\": \"open_ai_compatible\",
      \"base_url\": \"http://localhost:11434\",
      \"api_key_envs\": [],
      \"models\": [],
      \"enabled\": false
    }}

Compatible services: OpenAI, OpenRouter, Ollama, vLLM, DeepSeek, Groq,
  Together AI, xAI (Grok), Mistral, HuggingFace, NVIDIA NIM, Moonshot,
  Qianfan, Volcengine, BytePlus, KiloCode, OpenCode Zen, and more."
    );
}

fn print_anthropic() {
    println!(
        "\
anthropic — Anthropic Messages API
====================================

How it works:
  Accepts OpenAI-format requests, translates to Anthropic Messages API format,
  calls Anthropic (or any Anthropic-wire-compatible endpoint), and translates
  the response back to OpenAI format. Supports streaming.

Authentication:
  Set your Anthropic API key via environment variable.

  export ANTHROPIC_API_KEY=\"sk-ant-...\"

  The gateway automatically adds the required headers:
    x-api-key: <your key>
    anthropic-version: 2023-06-01

Config fields:
  name           Provider name (e.g., \"anthropic\")
  base_url       \"https://api.anthropic.com/v1\"
  api_key_envs   [\"ANTHROPIC_API_KEY\"]
  models         [\"claude-opus-4-5\", \"claude-sonnet-4-5\", \"claude-haiku-3-5\"]
  enabled        true/false

Example gateway.json:

  Anthropic (direct):
    {{
      \"name\": \"anthropic\",
      \"provider_type\": \"anthropic\",
      \"base_url\": \"https://api.anthropic.com/v1\",
      \"api_key_envs\": [\"ANTHROPIC_API_KEY\"],
      \"models\": [\"claude-opus-4-5\", \"claude-sonnet-4-5\", \"claude-haiku-3-5\"],
      \"enabled\": true
    }}

Also works with Anthropic-wire-compatible services:
  MiniMax (api.minimax.io/anthropic), Xiaomi (api.xiaomimimo.com/anthropic),
  Synthetic (api.synthetic.new/anthropic), Kimi Coding (api.kimi.com/coding/)."
    );
}

fn print_cursor() {
    println!(
        "\
cursor — Cursor CLI (Subprocess Provider)
==========================================

How it works:
  Spawns `cursor-agent` as a subprocess for each request. The prompt is passed
  via stdin/args and the response is captured from stdout. No HTTP proxying —
  requests are handled entirely through the CLI binary.

Prerequisites:
  Install cursor-agent: https://github.com/nicepkg/cursor-agent
    npm install -g cursor-agent

Authentication:
  Run the interactive OAuth flow:
    evo-gateway auth cursor

  This opens your browser for Cursor OAuth login and saves credentials
  to the gateway database.

Config fields:
  name             \"cursor\"
  provider_type    \"cursor\"
  base_url         \"\" (not used — subprocess-based)
  api_key_envs     [] (not used — auth is via OAuth)
  models           [\"auto\", \"gpt-4o\", \"claude-sonnet-4-5\", \"claude-opus-4-5\"]
  enabled          true/false

Environment overrides:
  CURSOR_AGENT_BINARY   Path to cursor-agent binary (default: \"cursor-agent\")

Example gateway.json:
  {{
    \"name\": \"cursor\",
    \"provider_type\": \"cursor\",
    \"base_url\": \"\",
    \"api_key_envs\": [],
    \"models\": [\"auto\", \"gpt-4o\", \"claude-sonnet-4-5\"],
    \"enabled\": true
  }}

Notes:
  - Each request spawns a new subprocess (no connection pooling)
  - Model selection is passed to cursor-agent which routes internally
  - Auth status checked on gateway startup"
    );
}

fn print_claude_code() {
    println!(
        "\
claude_code — Claude Code CLI (Subprocess Provider)
=====================================================

How it works:
  Spawns `claude` CLI in print mode with streaming JSON output:
    claude -p --output-format stream-json \"<prompt>\"

  Supports real-time streaming — gateway converts Claude's stream-json
  events into OpenAI-compatible SSE chunks.

Prerequisites:
  Install Claude Code CLI:
    npm install -g @anthropic-ai/claude-code
  Docs: https://docs.anthropic.com/en/docs/claude-code

Authentication:
  Run the interactive auth flow:
    evo-gateway auth claude

  This runs `claude` which prompts for login. Credentials are stored
  by the Claude CLI itself. Gateway records auth status in its database.

Config fields:
  name             \"claude-code\"
  provider_type    \"claude_code\"
  base_url         \"\" (not used — subprocess-based)
  api_key_envs     [] (not used — auth managed by claude CLI)
  models           [\"claude-sonnet-4-5\", \"claude-opus-4-5\", \"claude-haiku-3-5\"]
  enabled          true/false

Environment overrides:
  CLAUDE_CODE_BINARY   Path to claude binary (default: \"claude\")

Example gateway.json:
  {{
    \"name\": \"claude-code\",
    \"provider_type\": \"claude_code\",
    \"base_url\": \"\",
    \"api_key_envs\": [],
    \"models\": [\"claude-sonnet-4-5\", \"claude-opus-4-5\", \"claude-haiku-3-5\"],
    \"enabled\": true
  }}

Notes:
  - Supports streaming via --output-format stream-json
  - Model passed via --model flag to claude CLI
  - Auth status and version checked on gateway startup"
    );
}

fn print_codex_cli() {
    println!(
        "\
codex_cli — Codex CLI (Subprocess Provider)
=============================================

How it works:
  Spawns `codex` CLI in exec mode for each request. The gateway uses PTY
  (pseudo-terminal) to interact with codex, supporting model discovery
  at startup and prompt execution at request time.

Prerequisites:
  Install OpenAI Codex CLI:
    npm install -g @openai/codex

Authentication:
  Run the interactive auth flow:
    evo-gateway auth codex

  This runs `codex` which prompts for login/API key setup.
  Credentials are stored by the Codex CLI itself.

Config fields:
  name             \"codex-cli\"
  provider_type    \"codex_cli\"
  base_url         \"\" (not used — subprocess-based)
  api_key_envs     [] (not used — auth managed by codex CLI)
  models           [\"gpt-5.3-codex\", \"gpt-5.2-codex\", \"default\"]
  enabled          true/false

Environment overrides:
  CODEX_CLI_BINARY   Path to codex binary (default: \"codex\")

Example gateway.json:
  {{
    \"name\": \"codex-cli\",
    \"provider_type\": \"codex_cli\",
    \"base_url\": \"\",
    \"api_key_envs\": [],
    \"models\": [\"gpt-5.3-codex\", \"gpt-5.2-codex\", \"default\"],
    \"enabled\": true
  }}

Features:
  - PTY model discovery: if models list is empty, gateway auto-discovers
    available models from codex CLI at startup
  - Models cached in memory for fast /v1/models responses
  - Auth status and version checked on gateway startup"
    );
}

fn print_codex_auth() {
    println!(
        "\
codex_auth — OpenAI Codex Responses API (HTTP + OAuth)
=======================================================

How it works:
  Direct HTTP requests to OpenAI's Responses API using an OAuth bearer token
  or API key. Unlike codex_cli (subprocess), this uses standard HTTP proxying
  with token-based authentication.

Authentication (choose one):
  Option A — OAuth browser flow:
    evo-gateway auth codex-auth
    Opens browser for OpenAI OAuth PKCE login.

  Option B — Direct token:
    evo-gateway auth codex-auth --token \"<api-key-or-token>\"
    Skip interactive prompt, store token directly.

  Option C — Team accounts:
    evo-gateway auth codex-auth --token \"<token>\" --account-id \"<id>\"
    Include ChatGPT account ID for team routing.

  Token stored in: $HOME/.evo-gateway/gateway.db

Config fields:
  name             \"codex-auth\"
  provider_type    \"codex_auth\"
  base_url         \"https://api.openai.com/v1\"
  api_key_envs     [\"OPENAI_API_KEY\"] (fallback if no OAuth token)
  models           [\"codex-mini-latest\", \"gpt-4.1\", \"o3\", \"o4-mini\"]
  enabled          true/false

Example gateway.json:
  {{
    \"name\": \"codex-auth\",
    \"provider_type\": \"codex_auth\",
    \"base_url\": \"https://api.openai.com/v1\",
    \"api_key_envs\": [\"OPENAI_API_KEY\"],
    \"models\": [\"codex-mini-latest\", \"gpt-4.1\", \"gpt-4.1-mini\", \"o3\", \"o4-mini\"],
    \"enabled\": true
  }}

Notes:
  - OAuth token takes priority over api_key_envs
  - Token auto-refreshed when expired (if obtained via OAuth flow)
  - Compatible with OpenAI Responses API and Chat Completions API"
    );
}

fn print_google() {
    println!(
        "\
google — Google Gemini (Native API)
=====================================

How it works:
  Translates incoming OpenAI-format requests to Google's native
  generateContent API format. Sends requests directly to the Gemini API
  and translates responses back to OpenAI format.

  Unlike using Gemini via open_ai_compatible (which uses Google's OpenAI
  compatibility layer), this provider uses the native API for full feature
  support including native streaming.

Authentication:
  Set your API key via environment variable:

  export GEMINI_API_KEY=\"AI...\"
  # or
  export GOOGLE_API_KEY=\"AI...\"

  The API key is passed as a query parameter (?key=...), not in headers.

Config fields:
  name             \"google\"
  provider_type    \"google\"
  base_url         \"https://generativelanguage.googleapis.com\"
  api_key_envs     [\"GEMINI_API_KEY\", \"GOOGLE_API_KEY\"]
  models           [\"gemini-2.5-pro\", \"gemini-2.5-flash\", \"gemini-2.0-flash\"]
  enabled          true/false

Example gateway.json:
  {{
    \"name\": \"google\",
    \"provider_type\": \"google\",
    \"base_url\": \"https://generativelanguage.googleapis.com\",
    \"api_key_envs\": [\"GEMINI_API_KEY\", \"GOOGLE_API_KEY\"],
    \"models\": [\"gemini-2.5-pro\", \"gemini-2.5-flash\", \"gemini-2.0-flash\"],
    \"enabled\": true
  }}

Notes:
  - Get an API key at: https://aistudio.google.com/app/apikey
  - Supports both streaming and non-streaming requests
  - For OpenAI-compatible access instead, use provider_type \"open_ai_compatible\"
    with base_url \"https://generativelanguage.googleapis.com/v1beta/openai\""
    );
}

fn print_github_copilot() {
    println!(
        "\
github_copilot — GitHub Copilot (Token Exchange)
==================================================

How it works:
  Exchanges a GitHub personal access token (or Copilot token) for a
  short-lived Copilot session token, then uses OpenAI-compatible wire
  format to route requests to GitHub Copilot's completion endpoint.

  Token exchange: GitHub token -> Copilot internal API -> session token
  The session token is cached and auto-refreshed when expired.

Authentication:
  Set your GitHub token via environment variable:

  export COPILOT_GITHUB_TOKEN=\"ghp_...\"
  # or
  export GH_TOKEN=\"ghp_...\"

  The token needs an active GitHub Copilot subscription.

Config fields:
  name             \"github-copilot\"
  provider_type    \"github_copilot\"
  base_url         \"\" (auto-resolved via token exchange)
  api_key_envs     [\"COPILOT_GITHUB_TOKEN\", \"GH_TOKEN\"]
  models           [\"claude-sonnet-4-5\", \"gpt-4o\", \"gpt-4.1\", \"o4-mini\"]
  enabled          true/false

Example gateway.json:
  {{
    \"name\": \"github-copilot\",
    \"provider_type\": \"github_copilot\",
    \"base_url\": \"\",
    \"api_key_envs\": [\"COPILOT_GITHUB_TOKEN\", \"GH_TOKEN\"],
    \"models\": [\"claude-sonnet-4-5\", \"gpt-4o\", \"gpt-4.1\", \"o4-mini\"],
    \"enabled\": true
  }}

Notes:
  - Requires active GitHub Copilot subscription (Individual, Business, or Enterprise)
  - Token exchange endpoint: https://api.github.com/copilot_internal/v2/token
  - Session tokens are short-lived and auto-refreshed
  - Supports streaming via OpenAI-compatible SSE format"
    );
}

fn print_unknown(type_name: &str) {
    eprintln!("Unknown provider type: \"{type_name}\"\n");
    eprintln!("Available types:");
    eprintln!("  open_ai_compatible   anthropic   cursor       claude_code");
    eprintln!("  codex_cli            codex_auth  google       github_copilot");
    eprintln!();
    eprintln!("Common aliases: openai, claude, codex, gemini, copilot");
    eprintln!();
    eprintln!("Run `evo-gateway providers list` for a full summary.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_alias_canonical() {
        assert_eq!(resolve_alias("open_ai_compatible"), "open_ai_compatible");
        assert_eq!(resolve_alias("anthropic"), "anthropic");
        assert_eq!(resolve_alias("cursor"), "cursor");
        assert_eq!(resolve_alias("claude_code"), "claude_code");
        assert_eq!(resolve_alias("codex_cli"), "codex_cli");
        assert_eq!(resolve_alias("codex_auth"), "codex_auth");
        assert_eq!(resolve_alias("google"), "google");
        assert_eq!(resolve_alias("github_copilot"), "github_copilot");
    }

    #[test]
    fn test_resolve_alias_shortcuts() {
        assert_eq!(resolve_alias("openai"), "open_ai_compatible");
        assert_eq!(resolve_alias("claude"), "claude_code");
        assert_eq!(resolve_alias("codex"), "codex_cli");
        assert_eq!(resolve_alias("gemini"), "google");
        assert_eq!(resolve_alias("copilot"), "github_copilot");
    }

    #[test]
    fn test_resolve_alias_hyphenated() {
        assert_eq!(resolve_alias("claude-code"), "claude_code");
        assert_eq!(resolve_alias("codex-cli"), "codex_cli");
        assert_eq!(resolve_alias("codex-auth"), "codex_auth");
        assert_eq!(resolve_alias("github-copilot"), "github_copilot");
    }

    #[test]
    fn test_resolve_alias_unknown_passthrough() {
        assert_eq!(resolve_alias("unknown"), "unknown");
        assert_eq!(resolve_alias("foobar"), "foobar");
    }

    #[test]
    fn test_run_providers_command_list() {
        // Should not panic
        assert!(run_providers_command(None).is_ok());
        assert!(run_providers_command(Some(ProvidersAction::List)).is_ok());
    }

    #[test]
    fn test_run_providers_command_info_all_types() {
        let types = [
            "open_ai_compatible",
            "anthropic",
            "cursor",
            "claude_code",
            "codex_cli",
            "codex_auth",
            "google",
            "github_copilot",
        ];
        for t in types {
            assert!(
                run_providers_command(Some(ProvidersAction::Info {
                    provider_type: t.to_string(),
                }))
                .is_ok()
            );
        }
    }

    #[test]
    fn test_run_providers_command_info_unknown() {
        // Unknown type should still return Ok (prints to stderr)
        assert!(
            run_providers_command(Some(ProvidersAction::Info {
                provider_type: "unknown".to_string(),
            }))
            .is_ok()
        );
    }
}
