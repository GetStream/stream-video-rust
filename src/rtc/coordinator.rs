//! Coordinator `JoinCall` REST + location discovery.
//!
//! `POST /api/v2/video/call/{type}/{id}/join` is a **user-token** operation
//! (unlike the server-token REST in [`crate::video`]): the coordinator returns the SFU
//! credentials the participant needs — the Twirp base URL, the SFU token, the
//! signaling WebSocket endpoint, the ICE servers, and the stats options to
//! cache. Ported from stream-py `connection_utils.join_call_coordinator_request`
//! and the OpenAPI coordinator models shared by all SDKs.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::Client;
use crate::models::{CallRequest, CallResponse, MemberResponse};

use super::error::{Result, RtcError};

/// The CloudFront hint endpoint used to discover the caller's edge location
/// (JS `getLocationHint` / stream-py `location_discovery`).
const LOCATION_HINT_URL: &str = "https://hint.stream-io-video.com/";
/// Response header carrying the CloudFront PoP (e.g. `AMS1-P2`).
const CF_POP_HEADER: &str = "x-amz-cf-pop";
/// Location sent when discovery is disabled or fails. The coordinator accepts
/// `"auto"` and picks the nearest SFU itself (stream-py's default path).
pub const FALLBACK_LOCATION: &str = "auto";

/// Request body for the coordinator join (`JoinCallRequest`). Only `location`
/// is required; the rest mirror the JS/Go/Python join options.
#[derive(Debug, Clone, Default, Serialize)]
pub struct JoinCallRequest {
    /// The caller's edge location hint (e.g. `AMS`) or `"auto"`.
    pub location: String,
    /// Create the call if it does not exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create: Option<bool>,
    /// Call creation data, applied when `create` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<CallRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members_limit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ring: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<bool>,
    /// The SFU id we are migrating away from (forces a different edge).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migrating_from: Option<String>,
    /// SFU ids to exclude from selection (accumulated across failures).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub migrating_from_list: Vec<String>,
}

/// A single ICE server from the join credentials (`ICEServerResponse`).
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct IceServer {
    /// STUN/TURN URLs.
    pub urls: Vec<String>,
    /// TURN username (empty for STUN-only).
    pub username: String,
    /// TURN credential (empty for STUN-only).
    pub password: String,
}

impl std::fmt::Debug for IceServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IceServer")
            .field("urls", &self.urls)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The selected SFU edge (`SFUResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SfuServer {
    /// The edge identifier — the join loop tracks failures per `edge_name`.
    pub edge_name: String,
    /// The Twirp base URL (already includes the `/twirp` prefix).
    pub url: String,
    /// The SFU signaling WebSocket endpoint.
    pub ws_endpoint: String,
}

/// The participant credentials returned by the coordinator (`Credentials`).
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct Credentials {
    /// The selected SFU edge.
    pub server: SfuServer,
    /// The SFU token (distinct from the coordinator user token).
    pub token: String,
    /// ICE servers for the publisher/subscriber PeerConnections.
    pub ice_servers: Vec<IceServer>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("server", &self.server)
            .field("token", &"<redacted>")
            .field("ice_servers", &self.ice_servers)
            .finish()
    }
}

/// Stats reporting options the SDK must cache across partial coordinator
/// failures (JS `lastStatsOptions`). Off by default (videosdk default 0).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StatsOptions {
    /// Reporting cadence in ms. `0` disables periodic stats.
    pub reporting_interval_ms: i32,
    /// Whether RTC stats collection is enabled.
    pub enable_rtc_stats: bool,
}

/// The coordinator join response (`JoinCallResponse`).
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct JoinCallResponse {
    /// Server-reported request duration.
    pub duration: String,
    /// Whether the call was created by this join.
    pub created: bool,
    /// The call and its state.
    pub call: CallResponse,
    /// Current members.
    pub members: Vec<MemberResponse>,
    /// This participant's capabilities.
    pub own_capabilities: Vec<String>,
    /// SFU credentials for the participant path.
    pub credentials: Credentials,
    /// Stats options to cache.
    pub stats_options: StatsOptions,
    /// Raw membership object, kept opaque.
    pub membership: Option<Value>,
}

impl std::fmt::Debug for JoinCallResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinCallResponse")
            .field("duration", &self.duration)
            .field("created", &self.created)
            .field("call", &self.call)
            .field("members", &self.members)
            .field("own_capabilities", &self.own_capabilities)
            .field("credentials", &self.credentials)
            .field("stats_options", &self.stats_options)
            .field("membership", &self.membership)
            .finish()
    }
}

/// Perform the coordinator join for `<call_type>:<call_id>` using the user's
/// JWT. Returns the SFU credentials and cached stats options.
pub(crate) async fn join_call(
    client: &Arc<Client>,
    user_token: &str,
    call_type: &str,
    call_id: &str,
    request: &JoinCallRequest,
    query: &[(String, String)],
) -> Result<JoinCallResponse> {
    let path = Client::build_path(
        "/api/v2/video/call/{type}/{id}/join",
        &[("type", call_type), ("id", call_id)],
    );
    let resp = client
        .request_as_user(Method::POST, &path, query, Some(request), user_token)
        .await?;
    Ok(resp)
}

/// Discover the caller's edge location via the CloudFront hint endpoint.
///
/// HEADs the location hint URL, reads the `x-amz-cf-pop` header, and returns its
/// first three characters (e.g. `AMS1-P2` → `AMS`). Returns [`FALLBACK_LOCATION`]
/// (`"auto"`) on any failure so the coordinator still picks an edge.
pub async fn discover_location(http: &reqwest::Client) -> String {
    match discover_location_inner(http).await {
        Some(loc) if !loc.is_empty() => loc,
        _ => {
            tracing::debug!("stream.rtc.location.fallback");
            FALLBACK_LOCATION.to_owned()
        }
    }
}

async fn discover_location_inner(http: &reqwest::Client) -> Option<String> {
    let response = http
        .head(LOCATION_HINT_URL)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?;
    let pop = response.headers().get(CF_POP_HEADER)?.to_str().ok()?;
    let hint: String = pop.chars().take(3).collect();
    if hint.len() == 3 { Some(hint) } else { None }
}

impl RtcError {
    /// Helper to surface a missing-credential field as a coordinator error.
    pub(crate) fn missing_credential(field: &str) -> Self {
        RtcError::Coordinator(format!("join credentials missing {field}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_debug_redacts_every_secret() {
        let response = JoinCallResponse {
            credentials: Credentials {
                server: SfuServer {
                    edge_name: "edge".to_owned(),
                    url: "https://sfu.example/twirp".to_owned(),
                    ws_endpoint: "wss://sfu.example/ws".to_owned(),
                },
                token: "sfu-token-must-not-leak".to_owned(),
                ice_servers: vec![IceServer {
                    urls: vec!["turn:turn.example".to_owned()],
                    username: "turn-user".to_owned(),
                    password: "turn-password-must-not-leak".to_owned(),
                }],
            },
            ..Default::default()
        };

        let debug = format!("{response:?}");
        assert!(!debug.contains("sfu-token-must-not-leak"));
        assert!(!debug.contains("turn-password-must-not-leak"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("turn:turn.example"));
    }
}
