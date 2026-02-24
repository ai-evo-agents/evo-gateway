//! Shared helpers for CLI-subprocess providers (Cursor, Claude Code, etc.).
//!
//! These functions extract prompts from OpenAI-format message arrays and build
//! OpenAI-compatible responses / SSE chunks that can be returned to any client.

use crate::error::GatewayError;
use serde_json::{Value, json};

/// Concatenate the OpenAI messages array into a single prompt string.
///
/// - `system` -> `[System]: <content>`
/// - `user` -> `<content>`
/// - `assistant` -> `[Previous assistant]: <content>`
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

/// Build an OpenAI-compatible `chat.completion` response JSON.
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

/// Build an OpenAI-compatible `chat.completion` response with usage info.
pub fn build_openai_response_with_usage(
    content: &str,
    model: &str,
    request_id: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> Value {
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
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
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
    fn test_build_openai_response_with_usage() {
        let resp = build_openai_response_with_usage("Hello", "sonnet", "req-1", 10, 5);
        assert_eq!(resp["usage"]["prompt_tokens"], 10);
        assert_eq!(resp["usage"]["completion_tokens"], 5);
        assert_eq!(resp["usage"]["total_tokens"], 15);
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
}
