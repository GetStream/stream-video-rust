//! Subscription policy ([`SubscriptionConfig`]) for the `UpdateSubscriptions`
//! signal RPC.
//!
//! The SFU never auto-forwards media — without an explicit subscription no
//! `on_track` fires (JS `DynascaleManager`, stream-py `SubscriptionManager`,
//! videosdk `UpdateSubscriptions`). This module holds the declarative policy;
//! [`RtcCore`](super::join::RtcCore) turns it plus the live participants into
//! the concrete `TrackSubscriptionDetails` list and (re)sends it whenever the
//! participants change.
//!
//! The policy has the shape of the stream-py `SubscriptionConfig`: a default
//! rule, rules by participant role, and a limit on the number of tracks.

use std::collections::{HashMap, HashSet};

use super::proto::models::{self, TrackType};
use super::proto::signal;

/// Video dimension requested when a subscription gives none. The SFU rejects a
/// video or screen-share subscription without a dimension.
pub(crate) const DEFAULT_VIDEO_DIMENSION: (u32, u32) = (1920, 1080);

/// A precise subscription to one participant session and track kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct SubscriptionTarget {
    /// The publishing participant's SFU session id.
    pub session_id: String,
    /// The remote track kind to receive.
    pub track_type: TrackType,
    /// Preferred video dimensions sent as an SFU adaptation hint. `None`
    /// requests 1920×1080 for video and screen-share.
    pub dimension: Option<(u32, u32)>,
}

impl SubscriptionTarget {
    /// Subscribe to `track_type` from `session_id`, at 1920×1080 for video.
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

/// The subscription rule for a group of participants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSubscriptionConfig {
    /// The remote track kinds to receive.
    pub track_types: Vec<TrackType>,
    /// Preferred camera video dimension (width, height), sent to the SFU as an
    /// adaptation hint.
    pub video_dimension: (u32, u32),
    /// Preferred screen-share dimension (width, height), sent to the SFU as an
    /// adaptation hint.
    pub screenshare_dimension: (u32, u32),
}

impl Default for TrackSubscriptionConfig {
    /// No track kinds, 1920×1080 for video and screen-share.
    fn default() -> Self {
        Self {
            track_types: Vec::new(),
            video_dimension: DEFAULT_VIDEO_DIMENSION,
            screenshare_dimension: DEFAULT_VIDEO_DIMENSION,
        }
    }
}

/// Which remote tracks to subscribe to.
///
/// Reactive: the call subscribes to the matching tracks of every other
/// participant, and updates as participants join, leave, change, and publish.
/// The default subscribes to nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubscriptionConfig {
    /// The rule for a participant whose roles have no rule in `role_filters`.
    pub default: TrackSubscriptionConfig,
    /// Rules by participant role. The first role of the participant that has a
    /// rule selects it.
    pub role_filters: HashMap<String, TrackSubscriptionConfig>,
    /// The maximum number of subscribed tracks.
    pub max_subscriptions: Option<usize>,
}

