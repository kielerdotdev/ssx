//! OAuth 2.0 authorization-code flow with PKCE for desktop apps (RFC 6749 + RFC 7636 +
//! RFC 8252 loopback redirect).
//!
//! Flow: bind `127.0.0.1:0`, build the authorization URL (random `state`, `S256` PKCE
//! challenge), hand it to a [`BrowserOpener`], wait for the browser to be redirected to the
//! loopback listener, verify `state`, exchange the code for tokens, persist them in the
//! [`SecretStore`]. [`OAuthClient::access_token`] returns a valid token, refreshing (once,
//! even under concurrent callers, because rotating refresh tokens are single-use) when it is
//! about to expire.
//!
//! Hardening of the loopback listener: it only binds the loopback interface, rejects
//! requests whose `Host` header is not the loopback address (DNS rebinding), ignores other
//! paths (favicon requests must not abort the flow), refuses callbacks whose `state` does not
//! match without consuming the flow, bounds request size and read time, and gives up after
//! [`OAuthConfig::timeout`] or on cancellation.
//!
//! The opener is injected so tests never launch a browser and the UI crate can use the
//! platform's default handler.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rand::Rng as _;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use crate::context::UploadContext;
use crate::error::UploadError;
use crate::http;
use crate::util::url_encode;

/// Refresh this long before the recorded expiry.
const EXPIRY_SKEW_SECS: u64 = 60;
/// Largest callback request we read.
const MAX_REQUEST_BYTES: usize = 16 * 1024;
/// How long one loopback connection may take to send its request.
const CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens the authorization URL in the user's browser.
pub trait BrowserOpener: Send + Sync {
    /// Open `url`. An `Err` aborts the flow with the URL in the message so the user can open
    /// it manually.
    fn open(&self, url: &str) -> Result<(), String>;
}

/// How the client authenticates at the token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClientAuth {
    /// `client_id` (and `client_secret`) in the form body.
    #[default]
    Body,
    /// HTTP Basic with `client_id:client_secret`.
    BasicHeader,
}

/// Provider configuration.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    /// Provider name; part of the secret store key for the tokens.
    pub name: String,
    /// Client id.
    pub client_id: String,
    /// Client secret for providers that insist on one even for desktop apps.
    pub client_secret: Option<String>,
    /// Authorization endpoint.
    pub auth_url: String,
    /// Token endpoint.
    pub token_url: String,
    /// Requested scopes.
    pub scopes: Vec<String>,
    /// Extra authorization query parameters (`access_type=offline`, `prompt=consent`).
    pub extra_auth_params: Vec<(String, String)>,
    /// Send a PKCE challenge (disable only for providers that reject it, such as Imgur).
    pub use_pkce: bool,
    /// Path of the loopback redirect.
    pub callback_path: String,
    /// Give up waiting for the browser after this long.
    pub timeout: Duration,
    /// Token endpoint client authentication.
    pub client_auth: ClientAuth,
}

impl OAuthConfig {
    /// A PKCE public client with sensible defaults.
    pub fn new(name: &str, client_id: &str, auth_url: &str, token_url: &str) -> Self {
        Self {
            name: name.to_owned(),
            client_id: client_id.to_owned(),
            client_secret: None,
            auth_url: auth_url.to_owned(),
            token_url: token_url.to_owned(),
            scopes: Vec::new(),
            extra_auth_params: Vec::new(),
            use_pkce: true,
            callback_path: "/callback".to_owned(),
            timeout: Duration::from_secs(300),
            client_auth: ClientAuth::Body,
        }
    }

    /// Imgur: authorization-code flow without PKCE (Imgur ignores it) but with the client
    /// secret from the registered application.
    pub fn imgur(client_id: &str, client_secret: &str) -> Self {
        let mut c = Self::new(
            "imgur",
            client_id,
            "https://api.imgur.com/oauth2/authorize",
            "https://api.imgur.com/oauth2/token",
        );
        c.client_secret = Some(client_secret.to_owned());
        c.use_pkce = false;
        c
    }

    fn tokens_key(&self) -> String {
        format!("oauth.{}.tokens", self.name)
    }
}

/// A PKCE verifier and its `S256` challenge.
#[derive(Clone, PartialEq, Eq)]
pub struct Pkce {
    /// The secret kept until the token exchange.
    pub verifier: String,
    /// `BASE64URL(SHA256(verifier))`, sent with the authorization request.
    pub challenge: String,
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pkce").field("challenge", &self.challenge).finish_non_exhaustive()
    }
}

impl Pkce {
    /// A fresh 256-bit verifier (43 characters, within RFC 7636's 43..128).
    pub fn generate() -> Self {
        let verifier = random_token(32);
        let challenge = Self::challenge_for(&verifier);
        Self { verifier, challenge }
    }

