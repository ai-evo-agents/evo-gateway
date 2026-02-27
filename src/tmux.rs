//! TmuxManager — manages tmux sessions for CLI provider execution.
//!
//! Replaces direct `tokio::process::Command` subprocess spawning with tmux sessions,
//! providing persistent PTY handling, output capture, and session lifecycle management.

use crate::error::GatewayError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, error, info, warn};

// ─── Types ──────────────────────────────────────────────────────────────────

/// Session mode determines lifecycle behavior.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// Created for a single HTTP request, auto-destroyed on completion.
    Ephemeral,
    /// Created via Socket.IO, persists until explicitly killed.
    Persistent,
}

/// Tracks the state of a tmux session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Running,
    Completed,
    Failed,
    Killed,
}

/// Metadata for a managed tmux session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub tmux_name: String,
    pub provider: String,
    pub mode: SessionMode,
    pub owner_id: Option<String>,
    pub status: SessionStatus,
    pub output_file: PathBuf,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
}

/// A line of output from a tmux session, broadcast to subscribers.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OutputEvent {
    pub session_id: String,
    pub line: String,
    pub timestamp: DateTime<Utc>,
    pub is_final: bool,
}

// ─── TmuxManager ────────────────────────────────────────────────────────────

/// Manages tmux session lifecycles for CLI provider execution.
pub struct TmuxManager {
    sessions: RwLock<HashMap<String, SessionInfo>>,
    output_channels: RwLock<HashMap<String, broadcast::Sender<OutputEvent>>>,
    output_dir: PathBuf,
    max_sessions: usize,
}

