//! Typed errors for the Stream SDK.
//!
//! All fallible SDK calls return [`Result<T>`]. API failures surface as
//! [`Error::Api`] carrying an [`ApiError`] (the coordinator's
//! `APIErrorResponse` envelope), mirroring the `ErrorFromResponse` shape used
//! by the JS/Go SDKs: `code`, HTTP `status`, and the `unrecoverable` flag that
//! gates retries.

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

/// Convenience alias for SDK results.
pub type Result<T> = std::result::Result<T, Error>;

/// Top-level SDK error.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The API returned an HTTP 4xx/5xx with an error envelope. Boxed to keep
    /// [`Error`] (and thus `Result`) small.
    #[error(transparent)]
    Api(Box<ApiError>),

    /// A network/transport failure prevented a response (connection reset,
    /// timeout, TLS, DNS). Never carries an HTTP status.
    #[error("transport error: {0}")]
    Transport(#[source] reqwest::Error),

    /// Failed to serialize a request body or deserialize a response body.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// The client was constructed with missing or invalid configuration
    /// (e.g. an empty API key/secret, or an unparseable base URL).
    #[error("configuration error: {0}")]
    Config(String),

    /// Token minting or verification failed.
    ///
    /// Retained for source compatibility. New typed token-boundary failures use
    /// [`Error::TokenValidation`].
    #[error("token error: {0}")]
    Token(String),

    /// Token minting, parsing, verification, or temporal validation failed.
    #[error(transparent)]
    TokenValidation(#[from] TokenError),

    /// An HTTP response exceeded the configured body limit.
    #[error("HTTP response body exceeded {limit} bytes (received at least {actual})")]
    ResponseTooLarge {
        /// Configured maximum response body size.
        limit: usize,
        /// Bytes observed, or the declared content length when it was larger.
        actual: usize,
    },

    /// An operation is not valid in the current call lifecycle state.
    #[error("illegal state: {0}")]
    IllegalState(String),

    /// Webhook verification or parsing failed.
    #[error(transparent)]
    Webhook(#[from] WebhookError),
}

/// JWT minting, parsing, verification, and operational-validation failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TokenError {
    /// The compact JWT exceeded the parser's fixed upper bound.
    #[error("JWT exceeds {limit} bytes (received {actual})")]
    TooLarge {
        /// Maximum accepted compact-token size.
        limit: usize,
        /// Actual compact-token size.
        actual: usize,
    },

    /// The compact serialization was not exactly three bounded segments.
    #[error("malformed JWT: {0}")]
    Malformed(&'static str),

    /// A base64url segment was invalid.
    #[error("invalid JWT {segment} encoding")]
    InvalidEncoding {
        /// Segment that failed to decode.
        segment: &'static str,
    },

    /// The protected header was not valid JSON or used unsupported fields.
    #[error("invalid JWT protected header: {0}")]
    InvalidHeader(String),

    /// Only HMAC-SHA256 tokens are accepted.
    #[error("unsupported JWT algorithm {actual:?}; expected \"HS256\"")]
    UnsupportedAlgorithm {
        /// Algorithm declared by the protected header.
        actual: String,
    },

    /// `typ`, when present, must identify a JWT.
    #[error("incompatible JWT type {actual:?}")]
    IncompatibleType {
        /// Type declared by the protected header.
        actual: String,
    },

    /// The HMAC did not authenticate the exact protected-header and payload segments.
    #[error("JWT signature verification failed")]
    SignatureMismatch,

    /// Reserved claims were absent or had incompatible JSON types.
    #[error("invalid JWT claims: {0}")]
    InvalidClaims(String),

    /// Custom claims cannot replace SDK-owned authentication claims.
    #[error("custom claims cannot replace reserved claim {claim:?}")]
    ReservedClaim {
        /// Reserved claim supplied in the custom map.
        claim: String,
    },

    /// A Unix timestamp or duration could not be represented safely.
    #[error("JWT timestamp arithmetic overflow")]
    TimestampOverflow,

    /// The token is expired for an authenticated operation.
    #[error("JWT expired at {exp} (current time {now})")]
    Expired {
        /// `exp` claim.
        exp: i64,
        /// Validation time.
        now: i64,
    },

    /// The token is not valid yet.
    #[error("JWT is not valid before {nbf} (current time {now})")]
    NotYetValid {
        /// `nbf` claim.
        nbf: i64,
        /// Validation time.
        now: i64,
    },

    /// The token's issuance time is implausibly far in the future.
    #[error("JWT was issued in the future at {iat} (current time {now})")]
    IssuedInFuture {
        /// `iat` claim.
        iat: i64,
        /// Validation time.
        now: i64,
    },

    /// A participant token belonged to a different user.
    #[error("JWT user_id {actual:?} does not match expected user {expected:?}")]
    UserMismatch {
        /// User expected by the operation.
        expected: String,
        /// User encoded in the token.
        actual: String,
    },

    /// Stream rejected the token as expired before local claims could establish an expiry.
    #[error("Stream rejected the JWT as expired")]
    ExpiredByServer,
}

impl From<ApiError> for Error {
    fn from(e: ApiError) -> Self {
        Error::Api(Box::new(e))
    }
}

impl Error {
    /// Returns the [`ApiError`] if this is an API-response error.
    pub fn as_api_error(&self) -> Option<&ApiError> {
        match self {
            Error::Api(e) => Some(e),
            _ => None,
        }
    }

    /// Whether the error is safe to retry. API errors marked `unrecoverable`
    /// are never retryable; transport errors and HTTP 429 are.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Transport(_) => true,
            Error::Api(e) => !e.unrecoverable && e.status == 429,
            _ => false,
        }
    }
}

