//! VP8/VP9 RTP payload packetization for the outbound video path.
//!
//! webrtc-rs's built-in VP9 payloader emits a minimal descriptor — always
//! flexible mode, `P=0`, no layer indices, no scalability structure. The Stream
//! SFU cannot use that to identify a keyframe: it just spins sending PLIs and
//! never forwards the track. We build the descriptor ourselves from the
//! encoder's keyframe flag, matching what libvpx / browsers emit: non-flexible
//! mode with layer indices, and the scalability structure (SS) on each keyframe
//! so the SFU learns the single spatial layer's resolution and can forward.
//!
//! References: RFC 7741 (VP8) and RFC 9628 (VP9).

use std::time::{SystemTime, UNIX_EPOCH};

use super::vpx::{EncodedFrame, Vp9SvcMode, VpxCodec};

/// One RTP-ready payload (a VPx descriptor followed by a bitstream fragment)
/// plus whether it carries the RTP marker bit. For SVC this is true only on the
/// final packet of the picture's highest spatial frame.
pub(crate) struct RtpPayload {
    pub data: Vec<u8>,
    pub last: bool,
}

/// Stateful VP8/VP9 RTP packetizer, tracking the per-frame picture id and the
/// temporal-layer-0 index the descriptors carry.
#[derive(Clone)]
pub(crate) struct VpxRtpPacketizer {
    codec: VpxCodec,
    /// 15-bit VP9 / 15-bit VP8 picture id, wraps at `0x7FFF`.
    picture_id: u16,
    /// VP9 temporal-layer-0 index (non-flexible mode), wraps at `0xFF`.
    tl0_pic_idx: u8,
    /// Position in the fixed temporal picture group.
    gof_index: u8,
}

impl VpxRtpPacketizer {
    pub(crate) fn new(codec: VpxCodec) -> Self {
        Self {
            codec,
            picture_id: seed_u16() & 0x7FFF,
            tl0_pic_idx: seed_u16() as u8,
            gof_index: 0,
        }
    }

    /// Split one encoded frame into RTP payloads (each descriptor + fragment fits
    /// in `max_payload`), writing the codec descriptor onto every fragment.
    /// `width`/`height` populate the VP9 scalability structure on keyframes.
    pub(crate) fn packetize(
        &mut self,
        frame: &[u8],
        keyframe: bool,
        width: u16,
        height: u16,
        max_payload: usize,
    ) -> Vec<RtpPayload> {
        let out = match self.codec {
            VpxCodec::Vp9 => self.packetize_vp9(frame, keyframe, width, height, max_payload),
            VpxCodec::Vp8 => self.packetize_vp8(frame, max_payload),
        };
        self.picture_id = (self.picture_id + 1) & 0x7FFF;
        self.tl0_pic_idx = self.tl0_pic_idx.wrapping_add(1);
        out
    }

