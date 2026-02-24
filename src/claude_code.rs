//! Claude Code provider — spawns `claude` CLI subprocess in print mode.
//!
//! Mirrors the Cursor provider pattern: non-streaming and streaming chat via a
//! local CLI binary, reusing shared helpers from `cli_common`.

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

/// Max concurrent claude processes (env: CLAUDE_CODE_MAX_CONCURRENT, default: 4).
static SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| {
    let max = std::env::var("CLAUDE_CODE_MAX_CONCURRENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4usize);
    Semaphore::new(max)
});

/// Per-request timeout in seconds (env: CLAUDE_CODE_TIMEOUT_SECS, default: 300).
fn timeout_secs() -> u64 {
    std::env::var("CLAUDE_CODE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// Path to the claude binary (env: CLAUDE_CODE_BINARY, default: "claude").
fn claude_binary() -> String {
    std::env::var("CLAUDE_CODE_BINARY").unwrap_or_else(|_| "claude".into())
}

// ─── Non-streaming ──────────────────────────────────────────────────────────

/// Execute a Claude Code request and return an OpenAI-compatible JSON response.
///
/// Spawns: `claude -p <prompt> --output-format json --model <model> --max-turns 1`
///
/// Claude returns JSON like:
/// ```json
/// {"result": "...", "session_id": "...", "usage": {"input_tokens": N, "output_tokens": N}}
/// ```
pub async fn claude_code_chat(body: &Value, model: &str) -> Result<Value, GatewayError> {
    let prompt = extract_prompt(body)?;
    let binary = claude_binary();

    let permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("claude-code concurrent request limit reached".into())
    })?;

    let mut cmd = tokio::process::Command::new(&binary);
    cmd.args([
        "-p",
        &prompt,
        "--output-format",
        "json",
        "--model",
        model,
        "--max-turns",
        "1",
    ]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    debug!(binary = %binary, model = %model, "spawning claude");

    let child = cmd.spawn().map_err(|e| {
        GatewayError::ConfigError(format!(
            "failed to spawn claude ('{binary}'): {e}. Is Claude Code CLI installed?"
        ))
    })?;

    let timeout = tokio::time::Duration::from_secs(timeout_secs());
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| GatewayError::UpstreamError("claude timed out".into()))?
        .map_err(|e| GatewayError::UpstreamError(format!("claude process error: {e}")))?;

    drop(permit);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GatewayError::UpstreamError(format!(
            "claude exited with {}: {stderr}",
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Claude outputs a single JSON object
    let result_json: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        GatewayError::UpstreamError(format!("failed to parse claude output as JSON: {e}"))
    })?;

    let content = result_json["result"].as_str().unwrap_or("");
    let session_id = result_json["session_id"]
        .as_str()
        .unwrap_or("claude-unknown");

    // Extract usage if available
    let input_tokens = result_json["usage"]["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = result_json["usage"]["output_tokens"].as_u64().unwrap_or(0);

    Ok(build_openai_response_with_usage(
        content,
        model,
        session_id,
        input_tokens,
        output_tokens,
    ))
}

// ─── Streaming ──────────────────────────────────────────────────────────────

/// Execute a Claude Code request in streaming mode, returning SSE chunks.
///
/// Spawns: `claude -p <prompt> --output-format stream-json --verbose --model <model> --max-turns 1`
///
/// Claude emits NDJSON lines. Text deltas look like:
/// ```json
/// {"type": "assistant", "subtype": "text", "content_block_delta": {"type": "text_delta", "text": "..."}}
/// ```
pub async fn claude_code_chat_streaming(
    body: &Value,
    model: &str,
) -> Result<Response, GatewayError> {
    let prompt = extract_prompt(body)?;
    let binary = claude_binary();

    let permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("claude-code concurrent request limit reached".into())
    })?;

    let mut cmd = tokio::process::Command::new(&binary);
    cmd.args([
        "-p",
        &prompt,
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        model,
        "--max-turns",
        "1",
    ]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    debug!(binary = %binary, model = %model, "spawning claude (streaming)");

    let mut child = cmd.spawn().map_err(|e| {
        GatewayError::ConfigError(format!(
            "failed to spawn claude ('{binary}'): {e}. Is Claude Code CLI installed?"
        ))
    })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GatewayError::Internal("failed to capture claude stdout".into()))?;

    let model = model.to_string();
    let id = uuid::Uuid::new_v4().to_string();

    let stream = async_stream::stream! {
        let _permit = permit;
        let reader = tokio::io::BufReader::new(stdout);
        let mut lines = reader.lines();

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
                        debug!(line = %line, "skipping non-JSON line from claude");
                        continue;
                    };

                    // Check for text delta events from claude stream-json output
                    // Format: {"type": "assistant", "subtype": "text",
                    //          "content_block_delta": {"type": "text_delta", "text": "..."}}
                    if let Some(delta) = json.get("content_block_delta") {
                        if delta["type"].as_str() == Some("text_delta")
                            && let Some(text) = delta["text"].as_str()
                            && !text.is_empty()
                        {
                            let chunk = build_sse_chunk(text, &model, &id, false);
                            yield Ok::<_, std::io::Error>(chunk);
                        }
                        continue;
                    }

                    // Also handle result event (final message)
                    if json["type"].as_str() == Some("result") {
                        // Result signals end of stream; the content was already
                        // streamed via deltas above.
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
                    error!(error = %e, "error reading claude stdout");
                    break;
                }
                Err(_) => {
                    warn!("claude streaming timed out");
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

/// Check Claude Code availability by running `claude --version`.
///
/// Returns `(reachable, version_string)`.
pub async fn check_claude_code_status() -> (bool, Option<String>) {
    let binary = claude_binary();
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
                "claude --version returned non-zero"
            );
            (false, None)
        }
        Err(e) => {
            warn!(error = %e, "claude --version check failed");
            (false, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn test_parse_claude_json_output() {
        let output = json!({
            "result": "Hello! How can I help you today?",
            "session_id": "sess-abc123",
            "usage": {
                "input_tokens": 15,
                "output_tokens": 8,
            }
        });

        let content = output["result"].as_str().unwrap();
        assert_eq!(content, "Hello! How can I help you today?");

        let input_tokens = output["usage"]["input_tokens"].as_u64().unwrap();
        let output_tokens = output["usage"]["output_tokens"].as_u64().unwrap();
        assert_eq!(input_tokens, 15);
        assert_eq!(output_tokens, 8);
    }

    #[test]
    fn test_parse_claude_stream_delta() {
        let line = json!({
            "type": "assistant",
            "subtype": "text",
            "content_block_delta": {
                "type": "text_delta",
                "text": "Hello"
            }
        });

        let delta = line.get("content_block_delta").unwrap();
        assert_eq!(delta["type"].as_str(), Some("text_delta"));
        assert_eq!(delta["text"].as_str(), Some("Hello"));
    }

    #[test]
    fn test_parse_claude_stream_result() {
        let line = json!({
            "type": "result",
            "result": "Full response text",
            "session_id": "sess-xyz"
        });

        assert_eq!(line["type"].as_str(), Some("result"));
    }
}
