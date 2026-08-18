//! SFU WebRTC participant support. Always compiled — there is no `webrtc`
//! Cargo feature.
//!
//! The wire layer holds the generated protobuf types ([`proto`]), the Twirp
//! signal client ([`signal`]), the SFU protobuf WebSocket ([`sfu_ws`]), and the
//! coordinator auth WebSocket ([`coordinator_ws`]).
//!
//! The participant layer sits on top: the [`coordinator`] join REST, dual
//! publisher/subscriber PeerConnections ([`peer`]), and the [`join`] state
//! machine ([`join::RtcCore`]) with `max_join_retries`, Stream's reconnect
//! strategies, and typed [`join::CallEvent`]s. [`crate::Call::join`] and
//! [`crate::Call::leave`] are the high-level entry points; [`RtcClient`] is the
//! lower-level user-token client.
//!
//! # Stability
//!
//! The wire-layer modules — [`proto`], [`peer`], [`sfu_ws`], [`signal`],
//! [`publisher`], [`tracer`], and [`coordinator_ws`] — mirror Stream's SFU
//! protocol and change with it. They are exempt from this crate's compatibility
//! guarantees at any version bump. Prefer [`crate::Call`], [`RtcClient`], and
//! the re-exports below, which are covered by the crate's semver policy.

pub mod client;
pub mod coordinator;
pub mod coordinator_ws;
pub mod error;
mod h264;
pub mod identity;
pub mod join;
mod layers;
pub mod local_track;
mod opus;
pub mod pcm;
pub mod peer;
pub mod proto;
mod publish_options;
pub mod publisher;
pub mod reconnect;
pub mod remote_track;
mod rtp_h264;
mod rtp_vpx;
pub mod sfu_ws;
pub mod signal;
pub mod stats;
pub mod subscriptions;
pub mod tracer;
pub mod video_frame;
mod vpx;
mod vpx_decode;

pub use client::{RtcCall, RtcClient, TokenFuture, TokenProvider};
pub use coordinator::{
    Credentials, IceServer, JoinCallRequest, JoinCallResponse, SfuServer, StatsOptions,
};
pub use coordinator_ws::{
    ConnectUserDetails, CoordinatorEvent, CoordinatorEvents, CoordinatorWs, WsAuthMessage,
};
pub use error::{
    ErrorFromResponse, NegotiationError, Result as RtcResult, RtcError, SfuJoinError,
    SfuTimeoutError, TwirpError, WsConnectionError, is_join_error_code,
};
pub use identity::{CLIENT_TYPE, SDK_TYPE, client_details, client_header};
pub use join::{CallEvent, CallStateSnapshot, CallingState, JoinCallData, RtcCore};
pub use local_track::{
    LocalAudioTrack, LocalTrack, LocalVideoTrack, LocalVideoTrackConfig, RtpPacket, VideoLayering,
    audio_level_dbov,
};
pub use pcm::chunk::Pad;
pub use pcm::convert::G711_SAMPLE_RATE;
pub use pcm::{
    FRAME_SAMPLES_20MS, G711Mapping, OPUS_SAMPLE_RATE, PcmFrame, Resampler, StreamResampler,
};
pub use publish_options::{ClientPublishOptions, PreferredVideoCodec};
pub use reconnect::{
    DEFAULT_MAX_JOIN_RETRIES, JoinAttemptOutcome, ReconnectStrategy, retry_interval,
};
pub use remote_track::{Codec, RemoteParticipant, RemoteTrack};
pub use sfu_ws::{SfuReceiver, SfuSender};
pub use signal::SignalClient;
pub use stats::{DEFAULT_REPORTING_INTERVAL_MS, reporting_interval};
pub use subscriptions::{SubscriptionConfig, SubscriptionTarget};
pub use tracer::{TraceRecord, Tracer};
pub use video_frame::VideoFrame;

