//! Native `getstream_rtc_core` extension module.
//!
//! The module owns a dedicated multi-thread Tokio runtime. Python `async def`
//! methods are bridged onto that runtime with `pyo3-async-runtimes`.

use pyo3::prelude::*;

mod runtime;

/// Python version of this wheel, matching the native crate.
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    runtime::init();
    m.add("__version__", VERSION)?;
    Ok(())
}
