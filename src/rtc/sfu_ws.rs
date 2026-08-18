//! SFU WebSocket transport: binary protobuf `SfuRequest` / `SfuEvent` framing.
//!
//! Ported from JS `StreamSfuClient` (`send`/`ping`/`notifyLeave`) and stream-py
//! `WebSocketClient`:
//!
//! - the socket carries **binary** protobuf frames in both directions
//! - the client sends [`SfuRequest`] messages (join, health-check ping, leave)
//! - the server sends [`SfuEvent`] messages (join response, subscriber offer,
//!   ICE trickle, participant events, health-check response, …)
//! - the first client frame after open is the `JoinRequest`; the first server
//!   frame is expected to be a `JoinResponse` or an `Error`
//!
//! This module intentionally provides **only** the framed transport: connect,
//! a typed send path, and an async event receiver. The join handshake and
//! reconnect logic (waiting for `JoinResponse`, ping cadence, health watchdog)
//! belong to the join state machine in [`super::join`].

use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

use crate::client::DEFAULT_MAX_WEBSOCKET_MESSAGE_BYTES;

use super::error::{Result, RtcError};
use super::proto::event::{
    HealthCheckRequest, JoinRequest, LeaveCallRequest, SfuEvent, SfuRequest, sfu_request,
};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Connect to the SFU signaling WebSocket at `endpoint` and split it into a
/// [`SfuSender`] (send path) and [`SfuReceiver`] (event stream).
///
/// `endpoint` is `credentials.server.ws_endpoint` with the query string the
/// SFU expects (`?attempt=…&user_id=…&api_key=…&user_session_id=…&cid=…`),
/// built by the join code in [`super::join`].
pub async fn connect(endpoint: &str) -> Result<(SfuSender, SfuReceiver)> {
    connect_with_limit(endpoint, DEFAULT_MAX_WEBSOCKET_MESSAGE_BYTES).await
}

/// Connect with an explicit maximum frame/message size.
pub async fn connect_with_limit(
    endpoint: &str,
    max_message_bytes: usize,
) -> Result<(SfuSender, SfuReceiver)> {
    let config = WebSocketConfig::default()
        .read_buffer_size(max_message_bytes.clamp(1, 128 * 1024))
        .max_frame_size(Some(max_message_bytes))
        .max_message_size(Some(max_message_bytes));
    let (stream, _response) = connect_async_with_config(endpoint, Some(config), false).await?;
    let (sink, source) = stream.split();
    Ok((
        SfuSender {
            sink,
            max_message_bytes,
        },
        SfuReceiver {
            source,
            max_message_bytes,
        },
    ))
}

/// The send half of an SFU WebSocket connection.
///
/// Encodes typed [`SfuRequest`]s to binary protobuf frames.
#[derive(Debug)]
pub struct SfuSender {
    sink: SplitSink<WsStream, Message>,
    max_message_bytes: usize,
}

impl SfuSender {
    /// Send an arbitrary [`SfuRequest`] as a binary protobuf frame.
    pub async fn send(&mut self, request: SfuRequest) -> Result<()> {
        ensure_message_size(
            "SFU WebSocket outbound message",
            request.encoded_len(),
            self.max_message_bytes,
        )?;
        let bytes = Bytes::from(request.encode_to_vec());
        self.sink.send(Message::Binary(bytes)).await?;
        Ok(())
    }

    /// Send the initial `JoinRequest` (the first frame after the socket opens).
    pub async fn send_join(&mut self, join: JoinRequest) -> Result<()> {
        self.send(SfuRequest {
            request_payload: Some(sfu_request::RequestPayload::JoinRequest(join)),
        })
        .await
    }

    /// Send a health-check ping. The SFU replies with a `HealthCheckResponse`
    /// event; the join watchdog uses the cadence (JS 5s, stream-py 10s).
    pub async fn send_health_check(&mut self) -> Result<()> {
        self.send(SfuRequest {
            request_payload: Some(sfu_request::RequestPayload::HealthCheckRequest(
                HealthCheckRequest {},
            )),
        })
        .await
    }

    /// Send a graceful `LeaveCallRequest` before closing.
    pub async fn send_leave(
        &mut self,
        session_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<()> {
        self.send(SfuRequest {
            request_payload: Some(sfu_request::RequestPayload::LeaveCallRequest(
                LeaveCallRequest {
                    session_id: session_id.into(),
                    reason: reason.into(),
                },
            )),
        })
        .await
    }

    /// Close the underlying WebSocket.
    pub async fn close(&mut self) -> Result<()> {
        self.sink.close().await?;
        Ok(())
    }
}

/// The receive half of an SFU WebSocket connection: an async stream of
/// decoded [`SfuEvent`]s.
#[derive(Debug)]
pub struct SfuReceiver {
    source: SplitStream<WsStream>,
    max_message_bytes: usize,
}

impl SfuReceiver {
    /// Await the next [`SfuEvent`].
    ///
    /// Returns `Ok(None)` when the SFU closes the socket. Non-data frames
    /// (ping/pong/text) are skipped; binary frames are decoded as `SfuEvent`.
    pub async fn recv(&mut self) -> Result<Option<SfuEvent>> {
        while let Some(message) = self.source.next().await {
            let message = message.map_err(|error| {
                RtcError::from_websocket_with_boundary(error, "SFU WebSocket inbound message")
            })?;
            match message {
                Message::Binary(bytes) => {
                    ensure_message_size(
                        "SFU WebSocket inbound message",
                        bytes.len(),
                        self.max_message_bytes,
                    )?;
                    let event = SfuEvent::decode(bytes)?;
                    return Ok(Some(event));
                }
                Message::Close(frame) => {
                    let reason = frame
                        .map(|f| format!("{} {}", f.code, f.reason))
                        .unwrap_or_else(|| "no close frame".to_owned());
                    tracing::debug!(reason = %reason, "stream.sfu.ws.closed");
                    return Ok(None);
                }
                // Ping/Pong are handled by the transport; Text is unexpected on
                // the SFU socket. Skip and await the next frame.
                _ => continue,
            }
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

    #[test]
    fn message_limit_rejects_before_decode() {
        ensure_message_size("test WebSocket message", 8, 8).expect("at limit");
        assert!(matches!(
            ensure_message_size("test WebSocket message", 9, 8).expect_err("oversized message"),
            RtcError::SizeLimitExceeded {
                boundary: "test WebSocket message",
                limit: 8,
                actual: 9,
            }
        ));
    }
}
