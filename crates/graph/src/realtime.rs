//! Near-real-time change signals from Microsoft Graph's Socket.IO endpoint
//! (`/drives/{id}/root/subscriptions/socketIo`), as the D client's
//! `socketio.d`. A notification carries no change: it only means "run /delta
//! now"; several often come for one change.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

use crate::client::{GRAPH_BASE, Graph, USER_AGENT};

/// Fetch a new endpoint this long before the current one expires (as D).
const RENEW_EARLY: Duration = Duration::from_secs(120);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Until the server sends its own values in the open packet.
const DEFAULT_PING: (Duration, Duration) = (Duration::from_secs(25), Duration::from_secs(60));

/// What a listener reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Listening: changes now arrive within seconds.
    Connected,
    /// Connection lost; retrying. Changes are only seen at the next scheduled pass.
    Disconnected,
    /// Something changed online.
    Changed,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint {
    notification_url: String,
    #[serde(with = "time::serde::rfc3339")]
    expiration_date_time: OffsetDateTime,
}

impl Graph {
    async fn socket_io_endpoint(&self, drive_id: &str) -> crate::Result<Endpoint> {
        self.get_json(&format!("{GRAPH_BASE}/drives/{drive_id}/root/subscriptions/socketIo")).await
    }
}

/// Listen for changes of a drive until `tx` is closed, reconnecting with a
/// capped exponential backoff and renewing the endpoint before it expires.
pub async fn listen(graph: Arc<Graph>, drive_id: String, tx: mpsc::UnboundedSender<Signal>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let failure = match graph.socket_io_endpoint(&drive_id).await {
            Ok(endpoint) => match session(&endpoint, &tx, &mut backoff).await {
                Ok(End::Renew) => continue,
                Ok(End::Closed) => return,
                Err(e) => e,
            },
            Err(e) => format!("cannot get the notification endpoint: {e}"),
        };
        if tx.send(Signal::Disconnected).is_err() {
            return;
        }
        tracing::debug!(drive_id, error = failure, retry_in = ?backoff, "realtime notifications interrupted");
        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            () = tx.closed() => return,
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

enum End {
    /// The endpoint is about to expire: get a new one.
    Renew,
    /// Nobody listens any more.
    Closed,
}

async fn session(endpoint: &Endpoint, tx: &mpsc::UnboundedSender<Signal>, backoff: &mut Duration) -> Result<End, String> {
    let url = ws_url(&endpoint.notification_url).ok_or("invalid notificationUrl")?;
    let renew_in = (endpoint.expiration_date_time - OffsetDateTime::now_utc() - RENEW_EARLY)
        .try_into()
        .unwrap_or(Duration::from_secs(5))
        .max(Duration::from_secs(5));
    let ws = tokio::time::timeout(CONNECT_TIMEOUT, connect(&url)).await.map_err(|_| "connection timed out".to_owned())??;
    let (mut sink, mut stream) = ws.split();

    // Engine.IO open packet first, then join the namespaces (as D: "/" and "/notifications").
    let (ping_interval, ping_timeout) = match tokio::time::timeout(CONNECT_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => match parse(&t) {
            Packet::Open { ping_interval, ping_timeout } => (ping_interval, ping_timeout),
            other => return Err(format!("unexpected first packet {other:?}")),
        },
        Ok(other) => return Err(format!("no open packet: {other:?}")),
        Err(_) => return Err("no open packet in time".into()),
    };
    for ns in ["40", "40/notifications"] {
        sink.send(Message::text(ns)).await.map_err(|e| e.to_string())?;
    }
    tracing::info!(host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default(), "realtime notifications connected");
    *backoff = Duration::from_secs(1);
    tx.send(Signal::Connected).map_err(|_| "closed".to_owned())?;

    let renew = tokio::time::sleep(renew_in);
    tokio::pin!(renew);
    loop {
        let msg = tokio::select! {
            m = tokio::time::timeout(ping_interval + ping_timeout, stream.next()) => m,
            () = &mut renew => {
                let _ = sink.close().await;
                return Ok(End::Renew);
            }
            () = tx.closed() => {
                let _ = sink.close().await;
                return Ok(End::Closed);
            }
        };
        match msg {
            Err(_) => return Err("no ping from the server".into()),
            Ok(None) => return Err("connection closed".into()),
            Ok(Some(Err(e))) => return Err(e.to_string()),
            Ok(Some(Ok(Message::Close(frame)))) => return Err(format!("closed by the server: {frame:?}")),
            Ok(Some(Ok(Message::Text(t)))) => {
                tracing::trace!(text = t.as_str(), "realtime: received");
                match parse(&t) {
                    Packet::Other => tracing::debug!(text = t.as_str(), "realtime: unexpected packet"),
                    Packet::Ping => sink.send(Message::text("3")).await.map_err(|e| e.to_string())?,
                    Packet::Event { name } if name == "notification" => {
                        tracing::debug!("realtime notification");
                        tx.send(Signal::Changed).map_err(|_| "closed".to_owned())?;
                    }
                    Packet::Disconnect => return Err("disconnected by the server".into()),
                    Packet::Open { .. } | Packet::Pong | Packet::Connected(_) | Packet::Event { .. } => {}
                }
            }
            // WebSocket pings are answered by tungstenite itself.
            Ok(Some(Ok(other))) => tracing::trace!(message = ?other, "realtime: non-text message"),
        }
    }
}

type Stream = tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>;

/// TLS with the same stack as the HTTP client (rustls, aws-lc-rs, system trust store).
async fn connect(url: &str) -> Result<Stream, String> {
    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    let host = parsed.host_str().ok_or("no host")?.to_owned();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let tcp = TcpStream::connect((host.as_str(), port)).await.map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from(host).map_err(|e| e.to_string())?;
    let tls = tokio_rustls::TlsConnector::from(tls_config()?).connect(name, tcp).await.map_err(|e| e.to_string())?;
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    request.headers_mut().insert("User-Agent", USER_AGENT.parse().expect("ASCII user agent"));
    let (ws, _) = tokio_tungstenite::client_async(request, tls).await.map_err(|e| e.to_string())?;
    Ok(ws)
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    if let Some(c) = CONFIG.get() {
        return Ok(c.clone());
    }
    use rustls_platform_verifier::BuilderVerifierExt as _;
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_platform_verifier()
        .map_err(|e| e.to_string())?
        .with_no_client_auth();
    Ok(CONFIG.get_or_init(|| Arc::new(config)).clone())
}

/// `https://host/notifications?token=…` → `wss://host/socket.io/?EIO=4&transport=websocket&token=…`
fn ws_url(notification_url: &str) -> Option<String> {
    let u = url::Url::parse(notification_url).ok()?;
    let mut out = format!("wss://{}", u.host_str()?);
    if let Some(port) = u.port() {
        out.push_str(&format!(":{port}"));
    }
    out.push_str("/socket.io/?EIO=4&transport=websocket");
    if let Some(q) = u.query().filter(|q| !q.is_empty()) {
        out.push('&');
        out.push_str(q);
    }
    Some(out)
}

/// The Engine.IO / Socket.IO v4 packets that matter here.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Packet {
    Open { ping_interval: Duration, ping_timeout: Duration },
    Ping,
    Pong,
    /// A namespace was joined.
    Connected(String),
    Disconnect,
    Event { name: String },
    Other,
}

