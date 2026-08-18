//! Twirp-over-HTTP client for the SFU `SignalServer` service.
//!
//! Framing is ported from JS `StreamSfuClient` / `createSignalClient` and
//! stream-py `twirp_client_wrapper`:
//!
//! - `POST {base}/stream.video.sfu.signal.SignalServer/{Method}`
//! - `Content-Type: application/protobuf` with a binary prost body
//! - `Authorization: Bearer <sfu_token>` (the SFU token from join credentials)
//! - non-200 responses carry a JSON [`TwirpError`] envelope
//! - a 200 response may still embed an application-level `models.Error`, which
//!   we surface as [`RtcError::Signal`] (matching stream-py `_check_response_for_error`)
//!
//! The `base` URL is the Twirp root exactly as returned in
//! `credentials.server.url` — it already includes the `/twirp` prefix, so we
//! append only `{service}/{method}` (same as the JS `TwirpFetchTransport`).

use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};
use url::Url;

use crate::client::DEFAULT_MAX_RESPONSE_BODY_BYTES;

use super::error::{Result, RtcError, TwirpError};
use super::identity;
use super::proto::{models, signal};
use super::tracer::Tracer;

/// Fully-qualified Twirp service name (`package.Service`).
const SERVICE: &str = "stream.video.sfu.signal.SignalServer";
/// Twirp binary content type.
const PROTOBUF_CONTENT_TYPE: &str = "application/protobuf";
/// Default per-RPC timeout, matching JS `rpcRequestTimeout` (5s).
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// A Twirp client for the SFU signaling RPCs.
///
/// Cheap to clone (shares the underlying `reqwest` connection pool). Construct
/// one per SFU connection with the credentials from the coordinator join
/// response.
#[derive(Clone)]
pub struct SignalClient {
    http: reqwest::Client,
    base: Url,
    token: String,
    client_header: String,
    timeout: Duration,
    max_response_body_bytes: usize,
    /// Optional RTC event tracer. When present, each signal RPC (except the
    /// stats/metrics RPCs themselves) records a trace so the `SendStats`
    /// `rtc_stats` rollup carries the RPC timeline the dashboard renders.
    tracer: Option<Arc<Tracer>>,
}

impl std::fmt::Debug for SignalClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalClient")
            .field("http", &"<client>")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .field("client_header", &self.client_header)
            .field("timeout", &self.timeout)
            .field("max_response_body_bytes", &self.max_response_body_bytes)
            .field("tracer", &self.tracer)
            .finish()
    }
}

impl SignalClient {
    /// Build a signal client from the SFU Twirp root URL and SFU token.
    ///
    /// `base` is `credentials.server.url` (already ending in `/twirp`). Uses a
    /// freshly built `reqwest` client; see [`SignalClient::with_http`] to share
    /// an existing pool.
    pub fn new(base: &str, token: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(RtcError::Transport)?;
        Self::with_http(http, base, token)
    }

    /// Build a signal client reusing an existing `reqwest` client (connection
    /// pool), e.g. the one owned by the server-token REST client.
    pub fn with_http(http: reqwest::Client, base: &str, token: impl Into<String>) -> Result<Self> {
        // Normalize to a directory-style base so `Url::join` appends rather than
        // replaces the final path segment.
        let normalized = if base.ends_with('/') {
            base.to_owned()
        } else {
            format!("{base}/")
        };
        let base = Url::parse(&normalized).map_err(|e| RtcError::Url(format!("{base:?}: {e}")))?;
        Ok(Self {
            http,
            base,
            token: token.into(),
            client_header: identity::client_header(),
            timeout: DEFAULT_RPC_TIMEOUT,
            max_response_body_bytes: DEFAULT_MAX_RESPONSE_BODY_BYTES,
            tracer: None,
        })
    }

    /// Override the per-RPC request timeout (default 5s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the maximum accepted Twirp response body size (default 16 MiB).
    pub fn with_max_response_body_bytes(mut self, limit: usize) -> Self {
        self.max_response_body_bytes = limit;
        self
    }

