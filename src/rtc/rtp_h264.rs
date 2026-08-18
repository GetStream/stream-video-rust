//! RFC 6184 H264 RTP packetization and bounded, panic-free depacketization.

use bytes::{BufMut, Bytes, BytesMut};
use webrtc::rtp::Error as RtpError;
use webrtc::rtp::codecs::h264::H264Payloader;
use webrtc::rtp::packetizer::{Depacketizer, Payloader};

use super::error::{Result, RtcError};

const ANNEX_B_START_CODE: &[u8] = &[0, 0, 0, 1];
const NAL_TYPE_MASK: u8 = 0x1f;
const NAL_NRI_MASK: u8 = 0x60;
const STAP_A: u8 = 24;
const FU_A: u8 = 28;
const FU_START: u8 = 0x80;
const FU_END: u8 = 0x40;
const FU_RESERVED: u8 = 0x20;
const MAX_FRAGMENTED_NAL_BYTES: usize = 16 * 1024 * 1024;

type RtpResult<T> = std::result::Result<T, RtpError>;

pub(crate) struct RtpPayload {
    pub data: Bytes,
    pub last: bool,
}

#[derive(Default)]
pub(crate) struct H264RtpPacketizer {
    payloader: H264Payloader,
}

impl H264RtpPacketizer {
    pub(crate) fn packetize(
        &mut self,
        access_unit: &[u8],
        max_payload: usize,
    ) -> Result<Vec<RtpPayload>> {
        if access_unit.is_empty() {
            return Ok(Vec::new());
        }
        if max_payload <= 2 {
            return Err(RtcError::Media(
                "H264 RTP payload size must exceed the FU-A header".to_owned(),
            ));
        }

        let payloads = self
            .payloader
            .payload(max_payload, &Bytes::copy_from_slice(access_unit))
            .map_err(|error| RtcError::Media(format!("H264 packetize: {error}")))?;
        let last_index = payloads.len().saturating_sub(1);
        Ok(payloads
            .into_iter()
            .enumerate()
            .map(|(index, data)| RtpPayload {
                data,
                last: index == last_index,
            })
            .collect())
    }
}

struct FragmentedNal {
    indicator: u8,
    nal_type: u8,
    data: BytesMut,
}

/// Stateful H264 depacketizer that rejects malformed STAP-A and FU-A payloads.
///
/// The upstream depacketizer accepts FU-A fragments without a start packet and
/// can index past a malformed STAP-A length field. This implementation keeps
/// those network inputs fallible and caps fragmented-NAL growth.
#[derive(Default)]
pub(crate) struct H264Depacketizer {
    fragmented: Option<FragmentedNal>,
}

impl H264Depacketizer {
    fn malformed(message: impl Into<String>) -> RtpError {
        RtpError::Other(message.into())
    }

    fn depacketize_single(packet: &Bytes) -> RtpResult<Bytes> {
        if packet.len() < 2 {
            return Err(RtpError::ErrShortPacket);
        }
        let mut output = BytesMut::with_capacity(ANNEX_B_START_CODE.len() + packet.len());
        output.put_slice(ANNEX_B_START_CODE);
        output.put_slice(packet);
        Ok(output.freeze())
    }

    fn depacketize_stap_a(packet: &Bytes) -> RtpResult<Bytes> {
        let mut offset = 1usize;
        let mut output = BytesMut::new();
        while offset < packet.len() {
            let remaining = packet.len() - offset;
            if remaining < 2 {
                return Err(Self::malformed("truncated H264 STAP-A length"));
            }
            let nal_len = usize::from(u16::from_be_bytes([packet[offset], packet[offset + 1]]));
            offset += 2;
            if nal_len == 0 {
                return Err(Self::malformed("zero-length H264 STAP-A NAL unit"));
            }
            let available = packet.len() - offset;
            if nal_len > available {
                return Err(RtpError::StapASizeLargerThanBuffer(nal_len, available));
            }
            let nal_type = packet[offset] & NAL_TYPE_MASK;
            if !(1..=23).contains(&nal_type) {
                return Err(Self::malformed(format!(
                    "nested or invalid H264 STAP-A NAL type {nal_type}"
                )));
            }
            if output
                .len()
                .saturating_add(ANNEX_B_START_CODE.len())
                .saturating_add(nal_len)
                > MAX_FRAGMENTED_NAL_BYTES
            {
                return Err(Self::malformed("H264 STAP-A exceeds size limit"));
            }
            output.reserve(ANNEX_B_START_CODE.len() + nal_len);
            output.put_slice(ANNEX_B_START_CODE);
            output.put_slice(&packet[offset..offset + nal_len]);
            offset += nal_len;
        }
        if output.is_empty() {
            return Err(Self::malformed("empty H264 STAP-A payload"));
        }
        Ok(output.freeze())
    }

