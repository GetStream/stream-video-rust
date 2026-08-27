//! Serialization of RTC state and events into the camel-case JSON shapes
//! declared by `@stream-io/node-sdk`'s `RtcCallStateSnapshot` and
//! `RtcCallEventMap`.
//!
//! Every payload crossing into JavaScript goes through here so the Node types
//! stay the single source of truth for field naming.

use prost_types::{Struct, Timestamp, Value as ProstValue, value::Kind};
use serde_json::{Map, Value, json};

use getstream::rtc::proto::models::{
    CallGrants, ConnectionQuality, ParticipantCount, ParticipantSource, Pin, TrackType,
};
use getstream::rtc::{CallEvent, CallStateSnapshot, CallingState, RemoteParticipant};

use crate::track_type_name;

pub(crate) fn calling_state_name(state: CallingState) -> &'static str {
    match state {
        CallingState::Idle => "idle",
        CallingState::Joining => "joining",
        CallingState::Joined => "joined",
        CallingState::Reconnecting => "reconnecting",
        CallingState::Migrating => "migrating",
        CallingState::ReconnectingFailed => "reconnecting-failed",
        CallingState::Left => "left",
        CallingState::Offline => "offline",
    }
}

fn connection_quality_name(quality: ConnectionQuality) -> &'static str {
    match quality {
        ConnectionQuality::Unspecified => "unspecified",
        ConnectionQuality::Poor => "poor",
        ConnectionQuality::Good => "good",
        ConnectionQuality::Excellent => "excellent",
    }
}

fn participant_source_name(source: ParticipantSource) -> &'static str {
    match source {
        ParticipantSource::WebrtcUnspecified => "webrtc",
        ParticipantSource::Rtmp => "rtmp",
        ParticipantSource::Whip => "whip",
        ParticipantSource::Sip => "sip",
        ParticipantSource::Rtsp => "rtsp",
        ParticipantSource::Srt => "srt",
    }
}

/// Resolve a wire `TrackType` code to its Node name, tolerating unknown codes
/// from a newer SFU rather than dropping the event.
fn track_type_name_from_code(code: i32) -> &'static str {
    TrackType::try_from(code)
        .map(track_type_name)
        .unwrap_or("unspecified")
}

fn track_type_names(types: &[TrackType]) -> Vec<&'static str> {
    types.iter().copied().map(track_type_name).collect()
}

fn timestamp(value: Option<&Timestamp>) -> Option<String> {
    value.map(ToString::to_string)
}

/// Convert a protobuf `Struct` (participant `custom` data) to plain JSON.
fn struct_to_json(value: &Struct) -> Value {
    Value::Object(
        value
            .fields
            .iter()
            .map(|(key, field)| (key.clone(), prost_value_to_json(field)))
            .collect::<Map<_, _>>(),
    )
}

fn prost_value_to_json(value: &ProstValue) -> Value {
    match &value.kind {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::BoolValue(inner)) => Value::Bool(*inner),
        Some(Kind::NumberValue(inner)) => serde_json::Number::from_f64(*inner)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Some(Kind::StringValue(inner)) => Value::String(inner.clone()),
        Some(Kind::ListValue(inner)) => {
            Value::Array(inner.values.iter().map(prost_value_to_json).collect())
        }
        Some(Kind::StructValue(inner)) => struct_to_json(inner),
    }
}

fn grants_json(grants: &CallGrants) -> Value {
    json!({
        "canPublishAudio": grants.can_publish_audio,
        "canPublishVideo": grants.can_publish_video,
        "canScreenshare": grants.can_screenshare,
    })
}

fn pins_json(pins: &[Pin]) -> Value {
    Value::Array(
        pins.iter()
            .map(|pin| {
                json!({
                    "userId": pin.user_id,
                    "sessionId": pin.session_id,
                })
            })
            .collect(),
    )
}

fn count_json(count: &ParticipantCount) -> (u32, u32) {
    (count.total, count.anonymous)
}

pub(crate) fn participant_json(participant: &RemoteParticipant) -> Value {
    json!({
        "userId": participant.user_id,
        "sessionId": participant.session_id,
        "trackLookupPrefix": participant.track_lookup_prefix,
        "publishedTracks": track_type_names(&participant.published_tracks),
        "joinedAt": timestamp(participant.joined_at.as_ref()),
        "connectionQuality": connection_quality_name(participant.connection_quality),
        "isSpeaking": participant.is_speaking,
        "isDominantSpeaker": participant.is_dominant_speaker,
        "audioLevel": participant.audio_level,
        "name": participant.name,
        "image": participant.image,
        "custom": participant.custom.as_ref().map(struct_to_json),
        "roles": participant.roles,
        "source": participant_source_name(participant.source),
        "pausedTracks": track_type_names(&participant.paused_tracks),
    })
}

