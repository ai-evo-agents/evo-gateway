//! OpenAI Codex Responses API provider — direct HTTP with OAuth/bearer token auth.
//!
//! Supports both WebSocket and SSE transports (auto mode tries WS first, falls back to SSE).
//! Converts OpenAI chat/completions format ↔ Responses API format transparently.
//!
//! Inspired by <https://github.com/zeroclaw-labs/zeroclaw/blob/979b5fa/src/providers/openai_codex.rs>.

use crate::cli_common::{build_openai_response, build_sse_chunk};
use crate::error::GatewayError;
use crate::state::{AppState, ProviderPool};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message as WsMessage,
        client::IntoClientRequest,
        http::{
            HeaderValue as WsHeaderValue,
            header::{AUTHORIZATION, USER_AGENT},
        },
    },
};

// ─── Constants ──────────────────────────────────────────────────────────────

const DEFAULT_RESPONSES_PATH: &str = "/responses";
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const WS_SEND_TIMEOUT: Duration = Duration::from_secs(15);
const WS_READ_TIMEOUT: Duration = Duration::from_secs(60);

// ─── Transport ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexTransport {
    Auto,
    WebSocket,
    Sse,
}

/// Resolve transport mode from env var `EVO_CODEX_AUTH_TRANSPORT`.
fn resolve_transport() -> CodexTransport {
    match std::env::var("EVO_CODEX_AUTH_TRANSPORT")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "websocket" | "ws" => CodexTransport::WebSocket,
        "sse" | "http" => CodexTransport::Sse,
        _ => CodexTransport::Auto,
    }
}

// ─── WebSocket error types ──────────────────────────────────────────────────

#[derive(Debug)]
enum WsRequestError {
    /// Connection-level failure — safe to fall back to SSE.
    TransportUnavailable(String),
    /// Stream-level failure after connection was established.
    Stream(String),
}

impl From<WsRequestError> for GatewayError {
    fn from(e: WsRequestError) -> Self {
        match e {
            WsRequestError::TransportUnavailable(msg) | WsRequestError::Stream(msg) => {
                GatewayError::UpstreamError(msg)
            }
        }
    }
}

// ─── Request/Response types (Responses API) ─────────────────────────────────

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<ResponsesInput>,
    instructions: String,
    store: bool,
    stream: bool,
    text: ResponsesTextOptions,
    reasoning: ResponsesReasoningOptions,
    include: Vec<String>,
    tool_choice: String,
    parallel_tool_calls: bool,
}

#[derive(Debug, Serialize)]
struct ResponsesInput {
    role: String,
    content: Vec<ResponsesInputContent>,
}

#[derive(Debug, Serialize)]
struct ResponsesInputContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResponsesTextOptions {
    verbosity: String,
}

