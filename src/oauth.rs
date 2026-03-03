//! OAuth2 Authorization Code + PKCE flow for OpenAI Codex authentication.
//!
//! Implements the browser-based login flow:
//! 1. Generate PKCE parameters (code_verifier, code_challenge, state)
//! 2. Build authorization URL and open browser
//! 3. Listen on localhost for OAuth callback
//! 4. Exchange authorization code for tokens
//! 5. Extract account_id from JWT claims

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::Duration;
use tokio::net::TcpListener;

// ─── Constants ───────────────────────────────────────────────────────────────

const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPES: &str = "openid profile email offline_access api";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(120);

// ─── Types ───────────────────────────────────────────────────────────────────

/// PKCE parameters for the OAuth authorization flow.
pub struct PkceParams {
    pub code_verifier: String,
    pub code_challenge: String,
    pub state: String,
}

/// Token response from the OpenAI OAuth token endpoint.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Callback received from the OAuth redirect.
struct CallbackResult {
    code: String,
}

/// Final result of the OAuth flow.
pub struct OAuthResult {
    pub access_token: String,
    pub account_id: Option<String>,
}

// ─── PKCE ────────────────────────────────────────────────────────────────────

/// Generate PKCE parameters (code_verifier, code_challenge, state).
///
/// - code_verifier: 32 random bytes → base64url (43 chars)
/// - code_challenge: SHA256(code_verifier) → base64url (S256)
/// - state: 16 random bytes → hex (32 chars)
pub fn generate_pkce() -> PkceParams {
    let mut verifier_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut verifier_bytes);
    let code_verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);

    let challenge_hash = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(challenge_hash);

    let mut state_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut state_bytes);
    let state = hex::encode(state_bytes);

    PkceParams {
        code_verifier,
        code_challenge,
        state,
    }
}

// ─── URL building ────────────────────────────────────────────────────────────

/// Minimal percent-encoding for URL query values.
fn urlencode(s: &str) -> String {
    s.replace(' ', "%20")
        .replace(':', "%3A")
        .replace('/', "%2F")
}

/// Build the full OAuth authorization URL with all required query parameters.
pub fn build_authorize_url(pkce: &PkceParams) -> String {
    format!(
        "{OPENAI_AUTHORIZE_URL}?\
         client_id={OPENAI_CLIENT_ID}\
         &redirect_uri={}\
         &response_type=code\
         &scope={}\
         &state={}\
         &code_challenge={}\
         &code_challenge_method=S256\
         &codex_cli_simplified_flow=true\
         &id_token_add_organizations=true",
        urlencode(REDIRECT_URI),
        urlencode(SCOPES),
        pkce.state,
        pkce.code_challenge,
    )
}

// ─── Browser ─────────────────────────────────────────────────────────────────

/// Attempt to open the given URL in the default browser.
///
/// Returns `Ok(true)` if the command launched, `Ok(false)` if no browser
/// command was found, `Err` only on unexpected failures.
pub fn open_browser(url: &str) -> Result<bool> {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else if cfg!(target_os = "windows") {
        "start"
    } else {
        return Ok(false);
    };

    match std::process::Command::new(cmd).arg(url).spawn() {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context("Failed to open browser"),
    }
}

// ─── Callback server ─────────────────────────────────────────────────────────

