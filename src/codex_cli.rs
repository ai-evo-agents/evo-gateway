//! Codex CLI provider — spawns `codex` CLI subprocess in exec mode.
//!
//! Mirrors the Claude Code provider pattern: non-streaming and streaming chat
//! via a local CLI binary, reusing shared helpers from `cli_common`.

use crate::cli_common::{build_openai_response_with_usage, build_sse_chunk, extract_prompt};
use crate::error::GatewayError;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::Value;
use std::sync::LazyLock;
use tokio::io::AsyncBufReadExt;
use tokio::sync::Semaphore;
use tracing::{debug, error, warn};

/// Max concurrent codex processes (env: CODEX_CLI_MAX_CONCURRENT, default: 4).
static SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| {
    let max = std::env::var("CODEX_CLI_MAX_CONCURRENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4usize);
    Semaphore::new(max)
});

/// Per-request timeout in seconds (env: CODEX_CLI_TIMEOUT_SECS, default: 300).
fn timeout_secs() -> u64 {
    std::env::var("CODEX_CLI_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// Path to the codex binary (env: CODEX_CLI_BINARY, default: "codex").
fn codex_binary() -> String {
    std::env::var("CODEX_CLI_BINARY").unwrap_or_else(|_| "codex".into())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Extract assistant text from a Codex `item.completed` event.
///
/// Handles both old format (`assistant_message` with `content[].text` blocks)
/// and new format (`agent_message` with top-level `text` field).
fn extract_assistant_text(item: &Value) -> Option<String> {
    // New format: agent_message has top-level "text" field
    if let Some(text) = item["text"].as_str()
        && !text.is_empty()
    {
        return Some(text.to_string());
    }
    // Old format: assistant_message has content[] array of text blocks
    if let Some(content) = item["content"].as_array() {
        let texts: Vec<&str> = content
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect();
        if !texts.is_empty() {
            return Some(texts.join(""));
        }
    }
    None
}

/// Parse full Codex NDJSON output, returning `(assistant_text, input_tokens, output_tokens)`.
fn parse_codex_output(stdout: &str) -> Result<(String, u64, u64), GatewayError> {
    let mut assistant_text: Option<String> = None;
    let mut input_tokens: u64 = 0;
    let mut output_tokens: u64 = 0;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(json): Result<Value, _> = serde_json::from_str(line) else {
            continue;
        };

        let event_type = json["type"].as_str().unwrap_or("");

        // item.completed with assistant_message or agent_message → extract text
        if event_type == "item.completed"
            && let Some(item) = json.get("item")
            && matches!(
                item["type"].as_str(),
                Some("assistant_message") | Some("agent_message")
            )
        {
            assistant_text = extract_assistant_text(item);
        }

        // turn.completed → extract usage
        if event_type == "turn.completed"
            && let Some(usage) = json.get("usage")
        {
            input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
            output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
        }

        // turn.failed → extract error message
        if event_type == "turn.failed" {
            let error_msg = json["error"]["message"]
                .as_str()
                .unwrap_or("codex turn failed (unknown reason)");
            return Err(GatewayError::UpstreamError(format!(
                "codex turn.failed: {error_msg}"
            )));
        }
    }

    let text = assistant_text.ok_or_else(|| {
        GatewayError::UpstreamError("no assistant/agent message found in codex output".into())
    })?;

    Ok((text, input_tokens, output_tokens))
}

// ─── Non-streaming ──────────────────────────────────────────────────────────

/// Execute a Codex CLI request and return an OpenAI-compatible JSON response.
///
/// Spawns: `codex exec --json --ephemeral --full-auto -m <model> "<prompt>"`
pub async fn codex_cli_chat(body: &Value, model: &str) -> Result<Value, GatewayError> {
    let prompt = extract_prompt(body)?;
    let binary = codex_binary();

    let permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("codex-cli concurrent request limit reached".into())
    })?;

    let mut cmd = tokio::process::Command::new(&binary);
    let mut args = vec!["exec", "--json", "--ephemeral", "--full-auto"];
    // Only pass -m flag if model is specified and not "default"
    if !model.is_empty() && model != "default" {
        args.push("-m");
        args.push(model);
    }
    args.push(&prompt);
    cmd.args(&args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    debug!(binary = %binary, model = %model, "spawning codex");

    let child = cmd.spawn().map_err(|e| {
        GatewayError::ConfigError(format!(
            "failed to spawn codex ('{binary}'): {e}. Is Codex CLI installed?"
        ))
    })?;

    let timeout = tokio::time::Duration::from_secs(timeout_secs());
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| GatewayError::UpstreamError("codex timed out".into()))?
        .map_err(|e| GatewayError::UpstreamError(format!("codex process error: {e}")))?;

    drop(permit);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GatewayError::UpstreamError(format!(
            "codex exited with {}: {stderr}",
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (text, input_tokens, output_tokens) = parse_codex_output(&stdout)?;

    Ok(build_openai_response_with_usage(
        &text,
        model,
        "codex-exec",
        input_tokens,
        output_tokens,
    ))
}

// ─── Streaming ──────────────────────────────────────────────────────────────

/// Execute a Codex CLI request in streaming mode, returning SSE chunks.
///
/// Spawns the same command and streams NDJSON line-by-line, emitting text
/// deltas from `item.updated` events with `assistant_message` type.
pub async fn codex_cli_chat_streaming(body: &Value, model: &str) -> Result<Response, GatewayError> {
    let prompt = extract_prompt(body)?;
    let binary = codex_binary();

    let permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("codex-cli concurrent request limit reached".into())
    })?;

    let mut cmd = tokio::process::Command::new(&binary);
    let mut args = vec!["exec", "--json", "--ephemeral", "--full-auto"];
    // Only pass -m flag if model is specified and not "default"
    if !model.is_empty() && model != "default" {
        args.push("-m");
        args.push(model);
    }
    args.push(&prompt);
    cmd.args(&args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    debug!(binary = %binary, model = %model, "spawning codex (streaming)");

    let mut child = cmd.spawn().map_err(|e| {
        GatewayError::ConfigError(format!(
            "failed to spawn codex ('{binary}'): {e}. Is Codex CLI installed?"
        ))
    })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GatewayError::Internal("failed to capture codex stdout".into()))?;

    let model = model.to_string();
    let id = uuid::Uuid::new_v4().to_string();

    let stream = async_stream::stream! {
        let _permit = permit;
        let reader = tokio::io::BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut accumulated = String::new();

        loop {
            let line_result = tokio::time::timeout(
                tokio::time::Duration::from_secs(timeout_secs()),
                lines.next_line(),
            )
            .await;

            match line_result {
                Ok(Ok(Some(line))) => {
                    if line.trim().is_empty() {
                        continue;
                    }

                    let Ok(json): Result<Value, _> = serde_json::from_str(&line) else {
                        debug!(line = %line, "skipping non-JSON line from codex");
                        continue;
                    };

                    let event_type = json["type"].as_str().unwrap_or("");

                    // item.updated with assistant/agent message → accumulated text delta
                    if event_type == "item.updated"
                        && let Some(item) = json.get("item")
                        && matches!(item["type"].as_str(), Some("assistant_message") | Some("agent_message"))
                        && let Some(full_text) = extract_assistant_text(item)
                        && full_text.len() > accumulated.len()
                    {
                        let delta = &full_text[accumulated.len()..];
                        let chunk = build_sse_chunk(delta, &model, &id, false);
                        accumulated = full_text;
                        yield Ok::<_, std::io::Error>(chunk);
                        continue;
                    }

                    // item.completed with assistant/agent message → final delta
                    if event_type == "item.completed"
                        && let Some(item) = json.get("item")
                        && matches!(item["type"].as_str(), Some("assistant_message") | Some("agent_message"))
                        && let Some(full_text) = extract_assistant_text(item)
                        && full_text.len() > accumulated.len()
                    {
                        let delta = &full_text[accumulated.len()..];
                        let chunk = build_sse_chunk(delta, &model, &id, false);
                        yield Ok::<_, std::io::Error>(chunk);
                        continue;
                    }

                    // turn.failed → send error as content and finish
                    if event_type == "turn.failed" {
                        let error_msg = json["error"]["message"]
                            .as_str()
                            .unwrap_or("codex turn failed");
                        let chunk = build_sse_chunk(&format!("[Error: {error_msg}]"), &model, &id, false);
                        yield Ok::<_, std::io::Error>(chunk);
                        let done_chunk = build_sse_chunk("", &model, &id, true);
                        yield Ok(done_chunk);
                        yield Ok("data: [DONE]\n\n".to_string());
                        break;
                    }

                    // turn.completed → finish
                    if event_type == "turn.completed" {
                        let chunk = build_sse_chunk("", &model, &id, true);
                        yield Ok(chunk);
                        yield Ok("data: [DONE]\n\n".to_string());
                        break;
                    }
                }
                Ok(Ok(None)) => {
                    // EOF — send finish
                    let chunk = build_sse_chunk("", &model, &id, true);
                    yield Ok(chunk);
                    yield Ok("data: [DONE]\n\n".to_string());
                    break;
                }
                Ok(Err(e)) => {
                    error!(error = %e, "error reading codex stdout");
                    break;
                }
                Err(_) => {
                    warn!("codex streaming timed out");
                    break;
                }
            }
        }

        // Ensure child is cleaned up
        let _ = child.kill().await;
    };

    let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new({
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(32);
        tokio::spawn(async move {
            use tokio_stream::StreamExt;
            let mut stream = std::pin::pin!(stream);
            while let Some(item) = stream.next().await {
                if tx.send(item).await.is_err() {
                    break;
                }
            }
        });
        rx
    }));

    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("cache-control", HeaderValue::from_static("no-cache"));
    headers.insert("connection", HeaderValue::from_static("keep-alive"));

    let mut resp = Response::new(body);
    *resp.status_mut() = StatusCode::OK;
    *resp.headers_mut() = headers;

    Ok(resp)
}