#[derive(Debug, Serialize)]
struct ResponsesReasoningOptions {
    effort: String,
    summary: String,
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    output: Vec<ResponsesOutput>,
    #[serde(default)]
    output_text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesOutput {
    #[serde(default)]
    content: Vec<ResponsesContent>,
}

#[derive(Debug, Deserialize)]
struct ResponsesContent {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

// ─── Message conversion ─────────────────────────────────────────────────────

/// Convert OpenAI chat messages into Responses API format.
///
/// - `system` messages → collected into `instructions`
/// - `user` messages → `input` with `input_text` content
/// - `assistant` messages → `input` with `output_text` content
fn build_responses_input(body: &Value) -> (String, Vec<ResponsesInput>) {
    let messages = body["messages"].as_array();
    let mut system_parts: Vec<String> = Vec::new();
    let mut input: Vec<ResponsesInput> = Vec::new();

    if let Some(messages) = messages {
        for msg in messages {
            let role = msg["role"].as_str().unwrap_or("user");
            let content = msg["content"].as_str().unwrap_or("").to_string();

            match role {
                "system" => system_parts.push(content),
                "user" => {
                    input.push(ResponsesInput {
                        role: "user".to_string(),
                        content: vec![ResponsesInputContent {
                            kind: "input_text".to_string(),
                            text: Some(content),
                        }],
                    });
                }
                "assistant" => {
                    input.push(ResponsesInput {
                        role: "assistant".to_string(),
                        content: vec![ResponsesInputContent {
                            kind: "output_text".to_string(),
                            text: Some(content),
                        }],
                    });
                }
                _ => {} // ignore tool, function, etc.
            }
        }
    }

    let instructions = if system_parts.is_empty() {
        "You are a helpful coding assistant.".to_string()
    } else {
        system_parts.join("\n\n")
    };

    (instructions, input)
}

// ─── Reasoning effort clamping ──────────────────────────────────────────────

/// Normalize model ID — strip any `provider/` prefix.
fn normalize_model_id(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// Clamp reasoning effort to what the model actually supports.
fn clamp_reasoning_effort(model: &str, effort: &str) -> String {
    let id = normalize_model_id(model);

    // gpt-5-codex supports only low|medium|high
    if id == "gpt-5-codex" {
        return match effort {
            "low" | "medium" | "high" => effort.to_string(),
            "minimal" => "low".to_string(),
            _ => "high".to_string(),
        };
    }
    if (id.starts_with("gpt-5.2") || id.starts_with("gpt-5.3")) && effort == "minimal" {
        return "low".to_string();
    }
    if id.starts_with("gpt-5-codex") && effort == "xhigh" {
        return "high".to_string();
    }
    if id == "gpt-5.1" && effort == "xhigh" {
        return "high".to_string();
    }
    if id == "gpt-5.1-codex-mini" {
        return if effort == "high" || effort == "xhigh" {
            "high".to_string()
        } else {
            "medium".to_string()
        };
    }
    effort.to_string()
}

/// Resolve reasoning effort from request body or env var, with model clamping.
fn resolve_reasoning_effort(model: &str, body: &Value) -> String {
    let from_body = body
        .get("model_reasoning_effort")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let from_env = std::env::var("EVO_CODEX_AUTH_REASONING_EFFORT").ok();

    let raw = from_body.or(from_env).unwrap_or_else(|| "high".to_string());

    clamp_reasoning_effort(model, &raw)
}

// ─── SSE parsing helpers ────────────────────────────────────────────────────

/// Extract text from a single SSE stream event.
fn extract_stream_event_text(event: &Value, saw_delta: bool) -> Option<String> {
    let event_type = event.get("type").and_then(Value::as_str);
    match event_type {
        Some("response.output_text.delta") => {
            event.get("delta").and_then(Value::as_str).and_then(|s| {
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            })
        }
        Some("response.output_text.done") if !saw_delta => {
            event.get("text").and_then(Value::as_str).and_then(|s| {
                if s.is_empty() {
                    None
                } else {
                    Some(s.to_string())
                }
            })
        }
        Some("response.completed" | "response.done") => event
            .get("response")
            .and_then(|v| serde_json::from_value::<ResponsesResponse>(v.clone()).ok())
            .and_then(|r| extract_responses_text(&r)),
        _ => None,
    }
}

/// Extract error message from stream events.
fn extract_stream_error(event: &Value) -> Option<String> {
    let event_type = event.get("type").and_then(Value::as_str);

    if event_type == Some("error") {
        return event
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| event.get("code").and_then(Value::as_str))
            .or_else(|| {
                event
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
            })
            .map(|s| s.to_string());
    }

    if event_type == Some("response.failed") {
        return event
            .get("response")
            .and_then(|r| r.get("error"))
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map(|s| s.to_string());
    }

    None
}

/// Extract final text from a non-streaming Responses API response.
fn extract_responses_text(response: &ResponsesResponse) -> Option<String> {
    // Prefer top-level output_text
    if let Some(ref text) = response.output_text
        && !text.trim().is_empty()
    {
        return Some(text.clone());
    }

    // Try nested output → content with output_text type
    for item in &response.output {
        for content in &item.content {
            if content.kind.as_deref() == Some("output_text")
                && let Some(ref text) = content.text
                && !text.trim().is_empty()
            {
                return Some(text.clone());
            }
        }
    }

    // Fallback: any content with text
    for item in &response.output {
        for content in &item.content {
            if let Some(ref text) = content.text
                && !text.trim().is_empty()
            {
                return Some(text.clone());
            }
        }
    }

    None
}

/// Parse accumulated SSE text body into final response text.
fn parse_sse_body(body: &str) -> Result<Option<String>, GatewayError> {
    let mut saw_delta = false;
    let mut delta_accumulator = String::new();
    let mut fallback_text: Option<String> = None;

    for chunk in body.split("\n\n") {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            continue;
        }

        // Collect all `data:` lines in this chunk
        let data_lines: Vec<&str> = chunk
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(|d| d.trim()))
            .collect();

