//! Socket.IO server for interactive tmux session management.
//!
//! Integrates with axum via `socketioxide` tower layer (same port as HTTP).
//! Other agents can connect and manage CLI sessions in real-time.
//!
//! ## Events
//!
//! **Client -> Gateway (with ACK):**
//! - `session:create`    — Create a new tmux session
//! - `session:input`     — Send text input to a session
//! - `session:keys`      — Send raw key sequences (C-c, Escape, etc.)
//! - `session:capture`   — Snapshot current pane content
//! - `session:kill`      — Kill a session
//! - `session:list`      — List all active sessions
//! - `session:subscribe` — Subscribe to a session's output stream
//!
//! **Gateway -> Client:**
//! - `session:output`    — Real-time output lines (to room subscribers)
//! - `session:status`    — Session status changes

use crate::state::AppState;
use crate::tmux::{SessionMode, TmuxManager};
use serde_json::json;
use socketioxide::SocketIo;
use socketioxide::extract::{AckSender, Data, SocketRef};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Register all Socket.IO event handlers for session management.
pub fn register_handlers(io: SocketIo, state: Arc<AppState>) {
    io.ns("/", move |socket: SocketRef| {
        info!(sid = %socket.id, "agent connected to gateway Socket.IO");

        let s_create = Arc::clone(&state);
        let s_input = Arc::clone(&state);
        let s_keys = Arc::clone(&state);
        let s_capture = Arc::clone(&state);
        let s_kill = Arc::clone(&state);
        let s_list = Arc::clone(&state);
        let s_subscribe = Arc::clone(&state);
        let s_disconnect = Arc::clone(&state);

        // session:create — create a new tmux session
        // Payload: { provider: string, command?: string[], mode?: "ephemeral"|"persistent", env?: {} }
        // ACK:    { session_id: string, status: "running" } | { error: string }
        socket.on(
            "session:create",
            move |s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_create);
                async move {
                    let result = handle_create(s, data, &state).await;
                    let _ = ack.send(&result);
                }
            },
        );

        // session:input — send text input to a session (appends Enter)
        // Payload: { session_id: string, input: string }
        // ACK:    { ok: true } | { error: string }
        socket.on(
            "session:input",
            move |_s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_input);
                async move {
                    let result = handle_input(data, &state).await;
                    let _ = ack.send(&result);
                }
            },
        );

        // session:keys — send raw key sequences (e.g., "C-c", "Escape", "Enter")
        // Payload: { session_id: string, keys: string }
        // ACK:    { ok: true } | { error: string }
        socket.on(
            "session:keys",
            move |_s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_keys);
                async move {
                    let result = handle_keys(data, &state).await;
                    let _ = ack.send(&result);
                }
            },
        );

        // session:capture — capture current pane content (full scrollback)
        // Payload: { session_id: string }
        // ACK:    { content: string } | { error: string }
        socket.on(
            "session:capture",
            move |_s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_capture);
                async move {
                    let result = handle_capture(data, &state).await;
                    let _ = ack.send(&result);
                }
            },
        );

        // session:kill — kill a session
        // Payload: { session_id: string }
        // ACK:    { ok: true } | { error: string }
        socket.on(
            "session:kill",
            move |_s: SocketRef, Data::<serde_json::Value>(data), ack: AckSender| {
                let state = Arc::clone(&s_kill);
                async move {
                    let result = handle_kill(data, &state).await;
                    let _ = ack.send(&result);
                }
            },
        );

        // session:list — list all managed sessions
        // Payload: {} (empty)
        // ACK:    { sessions: [...] }
        socket.on(
            "session:list",
            move |_s: SocketRef, Data::<serde_json::Value>(_data), ack: AckSender| {
                let state = Arc::clone(&s_list);
                async move {
                    let result = handle_list(&state).await;
                    let _ = ack.send(&result);
                }
            },
        );

        // session:subscribe — join a session's output room
        // Payload: { session_id: string }
        // After subscribing, client receives "session:output" events
        socket.on(
            "session:subscribe",
            move |s: SocketRef, Data::<serde_json::Value>(data)| {
                let state = Arc::clone(&s_subscribe);
                async move {
                    handle_subscribe(s, data, &state).await;
                }
            },
        );

        // Cleanup on disconnect: kill owned ephemeral sessions
        socket.on_disconnect(
            move |s: SocketRef, _reason: socketioxide::socket::DisconnectReason| {
                let state = Arc::clone(&s_disconnect);
                async move {
                    handle_disconnect(s, &state).await;
                }
            },
        );
    });
}

// ─── Handler implementations ────────────────────────────────────────────────

fn get_manager(state: &AppState) -> Result<&TmuxManager, serde_json::Value> {
    state
        .tmux_manager
        .as_deref()
        .ok_or_else(|| json!({"error": "tmux not available"}))
}

