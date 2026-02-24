use crate::error::GatewayError;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::{Value, json};
use std::sync::LazyLock;
use tokio::io::AsyncBufReadExt;
use tokio::sync::Semaphore;
use tracing::{debug, error, warn};

/// Max concurrent cursor-agent processes (env: CURSOR_MAX_CONCURRENT, default: 4).
static SEMAPHORE: LazyLock<Semaphore> = LazyLock::new(|| {
    let max = std::env::var("CURSOR_MAX_CONCURRENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4usize);
    Semaphore::new(max)
});

/// Per-request timeout in seconds (env: CURSOR_TIMEOUT_SECS, default: 120).
fn timeout_secs() -> u64 {
    std::env::var("CURSOR_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120)
}

/// Path to the cursor-agent binary (env: CURSOR_AGENT_BINARY, default: "cursor-agent").
fn cursor_binary() -> String {
    std::env::var("CURSOR_AGENT_BINARY").unwrap_or_else(|_| "cursor-agent".into())
}

// ─── Prompt extraction ──────────────────────────────────────────────────────

/// Concatenate the OpenAI messages array into a single prompt string.
///
/// - `system` → `[System]: <content>`
/// - `user` → `<content>`
/// - `assistant` → `[Previous assistant]: <content>`
pub fn extract_prompt(body: &Value) -> Result<String, GatewayError> {
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| GatewayError::ConfigError("missing 'messages' array".into()))?;

    if messages.is_empty() {
        return Err(GatewayError::ConfigError(
            "'messages' array is empty".into(),
        ));
    }

    let mut parts = Vec::with_capacity(messages.len());

    for msg in messages {
        let role = msg["role"].as_str().unwrap_or("user");
        let content = msg["content"].as_str().unwrap_or("");
        match role {
            "system" => parts.push(format!("[System]: {content}")),
            "assistant" => parts.push(format!("[Previous assistant]: {content}")),
            _ => parts.push(content.to_string()),
        }
    }

    Ok(parts.join("\n\n"))
}

// ─── Response builders ──────────────────────────────────────────────────────

/// Build an OpenAI-compatible chat.completion response from cursor-agent output.
pub fn build_openai_response(content: &str, model: &str, request_id: &str) -> Value {
    json!({
        "id": format!("chatcmpl-{request_id}"),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
            },
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0,
        },
    })
}

/// Build a single SSE chunk in OpenAI streaming format.
///
/// If `finish` is true, produces a `finish_reason: "stop"` delta with no content.
pub fn build_sse_chunk(content: &str, model: &str, id: &str, finish: bool) -> String {
    let data = if finish {
        json!({
            "id": format!("chatcmpl-{id}"),
            "object": "chat.completion.chunk",
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop",
            }],
        })
    } else {
        json!({
            "id": format!("chatcmpl-{id}"),
            "object": "chat.completion.chunk",
            "model": model,
            "choices": [{
                "index": 0,
                "delta": { "content": content },
                "finish_reason": null,
            }],
        })
    };
    format!("data: {data}\n\n")
}

// ─── Non-streaming ──────────────────────────────────────────────────────────