fn parse(text: &str) -> Packet {
    if let Some(json) = text.strip_prefix('0') {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Open {
            ping_interval: Option<u64>,
            ping_timeout: Option<u64>,
        }
        let open: Option<Open> = serde_json::from_str(json).ok();
        let ms = |v: Option<u64>, default: Duration| v.map_or(default, Duration::from_millis);
        return Packet::Open {
            ping_interval: ms(open.as_ref().and_then(|o| o.ping_interval), DEFAULT_PING.0),
            ping_timeout: ms(open.as_ref().and_then(|o| o.ping_timeout), DEFAULT_PING.1),
        };
    }
    match text {
        "2" => return Packet::Ping,
        "3" => return Packet::Pong,
        _ => {}
    }
    // Socket.IO message: "4" + type, then an optional "/namespace," and ack id.
    let Some(rest) = text.strip_prefix('4') else { return Packet::Other };
    let (kind, rest) = rest.split_at(rest.chars().next().map_or(0, char::len_utf8));
    let (namespace, payload) = match rest.strip_prefix('/') {
        Some(r) => match r.split_once(',') {
            Some((ns, p)) => (ns, p),
            None => (r, ""),
        },
        None => ("", rest),
    };
    match kind {
        "0" => Packet::Connected(format!("/{namespace}")),
        "1" => Packet::Disconnect,
        "2" => {
            let payload = payload.trim_start_matches(|c: char| c.is_ascii_digit());
            let name = serde_json::from_str::<Vec<serde_json::Value>>(payload)
                .ok()
                .and_then(|v| v.first().and_then(|n| n.as_str().map(str::to_owned)));
            name.map_or(Packet::Other, |name| Packet::Event { name })
        }
        _ => Packet::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_url() {
        assert_eq!(
            ws_url("https://f3hb0mpua.svc.ms/notifications?token=abc%3D&applicationId=123").as_deref(),
            Some("wss://f3hb0mpua.svc.ms/socket.io/?EIO=4&transport=websocket&token=abc%3D&applicationId=123")
        );
        assert_eq!(ws_url("https://h.example/n").as_deref(), Some("wss://h.example/socket.io/?EIO=4&transport=websocket"));
        assert_eq!(ws_url("not a url"), None);
    }

    #[test]
    fn packets() {
        assert_eq!(
            parse(r#"0{"sid":"x","upgrades":[],"pingInterval":25000,"pingTimeout":20000}"#),
            Packet::Open { ping_interval: Duration::from_secs(25), ping_timeout: Duration::from_secs(20) }
        );
        assert_eq!(parse("0"), Packet::Open { ping_interval: DEFAULT_PING.0, ping_timeout: DEFAULT_PING.1 });
        assert_eq!(parse("2"), Packet::Ping);
        assert_eq!(parse("3"), Packet::Pong);
        assert_eq!(parse(r#"40{"sid":"y"}"#), Packet::Connected("/".into()));
        assert_eq!(parse(r#"40/notifications,{"sid":"z"}"#), Packet::Connected("/notifications".into()));
        assert_eq!(parse("41"), Packet::Disconnect);
        let event = Packet::Event { name: "notification".into() };
        assert_eq!(parse(r#"42/notifications,["notification","{\"clientState\":null}"]"#), event);
        assert_eq!(parse(r#"42["notification",{"a":1}]"#), event);
        assert_eq!(parse(r#"42/notifications,17["notification"]"#), event);
        assert_eq!(parse("42garbage"), Packet::Other);
        assert_eq!(parse("6"), Packet::Other);
    }
}
