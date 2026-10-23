//! OAuth2 authorisation code flow (public client + PKCE) against the Microsoft
//! identity platform, mirroring `src/onedrive.d` / `src/localAuth.d`.
//!
//! Two interactive flows are offered:
//! - [`LoopbackLogin`]: a one-shot HTTP listener on `127.0.0.1:53100-53149`
//!   receives the redirect from the browser (preferred);
//! - [`ManualLogin`]: the `nativeclient` redirect URI, the user pastes the
//!   final URL back (fallback when no browser/listener is available).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;
use sha2::Digest as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use crate::error::{Error, Result};

pub const DEFAULT_CLIENT_ID: &str = "d50ca740-c83f-4d1b-b616-12c519384f0c";
const AUTHORITY: &str = "https://login.microsoftonline.com/common/oauth2/v2.0";
const NATIVE_CLIENT_REDIRECT: &str = "https://login.microsoftonline.com/common/oauth2/nativeclient";
const LOOPBACK_PORTS: std::ops::RangeInclusive<u16> = 53100..=53149;
const SCOPES_RW: &str = "Files.ReadWrite Files.ReadWrite.All Sites.ReadWrite.All offline_access";
const SCOPES_RO: &str = "Files.Read Files.Read.All Sites.Read.All offline_access";

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub client_id: String,
    pub read_only: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self { client_id: DEFAULT_CLIENT_ID.to_owned(), read_only: false }
    }
}

impl AuthConfig {
    fn scopes(&self) -> &'static str {
        if self.read_only { SCOPES_RO } else { SCOPES_RW }
    }

    fn authorize_url(&self, redirect_uri: &str, state: &str, challenge: &str) -> String {
        let mut url = Url::parse(&format!("{AUTHORITY}/authorize")).expect("static URL");
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("scope", self.scopes())
            .append_pair("response_type", "code")
            .append_pair("prompt", "login")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        url.into()
    }
}

/// Tokens returned by the token endpoint.
#[derive(Debug, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: Instant,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    error_description: Option<String>,
}

async fn token_request(http: &reqwest::Client, form: &[(&str, &str)], previous_refresh: Option<&str>) -> Result<Tokens> {
    let resp = http.post(format!("{AUTHORITY}/token")).form(form).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        let msg = match serde_json::from_str::<TokenError>(&body) {
            Ok(e) => format!("{}: {}", e.error, e.error_description.unwrap_or_default()),
            Err(_) => format!("HTTP {status}"),
        };
        return Err(Error::Auth(msg));
    }
    let t: TokenResponse = serde_json::from_str(&body)?;
    let refresh_token = t
        .refresh_token
        .or_else(|| previous_refresh.map(str::to_owned))
        .ok_or_else(|| Error::Auth("no refresh_token returned (offline_access missing?)".into()))?;
    Ok(Tokens {
        access_token: t.access_token,
        refresh_token,
        expires_at: Instant::now() + Duration::from_secs(t.expires_in),
    })
}

