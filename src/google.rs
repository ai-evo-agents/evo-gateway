//! Google Generative AI (Gemini) provider.
//!
//! Converts OpenAI chat/completions format to Gemini's generateContent API
//! and vice versa. Auth uses API key as URL query parameter, not Bearer header.
//!
//! Endpoints:
//!   Non-streaming: POST /v1beta/models/{model}:generateContent?key={api_key}
//!   Streaming:     POST /v1beta/models/{model}:streamGenerateContent?key={api_key}&alt=sse

use crate::error::GatewayError;
use crate::state::{AppState, ProviderPool};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

// ─── Request conversion ─────────────────────────────────────────────────────

/// Convert OpenAI chat/completions request body to Gemini generateContent format.
fn convert_openai_to_gemini(body: &Value) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    let mut system_parts: Vec<Value> = Vec::new();

    if let Some(messages) = body["messages"].as_array() {
        for msg in messages {
            let role = msg["role"].as_str().unwrap_or("user");
            let content = &msg["content"];

            match role {
                "system" => {
                    // System messages go into systemInstruction
                    if let Some(text) = content.as_str() {
                        system_parts.push(json!({"text": text}));
                    }
                }
                "assistant" => {
                    // Gemini uses "model" role for assistant
                    let parts = content_to_parts(content);
                    if !parts.is_empty() {
                        contents.push(json!({"role": "model", "parts": parts}));
                    }
                }
                _ => {
                    // "user" and any other role
                    let parts = content_to_parts(content);
                    if !parts.is_empty() {
                        contents.push(json!({"role": "user", "parts": parts}));
                    }
                }
            }
        }
    }

    let mut request = json!({"contents": contents});

    // Add system instruction if present
    if !system_parts.is_empty() {
        request["systemInstruction"] = json!({"parts": system_parts});
    }

    // Map generation config parameters
    let mut gen_config = json!({});
    let mut has_gen_config = false;

    if let Some(temp) = body.get("temperature") {
        gen_config["temperature"] = temp.clone();
        has_gen_config = true;
    }
    if let Some(max_tokens) = body.get("max_tokens") {
        gen_config["maxOutputTokens"] = max_tokens.clone();
        has_gen_config = true;
    }
    if let Some(top_p) = body.get("top_p") {
        gen_config["topP"] = top_p.clone();
        has_gen_config = true;
    }
    if let Some(stop) = body.get("stop") {
        gen_config["stopSequences"] = stop.clone();
        has_gen_config = true;
    }

    if has_gen_config {
        request["generationConfig"] = gen_config;
    }

    request
}

/// Convert message content (string or array) to Gemini parts array.
fn content_to_parts(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) => vec![json!({"text": text})],
        Value::Array(parts) => {
            parts
                .iter()
                .filter_map(|part| {
                    match part["type"].as_str() {
                        Some("text") => part["text"].as_str().map(|t| json!({"text": t})),
                        Some("image_url") => {
                            // Convert OpenAI image_url data: URI to Gemini inline_data
                            if let Some(url) = part["image_url"]["url"].as_str()
                                && let Some(data_url) = url.strip_prefix("data:")
                                && let Some((mime, b64)) = data_url.split_once(";base64,")
                            {
                                return Some(json!({
                                    "inline_data": {
                                        "mime_type": mime,
                                        "data": b64
                                    }
                                }));
                            }
                            None
                        }
                        _ => part["text"].as_str().map(|t| json!({"text": t})),
                    }
                })
                .collect()
        }
        _ => vec![],
    }
}

// ─── Response conversion ────────────────────────────────────────────────────

/// Convert Gemini generateContent response to OpenAI chat.completion format.
fn convert_gemini_to_openai(gemini_resp: &Value, model: &str) -> Value {
    let text = gemini_resp["candidates"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(|c| c["content"]["parts"].as_array())
        .and_then(|parts| {
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .first()
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    let finish_reason = gemini_resp["candidates"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(|c| c["finishReason"].as_str())
        .map(gemini_finish_reason_to_openai)
        .unwrap_or("stop");

    let mut usage = json!({});
    if let Some(meta) = gemini_resp.get("usageMetadata") {
        if let Some(pt) = meta["promptTokenCount"].as_u64() {
            usage["prompt_tokens"] = json!(pt);
        }
        if let Some(ct) = meta["candidatesTokenCount"].as_u64() {
            usage["completion_tokens"] = json!(ct);
        }
        if let (Some(pt), Some(ct)) = (
            meta["promptTokenCount"].as_u64(),
            meta["candidatesTokenCount"].as_u64(),
        ) {
            usage["total_tokens"] = json!(pt + ct);
        }
    }

    json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": text,
            },
            "finish_reason": finish_reason,
        }],
        "usage": usage,
    })
}

