//! Webhook signature verification (HMAC-SHA256) and typed event parsing.
//!
//! Stream signs each webhook body with HMAC-SHA256 (hex-encoded) using the app
//! secret, delivered in the `X-Signature` header. Verify before trusting a
//! payload, then parse into a [`WebhookEvent`].
//!
//! Signature verification authenticates the exact bytes but does not prevent a
//! valid delivery from being replayed. Applications should retain Stream's
//! delivery/event identifier when present, or a digest of the verified body,
//! for an application-appropriate deduplication window.

use std::io::Read;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use flate2::read::GzDecoder;
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;

use crate::error::WebhookError;
use crate::models::{CallResponse, MemberResponse, UserResponse};

type HmacSha256 = Hmac<Sha256>;

/// Compute the lowercase hex HMAC-SHA256 of `body` under `secret`.
fn hex_hmac(secret: &[u8], body: &[u8]) -> Option<String> {
    let mut mac = HmacSha256::new_from_slice(secret).ok()?;
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    Some(out)
}

/// Constant-time byte comparison to avoid timing side channels.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Verify a webhook `signature` (hex HMAC-SHA256) against the exact raw `body`
/// using `secret`. Comparison is constant-time for valid signature lengths.
///
/// This authenticates the payload but does not provide replay protection.
pub fn verify_signature(body: &[u8], signature: &str, secret: &[u8]) -> bool {
    match hex_hmac(secret, body) {
        Some(expected) => constant_time_eq(expected.as_bytes(), signature.as_bytes()),
        None => false,
    }
}

/// Maximum size a gzip webhook payload may decompress to.
///
/// Bounds decompression so a malicious delivery cannot expand a small gzip body
/// into an unbounded allocation (a decompression bomb). Mirrors the coordinator
/// HTTP ceiling ([`crate::client::DEFAULT_MAX_RESPONSE_BODY_BYTES`]).
const MAX_DECOMPRESSED_BYTES: usize = 16 * 1024 * 1024;

/// Decompress a gzip-prefixed payload, or return an uncompressed payload unchanged.
pub fn gunzip_payload(body: &[u8]) -> std::result::Result<Vec<u8>, WebhookError> {
    if !body.starts_with(&[0x1f, 0x8b]) {
        return Ok(body.to_vec());
    }

    // Read one byte past the ceiling so an over-limit payload is detectable
    // instead of silently truncated.
    let read_limit = MAX_DECOMPRESSED_BYTES.saturating_add(1) as u64;
    let mut payload = Vec::new();
    GzDecoder::new(body)
        .take(read_limit)
        .read_to_end(&mut payload)
        .map_err(|error| {
            WebhookError::InvalidPayload(format!("gzip decompression failed: {error}"))
        })?;
    if payload.len() > MAX_DECOMPRESSED_BYTES {
        return Err(WebhookError::PayloadTooLarge {
            limit: MAX_DECOMPRESSED_BYTES,
        });
    }
    Ok(payload)
}

/// Decode an SQS message body, accepting raw JSON or base64-encoded gzip.
pub fn decode_sqs_payload(message_body: &str) -> std::result::Result<Vec<u8>, WebhookError> {
    let decoded = STANDARD
        .decode(message_body)
        .unwrap_or_else(|_| message_body.as_bytes().to_vec());
    gunzip_payload(&decoded)
}

/// Decode an SNS notification envelope or a pre-extracted `Message` value.
pub fn decode_sns_payload(notification_body: &str) -> std::result::Result<Vec<u8>, WebhookError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct SnsEnvelope {
        message: String,
    }

    let message = serde_json::from_str::<SnsEnvelope>(notification_body)
        .ok()
        .map(|envelope| envelope.message)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| notification_body.to_owned());
    decode_sqs_payload(&message)
}

/// Decode and parse an SQS webhook message body.
pub fn parse_sqs(message_body: &str) -> std::result::Result<WebhookEvent, WebhookError> {
    parse_event(&decode_sqs_payload(message_body)?)
}

