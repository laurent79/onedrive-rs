//! Authenticated Microsoft Graph client with token refresh and retries.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use reqwest::{Method, RequestBuilder, Response, StatusCode, header};
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex;

use crate::auth::{self, AuthConfig, TokenStore, Tokens};
use crate::error::{Error, Result};
use crate::types::{DELTA_SELECT, DeltaPage, Drive, DriveItem, DriveListing, Page, SiteResource};

pub const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
pub(crate) const USER_AGENT: &str = concat!("ISV|onedrive-rs|onedrive-rs/", env!("CARGO_PKG_VERSION"));
pub(crate) const MAX_ATTEMPTS: u32 = 8;
/// Throttling (429/503) is retried for this long before giving up.
const MAX_THROTTLED: Duration = Duration::from_secs(3600);
/// Refresh the access token when it expires within this margin.
const EXPIRY_MARGIN: Duration = Duration::from_secs(300);

pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .build()?)
}

pub struct Graph {
    pub(crate) http: reqwest::Client,
    cfg: AuthConfig,
    store: TokenStore,
    tokens: Mutex<Option<Tokens>>,
    prefer_add_to_onedrive: AtomicBool,
    /// When Graph throttles one request, every request waits until this instant.
    throttled_until: std::sync::Mutex<Option<Instant>>,
}

impl Graph {
    /// Build a client from a stored refresh token. Fails with [`Error::NotLoggedIn`] if there is none.
    pub fn new(http: reqwest::Client, cfg: AuthConfig, store: TokenStore) -> Result<Self> {
        if store.load()?.is_none() {
            return Err(Error::NotLoggedIn);
        }
        Ok(Self { http, cfg, store, tokens: Mutex::new(None),
            prefer_add_to_onedrive: AtomicBool::new(false),
            throttled_until: std::sync::Mutex::new(None),
        })
    }

    /// Persist tokens obtained from an interactive login.
    pub fn save_login(store: &TokenStore, tokens: &Tokens) -> Result<()> {
        store.save(&tokens.refresh_token)
    }

    /// Send `Prefer: Include-Feature=AddToOneDrive` (personal accounts, or Business with shared items).
    pub fn set_prefer_add_to_onedrive(&self, on: bool) {
        self.prefer_add_to_onedrive.store(on, Ordering::Relaxed);
    }

    async fn access_token(&self, force_refresh: bool) -> Result<String> {
        let mut guard = self.tokens.lock().await;
        if let Some(t) = guard.as_ref()
            && !force_refresh
            && t.expires_at > Instant::now() + EXPIRY_MARGIN
        {
            return Ok(t.access_token.clone());
        }
        let refresh_token = match guard.as_ref() {
            Some(t) => t.refresh_token.clone(),
            None => self.store.load()?.ok_or(Error::NotLoggedIn)?,
        };
        tracing::debug!("refreshing access token");
        let fresh = match auth::refresh(&self.http, &self.cfg, &refresh_token).await {
            Ok(t) => t,
            Err(e) => {
                // Forget the cached token: after `onedrive-rs login` the next call reads the new one from disk.
                *guard = None;
                return Err(e);
            }
        };
        if fresh.refresh_token != refresh_token {
            self.store.save(&fresh.refresh_token)?;
        }
        let access = fresh.access_token.clone();
        *guard = Some(fresh);
        Ok(access)
    }

    /// Sleep while a throttling window announced by Graph is in effect.
    pub(crate) async fn wait_throttle(&self) {
        loop {
            let until = *self.throttled_until.lock().unwrap();
            match until {
                Some(t) if t > Instant::now() => tokio::time::sleep_until(t.into()).await,
                _ => return,
            }
        }
    }

    pub(crate) fn throttle(&self, delay: Duration) {
        let until = Instant::now() + delay;
        let mut guard = self.throttled_until.lock().unwrap();
        if guard.is_none_or(|t| t < until) {
            *guard = Some(until);
        }
    }

    /// Send an authenticated request, retrying on throttling, transient server
    /// errors and network failures. Non-success statuses become [`Error::Status`].
    ///
    /// 429/503 pause *all* requests of this client for `Retry-After` and are
    /// retried for up to [`MAX_THROTTLED`]; other failures up to [`MAX_ATTEMPTS`] times.
    pub(crate) async fn send(&self, method: Method, url: &str, build: impl Fn(RequestBuilder) -> Result<RequestBuilder>) -> Result<Response> {
        let mut attempt = 0;
        let mut force_refresh = false;
        let started = Instant::now();
        loop {
            self.wait_throttle().await;
            attempt += 1;
            let token = self.access_token(force_refresh).await?;
            force_refresh = false;
            let mut req = self.http.request(method.clone(), url).bearer_auth(token);
            if self.prefer_add_to_onedrive.load(Ordering::Relaxed) {
                req = req.header("Prefer", "Include-Feature=AddToOneDrive");
            }
            let delay = match build(req)?.send().await {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status();
                    if status == StatusCode::UNAUTHORIZED && attempt == 1 {
                        force_refresh = true;
                        continue;
                    }
                    let throttled = matches!(status.as_u16(), 429 | 503);
                    let retryable = throttled || matches!(status.as_u16(), 408 | 500 | 502 | 504);
                    let exhausted = if throttled { started.elapsed() > MAX_THROTTLED } else { attempt >= MAX_ATTEMPTS };
                    if !retryable || exhausted {
                        let body = resp.text().await.unwrap_or_default();
                        return Err(Error::Status { status: status.as_u16(), url: url.to_owned(), body });
                    }
                    let retry_after = resp
                        .headers()
                        .get(header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let delay = retry_after.unwrap_or_else(|| backoff(attempt));
                    if throttled {
                        tracing::warn!(%status, attempt, ?delay, "Graph is throttling, pausing all requests");
                        self.throttle(delay);
                        continue;
                    }
                    tracing::warn!(%status, url, attempt, "Graph request failed, retrying");
                    delay
                }
                Err(e) if attempt < MAX_ATTEMPTS && (e.is_timeout() || e.is_connect() || e.is_request()) => {
                    tracing::warn!(error = %e, url, attempt, "network error, retrying");
                    backoff(attempt)
                }
                Err(e) => return Err(e.into()),
            };
            tokio::time::sleep(delay).await;
        }
    }

    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let resp = self.send(Method::GET, url, Ok).await?;
        let bytes = resp.bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub async fn me_drive(&self) -> Result<Drive> {
        self.get_json(&format!("{GRAPH_BASE}/me/drive")).await
    }

