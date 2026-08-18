#![doc = include_str!("../README.md")]

pub mod client;
pub mod error;
pub mod models;
pub mod token;
mod users;
pub mod video;
pub mod webhook;

use std::sync::Arc;

use client::{Client, ClientConfig, NetworkLimits};
pub use error::{ApiError, Error, Result, TokenError, WebhookError};
pub use token::{TokenClaims, TokenOptions};
pub use video::{Call, VideoClient};
pub use webhook::{WebhookEvent, parse_event, verify_signature};

/// Environment variable holding the Stream API key.
pub const ENV_API_KEY: &str = "STREAM_API_KEY";
/// Environment variable holding the Stream API secret.
pub const ENV_API_SECRET: &str = "STREAM_API_SECRET";

/// The server-side Stream client. Holds the API key + secret and mints the
/// server JWT used to authenticate REST calls.
///
/// Named after the Go/Python SDKs (`Stream::new` / `from_env`) — intentionally
/// **not** `StreamVideoClient` (that is the JS browser client). Cheap to clone.
#[derive(Clone)]
pub struct Stream {
    inner: Arc<Client>,
}

impl Stream {
    /// Construct a client from an API key and secret using default connection
    /// settings (max 5 conns/host, 55s idle, 30s request timeout).
    pub fn new(api_key: impl Into<String>, api_secret: impl Into<String>) -> Result<Self> {
        Self::with_config(api_key, api_secret, ClientConfig::default())
    }

    /// Construct a client with a custom [`ClientConfig`].
    pub fn with_config(
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        config: ClientConfig,
    ) -> Result<Self> {
        let client = Client::new(api_key.into(), api_secret.into(), config)?;
        Ok(Self {
            inner: Arc::new(client),
        })
    }

    /// Construct a client with custom connection settings and payload limits.
    pub fn with_config_and_limits(
        api_key: impl Into<String>,
        api_secret: impl Into<String>,
        config: ClientConfig,
        limits: NetworkLimits,
    ) -> Result<Self> {
        let client = Client::new_with_limits(api_key.into(), api_secret.into(), config, limits)?;
        Ok(Self {
            inner: Arc::new(client),
        })
    }

    /// Construct a client from `STREAM_API_KEY` / `STREAM_API_SECRET`.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var(ENV_API_KEY)
            .map_err(|_| Error::Config(format!("{ENV_API_KEY} is not set")))?;
        let api_secret = std::env::var(ENV_API_SECRET)
            .map_err(|_| Error::Config(format!("{ENV_API_SECRET} is not set")))?;
        Self::new(api_key, api_secret)
    }

    pub(crate) fn client(&self) -> &Client {
        &self.inner
    }

    /// The configured API key.
    pub fn api_key(&self) -> &str {
        self.inner.api_key()
    }

    /// Access the video coordinator endpoints.
    pub fn video(&self) -> VideoClient {
        VideoClient::new(self.inner.clone())
    }

    /// Mint a user token with no expiry or extra claims.
    pub fn create_token(&self, user_id: &str) -> Result<String> {
        token::create_user_token(self.inner.api_secret(), user_id, &TokenOptions::default())
    }

    /// Mint a user token with optional expiration and claims.
    ///
    /// Rust compatibility adapter with no direct `getstream-go` or
    /// `stream-video-js` equivalent.
    pub fn create_token_with(&self, user_id: &str, opts: TokenOptions) -> Result<String> {
        token::create_user_token(self.inner.api_secret(), user_id, &opts)
    }

    /// Verify a user token's HS256 signature and return its claims.
    ///
    /// This is claims inspection: it intentionally does not reject tokens based
    /// on `exp`, `nbf`, or `iat`. Operational RTC authentication separately
    /// applies a 60-second clock-skew allowance to all three temporal claims.
    pub fn decode_token(&self, token: &str) -> Result<TokenClaims> {
        token::decode_token(self.inner.api_secret(), token)
    }

    /// Verify the exact raw webhook body against its `X-Signature`
    /// (HMAC-SHA256, hex). This authenticates bytes but does not prevent replay.
    pub fn verify_webhook(&self, body: &[u8], signature: &str) -> bool {
        webhook::verify_signature(body, signature, self.inner.api_secret())
    }

    /// Verify a webhook signature and parse the payload into a typed event.
    ///
    /// Applications should deduplicate verified deliveries using the provider's
    /// delivery/event identifier when present, or a digest of the raw body.
    pub fn parse_webhook(&self, body: &[u8], signature: &str) -> Result<WebhookEvent> {
        if !self.verify_webhook(body, signature) {
            return Err(Error::Webhook(WebhookError::SignatureMismatch));
        }
        Ok(webhook::parse_event(body)?)
    }
}

// RTC (SFU WebRTC) — core to this SDK, always compiled in.
pub mod rtc;
