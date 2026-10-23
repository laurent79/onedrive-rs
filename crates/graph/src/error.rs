#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Graph returned HTTP {status} for {url}: {body}")]
    Status { status: u16, url: String, body: String },
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("not logged in (no refresh token); run `onedrive-rs login`")]
    NotLoggedIn,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON from Graph: {0}")]
    Json(#[from] serde_json::Error),
    #[error("downloaded content does not match: {0}")]
    Integrity(String),
}

impl Error {
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Status { status, .. } => Some(*status),
            Error::Http(e) => e.status().map(|s| s.as_u16()),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