        if data_lines.is_empty() {
            continue;
        }

        let joined = data_lines.join("\n");
        let trimmed = joined.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            continue;
        }

        // Try parsing as a single JSON object first
        let events: Vec<Value> = if let Ok(event) = serde_json::from_str::<Value>(trimmed) {
            vec![event]
        } else {
            // Fall back to parsing each data line individually
            data_lines
                .iter()
                .filter(|l| !l.is_empty() && *l != &"[DONE]")
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .collect()
        };

        for event in events {
            if let Some(message) = extract_stream_error(&event) {
                return Err(GatewayError::UpstreamError(format!(
                    "Codex Responses API error: {message}"
                )));
            }

            if let Some(text) = extract_stream_event_text(&event, saw_delta) {
                let event_type = event.get("type").and_then(Value::as_str);
                if event_type == Some("response.output_text.delta") {
                    saw_delta = true;
                    delta_accumulator.push_str(&text);
                } else if fallback_text.is_none() {
                    fallback_text = Some(text);
                }
            }
        }
    }

    if saw_delta && !delta_accumulator.is_empty() {
        return Ok(Some(delta_accumulator));
    }
    Ok(fallback_text)
}

// ─── Token resolution ───────────────────────────────────────────────────────

/// Resolve bearer token: try DB-stored OAuth token first, then api_key_envs.
async fn resolve_bearer_token(
    pool: &ProviderPool,
) -> Result<(String, Option<String>), GatewayError> {
    // Try DB-stored OAuth token (always from $HOME/.evo-gateway/gateway.db)
    if let Ok(db) = crate::db::init_codex_auth_db().await
        && let Ok(conn) = db.connect()
        && let Ok(Some((token, account_id))) = crate::db::get_codex_auth_token(&conn).await
    {
        return Ok((token, account_id));
    }

    // Fall back to api_key_envs
    if let Some(token) = pool.token_pool.next_token() {
        return Ok((token.to_string(), None));
    }

    Err(GatewayError::ConfigError(
        "No codex-auth token available. Run `evo-gateway auth codex-auth` or set api_key_envs."
            .into(),
    ))
}

// ─── Responses URL building ─────────────────────────────────────────────────

/// Build the responses endpoint URL from the provider's base_url.
fn build_responses_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/responses") {
        base.to_string()
    } else {
        format!("{base}{DEFAULT_RESPONSES_PATH}")
    }
}

/// Build WebSocket URL from the HTTPS responses URL.
fn build_ws_url(responses_url: &str, model: &str) -> Result<String, WsRequestError> {
    let mut url = reqwest::Url::parse(responses_url)
        .map_err(|e| WsRequestError::TransportUnavailable(format!("invalid responses URL: {e}")))?;

    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        other => {
            return Err(WsRequestError::TransportUnavailable(format!(
                "unsupported URL scheme for WebSocket: {other}"
            )));
        }
    };

    url.set_scheme(scheme).map_err(|()| {
        WsRequestError::TransportUnavailable("failed to set WebSocket URL scheme".into())
    })?;

    if !url.query_pairs().any(|(k, _)| k == "model") {
        url.query_pairs_mut().append_pair("model", model);
    }

    Ok(url.to_string())
}