    fn depacketize_fu_a(&mut self, packet: &Bytes) -> RtpResult<Bytes> {
        if packet.len() <= 2 {
            self.fragmented = None;
            return Err(RtpError::ErrShortPacket);
        }

        let indicator = packet[0] & (0x80 | NAL_NRI_MASK);
        let header = packet[1];
        let nal_type = header & NAL_TYPE_MASK;
        let start = header & FU_START != 0;
        let end = header & FU_END != 0;
        if header & FU_RESERVED != 0 || nal_type == 0 || start && end {
            self.fragmented = None;
            return Err(Self::malformed("invalid H264 FU-A header"));
        }

        if start {
            if self.fragmented.is_some() {
                self.fragmented = None;
                return Err(Self::malformed(
                    "H264 FU-A start arrived before the prior fragment ended",
                ));
            }
            if ANNEX_B_START_CODE
                .len()
                .saturating_add(1)
                .saturating_add(packet.len() - 2)
                > MAX_FRAGMENTED_NAL_BYTES
            {
                return Err(Self::malformed("H264 fragmented NAL exceeds size limit"));
            }
            let mut data = BytesMut::with_capacity(packet.len() + ANNEX_B_START_CODE.len());
            data.put_slice(ANNEX_B_START_CODE);
            data.put_u8(indicator | nal_type);
            data.put_slice(&packet[2..]);
            self.fragmented = Some(FragmentedNal {
                indicator,
                nal_type,
                data,
            });
            return Ok(Bytes::new());
        }

        let Some(fragmented) = self.fragmented.as_mut() else {
            return Err(Self::malformed("H264 FU-A continuation without a start"));
        };
        if fragmented.indicator != indicator || fragmented.nal_type != nal_type {
            self.fragmented = None;
            return Err(Self::malformed(
                "H264 FU-A continuation changed NRI or NAL type",
            ));
        }
        if fragmented.data.len().saturating_add(packet.len() - 2) > MAX_FRAGMENTED_NAL_BYTES {
            self.fragmented = None;
            return Err(Self::malformed("H264 fragmented NAL exceeds size limit"));
        }
        fragmented.data.put_slice(&packet[2..]);
        if end {
            let completed = self
                .fragmented
                .take()
                .ok_or_else(|| Self::malformed("H264 FU-A state disappeared"))?;
            return Ok(completed.data.freeze());
        }
        Ok(Bytes::new())
    }
}

impl Depacketizer for H264Depacketizer {
    fn depacketize(&mut self, packet: &Bytes) -> RtpResult<Bytes> {
        let Some(first) = packet.first().copied() else {
            self.fragmented = None;
            return Err(RtpError::ErrShortPacket);
        };
        match first & NAL_TYPE_MASK {
            1..=23 => {
                if self.fragmented.take().is_some() {
                    return Err(Self::malformed(
                        "single H264 NAL interrupted a fragmented NAL",
                    ));
                }
                Self::depacketize_single(packet)
            }
            STAP_A => {
                if self.fragmented.take().is_some() {
                    return Err(Self::malformed("H264 STAP-A interrupted a fragmented NAL"));
                }
                Self::depacketize_stap_a(packet)
            }
            FU_A => self.depacketize_fu_a(packet),
            nal_type => {
                self.fragmented = None;
                Err(RtpError::NaluTypeIsNotHandled(nal_type))
            }
        }
    }

    fn is_partition_head(&self, payload: &Bytes) -> bool {
        let Some(first) = payload.first() else {
            return false;
        };
        if first & NAL_TYPE_MASK == FU_A {
            payload.get(1).is_some_and(|header| header & FU_START != 0)
        } else {
            true
        }
    }