    /// The S256 challenge for `verifier`.
    pub fn challenge_for(verifier: &str) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    }
}

fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill(buf.as_mut_slice());
    URL_SAFE_NO_PAD.encode(buf)
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// OAuth tokens as persisted in the secret store.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSet {
    /// Access token.
    pub access_token: String,
    /// Refresh token, when the provider issued one.
    pub refresh_token: Option<String>,
    /// Usually `Bearer`.
    pub token_type: String,
    /// Granted scope, when reported.
    pub scope: Option<String>,
    /// Unix time (seconds) at which the access token expires, when known.
    pub expires_at: Option<u64>,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("token_type", &self.token_type)
            .field("scope", &self.scope)
            .field("expires_at", &self.expires_at)
            .field("has_refresh_token", &self.refresh_token.is_some())
            .finish_non_exhaustive()
    }
}

impl TokenSet {
    fn expired(&self, now: u64) -> bool {
        self.expires_at.is_some_and(|e| now + EXPIRY_SKEW_SECS >= e)
    }
}

/// The OAuth client: authorization, refresh and persistence for one provider.
#[derive(Clone)]
pub struct OAuthClient {
    cfg: Arc<OAuthConfig>,
    ctx: UploadContext,
    opener: Arc<dyn BrowserOpener>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient").field("provider", &self.cfg.name).finish_non_exhaustive()
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Parameters of the loopback callback.
struct Callback {
    code: String,
}

impl OAuthClient {
    /// Build a client. `ctx` supplies the HTTP client, secret store and cancellation token.
    pub fn new(cfg: OAuthConfig, ctx: UploadContext, opener: Arc<dyn BrowserOpener>) -> Self {
        Self {
            cfg: Arc::new(cfg),
            ctx,
            opener,
            now: Arc::new(unix_now),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Replace the clock (tests).
    #[must_use]
    pub fn with_clock(mut self, now: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.now = now;
        self
    }

    /// Tokens currently in the secret store.
    pub fn stored_tokens(&self) -> Result<Option<TokenSet>, UploadError> {
        let key = self.cfg.tokens_key();
        match self.ctx.secrets.get(&key) {
            Ok(Some(json)) => {
                serde_json::from_str(&json).map(Some).map_err(|e| UploadError::Auth {
                    message: format!("stored OAuth tokens are corrupt ({e}); sign in again"),
                })
            }
            Ok(None) => Ok(None),
            Err(e) => Err(UploadError::Auth {
                message: format!("could not read stored OAuth tokens: {e}"),
            }),
        }
    }

    fn store_tokens(&self, tokens: &TokenSet) -> Result<(), UploadError> {
        let json = serde_json::to_string(tokens)
            .map_err(|e| UploadError::config(format!("cannot serialise tokens: {e}")))?;
        self.ctx.secrets.set(&self.cfg.tokens_key(), &json).map_err(|e| UploadError::Auth {
            message: format!("could not store OAuth tokens: {e}"),
        })
    }

    /// Forget the stored tokens.
    pub fn sign_out(&self) -> Result<(), UploadError> {
        self.ctx.secrets.delete(&self.cfg.tokens_key()).map_err(|e| UploadError::Auth {
            message: format!("could not delete OAuth tokens: {e}"),
        })
    }

    fn authorization_url(&self, redirect_uri: &str, state: &str, pkce: Option<&Pkce>) -> String {
        let mut params: Vec<(String, String)> = vec![
            ("response_type".into(), "code".into()),
            ("client_id".into(), self.cfg.client_id.clone()),
            ("redirect_uri".into(), redirect_uri.to_owned()),
            ("state".into(), state.to_owned()),
        ];
        if !self.cfg.scopes.is_empty() {
            params.push(("scope".into(), self.cfg.scopes.join(" ")));
        }
        if let Some(p) = pkce {
            params.push(("code_challenge".into(), p.challenge.clone()));
            params.push(("code_challenge_method".into(), "S256".into()));
        }
        params.extend(self.cfg.extra_auth_params.iter().cloned());
        let query = params
            .iter()
            .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let sep = if self.cfg.auth_url.contains('?') { '&' } else { '?' };
        format!("{}{sep}{query}", self.cfg.auth_url)
    }

    /// Run the interactive authorization and persist the resulting tokens.
    pub async fn authorize(&self) -> Result<TokenSet, UploadError> {
        self.ctx.check_cancelled()?;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| {
            UploadError::io("binding the loopback listener for the OAuth redirect", e)
        })?;
        let port = listener
            .local_addr()
            .map_err(|e| UploadError::io("reading the loopback port", e))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}{}", self.cfg.callback_path);
        let state = random_token(16);
        let pkce = self.cfg.use_pkce.then(Pkce::generate);
        let url = self.authorization_url(&redirect_uri, &state, pkce.as_ref());
        self.opener.open(&url).map_err(|e| UploadError::Auth {
            message: format!("could not open a browser ({e}); open this URL manually: {url}"),
        })?;
        let callback = self.wait_for_callback(&listener, port, &state).await?;
        let mut form = vec![
            ("grant_type", "authorization_code".to_owned()),
            ("code", callback.code),
            ("redirect_uri", redirect_uri),
        ];
        if let Some(p) = &pkce {
            form.push(("code_verifier", p.verifier.clone()));
        }
        let tokens = self.token_request(form).await?;
        self.store_tokens(&tokens)?;
        Ok(tokens)
    }

