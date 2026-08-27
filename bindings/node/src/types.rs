//! JavaScript-facing frame and RTP value types.

use bytes::Bytes;
use napi::bindgen_prelude::Buffer;
use napi_derive::napi;
use webrtc::rtp::header::{Extension, Header};

use getstream::rtc::{PcmFrame, RtpPacket, VideoFrame};

use crate::error::invalid_argument;

#[napi(object)]
pub struct NativePcmFrame {
    pub data: Buffer,
    pub sample_rate: u32,
    pub channels: u32,
    pub duration_ms: f64,
}

impl From<PcmFrame> for NativePcmFrame {
    fn from(frame: PcmFrame) -> Self {
        let mut bytes = Vec::with_capacity(frame.samples.len().saturating_mul(2));
        for sample in &frame.samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        Self {
            data: bytes.into(),
            sample_rate: frame.sample_rate,
            channels: u32::from(frame.channels),
            duration_ms: frame.duration_ms(),
        }
    }
}

#[napi(object)]
pub struct NativeVideoFrame {
    pub data: Buffer,
    pub width: u32,
    pub height: u32,
    pub rtp_timestamp: u32,
}

impl From<VideoFrame> for NativeVideoFrame {
    fn from(frame: VideoFrame) -> Self {
        Self {
            data: frame.data.into(),
            width: frame.width,
            height: frame.height,
            rtp_timestamp: frame.rtp_timestamp,
        }
    }
}

#[napi(object)]
pub struct NativeRtpExtension {
    pub id: u32,
    pub payload: Buffer,
}

#[napi(object)]
pub struct NativeRtpPacket {
    pub version: u32,
    pub padding: bool,
    pub extension: bool,
    pub marker: bool,
    pub payload_type: u32,
    pub sequence_number: u32,
    pub timestamp: u32,
    pub ssrc: u32,
    pub csrc: Vec<u32>,
    pub extension_profile: u32,
    pub extensions: Vec<NativeRtpExtension>,
    pub extensions_padding: u32,
    pub payload: Buffer,
}

impl From<RtpPacket> for NativeRtpPacket {
    fn from(packet: RtpPacket) -> Self {
        Self {
            version: u32::from(packet.header.version),
            padding: packet.header.padding,
            extension: packet.header.extension,
            marker: packet.header.marker,
            payload_type: u32::from(packet.header.payload_type),
            sequence_number: u32::from(packet.header.sequence_number),
            timestamp: packet.header.timestamp,
            ssrc: packet.header.ssrc,
            csrc: packet.header.csrc,
            extension_profile: u32::from(packet.header.extension_profile),
            extensions: packet
                .header
                .extensions
                .into_iter()
                .map(|extension| NativeRtpExtension {
                    id: u32::from(extension.id),
                    payload: extension.payload.to_vec().into(),
                })
                .collect(),
            extensions_padding: u32::try_from(packet.header.extensions_padding).unwrap_or(u32::MAX),
            payload: packet.payload.to_vec().into(),
        }
    }
}

impl TryFrom<NativeRtpPacket> for RtpPacket {
    type Error = napi::Error;

    fn try_from(packet: NativeRtpPacket) -> Result<Self, Self::Error> {
        if packet.version != 2 {
            return Err(invalid_argument("RTP version must be 2"));
        }
        if packet.payload_type > 127 {
            return Err(invalid_argument(
                "RTP payloadType must be between 0 and 127",
            ));
        }
        if packet.csrc.len() > 15 {
            return Err(invalid_argument(
                "RTP csrc cannot contain more than 15 entries",
            ));
        }
        let version = u8::try_from(packet.version)
            .map_err(|_| invalid_argument("RTP version does not fit u8"))?;
        let payload_type = u8::try_from(packet.payload_type)
            .map_err(|_| invalid_argument("RTP payloadType does not fit u8"))?;
        let sequence_number = u16::try_from(packet.sequence_number)
            .map_err(|_| invalid_argument("RTP sequenceNumber exceeds u16"))?;
        let extension_profile = u16::try_from(packet.extension_profile)
            .map_err(|_| invalid_argument("RTP extensionProfile exceeds u16"))?;
        let extensions_padding = usize::try_from(packet.extensions_padding)
            .map_err(|_| invalid_argument("RTP extensionsPadding exceeds usize"))?;
        let extensions = packet
            .extensions
            .into_iter()
            .map(|extension| {
                Ok(Extension {
                    id: u8::try_from(extension.id)
                        .map_err(|_| invalid_argument("RTP extension id exceeds u8"))?,
                    payload: Bytes::copy_from_slice(extension.payload.as_ref()),
                })
            })
            .collect::<Result<Vec<_>, napi::Error>>()?;

        Ok(RtpPacket {
            header: Header {
                version,
                padding: packet.padding,
                extension: packet.extension,
                marker: packet.marker,
                payload_type,
                sequence_number,
                timestamp: packet.timestamp,
                ssrc: packet.ssrc,
                csrc: packet.csrc,
                extension_profile,
                extensions,
                extensions_padding,
            },
            payload: Bytes::copy_from_slice(packet.payload.as_ref()),
        })
    }
}

