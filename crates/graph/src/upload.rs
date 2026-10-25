//! Write operations: folder creation, deletion and file uploads.
//!
//! File content is always streamed from disk, never buffered whole: simple
//! uploads up to [`SIMPLE_UPLOAD_MAX`], upload sessions in [`CHUNK_SIZE`]
//! fragments above that (like the D client). A session kept in a
//! [`SessionStore`] continues where it stopped after an interruption.

use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Method, StatusCode, header};
use serde::Deserialize;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use crate::client::{GRAPH_BASE, Graph, MAX_ATTEMPTS, backoff};
use crate::error::{Error, Result};
use crate::types::DriveItem;

/// Files up to this size use a single PUT (D: `sessionThresholdFileSize`).
pub const SIMPLE_UPLOAD_MAX: u64 = 4 << 20;
/// Session fragment size; must be a multiple of 320 KiB (D: `file_fragment_size` = 10 MiB).
pub const CHUNK_SIZE: u64 = 32 * 320 * 1024;

/// Where uploaded content goes.
#[derive(Debug, Clone)]
pub enum UploadTarget {
    /// A new file named `name` in folder `parent_id`; fails if the name already exists online.
    New { drive_id: String, parent_id: String, name: String },
    /// Replace the content of an existing file. `if_match` (cTag or eTag) makes
    /// the request fail with HTTP 412 if the file changed online meanwhile.
    Replace { drive_id: String, item_id: String, if_match: Option<String> },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadSession {
    upload_url: String,
    expiration_date_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionStatus {
    #[serde(default)]
    next_expected_ranges: Vec<String>,
}

/// Keeps the upload session of one file between runs (D: `session_upload.*`
/// files), so that an interrupted upload continues where it stopped.
pub trait SessionStore: Send + Sync {
    /// The session URL saved for this upload, if the file did not change since.
    fn load(&self) -> Option<String>;
    fn save(&self, upload_url: &str, expiration: Option<&str>);
    fn clear(&self);
}

/// The session URL no longer works: expired (its `tempauth` token can expire
/// before the session, as D notes: 403), gone, or out of sync with the file.
fn session_gone(e: &Error) -> bool {
    matches!(e.status(), Some(403 | 404 | 410 | 416))
}

/// Worth keeping the session to continue later (network, server, throttling).
fn transient(e: &Error) -> bool {
    match e {
        Error::Http(_) | Error::Io(_) => true,
        _ => matches!(e.status(), Some(408 | 429 | 500..=599)),
    }
}

/// Percent-encode one path segment for `items/{id}:/{name}:` addressing.
fn encode_segment(name: &str) -> String {
    url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>().replace('+', "%20")
}

fn child_url(drive_id: &str, parent_id: &str, name: &str) -> String {
    format!("{GRAPH_BASE}/drives/{drive_id}/items/{parent_id}:/{}:", encode_segment(name))
}

impl Graph {
    pub async fn get_item(&self, drive_id: &str, item_id: &str) -> Result<DriveItem> {
        self.get_json(&format!("{GRAPH_BASE}/drives/{drive_id}/items/{item_id}")).await
    }

    /// Child `name` of folder `parent_id`, or `None` if it does not exist.
    pub async fn get_child(&self, drive_id: &str, parent_id: &str, name: &str) -> Result<Option<DriveItem>> {
        match self.get_json(&child_url(drive_id, parent_id, name)).await {
            Ok(item) => Ok(Some(item)),
            Err(e) if e.status() == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Create folder `name` in `parent_id`. If a folder of that name already
    /// exists online it is returned instead.
    pub async fn create_folder(&self, drive_id: &str, parent_id: &str, name: &str) -> Result<DriveItem> {
        let url = format!("{GRAPH_BASE}/drives/{drive_id}/items/{parent_id}/children");
        let body = serde_json::json!({ "name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail" });
        match self.send(Method::POST, &url, |r| Ok(r.json(&body))).await {
            Ok(resp) => Ok(serde_json::from_slice(&resp.bytes().await?)?),
            Err(e) if e.status() == Some(409) => match self.get_child(drive_id, parent_id, name).await? {
                Some(item) if item.is_folder() => Ok(item),
                _ => Err(e),
            },
            Err(e) => Err(e),
        }
    }

    /// Move and/or rename an item; fails with HTTP 409 if the target name exists.
    pub async fn move_item(&self, drive_id: &str, item_id: &str, new_parent_id: &str, new_name: &str) -> Result<DriveItem> {
        let url = format!("{GRAPH_BASE}/drives/{drive_id}/items/{item_id}?@microsoft.graph.conflictBehavior=fail");
        let body = serde_json::json!({ "name": new_name, "parentReference": { "id": new_parent_id } });
        let resp = self.send(Method::PATCH, &url, |r| Ok(r.json(&body))).await?;
        Ok(serde_json::from_slice(&resp.bytes().await?)?)
    }

    /// Delete an item (folders recursively, into the online recycle bin).
    /// Already deleted is success; a changed item (`if_match` mismatch) is HTTP 412.
    pub async fn delete_item(&self, drive_id: &str, item_id: &str, if_match: Option<&str>) -> Result<()> {
        let url = format!("{GRAPH_BASE}/drives/{drive_id}/items/{item_id}");
        let result = self
            .send(Method::DELETE, &url, |r| Ok(match if_match {
                Some(tag) => r.header(header::IF_MATCH, tag),
                None => r,
            }))
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if e.status() == Some(404) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Upload `path` (`size` bytes, last modified `mtime` RFC 3339) to `target`.
    /// Above [`SIMPLE_UPLOAD_MAX`], `sessions` (if any) keeps the session for a later resume.
    pub async fn upload_file(
        &self,
        target: &UploadTarget,
        path: &Path,
        size: u64,
        mtime: &str,
        sessions: Option<&dyn SessionStore>,
    ) -> Result<DriveItem> {
        if size <= SIMPLE_UPLOAD_MAX {
            return self.upload_simple(target, path, size).await;
        }
        if let Some(store) = sessions
            && let Some(url) = store.load()
        {
            match self.resume_offset(target, &url).await? {
                Some(offset) if offset < size => {
                    tracing::info!(path = %path.display(), offset, size, "resuming an interrupted upload");
                    match self.upload_chunks(&url, path, size, offset).await {
                        Ok(item) => {
                            store.clear();
                            return Ok(item);
                        }
                        Err(e) if session_gone(&e) => {
                            tracing::info!(path = %path.display(), error = %e, "upload session lost, starting again");
                            store.clear();
                        }
                        Err(e) => return Err(e),
                    }
                }
                _ => {
                    store.clear();
                    let _ = self.http.delete(&url).send().await;
                }
            }
        }
        self.upload_session(target, path, size, mtime, sessions).await
    }

    /// Where a saved session continues, or `None` if it cannot be used: gone on
    /// the server, or the file it replaces changed online since (resuming would
    /// overwrite that change; a new session then fails `If-Match` as it should).
    async fn resume_offset(&self, target: &UploadTarget, upload_url: &str) -> Result<Option<u64>> {
        if let UploadTarget::Replace { drive_id, item_id, if_match: Some(tag) } = target {
            match self.get_item(drive_id, item_id).await {
                Ok(item) if item.c_tag.as_ref() == Some(tag) || item.e_tag.as_ref() == Some(tag) => {}
                Ok(_) => return Ok(None),
                Err(e) if e.status() == Some(404) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        let resp = self.http.get(upload_url).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            if matches!(status, 403 | 404 | 410) {
                return Ok(None);
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Status { status, url: "upload session".into(), body });
        }
        let status: SessionStatus = serde_json::from_slice(&resp.bytes().await?)?;
        Ok(status.next_expected_ranges.first().and_then(|r| r.split('-').next()?.parse().ok()))
    }

    async fn upload_simple(&self, target: &UploadTarget, path: &Path, size: u64) -> Result<DriveItem> {
        let (url, if_match) = match target {
            UploadTarget::New { drive_id, parent_id, name } => (
                format!("{}/content?@microsoft.graph.conflictBehavior=fail", child_url(drive_id, parent_id, name)),
                None,
            ),
            UploadTarget::Replace { drive_id, item_id, if_match } => {
                (format!("{GRAPH_BASE}/drives/{drive_id}/items/{item_id}/content"), if_match.clone())
            }
        };
        let resp = self
            .send(Method::PUT, &url, |r| {
                let mut r = r
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::CONTENT_LENGTH, size)
                    .body(file_body(path, 0, size)?);
                if let Some(tag) = &if_match {
                    r = r.header(header::IF_MATCH, tag);
                }
                Ok(r)
            })
            .await?;
        Ok(serde_json::from_slice(&resp.bytes().await?)?)
    }

    async fn upload_session(
        &self,
        target: &UploadTarget,
        path: &Path,
        size: u64,
        mtime: &str,
        sessions: Option<&dyn SessionStore>,
    ) -> Result<DriveItem> {
        let (url, if_match, behaviour) = match target {
            UploadTarget::New { drive_id, parent_id, name } => {
                (format!("{}/createUploadSession", child_url(drive_id, parent_id, name)), None, "fail")
            }
            UploadTarget::Replace { drive_id, item_id, if_match } => (
                format!("{GRAPH_BASE}/drives/{drive_id}/items/{item_id}/createUploadSession"),
                if_match.clone(),
                "replace",
            ),
        };
        let body = serde_json::json!({ "item": {
            "@microsoft.graph.conflictBehavior": behaviour,
            "fileSystemInfo": { "lastModifiedDateTime": mtime },
        }});
        let resp = self
            .send(Method::POST, &url, |r| {
                let r = r.json(&body);
                Ok(match &if_match {
                    Some(tag) => r.header(header::IF_MATCH, tag),
                    None => r,
                })
            })
            .await?;
        let session: UploadSession = serde_json::from_slice(&resp.bytes().await?)?;
        if let Some(store) = sessions {
            store.save(&session.upload_url, session.expiration_date_time.as_deref());
        }
        let result = self.upload_chunks(&session.upload_url, path, size, 0).await;
        match &result {
            Ok(_) => {
                if let Some(store) = sessions {
                    store.clear();
                }
            }
            // Kept for the next pass, which continues where this one stopped.
            Err(e) if sessions.is_some() && transient(e) => {}
            Err(_) => {
                if let Some(store) = sessions {
                    store.clear();
                }
                // Best effort: free the server-side session.
                let _ = self.http.delete(&session.upload_url).send().await;
            }
        }
        result
    }

    /// PUT the fragments from `start` to the pre-authenticated session URL (no bearer token).
    async fn upload_chunks(&self, upload_url: &str, path: &Path, size: u64, start: u64) -> Result<DriveItem> {
        let mut offset = start;
        loop {
            let len = CHUNK_SIZE.min(size - offset);
            let mut attempt = 0;
            let resp = loop {
                attempt += 1;
                self.wait_throttle().await;
                let range = format!("bytes {offset}-{}/{size}", offset + len - 1);
                let sent = self
                    .http
                    .put(upload_url)
                    .header(header::CONTENT_LENGTH, len)
                    .header(header::CONTENT_RANGE, range)
                    .body(file_body(path, offset, len)?)
                    .send()
                    .await;
                let delay = match sent {
                    Ok(r) if r.status().is_success() => break r,
                    Ok(r) => {
                        let status = r.status();
                        let retryable = matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504);
                        if !retryable || attempt >= MAX_ATTEMPTS {
                            let body = r.text().await.unwrap_or_default();
                            return Err(Error::Status { status: status.as_u16(), url: "upload session".into(), body });
                        }
                        let retry_after = r
                            .headers()
                            .get(header::RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.trim().parse().ok())
                            .map(Duration::from_secs);
                        let delay = retry_after.unwrap_or_else(|| backoff(attempt));
                        if matches!(status.as_u16(), 429 | 503) {
                            self.throttle(delay);
                        }
                        delay
                    }
                    Err(e) if attempt < MAX_ATTEMPTS => {
                        tracing::warn!(error = %e, offset, attempt, "upload fragment failed, retrying");
                        backoff(attempt)
                    }
                    Err(e) => return Err(e.into()),
                };
                tokio::time::sleep(delay).await;
            };
            offset += len;
            if offset >= size {
                if !matches!(resp.status(), StatusCode::OK | StatusCode::CREATED) {
                    return Err(Error::Integrity(format!("upload session ended with HTTP {}", resp.status())));
                }
                return Ok(serde_json::from_slice(&resp.bytes().await?)?);
            }
        }
    }
}

/// A request body streaming `len` bytes of `path` from `offset`, re-created for every attempt.
fn file_body(path: &Path, offset: u64, len: u64) -> Result<reqwest::Body> {
    let path = PathBuf::from(path);
    let stream = async_stream(path, offset, len);
    Ok(reqwest::Body::wrap_stream(stream))
}

fn async_stream(path: PathBuf, offset: u64, len: u64) -> impl futures_util::Stream<Item = std::io::Result<bytes::Bytes>> + Send + 'static {
    futures_util::stream::unfold(None::<(tokio::io::Take<tokio::fs::File>, Vec<u8>)>, move |state| {
        let path = path.clone();
        async move {
            let (mut reader, mut buf) = match state {
                Some(s) => s,
                None => {
                    let mut file = match tokio::fs::File::open(&path).await {
                        Ok(f) => f,
                        Err(e) => return Some((Err(e), None)),
                    };
                    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
                        return Some((Err(e), None));
                    }
                    (file.take(len), vec![0u8; 64 * 1024])
                }
            };
            match reader.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    let chunk = bytes::Bytes::copy_from_slice(&buf[..n]);
                    Some((Ok(chunk), Some((reader, buf))))
                }
                Err(e) => Some((Err(e), None)),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_encoding() {
        assert_eq!(encode_segment("Rapport final #2.docx"), "Rapport%20final%20%232.docx");
        assert_eq!(encode_segment("été+hiver"), "%C3%A9t%C3%A9%2Bhiver");
    }

    #[test]
    fn chunk_size_is_multiple_of_320k() {
        assert_eq!(CHUNK_SIZE % (320 * 1024), 0);
    }

    #[tokio::test]
    async fn file_body_streams_the_range() {
        use futures_util::StreamExt as _;
        let path = std::env::temp_dir().join(format!("odrs-body-{}", std::process::id()));
        let data: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let mut out = Vec::new();
        let mut s = std::pin::pin!(async_stream(path.clone(), 1000, 150_000));
        while let Some(chunk) = s.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        std::fs::remove_file(&path).unwrap();
        assert_eq!(out, &data[1000..151_000]);
    }
}