/// Execute a cursor-agent request and return an OpenAI-compatible JSON response.
pub async fn cursor_chat(body: &Value, model: &str) -> Result<Value, GatewayError> {
    let prompt = extract_prompt(body)?;
    let binary = cursor_binary();

    let permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("cursor concurrent request limit reached".into())
    })?;

    let mut cmd = tokio::process::Command::new(&binary);
    cmd.args([
        "--print",
        "--output-format",
        "json",
        "--model",
        model,
        "--mode",
        "ask",
        "--trust",
        &prompt,
    ]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    debug!(binary = %binary, model = %model, "spawning cursor-agent");

    let child = cmd.spawn().map_err(|e| {
        GatewayError::ConfigError(format!(
            "failed to spawn cursor-agent ('{binary}'): {e}. Is it installed?"
        ))
    })?;

    let timeout = tokio::time::Duration::from_secs(timeout_secs());
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| GatewayError::UpstreamError("cursor-agent timed out".into()))?
        .map_err(|e| GatewayError::UpstreamError(format!("cursor-agent process error: {e}")))?;

    drop(permit);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GatewayError::UpstreamError(format!(
            "cursor-agent exited with {}: {stderr}",
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // cursor-agent may output multiple JSON lines; find the result line
    let result_json = find_result_line(&stdout)?;

    if result_json["is_error"].as_bool().unwrap_or(false) {
        return Err(GatewayError::UpstreamError(format!(
            "cursor-agent reported error: {}",
            result_json["result"].as_str().unwrap_or("unknown")
        )));
    }

    let content = result_json["result"].as_str().unwrap_or("");
    let request_id = result_json["request_id"]
        .as_str()
        .unwrap_or("cursor-unknown");

    Ok(build_openai_response(content, model, request_id))
}

// ─── Streaming ──────────────────────────────────────────────────────────────

/// Execute a cursor-agent request in streaming mode, returning SSE chunks.
pub async fn cursor_chat_streaming(body: &Value, model: &str) -> Result<Response, GatewayError> {
    let prompt = extract_prompt(body)?;
    let binary = cursor_binary();

    let permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("cursor concurrent request limit reached".into())
    })?;

    let mut cmd = tokio::process::Command::new(&binary);
    cmd.args([
        "--print",
        "--output-format",
        "stream-json",
        "--stream-partial-output",
        "--model",
        model,
        "--mode",
        "ask",
        "--trust",
        &prompt,
    ]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    debug!(binary = %binary, model = %model, "spawning cursor-agent (streaming)");

    let mut child = cmd.spawn().map_err(|e| {
        GatewayError::ConfigError(format!(
            "failed to spawn cursor-agent ('{binary}'): {e}. Is it installed?"
        ))
    })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GatewayError::Internal("failed to capture cursor-agent stdout".into()))?;

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
                        debug!(line = %line, "skipping non-JSON line from cursor-agent");
                        continue;
                    };

                    // Check for partial output or final result
                    let msg_type = json["type"].as_str().unwrap_or("");

                    if msg_type == "result" {
                        // Final result — compute the remaining delta
                        if let Some(full) = json["result"].as_str()
                            && full.len() > accumulated.len()
                        {
                            let delta = &full[accumulated.len()..];
                            let chunk = build_sse_chunk(delta, &model, &id, false);
                            yield Ok::<_, std::io::Error>(chunk);
                        }
                        // Send finish chunk
                        let chunk = build_sse_chunk("", &model, &id, true);
                        yield Ok(chunk);
                        yield Ok("data: [DONE]\n\n".to_string());
                        break;
                    }

                    // Partial output: compute delta from accumulated content
                    if let Some(partial) = json["result"].as_str().or(json["content"].as_str())
                        && partial.len() > accumulated.len()
                    {
                        let delta = &partial[accumulated.len()..];
                        let chunk = build_sse_chunk(delta, &model, &id, false);
                        accumulated = partial.to_string();
                        yield Ok(chunk);
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
                    error!(error = %e, "error reading cursor-agent stdout");
                    break;
                }
                Err(_) => {
                    warn!("cursor-agent streaming timed out");
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

/// Check cursor-agent authentication status by running `cursor-agent status`.
pub async fn check_cursor_status() -> (bool, Option<String>) {
    let binary = cursor_binary();
    let result = tokio::process::Command::new(&binary)
        .arg("status")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let reachable = stdout.contains("Logged in");
            let email = stdout
                .lines()
                .find(|l| l.contains('@'))
                .map(|l| l.trim().to_string());
            (reachable, email)
        }
        Err(e) => {
            warn!(error = %e, "cursor-agent status check failed");
            (false, None)
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Find the JSON line with `"type":"result"` from cursor-agent stdout.
fn find_result_line(stdout: &str) -> Result<Value, GatewayError> {
    for line in stdout.lines() {
        if let Ok(json) = serde_json::from_str::<Value>(line)
            && json["type"].as_str() == Some("result")
        {
            return Ok(json);
        }
    }
    // Fall back to trying the last non-empty line
    let last_line = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    serde_json::from_str(last_line).map_err(|e| {
        GatewayError::UpstreamError(format!("failed to parse cursor-agent output as JSON: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_prompt_basic() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hello"},
            ]
        });
        let prompt = extract_prompt(&body).unwrap();
        assert!(prompt.contains("[System]: You are helpful."));
        assert!(prompt.contains("Hello"));
    }

    #[test]
    fn test_extract_prompt_with_assistant() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "Hi"},
                {"role": "assistant", "content": "Hello!"},
                {"role": "user", "content": "How are you?"},
            ]
        });
        let prompt = extract_prompt(&body).unwrap();
        assert!(prompt.contains("[Previous assistant]: Hello!"));
        assert!(prompt.contains("How are you?"));
    }

    #[test]
    fn test_extract_prompt_empty() {
        let body = json!({"messages": []});
        assert!(extract_prompt(&body).is_err());
    }

    #[test]
    fn test_extract_prompt_missing() {
        let body = json!({"model": "test"});
        assert!(extract_prompt(&body).is_err());
    }

    #[test]
    fn test_build_openai_response() {
        let resp = build_openai_response("Hello world", "auto", "req-123");
        assert_eq!(resp["object"], "chat.completion");
        assert_eq!(resp["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(resp["choices"][0]["finish_reason"], "stop");
        assert_eq!(resp["model"], "auto");
    }

    #[test]
    fn test_build_sse_chunk_content() {
        let chunk = build_sse_chunk("Hello", "auto", "id1", false);
        assert!(chunk.starts_with("data: "));
        assert!(chunk.contains("\"content\":\"Hello\""));
        assert!(chunk.contains("\"finish_reason\":null"));
    }

    #[test]
    fn test_build_sse_chunk_finish() {
        let chunk = build_sse_chunk("", "auto", "id1", true);
        assert!(chunk.contains("\"finish_reason\":\"stop\""));
        assert!(!chunk.contains("\"content\""));
    }

    #[test]
    fn test_find_result_line() {
        let stdout = r#"{"type":"progress","content":"partial"}
{"type":"result","subtype":"success","is_error":false,"result":"final answer","request_id":"r1"}"#;
        let result = find_result_line(stdout).unwrap();
        assert_eq!(result["type"], "result");
        assert_eq!(result["result"], "final answer");
    }

    #[test]
    fn test_find_result_line_fallback() {
        let stdout = r#"{"result":"only line","request_id":"r2"}"#;
        let result = find_result_line(stdout).unwrap();
        assert_eq!(result["result"], "only line");
    }
}
