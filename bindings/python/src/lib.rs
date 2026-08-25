//! Native `getstream_rtc_core` extension module.
//!
//! The module owns a dedicated multi-thread Tokio runtime. Python `async def`
//! methods are bridged onto that runtime with `pyo3-async-runtimes`.

use pyo3::prelude::*;

mod credentials;
mod error;
mod events;
mod frames;
mod runtime;
mod session;
mod tracks;

/// Python version of this wheel, matching the native crate.
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    runtime::init();
    m.add("__version__", VERSION)?;
    m.add("RtcError", m.py().get_type::<error::RtcError>())?;
    m.add_class::<credentials::PyIceServer>()?;
    m.add_class::<credentials::PyStatsOptions>()?;
    m.add_class::<credentials::PySfuCredentials>()?;
    m.add_class::<frames::PyPcmFrame>()?;
    m.add_class::<frames::PyVideoFrame>()?;
    m.add_class::<tracks::PyLocalAudioTrack>()?;
    m.add_class::<tracks::PyLocalVideoTrack>()?;
    m.add_class::<tracks::PyRemoteTrack>()?;
    m.add_class::<events::PyEventStream>()?;
    m.add_class::<session::PyRtcSession>()?;
    Ok(())
}