    /// Packetize all spatial frames belonging to one VP9 SVC picture. Every
    /// layer shares the same PID and RTP timestamp; only the final packet of
    /// the highest spatial frame carries the RTP marker.
    pub(crate) fn packetize_svc_picture(
        &mut self,
        frames: &[EncodedFrame],
        mode: Vp9SvcMode,
        full_width: u16,
        full_height: u16,
        max_payload: usize,
    ) -> Vec<RtpPayload> {
        let Some(first) = frames.first() else {
            return Vec::new();
        };
        let key_picture = first.key;
        let temporal_id = if key_picture { 0 } else { first.temporal_id };
        if key_picture {
            self.gof_index = 0;
        }
        let up_switch = temporal_up_switch(mode.temporal_layers(), self.gof_index);
        // RFC 9628 increments TL0PICIDX for the T0 picture itself; all higher
        // temporal pictures that depend on it retain that newly assigned value.
        if temporal_id == 0 {
            self.tl0_pic_idx = self.tl0_pic_idx.wrapping_add(1);
        }
        let current_tl0 = self.tl0_pic_idx;
        let highest_sid = frames
            .iter()
            .map(|frame| frame.spatial_id)
            .max()
            .unwrap_or_default();
        let dimensions = svc_dimensions(frames, mode, full_width, full_height);
        let mut payloads = Vec::new();

        for frame in frames {
            if frame.data.is_empty() {
                continue;
            }
            let total = frame.data.len();
            let mut offset = 0;
            let mut first_fragment = true;
            while offset < total {
                let has_ss = key_picture && frame.spatial_id == 0 && first_fragment;
                let ss_len = if has_ss {
                    scalability_structure_len(mode)
                } else {
                    0
                };
                let descriptor_len = 5 + ss_len;
                let room = max_payload.saturating_sub(descriptor_len).max(1);
                let fragment_len = room.min(total - offset);
                let end_of_frame = offset + fragment_len >= total;

                let mut b0 = 0x80 | 0x20; // I + L; F=0 (fixed pattern).
                if !key_picture {
                    b0 |= 0x40; // P = inter-picture predicted.
                    b0 |= 0x01; // Z = unused for upper-layer prediction in K-SVC.
                }
                if first_fragment {
                    b0 |= 0x08;
                }
                if end_of_frame {
                    b0 |= 0x04;
                }
                if has_ss {
                    b0 |= 0x02;
                }

                let mut data = Vec::with_capacity(descriptor_len + fragment_len);
                data.push(b0);
                data.push(0x80 | ((self.picture_id >> 8) as u8 & 0x7F));
                data.push((self.picture_id & 0xFF) as u8);
                let inter_layer_dependency = key_picture && frame.spatial_id > 0;
                data.push(
                    (temporal_id << 5)
                        | (u8::from(up_switch) << 4)
                        | (frame.spatial_id << 1)
                        | u8::from(inter_layer_dependency),
                );
                data.push(current_tl0);
                if has_ss {
                    write_scalability_structure(&mut data, mode, &dimensions);
                }
                data.extend_from_slice(&frame.data[offset..offset + fragment_len]);
                payloads.push(RtpPayload {
                    data,
                    last: end_of_frame && frame.spatial_id == highest_sid,
                });
                offset += fragment_len;
                first_fragment = false;
            }
        }

        self.picture_id = (self.picture_id + 1) & 0x7FFF;
        self.gof_index = (self.gof_index + 1) % temporal_pattern_len(mode.temporal_layers());
        payloads
    }

    fn packetize_vp9(
        &self,
        frame: &[u8],
        keyframe: bool,
        width: u16,
        height: u16,
        max_payload: usize,
    ) -> Vec<RtpPayload> {
        if frame.is_empty() {
            return Vec::new();
        }
        let mut payloads = Vec::new();
        let total = frame.len();
        let mut offset = 0;
        let mut first = true;
        while offset < total {
            let ss = keyframe && first;
            // Descriptor: b0 + picture-id(2) + layer-indices(1) + tl0picidx(1),
            // plus SS(5) on the first packet of a keyframe.
            let desc_len = 5 + if ss { 5 } else { 0 };
            let room = max_payload.saturating_sub(desc_len).max(1);
            let frag = room.min(total - offset);
            let last = offset + frag >= total;

            let mut b0 = 0x80u8; // I = picture id present
            if !keyframe {
                b0 |= 0x40; // P = inter-picture predicted
            }
            b0 |= 0x20; // L = layer indices present (F stays 0: non-flexible)
            if first {
                b0 |= 0x08; // B = start of frame
            }
            if last {
                b0 |= 0x04; // E = end of frame
            }
            if ss {
                b0 |= 0x02; // V = scalability structure present
            }

            let mut data = Vec::with_capacity(desc_len + frag);
            data.push(b0);
            // 15-bit picture id (M = 1).
            data.push(0x80 | ((self.picture_id >> 8) as u8 & 0x7F));
            data.push((self.picture_id & 0xFF) as u8);
            // Layer indices: TID=0, U=0, SID=0, D=0 (single base layer).
            data.push(0x00);
            data.push(self.tl0_pic_idx);
            if ss {
                // N_S=0 (1 spatial layer), Y=1 (resolution present), G=0.
                data.push(0x10);
                data.extend_from_slice(&width.to_be_bytes());
                data.extend_from_slice(&height.to_be_bytes());
            }
            data.extend_from_slice(&frame[offset..offset + frag]);
            payloads.push(RtpPayload { data, last });

            offset += frag;
            first = false;
        }
        payloads
    }