impl TmuxManager {
    /// Create a new TmuxManager.
    pub fn new(output_dir: PathBuf, max_sessions: usize) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            output_channels: RwLock::new(HashMap::new()),
            output_dir,
            max_sessions,
        }
    }

    /// Check if tmux is available on the system. Returns the version string.
    pub async fn check_tmux_available() -> Result<String, GatewayError> {
        let output = tokio::process::Command::new("tmux")
            .arg("-V")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| {
                GatewayError::TmuxError(format!("failed to run 'tmux -V': {e}. Is tmux installed?"))
            })?;

        if !output.status.success() {
            return Err(GatewayError::TmuxError(format!(
                "tmux -V returned {}",
                output.status
            )));
        }

        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(version)
    }

    /// Kill any orphaned `evo-gw-*` sessions from a previous gateway run.
    pub async fn cleanup_orphaned(&self) {
        match tmux_cmd(&["ls", "-F", "#{session_name}"]).await {
            Ok(output) => {
                for name in output.lines() {
                    let name = name.trim();
                    if name.starts_with("evo-gw-") {
                        info!(session = %name, "killing orphaned tmux session");
                        let _ = tmux_cmd(&["kill-session", "-t", name]).await;
                    }
                }
            }
            Err(_) => {
                // No tmux server running or no sessions — that's fine
            }
        }
    }

    /// Create a new tmux session running the given command.
    pub async fn create_session(
        &self,
        provider: &str,
        command: Vec<String>,
        mode: SessionMode,
        owner_id: Option<String>,
        env_vars: Option<HashMap<String, String>>,
    ) -> Result<String, GatewayError> {
        // Check capacity
        let sessions = self.sessions.read().await;
        let active = sessions
            .values()
            .filter(|s| matches!(s.status, SessionStatus::Starting | SessionStatus::Running))
            .count();
        if active >= self.max_sessions {
            return Err(GatewayError::RateLimitExceeded(format!(
                "tmux session limit reached ({}/{})",
                active, self.max_sessions
            )));
        }
        drop(sessions);

        let session_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let tmux_name = format!("evo-gw-{provider}-{session_id}");

        // Ensure output directory exists
        tokio::fs::create_dir_all(&self.output_dir)
            .await
            .map_err(|e| GatewayError::TmuxError(format!("failed to create output dir: {e}")))?;

        let output_file = self.output_dir.join(format!("{tmux_name}.log"));

        // Build the shell command string to run inside tmux
        let shell_cmd = build_shell_command(&command, &env_vars);

        // Create the tmux session
        tmux_cmd(&[
            "new-session",
            "-d",
            "-s",
            &tmux_name,
            "-x",
            "200",
            "-y",
            "50",
            &shell_cmd,
        ])
        .await
        .map_err(|e| GatewayError::TmuxError(format!("failed to create tmux session: {e}")))?;

        // Set up pipe-pane to capture output to file
        tmux_cmd(&[
            "pipe-pane",
            "-t",
            &tmux_name,
            "-o",
            &format!("cat >> {}", output_file.display()),
        ])
        .await
        .map_err(|e| GatewayError::TmuxError(format!("failed to set up pipe-pane: {e}")))?;

        let now = Utc::now();
        let info = SessionInfo {
            session_id: session_id.clone(),
            tmux_name: tmux_name.clone(),
            provider: provider.to_string(),
            mode,
            owner_id,
            status: SessionStatus::Running,
            output_file: output_file.clone(),
            created_at: now,
            last_activity: now,
        };

        // Create broadcast channel for output events
        let (tx, _) = broadcast::channel::<OutputEvent>(256);

        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(session_id.clone(), info);
        }
        {
            let mut channels = self.output_channels.write().await;
            channels.insert(session_id.clone(), tx.clone());
        }

        // Spawn file-tailing task
        let sid = session_id.clone();
        let tname = tmux_name.clone();
        let ofile = output_file;
        tokio::spawn(async move {
            tail_output_file(sid, tname, ofile, tx).await;
        });

        debug!(session_id = %session_id, tmux_name = %tmux_name, "tmux session created");
        Ok(session_id)
    }

    /// Send text input to a session (appends Enter key).
    pub async fn send_input(&self, session_id: &str, input: &str) -> Result<(), GatewayError> {
        let info = self.get_session_info(session_id).await?;
        tmux_cmd(&["send-keys", "-t", &info.tmux_name, input, "Enter"])
            .await
            .map_err(|e| GatewayError::TmuxError(format!("send_input failed: {e}")))?;
        self.touch_activity(session_id).await;
        Ok(())
    }

    /// Send raw key sequences (e.g., "C-c", "Escape", "Enter").
    pub async fn send_keys(&self, session_id: &str, keys: &str) -> Result<(), GatewayError> {
        let info = self.get_session_info(session_id).await?;
        tmux_cmd(&["send-keys", "-t", &info.tmux_name, keys])
            .await
            .map_err(|e| GatewayError::TmuxError(format!("send_keys failed: {e}")))?;
        self.touch_activity(session_id).await;
        Ok(())
    }

    /// Capture the current pane content (full scrollback snapshot).
    pub async fn capture_pane(&self, session_id: &str) -> Result<String, GatewayError> {
        let info = self.get_session_info(session_id).await?;
        let output = tmux_cmd(&["capture-pane", "-t", &info.tmux_name, "-p", "-S", "-"])
            .await
            .map_err(|e| GatewayError::TmuxError(format!("capture_pane failed: {e}")))?;
        Ok(output)
    }

    /// Subscribe to real-time output from a session.
    pub async fn subscribe_output(
        &self,
        session_id: &str,
    ) -> Result<broadcast::Receiver<OutputEvent>, GatewayError> {
        let channels = self.output_channels.read().await;
        channels
            .get(session_id)
            .map(|tx| tx.subscribe())
            .ok_or_else(|| GatewayError::SessionNotFound(session_id.to_string()))
    }

    /// Wait for a session's process to exit. Polls `tmux list-panes` for pane_dead flag.
    pub async fn wait_for_completion(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<SessionStatus, GatewayError> {
        let info = self.get_session_info(session_id).await?;
        let tmux_name = info.tmux_name.clone();

        let result = tokio::time::timeout(timeout, async {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                if !session_exists(&tmux_name).await {
                    return SessionStatus::Completed;
                }
                if is_pane_dead(&tmux_name).await {
                    // Give a moment for pipe-pane to flush
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    return SessionStatus::Completed;
                }
            }
        })
        .await;

        match result {
            Ok(status) => {
                let mut sessions = self.sessions.write().await;
                if let Some(s) = sessions.get_mut(session_id) {
                    s.status = status.clone();
                }
                Ok(status)
            }
            Err(_) => {
                let mut sessions = self.sessions.write().await;
                if let Some(s) = sessions.get_mut(session_id) {
                    s.status = SessionStatus::Failed;
                }
                Err(GatewayError::SessionTimeout(format!(
                    "session {session_id} timed out after {}s",
                    timeout.as_secs()
                )))
            }
        }
    }

    /// Read the full output capture file for a session.
    pub async fn read_output(&self, session_id: &str) -> Result<String, GatewayError> {
        let info = self.get_session_info(session_id).await?;
        tokio::fs::read_to_string(&info.output_file)
            .await
            .map_err(|e| GatewayError::TmuxError(format!("failed to read output file: {e}")))
    }

    /// Kill a tmux session and clean up.
    pub async fn kill_session(&self, session_id: &str) -> Result<(), GatewayError> {
        let info = {
            let sessions = self.sessions.read().await;
            sessions.get(session_id).cloned()
        };

        if let Some(info) = info {
            // Kill the tmux session (ignore errors — it may already be dead)
            let _ = tmux_cmd(&["kill-session", "-t", &info.tmux_name]).await;

            // Remove output file
            let _ = tokio::fs::remove_file(&info.output_file).await;

            // Remove from registries
            {
                let mut sessions = self.sessions.write().await;
                sessions.remove(session_id);
            }
            {
                let mut channels = self.output_channels.write().await;
                channels.remove(session_id);
            }

            debug!(session_id = %session_id, "tmux session killed and cleaned up");
        }

        Ok(())
    }

    /// List all managed sessions.
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions.values().cloned().collect()
    }

    /// Get info for a specific session.
    pub async fn get_session(&self, session_id: &str) -> Option<SessionInfo> {
        let sessions = self.sessions.read().await;
        sessions.get(session_id).cloned()
    }

    /// Count active (Running/Starting) sessions.
    pub async fn active_count(&self) -> usize {
        let sessions = self.sessions.read().await;
        sessions
            .values()
            .filter(|s| matches!(s.status, SessionStatus::Starting | SessionStatus::Running))
            .count()
    }

    /// Kill stale ephemeral sessions older than `max_age`.
    /// Returns the number of sessions cleaned up.
    pub async fn cleanup_stale(&self, max_age: Duration) -> usize {
        let stale_ids: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions
                .values()
                .filter(|s| {
                    s.mode == SessionMode::Ephemeral
                        && s.last_activity
                            .signed_duration_since(Utc::now())
                            .num_seconds()
                            .unsigned_abs()
                            > max_age.as_secs()
                })
                .map(|s| s.session_id.clone())
                .collect()
        };

        let count = stale_ids.len();
        for id in stale_ids {
            warn!(session_id = %id, "killing stale ephemeral session");
            let _ = self.kill_session(&id).await;
        }
        count
    }

    /// Reconcile internal state with actual tmux sessions.
    /// Marks sessions as Killed if they no longer exist in tmux.
    pub async fn reconcile_with_tmux(&self) {
        let live_sessions = match tmux_cmd(&["ls", "-F", "#{session_name}"]).await {
            Ok(output) => output
                .lines()
                .map(|l| l.trim().to_string())
                .collect::<std::collections::HashSet<_>>(),
            Err(_) => std::collections::HashSet::new(),
        };

        let mut sessions = self.sessions.write().await;
        let dead_ids: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| {
                matches!(s.status, SessionStatus::Running | SessionStatus::Starting)
                    && !live_sessions.contains(&s.tmux_name)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in dead_ids {
            if let Some(s) = sessions.get_mut(&id) {
                debug!(session_id = %id, "session disappeared from tmux, marking as killed");
                s.status = SessionStatus::Killed;
            }
        }
    }

    /// Kill all ephemeral sessions owned by a specific owner (Socket.IO disconnect cleanup).
    pub async fn kill_owned_ephemeral(&self, owner_id: &str) {
        let owned_ids: Vec<String> = {
            let sessions = self.sessions.read().await;
            sessions
                .values()
                .filter(|s| {
                    s.mode == SessionMode::Ephemeral && s.owner_id.as_deref() == Some(owner_id)
                })
                .map(|s| s.session_id.clone())
                .collect()
        };

        for id in owned_ids {
            debug!(session_id = %id, owner = %owner_id, "killing owned ephemeral session on disconnect");
            let _ = self.kill_session(&id).await;
        }
    }

    // ── Private helpers ─────────────────────────────────────────────────────

    async fn get_session_info(&self, session_id: &str) -> Result<SessionInfo, GatewayError> {
        let sessions = self.sessions.read().await;
        sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| GatewayError::SessionNotFound(session_id.to_string()))
    }

    async fn touch_activity(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        if let Some(s) = sessions.get_mut(session_id) {
            s.last_activity = Utc::now();
        }
    }
}

