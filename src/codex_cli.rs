//! Codex CLI provider — spawns `codex` CLI subprocess in exec mode.
//!
//! Supports two execution backends:
//! - **Direct**: `tokio::process::Command` (original, used as fallback)
//! - **Tmux**: managed tmux sessions via `TmuxManager` (preferred when available)

use crate::cli_common::{build_openai_response_with_usage, build_sse_chunk, extract_prompt};
use crate::error::GatewayError;
use crate::tmux::{SessionMode, TmuxManager};
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

        if event_type == "item.completed"
            && let Some(item) = json.get("item")
            && matches!(
                item["type"].as_str(),
                Some("assistant_message") | Some("agent_message")
            )
        {
            assistant_text = extract_assistant_text(item);
        }

        if event_type == "turn.completed"
            && let Some(usage) = json.get("usage")
        {
            input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
            output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
        }

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

/// Build the codex command args vector.
fn codex_command_args(model: &str, prompt: &str) -> Vec<String> {
    let binary = codex_binary();
    let mut args = vec![
        binary,
        "exec".into(),
        "--json".into(),
        "--ephemeral".into(),
        "--full-auto".into(),
    ];
    if !model.is_empty() && model != "default" {
        args.push("-m".into());
        args.push(model.to_string());
    }
    args.push(prompt.to_string());
    args
}

// ─── Non-streaming ──────────────────────────────────────────────────────────

/// Execute a Codex CLI request and return an OpenAI-compatible JSON response.
pub async fn codex_cli_chat(
    body: &Value,
    model: &str,
    tmux: Option<&TmuxManager>,
) -> Result<Value, GatewayError> {
    let prompt = extract_prompt(body)?;

    if let Some(manager) = tmux {
        codex_cli_chat_tmux(manager, &prompt, model).await
    } else {
        codex_cli_chat_direct(&prompt, model).await
    }
}

/// Direct subprocess execution (original implementation).
async fn codex_cli_chat_direct(prompt: &str, model: &str) -> Result<Value, GatewayError> {
    let binary = codex_binary();

    let permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("codex-cli concurrent request limit reached".into())
    })?;

    let mut cmd = tokio::process::Command::new(&binary);
    let mut args = vec!["exec", "--json", "--ephemeral", "--full-auto"];
    if !model.is_empty() && model != "default" {
        args.push("-m");
        args.push(model);
    }
    args.push(prompt);
    cmd.args(&args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    debug!(binary = %binary, model = %model, "spawning codex (direct)");

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

/// Tmux-backed execution.
async fn codex_cli_chat_tmux(
    manager: &TmuxManager,
    prompt: &str,
    model: &str,
) -> Result<Value, GatewayError> {
    let _permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("codex-cli concurrent request limit reached".into())
    })?;

    let command = codex_command_args(model, prompt);

    debug!(model = %model, "spawning codex (tmux)");

    let session_id = manager
        .create_session("codex-cli", command, SessionMode::Ephemeral, None, None)
        .await?;

    let timeout = std::time::Duration::from_secs(timeout_secs());
    let status = manager.wait_for_completion(&session_id, timeout).await;

    let output = manager.read_output(&session_id).await.unwrap_or_default();
    let _ = manager.kill_session(&session_id).await;

    status?;

    let (text, input_tokens, output_tokens) = parse_codex_output(&output)?;

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
pub async fn codex_cli_chat_streaming(
    body: &Value,
    model: &str,
    tmux: Option<&TmuxManager>,
) -> Result<Response, GatewayError> {
    let prompt = extract_prompt(body)?;

    if let Some(manager) = tmux {
        codex_cli_chat_streaming_tmux(manager, &prompt, model).await
    } else {
        codex_cli_chat_streaming_direct(&prompt, model).await
    }
}

/// Direct subprocess streaming (original implementation).
async fn codex_cli_chat_streaming_direct(
    prompt: &str,
    model: &str,
) -> Result<Response, GatewayError> {
    let binary = codex_binary();

    let permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("codex-cli concurrent request limit reached".into())
    })?;

    let mut cmd = tokio::process::Command::new(&binary);
    let mut args = vec!["exec", "--json", "--ephemeral", "--full-auto"];
    if !model.is_empty() && model != "default" {
        args.push("-m");
        args.push(model);
    }
    args.push(prompt);
    cmd.args(&args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    debug!(binary = %binary, model = %model, "spawning codex (streaming direct)");

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

                    if event_type == "turn.completed" {
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
                    error!(error = %e, "error reading codex stdout");
                    break;
                }
                Err(_) => {
                    warn!("codex streaming timed out");
                    break;
                }
            }
        }

        let _ = child.kill().await;
    };

    crate::claude_code::build_sse_response(stream)
}