    fn packetize_vp8(&self, frame: &[u8], max_payload: usize) -> Vec<RtpPayload> {
        if frame.is_empty() {
            return Vec::new();
        }
        let mut payloads = Vec::new();
        let total = frame.len();
        let mut offset = 0;
        let mut first = true;
        // Descriptor: X byte + I extension byte + 15-bit picture id.
        let desc_len = 4;
        while offset < total {
            let room = max_payload.saturating_sub(desc_len).max(1);
            let frag = room.min(total - offset);
            let last = offset + frag >= total;

            let mut x = 0x80u8; // X = extended control bits present (for picture id)
            if first {
                x |= 0x10; // S = start of partition
            }
            let mut data = Vec::with_capacity(desc_len + frag);
            data.push(x);
            data.push(0x80); // I = picture id present
            data.push(0x80 | ((self.picture_id >> 8) as u8 & 0x7F));
            data.push((self.picture_id & 0xFF) as u8);
            data.extend_from_slice(&frame[offset..offset + frag]);
            payloads.push(RtpPayload { data, last });

            offset += frag;
            first = false;
        }
        payloads
    }
}

fn temporal_pattern_len(temporal_layers: u8) -> u8 {
    match temporal_layers {
        1 => 1,
        2 => 2,
        _ => 4,
    }
}

fn temporal_up_switch(temporal_layers: u8, gof_index: u8) -> bool {
    match temporal_layers {
        1 => true,
        2 => [false, true][usize::from(gof_index % 2)],
        _ => [false, true, true, false][usize::from(gof_index % 4)],
    }
}

fn scalability_structure_len(mode: Vp9SvcMode) -> usize {
    let dimensions = usize::from(mode.spatial_layers()) * 4;
    let gof = match mode.temporal_layers() {
        1 => 0,
        2 => 1 + (2 * 2),
        _ => 1 + 2 + 2 + 2 + 3,
    };
    1 + dimensions + gof
}

fn svc_dimensions(
    frames: &[EncodedFrame],
    mode: Vp9SvcMode,
    full_width: u16,
    full_height: u16,
) -> Vec<(u16, u16)> {
    let full = (full_width.max(2), full_height.max(2));
    (0..mode.spatial_layers())
        .map(|spatial_id| {
            frames
                .iter()
                .find(|frame| frame.spatial_id == spatial_id)
                .map(|frame| (frame.width, frame.height))
                .unwrap_or_else(|| {
                    let shift = u32::from(mode.spatial_layers() - spatial_id - 1);
                    ((full.0 >> shift).max(2), (full.1 >> shift).max(2))
                })
        })
        .collect()
}

fn write_scalability_structure(data: &mut Vec<u8>, mode: Vp9SvcMode, dimensions: &[(u16, u16)]) {
    let has_gof = mode.temporal_layers() > 1;
    data.push(((mode.spatial_layers() - 1) << 5) | 0x10 | (u8::from(has_gof) << 3));
    for (width, height) in dimensions {
        data.extend_from_slice(&width.to_be_bytes());
        data.extend_from_slice(&height.to_be_bytes());
    }
    match mode.temporal_layers() {
        1 => {}
        2 => {
            data.push(2); // N_G
            write_gof_entry(data, 0, false, &[2]);
            write_gof_entry(data, 1, true, &[1]);
        }
        _ => {
            data.push(4); // N_G
            write_gof_entry(data, 0, false, &[4]);
            write_gof_entry(data, 2, true, &[1]);
            write_gof_entry(data, 1, true, &[2]);
            write_gof_entry(data, 2, false, &[1, 2]);
        }
    }
}

fn write_gof_entry(data: &mut Vec<u8>, temporal_id: u8, up_switch: bool, references: &[u8]) {
    data.push(
        (temporal_id << 5)
            | (u8::from(up_switch) << 4)
            | (u8::try_from(references.len()).unwrap_or(3).min(3) << 2),
    );
    data.extend_from_slice(references);
}

