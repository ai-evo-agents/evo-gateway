#!/usr/bin/env bash
# test-gateway.sh — comprehensive curl tests for evo-gateway
#
# Usage:
#   ./test-gateway.sh              # defaults to http://localhost:8080
#   ./test-gateway.sh http://host:port
#   AUTH_KEY=evo-xxx ./test-gateway.sh   # with authentication
#
# Prerequisites: curl, jq

set -euo pipefail

BASE="${1:-http://localhost:8080}"
AUTH_ARGS=()
if [[ -n "${AUTH_KEY:-}" ]]; then
  AUTH_ARGS=(-H "Authorization: Bearer $AUTH_KEY")
fi

pass=0
fail=0
skip=0

run_test() {
  local name="$1"
  shift
  echo ""
  echo "━━━ $name ━━━"
  if "$@"; then
    echo ""
    ((pass++))
  else
    echo "  ⚠ FAILED (exit $?)"
    ((fail++))
  fi
}

# Helper: POST JSON to an endpoint (non-streaming)
post_json() {
  local path="$1"
  local data="$2"
  curl -sf "$BASE$path" "${AUTH_ARGS[@]}" \
    -H "Content-Type: application/json" \
    -d "$data" | jq .
}

# Helper: POST JSON with streaming (SSE)
post_stream() {
  local path="$1"
  local data="$2"
  curl -sN "$BASE$path" "${AUTH_ARGS[@]}" \
    -H "Content-Type: application/json" \
    -d "$data"
  echo ""
}

echo "╔══════════════════════════════════════════════════════╗"
echo "║       evo-gateway — comprehensive test suite        ║"
echo "╚══════════════════════════════════════════════════════╝"
echo "Target: $BASE"
echo "Auth:   ${AUTH_KEY:+enabled}${AUTH_KEY:-disabled}"
echo ""

# ──────────────────────────────────────────────────────────────
# 1. Health & Discovery
# ──────────────────────────────────────────────────────────────

run_test "1. Health Check (GET /health)" \
  curl -sf "$BASE/health" | jq .

run_test "2. List All Models (GET /v1/models)" \
  curl -sf "$BASE/v1/models" "${AUTH_ARGS[@]}" | jq .

run_test "3. List Models per Provider (GET /v1/models/openai)" \
  curl -sf "$BASE/v1/models/openai" "${AUTH_ARGS[@]}" | jq .

run_test "4. List Models per Provider (GET /v1/models/anthropic)" \
  curl -sf "$BASE/v1/models/anthropic" "${AUTH_ARGS[@]}" | jq .

# ──────────────────────────────────────────────────────────────
# 2. OpenAI Provider
# ──────────────────────────────────────────────────────────────

run_test "5. OpenAI Chat — non-streaming (openai:gpt-4o-mini)" \
  post_json "/v1/chat/completions" '{
    "model": "openai:gpt-4o-mini",
    "messages": [{"role": "user", "content": "Say hello in one word"}],
    "max_tokens": 10
  }'

run_test "6. OpenAI Chat — streaming (openai:gpt-4o-mini)" \
  post_stream "/v1/chat/completions" '{
    "model": "openai:gpt-4o-mini",
    "messages": [{"role": "user", "content": "Count to 3"}],
    "stream": true,
    "max_tokens": 30
  }'

run_test "7. OpenAI Embeddings (openai:text-embedding-3-small)" \
  post_json "/v1/embeddings" '{
    "model": "openai:text-embedding-3-small",
    "input": "Hello world"
  }'

# ──────────────────────────────────────────────────────────────
# 3. Anthropic Provider
# ──────────────────────────────────────────────────────────────

run_test "8. Anthropic via /v1/chat/completions (anthropic:claude-haiku-3-5)" \
  post_json "/v1/chat/completions" '{
    "model": "anthropic:claude-haiku-3-5",
    "messages": [{"role": "user", "content": "Say hello in one word"}],
    "max_tokens": 10
  }'

run_test "9. Anthropic Native /v1/messages" \
  post_json "/v1/messages" '{
    "model": "claude-haiku-3-5",
    "max_tokens": 10,
    "messages": [{"role": "user", "content": "Say hello in one word"}]
  }'

run_test "10. Anthropic Native — streaming" \
  post_stream "/v1/messages" '{
    "model": "claude-haiku-3-5",
    "max_tokens": 30,
    "stream": true,
    "messages": [{"role": "user", "content": "Count to 3"}]
  }'

# ──────────────────────────────────────────────────────────────
# 4. Auto-Routing (no provider prefix)
# ──────────────────────────────────────────────────────────────

run_test "11. Auto-routed Chat (gpt-4o-mini, no prefix)" \
  post_json "/v1/chat/completions" '{
    "model": "gpt-4o-mini",
    "messages": [{"role": "user", "content": "Say hello"}],
    "max_tokens": 10
  }'

# ──────────────────────────────────────────────────────────────
# 5. Cursor Provider (CLI)
# ──────────────────────────────────────────────────────────────

