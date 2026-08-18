//! Coordinator WebSocket auth transport (`WSAuthMessage`, JSON over WS).
//!
//! Ported from videosdk `coordinator.NewClient` + `websocket.go`:
//!
//! - connect to `wss://…/api/v2/connect?api_key=…&user_id=…&stream-auth-type=jwt`
//! - the socket carries **JSON text** frames in both directions
//! - the first client frame is the [`WsAuthMessage`] (`{token, user_details, products?}`)
//! - the first server frame is a `connection.ok` ([`Connected`]) event carrying
//!   the `connection_id`, or a `connection.error` event
//! - the client pings with a `health.check` event (~20s in videosdk)
//!
//! This module covers connect, auth, and typed event decode only. The join and
//! call-watch flow that builds on it lives in [`super::join`].

use std::time::Duration;

use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};
use url::Url;

use crate::client::{DEFAULT_BASE_URL, DEFAULT_MAX_WEBSOCKET_MESSAGE_BYTES};

use super::error::{Result, RtcError, SfuTimeoutError};
use super::identity;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Default coordinator connect WebSocket URL (videosdk `defaultOptions.wsURL`).
pub const DEFAULT_COORDINATOR_WS_URL: &str = "wss://video.stream-io-api.com/api/v2/connect";
pub(crate) const DEFAULT_COORDINATOR_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// The user identity sent in the coordinator auth message.
///
/// Mirrors the coordinator `ConnectUserDetailsRequest`; only `id` is required.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectUserDetails {
    /// The user id to connect as.
    pub id: String,
    /// Optional display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional avatar image URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Optional custom data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<serde_json::Value>,
    /// Connect without appearing online.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invisible: Option<bool>,
}

impl ConnectUserDetails {
    /// Details for a user with just an id.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            image: None,
            custom: None,
            invisible: None,
        }
    }
}

/// The coordinator WebSocket auth message (`WSAuthMessage`), sent as the first
/// frame after the socket opens.
#[derive(Clone, Serialize)]
pub struct WsAuthMessage {
    /// Products this connection subscribes to, e.g. `["video"]`. Optional.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub products: Option<Vec<String>>,
    /// The user JWT.
    pub token: String,
    /// The connecting user's details.
    pub user_details: ConnectUserDetails,
}

impl std::fmt::Debug for WsAuthMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsAuthMessage")
            .field("products", &self.products)
            .field("token", &"<redacted>")
            .field("user_details", &self.user_details)
            .finish()
    }
}

impl WsAuthMessage {
    /// Build an auth message for `user` with `token` and the `video` product.
    pub fn video(token: impl Into<String>, user: ConnectUserDetails) -> Self {
        Self {
            products: Some(vec!["video".to_owned()]),
            token: token.into(),
            user_details: user,
        }
    }
}

/// Result of a successful coordinator auth handshake.
#[derive(Debug, Clone)]
pub struct Connected {
    /// The server-assigned connection id (used as `connection_id` on join REST).
    pub connection_id: String,
    /// The full `connection.ok` event payload.
    pub raw: serde_json::Value,
}

/// A decoded coordinator event (JSON), discriminated by its `type` field.
#[derive(Debug, Clone)]
pub struct CoordinatorEvent {
    /// The event type, e.g. `call.created`, `health.check`, `connection.ok`.
    /// Empty if the payload had no `type` field.
    pub event_type: String,
    /// The full raw event, for callers that want fields beyond `type`.
    pub raw: serde_json::Value,
}

impl CoordinatorEvent {
    fn from_value(raw: serde_json::Value) -> Self {
        let event_type = raw
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_owned();
        Self { event_type, raw }
    }
}