/// Decode and parse an SNS webhook notification.
pub fn parse_sns(notification_body: &str) -> std::result::Result<WebhookEvent, WebhookError> {
    parse_event(&decode_sns_payload(notification_body)?)
}

/// Common fields shared by call-scoped webhook events. Event-specific fields are
/// preserved in `extra`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CallEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub created_at: Option<String>,
    pub call_cid: Option<String>,
    pub call: Option<CallResponse>,
    pub user: Option<UserResponse>,
    pub members: Option<Vec<MemberResponse>>,
    pub session_id: Option<String>,
    /// Any additional event-specific fields.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// A parsed webhook event. Known video/call events are typed; anything else is
/// [`WebhookEvent::Unknown`] with the raw payload preserved (forward-compat).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum WebhookEvent {
    CallCreated(CallEvent),
    CallUpdated(CallEvent),
    CallEnded(CallEvent),
    CallDeleted(CallEvent),
    CallSessionStarted(CallEvent),
    CallSessionEnded(CallEvent),
    CallSessionParticipantJoined(CallEvent),
    CallSessionParticipantLeft(CallEvent),
    CallSessionParticipantCountUpdated(CallEvent),
    CallRecordingStarted(CallEvent),
    CallRecordingStopped(CallEvent),
    CallRecordingReady(CallEvent),
    CallRecordingFailed(CallEvent),
    CallTranscriptionStarted(CallEvent),
    CallTranscriptionStopped(CallEvent),
    CallTranscriptionReady(CallEvent),
    CallTranscriptionFailed(CallEvent),
    CallClosedCaption(CallEvent),
    CallClosedCaptionsStarted(CallEvent),
    CallClosedCaptionsStopped(CallEvent),
    CallClosedCaptionsFailed(CallEvent),
    CallFrameRecordingStarted(CallEvent),
    CallFrameRecordingStopped(CallEvent),
    CallFrameRecordingReady(CallEvent),
    CallFrameRecordingFailed(CallEvent),
    CallLiveStarted(CallEvent),
    CallHlsBroadcastingFailed(CallEvent),
    CallHlsBroadcastingStarted(CallEvent),
    CallHlsBroadcastingStopped(CallEvent),
    CallRtmpBroadcastFailed(CallEvent),
    CallRtmpBroadcastStarted(CallEvent),
    CallRtmpBroadcastStopped(CallEvent),
    CallRing(CallEvent),
    CallMissed(CallEvent),
    CallNotification(CallEvent),
    CallAccepted(CallEvent),
    CallRejected(CallEvent),
    CallKickedUser(CallEvent),
    CallMemberAdded(CallEvent),
    CallMemberRemoved(CallEvent),
    CallMemberUpdated(CallEvent),
    CallMemberUpdatedPermission(CallEvent),
    CallPermissionRequest(CallEvent),
    CallPermissionsUpdated(CallEvent),
    CallReactionNew(CallEvent),
    CallBlockedUser(CallEvent),
    CallUnblockedUser(CallEvent),
    CallUserMuted(CallEvent),
    CallDtmf(CallEvent),
    CallModerationBlur(CallEvent),
    CallModerationWarning(CallEvent),
    CallStatsReportReady(CallEvent),
    CallUserFeedbackSubmitted(CallEvent),
    Custom(CallEvent),
    /// A well-formed event whose `type` is not in the typed set.
    Unknown {
        event_type: String,
        raw: Value,
    },
}