/// Send a success HTML page back to the browser.
fn send_success_response(stream: &mut std::net::TcpStream) {
    let html = r#"<!DOCTYPE html>
<html><head><title>Authentication Successful</title></head>
<body style="font-family: sans-serif; text-align: center; padding-top: 50px;">
<h1>Authentication Successful!</h1>
<p>You can close this tab and return to the terminal.</p>
</body></html>"#;

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Send an error HTML page back to the browser.
fn send_error_response(stream: &mut std::net::TcpStream, error: &str, description: &str) {
    let html = format!(
        r#"<!DOCTYPE html>
<html><head><title>Authentication Failed</title></head>
<body style="font-family: sans-serif; text-align: center; padding-top: 50px;">
<h1>Authentication Failed</h1>
<p>Error: {error}</p>
<p>{description}</p>
<p>Please close this tab and try again in the terminal.</p>
</body></html>"#
    );

    let response = format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Start a local TCP listener and wait for the OAuth callback.
///
/// Listens on `127.0.0.1:1455`, validates the state parameter, extracts the
/// authorization code, and sends an HTML response to the browser.
async fn wait_for_callback(expected_state: &str) -> Result<CallbackResult> {
    let listener = TcpListener::bind(format!("127.0.0.1:{CALLBACK_PORT}"))
        .await
        .with_context(|| {
            format!(
                "Failed to bind to 127.0.0.1:{CALLBACK_PORT}. Is another process using this port?"
            )
        })?;

    let (stream, _addr) = tokio::time::timeout(CALLBACK_TIMEOUT, listener.accept())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Timed out waiting for OAuth callback after {}s. Did you complete the browser login?",
                CALLBACK_TIMEOUT.as_secs()
            )
        })?
        .context("Failed to accept callback connection")?;

    // into_std() returns a socket still in non-blocking mode (tokio uses
    // non-blocking I/O). We must switch to blocking before synchronous read.
    let mut std_stream = stream
        .into_std()
        .context("Failed to convert async stream")?;
    std_stream
        .set_nonblocking(false)
        .context("Failed to set stream to blocking mode")?;
    std_stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let mut buf = vec![0u8; 4096];
    let n = std_stream
        .read(&mut buf)
        .context("Failed to read callback request")?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse GET /auth/callback?code=...&state=... HTTP/1.1
    let query_string = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|uri| uri.split('?').nth(1))
        .unwrap_or("");

    let params: HashMap<String, String> = query_string
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            Some((parts.next()?.to_string(), parts.next()?.to_string()))
        })
        .collect();

    // Check for OAuth error
    if let Some(error) = params.get("error") {
        let desc = params
            .get("error_description")
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        send_error_response(&mut std_stream, error, desc);
        bail!("OAuth error: {error} - {desc}");
    }

    let code = params
        .get("code")
        .context("No 'code' parameter in callback")?
        .clone();
    let state = params
        .get("state")
        .context("No 'state' parameter in callback")?;

    // Validate state to prevent CSRF
    if state != expected_state {
        send_error_response(&mut std_stream, "state_mismatch", "Security check failed");
        bail!("OAuth state mismatch: expected {expected_state}, got {state}");
    }

    send_success_response(&mut std_stream);

    Ok(CallbackResult { code })
}

// ─── Token exchange ──────────────────────────────────────────────────────────

/// Exchange the authorization code for tokens via POST to the token endpoint.
async fn exchange_code(code: &str, code_verifier: &str) -> Result<TokenResponse> {
    let client = reqwest::Client::new();

    let params = [
        ("grant_type", "authorization_code"),
        ("client_id", OPENAI_CLIENT_ID),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("code_verifier", code_verifier),
    ];

    let response = client
        .post(OPENAI_TOKEN_URL)
        .form(&params)
        .send()
        .await
        .context("Failed to send token exchange request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("Token exchange failed (HTTP {status}): {body}");
    }

    response
        .json::<TokenResponse>()
        .await
        .context("Failed to parse token response JSON")
}

// ─── JWT account extraction ──────────────────────────────────────────────────

/// Extract account_id from the id_token JWT (or access_token if no id_token).
///
/// Decodes the JWT payload without signature verification (we trust the token
/// since it came directly from OpenAI over HTTPS in the token exchange).
pub fn extract_account_id(token_response: &TokenResponse) -> Option<String> {
    let jwt = token_response
        .id_token
        .as_deref()
        .or(Some(token_response.access_token.as_str()))?;

    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    let payload_bytes = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;

    // Try claim keys used by OpenAI tokens
    for key in [
        "account_id",
        "accountId",
        "acct",
        "sub",
        "https://api.openai.com/account_id",
    ] {
        if let Some(value) = payload.get(key).and_then(|v| v.as_str())
            && !value.trim().is_empty()
        {
            return Some(value.to_string());
        }
    }

    None
}

// ─── Public entry point ──────────────────────────────────────────────────────