/// Tmux-backed streaming execution.
async fn codex_cli_chat_streaming_tmux(
    manager: &TmuxManager,
    prompt: &str,
    model: &str,
) -> Result<Response, GatewayError> {
    let permit = SEMAPHORE.try_acquire().map_err(|_| {
        GatewayError::RateLimitExceeded("codex-cli concurrent request limit reached".into())
    })?;

    let command = codex_command_args(model, prompt);

    debug!(model = %model, "spawning codex (streaming tmux)");

    let session_id = manager
        .create_session("codex-cli", command, SessionMode::Ephemeral, None, None)
        .await?;

    let mut rx = manager.subscribe_output(&session_id).await?;
    let model = model.to_string();
    let id = uuid::Uuid::new_v4().to_string();
    let sid = session_id.clone();

    let stream = async_stream::stream! {
        let _permit = permit;
        let mut accumulated = String::new();

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

                    let event_type = json["type"].as_str().unwrap_or("");

                    if event_type == "item.updated"
                        && let Some(item) = json.get("item")
                        && matches!(item["type"].as_str(), Some("assistant_message") | Some("agent_message"))
                        && let Some(full_text) = extract_assistant_text(item)
                        && full_text.len() > accumulated.len()
                    {
                        let delta = &full_text[accumulated.len()..];
                        let chunk = build_sse_chunk(delta, &model, &id, false);
                        accumulated = full_text;
                        yield Ok(chunk);
                        continue;
                    }

                    if event_type == "item.completed"
                        && let Some(item) = json.get("item")
                        && matches!(item["type"].as_str(), Some("assistant_message") | Some("agent_message"))
                        && let Some(full_text) = extract_assistant_text(item)
                        && full_text.len() > accumulated.len()
                    {
                        let delta = &full_text[accumulated.len()..];
                        let chunk = build_sse_chunk(delta, &model, &id, false);
                        yield Ok(chunk);
                        continue;
                    }

                    if event_type == "turn.failed" {
                        let error_msg = json["error"]["message"]
                            .as_str()
                            .unwrap_or("codex turn failed");
                        let chunk = build_sse_chunk(&format!("[Error: {error_msg}]"), &model, &id, false);
                        yield Ok(chunk);
                        let done_chunk = build_sse_chunk("", &model, &id, true);
                        yield Ok(done_chunk);
                        yield Ok("data: [DONE]\n\n".to_string());
                        break;
                    }

                    if event_type == "turn.completed" {
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

    crate::claude_code::build_sse_response(stream)
}

// ─── Health check ───────────────────────────────────────────────────────────

/// Check Codex CLI availability by running `codex --version`.
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

// ─── PTY-based model discovery ──────────────────────────────────────────────

/// Strip ANSI escape sequences from PTY output.
pub fn strip_ansi(s: &str) -> String {
    let re = regex::Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b\].*?\x07|\x1b\[[\?]?[0-9;]*[hlm]")
        .expect("valid regex");
    re.replace_all(s, "").to_string()
}

/// Parse model names from the Codex `/model` TUI output.
pub fn parse_model_list_output(s: &str) -> Vec<String> {
    let re = regex::Regex::new(r"\d+\.\s*([a-z][a-z0-9._-]*)").expect("valid regex");
    re.captures_iter(s)
        .filter_map(|cap| {
            let model = cap[1].to_string();
            if model.len() < 2 { None } else { Some(model) }
        })
        .collect()
}

/// Discover available Codex CLI models by spawning codex in a PTY.
pub async fn discover_codex_models() -> Result<Vec<String>, GatewayError> {
    tokio::task::spawn_blocking(discover_codex_models_blocking)
        .await
        .map_err(|e| GatewayError::Internal(format!("PTY task join error: {e}")))?
}

/// Blocking PTY-based model discovery.
fn discover_codex_models_blocking() -> Result<Vec<String>, GatewayError> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::{Read, Write};
    use std::time::Duration;

    let binary = codex_binary();

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| GatewayError::Internal(format!("failed to open PTY: {e}")))?;

    let mut cmd = CommandBuilder::new(&binary);
    cmd.arg("--dangerously-bypass-approvals-and-sandbox");
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLUMNS", "120");
    cmd.env("LINES", "24");

    debug!(binary = %binary, "spawning codex PTY for model discovery");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| GatewayError::ConfigError(format!("failed to spawn codex PTY: {e}")))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| GatewayError::Internal(format!("PTY reader error: {e}")))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| GatewayError::Internal(format!("PTY writer error: {e}")))?;

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    if tx.send(chunk).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Phase 1: Wait for codex to initialize
    debug!("waiting for codex to initialize...");
    let init_deadline = std::time::Instant::now() + Duration::from_secs(45);
    let mut init_output = String::new();
    let mut last_data_at = std::time::Instant::now();
    let mut saw_prompt = false;
    loop {
        if std::time::Instant::now() > init_deadline {
            debug!("codex init timed out after 45s");
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
        let mut got_data = false;
        while let Ok(chunk) = rx.try_recv() {
            init_output.push_str(&chunk);
            got_data = true;
        }
        if got_data {
            last_data_at = std::time::Instant::now();
        }
        let clean_init = strip_ansi(&init_output);
        if clean_init.contains("context left") {
            saw_prompt = true;
        }
        let mcp_starting =
            clean_init.contains("Starting MCP") || clean_init.contains("esc to interrupt");
        if saw_prompt && !mcp_starting && last_data_at.elapsed() > Duration::from_secs(2) {
            debug!("codex fully initialized — prompt detected, MCP servers done");
            break;
        }
        if saw_prompt && last_data_at.elapsed() > Duration::from_secs(3) {
            debug!("codex init settled after 3s of quiet");
            break;
        }
    }

    while rx.try_recv().is_ok() {}

    // Phase 2: Send /model command
    debug!("sending /model command to codex PTY (keystroke-by-keystroke)");
    for ch in b"/model" {
        if writer.write_all(&[*ch]).is_err() {
            let _ = child.kill();
            drop(reader_thread);
            return Err(GatewayError::Internal(
                "failed to write /model to PTY".into(),
            ));
        }
        let _ = writer.flush();
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(200));
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();

    // Phase 3: Collect model list output
    let model_deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut model_output = String::new();
    loop {
        if std::time::Instant::now() > model_deadline {
            debug!("model list collection timed out");
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
        while let Ok(chunk) = rx.try_recv() {
            model_output.push_str(&chunk);
        }
        let clean = strip_ansi(&model_output);
        if clean.contains("Select Model") || clean.contains("reasoning effort") {
            std::thread::sleep(Duration::from_millis(500));
            while let Ok(chunk) = rx.try_recv() {
                model_output.push_str(&chunk);
            }
            break;
        }
    }

    // Phase 4: Cleanup
    let _ = writer.write_all(b"\x1b");
    std::thread::sleep(Duration::from_millis(300));
    let _ = child.kill();
    let _ = reader_thread.join();

    // Phase 5: Parse models
    let clean = strip_ansi(&model_output);
    let models = parse_model_list_output(&clean);

    if models.is_empty() {
        debug!(raw_output = %clean, "no models found in codex /model output");
        return Err(GatewayError::UpstreamError(
            "codex /model returned no parseable models".into(),
        ));
    }

    debug!(models = ?models, "discovered codex-cli models via PTY");
    Ok(models)
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

    #[test]
    fn test_strip_ansi() {
        let input = "\x1b[33m\x1b[1m⚠\x1b[22m\x1b[39m Warning: hello\x1b[0m";
        let clean = strip_ansi(input);
        assert!(clean.contains("Warning: hello"));
        assert!(!clean.contains("\x1b"));
    }

    #[test]
    fn test_strip_ansi_cursor_codes() {
        let input = "\x1b[?25l\x1b[1;1HSelect Model\x1b[?25h";
        let clean = strip_ansi(input);
        assert!(clean.contains("Select Model"));
    }

    #[test]
    fn test_parse_model_list_output() {
        let output = r#"Select Model and Effort
Access legacy models by running codex -m <model_name> or in your config.toml

> 1. gpt-5.3-codex (current)  Latest frontier agentic coding model.
  2. gpt-5.2-codex            Frontier agentic coding model.
  3. gpt-5.1-codex-max        Codex-optimized flagship for deep and fast reasoning.
  4. gpt-5.2                  Latest frontier model with improvements across knowledge, reasoning and coding
  5. gpt-5.1-codex-mini       Optimized for codex. Cheaper, faster, but less capable.

Press enter to select reasoning effort, or esc to dismiss."#;
        let models = parse_model_list_output(output);
        assert_eq!(
            models,
            vec![
                "gpt-5.3-codex",
                "gpt-5.2-codex",
                "gpt-5.1-codex-max",
                "gpt-5.2",
                "gpt-5.1-codex-mini",
            ]
        );
    }

    #[test]
    fn test_parse_model_list_with_parenthetical() {
        let output = "> 1. gpt-5.3-codex (current)  Description\n  2. gpt-5.2-codex  Description";
        let models = parse_model_list_output(output);
        assert_eq!(models, vec!["gpt-5.3-codex", "gpt-5.2-codex"]);
    }

    #[test]
    fn test_parse_model_list_concatenated_tui() {
        let output = "Select Model and Effort\
        Access legacy models by running codex -m <model_name> or in your config.toml\
        › 1. gpt-5.3-codex (current)  Latest frontier agentic coding model.\
        2.gpt-5.2-codexFrontier agentic coding model.\
        3.gpt-5.1-codex-maxCodex-optimized flagship for deep and fast reasoning.\
        4.gpt-5.2Latest frontier model with improvements\
        5.gpt-5.1-codex-miniOptimized for codex. Cheaper, faster, but less capable.\
        Press enter to select reasoning effort, or esc to dismiss.";
        let models = parse_model_list_output(output);
        assert_eq!(
            models,
            vec![
                "gpt-5.3-codex",
                "gpt-5.2-codex",
                "gpt-5.1-codex-max",
                "gpt-5.2",
                "gpt-5.1-codex-mini",
            ]
        );
    }

    #[test]
    fn test_parse_model_list_empty() {
        let output = "No models available";
        let models = parse_model_list_output(output);
        assert!(models.is_empty());
    }
}
