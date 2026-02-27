//! Claude Code provider — spawns `claude` CLI subprocess in print mode.
//!
//! Supports two execution backends:
//! - **Direct**: `tokio::process::Command` (original, used as fallback)
//! - **Tmux**: managed tmux sessions via `TmuxManager` (preferred when available)

use crate::cli_common::{build_openai_response_with_usage, build_sse_chunk, extract_prompt};
use crate::error::GatewayError;
use crate::tmux::{SessionMode, TmuxManager};
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
/// Routes to tmux-backed or direct subprocess execution depending on availability.
pub async fn claude_code_chat(
    body: &Value,
    model: &str,
    tmux: Option<&TmuxManager>,
) -> Result<Value, GatewayError> {
    let prompt = extract_prompt(body)?;
    let binary = claude_binary();

    if let Some(manager) = tmux {
        claude_code_chat_tmux(manager, &prompt, model, &binary).await
    } else {
        claude_code_chat_direct(&prompt, model, &binary).await
    }
}

/// Direct subprocess execution (original implementation).
async fn claude_code_chat_direct(
    prompt: &str,
    model: &str,
    binary: &str,
) -> Result<Value, GatewayError> {
    let permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("claude-code concurrent request limit reached".into())
    })?;

    let mut cmd = tokio::process::Command::new(binary);
    cmd.args([
        "-p",
        prompt,
        "--output-format",
        "json",
        "--model",
        model,
        "--max-turns",
        "1",
    ]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    debug!(binary = %binary, model = %model, "spawning claude (direct)");

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
    parse_claude_json_response(&stdout, model)
}

/// Tmux-backed execution.
async fn claude_code_chat_tmux(
    manager: &TmuxManager,
    prompt: &str,
    model: &str,
    binary: &str,
) -> Result<Value, GatewayError> {
    let _permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("claude-code concurrent request limit reached".into())
    })?;

    let command = vec![
        binary.to_string(),
        "-p".into(),
        prompt.to_string(),
        "--output-format".into(),
        "json".into(),
        "--model".into(),
        model.to_string(),
        "--max-turns".into(),
        "1".into(),
    ];

    debug!(binary = %binary, model = %model, "spawning claude (tmux)");

    let session_id = manager
        .create_session("claude-code", command, SessionMode::Ephemeral, None, None)
        .await?;

    let timeout = std::time::Duration::from_secs(timeout_secs());
    let status = manager.wait_for_completion(&session_id, timeout).await;

    let output = manager.read_output(&session_id).await.unwrap_or_default();
    let _ = manager.kill_session(&session_id).await;

    // Check for timeout
    status?;

    parse_claude_json_response(&output, model)
}

/// Parse Claude JSON output into an OpenAI-compatible response.
fn parse_claude_json_response(stdout: &str, model: &str) -> Result<Value, GatewayError> {
    // Claude outputs a single JSON object
    let result_json: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        GatewayError::UpstreamError(format!("failed to parse claude output as JSON: {e}"))
    })?;

    let content = result_json["result"].as_str().unwrap_or("");
    let session_id = result_json["session_id"]
        .as_str()
        .unwrap_or("claude-unknown");

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
/// Routes to tmux-backed or direct subprocess execution depending on availability.
pub async fn claude_code_chat_streaming(
    body: &Value,
    model: &str,
    tmux: Option<&TmuxManager>,
) -> Result<Response, GatewayError> {
    let prompt = extract_prompt(body)?;
    let binary = claude_binary();

    if let Some(manager) = tmux {
        claude_code_chat_streaming_tmux(manager, &prompt, model, &binary).await
    } else {
        claude_code_chat_streaming_direct(&prompt, model, &binary).await
    }
}

/// Direct subprocess streaming (original implementation).
async fn claude_code_chat_streaming_direct(
    prompt: &str,
    model: &str,
    binary: &str,
) -> Result<Response, GatewayError> {
    let permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("claude-code concurrent request limit reached".into())
    })?;

    let mut cmd = tokio::process::Command::new(binary);
    cmd.args([
        "-p",
        prompt,
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

    debug!(binary = %binary, model = %model, "spawning claude (streaming direct)");

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

                    if json["type"].as_str() == Some("result") {
                        let chunk = build_sse_chunk("", &model, &id, true);
                        yield Ok(chunk);
                        yield Ok("data: [DONE]\n\n".to_string());
                        break;
                    }
                }
                Ok(Ok(None)) => {
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

        let _ = child.kill().await;
    };

    build_sse_response(stream)
}

/// Tmux-backed streaming execution.
async fn claude_code_chat_streaming_tmux(
    manager: &TmuxManager,
    prompt: &str,
    model: &str,
    binary: &str,
) -> Result<Response, GatewayError> {
    let permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("claude-code concurrent request limit reached".into())
    })?;

    let command = vec![
        binary.to_string(),
        "-p".into(),
        prompt.to_string(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--model".into(),
        model.to_string(),
        "--max-turns".into(),
        "1".into(),
    ];

    debug!(binary = %binary, model = %model, "spawning claude (streaming tmux)");

    let session_id = manager
        .create_session("claude-code", command, SessionMode::Ephemeral, None, None)
        .await?;

    let mut rx = manager.subscribe_output(&session_id).await?;
    let model = model.to_string();
    let id = uuid::Uuid::new_v4().to_string();
    let sid = session_id.clone();
    // Clone manager fields we need for the stream closure
    // We need the session_id to kill later, but manager isn't Send-safe to move
    // Instead we rely on ephemeral cleanup

    let stream = async_stream::stream! {
        let _permit = permit;

        loop {
            match rx.recv().await {
                Ok(event) => {
                    if event.is_final {
                        let chunk = build_sse_chunk("", &model, &id, true);
                        yield Ok::<_, std::io::Error>(chunk);
                        yield Ok("data: [DONE]\n\n".to_string());
                        break;
                    }

                    let line = &event.line;
                    if line.trim().is_empty() {
                        continue;
                    }

                    let Ok(json): Result<Value, _> = serde_json::from_str(line) else {
                        continue;
                    };

                    if let Some(delta) = json.get("content_block_delta") {
                        if delta["type"].as_str() == Some("text_delta")
                            && let Some(text) = delta["text"].as_str()
                            && !text.is_empty()
                        {
                            let chunk = build_sse_chunk(text, &model, &id, false);
                            yield Ok(chunk);
                        }
                        continue;
                    }

                    if json["type"].as_str() == Some("result") {
                        let chunk = build_sse_chunk("", &model, &id, true);
                        yield Ok(chunk);
                        yield Ok("data: [DONE]\n\n".to_string());
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, session = %sid, "output subscriber lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    let chunk = build_sse_chunk("", &model, &id, true);
                    yield Ok(chunk);
                    yield Ok("data: [DONE]\n\n".to_string());
                    break;
                }
            }
        }
    };

    build_sse_response(stream)
}

/// Build an SSE response from an async stream of string results.
pub(crate) fn build_sse_response(
    stream: impl tokio_stream::Stream<Item = Result<String, std::io::Error>> + Send + 'static,
) -> Result<Response, GatewayError> {
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
