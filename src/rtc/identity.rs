//! SDK identity reported to the SFU and coordinator.
//!
//! HARD RULE (see AGENTS.md, "Never report `SDK_TYPE_GO` to the SFU"): the SFU
//! substitutes a restricted codec list for Go clients, so this SDK must never
//! present itself as [`SdkType::Go`]. There is no dedicated Rust `SdkType`
//! variant upstream, so we report [`SdkType::Unspecified`] and identify the
//! implementation through the `stream-rust-<version>` client header instead —
//! the same header the server-token REST client already sends.

use super::proto::models::{self, SdkType};

/// The client-type prefix reported to Stream services. Combined with the crate
/// version to form the full `stream-rust-<version>` identifier.
pub const CLIENT_TYPE: &str = "stream-rust";

/// SDK type sent to the SFU in `ClientDetails`.
///
/// Deliberately [`SdkType::Unspecified`], never [`SdkType::Go`]: reporting Go
/// makes the SFU send a substituted codec list intended for the Go SDK.
pub const SDK_TYPE: SdkType = SdkType::Unspecified;

/// The WebRTC implementation string reported in `SendStats.webrtc_version`.
///
/// The JS/Swift SDKs report a browser/native WebRTC version here; this SDK is
/// built on [webrtc-rs](https://github.com/webrtc-rs/webrtc), so we report its
/// crate version. Keep in sync with the `webrtc` dependency in `Cargo.toml`.
pub const WEBRTC_VERSION: &str = "webrtc-rs-0.17.2";

/// The SDK name reported in `SendStats.sdk` (e.g. `stream-rust`).
///
/// Mirrors JS `getSdkName` (`stream-js` / `stream-react` / …) but identifies
/// this implementation, matching the `X-Stream-Client` header prefix.
pub fn sdk_name() -> &'static str {
    CLIENT_TYPE
}

/// The SDK version reported in `SendStats.sdk_version` (the crate version).
pub fn sdk_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// The full client identifier, e.g. `stream-rust-0.1.0`.
///
/// Sent as the `X-Stream-Client` header on REST/Twirp requests and as the
/// coordinator WebSocket `X-Stream-Client` query param, matching the
/// server-token REST client and getstream-go's version header convention.
pub fn client_header() -> String {
    format!("{CLIENT_TYPE}-{}", env!("CARGO_PKG_VERSION"))
}

/// Build the `ClientDetails` announced to the SFU on the join request.
///
/// The `sdk` version fields carry the crate's semantic version so the server
/// can attribute traffic to this SDK even though the enum type is unspecified.
pub fn client_details() -> models::ClientDetails {
    models::ClientDetails {
        sdk: Some(models::Sdk {
            r#type: SDK_TYPE as i32,
            major: env!("CARGO_PKG_VERSION_MAJOR").to_owned(),
            minor: env!("CARGO_PKG_VERSION_MINOR").to_owned(),
            patch: env!("CARGO_PKG_VERSION_PATCH").to_owned(),
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_reports_go_sdk_type() {
        // Guards the AGENTS.md hard rule at the type level.
        assert_ne!(SDK_TYPE, SdkType::Go);
        assert_eq!(SDK_TYPE, SdkType::Unspecified);
    }

    #[test]
    fn client_header_is_stream_rust_versioned() {
        let header = client_header();
        assert!(header.starts_with("stream-rust-"));
        assert!(!header.contains("go"));
    }

    #[test]
    fn client_details_carry_non_go_sdk() {
        let details = client_details();
        let sdk = details.sdk.expect("sdk populated");
        assert_eq!(sdk.r#type, SdkType::Unspecified as i32);
        assert_ne!(sdk.r#type, SdkType::Go as i32);
    }
}