impl WebhookEvent {
    /// The event's `type` discriminator.
    pub fn event_type(&self) -> &str {
        match self {
            WebhookEvent::Unknown { event_type, .. } => event_type,
            WebhookEvent::CallCreated(e)
            | WebhookEvent::CallUpdated(e)
            | WebhookEvent::CallEnded(e)
            | WebhookEvent::CallDeleted(e)
            | WebhookEvent::CallSessionStarted(e)
            | WebhookEvent::CallSessionEnded(e)
            | WebhookEvent::CallSessionParticipantJoined(e)
            | WebhookEvent::CallSessionParticipantLeft(e)
            | WebhookEvent::CallSessionParticipantCountUpdated(e)
            | WebhookEvent::CallRecordingStarted(e)
            | WebhookEvent::CallRecordingStopped(e)
            | WebhookEvent::CallRecordingReady(e)
            | WebhookEvent::CallRecordingFailed(e)
            | WebhookEvent::CallTranscriptionStarted(e)
            | WebhookEvent::CallTranscriptionStopped(e)
            | WebhookEvent::CallTranscriptionReady(e)
            | WebhookEvent::CallTranscriptionFailed(e)
            | WebhookEvent::CallClosedCaption(e)
            | WebhookEvent::CallClosedCaptionsStarted(e)
            | WebhookEvent::CallClosedCaptionsStopped(e)
            | WebhookEvent::CallClosedCaptionsFailed(e)
            | WebhookEvent::CallFrameRecordingStarted(e)
            | WebhookEvent::CallFrameRecordingStopped(e)
            | WebhookEvent::CallFrameRecordingReady(e)
            | WebhookEvent::CallFrameRecordingFailed(e)
            | WebhookEvent::CallLiveStarted(e)
            | WebhookEvent::CallHlsBroadcastingFailed(e)
            | WebhookEvent::CallHlsBroadcastingStarted(e)
            | WebhookEvent::CallHlsBroadcastingStopped(e)
            | WebhookEvent::CallRtmpBroadcastFailed(e)
            | WebhookEvent::CallRtmpBroadcastStarted(e)
            | WebhookEvent::CallRtmpBroadcastStopped(e)
            | WebhookEvent::CallRing(e)
            | WebhookEvent::CallMissed(e)
            | WebhookEvent::CallNotification(e)
            | WebhookEvent::CallAccepted(e)
            | WebhookEvent::CallRejected(e)
            | WebhookEvent::CallKickedUser(e)
            | WebhookEvent::CallMemberAdded(e)
            | WebhookEvent::CallMemberRemoved(e)
            | WebhookEvent::CallMemberUpdated(e)
            | WebhookEvent::CallMemberUpdatedPermission(e)
            | WebhookEvent::CallPermissionRequest(e)
            | WebhookEvent::CallPermissionsUpdated(e)
            | WebhookEvent::CallReactionNew(e)
            | WebhookEvent::CallBlockedUser(e)
            | WebhookEvent::CallUnblockedUser(e)
            | WebhookEvent::CallUserMuted(e)
            | WebhookEvent::CallDtmf(e)
            | WebhookEvent::CallModerationBlur(e)
            | WebhookEvent::CallModerationWarning(e)
            | WebhookEvent::CallStatsReportReady(e)
            | WebhookEvent::CallUserFeedbackSubmitted(e)
            | WebhookEvent::Custom(e) => &e.event_type,
        }
    }
}