async fn handle_create(
    socket: SocketRef,
    data: serde_json::Value,
    state: &AppState,
) -> serde_json::Value {
    let manager = match get_manager(state) {
        Ok(m) => m,
        Err(e) => return e,
    };

    let provider = data["provider"].as_str().unwrap_or("claude-code");
    let mode = match data["mode"].as_str() {
        Some("persistent") => SessionMode::Persistent,
        _ => SessionMode::Ephemeral,
    };

    // Build command from explicit array or use default for provider
    let command = if let Some(cmd_arr) = data["command"].as_array() {
        cmd_arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else {
        default_command_for_provider(provider)
    };

    if command.is_empty() {
        return json!({"error": "no command specified and no default for provider"});
    }

    // Parse optional env vars
    let env_vars = data["env"].as_object().map(|obj| {
        obj.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    });

    let owner_id = Some(socket.id.to_string());

    match manager
        .create_session(provider, command, mode, owner_id, env_vars)
        .await
    {
        Ok(session_id) => {
            // Auto-join the creator to the session's output room
            let room = format!("session:{session_id}");
            let _ = socket.join(room);

            // Spawn output relay task
            if let Some(io) = &state.io {
                spawn_output_relay(manager, &session_id, io.clone());
            }

            json!({"session_id": session_id, "status": "running"})
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn handle_input(data: serde_json::Value, state: &AppState) -> serde_json::Value {
    let manager = match get_manager(state) {
        Ok(m) => m,
        Err(e) => return e,
    };

    let session_id = match data["session_id"].as_str() {
        Some(id) => id,
        None => return json!({"error": "missing session_id"}),
    };
    let input = match data["input"].as_str() {
        Some(i) => i,
        None => return json!({"error": "missing input"}),
    };

    match manager.send_input(session_id, input).await {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn handle_keys(data: serde_json::Value, state: &AppState) -> serde_json::Value {
    let manager = match get_manager(state) {
        Ok(m) => m,
        Err(e) => return e,
    };

    let session_id = match data["session_id"].as_str() {
        Some(id) => id,
        None => return json!({"error": "missing session_id"}),
    };
    let keys = match data["keys"].as_str() {
        Some(k) => k,
        None => return json!({"error": "missing keys"}),
    };

    match manager.send_keys(session_id, keys).await {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn handle_capture(data: serde_json::Value, state: &AppState) -> serde_json::Value {
    let manager = match get_manager(state) {
        Ok(m) => m,
        Err(e) => return e,
    };

    let session_id = match data["session_id"].as_str() {
        Some(id) => id,
        None => return json!({"error": "missing session_id"}),
    };

    match manager.capture_pane(session_id).await {
        Ok(content) => json!({"content": content}),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn handle_kill(data: serde_json::Value, state: &AppState) -> serde_json::Value {
    let manager = match get_manager(state) {
        Ok(m) => m,
        Err(e) => return e,
    };

    let session_id = match data["session_id"].as_str() {
        Some(id) => id,
        None => return json!({"error": "missing session_id"}),
    };

    match manager.kill_session(session_id).await {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn handle_list(state: &AppState) -> serde_json::Value {
    let manager = match get_manager(state) {
        Ok(m) => m,
        Err(e) => return e,
    };

    let sessions = manager.list_sessions().await;
    let sessions_json: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "session_id": s.session_id,
                "provider": s.provider,
                "mode": s.mode,
                "status": s.status,
                "created_at": s.created_at.to_rfc3339(),
                "last_activity": s.last_activity.to_rfc3339(),
                "owner_id": s.owner_id,
            })
        })
        .collect();

    json!({"sessions": sessions_json})
}

async fn handle_subscribe(socket: SocketRef, data: serde_json::Value, state: &AppState) {
    let manager = match &state.tmux_manager {
        Some(m) => m,
        None => {
            warn!(sid = %socket.id, "subscribe failed: tmux not available");
            return;
        }
    };

    let session_id = match data["session_id"].as_str() {
        Some(id) => id,
        None => {
            warn!(sid = %socket.id, "subscribe failed: missing session_id");
            return;
        }
    };

    if manager.get_session(session_id).await.is_none() {
        warn!(sid = %socket.id, session = %session_id, "subscribe failed: session not found");
        return;
    }

    let room = format!("session:{session_id}");
    let _ = socket.join(room);
    debug!(sid = %socket.id, session = %session_id, "client subscribed to session output");
}

async fn handle_disconnect(socket: SocketRef, state: &AppState) {
    let owner_id = socket.id.to_string();
    info!(sid = %owner_id, "client disconnected from gateway Socket.IO");

    if let Some(manager) = &state.tmux_manager {
        manager.kill_owned_ephemeral(&owner_id).await;
    }
}

// ─── Output relay ───────────────────────────────────────────────────────────

/// Spawn a background task that relays output from a tmux session to Socket.IO subscribers.
fn spawn_output_relay(manager: &TmuxManager, session_id: &str, io: SocketIo) {
    let mut rx = match tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(manager.subscribe_output(session_id))
    }) {
        Ok(rx) => rx,
        Err(e) => {
            error!(session = %session_id, error = %e, "failed to subscribe for output relay");
            return;
        }
    };

    let room = format!("session:{}", session_id);
    let sid = session_id.to_string();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let payload = json!({
                        "session_id": &sid,
                        "line": &event.line,
                        "timestamp": event.timestamp.to_rfc3339(),
                        "is_final": event.is_final,
                    });
                    let _ = io.to(room.clone()).emit("session:output", &payload);

                    if event.is_final {
                        let status_payload = json!({
                            "session_id": &sid,
                            "status": "completed",
                        });
                        let _ = io.to(room.clone()).emit("session:status", &status_payload);
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(session = %sid, skipped = n, "output relay lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    debug!(session = %sid, "output relay channel closed");
                    break;
                }
            }
        }
        debug!(session = %sid, "output relay task finished");
    });
}

// ─── Default commands ───────────────────────────────────────────────────────

/// Return a default interactive command for a CLI provider.
fn default_command_for_provider(provider: &str) -> Vec<String> {
    match provider {
        "claude-code" => {
            let binary = std::env::var("CLAUDE_CODE_BINARY").unwrap_or_else(|_| "claude".into());
            vec![binary]
        }
        "codex-cli" => {
            let binary = std::env::var("CODEX_CLI_BINARY").unwrap_or_else(|_| "codex".into());
            vec![binary]
        }
        "cursor" => {
            let binary =
                std::env::var("CURSOR_AGENT_BINARY").unwrap_or_else(|_| "cursor-agent".into());
            vec![binary]
        }
        _ => vec![],
    }
}
