//! The participant roster and cached call state: the SFU's view of who is
//! in the call, what they publish, and the call-level state that join and
//! incremental SFU events maintain.

use super::*;

/// A participant known to be in the call, used to correlate inbound tracks
/// (by `track_lookup_prefix`) and to build the subscription list.
#[derive(Clone, Default)]
pub(super) struct RosterEntry {
    pub(super) user_id: String,
    pub(super) session_id: String,
    pub(super) track_lookup_prefix: String,
    pub(super) published: HashSet<i32>,
    pub(super) participant: models::Participant,
    pub(super) paused: HashSet<i32>,
}

#[derive(Clone, Default)]
pub(super) struct CallStateCache {
    pub(super) participant_count: models::ParticipantCount,
    pub(super) pins: Vec<models::Pin>,
    pub(super) started_at: Option<prost_types::Timestamp>,
    pub(super) e2ee_enabled: bool,
    pub(super) current_grants: Option<models::CallGrants>,
}

impl RtcCore {
    /// A snapshot of the participants currently known in the call (including this
    /// session), built from the SFU roster. Updated as `ParticipantJoined` /
    /// `ParticipantLeft` events arrive.
    pub fn participants(&self) -> Vec<RemoteParticipant> {
        let roster = self.roster.lock().unwrap_or_else(|e| e.into_inner());
        roster
            .values()
            .map(|entry| {
                RemoteParticipant::from_proto(&entry.participant, entry.paused.iter().copied())
            })
            .collect()
    }

    /// Return the latest call state assembled from the join response and SFU events.
    pub fn call_state(&self) -> CallStateSnapshot {
        let participants = self.participants();
        let state = self
            .call_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let mut own_capabilities = self
            .own_capabilities
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        own_capabilities.sort();
        CallStateSnapshot {
            participants,
            participant_count: state.participant_count,
            pins: state.pins,
            started_at: state.started_at,
            e2ee_enabled: state.e2ee_enabled,
            own_capabilities,
            current_grants: state.current_grants,
        }
    }

    pub(super) fn lookup_participant(&self, prefix: &str) -> RemoteParticipant {
        let roster = self.roster.lock().unwrap_or_else(|e| e.into_inner());
        for entry in roster.values() {
            if !entry.track_lookup_prefix.is_empty() && entry.track_lookup_prefix == prefix {
                return RemoteParticipant::from_proto(
                    &entry.participant,
                    entry.paused.iter().copied(),
                );
            }
        }
        // Unknown publisher: fall back to the msid prefix as a stable id.
        RemoteParticipant {
            session_id: prefix.to_owned(),
            ..Default::default()
        }
    }

    /// Replace the roster from an authoritative SFU join response when its
    /// lifecycle generation is still active.
    pub(super) fn apply_join_call_state_if_current(
        &self,
        generation: u64,
        session_id: &str,
        user_id: &str,
        call_state: Option<models::CallState>,
    ) -> bool {
        let lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if lifecycle.generation != generation {
            return false;
        }
        let state = call_state.unwrap_or_default();
        let participants = state.participants.clone();
        *self
            .call_state
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = CallStateCache {
            participant_count: state.participant_count.unwrap_or_default(),
            pins: state.pins,
            started_at: state.started_at,
            e2ee_enabled: state.e2ee_enabled,
            current_grants: None,
        };
        {
            let mut roster = self.roster.lock().unwrap_or_else(|e| e.into_inner());
            roster.clear();
            let me = roster.entry(session_id.to_owned()).or_default();
            me.user_id = user_id.to_owned();
            me.session_id = session_id.to_owned();
            me.participant.user_id = user_id.to_owned();
            me.participant.session_id = session_id.to_owned();
            for participant in &participants {
                if participant.session_id.is_empty() {
                    continue;
                }
                let entry = roster.entry(participant.session_id.clone()).or_default();
                entry.user_id.clone_from(&participant.user_id);
                entry.session_id.clone_from(&participant.session_id);
                entry.participant.clone_from(participant);
                if !participant.track_lookup_prefix.is_empty() {
                    entry
                        .track_lookup_prefix
                        .clone_from(&participant.track_lookup_prefix);
                }
                entry.published.clear();
                entry
                    .published
                    .extend(participant.published_tracks.iter().copied());
            }
        }
        for participant in participants {
            if participant.session_id != session_id {
                let _ = self
                    .events_tx
                    .send(CallEvent::ParticipantJoined(participant));
            }
        }
        true
    }