run_test "12. Cursor Chat — non-streaming (cursor:auto)" \
  post_json "/v1/chat/completions" '{
    "model": "cursor:auto",
    "messages": [{"role": "user", "content": "What is 2+2?"}],
    "max_tokens": 20
  }'

run_test "13. Cursor Chat — streaming (cursor:auto)" \
  post_stream "/v1/chat/completions" '{
    "model": "cursor:auto",
    "messages": [{"role": "user", "content": "Count to 3"}],
    "stream": true
  }'

# ──────────────────────────────────────────────────────────────
# 6. Claude Code Provider (CLI)
# ──────────────────────────────────────────────────────────────

run_test "14. Claude Code Chat — non-streaming (claude-code:claude-sonnet-4-5)" \
  post_json "/v1/chat/completions" '{
    "model": "claude-code:claude-sonnet-4-5",
    "messages": [{"role": "user", "content": "What is 2+2?"}],
    "max_tokens": 20
  }'

run_test "15. Claude Code Chat — streaming (claude-code:claude-sonnet-4-5)" \
  post_stream "/v1/chat/completions" '{
    "model": "claude-code:claude-sonnet-4-5",
    "messages": [{"role": "user", "content": "Count to 3"}],
    "stream": true
  }'

run_test "16. Claude Code Chat — different model (claude-code:claude-opus-4-5)" \
  post_json "/v1/chat/completions" '{
    "model": "claude-code:claude-opus-4-5",
    "messages": [{"role": "user", "content": "What is 2+2?"}],
    "max_tokens": 20
  }'

# ──────────────────────────────────────────────────────────────
# 7. Codex CLI Provider
# ──────────────────────────────────────────────────────────────

run_test "17. Codex CLI Chat — non-streaming (codex-cli:gpt-5.3-codex)" \
  post_json "/v1/chat/completions" '{
    "model": "codex-cli:gpt-5.3-codex",
    "messages": [{"role": "user", "content": "What is 2+2?"}],
    "max_tokens": 20
  }'

run_test "18. Codex CLI Chat — default model (codex-cli:default)" \
  post_json "/v1/chat/completions" '{
    "model": "codex-cli:default",
    "messages": [{"role": "user", "content": "What is 2+2?"}]
  }'

run_test "19. Codex CLI Chat — streaming (codex-cli:gpt-5.3-codex)" \
  post_stream "/v1/chat/completions" '{
    "model": "codex-cli:gpt-5.3-codex",
    "messages": [{"role": "user", "content": "Count to 3"}],
    "stream": true
  }'

run_test "20. Codex CLI Chat — mini model (codex-cli:gpt-5.1-codex-mini)" \
  post_json "/v1/chat/completions" '{
    "model": "codex-cli:gpt-5.1-codex-mini",
    "messages": [{"role": "user", "content": "What is 2+2?"}],
    "max_tokens": 20
  }'

# ──────────────────────────────────────────────────────────────
# 8. Multi-turn conversations
# ──────────────────────────────────────────────────────────────

run_test "21. Multi-turn (system + user + assistant + user)" \
  post_json "/v1/chat/completions" '{
    "model": "openai:gpt-4o-mini",
    "messages": [
      {"role": "system", "content": "You are a math tutor."},
      {"role": "user", "content": "What is 2+2?"},
      {"role": "assistant", "content": "4"},
      {"role": "user", "content": "And 3+3?"}
    ],
    "max_tokens": 10
  }'

# ──────────────────────────────────────────────────────────────
# 9. Error cases
# ──────────────────────────────────────────────────────────────

echo ""
echo "━━━ 22. Error: Unknown provider ━━━"
http_code=$(curl -so /dev/null -w "%{http_code}" "$BASE/v1/chat/completions" "${AUTH_ARGS[@]}" \
  -H "Content-Type: application/json" \
  -d '{"model":"nonexistent:gpt-4o","messages":[{"role":"user","content":"hi"}]}')
if [[ "$http_code" =~ ^(404|503)$ ]]; then
  echo "  Got HTTP $http_code (expected 404 or 503)"
  ((pass++))
else
  echo "  ⚠ Got HTTP $http_code (expected 404 or 503)"
  ((fail++))
fi

echo ""
echo "━━━ 23. Error: Disabled provider model listing ━━━"
http_code=$(curl -so /dev/null -w "%{http_code}" "$BASE/v1/models/nonexistent" "${AUTH_ARGS[@]}")
if [[ "$http_code" =~ ^(404|503)$ ]]; then
  echo "  Got HTTP $http_code (expected 404 or 503)"
  ((pass++))
else
  echo "  ⚠ Got HTTP $http_code (expected 404 or 503)"
  ((fail++))
fi

# ──────────────────────────────────────────────────────────────
# Summary
# ──────────────────────────────────────────────────────────────

echo ""
echo "╔══════════════════════════════════════════════════════╗"
echo "║  Results: $pass passed, $fail failed, $skip skipped"
echo "╚══════════════════════════════════════════════════════╝"

[[ "$fail" -eq 0 ]] && exit 0 || exit 1
