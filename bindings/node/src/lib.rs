//! Local Node.js bindings for the Stream Video Rust RTC stack.
//!
//! The JavaScript surface here is deliberately thin: it mirrors the high-level
//! [`getstream::Call`] API one-to-one and leaves lifecycle, reconnect, token
//! minting, and media handling in the Rust core. `@stream-io/node-sdk` wraps
//! these classes; applications never import this package directly.

mod call;
mod client;
mod error;
mod json;
mod tracks;
mod types;

use getstream::rtc::proto::models::TrackType;
use napi_derive::napi;

use crate::error::invalid_argument;

pub use call::NativeCall;
pub use client::NativeStreamClient;
pub use tracks::{NativeLocalAudioTrack, NativeLocalVideoTrack, NativeRemoteTrack};

/// Version of the contract between this addon and the Node SDK's loader.
///
/// The SDK refuses to construct a call when this does not match the version it
/// was built against, so bump it on any breaking change to the native surface.
#[napi]
pub fn binding_api_version() -> u32 {
    1
}

/// The camel-case track-type name used across the Node surface.
pub(crate) fn track_type_name(track_type: TrackType) -> &'static str {
    match track_type {
        TrackType::Audio => "audio",
        TrackType::Video => "video",
        TrackType::ScreenShare => "screenshare",
        TrackType::ScreenShareAudio => "screenshare_audio",
        TrackType::Unspecified => "unspecified",
    }
}

/// Parse a track-type name coming from JavaScript.
pub(crate) fn parse_track_type(name: &str) -> napi::Result<TrackType> {
    match name {
        "audio" => Ok(TrackType::Audio),
        "video" => Ok(TrackType::Video),
        "screenshare" | "screen_share" => Ok(TrackType::ScreenShare),
        "screenshare_audio" | "screen_share_audio" => Ok(TrackType::ScreenShareAudio),
        other => Err(invalid_argument(format!("unknown track type {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Every code the Node `RtcErrorCode` union accepts must round-trip through
    /// the encoded-message channel napi gives us.
    fn decode(error: napi::Error) -> Value {
        serde_json::from_str(&error.reason).expect("errors cross the boundary as JSON")
    }

    #[test]
    fn track_type_names_round_trip() {
        for (name, track_type) in [
            ("audio", TrackType::Audio),
            ("video", TrackType::Video),
            ("screenshare", TrackType::ScreenShare),
            ("screenshare_audio", TrackType::ScreenShareAudio),
        ] {
            assert_eq!(track_type_name(track_type), name);
            assert_eq!(parse_track_type(name).unwrap(), track_type);
        }
    }

    #[test]
    fn snake_case_track_type_aliases_are_accepted() {
        assert_eq!(
            parse_track_type("screen_share").unwrap(),
            TrackType::ScreenShare
        );
        assert_eq!(
            parse_track_type("screen_share_audio").unwrap(),
            TrackType::ScreenShareAudio
        );
    }

    #[test]
    fn unknown_track_type_is_a_structured_error() {
        let error = parse_track_type("hologram").expect_err("unknown track type");
        let decoded = decode(error);
        assert_eq!(decoded["code"], "RTC_MEDIA");
        assert!(
            decoded["message"].as_str().unwrap().contains("hologram"),
            "the message should name the offending value: {decoded}"
        );
    }

    #[test]
    fn binding_api_version_matches_the_node_loader() {
        // Bump both sides together: src/rtc/types.ts RTC_BINDING_API_VERSION.
        assert_eq!(binding_api_version(), 1);
    }
}
