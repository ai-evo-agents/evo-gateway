use crate::cli_common::{build_openai_response, build_sse_chunk, extract_prompt};
use crate::error::GatewayError;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::Value;
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
