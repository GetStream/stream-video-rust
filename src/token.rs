//! JWT minting and verification (HS256), matching getstream-go's token claims.
//!
//! Server tokens carry `{"server": true}`; user tokens carry `user_id`, `iat`,
//! and optional `exp` / `role` / `call_cids` plus arbitrary custom claims.
//! RTC authentication allows 60 seconds of clock skew when checking `exp`,
//! `nbf`, and future `iat`. Signature verification via
//! [`crate::Stream::decode_token`] intentionally remains claims inspection and
//! does not apply this temporal policy.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Sha256;

use crate::error::{Result, TokenError};

type HmacSha256 = Hmac<Sha256>;

const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_HEADER_SEGMENT_BYTES: usize = 512;
const MAX_PAYLOAD_SEGMENT_BYTES: usize = 15 * 1024;
const MAX_SIGNATURE_SEGMENT_BYTES: usize = 128;
const JWT_CLOCK_SKEW_SECONDS: i64 = 60;
const RESERVED_CLAIMS: [&str; 7] = [
    "user_id",
    "iat",
    "exp",
    "nbf",
    "server",
    "role",
    "call_cids",
];

/// Options for [`crate::Stream::create_token_with`].
///
/// Round-trippable: every field set here is recoverable via
/// [`crate::Stream::decode_token`].
#[derive(Debug, Clone, Default)]
pub struct TokenOptions {
    /// Token lifetime. When set, an `exp` claim is added (`iat + expiration`).
    pub expiration: Option<Duration>,
    /// Role claim for the user.
    pub role: Option<String>,
    /// Call CIDs the token grants access to (`call_cids` claim). Required for
    /// call tokens.
    pub call_cids: Option<Vec<String>>,
    /// Additional custom claims merged into the token payload.
    pub custom: Map<String, Value>,
}

/// Decoded user-token claims.
///
/// RTC authentication applies a 60-second clock-skew allowance consistently to
/// `exp`, `nbf`, and future `iat`. Decoding alone does not apply temporal
/// validation, so callers inspecting tokens can still observe expired claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    /// The user this token authenticates.
    pub user_id: String,
    /// Issued-at (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iat: Option<i64>,
    /// Expiry (unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>,
    /// Role claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Granted call CIDs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_cids: Option<Vec<String>>,
    /// Any remaining custom claims.
    #[serde(flatten)]
    pub custom: Map<String, Value>,
}

#[derive(Deserialize)]
struct OperationalTokenClaims {
    user_id: String,
    #[serde(default)]
    iat: Option<i64>,
    #[serde(default)]
    exp: Option<i64>,
    #[serde(default)]
    nbf: Option<i64>,
    #[serde(default)]
    server: Option<bool>,
    #[serde(default, rename = "role")]
    _role: Option<String>,
    #[serde(default, rename = "call_cids")]
    _call_cids: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtectedHeader {
    alg: String,
    #[serde(default)]
    typ: Option<String>,
}

struct JwtSegments<'a> {
    header: &'a str,
    payload: &'a str,
    signature: &'a str,
}

fn now_unix() -> Result<i64> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TokenError::TimestampOverflow)?
        .as_secs();
    i64::try_from(seconds)
        .map_err(|_| TokenError::TimestampOverflow)
        .map_err(Into::into)
}

fn sign(secret: &[u8], signing_input: &str) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|e| TokenError::InvalidClaims(format!("invalid HMAC key: {e}")))?;
    mac.update(signing_input.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn encode_jwt(secret: &[u8], claims: &Value) -> Result<String> {
    let header = serde_json::json!({ "alg": "HS256", "typ": "JWT" });
    let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);
    let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?);
    let signing_input = format!("{header_b64}.{payload_b64}");
    let sig = sign(secret, &signing_input)?;
    let token = format!("{signing_input}.{sig}");
    parse_segments(&token)?;
    Ok(token)
}

