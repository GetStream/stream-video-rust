//! SFU WebRTC participant support. Always compiled — there is no `webrtc`
//! Cargo feature.
//!
//! The wire layer holds the generated protobuf types ([`proto`]), the Twirp
//! signal client ([`sfu::signal`]), the SFU protobuf WebSocket ([`sfu::ws`]), and
//! the coordinator auth WebSocket ([`coordinator::ws`]).
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
//! The wire-layer modules — [`proto`], [`peer`], [`sfu`], [`tracer`], and
//! [`coordinator::ws`] — mirror Stream's SFU
//! protocol and change with it. They are exempt from this crate's compatibility
//! guarantees at any version bump. Prefer [`crate::Call`], [`RtcClient`], and
//! the re-exports below, which are covered by the crate's semver policy.

pub mod client;
mod codecs;
pub mod coordinator;
pub mod error;
pub mod identity;
pub mod join;
pub mod pcm;
pub mod peer;
pub mod proto;
mod publish_options;
pub mod reconnect;
pub mod sfu;
pub mod stats;
pub mod subscriptions;
pub mod tracer;
mod tracks;
pub mod video_frame;

pub use client::{RtcCall, RtcClient, TokenFuture, TokenProvider};
pub use coordinator::ws::{
    ConnectUserDetails, CoordinatorEvent, CoordinatorEvents, CoordinatorWs, WsAuthMessage,
};
pub use coordinator::{
    Credentials, IceServer, JoinCallRequest, JoinCallResponse, SfuServer, StatsOptions,
};
pub use error::{
    ErrorFromResponse, NegotiationError, Result as RtcResult, RtcError, SfuJoinError,
    SfuTimeoutError, TwirpError, WsConnectionError, is_join_error_code,
};
pub use identity::{CLIENT_TYPE, SDK_TYPE, client_details, client_header};
pub use join::{CallEvent, CallStateSnapshot, CallingState, JoinCallData, RtcCore};
pub use pcm::chunk::Pad;
pub use pcm::convert::G711_SAMPLE_RATE;
pub use pcm::{
    FRAME_SAMPLES_20MS, G711Mapping, OPUS_SAMPLE_RATE, PcmFrame, Resampler, StreamResampler,
};
pub use publish_options::{ClientPublishOptions, PreferredVideoCodec};
pub use reconnect::{
    DEFAULT_MAX_JOIN_RETRIES, JoinAttemptOutcome, ReconnectStrategy, retry_interval,
};
pub use sfu::signal::SignalClient;
pub use sfu::ws::{SfuReceiver, SfuSender};
pub use stats::{DEFAULT_REPORTING_INTERVAL_MS, reporting_interval};
pub use subscriptions::{SubscriptionConfig, SubscriptionTarget};
pub use tracer::{TraceRecord, Tracer};
pub use tracks::{
    Codec, LocalAudioTrack, LocalAudioTrackConfig, LocalTrack, LocalVideoTrack,
    LocalVideoTrackConfig, RemoteParticipant, RemoteTrack, RtpPacket, VideoLayering,
    audio_level_dbov,
};
pub use video_frame::VideoFrame;