    /// Any drive by id, e.g. a SharePoint document library.
    pub async fn drive(&self, drive_id: &str) -> Result<Drive> {
        self.get_json(&format!("{GRAPH_BASE}/drives/{drive_id}")).await
    }

    /// SharePoint sites the account can see, matching `query` (`*` for all).
    pub async fn search_sites(&self, query: &str) -> Result<Vec<SiteResource>> {
        let q: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
        self.collect_pages(format!("{GRAPH_BASE}/sites?search={q}&$select=id,displayName,name,webUrl")).await
    }

    /// Document libraries of a site.
    pub async fn site_drives(&self, site_id: &str) -> Result<Vec<DriveListing>> {
        self.collect_pages(format!("{GRAPH_BASE}/sites/{site_id}/drives?$select=id,name,driveType,webUrl")).await
    }

    /// Follow `@odata.nextLink` (bounded: these lists are for humans to pick from).
    async fn collect_pages<T: DeserializeOwned>(&self, mut url: String) -> Result<Vec<T>> {
        let mut out = Vec::new();
        for _ in 0..20 {
            let page: Page<T> = self.get_json(&url).await?;
            out.extend(page.value);
            match page.next_link {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn drive_root(&self, drive_id: &str) -> Result<DriveItem> {
        self.get_json(&format!("{GRAPH_BASE}/drives/{drive_id}/root")).await
    }

    /// URL of the first page of a full (initial) delta enumeration.
    pub fn delta_initial_url(drive_id: &str, root_id: &str) -> String {
        format!("{GRAPH_BASE}/drives/{drive_id}/items/{root_id}/delta?select={DELTA_SELECT}")
    }

    /// Fetch one delta page (either the initial URL, a `nextLink` or a stored `deltaLink`).
    pub async fn delta_page(&self, url: &str) -> Result<DeltaPage> {
        self.get_json(url).await
    }

    /// Stream the content of a file into `dest`, returning its QuickXorHash and
    /// size. Bytes already in `dest` (an earlier interrupted attempt or run) are
    /// kept and the rest is requested with an HTTP `Range`; the hash always
    /// covers the whole file, so a stale prefix is caught by the caller's check.
    pub async fn download_to(&self, drive_id: &str, item_id: &str, dest: &Path) -> Result<(String, u64)> {
        let url = format!("{GRAPH_BASE}/drives/{drive_id}/items/{item_id}/content");
        let mut state = match tokio::fs::metadata(dest).await {
            Ok(m) if m.is_file() && m.len() > 0 => {
                let path = dest.to_owned();
                tokio::task::spawn_blocking(move || odrs_quickxor::hash_file_state(&path))
                    .await
                    .map_err(std::io::Error::other)??
            }
            _ => (odrs_quickxor::QuickXor::new(), 0),
        };
        if state.1 > 0 {
            tracing::debug!(item_id, offset = state.1, "resuming download");
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.download_from(&url, dest, &mut state).await {
                Ok(()) => return Ok((state.0.finish_base64(), state.1)),
                Err(Error::Http(e)) if attempt < MAX_ATTEMPTS => {
                    tracing::warn!(error = %e, item_id, attempt, offset = state.1, "download interrupted, resuming");
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// One attempt from `state.1` bytes on. `state` always matches what is on disk.
    async fn download_from(&self, url: &str, dest: &Path, state: &mut (odrs_quickxor::QuickXor, u64)) -> Result<()> {
        let offset = state.1;
        let resp = match self
            .send(Method::GET, url, |r| Ok(if offset > 0 { r.header(header::RANGE, format!("bytes={offset}-")) } else { r }))
            .await
        {
            Ok(r) => r,
            // Nothing left to fetch: the caller's hash check decides whether the file is right.
            Err(e) if offset > 0 && e.status() == Some(416) => return Ok(()),
            Err(e) => return Err(e),
        };
        let resumed = offset > 0
            && resp.status() == StatusCode::PARTIAL_CONTENT
            && resp
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with(&format!("bytes {offset}-")));
        let mut file = if resumed {
            tokio::fs::OpenOptions::new().append(true).open(dest).await?
        } else {
            // Fresh download (or the server ignored the range): start over.
            *state = (odrs_quickxor::QuickXor::new(), 0);
            tokio::fs::File::create(dest).await?
        };
        let mut body = resp.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            state.0.update(&chunk);
            state.1 += chunk.len() as u64;
        }
        file.sync_all().await?;
        Ok(())
    }
}

pub(crate) fn backoff(attempt: u32) -> Duration {
    Duration::from_secs((1u64 << attempt.min(6)).min(60))
}
