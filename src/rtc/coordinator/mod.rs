//! Coordinator `JoinCall` REST + location discovery.
//!
//! `POST /api/v2/video/call/{type}/{id}/join` is a **user-token** operation
//! (unlike the server-token REST in [`crate::video`]): the coordinator returns the SFU
//! credentials the participant needs — the Twirp base URL, the SFU token, the
//! signaling WebSocket endpoint, the ICE servers, and the stats options to
//! cache. Ported from stream-py `connection_utils.join_call_coordinator_request`
//! and the OpenAPI coordinator models shared by all SDKs.
//!
//! The coordinator auth WebSocket lives in [`ws`].

mod rest;
pub mod ws;

pub(crate) use rest::join_call;
pub use rest::{
    Credentials, FALLBACK_LOCATION, IceServer, JoinCallRequest, JoinCallResponse, SfuServer,
    StatsOptions, discover_location,
};