#[cfg(test)]
mod tests {
    use super::error::RtcError;
    use super::proto::event::{
        HealthCheckRequest, JoinRequest, SfuEvent, SfuRequest, sfu_event, sfu_request,
    };
    use super::proto::{models, signal};
    use prost::Message;

    #[test]
    fn sfu_request_join_round_trips() {
        let request = SfuRequest {
            request_payload: Some(sfu_request::RequestPayload::JoinRequest(JoinRequest {
                token: "tok".to_owned(),
                session_id: "sess-123".to_owned(),
                subscriber_sdp: "v=0".to_owned(),
                client_details: Some(super::identity::client_details()),
                ..Default::default()
            })),
        };

        let bytes = request.encode_to_vec();
        let decoded = SfuRequest::decode(bytes.as_slice()).expect("decode SfuRequest");
        assert_eq!(request, decoded);

        match decoded.request_payload {
            Some(sfu_request::RequestPayload::JoinRequest(join)) => {
                assert_eq!(join.session_id, "sess-123");
                let sdk = join.client_details.and_then(|d| d.sdk).expect("sdk");
                // AGENTS.md hard rule: never report Go to the SFU.
                assert_ne!(sdk.r#type, models::SdkType::Go as i32);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn sfu_request_health_check_round_trips() {
        let request = SfuRequest {
            request_payload: Some(sfu_request::RequestPayload::HealthCheckRequest(
                HealthCheckRequest {},
            )),
        };
        let bytes = request.encode_to_vec();
        let decoded = SfuRequest::decode(bytes.as_slice()).expect("decode");
        assert_eq!(request, decoded);
    }

    #[test]
    fn sfu_event_error_round_trips() {
        let event = SfuEvent {
            event_payload: Some(sfu_event::EventPayload::Error(super::proto::event::Error {
                error: Some(models::Error {
                    code: models::ErrorCode::ParticipantSignalLost as i32,
                    message: "signal lost".to_owned(),
                    should_retry: true,
                }),
                reconnect_strategy: models::WebsocketReconnectStrategy::Rejoin as i32,
            })),
        };
        let bytes = event.encode_to_vec();
        let decoded = SfuEvent::decode(bytes.as_slice()).expect("decode SfuEvent");
        assert_eq!(event, decoded);
    }

    #[test]
    fn set_publisher_request_round_trips() {
        let request = signal::SetPublisherRequest {
            sdp: "v=0".to_owned(),
            session_id: "sess".to_owned(),
            tracks: vec![],
        };
        let bytes = request.encode_to_vec();
        let decoded = signal::SetPublisherRequest::decode(bytes.as_slice()).expect("decode");
        assert_eq!(request, decoded);
    }

    #[test]
    fn from_signal_error_maps_only_real_codes() {
        // UNSPECIFIED (and absent) is success.
        assert!(RtcError::from_signal_error(None).is_ok());
        assert!(
            RtcError::from_signal_error(Some(models::Error {
                code: models::ErrorCode::Unspecified as i32,
                message: String::new(),
                should_retry: false,
            }))
            .is_ok()
        );
        // A real code becomes an error.
        let err = RtcError::from_signal_error(Some(models::Error {
            code: models::ErrorCode::ParticipantSignalLost as i32,
            message: "boom".to_owned(),
            should_retry: true,
        }))
        .expect_err("should be an error");
        assert!(matches!(err, RtcError::Signal { .. }));
    }

    #[test]
    fn ws_auth_message_serializes_video_product() {
        let auth =
            super::WsAuthMessage::video("jwt-token", super::ConnectUserDetails::new("agent"));
        let json = serde_json::to_value(&auth).expect("serialize");
        assert_eq!(json["token"], "jwt-token");
        assert_eq!(json["user_details"]["id"], "agent");
        assert_eq!(json["products"][0], "video");
        // Optional user fields are omitted when unset.
        assert!(json["user_details"].get("name").is_none());
    }
}