/// The `RtcCallStateSnapshot` handed to `call.state` on every refresh.
pub(crate) fn state_json(
    snapshot: &CallStateSnapshot,
    calling_state: CallingState,
    session_id: Option<String>,
) -> Value {
    let (total, anonymous) = count_json(&snapshot.participant_count);
    json!({
        "callingState": calling_state_name(calling_state),
        "sessionId": session_id,
        "participants": snapshot
            .participants
            .iter()
            .map(participant_json)
            .collect::<Vec<_>>(),
        "participantCount": total,
        "anonymousParticipantCount": anonymous,
        "pins": pins_json(&snapshot.pins),
        "startedAt": timestamp(snapshot.started_at.as_ref()),
        "e2eeEnabled": snapshot.e2ee_enabled,
        "ownCapabilities": snapshot.own_capabilities,
        "currentGrants": snapshot.current_grants.as_ref().map(grants_json),
    })
}

pub(crate) fn stats_json(
    publisher: &Value,
    subscriber: &Value,
    dropped_remote_tracks: u64,
) -> Value {
    json!({
        "publisher": publisher,
        "subscriber": subscriber,
        "droppedRemoteTracks": dropped_remote_tracks,
    })
}

/// Map a typed [`CallEvent`] onto the `{ type, ... }` shape `StreamCall.on`
/// dispatches on.
///
/// Coordinator events keep their own Stream event name (`call.created`,
/// `call.permission_request`, …) so existing coordinator handlers work
/// unchanged; SFU events use the camel-case names in `RtcCallEventMap`.
pub(crate) fn event_json(event: &CallEvent) -> Value {
    match event {
        CallEvent::ParticipantJoined(participant) => json!({
            "type": "participantJoined",
            "userId": participant.user_id,
            "sessionId": participant.session_id,
            "participant": proto_participant_json(participant),
        }),
        CallEvent::ParticipantLeft(participant) => json!({
            "type": "participantLeft",
            "userId": participant.user_id,
            "sessionId": participant.session_id,
            "participant": proto_participant_json(participant),
        }),
        CallEvent::ParticipantUpdated(participant) => json!({
            "type": "participantUpdated",
            "userId": participant.user_id,
            "sessionId": participant.session_id,
            "participant": proto_participant_json(participant),
        }),
        CallEvent::Coordinator(coordinator) => {
            let mut value = coordinator.raw.clone();
            // Keep the coordinator payload intact but guarantee a `type` field
            // so the Node dispatcher never sees an untyped event.
            if let Some(object) = value.as_object_mut() {
                object
                    .entry("type")
                    .or_insert_with(|| Value::String(coordinator.event_type.clone()));
                return value;
            }
            json!({ "type": coordinator.event_type, "raw": coordinator.raw })
        }
        CallEvent::TrackPublished {
            user_id,
            session_id,
            track_type,
        } => json!({
            "type": "trackPublished",
            "userId": user_id,
            "sessionId": session_id,
            "trackType": track_type_name_from_code(*track_type),
            "trackTypeCode": track_type,
        }),
        CallEvent::TrackUnpublished {
            user_id,
            session_id,
            track_type,
        } => json!({
            "type": "trackUnpublished",
            "userId": user_id,
            "sessionId": session_id,
            "trackType": track_type_name_from_code(*track_type),
            "trackTypeCode": track_type,
        }),
        CallEvent::DominantSpeakerChanged {
            user_id,
            session_id,
        } => json!({
            "type": "dominantSpeakerChanged",
            "userId": user_id,
            "sessionId": session_id,
        }),
        CallEvent::AudioLevelChanged(levels) => json!({
            "type": "audioLevelChanged",
            "levels": levels.iter().map(|level| json!({
                "userId": level.user_id,
                "sessionId": level.session_id,
                "level": level.level,
                "isSpeaking": level.is_speaking,
            })).collect::<Vec<_>>(),
        }),
        CallEvent::ConnectionQualityChanged(updates) => json!({
            "type": "connectionQualityChanged",
            "updates": updates.iter().map(|update| json!({
                "userId": update.user_id,
                "sessionId": update.session_id,
                "connectionQuality": ConnectionQuality::try_from(update.connection_quality)
                    .map(connection_quality_name)
                    .unwrap_or("unspecified"),
            })).collect::<Vec<_>>(),
        }),
        CallEvent::ParticipantCountChanged(count) => json!({
            "type": "participantCountChanged",
            "participantCount": count.total,
            "anonymousParticipantCount": count.anonymous,
        }),
        CallEvent::PinsUpdated(pins) => json!({
            "type": "pinsUpdated",
            "pins": pins_json(pins),
        }),
        CallEvent::InboundStateChanged(states) => json!({
            "type": "inboundStateChanged",
            "states": states.iter().map(|state| json!({
                "userId": state.user_id,
                "sessionId": state.session_id,
                "trackType": track_type_name_from_code(state.track_type),
                "paused": state.paused,
            })).collect::<Vec<_>>(),
        }),
        CallEvent::PublishOptionsChanged {
            publish_options,
            reason,
        } => json!({
            "type": "publishOptionsChanged",
            "reason": reason,
            "publishOptions": publish_options.iter().map(|option| json!({
                "id": option.id,
                "trackType": track_type_name_from_code(option.track_type),
                "bitrate": option.bitrate,
                "maxSpatialLayers": option.max_spatial_layers,
                "maxTemporalLayers": option.max_temporal_layers,
                "fps": option.fps,
            })).collect::<Vec<_>>(),
        }),
        CallEvent::PublishQualityChanged(quality) => json!({
            "type": "publishQualityChanged",
            "audioSenderCount": quality.audio_senders.len(),
            "videoSenderCount": quality.video_senders.len(),
        }),
        CallEvent::CallGrantsUpdated(updated) => json!({
            "type": "callGrantsUpdated",
            "message": updated.message,
            "currentGrants": updated.current_grants.as_ref().map(grants_json),
        }),
        CallEvent::IceRestarted(peer_type) => json!({
            "type": "iceRestarted",
            "peerType": *peer_type as i32,
        }),
        CallEvent::Error(error) => json!({
            "type": "error",
            "code": error.code,
            "message": error.message,
            "shouldRetry": error.should_retry,
            "reconnectStrategy": error.reconnect_strategy,
        }),
        CallEvent::CallEnded => json!({ "type": "callEnded" }),
        CallEvent::CallingStateChanged(state) => json!({
            "type": "callingStateChanged",
            "callingState": calling_state_name(*state),
        }),
        other => json!({
            "type": "unknown",
            "debug": format!("{other:?}"),
        }),
    }
}