pub(crate) fn pcm_from_le_bytes(
    data: &[u8],
    sample_rate: u32,
    channels: u32,
) -> napi::Result<PcmFrame> {
    if !data.len().is_multiple_of(2) {
        return Err(invalid_argument(
            "PCM data must contain complete little-endian int16 samples",
        ));
    }
    let channels =
        u16::try_from(channels).map_err(|_| invalid_argument("PCM channels exceeds u16"))?;
    if sample_rate == 0 || channels == 0 {
        return Err(invalid_argument(
            "PCM sampleRate and channels must both be greater than zero",
        ));
    }
    if channels > 2 {
        return Err(invalid_argument("PCM channels must be 1 or 2"));
    }
    if !(data.len() / 2).is_multiple_of(usize::from(channels)) {
        return Err(invalid_argument(
            "PCM data must contain complete interleaved channel frames",
        ));
    }
    let samples = data
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
        .collect();
    Ok(PcmFrame::new(samples, sample_rate, channels))
}

pub(crate) fn duration_from_ms(duration_ms: f64) -> napi::Result<std::time::Duration> {
    if !duration_ms.is_finite() || duration_ms <= 0.0 {
        return Err(invalid_argument(
            "durationMs must be a finite number greater than zero",
        ));
    }
    let duration = std::time::Duration::try_from_secs_f64(duration_ms / 1_000.0)
        .map_err(|_| invalid_argument("durationMs must fit in the supported duration range"))?;
    if duration.is_zero() {
        return Err(invalid_argument(
            "durationMs must resolve to a duration greater than zero",
        ));
    }
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn decode(error: napi::Error) -> Value {
        serde_json::from_str(&error.reason).expect("errors cross the boundary as JSON")
    }

    #[test]
    fn pcm_decodes_little_endian_int16_samples() {
        let frame = pcm_from_le_bytes(&[0x01, 0x00, 0xff, 0xff], 48_000, 2).unwrap();
        assert_eq!(frame.samples, vec![1, -1]);
        assert_eq!(frame.sample_rate, 48_000);
        assert_eq!(frame.channels, 2);
    }

    #[test]
    fn pcm_accepts_an_empty_buffer() {
        assert!(
            pcm_from_le_bytes(&[], 48_000, 1)
                .unwrap()
                .samples
                .is_empty()
        );
    }

    #[test]
    fn pcm_rejects_a_truncated_sample() {
        let error = pcm_from_le_bytes(&[0x01, 0x00, 0x02], 48_000, 1).expect_err("odd length");
        assert_eq!(decode(error)["code"], "RTC_MEDIA");
    }

    #[test]
    fn pcm_rejects_a_zero_sample_rate_or_channel_count() {
        assert!(pcm_from_le_bytes(&[0x00, 0x00], 0, 1).is_err());
        assert!(pcm_from_le_bytes(&[0x00, 0x00], 48_000, 0).is_err());
    }

    #[test]
    fn pcm_rejects_a_channel_count_beyond_u16() {
        assert!(pcm_from_le_bytes(&[0x00, 0x00], 48_000, u32::MAX).is_err());
    }

    #[test]
    fn pcm_rejects_unsupported_channels_and_partial_interleaved_frames() {
        assert!(pcm_from_le_bytes(&[0x00, 0x00, 0x00, 0x00], 48_000, 3).is_err());
        assert!(pcm_from_le_bytes(&[0x00, 0x00], 48_000, 2).is_err());
    }

    #[test]
    fn duration_converts_milliseconds() {
        assert_eq!(
            duration_from_ms(20.0).unwrap(),
            std::time::Duration::from_millis(20)
        );
    }

    #[test]
    fn duration_rejects_non_positive_and_non_finite_values() {
        for value in [
            0.0,
            -1.0,
            f64::MIN_POSITIVE,
            f64::MAX,
            f64::NAN,
            f64::INFINITY,
        ] {
            assert!(
                duration_from_ms(value).is_err(),
                "durationMs {value} should be rejected"
            );
        }
    }

    fn native_rtp_packet() -> NativeRtpPacket {
        NativeRtpPacket {
            version: 2,
            padding: false,
            extension: false,
            marker: false,
            payload_type: 111,
            sequence_number: 65_535,
            timestamp: 123,
            ssrc: 456,
            csrc: Vec::new(),
            extension_profile: 0,
            extensions: Vec::new(),
            extensions_padding: 0,
            payload: vec![1, 2, 3].into(),
        }
    }

    #[test]
    fn rtp_packet_accepts_valid_numeric_boundaries() {
        let packet = RtpPacket::try_from(native_rtp_packet()).expect("valid RTP packet");
        assert_eq!(packet.header.version, 2);
        assert_eq!(packet.header.payload_type, 111);
        assert_eq!(packet.header.sequence_number, u16::MAX);
    }

    #[test]
    fn rtp_packet_rejects_invalid_version_payload_type_and_csrc_count() {
        let mut packet = native_rtp_packet();
        packet.version = 3;
        assert!(RtpPacket::try_from(packet).is_err());

        let mut packet = native_rtp_packet();
        packet.payload_type = 128;
        assert!(RtpPacket::try_from(packet).is_err());

        let mut packet = native_rtp_packet();
        packet.csrc = vec![0; 16];
        assert!(RtpPacket::try_from(packet).is_err());
    }
}