// ─── Health check ───────────────────────────────────────────────────────────

/// Check Codex CLI availability by running `codex --version`.
///
/// Returns `(reachable, version_string)`.
pub async fn check_codex_cli_status() -> (bool, Option<String>) {
    let binary = codex_binary();
    let result = tokio::process::Command::new(&binary)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let version = stdout.trim().to_string();
            let version = if version.is_empty() {
                None
            } else {
                Some(version)
            };
            (true, version)
        }
        Ok(output) => {
            warn!(
                status = %output.status,
                "codex --version returned non-zero"
            );
            (false, None)
        }
        Err(e) => {
            warn!(error = %e, "codex --version check failed");
            (false, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_assistant_text() {
        let item = json!({
            "type": "assistant_message",
            "content": [
                {"type": "text", "text": "Hello, "},
                {"type": "text", "text": "world!"}
            ]
        });
        let text = extract_assistant_text(&item).unwrap();
        assert_eq!(text, "Hello, world!");
    }

    #[test]
    fn test_extract_assistant_text_empty() {
        let item = json!({
            "type": "assistant_message",
            "content": []
        });
        assert!(extract_assistant_text(&item).is_none());
    }

    #[test]
    fn test_extract_assistant_text_agent_message() {
        let item = json!({
            "id": "item_1",
            "type": "agent_message",
            "text": "Four"
        });
        let text = extract_assistant_text(&item).unwrap();
        assert_eq!(text, "Four");
    }

    #[test]
    fn test_parse_codex_output_assistant_message() {
        let stdout = r#"{"type":"item.completed","item":{"type":"assistant_message","content":[{"type":"text","text":"Hello!"}]}}
{"type":"turn.completed","usage":{"input_tokens":42,"output_tokens":7}}"#;
        let (text, input, output) = parse_codex_output(stdout).unwrap();
        assert_eq!(text, "Hello!");
        assert_eq!(input, 42);
        assert_eq!(output, 7);
    }

    #[test]
    fn test_parse_codex_output_agent_message() {
        let stdout = r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"Four"}}
{"type":"turn.completed","usage":{"input_tokens":14879,"cached_input_tokens":3456,"output_tokens":38}}"#;
        let (text, input, output) = parse_codex_output(stdout).unwrap();
        assert_eq!(text, "Four");
        assert_eq!(input, 14879);
        assert_eq!(output, 38);
    }

    #[test]
    fn test_parse_codex_output_no_message() {
        let stdout = r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":0}}"#;
        let result = parse_codex_output(stdout);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_codex_output_turn_failed() {
        let stdout = r#"{"type":"turn.failed","error":{"message":"model not supported"}}"#;
        let result = parse_codex_output(stdout);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("turn.failed"));
    }
}
