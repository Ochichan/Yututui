//! OAuth 2.0 Authorization Code + PKCE (RFC 7636) against a loopback redirect.
//!
//! Spotify's rules (mandatory since April 2025): the redirect URI must be HTTPS or an
//! explicit loopback IP literal — `http://127.0.0.1:PORT/callback`, never `localhost`.
//! Users register their own app (Development Mode allows only an allowlist of accounts),
//! so the Client ID comes from config and there is no client secret anywhere.
//!
//! Token storage is a separate `spotify_token.json` (0600, atomic), NOT `config.json`:
//! PKCE refresh tokens rotate on every refresh from inside whatever task owns the client,
//! and config.json already has an owner (the settings screen). Rotation-crash safety:
//! [`refresh`] persists the new token *before* it is first used, so a crash can strand at
//! most one re-auth, never a half-rotated pair.

use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{AUTHORIZE_URL, BODY_MAX, SCOPES, TOKEN_URL};
use crate::util::http::json_limited;
use crate::util::safe_fs;
use crate::util::sanitize::sanitize_error_text;

/// How long we wait for the user to finish in the browser.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
/// Request-head cap for the one callback request we parse.
const REQUEST_HEAD_MAX: usize = 8 * 1024;
/// Consider a token expired this early, absorbing clock skew and in-flight time.
const EXPIRY_SKEW: i64 = 60;

// No `Debug`: access + refresh tokens are bearer credentials.
#[derive(Clone, Serialize, Deserialize)]
pub struct SpotifyToken {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds; already includes the [`EXPIRY_SKEW`] safety margin.
    pub expires_at: i64,
    #[serde(default)]
    pub scopes: String,
    /// The app that minted this token — a changed Client ID invalidates it.
    #[serde(default)]
    pub client_id: String,
}

impl SpotifyToken {
    pub fn is_expired(&self) -> bool {
        crate::signals::unix_now() >= self.expires_at
    }

    pub fn load() -> Option<Self> {
        let path = token_path()?;
        let text = safe_fs::read_to_string_no_symlink(&path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = token_path() else {
            return Ok(());
        };
        safe_fs::write_private_atomic_json(&path, self)
    }

    pub fn delete_saved() -> std::io::Result<()> {
        let Some(path) = token_path() else {
            return Ok(());
        };
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

pub fn token_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "yututui")
        .map(|d| d.data_dir().join("spotify_token.json"))
}

pub fn pending_auth_url_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "yututui")
        .map(|d| d.data_dir().join("spotify_auth_url.txt"))
}

pub fn save_pending_auth_url(url: &str) -> std::io::Result<Option<PathBuf>> {
    let Some(path) = pending_auth_url_path() else {
        return Ok(None);
    };
    safe_fs::write_private_atomic(&path, format!("{url}\n").as_bytes())?;
    Ok(Some(path))
}

pub fn clear_pending_auth_url() -> std::io::Result<()> {
    let Some(path) = pending_auth_url_path() else {
        return Ok(());
    };
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// RFC 7636 §4.1: 43–128 chars from the unreserved set. 64 gives ~380 bits of entropy.
fn pkce_verifier() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut bytes = [0u8; 64];
    getrandom::fill(&mut bytes).expect("os rng");
    bytes
        .iter()
        .map(|b| CHARSET[(*b as usize) % CHARSET.len()] as char)
        .collect()
}

/// RFC 7636 §4.2: BASE64URL-ENCODE(SHA256(ASCII(verifier))), no padding.
fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn random_state() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("os rng");
    bytes
        .iter()
        .map(|b| CHARSET[(*b as usize) % CHARSET.len()] as char)
        .collect()
}

/// Percent-encode everything outside the unreserved set (for the authorize URL we hand
/// to the browser; API calls use reqwest's own query encoding).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

pub fn authorize_url(client_id: &str, port: u16, state: &str, challenge: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge_method=S256&code_challenge={}",
        urlencode(client_id),
        urlencode(&redirect_uri(port)),
        urlencode(SCOPES),
        urlencode(state),
        urlencode(challenge),
    )
}

/// The whole PKCE dance: bind the loopback listener (before the browser opens — a taken
/// port fails fast with a fix-it message), hand the authorize URL to `on_url`, wait for
/// the one callback, exchange the code, persist and return the token.
pub async fn run_pkce_flow(
    http: &reqwest::Client,
    client_id: &str,
    port: u16,
    on_url: &mut (dyn FnMut(String) + Send),
) -> anyhow::Result<SpotifyToken> {
    use anyhow::{Context, bail};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| {
            format!(
                "could not listen on 127.0.0.1:{port} — is another YuTuTui! auth flow (or app) using it? \
                 Set spotify.redirect_port in config.json and update your Spotify app's redirect URI to match"
            )
        })?;

    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let state = random_state();
    on_url(authorize_url(client_id, port, &state, &challenge));

    let code = tokio::time::timeout(CALLBACK_TIMEOUT, wait_for_callback(&listener, &state))
        .await
        .context("timed out waiting for the browser approval (5 minutes)")??;

    // Exchange the code (PKCE: the verifier stands in for a client secret).
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri(port).as_str()),
            ("client_id", client_id),
            ("code_verifier", verifier.as_str()),
        ])
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(sanitize_error_text(format!("token exchange: {e}"))))?;
    let status = resp.status();
    let body: serde_json::Value = json_limited(resp, BODY_MAX).await?;
    if !status.is_success() {
        bail!(
            "token exchange failed (HTTP {status}): {}",
            body.get("error_description")
                .or_else(|| body.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
        );
    }
    let token = token_from_response(&body, client_id, None)
        .context("token exchange returned an unexpected shape")?;
    token.save().context("saving spotify_token.json")?;
    Ok(token)
}