// ─── Tmux command helpers ───────────────────────────────────────────────────

/// Run a tmux command and return stdout.
async fn tmux_cmd(args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new("tmux")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("failed to run tmux: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "tmux {} failed: {stderr}",
            args.first().unwrap_or(&"")
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Check if a tmux session exists by name.
async fn session_exists(name: &str) -> bool {
    tmux_cmd(&["has-session", "-t", name]).await.is_ok()
}

/// Check if the pane's process has exited (pane is dead).
async fn is_pane_dead(session_name: &str) -> bool {
    match tmux_cmd(&["list-panes", "-t", session_name, "-F", "#{pane_dead}"]).await {
        Ok(output) => output.trim() == "1",
        Err(_) => true, // Session doesn't exist = effectively dead
    }
}

/// Build a shell command string from a command vector and optional env vars.
fn build_shell_command(command: &[String], env_vars: &Option<HashMap<String, String>>) -> String {
    let mut parts = Vec::new();

    // Prepend env vars
    if let Some(vars) = env_vars {
        for (k, v) in vars {
            parts.push(format!("{}={}", shell_escape(k), shell_escape(v)));
        }
    }

    // Add the command
    for arg in command {
        parts.push(shell_escape(arg));
    }

    parts.join(" ")
}

/// Simple shell escaping — wraps in single quotes, escaping internal single quotes.
fn shell_escape(s: &str) -> String {
    if s.contains(' ')
        || s.contains('\'')
        || s.contains('"')
        || s.contains('\\')
        || s.contains('$')
        || s.contains('`')
        || s.contains('!')
        || s.contains('(')
        || s.contains(')')
        || s.contains('{')
        || s.contains('}')
        || s.contains('|')
        || s.contains('&')
        || s.contains(';')
        || s.contains('\n')
    {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

// ─── Output file tailing ────────────────────────────────────────────────────

/// Background task that tails the output file and broadcasts lines.
async fn tail_output_file(
    session_id: String,
    tmux_name: String,
    output_file: PathBuf,
    tx: broadcast::Sender<OutputEvent>,
) {
    use tokio::io::AsyncBufReadExt;

    // Wait for the output file to be created
    let mut attempts = 0;
    while !output_file.exists() && attempts < 50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        attempts += 1;
    }

    let file = match tokio::fs::File::open(&output_file).await {
        Ok(f) => f,
        Err(e) => {
            error!(session_id = %session_id, error = %e, "failed to open output file for tailing");
            return;
        }
    };

    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let event = OutputEvent {
                    session_id: session_id.clone(),
                    line,
                    timestamp: Utc::now(),
                    is_final: false,
                };
                // If no receivers, just discard
                let _ = tx.send(event);
            }
            Ok(None) => {
                // EOF — check if the session is still alive
                if !session_exists(&tmux_name).await || is_pane_dead(&tmux_name).await {
                    // Read any remaining content after a small flush delay
                    tokio::time::sleep(Duration::from_millis(300)).await;

                    // Drain remaining lines
                    while let Ok(Some(line)) = lines.next_line().await {
                        let event = OutputEvent {
                            session_id: session_id.clone(),
                            line,
                            timestamp: Utc::now(),
                            is_final: false,
                        };
                        let _ = tx.send(event);
                    }

                    // Send final event
                    let _ = tx.send(OutputEvent {
                        session_id: session_id.clone(),
                        line: String::new(),
                        timestamp: Utc::now(),
                        is_final: true,
                    });
                    break;
                }

                // Session still alive, just no new output yet — wait and retry
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                error!(session_id = %session_id, error = %e, "error reading output file");
                break;
            }
        }
    }

    debug!(session_id = %session_id, "output tailing task finished");
}