    /// Attach an RTC event tracer so signal RPCs are recorded into the
    /// `SendStats.rtc_stats` rollup. All clones share the same tracer.
    pub fn with_tracer(mut self, tracer: Arc<Tracer>) -> Self {
        self.tracer = Some(tracer);
        self
    }

    /// Record a signal/WS trace on the attached tracer (no-op if none). Used by
    /// the join flow for WS open/close and join request/response events.
    pub(crate) fn trace(&self, tag: &str, data: Value) {
        if let Some(tracer) = &self.tracer {
            tracer.trace(tag, data);
        }
    }

    /// Send the publisher SDP offer and negotiated tracks. Returns the SFU's SDP answer.
    pub async fn set_publisher(
        &self,
        request: signal::SetPublisherRequest,
    ) -> Result<signal::SetPublisherResponse> {
        self.trace(
            "SetPublisher",
            json!({ "session_id": request.session_id, "tracks": request.tracks.len() }),
        );
        let resp: signal::SetPublisherResponse = self
            .rpc("SetPublisher", &request)
            .await
            .inspect_err(|e| self.trace("SetPublisherOnFailure", json!(e.to_string())))?;
        if let Err(e) = RtcError::from_signal_error(resp.error.clone()) {
            self.trace("SetPublisherOnFailure", json!(e.to_string()));
            return Err(e);
        }
        self.trace(
            "SetPublisherResponse",
            json!({ "session_id": request.session_id }),
        );
        Ok(resp)
    }

    /// Send the SDP answer to a subscriber offer received over the SFU WebSocket.
    pub async fn send_answer(
        &self,
        request: signal::SendAnswerRequest,
    ) -> Result<signal::SendAnswerResponse> {
        self.trace(
            "SendAnswer",
            json!({ "session_id": request.session_id, "peer_type": request.peer_type }),
        );
        let resp: signal::SendAnswerResponse = self
            .rpc("SendAnswer", &request)
            .await
            .inspect_err(|e| self.trace("SendAnswerOnFailure", json!(e.to_string())))?;
        if let Err(e) = RtcError::from_signal_error(resp.error.clone()) {
            self.trace("SendAnswerOnFailure", json!(e.to_string()));
            return Err(e);
        }
        Ok(resp)
    }

    /// Trickle a local ICE candidate to the SFU for the given peer type.
    pub async fn ice_trickle(
        &self,
        request: models::IceTrickle,
    ) -> Result<signal::IceTrickleResponse> {
        self.trace(
            "IceTrickle",
            json!({ "session_id": request.session_id, "peer_type": request.peer_type }),
        );
        let resp: signal::IceTrickleResponse = self
            .rpc("IceTrickle", &request)
            .await
            .inspect_err(|e| self.trace("IceTrickleOnFailure", json!(e.to_string())))?;
        if let Err(e) = RtcError::from_signal_error(resp.error.clone()) {
            self.trace("IceTrickleOnFailure", json!(e.to_string()));
            return Err(e);
        }
        Ok(resp)
    }

    /// Request an ICE restart for the publisher or subscriber peer connection.
    pub async fn ice_restart(
        &self,
        request: signal::IceRestartRequest,
    ) -> Result<signal::IceRestartResponse> {
        let resp: signal::IceRestartResponse = self.rpc("IceRestart", &request).await?;
        RtcError::from_signal_error(resp.error.clone())?;
        Ok(resp)
    }

    /// Update the set of tracks this participant is subscribed to.
    pub async fn update_subscriptions(
        &self,
        request: signal::UpdateSubscriptionsRequest,
    ) -> Result<signal::UpdateSubscriptionsResponse> {
        self.trace(
            "UpdateSubscriptions",
            json!({ "session_id": request.session_id, "tracks": request.tracks.len() }),
        );
        let resp: signal::UpdateSubscriptionsResponse = self
            .rpc("UpdateSubscriptions", &request)
            .await
            .inspect_err(|e| self.trace("UpdateSubscriptionsOnFailure", json!(e.to_string())))?;
        if let Err(e) = RtcError::from_signal_error(resp.error.clone()) {
            self.trace("UpdateSubscriptionsOnFailure", json!(e.to_string()));
            return Err(e);
        }
        Ok(resp)
    }