/// Serve the loopback until the `/callback` request arrives; browsers also poke
/// `/favicon.ico` and may pre-connect, so anything else gets a 404 and we keep waiting.
async fn wait_for_callback(
    listener: &tokio::net::TcpListener,
    expected_state: &str,
) -> anyhow::Result<String> {
    use anyhow::bail;
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; REQUEST_HEAD_MAX];
        let mut read = 0;
        // Read until the end of the request head (or the cap); the request line is all we parse.
        while read < buf.len() {
            let n = stream.read(&mut buf[read..]).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            read += n;
            if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf[..read]);
        let Some(path) = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
        else {
            let _ = respond(&mut stream, 400, "Bad request").await;
            continue;
        };
        let Some(query) = path
            .strip_prefix("/callback")
            .and_then(|rest| rest.strip_prefix('?'))
        else {
            let _ = respond(&mut stream, 404, "Not found").await;
            continue;
        };
        let mut code = None;
        let mut state = None;
        let mut error = None;
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "code" => code = Some(percent_decode(v)),
                "state" => state = Some(percent_decode(v)),
                "error" => error = Some(percent_decode(v)),
                _ => {}
            }
        }
        if state.as_deref() != Some(expected_state) {
            let _ = respond(
                &mut stream,
                400,
                "State mismatch — close this tab and retry.",
            )
            .await;
            bail!("authorization response state mismatch");
        }
        if let Some(error) = error {
            let _ = respond(
                &mut stream,
                200,
                "Authorization was denied. You can close this tab.",
            )
            .await;
            bail!("authorization denied: {error}");
        }
        let Some(code) = code.filter(|c| !c.is_empty()) else {
            let _ = respond(&mut stream, 400, "Missing code — close this tab and retry.").await;
            bail!("authorization response had no code");
        };
        let _ = respond(
            &mut stream,
            200,
            "Spotify connected — you can close this tab and return to YuTuTui!.",
        )
        .await;
        return Ok(code);
    }
}

async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>YuTuTui!</title>\
         <body style=\"font-family:system-ui;margin:4rem auto;max-width:36rem;text-align:center\">\
         <h2>YuTuTui!</h2><p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// Refresh (PKCE apps get *rotating* refresh tokens): persist the successor **before**
/// returning it, falling back to the old refresh token when the response omits one.
pub async fn refresh(http: &reqwest::Client, token: &SpotifyToken) -> anyhow::Result<SpotifyToken> {
    use anyhow::{Context, bail};
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", token.refresh_token.as_str()),
            ("client_id", token.client_id.as_str()),
        ])
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(sanitize_error_text(format!("token refresh: {e}"))))?;
    let status = resp.status();
    let body: serde_json::Value = json_limited(resp, BODY_MAX).await?;
    if !status.is_success() {
        bail!(
            "token refresh failed (HTTP {status}): {}",
            body.get("error_description")
                .or_else(|| body.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
        );
    }
    let fresh = token_from_response(&body, &token.client_id, Some(&token.refresh_token))
        .context("token refresh returned an unexpected shape")?;
    fresh.save().context("saving rotated spotify_token.json")?;
    Ok(fresh)
}