    async fn wait_for_callback(
        &self,
        listener: &TcpListener,
        port: u16,
        state: &str,
    ) -> Result<Callback, UploadError> {
        let deadline = tokio::time::Instant::now() + self.cfg.timeout;
        loop {
            let accepted = tokio::select! {
                biased;
                () = self.ctx.cancel.cancelled() => return Err(UploadError::Cancelled),
                () = tokio::time::sleep_until(deadline) => {
                    return Err(UploadError::Auth { message: "timed out waiting for the browser to complete sign-in".into() });
                }
                r = listener.accept() => r,
            };
            let Ok((mut sock, _)) = accepted else { continue };
            let Some(request) = read_request(&mut sock).await else {
                let _ = respond(&mut sock, 400, "Bad request").await;
                continue;
            };
            let host_ok = request.host.as_deref().is_some_and(|h| {
                h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
            });
            if !host_ok {
                let _ = respond(&mut sock, 400, "Unexpected Host header").await;
                continue;
            }
            if request.method != "GET" || request.path != self.cfg.callback_path {
                let _ = respond(&mut sock, 404, "Not found").await;
                continue;
            }
            let param =
                |name: &str| request.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
            if !param("state").is_some_and(|s| constant_time_eq(&s, state)) {
                let _ = respond(
                    &mut sock,
                    400,
                    "Invalid state parameter; return to the app and start again.",
                )
                .await;
                tracing::warn!("ignored an OAuth callback with a missing or wrong state");
                continue;
            }
            if let Some(err) = param("error") {
                let _ = respond(
                    &mut sock,
                    200,
                    "Sign-in was not completed. You can close this window.",
                )
                .await;
                let desc = param("error_description").unwrap_or_default();
                return Err(UploadError::Auth {
                    message: format!("authorization denied: {err} {desc}").trim().to_owned(),
                });
            }
            let Some(code) = param("code").filter(|c| !c.is_empty()) else {
                let _ = respond(&mut sock, 400, "Missing authorization code").await;
                continue;
            };
            let _ =
                respond(&mut sock, 200, "Signed in. You can close this window and return to ssx.")
                    .await;
            return Ok(Callback { code });
        }
    }

    /// Exchange a refresh token for new tokens and persist them.
    pub async fn refresh(&self) -> Result<TokenSet, UploadError> {
        let _guard = self.refresh_lock.lock().await;
        let current = self.stored_tokens()?.ok_or_else(|| UploadError::Auth {
            message: "not signed in; authorize first".into(),
        })?;
        self.refresh_locked(&current).await
    }

    async fn refresh_locked(&self, current: &TokenSet) -> Result<TokenSet, UploadError> {
        let refresh = current.refresh_token.clone().ok_or_else(|| UploadError::Auth {
            message: "the access token expired and there is no refresh token; sign in again".into(),
        })?;
        let form =
            vec![("grant_type", "refresh_token".to_owned()), ("refresh_token", refresh.clone())];
        let mut tokens = self.token_request(form).await?;
        if tokens.refresh_token.is_none() {
            // Providers that do not rotate refresh tokens omit it from the response.
            tokens.refresh_token = Some(refresh);
        }
        self.store_tokens(&tokens)?;
        Ok(tokens)
    }

    /// A currently valid access token, refreshing first if it expires within a minute.
    pub async fn access_token(&self) -> Result<String, UploadError> {
        let now = (self.now)();
        let tokens = self.stored_tokens()?.ok_or_else(|| UploadError::Auth {
            message: format!("not signed in to {}; authorize first", self.cfg.name),
        })?;
        if !tokens.expired(now) {
            return Ok(tokens.access_token);
        }
        let _guard = self.refresh_lock.lock().await;
        // Another caller may have refreshed while we waited for the lock.
        let latest = self.stored_tokens()?.unwrap_or(tokens);
        if !latest.expired((self.now)()) {
            return Ok(latest.access_token);
        }
        Ok(self.refresh_locked(&latest).await?.access_token)
    }

