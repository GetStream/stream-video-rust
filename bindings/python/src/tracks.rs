//! Local and remote media tracks.

use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyAny;
use pyo3_async_runtimes::tokio::future_into_py;

use getstream::rtc::proto::models::TrackType;
use getstream::rtc::{LocalAudioTrack, LocalVideoTrack, RemoteTrack};

use crate::error::rtc_err;
use crate::frames::{PyPcmFrame, PyVideoFrame, duration_from_ms, read_bytes, read_i16_samples};

fn track_type_name(track_type: TrackType) -> &'static str {
    match track_type {
        TrackType::Audio => "audio",
        TrackType::Video => "video",
        TrackType::ScreenShare => "screenshare",
        TrackType::ScreenShareAudio => "screenshare_audio",
        _ => "unspecified",
    }
}

/// An outbound Opus audio track. Feed it PCM via [`write_pcm`](Self::write_pcm).
#[pyclass(name = "LocalAudioTrack", frozen)]
#[derive(Clone)]
pub struct PyLocalAudioTrack {
    pub(crate) inner: LocalAudioTrack,
}

#[pymethods]
impl PyLocalAudioTrack {
    /// Build a 48 kHz mono Opus track.
    #[staticmethod]
    fn opus() -> PyResult<Self> {
        Ok(Self {
            inner: LocalAudioTrack::opus().map_err(rtc_err)?,
        })
    }

    /// Queue interleaved s16 PCM for the paced Opus encoder.
    ///
    /// `samples` is a buffer-protocol object of little-endian int16 (or a
    /// packed byte buffer whose length is a multiple of 2).
    fn write_pcm<'py>(
        &self,
        py: Python<'py>,
        samples: Bound<'py, PyAny>,
        sample_rate: u32,
        channels: u16,
    ) -> PyResult<Bound<'py, PyAny>> {
        let samples = read_i16_samples(&samples)?;
        let frame = getstream::rtc::PcmFrame::new(samples, sample_rate, channels);
        let track = self.inner.clone();
        future_into_py(py, async move {
            track.write_pcm(frame).await.map_err(rtc_err)?;
            Ok(())
        })
    }

    fn __repr__(&self) -> &'static str {
        "LocalAudioTrack(opus)"
    }
}

/// An outbound video track. Feed it packed I420 via [`write_i420`](Self::write_i420).
#[pyclass(name = "LocalVideoTrack", frozen)]
#[derive(Clone)]
pub struct PyLocalVideoTrack {
    pub(crate) inner: LocalVideoTrack,
    codec: &'static str,
}

#[pymethods]
impl PyLocalVideoTrack {
    #[staticmethod]
    fn vp8() -> PyResult<Self> {
        Ok(Self {
            inner: LocalVideoTrack::vp8().map_err(rtc_err)?,
            codec: "vp8",
        })
    }

    #[staticmethod]
    fn vp9() -> PyResult<Self> {
        Ok(Self {
            inner: LocalVideoTrack::vp9().map_err(rtc_err)?,
            codec: "vp9",
        })
    }

    #[staticmethod]
    fn h264() -> PyResult<Self> {
        Ok(Self {
            inner: LocalVideoTrack::h264().map_err(rtc_err)?,
            codec: "h264",
        })
    }

    /// Encode and publish a packed I420 frame.
    ///
    /// `data` is a buffer-protocol object. `duration_ms` advances the RTP clock.
    fn write_i420<'py>(
        &self,
        py: Python<'py>,
        data: Bound<'py, PyAny>,
        width: u32,
        height: u32,
        duration_ms: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let data = read_bytes(&data)?;
        let duration = duration_from_ms(duration_ms)?;
        let track = self.inner.clone();
        future_into_py(py, async move {
            track
                .write_i420_vec(data, width, height, duration)
                .await
                .map_err(rtc_err)?;
            Ok(())
        })
    }

    fn __repr__(&self) -> String {
        format!("LocalVideoTrack(codec={})", self.codec)
    }
}

/// An inbound track delivered after a subscription lands.
#[pyclass(name = "RemoteTrack", frozen)]
pub struct PyRemoteTrack {
    inner: Arc<RemoteTrack>,
}

impl PyRemoteTrack {
    pub(crate) fn new(inner: RemoteTrack) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }
}

#[pymethods]
impl PyRemoteTrack {
    #[getter]
    fn user_id(&self) -> &str {
        &self.inner.participant().user_id
    }

    #[getter]
    fn session_id(&self) -> &str {
        &self.inner.participant().session_id
    }

    #[getter]
    fn track_lookup_prefix(&self) -> &str {
        &self.inner.participant().track_lookup_prefix
    }

    #[getter]
    fn track_type(&self) -> &'static str {
        track_type_name(self.inner.track_type())
    }

    #[getter]
    fn mime_type(&self) -> &str {
        &self.inner.codec().mime_type
    }

    /// Decode the next 48 kHz mono s16 PCM frame, or `None` when the track ends.
    fn next_pcm<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let track = self.inner.clone();
        future_into_py(py, async move {
            Ok(track.next_pcm().await.map(PyPcmFrame::from_sdk))
        })
    }

    /// Decode the next packed-I420 video frame, or `None` when the track ends.
    fn next_video_frame<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let track = self.inner.clone();
        future_into_py(py, async move {
            Ok(track.next_video_frame().await.map(PyVideoFrame::from_sdk))
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "RemoteTrack(user_id={:?}, track_type={})",
            self.user_id(),
            self.track_type()
        )
    }
}

pub(crate) fn parse_track_type(name: &str) -> PyResult<TrackType> {
    match name {
        "audio" => Ok(TrackType::Audio),
        "video" => Ok(TrackType::Video),
        "screenshare" | "screen_share" => Ok(TrackType::ScreenShare),
        "screenshare_audio" | "screen_share_audio" => Ok(TrackType::ScreenShareAudio),
        other => Err(PyValueError::new_err(format!(
            "unknown track type {other:?}"
        ))),
    }
}
