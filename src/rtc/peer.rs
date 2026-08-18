//! webrtc-rs PeerConnection construction and the throwaway generic SDPs.
//!
//! The participant path uses two PeerConnections (JS / videosdk): the publisher
//! is the offerer (SetPublisher over Twirp) and the subscriber is the answerer
//! (answers the SFU's `subscriber_offer` over the WS). This module builds them
//! with the SDK-supported codec + interceptor set and produces the "generic" SDPs the
//! SFU inspects to learn our codec capabilities on the `JoinRequest`
//! (JS `getGenericSdp`, stream-py `create_join_request`).

use std::sync::Arc;

use serde_json::json;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{
    MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9, MediaEngine,
};
use webrtc::api::{API, APIBuilder};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTCRtpHeaderExtensionCapability, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::sdp::extmap::{
    AUDIO_LEVEL_URI, SDES_MID_URI, SDES_REPAIR_RTP_STREAM_ID_URI, SDES_RTP_STREAM_ID_URI,
};

use super::coordinator::IceServer;
use super::error::Result;
use super::publish_options::H264_FMTP;
use super::tracer::Tracer;

const OPUS_PAYLOAD_TYPE: u8 = 111;
const VP8_PAYLOAD_TYPE: u8 = 96;
const VP9_PAYLOAD_TYPE: u8 = 98;
const H264_PAYLOAD_TYPE: u8 = 125;

