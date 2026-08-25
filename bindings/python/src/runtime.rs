//! Dedicated Tokio runtime owned by the Python module.

use std::sync::Once;

/// Install the module-owned multi-thread Tokio runtime.
///
/// `pyo3-async-runtimes` lazily builds the runtime on first use from this
/// builder. Calling `init` more than once is a no-op.
pub(crate) fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder
            .enable_all()
            .thread_name("getstream-rtc")
            .worker_threads(4);
        pyo3_async_runtimes::tokio::init(builder);
    });
}