/// The coordinator error envelope (`ErrorFromResponse`).
///
/// Field names track getstream-go's `StreamError`. `status` is always the HTTP
/// status code; `code` is Stream's application error code. `unrecoverable`, when
/// `true`, means the request must not be retried.
#[derive(Debug, Clone, Default, Deserialize, thiserror::Error)]
#[error("stream api error (code {code}, http {status}): {message}")]
#[non_exhaustive]
pub struct ApiError {
    /// Stream application-level error code.
    #[serde(default)]
    pub code: i32,

    /// Human-readable error message.
    #[serde(default)]
    pub message: String,

    /// HTTP status code. Deserialized from the `StatusCode` envelope field and
    /// otherwise set from the transport status.
    #[serde(rename = "StatusCode", default)]
    pub status: u16,

    /// Documentation link for the error, when provided.
    #[serde(default)]
    pub more_info: String,

    /// Server-reported request duration.
    #[serde(default)]
    pub duration: String,

    /// When `true`, the request that produced this error must not be retried.
    #[serde(default)]
    pub unrecoverable: bool,

    /// Field-level validation errors, keyed by field name.
    #[serde(default)]
    pub exception_fields: HashMap<String, String>,

    /// Parsed `Retry-After` header on HTTP 429. Not part of the JSON body.
    #[serde(skip)]
    pub retry_after: Option<Duration>,
}

impl ApiError {
    /// Whether this error was an HTTP 429 (rate limited).
    pub fn is_rate_limited(&self) -> bool {
        self.status == 429
    }
}

/// Webhook verification / parsing failures.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WebhookError {
    /// The provided HMAC-SHA256 signature did not match the body.
    #[error("webhook signature mismatch")]
    SignatureMismatch,

    /// The payload was not valid JSON.
    #[error("invalid webhook payload: {0}")]
    InvalidPayload(String),

    /// The payload was missing the `type` discriminator.
    #[error("webhook payload missing 'type' field")]
    MissingType,

    /// The gzip payload decompressed past the configured ceiling.
    #[error("webhook payload exceeded {limit} bytes after decompression")]
    PayloadTooLarge {
        /// Configured maximum decompressed payload size.
        limit: usize,
    },
}