    /// Insert/refresh a participant's roster entry from a `Participant` message.
    pub(super) fn roster_upsert(&self, p: &models::Participant) {
        if p.session_id.is_empty() {
            return;
        }
        let mut roster = self.roster.lock().unwrap_or_else(|e| e.into_inner());
        let entry = roster.entry(p.session_id.clone()).or_default();
        entry.user_id = p.user_id.clone();
        entry.session_id = p.session_id.clone();
        entry.participant.clone_from(p);
        if !p.track_lookup_prefix.is_empty() {
            entry.track_lookup_prefix = p.track_lookup_prefix.clone();
        }
        entry.published.clear();
        entry.published.extend(p.published_tracks.iter().copied());
    }

    pub(super) fn roster_remove(&self, session_id: &str) {
        self.roster
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// Record a newly-published track for a participant, learning the
    /// `track_lookup_prefix` from the optional participant hint when present.
    pub(super) fn roster_add_track(
        &self,
        user_id: &str,
        session_id: &str,
        track_type: i32,
        hint: Option<&models::Participant>,
    ) {
        if session_id.is_empty() {
            return;
        }
        {
            let mut roster = self.roster.lock().unwrap_or_else(|e| e.into_inner());
            let entry = roster.entry(session_id.to_owned()).or_default();
            if let Some(participant) = hint {
                entry.participant.clone_from(participant);
                if !participant.track_lookup_prefix.is_empty() {
                    entry
                        .track_lookup_prefix
                        .clone_from(&participant.track_lookup_prefix);
                }
            }
            entry.user_id = user_id.to_owned();
            entry.session_id = session_id.to_owned();
            entry.published.insert(track_type);
            entry.participant.user_id = user_id.to_owned();
            entry.participant.session_id = session_id.to_owned();
            if !entry.participant.published_tracks.contains(&track_type) {
                entry.participant.published_tracks.push(track_type);
            }
        }
        // A republish clears any prior manual unsubscribe for this track.
        if let Ok(tt) = TrackType::try_from(track_type) {
            self.manual_unsub
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&TrackKey::new(session_id.to_owned(), tt));
        }
    }

    pub(super) fn roster_remove_track(&self, session_id: &str, track_type: i32) {
        let mut roster = self.roster.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = roster.get_mut(session_id) {
            entry.published.remove(&track_type);
            entry
                .participant
                .published_tracks
                .retain(|published| *published != track_type);
        }
    }

    pub(super) fn update_connection_quality(&self, updates: &[event::ConnectionQualityInfo]) {
        let mut roster = self
            .roster
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for update in updates {
            if let Some(entry) = roster.get_mut(&update.session_id) {
                entry.participant.connection_quality = update.connection_quality;
            }
        }
    }

    pub(super) fn update_audio_levels(&self, levels: &[event::AudioLevel]) {
        let mut roster = self
            .roster
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for entry in roster.values_mut() {
            entry.participant.is_speaking = false;
            entry.participant.audio_level = 0.0;
        }
        for level in levels {
            if let Some(entry) = roster.get_mut(&level.session_id) {
                entry.participant.is_speaking = level.is_speaking;
                entry.participant.audio_level = level.level;
            }
        }
    }

    pub(super) fn update_dominant_speaker(&self, session_id: &str) {
        let mut roster = self
            .roster
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for entry in roster.values_mut() {
            entry.participant.is_dominant_speaker = entry.session_id == session_id;
        }
    }

    pub(super) fn update_participant_count(&self, participant_count: models::ParticipantCount) {
        self.call_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .participant_count = participant_count;
    }

    pub(super) fn update_pins(&self, pins: Vec<models::Pin>) {
        self.call_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pins = pins;
    }

    pub(super) fn update_inbound_state(&self, states: &[event::InboundVideoState]) {
        let mut roster = self
            .roster
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for state in states {
            let Some(entry) = roster.get_mut(&state.session_id) else {
                continue;
            };
            if state.paused {
                entry.paused.insert(state.track_type);
            } else {
                entry.paused.remove(&state.track_type);
            }
        }
    }

    pub(super) fn update_call_grants(&self, grants: Option<models::CallGrants>) {
        self.call_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current_grants = grants;
    }
}
