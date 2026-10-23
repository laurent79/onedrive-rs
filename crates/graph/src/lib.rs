//! Microsoft Graph access for onedrive-rs: OAuth2 login and token refresh,
//! an HTTP client with throttling-aware retries, typed `/delta` pages and
//! streamed downloads.

pub mod auth;
mod client;
mod error;
pub mod types;

pub use auth::{AuthConfig, LoopbackLogin, ManualLogin, TokenStore, Tokens};
pub use client::{GRAPH_BASE, Graph, http_client};
pub use error::{Error, Result};
pub use types::{DeltaPage, Drive, DriveItem};