fn gemini_finish_reason_to_openai(reason: &str) -> &'static str {
    match reason {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" => "content_filter",
        "RECITATION" => "content_filter",
        _ => "stop",
    }
}

// ─── URL building ───────────────────────────────────────────────────────────

fn build_gemini_url(base_url: &str, model: &str, api_key: &str, streaming: bool) -> String {
    let base = base_url.trim_end_matches('/');
    let action = if streaming {
        "streamGenerateContent"
    } else {
        "generateContent"
    };
    let mut url = format!(
        "{base}/v1beta/models/{model}:{action}?key={api_key}",
        model = urlencoding::encode(model),
        api_key = urlencoding::encode(api_key),
    );
    if streaming {
        url.push_str("&alt=sse");
    }
    url
}

// ─── Public handlers ────────────────────────────────────────────────────────

/// Non-streaming Gemini chat: convert request, call API, convert response.
pub async fn gemini_chat(
    state: &AppState,
    pool: &ProviderPool,
    body: Value,
    model: &str,
) -> Result<Response, GatewayError> {
    let api_key = pool.token_pool.next_token().ok_or_else(|| {
        GatewayError::ConfigError(format!(
            "no API key configured for provider '{}'",
            pool.name()
        ))
    })?;

    let gemini_body = convert_openai_to_gemini(&body);
    let url = build_gemini_url(&pool.config.base_url, model, api_key, false);

    let response = state
        .http_client
        .post(&url)
        .header("content-type", "application/json")
        .json(&gemini_body)
        .send()
        .await
        .map_err(GatewayError::from)?;

    let status = response.status();
    let gemini_resp: Value = response
        .json()
        .await
        .map_err(|e| GatewayError::UpstreamError(e.to_string()))?;

    if !status.is_success() {
        return Err(GatewayError::UpstreamError(format!(
            "Gemini returned {status}: {gemini_resp}"
        )));
    }

    let openai_resp = convert_gemini_to_openai(&gemini_resp, model);
    Ok(axum::Json(openai_resp).into_response())
}

