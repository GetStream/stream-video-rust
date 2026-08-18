//! HTTP transport, JWT auth, opt-in retry, and tracing for the Stream API.
//!
//! Connection defaults match getstream-go: max 5 connections per host, 55s idle
//! timeout, 30s request timeout, 10s connect timeout. Requests are authenticated
//! with a server JWT (`{"server": true}`) sent as `Authorization` +
//! `Stream-Auth-Type: jwt`, and every request carries the `api_key` query param.
//!
//! The HTTP client itself is crate-private. Public tunables ([`ClientConfig`],
//! [`RetryConfig`], [`NetworkLimits`]) are re-exported at the crate root for
//! [`crate::Stream::with_config`].

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;

use crate::error::{ApiError, Error, Result};
use crate::token;

/// Default coordinator base URL (matches getstream-go's `DefaultBaseURL`).
pub const DEFAULT_BASE_URL: &str = "https://chat.stream-io-api.com";

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(55);
const DEFAULT_MAX_CONNS_PER_HOST: usize = 5;
/// Default maximum body accepted from coordinator HTTP endpoints (16 MiB).
pub const DEFAULT_MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Default maximum inbound SFU/coordinator WebSocket frame and message size (4 MiB).
pub const DEFAULT_MAX_WEBSOCKET_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const REDACTED_BODY_KEYS: [&str; 3] = ["api_secret", "token", "password"];

/// Limits for payloads buffered by the HTTP and WebSocket transports.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct NetworkLimits {
    /// Maximum accepted HTTP response body size.
    pub max_response_body_bytes: usize,
    /// Maximum accepted inbound WebSocket frame/message size.
    pub max_websocket_message_bytes: usize,
}

impl Default for NetworkLimits {
    fn default() -> Self {
        Self {
            max_response_body_bytes: DEFAULT_MAX_RESPONSE_BODY_BYTES,
            max_websocket_message_bytes: DEFAULT_MAX_WEBSOCKET_MESSAGE_BYTES,
        }
    }
}

impl NetworkLimits {
    /// Override the maximum accepted HTTP response body size.
    #[must_use]
    pub fn with_max_response_body_bytes(mut self, limit: usize) -> Self {
        self.max_response_body_bytes = limit;
        self
    }

    /// Override the maximum accepted WebSocket frame/message size.
    #[must_use]
    pub fn with_max_websocket_message_bytes(mut self, limit: usize) -> Self {
        self.max_websocket_message_bytes = limit;
        self
    }
}

/// Opt-in auto-retry policy. Disabled by default: exactly one attempt, errors
/// surface unchanged. When enabled, only `GET`/`HEAD` requests failing with HTTP
/// 429 or a transport error are retried, and never when the API marked the error
/// `unrecoverable`. Mirrors getstream-go's `WithRetry`.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Turns retries on. Default `false`.
    pub enabled: bool,
    /// Total attempt budget including the initial request. Default 3.
    pub max_attempts: u32,
    /// Caps every wait between attempts, including `Retry-After` hints. Default 30s.
    pub max_backoff: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_attempts: 3,
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Tunables for the HTTP client. Use [`ClientConfig::default`] for getstream-go
/// defaults and override fields as needed before constructing a [`crate::Stream`].
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Coordinator base URL. Defaults to [`DEFAULT_BASE_URL`].
    pub base_url: String,
    /// Per-request timeout. Default 30s.
    pub request_timeout: Duration,
    /// TCP + TLS connect timeout. Default 10s.
    pub connect_timeout: Duration,
    /// Idle connection lifetime. Default 55s.
    pub idle_timeout: Duration,
    /// Max concurrent connections per host. Default 5.
    pub max_conns_per_host: usize,
    /// Opt-in retry policy. Disabled by default.
    pub retry: RetryConfig,
    /// Log request/response bodies on DEBUG events. Off by default; auth headers
    /// and known secret keys are redacted regardless. Other fields, including
    /// PII and secrets stored under custom keys, remain visible when enabled.
    pub log_bodies: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            max_conns_per_host: DEFAULT_MAX_CONNS_PER_HOST,
            retry: RetryConfig::default(),
            log_bodies: false,
        }
    }
}

