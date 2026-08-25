//! Map SDK errors onto a Python `RtcError` exception.

use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;

create_exception!(
    getstream_rtc_core,
    RtcError,
    PyException,
    "A Stream RTC session or media-path failure."
);

pub(crate) fn rtc_err(error: getstream::rtc::RtcError) -> PyErr {
    RtcError::new_err(error.to_string())
}

pub(crate) fn crate_err(error: getstream::Error) -> PyErr {
    RtcError::new_err(error.to_string())
}

pub(crate) fn json_err(error: serde_json::Error) -> PyErr {
    RtcError::new_err(format!("json conversion failed: {error}"))
}
