//! Error conversion at the Node-API boundary.

use getstream::rtc::RtcError;
use napi::{Error, Status};
use serde_json::{Value, json};

pub(crate) fn rtc_error(error: RtcError) -> Error {
    let (code, details) = match &error {
        RtcError::IllegalState(_) => ("RTC_ILLEGAL_STATE", json!({})),
        RtcError::PermissionDenied { capability } => {
            ("RTC_PERMISSION_DENIED", json!({ "capability": capability }))
        }
        RtcError::Join(join) => ("RTC_JOIN", json!({ "joinError": join.to_string() })),
        RtcError::Timeout(timeout) => ("RTC_TIMEOUT", json!({ "timeout": timeout.to_string() })),
        RtcError::Negotiation(negotiation) => (
            "RTC_NEGOTIATION",
            json!({ "negotiation": negotiation.to_string() }),
        ),
        RtcError::Transport(_)
        | RtcError::Twirp(_)
        | RtcError::Signal { .. }
        | RtcError::WebSocket(_)
        | RtcError::Coordinator(_)
        | RtcError::Api(_)
        | RtcError::WsConnection(_)
        | RtcError::Webrtc(_)
        | RtcError::Url(_) => ("RTC_CONNECTION", json!({})),
        RtcError::Media(_) => ("RTC_MEDIA", json!({})),
        RtcError::UnsupportedLayeredInput { input } => {
            ("RTC_UNSUPPORTED_LAYERING", json!({ "input": input }))
        }
        RtcError::UnsupportedVideoLayering { codec, track_type } => (
            "RTC_UNSUPPORTED_LAYERING",
            json!({ "codec": codec, "trackType": format!("{track_type:?}") }),
        ),
        RtcError::PcmQueueOverflow {
            dropped_samples,
            capacity_samples,
        } => (
            "RTC_QUEUE_OVERFLOW",
            json!({
                "droppedSamples": dropped_samples,
                "capacitySamples": capacity_samples,
            }),
        ),
        RtcError::SizeLimitExceeded {
            boundary,
            limit,
            actual,
        } => (
            "RTC_SIZE_LIMIT",
            json!({ "boundary": boundary, "limit": limit, "actual": actual }),
        ),
        RtcError::Closed(_) => ("RTC_CLOSED", json!({})),
        RtcError::Decode(_)
        | RtcError::Json(_)
        | RtcError::Token(_)
        | RtcError::TokenValidation(_) => ("RTC_UNKNOWN", json!({})),
        _ => ("RTC_UNKNOWN", json!({})),
    };

    encoded_error(code, error.to_string(), details)
}

pub(crate) fn sdk_error(error: getstream::Error) -> Error {
    encoded_error("RTC_CONNECTION", error.to_string(), json!({}))
}

pub(crate) fn invalid_argument(message: impl Into<String>) -> Error {
    encoded_error("RTC_MEDIA", message.into(), json!({}))
}

pub(crate) fn illegal_state(message: impl Into<String>, details: Value) -> Error {
    encoded_error("RTC_ILLEGAL_STATE", message.into(), details)
}

fn encoded_error(code: &str, message: String, details: Value) -> Error {
    let reason = json!({
        "code": code,
        "message": message,
        "details": details,
    })
    .to_string();
    Error::new(Status::GenericFailure, reason)
}
