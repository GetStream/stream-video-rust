//! Coordinator-join credentials passed into [`RtcSession::join`](crate::session::PyRtcSession).

use pyo3::prelude::*;

use getstream::rtc::{Credentials, IceServer, InjectedSfuJoin, SfuServer, StatsOptions};

/// One STUN/TURN server from coordinator join credentials.
#[pyclass(name = "IceServer", frozen)]
#[derive(Clone)]
pub struct PyIceServer {
    #[pyo3(get)]
    urls: Vec<String>,
    #[pyo3(get)]
    username: String,
    password: String,
}

#[pymethods]
impl PyIceServer {
    #[new]
    #[pyo3(signature = (urls, username=None, password=None))]
    fn new(urls: Vec<String>, username: Option<String>, password: Option<String>) -> Self {
        Self {
            urls,
            username: username.unwrap_or_default(),
            password: password.unwrap_or_default(),
        }
    }

    #[getter]
    fn password(&self) -> &str {
        &self.password
    }

    fn __repr__(&self) -> String {
        format!(
            "IceServer(urls={:?}, username={:?}, password='<redacted>')",
            self.urls, self.username
        )
    }
}

impl PyIceServer {
    fn to_sdk(&self) -> IceServer {
        IceServer::new(
            self.urls.clone(),
            self.username.clone(),
            self.password.clone(),
        )
    }
}

/// Cached stats options from a coordinator join response.
#[pyclass(name = "StatsOptions", frozen)]
#[derive(Clone)]
pub struct PyStatsOptions {
    #[pyo3(get)]
    reporting_interval_ms: i32,
    #[pyo3(get)]
    enable_rtc_stats: bool,
}

#[pymethods]
impl PyStatsOptions {
    #[new]
    #[pyo3(signature = (reporting_interval_ms=0, enable_rtc_stats=false))]
    fn new(reporting_interval_ms: i32, enable_rtc_stats: bool) -> Self {
        Self {
            reporting_interval_ms,
            enable_rtc_stats,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "StatsOptions(reporting_interval_ms={}, enable_rtc_stats={})",
            self.reporting_interval_ms, self.enable_rtc_stats
        )
    }
}

impl PyStatsOptions {
    fn to_sdk(&self) -> StatsOptions {
        StatsOptions::new(self.reporting_interval_ms, self.enable_rtc_stats)
    }
}

/// Pre-fetched SFU credentials from Python's coordinator join.
#[pyclass(name = "SfuCredentials", frozen)]
#[derive(Clone)]
pub struct PySfuCredentials {
    #[pyo3(get)]
    edge_name: String,
    #[pyo3(get)]
    url: String,
    #[pyo3(get)]
    ws_endpoint: String,
    token: String,
    ice_servers: Vec<PyIceServer>,
}

#[pymethods]
impl PySfuCredentials {
    #[new]
    #[pyo3(signature = (edge_name, url, ws_endpoint, token, ice_servers=None))]
    fn new(
        edge_name: String,
        url: String,
        ws_endpoint: String,
        token: String,
        ice_servers: Option<Vec<PyIceServer>>,
    ) -> Self {
        Self {
            edge_name,
            url,
            ws_endpoint,
            token,
            ice_servers: ice_servers.unwrap_or_default(),
        }
    }

    #[getter]
    fn ice_servers(&self) -> Vec<PyIceServer> {
        self.ice_servers.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "SfuCredentials(edge_name={:?}, url={:?}, ws_endpoint={:?}, token='<redacted>', ice_servers={})",
            self.edge_name,
            self.url,
            self.ws_endpoint,
            self.ice_servers.len()
        )
    }
}

impl PySfuCredentials {
    pub(crate) fn to_injected(
        &self,
        stats_options: Option<&PyStatsOptions>,
        own_capabilities: Vec<String>,
    ) -> InjectedSfuJoin {
        InjectedSfuJoin {
            credentials: Credentials::new(
                SfuServer::new(
                    self.edge_name.clone(),
                    self.url.clone(),
                    self.ws_endpoint.clone(),
                ),
                self.token.clone(),
                self.ice_servers.iter().map(PyIceServer::to_sdk).collect(),
            ),
            stats_options: stats_options
                .map(PyStatsOptions::to_sdk)
                .unwrap_or_default(),
            own_capabilities,
        }
    }
}