/// Mint the server token (`{"server": true}`) used to authenticate API calls.
pub(crate) fn create_server_token(secret: &[u8]) -> Result<String> {
    encode_jwt(secret, &serde_json::json!({ "server": true }))
}

/// Mint a user token with optional claims.
///
/// Backs [`crate::Stream::create_token_with`], a Rust compatibility adapter with
/// no direct `getstream-go` or `stream-video-js` equivalent.
pub(crate) fn create_user_token(
    secret: &[u8],
    user_id: &str,
    opts: &TokenOptions,
) -> Result<String> {
    create_user_token_at(secret, user_id, opts, now_unix()?)
}

fn create_user_token_at(
    secret: &[u8],
    user_id: &str,
    opts: &TokenOptions,
    now: i64,
) -> Result<String> {
    if user_id.is_empty() {
        return Err(TokenError::InvalidClaims("user ID is required".into()).into());
    }
    if let Some(claim) = opts
        .custom
        .keys()
        .find(|claim| RESERVED_CLAIMS.contains(&claim.as_str()))
    {
        return Err(TokenError::ReservedClaim {
            claim: claim.clone(),
        }
        .into());
    }

    let mut claims = Map::new();
    for (k, v) in &opts.custom {
        claims.insert(k.clone(), v.clone());
    }

    claims.insert("user_id".into(), Value::String(user_id.to_string()));
    claims.insert("iat".into(), Value::Number(now.into()));

    if let Some(exp) = opts.expiration {
        let secs = exp.as_secs();
        if secs > 0 {
            let secs = i64::try_from(secs).map_err(|_| TokenError::TimestampOverflow)?;
            let expires_at = now.checked_add(secs).ok_or(TokenError::TimestampOverflow)?;
            claims.insert("exp".into(), Value::Number(expires_at.into()));
        }
    }
    if let Some(role) = &opts.role
        && !role.is_empty()
    {
        claims.insert("role".into(), Value::String(role.clone()));
    }
    if let Some(cids) = &opts.call_cids
        && !cids.is_empty()
    {
        claims.insert(
            "call_cids".into(),
            Value::Array(cids.iter().cloned().map(Value::String).collect()),
        );
    }

    encode_jwt(secret, &Value::Object(claims))
}

/// Verify a token's HS256 signature against `secret` and return its claims.
pub(crate) fn decode_token(secret: &[u8], token: &str) -> Result<TokenClaims> {
    let segments = parse_segments(token)?;
    parse_header(segments.header)?;
    let signing_input = format!("{}.{}", segments.header, segments.payload);
    let expected =
        URL_SAFE_NO_PAD
            .decode(segments.signature)
            .map_err(|_| TokenError::InvalidEncoding {
                segment: "signature",
            })?;
    if expected.len() != 32 {
        return Err(TokenError::Malformed("HS256 signature must be 32 bytes").into());
    }

    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|e| TokenError::InvalidClaims(format!("invalid HMAC key: {e}")))?;
    mac.update(signing_input.as_bytes());
    mac.verify_slice(&expected)
        .map_err(|_| TokenError::SignatureMismatch)?;

    parse_claims(segments.payload)
}

fn inspect_token(token: &str) -> Result<OperationalTokenClaims> {
    let segments = parse_segments(token)?;
    parse_header(segments.header)?;
    parse_operational_claims(segments.payload)
}

pub(crate) fn validate_operational_token(token: &str, expected_user_id: &str) -> Result<()> {
    validate_operational_token_at(token, expected_user_id, now_unix()?)
}