/// Register exactly the codecs that the SDK can encode or decode.
///
/// `MediaEngine::register_default_codecs` also advertises legacy audio, VP9
/// profile 1, AV1, and HEVC. Negotiating any of those would deliver a track
/// that the decoded [`RemoteTrack`](super::RemoteTrack) APIs cannot consume.
fn register_supported_codecs(media_engine: &mut MediaEngine) -> Result<()> {
    media_engine.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: OPUS_PAYLOAD_TYPE,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;

    let video_feedback = vec![
        RTCPFeedback {
            typ: "goog-remb".to_owned(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "ccm".to_owned(),
            parameter: "fir".to_owned(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "pli".to_owned(),
        },
    ];
    for (mime_type, payload_type, fmtp) in [
        (MIME_TYPE_VP8, VP8_PAYLOAD_TYPE, ""),
        (MIME_TYPE_VP9, VP9_PAYLOAD_TYPE, "profile-id=0"),
        (MIME_TYPE_H264, H264_PAYLOAD_TYPE, H264_FMTP),
    ] {
        media_engine.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: mime_type.to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line: fmtp.to_owned(),
                    rtcp_feedback: video_feedback.clone(),
                },
                payload_type,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
    }

    Ok(())
}

/// Build a webrtc-rs [`API`] with the supported codecs and default interceptors
/// (NACK, RTCP reports, receiver-side TWCC).
///
/// Known gap versus Pion/videosdk: webrtc-rs ships no publisher-side congestion
/// controller (TWCC *sender* estimator / GCC) and no RTX/NACK retransmission
/// sender. Opus audio is unaffected — low bitrate, loss-tolerant, single layer —
/// but high-bitrate video publishing runs without bandwidth estimation or
/// retransmission. The default interceptor set below is wired as-is rather than
/// worked around.
fn build_api() -> Result<API> {
    let mut media_engine = MediaEngine::default();
    register_supported_codecs(&mut media_engine)?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;
    // RFC 6464 audio levels (videosdk `media_engine.go`). Registered *after* the
    // default interceptors so TWCC keeps its usual id and this takes the next
    // free one. Audio + send-only scopes it to our publisher's audio m-lines:
    // an answerer may not introduce an extension the offerer never offered, so
    // without this the SFU can never observe our level and no participant of
    // ours is ever marked speaking.
    media_engine.register_header_extension(
        RTCRtpHeaderExtensionCapability {
            uri: AUDIO_LEVEL_URI.to_owned(),
        },
        RTPCodecType::Audio,
        Some(RTCRtpTransceiverDirection::Sendonly),
    )?;
    // RID simulcast needs MID + RTP stream identifiers on each outbound packet.
    // webrtc-rs uses these registrations while binding `new_with_rid` tracks.
    for uri in [
        SDES_MID_URI,
        SDES_RTP_STREAM_ID_URI,
        SDES_REPAIR_RTP_STREAM_ID_URI,
    ] {
        media_engine.register_header_extension(
            RTCRtpHeaderExtensionCapability {
                uri: uri.to_owned(),
            },
            RTPCodecType::Video,
            Some(RTCRtpTransceiverDirection::Sendonly),
        )?;
    }
    Ok(APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build())
}

/// Map coordinator ICE servers to webrtc-rs [`RTCIceServer`]s.
pub fn to_rtc_ice_servers(servers: &[IceServer]) -> Vec<RTCIceServer> {
    servers
        .iter()
        .filter(|s| !s.urls.is_empty())
        .map(|s| RTCIceServer {
            urls: s.urls.clone(),
            username: s.username.clone(),
            credential: s.password.clone(),
        })
        .collect()
}

/// Create a fresh PeerConnection wired with the join credentials' ICE servers.
pub async fn new_peer_connection(ice: &[IceServer]) -> Result<Arc<RTCPeerConnection>> {
    let api = build_api()?;
    let config = RTCConfiguration {
        ice_servers: to_rtc_ice_servers(ice),
        ..Default::default()
    };
    let pc = api.new_peer_connection(config).await?;
    Ok(Arc::new(pc))
}

/// Wire the PeerConnection lifecycle events into `tracer`, mirroring JS
/// `traceRTCPeerConnection` tag names (`signalingstatechange`,
/// `icegatheringstatechange`, `iceconnectionstatechange`, `negotiationneeded`,
/// `datachannel`). The `onicecandidate` / `ontrack` / `connectionstatechange`
/// tags are emitted from the join module's existing handlers for those events
/// (each webrtc-rs `on_*` handler can be registered only once), so this covers
/// only the events that would otherwise have no handler.
pub fn trace_peer_events(pc: &Arc<RTCPeerConnection>, tracer: Arc<Tracer>) {
    let t = tracer.clone();
    pc.on_signaling_state_change(Box::new(move |state| {
        let t = t.clone();
        Box::pin(async move {
            t.trace("signalingstatechange", json!(state.to_string()));
        })
    }));

    let t = tracer.clone();
    pc.on_ice_gathering_state_change(Box::new(move |state| {
        let t = t.clone();
        Box::pin(async move {
            t.trace("icegatheringstatechange", json!(state.to_string()));
        })
    }));

    let t = tracer.clone();
    pc.on_ice_connection_state_change(Box::new(move |state| {
        let t = t.clone();
        Box::pin(async move {
            t.trace("iceconnectionstatechange", json!(state.to_string()));
        })
    }));

    let t = tracer.clone();
    pc.on_negotiation_needed(Box::new(move || {
        let t = t.clone();
        Box::pin(async move {
            t.trace("negotiationneeded", serde_json::Value::Null);
        })
    }));

    let t = tracer;
    pc.on_data_channel(Box::new(move |channel| {
        let t = t.clone();
        Box::pin(async move {
            t.trace("datachannel", json!([channel.id(), channel.label()]));
        })
    }));
}

/// Generate a throwaway SDP offer with `audio` + `video` m-lines in `direction`,
/// used purely so the SFU can extract our codec capabilities on the join
/// request. The temporary PeerConnection is closed before returning.
///
/// `sendonly` produces the publisher SDP; `recvonly` produces the subscriber SDP.
pub async fn generic_sdp(direction: RTCRtpTransceiverDirection) -> Result<String> {
    let api = build_api()?;
    let pc = api.new_peer_connection(RTCConfiguration::default()).await?;

    let result = build_generic_offer(&pc, direction).await;
    // Always tear the temp PC down, even if offer creation failed.
    let _ = pc.close().await;
    result
}

async fn build_generic_offer(
    pc: &RTCPeerConnection,
    direction: RTCRtpTransceiverDirection,
) -> Result<String> {
    use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

    pc.add_transceiver_from_kind(
        RTPCodecType::Video,
        Some(RTCRtpTransceiverInit {
            direction,
            send_encodings: vec![],
        }),
    )
    .await?;
    pc.add_transceiver_from_kind(
        RTPCodecType::Audio,
        Some(RTCRtpTransceiverInit {
            direction,
            send_encodings: vec![],
        }),
    )
    .await?;

    let offer = pc.create_offer(None).await?;
    Ok(offer.sdp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ice_servers_filter_empty_and_map_credential() {
        let servers = vec![
            IceServer {
                urls: vec!["stun:stun.l.google.com:19302".into()],
                username: String::new(),
                password: String::new(),
            },
            IceServer {
                urls: vec!["turn:turn.example.com:3478".into()],
                username: "user".into(),
                password: "pass".into(),
            },
            IceServer::default(), // no urls -> filtered out
        ];
        let mapped = to_rtc_ice_servers(&servers);
        assert_eq!(mapped.len(), 2);
        assert_eq!(mapped[1].username, "user");
        assert_eq!(mapped[1].credential, "pass");
    }

    #[tokio::test]
    async fn generic_sdp_has_audio_and_video_mlines() {
        let sdp = generic_sdp(RTCRtpTransceiverDirection::Recvonly)
            .await
            .expect("generic sdp");
        assert!(sdp.contains("m=audio"), "expected audio m-line");
        assert!(sdp.contains("m=video"), "expected video m-line");
    }

    #[tokio::test]
    async fn generic_sdp_advertises_only_decodable_codecs() {
        let sdp = generic_sdp(RTCRtpTransceiverDirection::Recvonly)
            .await
            .expect("generic sdp");
        let audio = media_section(&sdp, "audio");
        let video = media_section(&sdp, "video");

        assert!(audio.contains("opus/48000/2"), "missing Opus:\n{audio}");
        for unsupported in ["PCMU/8000", "PCMA/8000", "G722/8000"] {
            assert!(
                !audio.contains(unsupported),
                "advertised unsupported audio codec {unsupported}:\n{audio}"
            );
        }

        for supported in ["VP8/90000", "VP9/90000", "H264/90000"] {
            assert!(
                video.contains(supported),
                "missing supported video codec {supported}:\n{video}"
            );
        }
        for unsupported in ["AV1/90000", "H265/90000"] {
            assert!(
                !video.contains(unsupported),
                "advertised unsupported video codec {unsupported}:\n{video}"
            );
        }
        assert!(
            video.contains("profile-id=0"),
            "missing VP9 profile 0:\n{video}"
        );
        assert!(
            !video.contains("profile-id=1"),
            "advertised unsupported VP9 profile 1:\n{video}"
        );
    }

    /// Split an SDP into its per-m-line sections, keyed by media kind.
    fn media_section<'a>(sdp: &'a str, kind: &str) -> &'a str {
        let start = sdp
            .find(&format!("m={kind}"))
            .unwrap_or_else(|| panic!("no m={kind} section in:\n{sdp}"));
        let rest = &sdp[start..];
        match rest[1..].find("\r\nm=") {
            Some(end) => &rest[..end + 1],
            None => rest,
        }
    }

    #[tokio::test]
    async fn sendonly_sdp_offers_audio_level_on_audio_only() {
        let sdp = generic_sdp(RTCRtpTransceiverDirection::Sendonly)
            .await
            .expect("generic sdp");
        let audio = media_section(&sdp, "audio");
        let video = media_section(&sdp, "video");
        let extmap = audio
            .lines()
            .map(str::trim_end)
            .find(|l| l.starts_with("a=extmap:") && l.ends_with(AUDIO_LEVEL_URI));
        assert!(
            extmap.is_some(),
            "publisher audio m-line must offer a=extmap:<n> {AUDIO_LEVEL_URI}:\n{audio}"
        );
        assert!(
            !video.contains(AUDIO_LEVEL_URI),
            "audio-level extmap must not appear on the video m-line:\n{video}"
        );
    }

    #[tokio::test]
    async fn recvonly_sdp_omits_audio_level() {
        // The subscriber PC is recvonly and never publishes, so the send-only
        // registration must keep the extension off it.
        let sdp = generic_sdp(RTCRtpTransceiverDirection::Recvonly)
            .await
            .expect("generic sdp");
        assert!(
            !sdp.contains(AUDIO_LEVEL_URI),
            "recvonly SDP must not offer {AUDIO_LEVEL_URI}:\n{sdp}"
        );
    }
}
