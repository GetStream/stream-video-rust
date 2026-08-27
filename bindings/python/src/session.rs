//! [`RtcSession`] — SFU participant session bound to pre-fetched credentials.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pyo3::prelude::*;
use pyo3::types::{PyAny, PyType};
use pyo3_async_runtimes::tokio::future_into_py;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc;

use getstream::rtc::{
    JoinCallData, RemoteTrack, RtcCall, RtcClient, SubscriptionConfig, SubscriptionTarget,
};

use crate::credentials::{PySfuCredentials, PyStatsOptions};
use crate::error::{crate_err, rtc_err};
use crate::events::{PyEventStream, calling_state_str, event_to_py, json_to_py};
use crate::tracks::{PyLocalAudioTrack, PyLocalVideoTrack, PyRemoteTrack, parse_track_type};

const TRACK_QUEUE_CAP: usize = 32;

/// A live SFU session. Construct with [`RtcSession::join`](Self::join).
#[pyclass(name = "RtcSession", frozen)]
pub struct PyRtcSession {
    call: RtcCall,
    tracks: Arc<TokioMutex<mpsc::Receiver<RemoteTrack>>>,
    dropped_remote_tracks: Arc<AtomicU64>,
}

impl PyRtcSession {
    fn from_call(call: RtcCall) -> Self {
        let (tx, rx) = mpsc::channel(TRACK_QUEUE_CAP);
        let dropped_remote_tracks = Arc::new(AtomicU64::new(0));
        let overflow = dropped_remote_tracks.clone();
        call.on_track(move |track| {
            if let Err(error) = tx.try_send(track) {
                let dropped = overflow.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    dropped,
                    capacity = TRACK_QUEUE_CAP,
                    full = matches!(error, mpsc::error::TrySendError::Full(_)),
                    closed = matches!(error, mpsc::error::TrySendError::Closed(_)),
                    "stream.rtc.python.remote_track_queue_overflow"
                );
            }
        });
        Self {
            call,
            tracks: Arc::new(TokioMutex::new(rx)),
            dropped_remote_tracks,
        }
    }
}