/// Connect to the coordinator WebSocket, authenticate, and split the socket
/// into a [`CoordinatorWs`] (send path), [`CoordinatorEvents`] (event stream),
/// and the [`Connected`] handshake result.
///
/// `ws_url` is usually [`DEFAULT_COORDINATOR_WS_URL`]. The `api_key`/`user_id`
/// are added as query params (`stream-auth-type=jwt`), matching videosdk.
pub async fn connect(
    ws_url: &str,
    api_key: &str,
    user_id: &str,
    auth: &WsAuthMessage,
) -> Result<(CoordinatorWs, CoordinatorEvents, Connected)> {
    connect_with_limit(
        ws_url,
        api_key,
        user_id,
        auth,
        DEFAULT_MAX_WEBSOCKET_MESSAGE_BYTES,
    )
    .await
}

/// Connect with an explicit maximum frame/message size.
pub async fn connect_with_limit(
    ws_url: &str,
    api_key: &str,
    user_id: &str,
    auth: &WsAuthMessage,
    max_message_bytes: usize,
) -> Result<(CoordinatorWs, CoordinatorEvents, Connected)> {
    connect_with_limit_and_timeout(
        ws_url,
        api_key,
        user_id,
        auth,
        max_message_bytes,
        DEFAULT_COORDINATOR_HANDSHAKE_TIMEOUT,
    )
    .await
}

/// Connect with an explicit maximum frame/message size and a bounded handshake
/// timeout. A coordinator that never sends `connection.ok` fails with a
/// [`RtcError::Timeout`] instead of blocking the join indefinitely.
pub(crate) async fn connect_with_limit_and_timeout(
    ws_url: &str,
    api_key: &str,
    user_id: &str,
    auth: &WsAuthMessage,
    max_message_bytes: usize,
    handshake_timeout: Duration,
) -> Result<(CoordinatorWs, CoordinatorEvents, Connected)> {
    tokio::time::timeout(
        handshake_timeout,
        connect_inner(ws_url, api_key, user_id, auth, max_message_bytes),
    )
    .await
    .map_err(|_| {
        RtcError::Timeout(SfuTimeoutError::new(
            "coordinator connection.ok",
            handshake_timeout,
        ))
    })?
}

async fn connect_inner(
    ws_url: &str,
    api_key: &str,
    user_id: &str,
    auth: &WsAuthMessage,
    max_message_bytes: usize,
) -> Result<(CoordinatorWs, CoordinatorEvents, Connected)> {
    let mut url = Url::parse(ws_url).map_err(|e| RtcError::Url(format!("{ws_url:?}: {e}")))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("api_key", api_key);
        pairs.append_pair("user_id", user_id);
        pairs.append_pair("stream-auth-type", "jwt");
    }

    let mut request = url.as_str().into_client_request().map_err(RtcError::from)?;
    if let Ok(value) = HeaderValue::from_str(&identity::client_header()) {
        request.headers_mut().insert("X-Stream-Client", value);
    }

    let config = WebSocketConfig::default()
        .read_buffer_size(max_message_bytes.clamp(1, 128 * 1024))
        .max_frame_size(Some(max_message_bytes))
        .max_message_size(Some(max_message_bytes));
    let (stream, _response) = connect_async_with_config(request, Some(config), false).await?;
    let (mut sink, mut source) = stream.split();

    // First client frame: the JSON auth message.
    let auth_json = serde_json::to_string(auth)?;
    sink.send(Message::Text(auth_json.into())).await?;

    // First server frame: connection.ok or connection.error.
    let connected = await_connection_ok(&mut source, max_message_bytes).await?;

    Ok((
        CoordinatorWs {
            sink,
            max_message_bytes,
        },
        CoordinatorEvents {
            source,
            max_message_bytes,
        },
        connected,
    ))
}