/// Streaming Gemini chat: Gemini returns SSE events, each being a complete
/// GenerateContentResponse. We convert each to an OpenAI delta chunk and
/// forward as SSE.
pub async fn gemini_chat_streaming(
    state: &AppState,
    pool: &ProviderPool,
    body: Value,
    model: &str,
) -> Result<Response, GatewayError> {
    let api_key = pool.token_pool.next_token().ok_or_else(|| {
        GatewayError::ConfigError(format!(
            "no API key configured for provider '{}'",
            pool.name()
        ))
    })?;

    let gemini_body = convert_openai_to_gemini(&body);
    let url = build_gemini_url(&pool.config.base_url, model, api_key, true);

    let response = state
        .http_client
        .post(&url)
        .header("content-type", "application/json")
        .json(&gemini_body)
        .send()
        .await
        .map_err(GatewayError::from)?;

    let status = response.status();
    if !status.is_success() {
        let err_body: Value = response
            .json()
            .await
            .unwrap_or_else(|_| json!({"error": "unknown"}));
        return Err(GatewayError::UpstreamError(format!(
            "Gemini returned {status}: {err_body}"
        )));
    }

    let completion_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let model_owned = model.to_string();

    // Convert Gemini SSE stream → OpenAI SSE stream
    let stream = async_stream::stream! {
        use futures_util::StreamExt;
        let mut byte_stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = byte_stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Gemini stream error: {e}");
                    break;
                }
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE events from buffer
            while let Some(event_end) = buffer.find("\n\n") {
                let event_str = buffer[..event_end].to_string();
                buffer = buffer[event_end + 2..].to_string();

                // Extract data payload from SSE event
                let data = event_str
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .collect::<Vec<_>>()
                    .join("");

                if data.is_empty() {
                    continue;
                }

                let gemini_event: Value = match serde_json::from_str(&data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                // Extract text delta from Gemini response
                let text = gemini_event["candidates"]
                    .as_array()
                    .and_then(|c| c.first())
                    .and_then(|c| c["content"]["parts"].as_array())
                    .and_then(|parts| parts.first())
                    .and_then(|p| p["text"].as_str())
                    .unwrap_or("");

                let finish_reason = gemini_event["candidates"]
                    .as_array()
                    .and_then(|c| c.first())
                    .and_then(|c| c["finishReason"].as_str())
                    .map(gemini_finish_reason_to_openai);

                let openai_chunk = json!({
                    "id": &completion_id,
                    "object": "chat.completion.chunk",
                    "model": &model_owned,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "content": text,
                        },
                        "finish_reason": finish_reason,
                    }],
                });

                let sse = format!("data: {}\n\n", serde_json::to_string(&openai_chunk).unwrap_or_default());
                yield Ok::<_, std::io::Error>(sse);
            }
        }

        // Final [DONE] marker
        yield Ok("data: [DONE]\n\n".to_string());
    };

    let body = Body::from_stream(stream);
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("cache-control", HeaderValue::from_static("no-cache"));

    Ok((headers, body).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_simple_message() {
        let body = json!({
            "model": "gemini-2.5-pro",
            "messages": [
                {"role": "user", "content": "Hello"}
            ]
        });
        let gemini = convert_openai_to_gemini(&body);
        assert_eq!(gemini["contents"][0]["role"], "user");
        assert_eq!(gemini["contents"][0]["parts"][0]["text"], "Hello");
        assert!(gemini.get("systemInstruction").is_none());
    }

    #[test]
    fn convert_system_message() {
        let body = json!({
            "model": "gemini-2.5-pro",
            "messages": [
                {"role": "system", "content": "You are helpful"},
                {"role": "user", "content": "Hello"}
            ]
        });
        let gemini = convert_openai_to_gemini(&body);
        assert_eq!(
            gemini["systemInstruction"]["parts"][0]["text"],
            "You are helpful"
        );
        assert_eq!(gemini["contents"].as_array().unwrap().len(), 1);
        assert_eq!(gemini["contents"][0]["role"], "user");
    }

    #[test]
    fn convert_assistant_role_to_model() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "Hi"},
                {"role": "assistant", "content": "Hello!"},
                {"role": "user", "content": "How are you?"}
            ]
        });
        let gemini = convert_openai_to_gemini(&body);
        assert_eq!(gemini["contents"][1]["role"], "model");
    }

    #[test]
    fn convert_generation_config() {
        let body = json!({
            "messages": [{"role": "user", "content": "Hi"}],
            "temperature": 0.7,
            "max_tokens": 100,
            "top_p": 0.9,
        });
        let gemini = convert_openai_to_gemini(&body);
        assert_eq!(gemini["generationConfig"]["temperature"], 0.7);
        assert_eq!(gemini["generationConfig"]["maxOutputTokens"], 100);
        assert_eq!(gemini["generationConfig"]["topP"], 0.9);
    }

    #[test]
    fn convert_gemini_response() {
        let gemini_resp = json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello there!"}],
                    "role": "model"
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
            }
        });
        let openai = convert_gemini_to_openai(&gemini_resp, "gemini-2.5-pro");
        assert_eq!(openai["choices"][0]["message"]["content"], "Hello there!");
        assert_eq!(openai["choices"][0]["finish_reason"], "stop");
        assert_eq!(openai["usage"]["prompt_tokens"], 10);
        assert_eq!(openai["usage"]["completion_tokens"], 5);
        assert_eq!(openai["usage"]["total_tokens"], 15);
    }

    #[test]
    fn build_url_non_streaming() {
        let url = build_gemini_url(
            "https://generativelanguage.googleapis.com",
            "gemini-2.5-pro",
            "test-key",
            false,
        );
        assert!(url.contains("generateContent"));
        assert!(url.contains("key=test-key"));
        assert!(!url.contains("alt=sse"));
    }

    #[test]
    fn build_url_streaming() {
        let url = build_gemini_url(
            "https://generativelanguage.googleapis.com",
            "gemini-2.5-pro",
            "test-key",
            true,
        );
        assert!(url.contains("streamGenerateContent"));
        assert!(url.contains("alt=sse"));
    }
}