/// Parse a webhook payload into a typed [`WebhookEvent`] without verifying it.
///
/// Prefer [`crate::Stream::parse_webhook`], which verifies the exact raw bytes
/// before parsing. This helper is intended for payloads already authenticated
/// at another boundary.
pub fn parse_event(body: &[u8]) -> std::result::Result<WebhookEvent, WebhookError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| WebhookError::InvalidPayload(e.to_string()))?;
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(WebhookError::MissingType)?
        .to_string();

    let call_event = || -> std::result::Result<CallEvent, WebhookError> {
        serde_json::from_value(value.clone())
            .map_err(|e| WebhookError::InvalidPayload(e.to_string()))
    };

    let event = match event_type.as_str() {
        "call.created" => WebhookEvent::CallCreated(call_event()?),
        "call.updated" => WebhookEvent::CallUpdated(call_event()?),
        "call.ended" => WebhookEvent::CallEnded(call_event()?),
        "call.deleted" => WebhookEvent::CallDeleted(call_event()?),
        "call.session_started" => WebhookEvent::CallSessionStarted(call_event()?),
        "call.session_ended" => WebhookEvent::CallSessionEnded(call_event()?),
        "call.session_participant_joined" => {
            WebhookEvent::CallSessionParticipantJoined(call_event()?)
        }
        "call.session_participant_left" => WebhookEvent::CallSessionParticipantLeft(call_event()?),
        "call.session_participant_count_updated" => {
            WebhookEvent::CallSessionParticipantCountUpdated(call_event()?)
        }
        "call.recording_started" => WebhookEvent::CallRecordingStarted(call_event()?),
        "call.recording_stopped" => WebhookEvent::CallRecordingStopped(call_event()?),
        "call.recording_ready" => WebhookEvent::CallRecordingReady(call_event()?),
        "call.recording_failed" => WebhookEvent::CallRecordingFailed(call_event()?),
        "call.transcription_started" => WebhookEvent::CallTranscriptionStarted(call_event()?),
        "call.transcription_stopped" => WebhookEvent::CallTranscriptionStopped(call_event()?),
        "call.transcription_ready" => WebhookEvent::CallTranscriptionReady(call_event()?),
        "call.transcription_failed" => WebhookEvent::CallTranscriptionFailed(call_event()?),
        "call.closed_caption" => WebhookEvent::CallClosedCaption(call_event()?),
        "call.closed_captions_started" => WebhookEvent::CallClosedCaptionsStarted(call_event()?),
        "call.closed_captions_stopped" => WebhookEvent::CallClosedCaptionsStopped(call_event()?),
        "call.closed_captions_failed" => WebhookEvent::CallClosedCaptionsFailed(call_event()?),
        "call.frame_recording_started" => WebhookEvent::CallFrameRecordingStarted(call_event()?),
        "call.frame_recording_stopped" => WebhookEvent::CallFrameRecordingStopped(call_event()?),
        "call.frame_recording_ready" => WebhookEvent::CallFrameRecordingReady(call_event()?),
        "call.frame_recording_failed" => WebhookEvent::CallFrameRecordingFailed(call_event()?),
        "call.live_started" => WebhookEvent::CallLiveStarted(call_event()?),
        "call.hls_broadcasting_failed" => WebhookEvent::CallHlsBroadcastingFailed(call_event()?),
        "call.hls_broadcasting_started" => WebhookEvent::CallHlsBroadcastingStarted(call_event()?),
        "call.hls_broadcasting_stopped" => WebhookEvent::CallHlsBroadcastingStopped(call_event()?),
        "call.rtmp_broadcast_failed" => WebhookEvent::CallRtmpBroadcastFailed(call_event()?),
        "call.rtmp_broadcast_started" => WebhookEvent::CallRtmpBroadcastStarted(call_event()?),
        "call.rtmp_broadcast_stopped" => WebhookEvent::CallRtmpBroadcastStopped(call_event()?),
        "call.ring" => WebhookEvent::CallRing(call_event()?),
        "call.missed" => WebhookEvent::CallMissed(call_event()?),
        "call.notification" => WebhookEvent::CallNotification(call_event()?),
        "call.accepted" => WebhookEvent::CallAccepted(call_event()?),
        "call.rejected" => WebhookEvent::CallRejected(call_event()?),
        "call.kicked_user" => WebhookEvent::CallKickedUser(call_event()?),
        "call.member_added" => WebhookEvent::CallMemberAdded(call_event()?),
        "call.member_removed" => WebhookEvent::CallMemberRemoved(call_event()?),
        "call.member_updated" => WebhookEvent::CallMemberUpdated(call_event()?),
        "call.member_updated_permission" => {
            WebhookEvent::CallMemberUpdatedPermission(call_event()?)
        }
        "call.permission_request" => WebhookEvent::CallPermissionRequest(call_event()?),
        "call.permissions_updated" => WebhookEvent::CallPermissionsUpdated(call_event()?),
        "call.reaction_new" => WebhookEvent::CallReactionNew(call_event()?),
        "call.blocked_user" => WebhookEvent::CallBlockedUser(call_event()?),
        "call.unblocked_user" => WebhookEvent::CallUnblockedUser(call_event()?),
        "call.user_muted" => WebhookEvent::CallUserMuted(call_event()?),
        "call.dtmf" => WebhookEvent::CallDtmf(call_event()?),
        "call.moderation_blur" => WebhookEvent::CallModerationBlur(call_event()?),
        "call.moderation_warning" => WebhookEvent::CallModerationWarning(call_event()?),
        "call.stats_report_ready" => WebhookEvent::CallStatsReportReady(call_event()?),
        "call.user_feedback_submitted" => WebhookEvent::CallUserFeedbackSubmitted(call_event()?),
        "custom" => WebhookEvent::Custom(call_event()?),
        _ => WebhookEvent::Unknown {
            event_type,
            raw: value,
        },
    };
    Ok(event)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use serde_json::json;

    use super::*;

    const TEST_SECRET: &[u8] = b"webhook-test-secret";

    fn sign(body: &[u8]) -> String {
        hex_hmac(TEST_SECRET, body).expect("fixed test secret is a valid HMAC key")
    }

    #[test]
    fn verifies_signature_and_parses_typed_event() {
        let body = br#"{"type":"call.created","call_cid":"default:abc","created_at":"2024-01-01T00:00:00Z"}"#;
        let signature = sign(body);

        assert!(verify_signature(body, &signature, TEST_SECRET));
        assert!(!verify_signature(body, "deadbeef", TEST_SECRET));

        let event = parse_event(body).expect("parse signed webhook body");
        assert_eq!(event.event_type(), "call.created");
        match event {
            WebhookEvent::CallCreated(event) => {
                assert_eq!(event.call_cid.as_deref(), Some("default:abc"));
            }
            other => panic!("expected CallCreated, got {:?}", other.event_type()),
        }
    }

    #[test]
    fn parses_raw_sqs_payload() {
        let event = parse_sqs(r#"{"type":"call.created"}"#).expect("raw SQS payload should parse");
        assert_eq!(event.event_type(), "call.created");
    }

    #[test]
    fn parses_compressed_sqs_payload() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(br#"{"type":"call.updated"}"#)
            .expect("test payload should compress");
        let encoded = STANDARD.encode(encoder.finish().expect("gzip stream should finish"));

        let event = parse_sqs(&encoded).expect("compressed SQS payload should parse");
        assert_eq!(event.event_type(), "call.updated");
    }

    #[test]
    fn parses_sns_envelope() {
        let encoded = STANDARD.encode(br#"{"type":"call.ended"}"#);
        let envelope = json!({
            "Type": "Notification",
            "Message": encoded,
        })
        .to_string();

        let event = parse_sns(&envelope).expect("SNS envelope should parse");
        assert_eq!(event.event_type(), "call.ended");
    }

    #[test]
    fn rejects_invalid_gzip_payload() {
        let error = gunzip_payload(&[0x1f, 0x8b, 0x00])
            .expect_err("invalid gzip payload should return an error");
        assert!(error.to_string().contains("gzip decompression failed"));
    }

    #[test]
    fn rejects_gzip_payload_exceeding_decompressed_ceiling() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&vec![b'a'; MAX_DECOMPRESSED_BYTES + 1])
            .expect("oversized payload should compress");
        let compressed = encoder.finish().expect("gzip stream should finish");

        let error = gunzip_payload(&compressed).expect_err("over-limit payload should be rejected");
        assert!(matches!(
            error,
            WebhookError::PayloadTooLarge { limit } if limit == MAX_DECOMPRESSED_BYTES
        ));
    }

    #[test]
    fn accepts_gzip_payload_within_decompressed_ceiling() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(br#"{"type":"call.created"}"#)
            .expect("under-limit payload should compress");
        let compressed = encoder.finish().expect("gzip stream should finish");

        let payload = gunzip_payload(&compressed).expect("under-limit payload should decompress");
        let event = parse_event(&payload).expect("decompressed payload should parse");
        assert_eq!(event.event_type(), "call.created");
    }
}