fn validate_operational_token_at(token: &str, expected_user_id: &str, now: i64) -> Result<()> {
    let claims = inspect_token(token)?;
    if claims.user_id != expected_user_id {
        return Err(TokenError::UserMismatch {
            expected: expected_user_id.to_owned(),
            actual: claims.user_id,
        }
        .into());
    }
    if claims.server == Some(true) {
        return Err(TokenError::InvalidClaims(
            "server tokens cannot authenticate an RTC participant".into(),
        )
        .into());
    }
    if let Some(exp) = claims.exp {
        let earliest = now
            .checked_sub(JWT_CLOCK_SKEW_SECONDS)
            .ok_or(TokenError::TimestampOverflow)?;
        if exp < earliest {
            return Err(TokenError::Expired { exp, now }.into());
        }
    }
    if let Some(nbf) = claims.nbf {
        let latest = now
            .checked_add(JWT_CLOCK_SKEW_SECONDS)
            .ok_or(TokenError::TimestampOverflow)?;
        if nbf > latest {
            return Err(TokenError::NotYetValid { nbf, now }.into());
        }
    }
    if let Some(iat) = claims.iat {
        let latest = now
            .checked_add(JWT_CLOCK_SKEW_SECONDS)
            .ok_or(TokenError::TimestampOverflow)?;
        if iat > latest {
            return Err(TokenError::IssuedInFuture { iat, now }.into());
        }
    }
    Ok(())
}

fn parse_segments(token: &str) -> Result<JwtSegments<'_>> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(TokenError::TooLarge {
            limit: MAX_TOKEN_BYTES,
            actual: token.len(),
        }
        .into());
    }
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(TokenError::Malformed("expected exactly 3 segments").into());
    };
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return Err(TokenError::Malformed("segments must not be empty").into());
    }
    if header.len() > MAX_HEADER_SEGMENT_BYTES {
        return Err(TokenError::Malformed("protected header segment is too large").into());
    }
    if payload.len() > MAX_PAYLOAD_SEGMENT_BYTES {
        return Err(TokenError::Malformed("payload segment is too large").into());
    }
    if signature.len() > MAX_SIGNATURE_SEGMENT_BYTES {
        return Err(TokenError::Malformed("signature segment is too large").into());
    }
    Ok(JwtSegments {
        header,
        payload,
        signature,
    })
}

fn parse_header(encoded: &str) -> Result<ProtectedHeader> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| TokenError::InvalidEncoding {
            segment: "protected header",
        })?;
    let header: ProtectedHeader =
        serde_json::from_slice(&bytes).map_err(|e| TokenError::InvalidHeader(e.to_string()))?;
    if header.alg != "HS256" {
        return Err(TokenError::UnsupportedAlgorithm { actual: header.alg }.into());
    }
    if let Some(typ) = &header.typ
        && !typ.eq_ignore_ascii_case("JWT")
    {
        return Err(TokenError::IncompatibleType {
            actual: typ.clone(),
        }
        .into());
    }
    Ok(header)
}

fn parse_claims(encoded: &str) -> Result<TokenClaims> {
    let payload = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| TokenError::InvalidEncoding { segment: "payload" })?;
    serde_json::from_slice(&payload)
        .map_err(|e| TokenError::InvalidClaims(e.to_string()))
        .map_err(Into::into)
}

