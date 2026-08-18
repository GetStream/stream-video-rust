//! Subscription policy ([`SubscriptionConfig`]) for the `UpdateSubscriptions`
//! signal RPC.
//!
//! The SFU never auto-forwards media — without an explicit subscription no
//! `on_track` fires (JS `DynascaleManager`, stream-py `SubscriptionManager`,
//! videosdk `UpdateSubscriptions`). This module holds the declarative policy;
//! [`RtcCore`](super::join::RtcCore) turns it plus the live participant roster
//! into the concrete `TrackSubscriptionDetails` list and (re)sends it whenever
//! the roster changes.
//!
//! The default policy subscribes to remote **audio** only (the backend-bot
//! default); video and screen-share are opt-in.

use super::proto::models::TrackType;

/// A precise subscription to one participant session and track kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct SubscriptionTarget {
    /// The publishing participant's SFU session id.
    pub session_id: String,
    /// The remote track kind to receive.
    pub track_type: TrackType,
    /// Optional preferred video dimensions sent as an SFU adaptation hint.
    pub dimension: Option<(u32, u32)>,
}

impl SubscriptionTarget {
    /// Subscribe to `track_type` from `session_id` using the SFU's default size.
    pub fn new(session_id: impl Into<String>, track_type: TrackType) -> Self {
        Self {
            session_id: session_id.into(),
            track_type,
            dimension: None,
        }
    }

    /// Set a preferred video dimension for this target.
    #[must_use]
    pub fn with_dimension(mut self, width: u32, height: u32) -> Self {
        self.dimension = Some((width, height));
        self
    }
}

/// Which remote track kinds to subscribe to.
///
/// Reactive: the call subscribes to every matching track published by every
/// other participant, and updates as participants publish/unpublish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionConfig {
    /// Subscribe to remote audio.
    pub audio: bool,
    /// Subscribe to remote video.
    pub video: bool,
    /// Subscribe to remote screen-share (video + audio).
    pub screen_share: bool,
    /// Preferred video dimension hint sent to the SFU (width, height).
    pub video_dimension: Option<(u32, u32)>,
}

impl Default for SubscriptionConfig {
    /// Audio-only — the backend-bot default (matches stream-py's usual path).
    fn default() -> Self {
        Self {
            audio: true,
            video: false,
            screen_share: false,
            video_dimension: None,
        }
    }
}

impl SubscriptionConfig {
    /// Subscribe to audio from all participants (the default).
    pub fn audio_all() -> Self {
        Self::default()
    }

    /// Subscribe to audio and video from all participants.
    pub fn audio_video() -> Self {
        Self {
            audio: true,
            video: true,
            video_dimension: Some((1280, 720)),
            ..Self::default()
        }
    }

    /// Subscribe to audio, video, and screen-share.
    pub fn all() -> Self {
        Self {
            audio: true,
            video: true,
            screen_share: true,
            video_dimension: Some((1280, 720)),
        }
    }

    /// Subscribe to nothing (unsubscribe from all).
    pub fn none() -> Self {
        Self {
            audio: false,
            video: false,
            screen_share: false,
            video_dimension: None,
        }
    }

    /// Whether this policy subscribes to `track_type`.
    pub fn matches(&self, track_type: TrackType) -> bool {
        match track_type {
            TrackType::Audio => self.audio,
            TrackType::Video => self.video,
            TrackType::ScreenShare | TrackType::ScreenShareAudio => self.screen_share,
            TrackType::Unspecified => false,
        }
    }
}

/// A subscription identity: a specific participant session's specific track kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TrackKey {
    pub session_id: String,
    pub track_type: i32,
}

impl TrackKey {
    pub(crate) fn new(session_id: impl Into<String>, track_type: TrackType) -> Self {
        Self {
            session_id: session_id.into(),
            track_type: track_type as i32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_audio_only() {
        let c = SubscriptionConfig::default();
        assert!(c.matches(TrackType::Audio));
        assert!(!c.matches(TrackType::Video));
        assert!(!c.matches(TrackType::ScreenShare));
    }

    #[test]
    fn audio_video_opts_in_video() {
        let c = SubscriptionConfig::audio_video();
        assert!(c.matches(TrackType::Audio));
        assert!(c.matches(TrackType::Video));
        assert!(!c.matches(TrackType::ScreenShare));
    }

    #[test]
    fn none_matches_nothing() {
        let c = SubscriptionConfig::none();
        assert!(!c.matches(TrackType::Audio));
        assert!(!c.matches(TrackType::Video));
    }

    #[test]
    fn target_builder_preserves_session_track_and_dimension() {
        let target =
            SubscriptionTarget::new("session-1", TrackType::Video).with_dimension(640, 360);
        assert_eq!(target.session_id, "session-1");
        assert_eq!(target.track_type, TrackType::Video);
        assert_eq!(target.dimension, Some((640, 360)));
    }
}