// ─── SSE transport ──────────────────────────────────────────────────────────

/// Send request via SSE transport and collect the complete response.
async fn send_sse_request(
    client: &reqwest::Client,
    responses_url: &str,
    request: &ResponsesRequest,
    bearer_token: &str,
    account_id: Option<&str>,
) -> Result<String, GatewayError> {
    let mut req = client
        .post(responses_url)
        .header("Authorization", format!("Bearer {bearer_token}"))
        .header("Content-Type", "application/json")
        .header("accept", "text/event-stream");

    if let Some(account_id) = account_id {
        req = req.header("chatgpt-account-id", account_id);
    }

    let response = req
        .json(request)
        .send()
        .await
        .map_err(|e| GatewayError::UpstreamError(format!("Codex SSE request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(GatewayError::UpstreamError(format!(
            "Codex Responses API returned {status}: {text}"
        )));
    }

    let body = response
        .text()
        .await
        .map_err(|e| GatewayError::UpstreamError(format!("failed to read SSE body: {e}")))?;

    // Try parsing as SSE stream
    if let Some(text) = parse_sse_body(&body)? {
        return Ok(text);
    }

    // Try parsing as plain JSON (non-streaming response)
    if let Ok(parsed) = serde_json::from_str::<ResponsesResponse>(&body)
        && let Some(text) = extract_responses_text(&parsed)
    {
        return Ok(text);
    }

    Err(GatewayError::UpstreamError(
        "No response text from Codex Responses API".into(),
    ))
}

/// Send request via SSE and stream chunks back as OpenAI SSE format.
async fn send_sse_request_streaming(
    client: &reqwest::Client,
    responses_url: &str,
    request: &ResponsesRequest,
    bearer_token: &str,
    account_id: Option<&str>,
    model: &str,
    request_id: &str,
) -> Result<Response, GatewayError> {
    let mut req = client
        .post(responses_url)
        .header("Authorization", format!("Bearer {bearer_token}"))
        .header("Content-Type", "application/json")
        .header("accept", "text/event-stream");

    if let Some(account_id) = account_id {
        req = req.header("chatgpt-account-id", account_id);
    }

    let response = req
        .json(request)
        .send()
        .await
        .map_err(|e| GatewayError::UpstreamError(format!("Codex SSE request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(GatewayError::UpstreamError(format!(
            "Codex Responses API returned {status}: {text}"
        )));
    }

    let model = model.to_string();
    let request_id = request_id.to_string();

    // Stream conversion: Codex SSE → OpenAI SSE chunks
    let stream = async_stream::stream! {
        let mut byte_stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = match chunk_result {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(e) => {
                    tracing::warn!(error = %e, "SSE stream read error");
                    break;
                }
            };

            buffer.push_str(&chunk);

            // Process complete SSE events (delimited by double newline)
            while let Some(idx) = buffer.find("\n\n") {
                let event_text = buffer[..idx].to_string();
                buffer = buffer[idx + 2..].to_string();

                for line in event_text.lines() {
                    let Some(data) = line.strip_prefix("data:").map(|d| d.trim()) else {
                        continue;
                    };
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }
                    let Ok(event) = serde_json::from_str::<Value>(data) else {
                        continue;
                    };

                    if let Some(message) = extract_stream_error(&event) {
                        tracing::error!(error = %message, "Codex stream error");
                        break;
                    }

                    let event_type = event.get("type").and_then(Value::as_str);
                    if event_type == Some("response.output_text.delta")
                        && let Some(delta) = event.get("delta").and_then(Value::as_str)
                        && !delta.is_empty()
                    {
                        let sse_chunk = build_sse_chunk(delta, &model, &request_id, false);
                        yield Ok::<_, std::io::Error>(sse_chunk);
                    }
                }
            }
        }

        // Send finish chunk and [DONE]
        let finish = build_sse_chunk("", &model, &request_id, true);
        yield Ok(finish);
        yield Ok("data: [DONE]\n\n".to_string());
    };

    let body = Body::from_stream(stream);
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

