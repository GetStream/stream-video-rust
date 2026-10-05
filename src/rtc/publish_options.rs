//! Client-selected video publishing preferences.

use std::str::FromStr;

use super::error::RtcError;
use super::proto::models::{Codec, PublishOption, TrackType};

/// A video codec that the Rust media path can encode and decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PreferredVideoCodec {
    /// VP8.
    Vp8,
    /// VP9 profile 0.
    Vp9,
}

impl PreferredVideoCodec {
    fn name(self) -> &'static str {
        match self {
            Self::Vp8 => "VP8",
            Self::Vp9 => "VP9",
        }
    }

    fn fmtp(self) -> &'static str {
        match self {
            Self::Vp8 => "",
            Self::Vp9 => "profile-id=0",
        }
    }
}

impl FromStr for PreferredVideoCodec {
    type Err = RtcError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.eq_ignore_ascii_case("vp8") {
            Ok(Self::Vp8)
        } else if value.eq_ignore_ascii_case("vp9") {
            Ok(Self::Vp9)
        } else {
            Err(RtcError::Media(format!(
                "unsupported preferred video codec {value:?}; supported codecs are VP8 and VP9"
            )))
        }
    }
}

/// Pre-join client publishing preferences.
///
/// Set these options before [`crate::Call::join`]; updates after joining starts
/// do not affect that join generation. Passing [`ClientPublishOptions::default`]
/// restores server-selected publishing defaults for the next generation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClientPublishOptions {
    /// Preferred codec for camera video.
    pub preferred_codec: Option<PreferredVideoCodec>,
}

impl ClientPublishOptions {
    /// Select a preferred camera-video codec.
    #[must_use]
    pub const fn new(preferred_codec: PreferredVideoCodec) -> Self {
        Self {
            preferred_codec: Some(preferred_codec),
        }
    }

    pub(crate) fn preferred_publish_options(self) -> Vec<PublishOption> {
        let Some(codec) = self.preferred_codec else {
            return Vec::new();
        };
        vec![PublishOption {
            track_type: TrackType::Video as i32,
            codec: Some(Codec {
                name: codec.name().to_owned(),
                fmtp: codec.fmtp().to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vp9_preference_uses_profile_0() {
        let options =
            ClientPublishOptions::new(PreferredVideoCodec::Vp9).preferred_publish_options();

        assert_eq!(options.len(), 1);
        assert_eq!(options[0].track_type, TrackType::Video as i32);
        let codec = options[0].codec.as_ref().expect("VP9 codec");
        assert_eq!(codec.name, "VP9");
        assert_eq!(codec.fmtp, "profile-id=0");
    }

    #[test]
    fn empty_preference_preserves_server_default() {
        assert!(
            ClientPublishOptions::default()
                .preferred_publish_options()
                .is_empty()
        );
    }

    #[test]
    fn unsupported_codec_is_rejected() {
        for codec in ["av1", "h264"] {
            let error = codec
                .parse::<PreferredVideoCodec>()
                .expect_err("Rust media supports only VP8 and VP9");
            assert!(matches!(error, RtcError::Media(message) if message.contains(codec)));
        }
    }
}