async fn await_connection_ok(
    source: &mut SplitStream<WsStream>,
    max_message_bytes: usize,
) -> Result<Connected> {
    while let Some(message) = source.next().await {
        let message = message.map_err(|error| {
            RtcError::from_websocket_with_boundary(error, "coordinator WebSocket handshake message")
        })?;
        let value: serde_json::Value = match message {
            Message::Text(text) => {
                ensure_message_size(
                    "coordinator WebSocket handshake message",
                    text.len(),
                    max_message_bytes,
                )?;
                serde_json::from_str(&text)?
            }
            Message::Binary(bytes) => {
                ensure_message_size(
                    "coordinator WebSocket handshake message",
                    bytes.len(),
                    max_message_bytes,
                )?;
                serde_json::from_slice(&bytes)?
            }
            Message::Close(frame) => {
                let reason = frame
                    .map(|f| format!("{} {}", f.code, f.reason))
                    .unwrap_or_else(|| "no close frame".to_owned());
                return Err(RtcError::Closed(reason));
            }
            _ => continue,
        };

        match value.get("type").and_then(|t| t.as_str()) {
            Some("connection.error") => {
                let message = value
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown coordinator error")
                    .to_owned();
                return Err(RtcError::Coordinator(message));
            }
            Some("connection.ok") => {
                let connection_id = value
                    .get("connection_id")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default()
                    .to_owned();
                return Ok(Connected {
                    connection_id,
                    raw: value,
                });
            }
            _ => continue,
        }
    }
    Err(RtcError::Closed(
        "coordinator socket closed before connection.ok".to_owned(),
    ))
}

/// Resolve the coordinator connect WebSocket URL for the configured REST
/// environment. The default production base uses the pinned
/// [`DEFAULT_COORDINATOR_WS_URL`]; any custom base (staging, local) derives the
/// WebSocket URL from it so a reconfigured client never silently talks to
/// production.
pub(crate) fn coordinator_ws_url(base_url: &Url) -> Result<Url> {
    let default_base = Url::parse(DEFAULT_BASE_URL)
        .map_err(|error| RtcError::Url(format!("{DEFAULT_BASE_URL:?}: {error}")))?;
    if base_url.scheme() == default_base.scheme()
        && base_url.host_str() == default_base.host_str()
        && base_url.port_or_known_default() == default_base.port_or_known_default()
    {
        return Url::parse(DEFAULT_COORDINATOR_WS_URL)
            .map_err(|error| RtcError::Url(format!("{DEFAULT_COORDINATOR_WS_URL:?}: {error}")));
    }

    let mut url = base_url.clone();
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        "ws" => "ws",
        "wss" => "wss",
        other => {
            return Err(RtcError::Url(format!(
                "unsupported coordinator base URL scheme {other:?}"
            )));
        }
    };
    url.set_scheme(scheme)
        .map_err(|_| RtcError::Url(format!("cannot set WebSocket scheme on {base_url}")))?;
    url.set_path("/api/v2/connect");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

/// The send half of an authenticated coordinator WebSocket.
#[derive(Debug)]
pub struct CoordinatorWs {
    sink: SplitSink<WsStream, Message>,
    max_message_bytes: usize,
}

impl CoordinatorWs {
    /// Send a raw JSON event to the coordinator.
    pub async fn send_json(&mut self, value: &serde_json::Value) -> Result<()> {
        let text = serde_json::to_string(value)?;
        ensure_message_size(
            "coordinator WebSocket outbound message",
            text.len(),
            self.max_message_bytes,
        )?;
        self.sink.send(Message::Text(text.into())).await?;
        Ok(())
    }

    /// Send a `health.check` keep-alive (videosdk cadence ~20s).
    pub async fn send_health_check(&mut self) -> Result<()> {
        let payload = serde_json::json!({ "type": "health.check", "cid": "*" });
        self.send_json(&payload).await
    }

    /// Close the underlying WebSocket.
    pub async fn close(&mut self) -> Result<()> {
        self.sink.close().await?;
        Ok(())
    }
}

/// The receive half of an authenticated coordinator WebSocket: an async stream
/// of decoded [`CoordinatorEvent`]s.
#[derive(Debug)]
pub struct CoordinatorEvents {
    source: SplitStream<WsStream>,
    max_message_bytes: usize,
}