pub(crate) async fn refresh(http: &reqwest::Client, cfg: &AuthConfig, refresh_token: &str) -> Result<Tokens> {
    token_request(
        http,
        &[
            ("client_id", cfg.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", cfg.scopes()),
        ],
        Some(refresh_token),
    )
    .await
}

async fn exchange_code(http: &reqwest::Client, cfg: &AuthConfig, code: &str, redirect_uri: &str, verifier: &str) -> Result<Tokens> {
    token_request(
        http,
        &[
            ("client_id", cfg.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("scope", cfg.scopes()),
        ],
        None,
    )
    .await
}

fn random_b64url(len: usize) -> String {
    let mut buf = vec![0u8; len];
    rand::fill(&mut buf[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

struct Pkce {
    verifier: String,
    challenge: String,
    state: String,
}

impl Pkce {
    fn new() -> Self {
        let verifier = random_b64url(32);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
        Self { verifier, challenge, state: random_b64url(24) }
    }
}

/// Extract `code` from a redirect query string after validating `state`.
fn code_from_query(query: &str, expected_state: &str) -> Result<String> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        let slot = match k.as_ref() {
            "code" => &mut code,
            "state" => &mut state,
            "error_description" => &mut error,
            _ => continue,
        };
        if slot.replace(v.into_owned()).is_some() {
            return Err(Error::Auth(format!("duplicate `{k}` in redirect")));
        }
    }
    if let Some(e) = error {
        return Err(Error::Auth(e));
    }
    if state.as_deref() != Some(expected_state) {
        return Err(Error::Auth("OAuth state mismatch".into()));
    }
    code.ok_or_else(|| Error::Auth("no authorisation code in redirect".into()))
}

/// Browser-based login receiving the redirect on a local loopback port.
pub struct LoopbackLogin {
    cfg: AuthConfig,
    listener: TcpListener,
    redirect_uri: String,
    pkce: Pkce,
    url: String,
}

impl LoopbackLogin {
    pub async fn start(cfg: AuthConfig) -> Result<Self> {
        let mut last_err = None;
        for port in LOOPBACK_PORTS {
            match TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => {
                    let redirect_uri = format!("http://127.0.0.1:{port}/");
                    let pkce = Pkce::new();
                    let url = cfg.authorize_url(&redirect_uri, &pkce.state, &pkce.challenge);
                    return Ok(Self { cfg, listener, redirect_uri, pkce, url });
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(Error::Io(last_err.unwrap_or_else(|| std::io::Error::other("no free loopback port"))))
    }

    /// URL to open in the browser.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Wait for the browser redirect, then exchange the code for tokens.
    /// Requests that are not the genuine redirect are answered with 400 and ignored.
    pub async fn finish(self, http: &reqwest::Client) -> Result<Tokens> {
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            let mut buf = vec![0u8; 8192];
            let mut len = 0;
            let read = tokio::time::timeout(Duration::from_secs(10), async {
                while len < buf.len() {
                    let n = stream.read(&mut buf[len..]).await?;
                    if n == 0 {
                        break;
                    }
                    len += n;
                    if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                std::io::Result::Ok(())
            })
            .await;
            if !matches!(read, Ok(Ok(()))) {
                continue;
            }
            let request = String::from_utf8_lossy(&buf[..len]);
            let target = request
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("GET "))
                .and_then(|l| l.split(' ').next())
                .unwrap_or("");
            let result = match target.strip_prefix("/?") {
                Some(query) => code_from_query(query, &self.pkce.state),
                None => Err(Error::Auth("unexpected request".into())),
            };
            let (status, body) = match &result {
                Ok(_) => ("200 OK", "Authentication complete. You can close this window and return to onedrive-rs."),
                Err(Error::Auth(e)) if e.contains("mismatch") || e.contains("unexpected") => {
                    let _ = write_response(&mut stream, "400 Bad Request", "Bad request.").await;
                    continue;
                }
                Err(_) => ("400 Bad Request", "Authentication failed. See the onedrive-rs output."),
            };
            let _ = write_response(&mut stream, status, body).await;
            let code = result?;
            return exchange_code(http, &self.cfg, &code, &self.redirect_uri, &self.pkce.verifier).await;
        }
    }
}

async fn write_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let html = format!("<!doctype html><meta charset=utf-8><title>onedrive-rs</title><p>{body}</p>");
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.shutdown().await
}

/// Copy-paste login using the `nativeclient` redirect URI.
pub struct ManualLogin {
    cfg: AuthConfig,
    pkce: Pkce,
    url: String,
}

impl ManualLogin {
    pub fn new(cfg: AuthConfig) -> Self {
        let pkce = Pkce::new();
        let url = cfg.authorize_url(NATIVE_CLIENT_REDIRECT, &pkce.state, &pkce.challenge);
        Self { cfg, pkce, url }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// `response_url` is the full URL the browser ended on.
    pub async fn finish(self, http: &reqwest::Client, response_url: &str) -> Result<Tokens> {
        let url = Url::parse(response_url.trim()).map_err(|e| Error::Auth(format!("invalid response URL: {e}")))?;
        let code = code_from_query(url.query().unwrap_or(""), &self.pkce.state)?;
        exchange_code(http, &self.cfg, &code, NATIVE_CLIENT_REDIRECT, &self.pkce.verifier).await
    }
}

/// Persistent refresh token file (mode 0600, replaced atomically).
#[derive(Debug, Clone)]
pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, refresh_token: &str) -> Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(refresh_token.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_query_validation() {
        assert_eq!(code_from_query("code=abc&state=s1", "s1").unwrap(), "abc");
        assert!(code_from_query("code=abc&state=bad", "s1").is_err());
        assert!(code_from_query("code=abc&state=s1&state=s1", "s1").is_err());
        assert!(code_from_query("state=s1", "s1").is_err());
        assert!(code_from_query("error=access_denied&error_description=nope&state=s1", "s1").is_err());
    }

    #[test]
    fn authorize_url_has_pkce() {
        let cfg = AuthConfig::default();
        let p = Pkce::new();
        let url = cfg.authorize_url("http://127.0.0.1:53100/", &p.state, &p.challenge);
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A53100%2F"));
        assert_eq!(p.challenge.len(), 43);
    }
}