/// Shared HTTP client. Cheap to clone via `Arc` from [`crate::Stream`].
pub(crate) struct Client {
    api_key: String,
    /// API secret bytes used for JWT/webhook HMAC. Never logged or serialized.
    api_secret: Vec<u8>,
    server_token: String,
    base_url: Url,
    http: reqwest::Client,
    retry: RetryConfig,
    log_bodies: bool,
    max_response_body_bytes: usize,
    max_websocket_message_bytes: usize,
    stream_client_header: String,
}

// Manual `Debug` so the API secret and server JWT can never leak through a
// `{:?}` log line; both are redacted.
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("api_key", &self.api_key)
            .field("api_secret", &"<redacted>")
            .field("server_token", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("retry", &self.retry)
            .field("log_bodies", &self.log_bodies)
            .field("max_response_body_bytes", &self.max_response_body_bytes)
            .field(
                "max_websocket_message_bytes",
                &self.max_websocket_message_bytes,
            )
            .field("stream_client_header", &self.stream_client_header)
            .finish_non_exhaustive()
    }
}

impl Client {
    pub(crate) fn new(api_key: String, api_secret: String, config: ClientConfig) -> Result<Self> {
        Self::new_with_limits(api_key, api_secret, config, NetworkLimits::default())
    }

    pub(crate) fn new_with_limits(
        api_key: String,
        api_secret: String,
        config: ClientConfig,
        limits: NetworkLimits,
    ) -> Result<Self> {
        if api_key.is_empty() {
            return Err(Error::Config("API key is empty".into()));
        }
        if api_secret.is_empty() {
            return Err(Error::Config("API secret is empty".into()));
        }

        let base_url = Url::parse(&config.base_url)
            .map_err(|e| Error::Config(format!("invalid base URL {:?}: {e}", config.base_url)))?;

        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(config.max_conns_per_host)
            .pool_idle_timeout(config.idle_timeout)
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .build()
            .map_err(Error::Transport)?;

        let secret_bytes = api_secret.into_bytes();
        let server_token = token::create_server_token(&secret_bytes)?;

        Ok(Self {
            api_key,
            api_secret: secret_bytes,
            server_token,
            base_url,
            http,
            retry: config.retry,
            log_bodies: config.log_bodies,
            max_response_body_bytes: limits.max_response_body_bytes,
            max_websocket_message_bytes: limits.max_websocket_message_bytes,
            stream_client_header: format!("stream-rust-{}", env!("CARGO_PKG_VERSION")),
        })
    }

    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }

    pub(crate) fn api_secret(&self) -> &[u8] {
        &self.api_secret
    }

    pub(crate) fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// The shared `reqwest` client (connection pool). Used by the RTC layer to
    /// reuse the pool for coordinator join + SFU Twirp calls.
    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub(crate) fn max_response_body_bytes(&self) -> usize {
        self.max_response_body_bytes
    }

    pub(crate) fn max_websocket_message_bytes(&self) -> usize {
        self.max_websocket_message_bytes
    }

    /// Perform a single authenticated request as a **user** (not the server
    /// token): the participant join path authenticates with the user's JWT.
    /// No retry — the join loop owns recovery.
    pub(crate) async fn request_as_user<B, R>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
        user_token: &str,
    ) -> Result<R>
    where
        B: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let mut url = self
            .base_url
            .join(path)
            .map_err(|e| Error::Config(format!("invalid path {path:?}: {e}")))?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("api_key", &self.api_key);
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }

        let mut req = self
            .http
            .request(method.clone(), url)
            .header("X-Stream-Client", &self.stream_client_header)
            .header(AUTHORIZATION, user_token)
            .header("Stream-Auth-Type", "jwt");

        if method != Method::GET && method != Method::HEAD {
            let bytes = match body {
                Some(b) => serde_json::to_vec(b)?,
                None => b"{}".to_vec(),
            };
            if self.log_bodies {
                tracing::debug!(
                    body = %redact_json_body(&bytes),
                    "stream.http.request_body"
                );
            }
            req = req.header(CONTENT_TYPE, "application/json").body(bytes);
        }

        let resp = req.send().await.map_err(Error::Transport)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = read_response_body_limited(resp, self.max_response_body_bytes).await?;

        if self.log_bodies {
            tracing::debug!(
                status = status.as_u16(),
                body = %redact_json_body(&bytes),
                "stream.http.response_body"
            );
        }
        if status.as_u16() >= 399 {
            return Err(Error::from(build_api_error(status, &headers, &bytes)));
        }

        serde_json::from_slice(&bytes).map_err(Error::from)
    }

    /// Substitute `{name}` placeholders in a path template, URL-encoding values.
    pub(crate) fn build_path(template: &str, params: &[(&str, &str)]) -> String {
        let mut path = template.to_string();
        for (key, value) in params {
            let encoded: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
            path = path.replace(&format!("{{{key}}}"), &encoded);
        }
        path
    }

    /// Perform a request with the client's opt-in retry policy.
    ///
    /// `path` is the already-substituted path (see [`Client::build_path`]).
    /// `query` are extra query params (the `api_key` param is always added).
    pub(crate) async fn request<B, R>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
    ) -> Result<R>
    where
        B: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let mut url = self
            .base_url
            .join(path)
            .map_err(|e| Error::Config(format!("invalid path {path:?}: {e}")))?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("api_key", &self.api_key);
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }

        let body_bytes = match body {
            Some(b) => Some(serde_json::to_vec(b)?),
            None => None,
        };

        let mut attempt: u32 = 0;
        loop {
            let started = SystemTime::now();
            let result = self.attempt(&method, &url, body_bytes.as_deref()).await;

            match &result {
                Ok(_) => {
                    self.log_response(&method, path, StatusCode::OK, started);
                    return result;
                }
                Err(err) => {
                    if !self.should_retry(err, &method, attempt) {
                        return result;
                    }
                    let delay = self.retry_delay(err, attempt);
                    tracing::debug!(
                        method = %method,
                        path,
                        attempt = attempt + 1,
                        delay_ms = delay.as_millis() as u64,
                        "stream.http.retry_scheduled"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn attempt<R: DeserializeOwned>(
        &self,
        method: &Method,
        url: &Url,
        body: Option<&[u8]>,
    ) -> Result<R> {
        let mut req = self
            .http
            .request(method.clone(), url.clone())
            .header("X-Stream-Client", &self.stream_client_header)
            .header(AUTHORIZATION, &self.server_token)
            .header("Stream-Auth-Type", "jwt");

        if method != Method::GET
            && method != Method::HEAD
            && let Some(bytes) = body
        {
            req = req
                .header(CONTENT_TYPE, "application/json")
                .body(bytes.to_vec());
            if self.log_bodies {
                tracing::debug!(
                    body = %redact_json_body(bytes),
                    "stream.http.request_body"
                );
            }
        }

        let resp = req.send().await.map_err(Error::Transport)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = read_response_body_limited(resp, self.max_response_body_bytes).await?;

        if status.as_u16() >= 399 {
            return Err(Error::from(build_api_error(status, &headers, &bytes)));
        }

        if self.log_bodies {
            tracing::debug!(
                status = status.as_u16(),
                body = %redact_json_body(&bytes),
                "stream.http.response_body"
            );
        }

        serde_json::from_slice(&bytes).map_err(Error::from)
    }

    fn should_retry(&self, err: &Error, method: &Method, attempt: u32) -> bool {
        if !self.retry.enabled {
            return false;
        }
        if method != Method::GET && method != Method::HEAD {
            return false;
        }
        if attempt + 1 >= self.retry.max_attempts {
            return false;
        }
        err.is_retryable()
    }

    fn retry_delay(&self, err: &Error, attempt: u32) -> Duration {
        if let Some(retry_after) = err.as_api_error().and_then(|e| e.retry_after) {
            return retry_after.min(self.retry.max_backoff);
        }
        // Exponential backoff with full jitter, capped at max_backoff.
        let ceil_ms = (1000u64.saturating_mul(1u64 << attempt.min(20)))
            .min(u64::try_from(self.retry.max_backoff.as_millis()).unwrap_or(u64::MAX));
        if ceil_ms == 0 {
            return Duration::ZERO;
        }
        Duration::from_millis(jitter(ceil_ms))
    }

    fn log_response(&self, method: &Method, path: &str, status: StatusCode, started: SystemTime) {
        let elapsed_ms = started.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0);
        tracing::debug!(
            method = %method,
            path,
            status = status.as_u16(),
            duration_ms = elapsed_ms,
            "stream.http.response_received"
        );
    }
}

pub(crate) async fn read_response_body_limited(
    response: reqwest::Response,
    limit: usize,
) -> Result<Bytes> {
    let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
    if let Some(declared) = response.content_length()
        && declared > limit_u64
    {
        return Err(Error::ResponseTooLarge {
            limit,
            actual: usize::try_from(declared).unwrap_or(usize::MAX),
        });
    }
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(limit);
    let mut body = BytesMut::with_capacity(capacity);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Error::Transport)?;
        checked_body_size(body.len(), chunk.len(), limit)?;
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

fn checked_body_size(current: usize, incoming: usize, limit: usize) -> Result<usize> {
    let Some(actual) = current.checked_add(incoming) else {
        return Err(Error::ResponseTooLarge {
            limit,
            actual: usize::MAX,
        });
    };
    if actual > limit {
        return Err(Error::ResponseTooLarge { limit, actual });
    }
    Ok(actual)
}

fn redact_json_body(body: &[u8]) -> String {
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return "<non-JSON body omitted>".to_owned();
    };
    redact_json_value(&mut value);
    serde_json::to_string(&value).unwrap_or_else(|_| "<JSON body omitted>".to_owned())
}

fn redact_json_value(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if REDACTED_BODY_KEYS
                    .iter()
                    .any(|secret| key.eq_ignore_ascii_case(secret))
                {
                    *value = Value::String("<redacted>".to_owned());
                } else {
                    redact_json_value(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_json_value(value);
            }
        }
        _ => {}
    }
}

