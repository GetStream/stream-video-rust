//! PCM and packed-I420 frames. Payloads are `bytes` (buffer-protocol via the
//! built-in `bytes` type). Native `__getbuffer__` is unavailable on the
//! Python 3.10 limited ABI.

use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyByteArray, PyBytes};

/// Interleaved little-endian s16 PCM.
#[pyclass(name = "PcmFrame", frozen)]
pub struct PyPcmFrame {
    samples: Vec<i16>,
    sample_bytes: Vec<u8>,
    #[pyo3(get)]
    sample_rate: u32,
    #[pyo3(get)]
    channels: u16,
}

impl PyPcmFrame {
    pub(crate) fn from_sdk(frame: getstream::rtc::PcmFrame) -> Self {
        let sample_bytes = i16_to_le_bytes(&frame.samples);
        Self {
            samples: frame.samples,
            sample_bytes,
            sample_rate: frame.sample_rate,
            channels: frame.channels,
        }
    }
}

#[pymethods]
impl PyPcmFrame {
    #[new]
    #[pyo3(signature = (samples, sample_rate, channels=1))]
    fn new(samples: Bound<'_, PyAny>, sample_rate: u32, channels: u16) -> PyResult<Self> {
        let samples = read_i16_samples(&samples)?;
        let sample_bytes = i16_to_le_bytes(&samples);
        Ok(Self {
            samples,
            sample_rate,
            channels: channels.max(1),
            sample_bytes,
        })
    }

    /// Packed little-endian int16 sample bytes. `bytes` implements the buffer
    /// protocol, so callers can pass this to NumPy as `dtype='<i2'`.
    #[getter]
    fn samples<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        copy_to_pybytes(py, &self.sample_bytes)
    }

    #[getter]
    fn duration_ms(&self) -> f64 {
        if self.sample_rate == 0 || self.channels == 0 {
            return 0.0;
        }
        let frames = self.samples.len() as f64 / f64::from(self.channels);
        frames / f64::from(self.sample_rate) * 1000.0
    }

    fn __bytes__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        copy_to_pybytes(py, &self.sample_bytes)
    }

    fn __len__(&self) -> usize {
        self.samples.len()
    }

    fn __repr__(&self) -> String {
        format!(
            "PcmFrame(samples={}, sample_rate={}, channels={})",
            self.samples.len(),
            self.sample_rate,
            self.channels
        )
    }
}

/// Packed I420 video frame.
#[pyclass(name = "VideoFrame", frozen)]
pub struct PyVideoFrame {
    data: Vec<u8>,
    #[pyo3(get)]
    width: u32,
    #[pyo3(get)]
    height: u32,
    #[pyo3(get)]
    rtp_timestamp: u32,
}

impl PyVideoFrame {
    pub(crate) fn from_sdk(frame: getstream::rtc::VideoFrame) -> Self {
        Self {
            data: frame.data,
            width: frame.width,
            height: frame.height,
            rtp_timestamp: frame.rtp_timestamp,
        }
    }
}

#[pymethods]
impl PyVideoFrame {
    /// Packed I420 bytes (Y then U then V).
    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        copy_to_pybytes(py, &self.data)
    }

    fn __bytes__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        copy_to_pybytes(py, &self.data)
    }

    fn __len__(&self) -> usize {
        self.data.len()
    }

    fn __repr__(&self) -> String {
        format!(
            "VideoFrame(width={}, height={}, bytes={})",
            self.width,
            self.height,
            self.data.len()
        )
    }
}

fn i16_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

fn copy_to_pybytes<'py>(py: Python<'py>, src: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
    PyBytes::new_with(py, src.len(), |dest| {
        py.allow_threads(|| dest.copy_from_slice(src));
        Ok(())
    })
}

pub(crate) fn read_i16_samples(obj: &Bound<'_, PyAny>) -> PyResult<Vec<i16>> {
    let bytes = read_bytes(obj)?;
    if !bytes.len().is_multiple_of(2) {
        return Err(PyValueError::new_err(
            "pcm sample buffer length must be a multiple of 2",
        ));
    }
    Ok(obj.py().allow_threads(|| {
        let mut samples = Vec::with_capacity(bytes.len() / 2);
        for chunk in bytes.chunks_exact(2) {
            samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }
        samples
    }))
}

pub(crate) fn read_bytes(obj: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(bytes) = obj.downcast::<PyBytes>() {
        let slice = bytes.as_bytes();
        return Ok(obj.py().allow_threads(|| slice.to_vec()));
    }
    if let Ok(array) = obj.downcast::<PyByteArray>() {
        return Ok(array.to_vec());
    }
    let converted = obj
        .py()
        .import("builtins")?
        .getattr("bytes")?
        .call1((obj,))?;
    let bytes = converted.downcast::<PyBytes>()?;
    let slice = bytes.as_bytes();
    Ok(obj.py().allow_threads(|| slice.to_vec()))
}

pub(crate) fn duration_from_ms(duration_ms: f64) -> PyResult<Duration> {
    if !duration_ms.is_finite() || duration_ms < 0.0 {
        return Err(PyValueError::new_err(
            "duration_ms must be a finite non-negative number",
        ));
    }
    Ok(Duration::from_secs_f64(duration_ms / 1000.0))
}