#[pymethods]
impl PyRtcSession {
    /// Join the SFU with credentials already fetched by Python's coordinator.
    #[classmethod]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (
        api_key,
        user_token,
        call_type,
        call_id,
        user_id,
        credentials,
        stats_options=None,
        own_capabilities=None,
        opus_dtx_enabled=false,
    ))]
    fn join<'py>(
        _cls: &Bound<'py, PyType>,
        py: Python<'py>,
        api_key: String,
        user_token: String,
        call_type: String,
        call_id: String,
        user_id: String,
        credentials: Bound<'py, PySfuCredentials>,
        stats_options: Option<Bound<'py, PyStatsOptions>>,
        own_capabilities: Option<Vec<String>>,
        opus_dtx_enabled: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let stats = stats_options.as_ref().map(|opts| opts.borrow().clone());
        let injected = credentials.borrow().to_injected(
            stats.as_ref(),
            own_capabilities.unwrap_or_default(),
            opus_dtx_enabled,
        );
        future_into_py(py, async move {
            let client = RtcClient::new(api_key, user_token).map_err(crate_err)?;
            let call = client
                .join_with_credentials(call_type, call_id, JoinCallData::new(user_id), injected)
                .await
                .map_err(rtc_err)?;
            Ok(PyRtcSession::from_call(call))
        })
    }

    /// Leave the call and tear down PeerConnections.
    fn leave<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let call = self.call.clone();
        future_into_py(py, async move {
            call.leave().await.map_err(rtc_err)?;
            Ok(())
        })
    }

    fn publish_audio<'py>(
        &self,
        py: Python<'py>,
        track: Bound<'py, PyLocalAudioTrack>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let call = self.call.clone();
        let track = track.borrow().inner.clone();
        future_into_py(py, async move {
            call.publish_audio(track).await.map_err(rtc_err)?;
            Ok(())
        })
    }

    fn publish_video<'py>(
        &self,
        py: Python<'py>,
        track: Bound<'py, PyLocalVideoTrack>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let call = self.call.clone();
        let track = track.borrow().inner.clone();
        future_into_py(py, async move {
            call.publish_video(track).await.map_err(rtc_err)?;
            Ok(())
        })
    }

    fn publish_screen_share<'py>(
        &self,
        py: Python<'py>,
        track: Bound<'py, PyLocalVideoTrack>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let call = self.call.clone();
        let track = track.borrow().inner.clone();
        future_into_py(py, async move {
            call.publish_screen_share(track).await.map_err(rtc_err)?;
            Ok(())
        })
    }

    fn publish_screen_share_audio<'py>(
        &self,
        py: Python<'py>,
        track: Bound<'py, PyLocalAudioTrack>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let call = self.call.clone();
        let track = track.borrow().inner.clone();
        future_into_py(py, async move {
            call.publish_screen_share_audio(track)
                .await
                .map_err(rtc_err)?;
            Ok(())
        })
    }

    /// Await the next inbound [`RemoteTrack`](PyRemoteTrack), or `None` if the
    /// session is gone.
    fn next_track<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let tracks = self.tracks.clone();
        future_into_py(py, async move {
            let mut rx = tracks.lock().await;
            Ok(rx.recv().await.map(PyRemoteTrack::new))
        })
    }

    /// Async iterator over typed call events (`async for event in session.events()`).
    fn events(&self) -> PyEventStream {
        PyEventStream::new(self.call.subscribe())
    }

    /// Await a single call event, skipping lagged bursts.
    fn next_event<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let mut events = self.call.subscribe();
        future_into_py(py, async move {
            loop {
                match events.recv().await {
                    Ok(event) => return Python::with_gil(|py| event_to_py(py, &event)),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Python::with_gil(|py| Ok(py.None()));
                    }
                }
            }
        })
    }

    /// Publisher/subscriber `getStats` snapshot, or `None` if disconnected.
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let call = self.call.clone();
        future_into_py(py, async move {
            let Some(snapshot) = call.stats_snapshot().await else {
                return Python::with_gil(|py| Ok(py.None()));
            };
            let value = serde_json::json!({
                "publisher": snapshot.publisher,
                "subscriber": snapshot.subscriber,
            });
            Python::with_gil(|py| json_to_py(py, &value))
        })
    }

    fn session_id<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let call = self.call.clone();
        future_into_py(py, async move { Ok(call.session_id().await) })
    }

    #[getter]
    fn calling_state(&self) -> &'static str {
        calling_state_str(self.call.calling_state())
    }

    /// Inbound tracks dropped because [`next_track`](Self::next_track) lagged
    /// behind the `on_track` callback. Zero when the queue never overflowed.
    #[getter]
    fn dropped_remote_tracks(&self) -> u64 {
        self.dropped_remote_tracks.load(Ordering::Relaxed)
    }

    fn mute_track<'py>(&self, py: Python<'py>, track_type: String) -> PyResult<Bound<'py, PyAny>> {
        let call = self.call.clone();
        let track_type = parse_track_type(&track_type)?;
        future_into_py(py, async move {
            call.mute_track(track_type).await.map_err(rtc_err)?;
            Ok(())
        })
    }

    /// Coarse subscription policy applied to every remote participant.
    #[pyo3(signature = (audio=true, video=false, screen_share=false, video_width=None, video_height=None))]
    fn update_subscriptions<'py>(
        &self,
        py: Python<'py>,
        audio: bool,
        video: bool,
        screen_share: bool,
        video_width: Option<u32>,
        video_height: Option<u32>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let video_dimension = match (video_width, video_height) {
            (Some(width), Some(height)) => Some((width, height)),
            _ => None,
        };
        let config = SubscriptionConfig {
            audio,
            video,
            screen_share,
            video_dimension,
        };
        let call = self.call.clone();
        future_into_py(py, async move {
            call.update_subscriptions(config).await.map_err(rtc_err)?;
            Ok(())
        })
    }

    /// Exact per-session track list. Each item is
    /// `(session_id, track_type, width, height)` with optional dimensions.
    fn update_subscription_targets<'py>(
        &self,
        py: Python<'py>,
        targets: Vec<(String, String, Option<u32>, Option<u32>)>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mut parsed = Vec::with_capacity(targets.len());
        for (session_id, track_type, width, height) in targets {
            let mut target = SubscriptionTarget::new(session_id, parse_track_type(&track_type)?);
            if let (Some(width), Some(height)) = (width, height) {
                target = target.with_dimension(width, height);
            }
            parsed.push(target);
        }
        let call = self.call.clone();
        future_into_py(py, async move {
            call.update_subscription_targets(parsed)
                .await
                .map_err(rtc_err)?;
            Ok(())
        })
    }

    /// Snapshot of known participants (dicts matching SFU event payloads).
    fn participants(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let value = serde_json::json!(
            self.call
                .participants()
                .iter()
                .map(|participant| {
                    serde_json::json!({
                        "user_id": participant.user_id,
                        "session_id": participant.session_id,
                        "name": participant.name,
                        "image": participant.image,
                        "track_lookup_prefix": participant.track_lookup_prefix,
                        "is_speaking": participant.is_speaking,
                        "is_dominant_speaker": participant.is_dominant_speaker,
                        "audio_level": participant.audio_level,
                        "roles": participant.roles,
                        "published_tracks": participant
                            .published_tracks
                            .iter()
                            .map(|track_type| *track_type as i32)
                            .collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>()
        );
        json_to_py(py, &value)
    }

    fn unmute_track<'py>(
        &self,
        py: Python<'py>,
        track_type: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let call = self.call.clone();
        let track_type = parse_track_type(&track_type)?;
        future_into_py(py, async move {
            call.unmute_track(track_type).await.map_err(rtc_err)?;
            Ok(())
        })
    }

    fn __repr__(&self) -> String {
        format!("RtcSession(state={})", self.calling_state())
    }
}