/// A low-entropy 16-bit seed from the wall clock (no `rand` dependency), used to
/// randomize the initial picture id / tl0 index so a fresh track does not always
/// start at zero.
fn seed_u16() -> u16 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u16)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc_frame(spatial_id: u8, temporal_id: u8, key: bool) -> EncodedFrame {
        let shift = u32::from(2 - spatial_id);
        EncodedFrame {
            data: vec![spatial_id + 1; 8],
            key,
            spatial_id,
            temporal_id,
            width: 320 >> shift,
            height: 240 >> shift,
        }
    }

    #[test]
    fn vp9_keyframe_has_scalability_structure() {
        let mut p = VpxRtpPacketizer::new(VpxCodec::Vp9);
        let out = p.packetize(&[1, 2, 3, 4], true, 320, 240, 1200);
        assert_eq!(out.len(), 1);
        let d = &out[0].data;
        // I|P|L|F|B|E|V|Z : keyframe -> P=0, first+last -> B=E=1, V=1 (SS).
        assert_eq!(d[0] & 0x40, 0, "keyframe must have P=0");
        assert_eq!(d[0] & 0x08, 0x08, "first fragment must set B");
        assert_eq!(d[0] & 0x04, 0x04, "last fragment must set E");
        assert_eq!(
            d[0] & 0x02,
            0x02,
            "keyframe must carry the scalability structure"
        );
        assert!(out[0].last);
        // SS carries the resolution: ...,0x10, w(2), h(2).
        let ss = &d[5..10];
        assert_eq!(ss[0], 0x10);
        assert_eq!(u16::from_be_bytes([ss[1], ss[2]]), 320);
        assert_eq!(u16::from_be_bytes([ss[3], ss[4]]), 240);
    }

    #[test]
    fn vp9_inter_frame_sets_p_and_no_ss() {
        let mut p = VpxRtpPacketizer::new(VpxCodec::Vp9);
        let out = p.packetize(&[9; 20], false, 320, 240, 1200);
        assert_eq!(out.len(), 1);
        let d = &out[0].data;
        assert_eq!(d[0] & 0x40, 0x40, "inter frame must set P");
        assert_eq!(d[0] & 0x02, 0, "inter frame must not carry SS");
    }

    #[test]
    fn vp9_fragments_large_frame() {
        let mut p = VpxRtpPacketizer::new(VpxCodec::Vp9);
        let frame = vec![7u8; 3000];
        let out = p.packetize(&frame, true, 640, 480, 1200);
        assert!(
            out.len() >= 3,
            "a 3000-byte frame must fragment across packets"
        );
        assert!(out[0].data[0] & 0x08 != 0, "first sets B");
        assert!(!out[0].last);
        assert!(out.last().unwrap().last, "last fragment sets the marker");
        // Only the first fragment of a keyframe carries the SS.
        assert_eq!(out[0].data[0] & 0x02, 0x02);
        assert_eq!(out[1].data[0] & 0x02, 0);
    }

    #[test]
    fn picture_id_advances_and_wraps() {
        let mut p = VpxRtpPacketizer::new(VpxCodec::Vp9);
        p.picture_id = 0x7FFF;
        let _ = p.packetize(&[1], true, 16, 16, 1200);
        assert_eq!(p.picture_id, 0);
    }

    #[test]
    fn vp8_marks_start_partition_and_picture_id() {
        let mut p = VpxRtpPacketizer::new(VpxCodec::Vp8);
        let out = p.packetize(&[1, 2, 3], true, 0, 0, 1200);
        assert_eq!(out.len(), 1);
        let d = &out[0].data;
        assert_eq!(d[0] & 0x80, 0x80, "X bit set");
        assert_eq!(d[0] & 0x10, 0x10, "S bit set on first packet");
        assert_eq!(d[1] & 0x80, 0x80, "I bit set (picture id present)");
    }

    #[test]
    fn vp9_svc_key_picture_uses_one_pid_and_marks_only_highest_spatial_frame() {
        let mut packetizer = VpxRtpPacketizer::new(VpxCodec::Vp9);
        packetizer.picture_id = 0x1234;
        packetizer.tl0_pic_idx = 7;
        let mode = Vp9SvcMode::new(3, 3).expect("mode");
        let frames = [
            svc_frame(0, 0, true),
            svc_frame(1, 0, true),
            svc_frame(2, 0, true),
        ];

        let payloads = packetizer.packetize_svc_picture(&frames, mode, 320, 240, 1_200);

        assert_eq!(payloads.len(), 3);
        for (spatial_id, payload) in payloads.iter().enumerate() {
            assert_eq!(payload.data[0] & 0x40, 0, "key picture must clear P");
            assert_eq!(payload.data[1], 0x92);
            assert_eq!(payload.data[2], 0x34);
            assert_eq!((payload.data[3] >> 1) & 0x07, spatial_id as u8);
            assert_eq!(payload.data[3] & 0x01, u8::from(spatial_id > 0));
            assert_eq!(payload.data[4], 8);
            assert_eq!(payload.last, spatial_id == 2);
        }
        assert_ne!(payloads[0].data[0] & 0x02, 0, "base frame carries SS");
        assert_eq!(payloads[1].data[0] & 0x02, 0);
        assert_eq!(payloads[2].data[0] & 0x02, 0);

        let ss = &payloads[0].data[5..28];
        assert_eq!(ss[0], 0x58, "N_S=2, Y=1, G=1");
        assert_eq!(u16::from_be_bytes([ss[1], ss[2]]), 80);
        assert_eq!(u16::from_be_bytes([ss[3], ss[4]]), 60);
        assert_eq!(u16::from_be_bytes([ss[5], ss[6]]), 160);
        assert_eq!(u16::from_be_bytes([ss[7], ss[8]]), 120);
        assert_eq!(u16::from_be_bytes([ss[9], ss[10]]), 320);
        assert_eq!(u16::from_be_bytes([ss[11], ss[12]]), 240);
        assert_eq!(ss[13], 4, "four-picture 0212 GOF");
        assert_eq!(
            &ss[14..],
            &[0x04, 4, 0x54, 1, 0x34, 2, 0x48, 1, 2],
            "TIDs, U flags, and P_DIFF references must describe 0212"
        );
        assert_eq!(packetizer.picture_id, 0x1235);
        assert_eq!(packetizer.tl0_pic_idx, 8);
    }

    #[test]
    fn vp9_svc_temporal_pattern_advances_tl0_only_on_base_temporal_frames() {
        let mut packetizer = VpxRtpPacketizer::new(VpxCodec::Vp9);
        packetizer.picture_id = 1;
        packetizer.tl0_pic_idx = 10;
        let mode = Vp9SvcMode::new(1, 3).expect("mode");
        let tids = [0, 2, 1, 2, 0];
        let expected_tl0 = [11, 11, 11, 11, 12];
        let expected_up_switch = [false, true, true, false, false];

        for (index, ((temporal_id, tl0), up_switch)) in tids
            .into_iter()
            .zip(expected_tl0)
            .zip(expected_up_switch)
            .enumerate()
        {
            let payloads = packetizer.packetize_svc_picture(
                &[EncodedFrame {
                    data: vec![1, 2],
                    key: index == 0,
                    spatial_id: 0,
                    temporal_id,
                    width: 320,
                    height: 240,
                }],
                mode,
                320,
                240,
                1_200,
            );
            let payload = payloads.first().expect("payload");
            assert_eq!((payload.data[3] >> 5) & 0x07, temporal_id);
            assert_eq!(payload.data[3] & 0x10 != 0, up_switch);
            assert_eq!(payload.data[4], tl0);
            assert_eq!(payload.data[0] & 0x40 != 0, index > 0, "P bit");
            assert_eq!(payload.data[0] & 0x01 != 0, index > 0, "Z bit");
        }
        assert_eq!(packetizer.tl0_pic_idx, 12);
    }

    #[test]
    fn vp9_svc_ss_keeps_configured_dimensions_when_enhancement_is_dropped() {
        let mut packetizer = VpxRtpPacketizer::new(VpxCodec::Vp9);
        let mode = Vp9SvcMode::new(3, 1).expect("mode");
        let frames = [svc_frame(0, 0, true), svc_frame(1, 0, true)];

        let payloads = packetizer.packetize_svc_picture(&frames, mode, 320, 240, 1_200);

        let ss = &payloads[0].data[5..18];
        assert_eq!(u16::from_be_bytes([ss[9], ss[10]]), 320);
        assert_eq!(u16::from_be_bytes([ss[11], ss[12]]), 240);
        assert!(payloads.last().expect("payload").last);
    }
}