    async fn token_request(
        &self,
        mut form: Vec<(&'static str, String)>,
    ) -> Result<TokenSet, UploadError> {
        let mut rb = self
            .ctx
            .http
            .post(&self.cfg.token_url)
            .header(ACCEPT, HeaderValue::from_static("application/json"));
        match (self.cfg.client_auth, &self.cfg.client_secret) {
            (ClientAuth::BasicHeader, secret) => {
                let raw = format!(
                    "{}:{}",
                    url_encode(&self.cfg.client_id),
                    url_encode(secret.as_deref().unwrap_or_default())
                );
                let mut v = HeaderValue::from_str(&format!("Basic {}", STANDARD.encode(raw)))
                    .map_err(|_| {
                        UploadError::config("client credentials are not valid in a header")
                    })?;
                v.set_sensitive(true);
                rb = rb.header(AUTHORIZATION, v);
            }
            (ClientAuth::Body, secret) => {
                form.push(("client_id", self.cfg.client_id.clone()));
                if let Some(s) = secret {
                    form.push(("client_secret", s.clone()));
                }
            }
        }
        let body = form
            .iter()
            .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        rb = rb
            .header(CONTENT_TYPE, HeaderValue::from_static("application/x-www-form-urlencoded"))
            .body(body);
        let resp = http::fetch(&self.ctx, rb, None).await?;
        let json: serde_json::Value =
            serde_json::from_str(&resp.text).unwrap_or(serde_json::Value::Null);
        if let Some(err) = json.get("error").and_then(|e| e.as_str()) {
            let desc = json.get("error_description").and_then(|d| d.as_str()).unwrap_or_default();
            let hint = if err == "invalid_grant" {
                " (the authorization expired or was revoked; sign in again)"
            } else {
                ""
            };
            return Err(UploadError::Auth {
                message: format!("token endpoint rejected the request: {err} {desc}{hint}")
                    .replace("  ", " "),
            });
        }
        if !resp.is_success() {
            return Err(resp.to_error(None));
        }
        let access_token = json
            .get("access_token")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                UploadError::invalid_response(format!(
                    "token response has no access_token: {}",
                    http::snippet(&resp.text)
                ))
            })?
            .to_owned();
        let expires_in = json
            .get("expires_in")
            .and_then(|e| e.as_u64().or_else(|| e.as_str().and_then(|s| s.parse().ok())));
        Ok(TokenSet {
            access_token,
            refresh_token: json.get("refresh_token").and_then(|t| t.as_str()).map(str::to_owned),
            token_type: json
                .get("token_type")
                .and_then(|t| t.as_str())
                .unwrap_or("Bearer")
                .to_owned(),
            scope: json.get("scope").and_then(|s| s.as_str()).map(str::to_owned),
            expires_at: expires_in.map(|s| (self.now)() + s),
        })
    }
}

struct RequestHead {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    host: Option<String>,
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<RequestHead> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 2048];
    let read = async {
        loop {
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                return Some(());
            }
            if buf.len() > MAX_REQUEST_BYTES {
                return None;
            }
        }
    };
    tokio::time::timeout(CONNECTION_READ_TIMEOUT, read).await.ok()??;
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let target = first.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let host = lines.find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("host").then(|| v.trim().to_owned())
    });
    Some(RequestHead {
        method,
        path: path.to_owned(),
        query: url::form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect(),
        host,
    })
}

async fn respond(
    sock: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let escaped = message.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>ssx</title><body style=\"font-family:sans-serif;margin:3em\"><p>{escaped}</p>"
    );
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body.as_bytes()).await?;
    sock.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_appendix_b() {
        assert_eq!(
            Pkce::challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_pkce_is_valid() {
        let p = Pkce::generate();
        assert_eq!(p.verifier.len(), 43);
        assert!(p.verifier.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_eq!(p.challenge, Pkce::challenge_for(&p.verifier));
        assert_ne!(p.verifier, Pkce::generate().verifier);
        assert!(!format!("{p:?}").contains(&p.verifier), "verifier is secret");
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn token_expiry_uses_a_skew() {
        let t = TokenSet {
            access_token: "a".into(),
            refresh_token: None,
            token_type: "Bearer".into(),
            scope: None,
            expires_at: Some(1000),
        };
        assert!(!t.expired(900));
        assert!(t.expired(940));
        assert!(t.expired(2000));
        let never = TokenSet { expires_at: None, ..t };
        assert!(!never.expired(u64::MAX / 2));
    }

    #[test]
    fn token_debug_hides_secrets() {
        let t = TokenSet {
            access_token: "SECRETTOKEN".into(),
            refresh_token: Some("SECRETREFRESH".into()),
            token_type: "Bearer".into(),
            scope: None,
            expires_at: None,
        };
        let s = format!("{t:?}");
        assert!(!s.contains("SECRET"), "{s}");
    }
}
