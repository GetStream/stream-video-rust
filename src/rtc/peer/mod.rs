//! webrtc-rs PeerConnection construction and the throwaway generic SDPs.
//!
//! The participant path uses two PeerConnections (JS / videosdk): the publisher
//! is the offerer (SetPublisher over Twirp) and the subscriber is the answerer
//! (answers the SFU's `subscriber_offer` over the WS). This module builds them
//! with the SDK-supported codec + interceptor set and produces the "generic" SDPs the
//! SFU inspects to learn our codec capabilities on the `JoinRequest`
//! (JS `getGenericSdp`, stream-py `create_join_request`).

mod connection;
mod ice;
pub mod publisher;
mod subscriber;

pub use connection::{generic_sdp, new_peer_connection, to_rtc_ice_servers, trace_peer_events};
pub(super) use ice::{PendingIce, register_ice_trickle};
pub(super) use subscriber::negotiate_subscriber;