fn proto_participant_json(participant: &getstream::rtc::proto::models::Participant) -> Value {
    json!({
        "userId": participant.user_id,
        "sessionId": participant.session_id,
        "trackLookupPrefix": participant.track_lookup_prefix,
        "publishedTracks": participant
            .published_tracks
            .iter()
            .map(|code| track_type_name_from_code(*code))
            .collect::<Vec<_>>(),
        "connectionQuality": ConnectionQuality::try_from(participant.connection_quality)
            .map(connection_quality_name)
            .unwrap_or("unspecified"),
        "isSpeaking": participant.is_speaking,
        "isDominantSpeaker": participant.is_dominant_speaker,
        "audioLevel": participant.audio_level,
        "name": participant.name,
        "image": participant.image,
        "roles": participant.roles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use getstream::rtc::proto::event::AudioLevel;
    use getstream::rtc::proto::models::Participant;

    /// The Node `RtcCallingState` union is the contract; these names must match
    /// `src/rtc/types.ts` exactly, hyphen included.
    #[test]
    fn calling_state_names_match_the_node_union() {
        for (state, name) in [
            (CallingState::Idle, "idle"),
            (CallingState::Joining, "joining"),
            (CallingState::Joined, "joined"),
            (CallingState::Reconnecting, "reconnecting"),
            (CallingState::Migrating, "migrating"),
            (CallingState::ReconnectingFailed, "reconnecting-failed"),
            (CallingState::Left, "left"),
            (CallingState::Offline, "offline"),
        ] {
            assert_eq!(calling_state_name(state), name);
        }
    }

    #[test]
    fn state_snapshot_uses_the_camel_case_node_shape() {
        let value = state_json(
            &CallStateSnapshot::default(),
            CallingState::Joined,
            Some("session-1".to_owned()),
        );

        assert_eq!(value["callingState"], "joined");
        assert_eq!(value["sessionId"], "session-1");
        assert_eq!(value["participantCount"], 0);
        assert_eq!(value["anonymousParticipantCount"], 0);
        assert_eq!(value["e2eeEnabled"], false);
        assert!(value["participants"].is_array());
        assert!(value["pins"].is_array());
        assert!(value["ownCapabilities"].is_array());
        assert!(value["startedAt"].is_null());
        assert!(value["currentGrants"].is_null());
    }

    #[test]
    fn state_snapshot_omits_the_session_id_before_join() {
        let value = state_json(&CallStateSnapshot::default(), CallingState::Idle, None);
        assert!(value["sessionId"].is_null());
    }

    #[test]
    fn grants_use_the_camel_case_state_and_event_contract() {
        let grants = CallGrants {
            can_publish_audio: true,
            can_publish_video: false,
            can_screenshare: true,
        };
        let mut snapshot = CallStateSnapshot::default();
        snapshot.current_grants = Some(grants);
        let state = state_json(&snapshot, CallingState::Joined, None);
        assert_eq!(state["currentGrants"]["canPublishAudio"], true);
        assert_eq!(state["currentGrants"]["canPublishVideo"], false);
        assert_eq!(state["currentGrants"]["canScreenshare"], true);

        let event = event_json(&CallEvent::CallGrantsUpdated(
            getstream::rtc::proto::event::CallGrantsUpdated {
                current_grants: snapshot.current_grants.take(),
                message: "updated".to_owned(),
            },
        ));
        assert_eq!(event["type"], "callGrantsUpdated");
        assert_eq!(event["currentGrants"]["canPublishAudio"], true);
        assert_eq!(event["message"], "updated");
    }

    #[test]
    fn participants_carry_the_fields_node_reads() {
        let mut snapshot = CallStateSnapshot::default();
        let mut participant = RemoteParticipant::default();
        participant.user_id = "agent".to_owned();
        participant.session_id = "session-1".to_owned();
        participant.published_tracks = vec![TrackType::Audio, TrackType::ScreenShare];
        participant.is_speaking = true;
        snapshot.participants.push(participant);
        snapshot.own_capabilities = vec!["send-audio".to_owned()];

        let value = state_json(&snapshot, CallingState::Joined, None);
        let first = &value["participants"][0];

        assert_eq!(first["userId"], "agent");
        assert_eq!(first["sessionId"], "session-1");
        assert_eq!(first["publishedTracks"][0], "audio");
        assert_eq!(first["publishedTracks"][1], "screenshare");
        assert_eq!(first["isSpeaking"], true);
        assert_eq!(first["connectionQuality"], "unspecified");
        assert_eq!(first["source"], "webrtc");
        assert_eq!(value["ownCapabilities"][0], "send-audio");
    }

    /// `StreamCall` keys remote tracks by `sessionId:trackType`, so an
    /// unpublish event must carry the *name*, not the wire code.
    #[test]
    fn track_unpublished_carries_the_track_type_name() {
        let value = event_json(&CallEvent::TrackUnpublished {
            user_id: "peer".to_owned(),
            session_id: "peer-session".to_owned(),
            track_type: TrackType::ScreenShareAudio as i32,
        });

        assert_eq!(value["type"], "trackUnpublished");
        assert_eq!(value["sessionId"], "peer-session");
        assert_eq!(value["trackType"], "screenshare_audio");
        assert_eq!(value["trackTypeCode"], TrackType::ScreenShareAudio as i32);
    }

    #[test]
    fn an_unknown_track_type_code_does_not_drop_the_event() {
        let value = event_json(&CallEvent::TrackPublished {
            user_id: "peer".to_owned(),
            session_id: "peer-session".to_owned(),
            track_type: 999,
        });

        assert_eq!(value["type"], "trackPublished");
        assert_eq!(value["trackType"], "unspecified");
    }

    #[test]
    fn calling_state_changed_matches_the_node_handler_shape() {
        let value = event_json(&CallEvent::CallingStateChanged(
            CallingState::ReconnectingFailed,
        ));

        assert_eq!(value["type"], "callingStateChanged");
        assert_eq!(value["callingState"], "reconnecting-failed");
    }

    #[test]
    fn call_ended_is_a_bare_typed_event() {
        assert_eq!(event_json(&CallEvent::CallEnded)["type"], "callEnded");
    }

    #[test]
    fn participant_events_expose_ids_at_the_top_level() {
        let participant = Participant {
            user_id: "peer".to_owned(),
            session_id: "peer-session".to_owned(),
            ..Default::default()
        };
        let value = event_json(&CallEvent::ParticipantJoined(participant));

        assert_eq!(value["type"], "participantJoined");
        assert_eq!(value["userId"], "peer");
        assert_eq!(value["sessionId"], "peer-session");
        assert_eq!(value["participant"]["userId"], "peer");
    }

    #[test]
    fn audio_levels_are_camel_cased() {
        let value = event_json(&CallEvent::AudioLevelChanged(vec![AudioLevel {
            user_id: "peer".to_owned(),
            session_id: "peer-session".to_owned(),
            level: 0.5,
            is_speaking: true,
        }]));

        assert_eq!(value["type"], "audioLevelChanged");
        assert_eq!(value["levels"][0]["isSpeaking"], true);
        assert_eq!(value["levels"][0]["sessionId"], "peer-session");
    }

    #[test]
    fn stats_include_the_dropped_track_counter() {
        let value = stats_json(&json!([{"type": "outbound-rtp"}]), &json!([]), 7);
        assert_eq!(value["droppedRemoteTracks"], 7);
        assert_eq!(value["publisher"][0]["type"], "outbound-rtp");
        assert!(value["subscriber"].is_array());
    }
}