// ─── WebSocket transport ────────────────────────────────────────────────────

/// Send request via WebSocket and collect the complete response.
async fn send_ws_request(
    responses_url: &str,
    request: &ResponsesRequest,
    model: &str,
    bearer_token: &str,
    account_id: Option<&str>,
) -> Result<String, WsRequestError> {
    let ws_url = build_ws_url(responses_url, model)?;

    let mut ws_request = ws_url.into_client_request().map_err(|e| {
        WsRequestError::TransportUnavailable(format!("invalid WebSocket request URL: {e}"))
    })?;

    // Set auth headers
    let headers = ws_request.headers_mut();
    headers.insert(
        AUTHORIZATION,
        WsHeaderValue::from_str(&format!("Bearer {bearer_token}")).map_err(|e| {
            WsRequestError::TransportUnavailable(format!("invalid bearer token: {e}"))
        })?,
    );
    headers.insert("accept", WsHeaderValue::from_static("text/event-stream"));
    headers.insert(USER_AGENT, WsHeaderValue::from_static("evo-gateway"));

    if let Some(account_id) = account_id
        && let Ok(val) = WsHeaderValue::from_str(account_id)
    {
        headers.insert("chatgpt-account-id", val);
    }

    // Build the response.create payload
    let payload = json!({
        "type": "response.create",
        "model": &request.model,
        "input": &request.input,
        "instructions": &request.instructions,
        "store": request.store,
        "text": &request.text,
        "reasoning": &request.reasoning,
        "include": &request.include,
        "tool_choice": &request.tool_choice,
        "parallel_tool_calls": request.parallel_tool_calls,
    });

    // Connect
    let (mut ws_stream, _) = timeout(WS_CONNECT_TIMEOUT, connect_async(ws_request))
        .await
        .map_err(|_| {
            WsRequestError::TransportUnavailable(format!(
                "WebSocket connect timed out after {}s",
                WS_CONNECT_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| {
            WsRequestError::TransportUnavailable(format!("WebSocket connect failed: {e}"))
        })?;

    // Send payload
    let payload_str = serde_json::to_string(&payload)
        .map_err(|e| WsRequestError::TransportUnavailable(format!("payload serialization: {e}")))?;

    timeout(
        WS_SEND_TIMEOUT,
        ws_stream.send(WsMessage::Text(payload_str.into())),
    )
    .await
    .map_err(|_| {
        WsRequestError::TransportUnavailable(format!(
            "WebSocket send timed out after {}s",
            WS_SEND_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| WsRequestError::TransportUnavailable(format!("WebSocket send failed: {e}")))?;

    // Read response frames
    let mut saw_delta = false;
    let mut delta_accumulator = String::new();
    let mut fallback_text: Option<String> = None;

    loop {
        let frame = match timeout(WS_READ_TIMEOUT, ws_stream.next()).await {
            Ok(frame) => frame,
            Err(_) => {
                let _ = ws_stream.close(None).await;
                if saw_delta || fallback_text.is_some() {
                    break;
                }
                return Err(WsRequestError::Stream(format!(
                    "WebSocket read timed out after {}s",
                    WS_READ_TIMEOUT.as_secs()
                )));
            }
        };

        let Some(frame) = frame else { break };
        let frame =
            frame.map_err(|e| WsRequestError::Stream(format!("WebSocket frame error: {e}")))?;

        let event: Value = match frame {
            WsMessage::Text(text) => serde_json::from_str(text.as_ref())
                .map_err(|e| WsRequestError::Stream(format!("invalid JSON frame: {e}")))?,
            WsMessage::Binary(binary) => {
                let text = String::from_utf8(binary.to_vec())
                    .map_err(|e| WsRequestError::Stream(format!("invalid UTF-8 frame: {e}")))?;
                serde_json::from_str(&text)
                    .map_err(|e| WsRequestError::Stream(format!("invalid JSON frame: {e}")))?
            }
            WsMessage::Ping(payload) => {
                let _ = ws_stream.send(WsMessage::Pong(payload)).await;
                continue;
            }
            WsMessage::Close(_) => break,
            _ => continue,
        };

        if let Some(message) = extract_stream_error(&event) {
            return Err(WsRequestError::Stream(format!(
                "Codex WebSocket error: {message}"
            )));
        }

        if let Some(text) = extract_stream_event_text(&event, saw_delta) {
            let event_type = event.get("type").and_then(Value::as_str);
            if event_type == Some("response.output_text.delta") {
                saw_delta = true;
                delta_accumulator.push_str(&text);
            } else if fallback_text.is_none() {
                fallback_text = Some(text);
            }
        }

        let event_type = event.get("type").and_then(Value::as_str);
        if event_type == Some("response.completed") || event_type == Some("response.done") {
            // Try to get text from the completed response object
            if let Some(response_val) = event.get("response").cloned()
                && let Ok(parsed) = serde_json::from_value::<ResponsesResponse>(response_val)
                && let Some(text) = extract_responses_text(&parsed)
            {
                let _ = ws_stream.close(None).await;
                return Ok(text);
            }
            break;
        }
    }

    if saw_delta && !delta_accumulator.is_empty() {
        return Ok(delta_accumulator);
    }
    if let Some(text) = fallback_text {
        return Ok(text);
    }

    Err(WsRequestError::Stream(
        "No response from Codex WebSocket stream".into(),
    ))
}

// ─── Unified request dispatch ───────────────────────────────────────────────

/// Build the ResponsesRequest from the incoming chat body.
fn build_request(body: &Value, model: &str) -> ResponsesRequest {
    let (instructions, input) = build_responses_input(body);
    let effort = resolve_reasoning_effort(model, body);

    ResponsesRequest {
        model: normalize_model_id(model).to_string(),
        input,
        instructions,
        store: false,
        stream: true,
        text: ResponsesTextOptions {
            verbosity: "medium".to_string(),
        },
        reasoning: ResponsesReasoningOptions {
            effort,
            summary: "auto".to_string(),
        },
        include: vec!["reasoning.encrypted_content".to_string()],
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
    }
}

/// Send a request using the configured transport (auto, ws, sse).
async fn send_request(
    client: &reqwest::Client,
    responses_url: &str,
    request: &ResponsesRequest,
    model: &str,
    bearer_token: &str,
    account_id: Option<&str>,
) -> Result<String, GatewayError> {
    let transport = resolve_transport();

    match transport {
        CodexTransport::WebSocket => {
            send_ws_request(responses_url, request, model, bearer_token, account_id)
                .await
                .map_err(Into::into)
        }
        CodexTransport::Sse => {
            send_sse_request(client, responses_url, request, bearer_token, account_id).await
        }
        CodexTransport::Auto => {
            match send_ws_request(responses_url, request, model, bearer_token, account_id).await {
                Ok(text) => Ok(text),
                Err(WsRequestError::TransportUnavailable(e)) => {
                    tracing::warn!(
                        error = %e,
                        "Codex WebSocket transport unavailable; falling back to SSE"
                    );
                    send_sse_request(client, responses_url, request, bearer_token, account_id).await
                }
                Err(WsRequestError::Stream(e)) => Err(GatewayError::UpstreamError(e)),
            }
        }
    }
}

// ─── Public API ─────────────────────────────────────────────────────────────

/// Handle a non-streaming chat completion request via Codex Responses API.
///
/// Collects the full response and returns it as an OpenAI-compatible JSON body.
pub async fn codex_auth_chat(
    state: &AppState,
    pool: &ProviderPool,
    body: Value,
    model: &str,
) -> Result<Response, GatewayError> {
    let (bearer_token, account_id) = resolve_bearer_token(pool).await?;
    let responses_url = build_responses_url(&pool.config.base_url);
    let request = build_request(&body, model);
    let request_id = uuid::Uuid::new_v4().to_string();

    let text = send_request(
        &state.http_client,
        &responses_url,
        &request,
        model,
        &bearer_token,
        account_id.as_deref(),
    )
    .await?;

    let response = build_openai_response(&text, model, &request_id);
    Ok(axum::Json(response).into_response())
}

/// Handle a streaming chat completion request via Codex Responses API.
///
/// Converts Codex SSE events to OpenAI `chat.completion.chunk` SSE format.
pub async fn codex_auth_chat_streaming(
    state: &AppState,
    pool: &ProviderPool,
    body: Value,
    model: &str,
) -> Result<Response, GatewayError> {
    let (bearer_token, account_id) = resolve_bearer_token(pool).await?;
    let responses_url = build_responses_url(&pool.config.base_url);
    let mut request = build_request(&body, model);
    request.stream = true;
    let request_id = uuid::Uuid::new_v4().to_string();

    let transport = resolve_transport();

    // For streaming, we prefer SSE since we can pipe the stream directly.
    // WebSocket streaming would require a different approach.
    match transport {
        CodexTransport::Sse | CodexTransport::Auto => {
            send_sse_request_streaming(
                &state.http_client,
                &responses_url,
                &request,
                &bearer_token,
                account_id.as_deref(),
                model,
                &request_id,
            )
            .await
        }
        CodexTransport::WebSocket => {
            // For WS streaming, collect the full response and emit as a single chunk
            // (true streaming over WS would need a more complex adapter)
            let text = send_ws_request(
                &responses_url,
                &request,
                model,
                &bearer_token,
                account_id.as_deref(),
            )
            .await
            .map_err(GatewayError::from)?;

            let chunk = build_sse_chunk(&text, model, &request_id, false);
            let finish = build_sse_chunk("", model, &request_id, true);
            let body_str = format!("{chunk}{finish}data: [DONE]\n\n");

            let body = Body::from(body_str);
            let mut headers = HeaderMap::new();
            headers.insert(
                "content-type",
                HeaderValue::from_static("text/event-stream"),
            );
            headers.insert("cache-control", HeaderValue::from_static("no-cache"));

            let mut resp = Response::new(body);
            *resp.status_mut() = StatusCode::OK;
            *resp.headers_mut() = headers;
            Ok(resp)
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_responses_input_basic() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi there!"},
                {"role": "user", "content": "Thanks"},
            ]
        });
        let (instructions, input) = build_responses_input(&body);
        assert_eq!(instructions, "You are helpful.");
        assert_eq!(input.len(), 3); // user, assistant, user (system goes to instructions)

        assert_eq!(input[0].role, "user");
        assert_eq!(input[0].content[0].kind, "input_text");
        assert_eq!(input[0].content[0].text.as_deref(), Some("Hello"));

        assert_eq!(input[1].role, "assistant");
        assert_eq!(input[1].content[0].kind, "output_text");

        assert_eq!(input[2].role, "user");
    }

    #[test]
    fn test_build_responses_input_no_system() {
        let body = json!({
            "messages": [{"role": "user", "content": "Hi"}]
        });
        let (instructions, input) = build_responses_input(&body);
        assert_eq!(instructions, "You are a helpful coding assistant.");
        assert_eq!(input.len(), 1);
    }

    #[test]
    fn test_build_responses_input_multiple_system() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "Rule 1"},
                {"role": "system", "content": "Rule 2"},
                {"role": "user", "content": "Go"},
            ]
        });
        let (instructions, _) = build_responses_input(&body);
        assert_eq!(instructions, "Rule 1\n\nRule 2");
    }

    #[test]
    fn test_clamp_reasoning_effort() {
        assert_eq!(clamp_reasoning_effort("gpt-5-codex", "xhigh"), "high");
        assert_eq!(clamp_reasoning_effort("gpt-5-codex", "minimal"), "low");
        assert_eq!(clamp_reasoning_effort("gpt-5-codex", "medium"), "medium");
        assert_eq!(clamp_reasoning_effort("gpt-5.3-codex", "minimal"), "low");
        assert_eq!(clamp_reasoning_effort("gpt-5.1", "xhigh"), "high");
        assert_eq!(
            clamp_reasoning_effort("gpt-5.1-codex-mini", "low"),
            "medium"
        );
        assert_eq!(
            clamp_reasoning_effort("gpt-5.1-codex-mini", "xhigh"),
            "high"
        );
        // Unknown model passes through
        assert_eq!(clamp_reasoning_effort("gpt-4o", "high"), "high");
    }

    #[test]
    fn test_build_responses_url() {
        assert_eq!(
            build_responses_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            build_responses_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            build_responses_url("https://custom.endpoint.com/v1/responses"),
            "https://custom.endpoint.com/v1/responses"
        );
    }

    #[test]
    fn test_normalize_model_id() {
        assert_eq!(normalize_model_id("gpt-5-codex"), "gpt-5-codex");
        assert_eq!(normalize_model_id("openai/gpt-5-codex"), "gpt-5-codex");
    }

    #[test]
    fn test_extract_responses_text() {
        let resp = ResponsesResponse {
            output: vec![],
            output_text: Some("hello".into()),
        };
        assert_eq!(extract_responses_text(&resp).as_deref(), Some("hello"));

        let resp = ResponsesResponse {
            output: vec![ResponsesOutput {
                content: vec![ResponsesContent {
                    kind: Some("output_text".into()),
                    text: Some("nested".into()),
                }],
            }],
            output_text: None,
        };
        assert_eq!(extract_responses_text(&resp).as_deref(), Some("nested"));
    }

    #[test]
    fn test_parse_sse_body_deltas() {
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n\
                    data: {\"type\":\"response.output_text.delta\",\"delta\":\" world\"}\n\n\
                    data: {\"type\":\"response.completed\",\"response\":{\"output_text\":\"Hello world\"}}\n\n\
                    data: [DONE]\n\n";
        let result = parse_sse_body(body).unwrap();
        assert_eq!(result.as_deref(), Some("Hello world"));
    }

    #[test]
    fn test_parse_sse_body_completed_only() {
        let body = "data: {\"type\":\"response.completed\",\"response\":{\"output_text\":\"Done\"}}\n\ndata: [DONE]\n\n";
        let result = parse_sse_body(body).unwrap();
        assert_eq!(result.as_deref(), Some("Done"));
    }

    #[test]
    fn test_parse_sse_body_error() {
        let body = "data: {\"type\":\"error\",\"message\":\"rate limited\"}\n\n";
        let result = parse_sse_body(body);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rate limited"));
    }

    #[test]
    fn test_build_ws_url() {
        let url = build_ws_url("https://api.openai.com/v1/responses", "gpt-5-codex").unwrap();
        assert!(url.starts_with("wss://"));
        assert!(url.contains("model=gpt-5-codex"));
    }

    #[test]
    fn test_extract_stream_error() {
        let event = json!({"type": "error", "message": "bad request"});
        assert_eq!(extract_stream_error(&event).as_deref(), Some("bad request"));

        let event = json!({"type": "response.failed", "response": {"error": {"message": "quota"}}});
        assert_eq!(extract_stream_error(&event).as_deref(), Some("quota"));

        let event = json!({"type": "response.output_text.delta", "delta": "hi"});
        assert_eq!(extract_stream_error(&event), None);
    }
}