impl CoordinatorEvents {
    /// Await the next coordinator event. Returns `Ok(None)` on socket close.
    pub async fn recv(&mut self) -> Result<Option<CoordinatorEvent>> {
        while let Some(message) = self.source.next().await {
            let message = message.map_err(|error| {
                RtcError::from_websocket_with_boundary(
                    error,
                    "coordinator WebSocket inbound message",
                )
            })?;
            let bytes: Bytes = match message {
                Message::Text(text) => Bytes::from(text.as_str().to_owned()),
                Message::Binary(bytes) => bytes,
                Message::Close(_) => return Ok(None),
                _ => continue,
            };
            ensure_message_size(
                "coordinator WebSocket inbound message",
                bytes.len(),
                self.max_message_bytes,
            )?;
            let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
            return Ok(Some(CoordinatorEvent::from_value(raw)));
        }
        Ok(None)
    }
}

fn ensure_message_size(boundary: &'static str, actual: usize, limit: usize) -> Result<()> {
    if actual > limit {
        return Err(RtcError::SizeLimitExceeded {
            boundary,
            limit,
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[test]
    fn auth_debug_redacts_user_token() {
        let auth =
            WsAuthMessage::video("user-token-must-not-leak", ConnectUserDetails::new("user"));
        let debug = format!("{auth:?}");
        assert!(!debug.contains("user-token-must-not-leak"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("user"));
    }

    #[test]
    fn coordinator_message_limit_accepts_exact_and_rejects_oversized_input() {
        ensure_message_size("coordinator test message", 64, 64).expect("exact limit");
        assert!(matches!(
            ensure_message_size("coordinator test message", 65, 64).expect_err("oversized"),
            RtcError::SizeLimitExceeded {
                boundary: "coordinator test message",
                limit: 64,
                actual: 65,
            }
        ));
    }

    #[test]
    fn coordinator_url_follows_configured_rest_environment() {
        let staging =
            Url::parse("https://video-edge-staging.example.com/video?source=test").expect("url");
        assert_eq!(
            coordinator_ws_url(&staging)
                .expect("staging coordinator URL")
                .as_str(),
            "wss://video-edge-staging.example.com/api/v2/connect"
        );

        let local = Url::parse("http://127.0.0.1:3030/custom").expect("url");
        assert_eq!(
            coordinator_ws_url(&local)
                .expect("local coordinator URL")
                .as_str(),
            "ws://127.0.0.1:3030/api/v2/connect"
        );

        let production = Url::parse(DEFAULT_BASE_URL).expect("url");
        assert_eq!(
            coordinator_ws_url(&production)
                .expect("production coordinator URL")
                .as_str(),
            DEFAULT_COORDINATOR_WS_URL
        );
    }

    #[tokio::test]
    async fn coordinator_handshake_ignores_other_events_and_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local WebSocket");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut socket = accept_async(stream).await.expect("WebSocket handshake");
            socket
                .next()
                .await
                .expect("auth frame")
                .expect("valid frame");
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "type": "health.check",
                        "connection_id": "not-connected-yet"
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("send unrelated event");
            let _ = socket.next().await;
        });

        let timeout = Duration::from_millis(50);
        let auth = WsAuthMessage::video("token", ConnectUserDetails::new("user"));
        let error = connect_with_limit_and_timeout(
            &format!("ws://{address}/api/v2/connect"),
            "key",
            "user",
            &auth,
            1024,
            timeout,
        )
        .await
        .expect_err("missing connection.ok must time out");
        assert!(matches!(
            error,
            RtcError::Timeout(SfuTimeoutError { ref what, timeout: actual })
                if what == "coordinator connection.ok" && actual == timeout
        ));
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("server must observe client cancellation")
            .expect("server task");
    }
}
