//! Typed [`CallEvent`](getstream::rtc::CallEvent) conversion to Python dicts.

use pyo3::exceptions::PyStopAsyncIteration;
use pyo3::prelude::*;
use pyo3::types::PyAny;
use pyo3_async_runtimes::tokio::future_into_py;
use serde_json::{Value, json};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::broadcast;

use getstream::rtc::proto::models::Participant;
use getstream::rtc::{CallEvent, CallingState};

use crate::error::json_err;

fn calling_state_name(state: CallingState) -> &'static str {
    match state {
        CallingState::Idle => "idle",
        CallingState::Joining => "joining",
        CallingState::Joined => "joined",
        CallingState::Reconnecting => "reconnecting",
        CallingState::Migrating => "migrating",
        CallingState::ReconnectingFailed => "reconnecting_failed",
        CallingState::Left => "left",
        CallingState::Offline => "offline",
    }
}

fn participant_json(participant: &Participant) -> Value {
    json!({
        "user_id": participant.user_id,
        "session_id": participant.session_id,
        "name": participant.name,
        "image": participant.image,
        "track_lookup_prefix": participant.track_lookup_prefix,
        "is_speaking": participant.is_speaking,
        "is_dominant_speaker": participant.is_dominant_speaker,
        "audio_level": participant.audio_level,
        "connection_quality": participant.connection_quality,
        "roles": participant.roles,
        "published_tracks": participant.published_tracks,
        "source": participant.source,
    })
}

fn event_to_json(event: &CallEvent) -> Value {
    match event {
        CallEvent::ParticipantJoined(p) => json!({
            "kind": "participant_joined",
            "participant": participant_json(p),
        }),
        CallEvent::ParticipantLeft(p) => json!({
            "kind": "participant_left",
            "participant": participant_json(p),
        }),
        CallEvent::ParticipantUpdated(p) => json!({
            "kind": "participant_updated",
            "participant": participant_json(p),
        }),
        CallEvent::Coordinator(event) => json!({
            "kind": "coordinator",
            "event_type": event.event_type,
            "raw": event.raw,
        }),
        CallEvent::TrackPublished {
            user_id,
            session_id,
            track_type,
        } => json!({
            "kind": "track_published",
            "user_id": user_id,
            "session_id": session_id,
            "track_type": track_type,
        }),
        CallEvent::TrackUnpublished {
            user_id,
            session_id,
            track_type,
        } => json!({
            "kind": "track_unpublished",
            "user_id": user_id,
            "session_id": session_id,
            "track_type": track_type,
        }),
        CallEvent::DominantSpeakerChanged {
            user_id,
            session_id,
        } => json!({
            "kind": "dominant_speaker_changed",
            "user_id": user_id,
            "session_id": session_id,
        }),
        CallEvent::AudioLevelChanged(levels) => json!({
            "kind": "audio_level_changed",
            "levels": levels.iter().map(|level| json!({
                "user_id": level.user_id,
                "session_id": level.session_id,
                "level": level.level,
                "is_speaking": level.is_speaking,
            })).collect::<Vec<_>>(),
        }),
        CallEvent::ConnectionQualityChanged(info) => json!({
            "kind": "connection_quality_changed",
            "updates": info.iter().map(|item| json!({
                "user_id": item.user_id,
                "session_id": item.session_id,
                "connection_quality": item.connection_quality,
            })).collect::<Vec<_>>(),
        }),
        CallEvent::ParticipantCountChanged(count) => json!({
            "kind": "participant_count_changed",
            "total": count.total,
            "anonymous": count.anonymous,
        }),
        CallEvent::PinsUpdated(pins) => json!({
            "kind": "pins_updated",
            "pins": pins.iter().map(|pin| json!({
                "user_id": pin.user_id,
                "session_id": pin.session_id,
            })).collect::<Vec<_>>(),
        }),
        CallEvent::InboundStateChanged(states) => json!({
            "kind": "inbound_state_changed",
            "states": states.iter().map(|state| json!({
                "session_id": state.session_id,
                "track_type": state.track_type,
                "paused": state.paused,
            })).collect::<Vec<_>>(),
        }),
        CallEvent::PublishOptionsChanged {
            publish_options,
            reason,
        } => json!({
            "kind": "publish_options_changed",
            "reason": reason,
            "option_count": publish_options.len(),
        }),
        CallEvent::PublishQualityChanged(_) => json!({
            "kind": "publish_quality_changed",
        }),
        CallEvent::CallGrantsUpdated(_) => json!({
            "kind": "call_grants_updated",
        }),
        CallEvent::IceRestarted(peer_type) => json!({
            "kind": "ice_restarted",
            "peer_type": *peer_type as i32,
        }),
        CallEvent::Error(error) => json!({
            "kind": "error",
            "code": error.code,
            "message": error.message,
            "should_retry": error.should_retry,
            "reconnect_strategy": error.reconnect_strategy,
        }),
        CallEvent::CallEnded => json!({"kind": "call_ended"}),
        CallEvent::CallingStateChanged(state) => json!({
            "kind": "calling_state_changed",
            "state": calling_state_name(*state),
        }),
        other => json!({
            "kind": "unknown",
            "debug": format!("{other:?}"),
        }),
    }
}

pub(crate) fn json_to_py(py: Python<'_>, value: &Value) -> PyResult<Py<PyAny>> {
    let json = py.import("json")?;
    let text = serde_json::to_string(value).map_err(json_err)?;
    Ok(json.call_method1("loads", (text,))?.unbind())
}

pub(crate) fn event_to_py(py: Python<'_>, event: &CallEvent) -> PyResult<Py<PyAny>> {
    json_to_py(py, &event_to_json(event))
}

/// Async iterator over a session's [`CallEvent`] stream.
#[pyclass(name = "EventStream", frozen)]
pub struct PyEventStream {
    events: std::sync::Arc<TokioMutex<broadcast::Receiver<CallEvent>>>,
}

impl PyEventStream {
    pub(crate) fn new(events: broadcast::Receiver<CallEvent>) -> Self {
        Self {
            events: std::sync::Arc::new(TokioMutex::new(events)),
        }
    }
}

#[pymethods]
impl PyEventStream {
    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let events = self.events.clone();
        future_into_py(py, async move {
            loop {
                let result = {
                    let mut rx = events.lock().await;
                    rx.recv().await
                };
                match result {
                    Ok(event) => {
                        return Python::with_gil(|py| event_to_py(py, &event));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(PyStopAsyncIteration::new_err("event stream closed"));
                    }
                }
            }
        })
    }
}

pub(crate) fn calling_state_str(state: CallingState) -> &'static str {
    calling_state_name(state)
}