/// Execute the full OAuth browser flow and return the result.
pub async fn run_oauth_browser_flow() -> Result<OAuthResult> {
    // Step 1: Generate PKCE
    let pkce = generate_pkce();

    // Step 2: Build authorization URL
    let auth_url = build_authorize_url(&pkce);

    // Step 3: Display URL and try to open browser
    println!("Opening browser for OpenAI authentication...\n");
    println!("If the browser doesn't open automatically, visit this URL:\n");
    println!("  {auth_url}\n");

    match open_browser(&auth_url) {
        Ok(true) => println!("Browser opened. Waiting for authentication...\n"),
        Ok(false) => {
            println!("Could not detect a browser. Please open the URL above manually.\n");
        }
        Err(e) => {
            println!("Failed to open browser ({e}). Please open the URL above manually.\n");
        }
    }

    // Step 4-6: Wait for callback
    println!("Listening on http://127.0.0.1:{CALLBACK_PORT} for OAuth callback...");
    let callback = wait_for_callback(&pkce.state).await?;

    println!("Authorization code received. Exchanging for tokens...");

    // Step 7-8: Exchange code for tokens
    let token_response = exchange_code(&callback.code, &pkce.code_verifier).await?;

    // Step 9: Extract account_id from JWT
    let account_id = extract_account_id(&token_response);

    Ok(OAuthResult {
        access_token: token_response.access_token,
        account_id,
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_pkce_format() {
        let pkce = generate_pkce();
        // code_verifier: base64url-encoded 32 bytes = 43 chars
        assert_eq!(pkce.code_verifier.len(), 43);
        // code_challenge: base64url-encoded SHA256 = 43 chars
        assert_eq!(pkce.code_challenge.len(), 43);
        // state: hex-encoded 16 bytes = 32 chars
        assert_eq!(pkce.state.len(), 32);
        // Ensure randomness across calls
        let pkce2 = generate_pkce();
        assert_ne!(pkce.code_verifier, pkce2.code_verifier);
        assert_ne!(pkce.state, pkce2.state);
    }

    #[test]
    fn test_pkce_s256_challenge() {
        let pkce = generate_pkce();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.code_verifier.as_bytes()));
        assert_eq!(pkce.code_challenge, expected);
    }

    #[test]
    fn test_build_authorize_url_contains_required_params() {
        let pkce = generate_pkce();
        let url = build_authorize_url(&pkce);
        assert!(url.starts_with(OPENAI_AUTHORIZE_URL));
        assert!(url.contains(&format!("client_id={OPENAI_CLIENT_ID}")));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("state={}", pkce.state)));
        assert!(url.contains(&format!("code_challenge={}", pkce.code_challenge)));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("id_token_add_organizations=true"));
    }

    #[test]
    fn test_extract_account_id_from_claims() {
        let payload = serde_json::json!({"sub": "user-abc123", "account_id": "acct-xyz"});
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let fake_jwt = format!("eyJ0eXAiOiJKV1QifQ.{payload_b64}.fake_sig");

        let token_response = TokenResponse {
            access_token: "at_xxx".to_string(),
            token_type: Some("bearer".to_string()),
            expires_in: Some(3600),
            refresh_token: None,
            id_token: Some(fake_jwt),
            scope: None,
        };

        // account_id takes priority over sub
        assert_eq!(
            extract_account_id(&token_response).as_deref(),
            Some("acct-xyz")
        );
    }

    #[test]
    fn test_extract_account_id_fallback_to_sub() {
        let payload = serde_json::json!({"sub": "user-fallback"});
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let fake_jwt = format!("eyJ0eXAiOiJKV1QifQ.{payload_b64}.fake_sig");

        let token_response = TokenResponse {
            access_token: fake_jwt,
            token_type: None,
            expires_in: None,
            refresh_token: None,
            id_token: None,
            scope: None,
        };

        assert_eq!(
            extract_account_id(&token_response).as_deref(),
            Some("user-fallback")
        );
    }

    #[test]
    fn test_extract_account_id_non_jwt_returns_none() {
        let token_response = TokenResponse {
            access_token: "sk-proj-not-a-jwt".to_string(),
            token_type: None,
            expires_in: None,
            refresh_token: None,
            id_token: None,
            scope: None,
        };

        assert_eq!(extract_account_id(&token_response), None);
    }

    #[test]
    fn test_urlencode() {
        assert_eq!(urlencode("hello world"), "hello%20world");
        assert_eq!(
            urlencode("http://localhost:1455/auth/callback"),
            "http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"
        );
    }
}
