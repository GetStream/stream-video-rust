//! Outbound ([`LocalAudioTrack`], [`LocalVideoTrack`]) and inbound
//! ([`RemoteTrack`]) media tracks.

mod layers;
mod local;
mod remote;

pub use local::{
    LocalAudioTrack, LocalAudioTrackConfig, LocalTrack, LocalVideoTrack, LocalVideoTrackConfig,
    RtpPacket, VideoLayering, audio_level_dbov,
};
pub use remote::{Codec, RemoteParticipant, RemoteTrack};
