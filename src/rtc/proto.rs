//! Generated SFU wire types (prost) for the `stream.video.sfu.*` packages.
//!
//! The Rust here is emitted at build time by [`build.rs`](../../build.rs) from
//! the vendored `.proto` sources under `proto/` and included verbatim. The
//! include file rebuilds the protobuf package hierarchy
//! (`stream::video::sfu::{models, event, signal}`) so cross-package references
//! resolve; the [`models`], [`event`], and [`signal`] re-exports below are the
//! stable paths the rest of the crate should use.
#![allow(clippy::all)]
#![allow(clippy::pedantic)]
#![allow(clippy::nursery)]
#![allow(missing_docs)]
#![allow(rustdoc::all)]

include!(concat!(env!("OUT_DIR"), "/_sfu_proto.rs"));

#[doc(inline)]
pub use self::stream::video::sfu::{event, models, signal};

impl std::fmt::Debug for event::JoinRequest {
    // The pattern lists every field, so a new upstream field fails to compile here.
    #[allow(deprecated)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            token: _,
            session_id,
            unified_session_id,
            subscriber_sdp,
            publisher_sdp,
            client_details,
            migration,
            fast_reconnect,
            reconnect_details,
            preferred_publish_options,
            preferred_subscribe_options,
            capabilities,
            source,
        } = self;
        f.debug_struct("JoinRequest")
            .field("token", &"<redacted>")
            .field("session_id", session_id)
            .field("unified_session_id", unified_session_id)
            .field("subscriber_sdp", subscriber_sdp)
            .field("publisher_sdp", publisher_sdp)
            .field("client_details", client_details)
            .field("migration", migration)
            .field("fast_reconnect", fast_reconnect)
            .field("reconnect_details", reconnect_details)
            .field("preferred_publish_options", preferred_publish_options)
            .field("preferred_subscribe_options", preferred_subscribe_options)
            .field("capabilities", capabilities)
            .field("source", source)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::event::JoinRequest;

    #[test]
    fn join_request_debug_redacts_token() {
        let request = JoinRequest {
            token: "sfu-token-must-not-leak".to_owned(),
            session_id: "session-1".to_owned(),
            ..Default::default()
        };

        let debug = format!("{request:?}");
        assert!(!debug.contains("sfu-token-must-not-leak"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("session-1"));
    }
}