    fn is_partition_tail(&self, marker: bool, _payload: &Bytes) -> bool {
        marker
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_access_unit_round_trips_through_stap_and_fu_a() {
        let mut access_unit = Vec::new();
        access_unit.extend_from_slice(ANNEX_B_START_CODE);
        access_unit.extend_from_slice(&[0x67, 0x42, 0xe0, 0x1f, 0x89]);
        access_unit.extend_from_slice(ANNEX_B_START_CODE);
        access_unit.extend_from_slice(&[0x68, 0xce, 0x06, 0xe2]);
        access_unit.extend_from_slice(ANNEX_B_START_CODE);
        access_unit.extend_from_slice(&[0x65]);
        access_unit.extend(std::iter::repeat_n(0xab, 4_000));

        let mut packetizer = H264RtpPacketizer::default();
        let payloads = packetizer
            .packetize(&access_unit, 1_200)
            .expect("packetize H264");
        assert!(payloads.len() >= 5);
        assert_eq!(payloads[0].data[0] & NAL_TYPE_MASK, STAP_A);
        assert!(
            payloads
                .iter()
                .skip(1)
                .any(|p| p.data[0] & NAL_TYPE_MASK == FU_A)
        );
        assert!(payloads.last().is_some_and(|payload| payload.last));

        let mut depacketizer = H264Depacketizer::default();
        let mut rebuilt = Vec::new();
        for payload in payloads {
            rebuilt.extend(
                depacketizer
                    .depacketize(&payload.data)
                    .expect("depacketize H264"),
            );
        }
        assert_eq!(rebuilt, access_unit);
    }

    #[test]
    fn single_nal_packetization_mode_round_trips() {
        let packet = Bytes::from_static(&[0x61, 1, 2, 3]);
        let mut depacketizer = H264Depacketizer::default();
        let rebuilt = depacketizer
            .depacketize(&packet)
            .expect("depacketize single NAL");
        assert_eq!(rebuilt.as_ref(), &[0, 0, 0, 1, 0x61, 1, 2, 3]);
    }

    #[test]
    fn stap_a_preserves_sps_pps_idr_order() {
        let stap = Bytes::from_static(&[
            0x78, // STAP-A with NRI=3
            0, 3, 0x67, 0x42, 0xe0, // SPS
            0, 2, 0x68, 0xce, // PPS
            0, 3, 0x65, 0xaa, 0xbb, // IDR
        ]);
        let mut depacketizer = H264Depacketizer::default();
        let rebuilt = depacketizer
            .depacketize(&stap)
            .expect("depacketize SPS/PPS/IDR STAP-A");
        assert_eq!(
            rebuilt.as_ref(),
            &[
                0, 0, 0, 1, 0x67, 0x42, 0xe0, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0xaa, 0xbb,
            ]
        );
    }

    #[test]
    fn malformed_stap_a_trailing_length_byte_is_an_error() {
        let mut depacketizer = H264Depacketizer::default();
        let malformed = Bytes::from_static(&[STAP_A, 0, 1, 0x67, 0]);
        assert!(depacketizer.depacketize(&malformed).is_err());
    }

    #[test]
    fn fu_a_continuation_without_start_is_an_error() {
        let mut depacketizer = H264Depacketizer::default();
        let continuation = Bytes::from_static(&[FU_A | 0x60, 0x05, 1, 2, 3]);
        assert!(depacketizer.depacketize(&continuation).is_err());
    }

    #[test]
    fn malformed_fu_a_clears_state_for_the_next_nal() {
        let mut depacketizer = H264Depacketizer::default();
        let start = Bytes::from_static(&[FU_A | 0x60, FU_START | 0x05, 1, 2]);
        assert!(
            depacketizer
                .depacketize(&start)
                .expect("start fragmented IDR")
                .is_empty()
        );

        let wrong_type = Bytes::from_static(&[FU_A | 0x60, 0x01, 3]);
        assert!(depacketizer.depacketize(&wrong_type).is_err());

        let single = Bytes::from_static(&[0x61, 9, 8, 7]);
        let rebuilt = depacketizer
            .depacketize(&single)
            .expect("recover with a complete single NAL");
        assert_eq!(rebuilt.as_ref(), &[0, 0, 0, 1, 0x61, 9, 8, 7]);
    }

    #[test]
    fn invalid_packetizer_mtu_is_an_error() {
        let mut packetizer = H264RtpPacketizer::default();
        assert!(packetizer.packetize(&[0x65, 1, 2], 2).is_err());
    }
}