/// Build an [`ApiError`] from an HTTP 4xx/5xx response body.
fn build_api_error(status: StatusCode, headers: &HeaderMap, body: &[u8]) -> ApiError {
    let mut err: ApiError = serde_json::from_slice(body).unwrap_or_else(|_| {
        // Body wasn't the expected envelope; synthesize a minimal error.
        let mut e = ApiError {
            code: 0,
            message: format!(
                "failed to parse error response: unexpected server response code {}",
                status.as_u16()
            ),
            status: 0,
            more_info: String::new(),
            duration: String::new(),
            unrecoverable: false,
            exception_fields: Default::default(),
            retry_after: None,
        };
        e.message = if body.is_empty() {
            "empty response body".to_string()
        } else {
            e.message
        };
        e
    });

    if err.status == 0 {
        err.status = status.as_u16();
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        err.retry_after = parse_retry_after(headers);
    }
    err
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers
        .get("Retry-After")?
        .to_str()
        .ok()?
        .trim()
        .to_string();
    if let Ok(secs) = value.parse::<i64>() {
        if secs < 0 {
            return None;
        }
        return Some(Duration::from_secs(secs as u64));
    }
    None
}

/// Full-jitter helper: uniform in `[0, ceil_ms]` without a `rand` dependency.
fn jitter(ceil_ms: u64) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % (ceil_ms + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_redaction_is_recursive_and_case_insensitive() {
        let body = br#"{
            "token":"top-secret",
            "nested":{"password":"nested-secret"},
            "array":[{"API_SECRET":"array-secret"}],
            "safe":"visible"
        }"#;
        let redacted = redact_json_body(body);
        for secret in ["top-secret", "nested-secret", "array-secret"] {
            assert!(!redacted.contains(secret));
        }
        assert_eq!(redacted.matches("<redacted>").count(), 3);
        assert!(redacted.contains("visible"));
    }

    #[test]
    fn non_json_body_is_omitted_from_tracing() {
        let redacted = redact_json_body(b"token=must-not-leak");
        assert_eq!(redacted, "<non-JSON body omitted>");
        assert!(!redacted.contains("must-not-leak"));
    }

    #[test]
    fn compatibility_limits_are_conservative_and_configurable() {
        let limits = NetworkLimits::default();
        assert_eq!(limits.max_response_body_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.max_websocket_message_bytes, 4 * 1024 * 1024);
        let stricter = limits
            .with_max_response_body_bytes(1024)
            .with_max_websocket_message_bytes(2048);
        assert_eq!(stricter.max_response_body_bytes, 1024);
        assert_eq!(stricter.max_websocket_message_bytes, 2048);

        assert_eq!(checked_body_size(4, 4, 8).expect("at limit"), 8);
        assert!(matches!(
            checked_body_size(8, 1, 8).expect_err("oversized"),
            Error::ResponseTooLarge {
                limit: 8,
                actual: 9
            }
        ));
        assert!(matches!(
            checked_body_size(usize::MAX, 1, usize::MAX).expect_err("overflow"),
            Error::ResponseTooLarge {
                actual: usize::MAX,
                ..
            }
        ));
    }
}