impl SubscriptionConfig {
    /// Subscribe to audio from all participants.
    pub fn audio_all() -> Self {
        Self {
            default: TrackSubscriptionConfig {
                track_types: vec![TrackType::Audio],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Subscribe to audio and video from all participants.
    pub fn audio_video() -> Self {
        Self {
            default: TrackSubscriptionConfig {
                track_types: vec![TrackType::Audio, TrackType::Video],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Subscribe to audio, video, screen-share, and screen-share audio.
    pub fn all() -> Self {
        Self {
            default: TrackSubscriptionConfig {
                track_types: vec![
                    TrackType::Audio,
                    TrackType::Video,
                    TrackType::ScreenShare,
                    TrackType::ScreenShareAudio,
                ],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Subscribe to nothing (unsubscribe from all).
    pub fn none() -> Self {
        Self::default()
    }

    /// The subscriptions to the tracks of `participants`, in their order, except
    /// the tracks in `unsubscribed`.
    pub(crate) fn track_subscriptions<'a>(
        &self,
        participants: impl IntoIterator<Item = &'a models::Participant>,
        unsubscribed: &HashSet<TrackKey>,
    ) -> Vec<signal::TrackSubscriptionDetails> {
        let mut tracks = Vec::new();
        for participant in participants {
            let rule = self.rule_for(participant);
            for &published in &participant.published_tracks {
                let Ok(track_type) = TrackType::try_from(published) else {
                    continue;
                };
                if !rule.track_types.contains(&track_type)
                    || unsubscribed
                        .contains(&TrackKey::new(participant.session_id.clone(), track_type))
                {
                    continue;
                }
                let dimension = match track_type {
                    TrackType::Video => Some(rule.video_dimension),
                    TrackType::ScreenShare => Some(rule.screenshare_dimension),
                    _ => None,
                };
                tracks.push(signal::TrackSubscriptionDetails {
                    user_id: participant.user_id.clone(),
                    session_id: participant.session_id.clone(),
                    track_type: published,
                    dimension: dimension
                        .map(|(width, height)| models::VideoDimension { width, height }),
                });
            }
        }
        if let Some(max) = self.max_subscriptions {
            tracks.truncate(max);
        }
        tracks
    }

    fn rule_for(&self, participant: &models::Participant) -> &TrackSubscriptionConfig {
        participant
            .roles
            .iter()
            .find_map(|role| self.role_filters.get(role))
            .unwrap_or(&self.default)
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

    fn participant(
        session_id: &str,
        roles: &[&str],
        published: &[TrackType],
    ) -> models::Participant {
        models::Participant {
            user_id: format!("user-{session_id}"),
            session_id: session_id.to_owned(),
            roles: roles.iter().map(|role| (*role).to_owned()).collect(),
            published_tracks: published
                .iter()
                .map(|track_type| *track_type as i32)
                .collect(),
            ..Default::default()
        }
    }

    fn subscribed(
        config: &SubscriptionConfig,
        participants: &[models::Participant],
    ) -> Vec<(String, TrackType)> {
        config
            .track_subscriptions(participants, &HashSet::new())
            .into_iter()
            .map(|track| (track.session_id.clone(), track.track_type()))
            .collect()
    }

    fn rule(track_types: &[TrackType]) -> TrackSubscriptionConfig {
        TrackSubscriptionConfig {
            track_types: track_types.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn a_role_rule_replaces_the_default_rule() {
        let config = SubscriptionConfig {
            default: rule(&[TrackType::Audio]),
            role_filters: HashMap::from([("host".to_owned(), rule(&[TrackType::Video]))]),
            ..Default::default()
        };
        let both = [TrackType::Audio, TrackType::Video];
        let participants = [
            participant("host", &["host"], &both),
            participant("guest", &["user"], &both),
        ];

        assert_eq!(
            subscribed(&config, &participants),
            [
                ("host".to_owned(), TrackType::Video),
                ("guest".to_owned(), TrackType::Audio),
            ]
        );
    }

    #[test]
    fn the_first_role_of_the_participant_with_a_rule_wins() {
        let config = SubscriptionConfig {
            role_filters: HashMap::from([
                ("admin".to_owned(), rule(&[TrackType::Audio])),
                ("host".to_owned(), rule(&[TrackType::Video])),
            ]),
            ..Default::default()
        };
        let both = [TrackType::Audio, TrackType::Video];
        let participants = [
            participant("a", &["user", "host", "admin"], &both),
            participant("b", &["admin", "host"], &both),
        ];

        assert_eq!(
            subscribed(&config, &participants),
            [
                ("a".to_owned(), TrackType::Video),
                ("b".to_owned(), TrackType::Audio),
            ]
        );
    }

    #[test]
    fn video_and_screen_share_get_their_own_dimensions() {
        let config = SubscriptionConfig {
            default: TrackSubscriptionConfig {
                track_types: vec![TrackType::Audio, TrackType::Video, TrackType::ScreenShare],
                video_dimension: (640, 360),
                screenshare_dimension: (2560, 1440),
            },
            ..Default::default()
        };
        let presenter = participant(
            "presenter",
            &[],
            &[
                TrackType::Audio,
                TrackType::Video,
                TrackType::ScreenShare,
                TrackType::ScreenShareAudio,
            ],
        );

        let dimensions: Vec<_> = config
            .track_subscriptions(&[presenter], &HashSet::new())
            .into_iter()
            .map(|track| {
                let dimension = track.dimension.map(|d| (d.width, d.height));
                (track.track_type(), dimension)
            })
            .collect();

        assert_eq!(
            dimensions,
            [
                (TrackType::Audio, None),
                (TrackType::Video, Some((640, 360))),
                (TrackType::ScreenShare, Some((2560, 1440))),
            ]
        );
    }

    #[test]
    fn the_limit_keeps_the_first_tracks_in_participant_order() {
        let config = SubscriptionConfig {
            max_subscriptions: Some(2),
            ..SubscriptionConfig::audio_all()
        };
        let participants = ["c", "a", "b"].map(|id| participant(id, &[], &[TrackType::Audio]));
        let first = |tracks: Vec<signal::TrackSubscriptionDetails>| {
            tracks
                .into_iter()
                .map(|track| track.session_id)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            first(config.track_subscriptions(&participants, &HashSet::new())),
            ["c", "a"]
        );
        let unsubscribed = HashSet::from([TrackKey::new("c", TrackType::Audio)]);
        assert_eq!(
            first(config.track_subscriptions(&participants, &unsubscribed)),
            ["a", "b"]
        );
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