// ─── Cleanup loop ───────────────────────────────────────────────────────────

/// Background cleanup loop. Run as a spawned task.
pub async fn cleanup_loop(manager: Arc<TmuxManager>) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;

        let stale_timeout = Duration::from_secs(
            std::env::var("EVO_GATEWAY_TMUX_STALE_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
        );

        let cleaned = manager.cleanup_stale(stale_timeout).await;
        if cleaned > 0 {
            info!(cleaned = cleaned, "cleaned up stale tmux sessions");
        }

        manager.reconcile_with_tmux().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "hello");
        assert_eq!(shell_escape("claude"), "claude");
    }

    #[test]
    fn test_shell_escape_spaces() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    #[test]
    fn test_shell_escape_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_build_shell_command_basic() {
        let cmd = vec!["claude".to_string(), "-p".to_string(), "hello".to_string()];
        let result = build_shell_command(&cmd, &None);
        assert_eq!(result, "claude -p hello");
    }

    #[test]
    fn test_build_shell_command_with_env() {
        let cmd = vec!["claude".to_string()];
        let env = Some(HashMap::from([("FOO".to_string(), "bar".to_string())]));
        let result = build_shell_command(&cmd, &env);
        assert!(result.contains("FOO=bar"));
        assert!(result.contains("claude"));
    }

    #[test]
    fn test_session_mode_serialization() {
        let json = serde_json::to_string(&SessionMode::Ephemeral).unwrap();
        assert_eq!(json, "\"ephemeral\"");

        let json = serde_json::to_string(&SessionMode::Persistent).unwrap();
        assert_eq!(json, "\"persistent\"");
    }
}