fn token_from_response(
    body: &serde_json::Value,
    client_id: &str,
    fallback_refresh: Option<&str>,
) -> Option<SpotifyToken> {
    let access_token = body.get("access_token")?.as_str()?.to_owned();
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .or(fallback_refresh)?
        .to_owned();
    let expires_in = body
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);
    Some(SpotifyToken {
        access_token,
        refresh_token,
        expires_at: crate::signals::unix_now() + expires_in - EXPIRY_SKEW,
        scopes: body
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or(SCOPES)
            .to_owned(),
        client_id: client_id.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// RFC 7636 Appendix B reference vector.
    #[test]
    fn pkce_challenge_matches_rfc_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_shape_is_rfc_legal() {
        let v = pkce_verifier();
        assert_eq!(v.len(), 64);
        assert!(
            v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
        );
        assert_ne!(pkce_verifier(), v, "two verifiers must differ");
    }

    #[test]
    fn authorize_url_encodes_parameters() {
        let url = authorize_url("client123", 9271, "st ate", "chal+lenge");
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A9271%2Fcallback"));
        assert!(url.contains("scope=playlist-read-private%20"));
        assert!(url.contains("state=st%20ate"));
        assert!(url.contains("code_challenge=chal%2Blenge"));
        assert!(url.contains("code_challenge_method=S256"));
    }

    #[test]
    fn redirect_uri_is_loopback_ip_literal() {
        assert_eq!(redirect_uri(9271), "http://127.0.0.1:9271/callback");
        assert!(!redirect_uri(9271).contains("localhost"));
    }

    #[test]
    fn urlencode_leaves_only_unreserved_bytes_plain() {
        assert_eq!(urlencode("AZaz09-._~ +/한"), "AZaz09-._~%20%2B%2F%ED%95%9C");
    }

    #[test]
    fn token_rotation_keeps_old_refresh_when_absent() {
        let body = serde_json::json!({
            "access_token": "new-access",
            "expires_in": 3600,
            "scope": "user-library-read",
        });
        let t = token_from_response(&body, "cid", Some("old-refresh")).unwrap();
        assert_eq!(t.refresh_token, "old-refresh");
        assert_eq!(t.client_id, "cid");
        assert!(!t.is_expired());

        let body = serde_json::json!({
            "access_token": "a", "refresh_token": "rotated", "expires_in": 3600,
        });
        let t = token_from_response(&body, "cid", Some("old")).unwrap();
        assert_eq!(t.refresh_token, "rotated");
    }

    #[test]
    fn token_from_response_defaults_scope_and_rejects_missing_bearer_fields() {
        let body = serde_json::json!({
            "access_token": "access",
            "refresh_token": "refresh",
        });
        let token = token_from_response(&body, "cid", None).unwrap();
        assert_eq!(token.scopes, SCOPES);
        assert_eq!(token.client_id, "cid");
        assert!(token.expires_at <= crate::signals::unix_now() + 3600);

        assert!(token_from_response(&serde_json::json!({}), "cid", Some("r")).is_none());
        assert!(
            token_from_response(&serde_json::json!({"access_token": "a"}), "cid", None).is_none()
        );
    }

    #[test]
    fn percent_decode_handles_common_cases() {
        assert_eq!(percent_decode("a%20b%2Bc+d"), "a b+c d");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    fn callback_request(path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
    }

    async fn send_callback_request(addr: SocketAddr, request: String) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect to callback listener");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write callback request");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("read callback response");
        response
    }

    #[tokio::test]
    async fn wait_for_callback_ignores_noise_then_decodes_code() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind callback listener");
        let addr = listener.local_addr().expect("callback listener address");

        let server = wait_for_callback(&listener, "expected state");
        let client = async {
            let bad = send_callback_request(addr, "\r\n\r\n".to_owned()).await;
            assert!(bad.starts_with("HTTP/1.1 400 Bad Request"));
            assert!(bad.contains("Bad request"));

            let missing = send_callback_request(addr, callback_request("/favicon.ico")).await;
            assert!(missing.starts_with("HTTP/1.1 404 Not Found"));
            assert!(missing.contains("Not found"));

            let ok = send_callback_request(
                addr,
                callback_request("/callback?code=abc%2B123&state=expected+state"),
            )
            .await;
            assert!(ok.starts_with("HTTP/1.1 200 OK"));
            assert!(ok.contains("Spotify connected"));
        };

        let (code, ()) = tokio::join!(server, client);
        assert_eq!(code.expect("callback accepted"), "abc+123");
    }

    #[tokio::test]
    async fn wait_for_callback_rejects_state_mismatch_after_response() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind callback listener");
        let addr = listener.local_addr().expect("callback listener address");

        let server = async {
            wait_for_callback(&listener, "good")
                .await
                .unwrap_err()
                .to_string()
        };
        let client = async {
            send_callback_request(addr, callback_request("/callback?code=abc&state=bad")).await
        };

        let (error, response) = tokio::join!(server, client);
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(response.contains("State mismatch"));
        assert!(error.contains("state mismatch"));
    }

    #[tokio::test]
    async fn wait_for_callback_reports_denied_and_missing_code() {
        let denied = {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind callback listener");
            let addr = listener.local_addr().expect("callback listener address");
            let server = async {
                wait_for_callback(&listener, "ok")
                    .await
                    .unwrap_err()
                    .to_string()
            };
            let client = async {
                send_callback_request(
                    addr,
                    callback_request("/callback?error=access_denied&state=ok"),
                )
                .await
            };
            let (error, response) = tokio::join!(server, client);
            assert!(response.starts_with("HTTP/1.1 200 OK"));
            assert!(response.contains("Authorization was denied"));
            error
        };
        assert!(denied.contains("authorization denied: access_denied"));

        let missing_code = {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind callback listener");
            let addr = listener.local_addr().expect("callback listener address");
            let server = async {
                wait_for_callback(&listener, "ok")
                    .await
                    .unwrap_err()
                    .to_string()
            };
            let client =
                async { send_callback_request(addr, callback_request("/callback?state=ok")).await };
            let (error, response) = tokio::join!(server, client);
            assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
            assert!(response.contains("Missing code"));
            error
        };
        assert!(missing_code.contains("no code"));
    }
}