    /// Update the mute state of this participant's published tracks.
    pub async fn update_mute_states(
        &self,
        request: signal::UpdateMuteStatesRequest,
    ) -> Result<signal::UpdateMuteStatesResponse> {
        let resp: signal::UpdateMuteStatesResponse = self.rpc("UpdateMuteStates", &request).await?;
        RtcError::from_signal_error(resp.error.clone())?;
        Ok(resp)
    }

    /// Ask the SFU to start server-side noise cancellation for this session.
    pub async fn start_noise_cancellation(
        &self,
        request: signal::StartNoiseCancellationRequest,
    ) -> Result<signal::StartNoiseCancellationResponse> {
        let resp: signal::StartNoiseCancellationResponse =
            self.rpc("StartNoiseCancellation", &request).await?;
        RtcError::from_signal_error(resp.error.clone())?;
        Ok(resp)
    }

    /// Ask the SFU to stop server-side noise cancellation for this session.
    pub async fn stop_noise_cancellation(
        &self,
        request: signal::StopNoiseCancellationRequest,
    ) -> Result<signal::StopNoiseCancellationResponse> {
        let resp: signal::StopNoiseCancellationResponse =
            self.rpc("StopNoiseCancellation", &request).await?;
        RtcError::from_signal_error(resp.error.clone())?;
        Ok(resp)
    }

    /// Send periodic WebRTC stats to the SFU. Not retried by callers (matching JS).
    pub async fn send_stats(
        &self,
        request: signal::SendStatsRequest,
    ) -> Result<signal::SendStatsResponse> {
        let resp: signal::SendStatsResponse = self.rpc("SendStats", &request).await?;
        RtcError::from_signal_error(resp.error.clone())?;
        Ok(resp)
    }

    /// Send RTC performance metrics to the SFU.
    pub async fn send_metrics(
        &self,
        request: signal::SendMetricsRequest,
    ) -> Result<signal::SendMetricsResponse> {
        self.rpc("SendMetrics", &request).await
    }

    /// Perform a single Twirp unary call: encode `req` as protobuf, POST it, and
    /// decode the protobuf response. Non-2xx bodies are parsed as Twirp errors.
    async fn rpc<Req, Resp>(&self, method: &str, req: &Req) -> Result<Resp>
    where
        Req: Message,
        Resp: Message + Default,
    {
        let url = self
            .base
            .join(&format!("{SERVICE}/{method}"))
            .map_err(|e| RtcError::Url(format!("{method}: {e}")))?;

        let body = req.encode_to_vec();
        let response = self
            .http
            .post(url)
            .header(CONTENT_TYPE, PROTOBUF_CONTENT_TYPE)
            .header(ACCEPT, PROTOBUF_CONTENT_TYPE)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header("X-Stream-Client", &self.client_header)
            .timeout(self.timeout)
            .body(body)
            .send()
            .await
            .map_err(RtcError::Transport)?;

        let status = response.status();
        let bytes =
            crate::client::read_response_body_limited(response, self.max_response_body_bytes)
                .await
                .map_err(|error| match error {
                    crate::error::Error::ResponseTooLarge { limit, actual } => {
                        RtcError::SizeLimitExceeded {
                            boundary: "SFU signal HTTP response body",
                            limit,
                            actual,
                        }
                    }
                    other => RtcError::from(other),
                })?;

        if !status.is_success() {
            // Twirp errors are always a JSON envelope, regardless of request encoding.
            let twirp: TwirpError = serde_json::from_slice(&bytes).unwrap_or_else(|_| TwirpError {
                code: status.as_str().to_owned(),
                msg: String::from_utf8_lossy(&bytes).into_owned(),
                meta: Default::default(),
            });
            return Err(RtcError::Twirp(twirp));
        }

        Resp::decode(bytes).map_err(RtcError::Decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_sfu_token() {
        let client = SignalClient::new("https://sfu.example/twirp", "signal-token-must-not-leak")
            .expect("signal client");
        let debug = format!("{client:?}");
        assert!(!debug.contains("signal-token-must-not-leak"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("sfu.example"));
    }
}