fn parse_operational_claims(encoded: &str) -> Result<OperationalTokenClaims> {
    let payload = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| TokenError::InvalidEncoding { segment: "payload" })?;
    serde_json::from_slice(&payload)
        .map_err(|e| TokenError::InvalidClaims(e.to_string()))
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    const SECRET: &[u8] = b"boundary-test-secret";

    fn token_with(header: Value, claims: Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).expect("header JSON"));
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("claims JSON"));
        let input = format!("{header}.{payload}");
        let signature = sign(SECRET, &input).expect("signature");
        format!("{input}.{signature}")
    }

    fn token_error(err: Error) -> TokenError {
        match err {
            Error::TokenValidation(err) => err,
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn verified_decode_does_not_apply_temporal_policy() {
        let token = token_with(
            serde_json::json!({"alg": "HS256", "typ": "JWT"}),
            serde_json::json!({
                "user_id": "user",
                "iat": 10,
                "exp": 20,
                "nbf": 5,
                "server": false
            }),
        );
        let claims = decode_token(SECRET, &token).expect("verified claims");
        assert_eq!(claims.exp, Some(20));
        assert_eq!(claims.custom.get("nbf"), Some(&Value::from(5)));
        assert_eq!(claims.custom.get("server"), Some(&Value::from(false)));
    }

    #[test]
    fn minted_user_claims_round_trip() {
        let mut custom = Map::new();
        custom.insert("tenant".to_owned(), Value::String("acme".to_owned()));
        let options = TokenOptions {
            expiration: Some(Duration::from_secs(3_600)),
            role: Some("admin".to_owned()),
            call_cids: Some(vec!["default:call".to_owned()]),
            custom,
        };

        let token = create_user_token_at(SECRET, "user", &options, 100)
            .expect("mint user token with optional claims");
        let claims = decode_token(SECRET, &token).expect("verify minted user token");

        assert_eq!(claims.user_id, "user");
        assert_eq!(claims.iat, Some(100));
        assert_eq!(claims.exp, Some(3_700));
        assert_eq!(claims.role.as_deref(), Some("admin"));
        assert_eq!(claims.call_cids, Some(vec!["default:call".to_owned()]));
        assert_eq!(
            claims.custom.get("tenant"),
            Some(&Value::String("acme".to_owned()))
        );
    }

    #[test]
    fn operational_validation_applies_clock_skew_boundaries() {
        let exp_at_allowance = token_with(
            serde_json::json!({"alg": "HS256"}),
            serde_json::json!({"user_id": "user", "exp": 40}),
        );
        validate_operational_token_at(&exp_at_allowance, "user", 100)
            .expect("exp at clock-skew allowance");

        let exp_beyond_allowance = token_with(
            serde_json::json!({"alg": "HS256"}),
            serde_json::json!({"user_id": "user", "exp": 39}),
        );
        assert!(matches!(
            token_error(
                validate_operational_token_at(&exp_beyond_allowance, "user", 100)
                    .expect_err("exp beyond clock-skew allowance")
            ),
            TokenError::Expired { exp: 39, now: 100 }
        ));

        let nbf_at_allowance = token_with(
            serde_json::json!({"alg": "HS256"}),
            serde_json::json!({"user_id": "user", "nbf": 160}),
        );
        validate_operational_token_at(&nbf_at_allowance, "user", 100)
            .expect("nbf at clock-skew allowance");

        let nbf_beyond_allowance = token_with(
            serde_json::json!({"alg": "HS256"}),
            serde_json::json!({"user_id": "user", "nbf": 161}),
        );
        assert!(matches!(
            token_error(
                validate_operational_token_at(&nbf_beyond_allowance, "user", 100)
                    .expect_err("nbf beyond clock-skew allowance")
            ),
            TokenError::NotYetValid { nbf: 161, now: 100 }
        ));

        let iat_at_allowance = token_with(
            serde_json::json!({"alg": "HS256"}),
            serde_json::json!({"user_id": "user", "iat": 160}),
        );
        validate_operational_token_at(&iat_at_allowance, "user", 100)
            .expect("iat at clock-skew allowance");

        let iat_beyond_allowance = token_with(
            serde_json::json!({"alg": "HS256"}),
            serde_json::json!({"user_id": "user", "iat": 161}),
        );
        assert!(matches!(
            token_error(
                validate_operational_token_at(&iat_beyond_allowance, "user", 100)
                    .expect_err("iat beyond clock-skew allowance")
            ),
            TokenError::IssuedInFuture { iat: 161, now: 100 }
        ));
    }

    #[test]
    fn operational_validation_rejects_wrong_user_and_server_tokens() {
        let wrong_user = token_with(
            serde_json::json!({"alg": "HS256"}),
            serde_json::json!({"user_id": "user-a"}),
        );
        assert!(matches!(
            token_error(
                validate_operational_token_at(&wrong_user, "user-b", 100)
                    .expect_err("token user must match the RTC participant")
            ),
            TokenError::UserMismatch { expected, actual }
                if expected == "user-b" && actual == "user-a"
        ));

        let server = token_with(
            serde_json::json!({"alg": "HS256"}),
            serde_json::json!({"user_id": "user", "server": true}),
        );
        assert!(matches!(
            token_error(
                validate_operational_token_at(&server, "user", 100)
                    .expect_err("server token must not authenticate an RTC participant")
            ),
            TokenError::InvalidClaims(message)
                if message == "server tokens cannot authenticate an RTC participant"
        ));
    }

    #[test]
    fn strict_parser_rejects_malformed_oversized_tampered_and_wrong_alg() {
        assert!(matches!(
            token_error(decode_token(SECRET, "one.two").expect_err("segments")),
            TokenError::Malformed(_)
        ));

        let oversized = "x".repeat(MAX_TOKEN_BYTES + 1);
        assert!(matches!(
            token_error(decode_token(SECRET, &oversized).expect_err("oversized")),
            TokenError::TooLarge { .. }
        ));

        let valid = token_with(
            serde_json::json!({"alg": "HS256", "typ": "JWT"}),
            serde_json::json!({"user_id": "user"}),
        );
        let mut tampered = valid.into_bytes();
        let last = tampered.last_mut().expect("signature byte");
        *last = if *last == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).expect("ASCII JWT");
        assert!(matches!(
            token_error(decode_token(SECRET, &tampered).expect_err("tampered")),
            TokenError::SignatureMismatch
        ));

        let wrong_alg = token_with(
            serde_json::json!({"alg": "none", "typ": "JWT"}),
            serde_json::json!({"user_id": "user"}),
        );
        assert!(matches!(
            token_error(decode_token(SECRET, &wrong_alg).expect_err("wrong alg")),
            TokenError::UnsupportedAlgorithm { .. }
        ));
    }

    #[test]
    fn reserved_custom_claims_and_timestamp_overflow_are_rejected() {
        for claim in RESERVED_CLAIMS {
            let mut options = TokenOptions::default();
            options.custom.insert(claim.to_owned(), Value::Null);
            assert!(matches!(
                token_error(
                    create_user_token_at(SECRET, "user", &options, 1).expect_err("reserved claim")
                ),
                TokenError::ReservedClaim { .. }
            ));
        }

        let options = TokenOptions {
            expiration: Some(Duration::from_secs(1)),
            ..Default::default()
        };
        assert!(matches!(
            token_error(
                create_user_token_at(SECRET, "user", &options, i64::MAX)
                    .expect_err("timestamp overflow")
            ),
            TokenError::TimestampOverflow
        ));
    }

    #[test]
    fn minting_rejects_tokens_beyond_parser_limits() {
        let mut options = TokenOptions::default();
        options.custom.insert(
            "oversized".to_owned(),
            Value::String("x".repeat(MAX_TOKEN_BYTES)),
        );

        assert!(matches!(
            token_error(
                create_user_token_at(SECRET, "user", &options, 1)
                    .expect_err("oversized minted token")
            ),
            TokenError::TooLarge { .. }
        ));
    }

    #[test]
    fn compatible_typ_is_optional_or_jwt_only() {
        let absent = token_with(
            serde_json::json!({"alg": "HS256"}),
            serde_json::json!({"user_id": "user"}),
        );
        decode_token(SECRET, &absent).expect("typ may be absent");

        let lowercase = token_with(
            serde_json::json!({"alg": "HS256", "typ": "jwt"}),
            serde_json::json!({"user_id": "user"}),
        );
        decode_token(SECRET, &lowercase).expect("typ is case insensitive");

        let incompatible = token_with(
            serde_json::json!({"alg": "HS256", "typ": "JWE"}),
            serde_json::json!({"user_id": "user"}),
        );
        assert!(matches!(
            token_error(decode_token(SECRET, &incompatible).expect_err("incompatible typ")),
            TokenError::IncompatibleType { .. }
        ));
    }
}
