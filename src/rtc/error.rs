//! Error types for the RTC join / signaling paths.
//!
//! Two layers share this module. [`RtcError`] carries the transport surface —
//! Twirp and WebSocket failures — while [`SfuJoinError`], [`SfuTimeoutError`],
//! [`NegotiationError`], and [`WsConnectionError`] form the join/reconnect
//! taxonomy the reconnect state machine dispatches on. The coordinator's
//! `ErrorFromResponse` shape is [`crate::error::ApiError`], re-exported here as
//! [`ErrorFromResponse`] so recovery code can reason about `unrecoverable`.

use std::collections::HashMap;
use std::time::Duration;

use super::proto::models::{self, ErrorCode, WebsocketReconnectStrategy};

/// Result alias for RTC operations.
pub type Result<T> = std::result::Result<T, RtcError>;

/// The coordinator error envelope (`ErrorFromResponse` in JS). Same type as the
/// REST layer's [`crate::error::ApiError`]; re-exported so join/reconnect code
/// can gate retries on `unrecoverable` without a second definition.
pub type ErrorFromResponse = crate::error::ApiError;

/// A failure in the SFU/coordinator signaling transports or the join flow.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RtcError {
    /// HTTP transport failure talking to the SFU Twirp endpoint.
    #[error("transport error: {0}")]
    Transport(#[source] reqwest::Error),

    /// The Twirp endpoint returned a non-200 status with a Twirp error envelope.
    #[error(transparent)]
    Twirp(#[from] TwirpError),

    /// A signal RPC returned HTTP 200 but its protobuf body carried an
    /// application-level `models.Error` (e.g. `ERROR_CODE_PARTICIPANT_*`).
    #[error("sfu signal error (code {code}): {message}")]
    Signal {
        /// The `stream.video.sfu.models.ErrorCode` value.
        code: i32,
        /// Human-readable message from the SFU.
        message: String,
        /// Whether the SFU hinted the call is retryable.
        should_retry: bool,
    },

    /// Failed to decode a protobuf message off the wire.
    #[error("protobuf decode error: {0}")]
    Decode(#[from] prost::DecodeError),

    /// Failed to (de)serialize a JSON payload (coordinator WS / Twirp errors).
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// WebSocket transport failure (SFU or coordinator socket).
    #[error("websocket error: {0}")]
    WebSocket(#[source] Box<tokio_tungstenite::tungstenite::Error>),

    /// A URL could not be parsed or was missing required components.
    #[error("invalid url: {0}")]
    Url(String),

    /// The WebSocket closed before or during the expected exchange.
    #[error("connection closed: {0}")]
    Closed(String),

    /// The coordinator returned a `connection.error` during WS auth.
    #[error("coordinator connection error: {0}")]
    Coordinator(String),

    // join/reconnect taxonomy
    /// The coordinator REST call returned a typed error envelope. Never retried
    /// when `unrecoverable`.
    #[error(transparent)]
    Api(Box<ErrorFromResponse>),

    /// The SFU rejected the join with an `Error` event. Unrecoverable iff the
    /// SFU asked us to `DISCONNECT`.
    #[error(transparent)]
    Join(#[from] SfuJoinError),

    /// A client deadline elapsed waiting for the SFU (WS open or `JoinResponse`).
    #[error(transparent)]
    Timeout(#[from] SfuTimeoutError),

    /// SDP offer/answer negotiation failed. Counted toward the consecutive
    /// negotiation-failure cap.
    #[error(transparent)]
    Negotiation(#[from] NegotiationError),

    /// The signaling WebSocket connection failed. `is_ws_failure` distinguishes
    /// a retriable transport drop from a permanent failure.
    #[error(transparent)]
    WsConnection(#[from] WsConnectionError),

    /// A webrtc-rs (PeerConnection / ICE / DTLS) error.
    #[error("webrtc error: {0}")]
    Webrtc(#[source] Box<webrtc::error::Error>),

    /// The API was used incorrectly (e.g. `join()` called twice).
    #[error("illegal state: {0}")]
    IllegalState(String),

    /// The current participant lacks a capability required for the operation.
    #[error("permission denied: missing `{capability}` capability")]
    PermissionDenied {
        /// The capability required by the rejected operation.
        capability: &'static str,
    },

    /// A media path failure: Opus encode/decode, RTP packetization, publish, or
    /// a republish codec mismatch. Surfaced (not swallowed) so SDK users can see
    /// exactly where the audio/video pipeline broke.
    #[error("media error: {0}")]
    Media(String),

    /// A server-managed layered track was given input that cannot be split
    /// safely into independent encodings by the SDK.
    #[error("{input} is unsupported for server-managed layered video; use write_i420")]
    UnsupportedLayeredInput {
        /// The rejected input path.
        input: &'static str,
    },

    /// The selected codec/track-kind combination has no supported layered
    /// topology in this SDK version.
    #[error("server-managed layering is unsupported for {codec} {track_type:?}")]
    UnsupportedVideoLayering {
        /// Codec subtype requested by the local track.
        codec: String,
        /// Publication kind requested by the caller.
        track_type: models::TrackType,
    },

    /// A PCM write exceeded the track's low-latency queue. The newest samples
    /// were retained and this many oldest samples were discarded.
    #[error(
        "pcm queue overflow: dropped {dropped_samples} oldest samples \
         (capacity {capacity_samples})"
    )]
    PcmQueueOverflow {
        /// Number of oldest queued/input samples discarded by this write.
        dropped_samples: usize,
        /// Maximum number of resampled 48 kHz mono samples retained.
        capacity_samples: usize,
    },

    /// Token minting failed for the participant path.
    ///
    /// Retained for source compatibility. New typed token-boundary failures use
    /// [`RtcError::TokenValidation`].
    #[error("token error: {0}")]
    Token(String),

    /// Token minting or operational validation failed for the participant path.
    #[error(transparent)]
    TokenValidation(#[from] crate::error::TokenError),

    /// A bounded HTTP response or WebSocket message exceeded its configured limit.
    #[error("{boundary} exceeded {limit} bytes (received at least {actual})")]
    SizeLimitExceeded {
        /// Transport boundary that rejected the data.
        boundary: &'static str,
        /// Configured maximum size.
        limit: usize,
        /// Observed size or declared content length.
        actual: usize,
    },
}

impl From<reqwest::Error> for RtcError {
    fn from(e: reqwest::Error) -> Self {
        RtcError::Transport(e)
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for RtcError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        RtcError::WebSocket(Box::new(e))
    }
}

impl From<webrtc::error::Error> for RtcError {
    fn from(e: webrtc::error::Error) -> Self {
        RtcError::Webrtc(Box::new(e))
    }
}

impl From<ErrorFromResponse> for RtcError {
    fn from(e: ErrorFromResponse) -> Self {
        RtcError::Api(Box::new(e))
    }
}

impl From<crate::error::Error> for RtcError {
    fn from(e: crate::error::Error) -> Self {
        match e {
            crate::error::Error::Api(api) => RtcError::Api(api),
            crate::error::Error::Transport(t) => RtcError::Transport(t),
            crate::error::Error::Serde(s) => RtcError::Json(s),
            crate::error::Error::Token(t) => RtcError::Token(t),
            crate::error::Error::TokenValidation(t) => RtcError::TokenValidation(t),
            crate::error::Error::ResponseTooLarge { limit, actual } => {
                RtcError::SizeLimitExceeded {
                    boundary: "coordinator HTTP response body",
                    limit,
                    actual,
                }
            }
            other => RtcError::Coordinator(other.to_string()),
        }
    }
}

impl RtcError {
    pub(crate) fn from_websocket_with_boundary(
        error: tokio_tungstenite::tungstenite::Error,
        boundary: &'static str,
    ) -> Self {
        use tokio_tungstenite::tungstenite::error::CapacityError;

        match error {
            tokio_tungstenite::tungstenite::Error::Capacity(CapacityError::MessageTooLong {
                size,
                max_size,
            }) => Self::SizeLimitExceeded {
                boundary,
                limit: max_size,
                actual: size,
            },
            other => Self::from(other),
        }
    }

    /// Convert a signal response's optional `models.Error` into an error when it
    /// carries a real (non-`UNSPECIFIED`) code. Mirrors stream-py's
    /// `_check_response_for_error`.
    pub(crate) fn from_signal_error(error: Option<models::Error>) -> Result<()> {
        match error {
            Some(e) if e.code != ErrorCode::Unspecified as i32 => Err(RtcError::Signal {
                code: e.code,
                message: e.message,
                should_retry: e.should_retry,
            }),
            _ => Ok(()),
        }
    }

    /// Whether this failure must not be retried by the join loop or the
    /// reconnect state machine.
    ///
    /// Only two conditions are truly unrecoverable (JS `Call.ts`): a coordinator
    /// error flagged `unrecoverable`, or an SFU join error whose reconnect
    /// strategy is `DISCONNECT`. Everything else (transport drops, timeouts,
    /// negotiation failures) is subject to bounded retry.
    pub fn is_unrecoverable(&self) -> bool {
        match self {
            RtcError::Api(e) => e.unrecoverable,
            RtcError::Join(e) => e.is_unrecoverable(),
            _ => false,
        }
    }

    /// True if this error came from the SFU with an `is_join_error_code` code
    /// (`SFU_FULL`, `SFU_SHUTTING_DOWN`, `CALL_PARTICIPANT_LIMIT_REACHED`),
    /// which forces the join loop to migrate to a different edge.
    pub fn is_join_error_code(&self) -> bool {
        match self {
            RtcError::Join(e) => e.is_join_error_code(),
            RtcError::Signal { code, .. } => is_join_error_code(*code),
            _ => false,
        }
    }

    /// Whether Stream rejected the current participant token as expired.
    pub(crate) fn is_token_expired(&self) -> bool {
        match self {
            RtcError::Api(error) => error.code == 40,
            RtcError::Signal { code, .. } => *code == ErrorCode::Unauthenticated as i32,
            RtcError::Twirp(error) => error.code == "unauthenticated",
            RtcError::TokenValidation(crate::error::TokenError::Expired { .. })
            | RtcError::TokenValidation(crate::error::TokenError::ExpiredByServer) => true,
            _ => false,
        }
    }
}

/// The SFU rejected the participant with an `Error` event.
///
/// Wraps the SFU `models.Error` plus the `WebsocketReconnectStrategy` the SFU
/// requested. Matches JS `SfuJoinError`: unrecoverable when the strategy is
/// `DISCONNECT` (e.g. participant limit with no other edge).
#[derive(Debug, Clone, thiserror::Error)]
#[error("sfu join error (code {code}, strategy {reconnect_strategy}): {message}")]
#[non_exhaustive]
pub struct SfuJoinError {
    /// The `stream.video.sfu.models.ErrorCode` value.
    pub code: i32,
    /// Human-readable message from the SFU.
    pub message: String,
    /// Whether the SFU hinted retry is possible.
    pub should_retry: bool,
    /// The `WebsocketReconnectStrategy` the SFU asked the client to follow.
    pub reconnect_strategy: i32,
}

impl SfuJoinError {
    /// Build from an SFU `Error` event payload.
    pub fn from_event(error: Option<models::Error>, reconnect_strategy: i32) -> Self {
        let error = error.unwrap_or_default();
        Self {
            code: error.code,
            message: error.message,
            should_retry: error.should_retry,
            reconnect_strategy,
        }
    }

    /// Unrecoverable iff the SFU asked us to permanently disconnect.
    pub fn is_unrecoverable(&self) -> bool {
        self.reconnect_strategy == WebsocketReconnectStrategy::Disconnect as i32
    }

    /// One of the codes that forces an SFU switch on the join loop.
    pub fn is_join_error_code(&self) -> bool {
        is_join_error_code(self.code)
    }
}

/// A client-side deadline elapsed waiting for the SFU (WS open or `JoinResponse`).
#[derive(Debug, Clone, thiserror::Error)]
#[error("sfu timeout waiting for {what} after {}ms", timeout.as_millis())]
pub struct SfuTimeoutError {
    /// What we were waiting for, e.g. `"join response"`.
    pub what: String,
    /// The deadline that elapsed.
    pub timeout: Duration,
}

impl SfuTimeoutError {
    /// A timeout waiting for `what` after `timeout`.
    pub fn new(what: impl Into<String>, timeout: Duration) -> Self {
        Self {
            what: what.into(),
            timeout,
        }
    }
}

/// SDP offer/answer negotiation failed (create/set description, or the SFU
/// rejected the SDP). Counted toward the consecutive-negotiation-failure cap.
#[derive(Debug, Clone, thiserror::Error)]
#[error("negotiation failed: {0}")]
pub struct NegotiationError(pub String);

/// The signaling WebSocket connection failed.
///
/// `is_ws_failure` mirrors JS `isWSFailure`: `true` means a transport-level drop
/// that is safe to reconnect; `false` means a permanent/protocol failure.
#[derive(Debug, Clone, thiserror::Error)]
#[error("websocket connection error (ws_failure={is_ws_failure}): {message}")]
pub struct WsConnectionError {
    /// Human-readable failure detail.
    pub message: String,
    /// Whether this is a retriable transport failure.
    pub is_ws_failure: bool,
}

impl WsConnectionError {
    /// A retriable transport-level WS failure.
    pub fn transport(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_ws_failure: true,
        }
    }

    /// A permanent (non-retriable) WS failure.
    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_ws_failure: false,
        }
    }
}

/// The SFU error codes that make the join loop migrate to a different edge and
/// that JS treats as `isJoinErrorCode`.
pub fn is_join_error_code(code: i32) -> bool {
    code == ErrorCode::SfuFull as i32
        || code == ErrorCode::SfuShuttingDown as i32
        || code == ErrorCode::CallParticipantLimitReached as i32
}

/// A Twirp error envelope, returned as JSON on any non-200 Twirp response.
///
/// See the [Twirp spec](https://twitchtv.github.io/twirp/docs/spec_v7.html):
/// the body is `{"code": "<code>", "msg": "<message>", "meta": {…}}` with a
/// `Content-Type: application/json` header regardless of the request encoding.
#[derive(Debug, Clone, serde::Deserialize, thiserror::Error)]
#[error("twirp error [{code}]: {msg}")]
#[non_exhaustive]
pub struct TwirpError {
    /// The Twirp error code string, e.g. `internal`, `unauthenticated`.
    pub code: String,
    /// Human-readable message.
    #[serde(default)]
    pub msg: String,
    /// Optional key/value metadata attached by the server.
    #[serde(default)]
    pub meta: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_error_codes_match_sfu() {
        assert!(is_join_error_code(ErrorCode::SfuFull as i32));
        assert!(is_join_error_code(ErrorCode::SfuShuttingDown as i32));
        assert!(is_join_error_code(
            ErrorCode::CallParticipantLimitReached as i32
        ));
        assert!(!is_join_error_code(ErrorCode::ParticipantSignalLost as i32));
        assert!(!is_join_error_code(ErrorCode::Unspecified as i32));
    }

    #[test]
    fn sfu_join_error_unrecoverable_only_on_disconnect() {
        let disconnect = SfuJoinError::from_event(
            Some(models::Error {
                code: ErrorCode::CallParticipantLimitReached as i32,
                message: "full".into(),
                should_retry: false,
            }),
            WebsocketReconnectStrategy::Disconnect as i32,
        );
        assert!(disconnect.is_unrecoverable());
        assert!(disconnect.is_join_error_code());

        let rejoin = SfuJoinError::from_event(
            Some(models::Error {
                code: ErrorCode::SfuFull as i32,
                message: "full".into(),
                should_retry: true,
            }),
            WebsocketReconnectStrategy::Rejoin as i32,
        );
        assert!(!rejoin.is_unrecoverable());
        assert!(rejoin.is_join_error_code());
    }

    #[test]
    fn api_unrecoverable_flows_through_rtc_error() {
        let api = ErrorFromResponse {
            unrecoverable: true,
            ..Default::default()
        };
        let err: RtcError = api.into();
        assert!(err.is_unrecoverable());
    }

    #[test]
    fn ws_failure_flag() {
        assert!(WsConnectionError::transport("drop").is_ws_failure);
        assert!(!WsConnectionError::permanent("bad").is_ws_failure);
    }

    #[test]
    fn websocket_capacity_failure_maps_to_typed_size_error() {
        use tokio_tungstenite::tungstenite::error::CapacityError;

        let error =
            tokio_tungstenite::tungstenite::Error::Capacity(CapacityError::MessageTooLong {
                size: 65,
                max_size: 64,
            });
        assert!(matches!(
            RtcError::from_websocket_with_boundary(error, "test WebSocket message"),
            RtcError::SizeLimitExceeded {
                boundary: "test WebSocket message",
                limit: 64,
                actual: 65,
            }
        ));
    }
}
