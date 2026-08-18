//! In-process H264 encode/decode through the maintained `openh264` wrapper.
//!
//! OpenH264 is BSD-2-Clause, but H264 may be covered by patents in some
//! jurisdictions. Applications distributing H264 functionality must evaluate
//! their own patent-licensing obligations.

use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Level,
    Profile, RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::formats::YUVSource;

use super::error::{Result, RtcError};
use super::video_frame::{VideoFrame, i420_len};

const MAX_H264_DECODE_DIMENSION: usize = 3_840;
const MAX_H264_DECODE_PIXELS: usize = 3_840 * 2_160;
const MAX_H264_ACCESS_UNIT_BYTES: usize = 16 * 1024 * 1024;
const H264_MACROBLOCK_EDGE: u32 = 16;
const H264_LEVEL_3_1_MAX_CODED_EDGE: u32 = 2_704;
const H264_LEVEL_3_1_MAX_FRAME_MACROBLOCKS: u32 = 3_600;
const H264_LEVEL_3_1_MAX_MACROBLOCKS_PER_SECOND: u32 = 108_000;
const H264_ENCODER_MAX_FRAME_RATE: u32 = 30;

fn minimum_frame_duration_ns() -> u128 {
    1_000_000_000u128.div_ceil(u128::from(H264_ENCODER_MAX_FRAME_RATE))
}

fn minimum_mbps_duration_ns(macroblocks: u32) -> u128 {
    (u128::from(macroblocks) * 1_000_000_000)
        .div_ceil(u128::from(H264_LEVEL_3_1_MAX_MACROBLOCKS_PER_SECOND))
}

fn validate_h264_encode_dimensions(width: u32, height: u32) -> Result<u32> {
    if width == 0
        || height == 0
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || width > H264_LEVEL_3_1_MAX_CODED_EDGE
        || height > H264_LEVEL_3_1_MAX_CODED_EDGE
    {
        return Err(RtcError::Media(format!(
            "H264 level 3.1 encode dimensions must be non-zero, even, and at most \
             {H264_LEVEL_3_1_MAX_CODED_EDGE} per edge (got {width}x{height})"
        )));
    }
    let macroblocks = width
        .div_ceil(H264_MACROBLOCK_EDGE)
        .checked_mul(height.div_ceil(H264_MACROBLOCK_EDGE))
        .ok_or_else(|| RtcError::Media("H264 macroblock count overflow".to_owned()))?;
    if macroblocks > H264_LEVEL_3_1_MAX_FRAME_MACROBLOCKS {
        return Err(RtcError::Media(format!(
            "H264 level 3.1 allows at most {H264_LEVEL_3_1_MAX_FRAME_MACROBLOCKS} \
             macroblocks per frame (got {macroblocks} for {width}x{height})"
        )));
    }
    Ok(macroblocks)
}

pub(crate) fn validate_h264_encode_request(
    width: u32,
    height: u32,
    duration: std::time::Duration,
) -> Result<()> {
    let macroblocks = validate_h264_encode_dimensions(width, height)?;
    let duration_ns = duration.as_nanos();
    let minimum_duration_ns =
        minimum_frame_duration_ns().max(minimum_mbps_duration_ns(macroblocks));
    if duration_ns < minimum_duration_ns {
        return Err(RtcError::Media(format!(
            "H264 level 3.1 encode duration is too short for {width}x{height}: \
             need at least {minimum_duration_ns} ns (got {duration_ns} ns)"
        )));
    }
    Ok(())
}

pub(crate) fn access_unit_has_idr(data: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset + 4 < data.len() {
        let start_len = if data[offset..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[offset..].starts_with(&[0, 0, 1]) {
            3
        } else {
            offset += 1;
            continue;
        };
        let nal_offset = offset + start_len;
        if data
            .get(nal_offset)
            .is_some_and(|header| header & 0x1f == 5)
        {
            return true;
        }
        offset = nal_offset.saturating_add(1);
    }
    false
}

struct PackedI420<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
    y_len: usize,
    chroma_len: usize,
}

impl<'a> PackedI420<'a> {
    fn new(data: &'a [u8], width: u32, height: u32) -> Result<Self> {
        let width = usize::try_from(width)
            .map_err(|_| RtcError::Media("H264 width does not fit usize".to_owned()))?;
        let height = usize::try_from(height)
            .map_err(|_| RtcError::Media("H264 height does not fit usize".to_owned()))?;
        if width == 0
            || height == 0
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || width > MAX_H264_DECODE_DIMENSION
            || height > MAX_H264_DECODE_DIMENSION
        {
            return Err(RtcError::Media(format!(
                "H264 I420 dimensions must be non-zero, even, and at most \
                 {MAX_H264_DECODE_DIMENSION} per edge (got {width}x{height})"
            )));
        }
        let pixels = width
            .checked_mul(height)
            .filter(|pixels| *pixels <= MAX_H264_DECODE_PIXELS)
            .ok_or_else(|| RtcError::Media("H264 frame dimensions are too large".to_owned()))?;
        let chroma_len = pixels / 4;
        let expected = pixels
            .checked_add(chroma_len.saturating_mul(2))
            .ok_or_else(|| RtcError::Media("H264 I420 length overflow".to_owned()))?;
        if data.len() < expected {
            return Err(RtcError::Media(format!(
                "H264 I420 buffer too small: {} bytes for {width}x{height} (need {expected})",
                data.len()
            )));
        }
        Ok(Self {
            data: &data[..expected],
            width,
            height,
            y_len: pixels,
            chroma_len,
        })
    }
}

impl YUVSource for PackedI420<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.data[..self.y_len]
    }

    fn u(&self) -> &[u8] {
        &self.data[self.y_len..self.y_len + self.chroma_len]
    }

    fn v(&self) -> &[u8] {
        &self.data[self.y_len + self.chroma_len..]
    }
}

pub(crate) struct H264Encoder {
    encoder: Encoder,
    dimensions: Option<(u32, u32)>,
}

impl H264Encoder {
    pub(crate) fn new(bitrate_bps: u32) -> Result<Self> {
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(30.0))
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(true)
            .usage_type(UsageType::CameraVideoRealTime)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .profile(Profile::Baseline)
            .level(Level::Level_3_1)
            .complexity(Complexity::Low)
            .num_threads(2)
            .intra_frame_period(IntraFramePeriod::from_num_frames(30));
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|error| RtcError::Media(format!("OpenH264 encoder init: {error}")))?;
        Ok(Self {
            encoder,
            dimensions: None,
        })
    }

    #[cfg(test)]
    fn dimensions(&self) -> Option<(u32, u32)> {
        self.dimensions
    }

    pub(crate) fn encode_into(
        &mut self,
        i420: &[u8],
        width: u32,
        height: u32,
        force_keyframe: bool,
        output: &mut Vec<u8>,
    ) -> Result<bool> {
        validate_h264_encode_dimensions(width, height)?;
        let source = PackedI420::new(i420, width, height)?;
        let dimensions_changed = self.dimensions != Some((width, height));
        if force_keyframe && self.dimensions.is_some() && !dimensions_changed {
            self.encoder.force_intra_frame();
        }

        let bitstream = self
            .encoder
            .encode(&source)
            .map_err(|error| RtcError::Media(format!("OpenH264 encode: {error}")))?;
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        output.clear();
        bitstream.write_vec(output);
        if output.len() > MAX_H264_ACCESS_UNIT_BYTES {
            output.clear();
            return Err(RtcError::Media(
                "OpenH264 encoded access unit exceeds size limit".to_owned(),
            ));
        }
        self.dimensions = Some((width, height));
        Ok(keyframe)
    }
}

pub(crate) struct H264Decoder {
    decoder: Decoder,
}

impl H264Decoder {
    pub(crate) fn new() -> Result<Self> {
        let decoder = Decoder::new()
            .map_err(|error| RtcError::Media(format!("OpenH264 decoder init: {error}")))?;
        Ok(Self { decoder })
    }

    pub(crate) fn restart(&mut self) -> Result<()> {
        self.decoder = Decoder::new()
            .map_err(|error| RtcError::Media(format!("OpenH264 decoder restart: {error}")))?;
        Ok(())
    }

    pub(crate) fn decode(
        &mut self,
        access_unit: &[u8],
        rtp_timestamp: u32,
    ) -> Result<Vec<VideoFrame>> {
        if access_unit.is_empty() {
            return Ok(Vec::new());
        }
        if access_unit.len() > MAX_H264_ACCESS_UNIT_BYTES {
            return Err(RtcError::Media(
                "H264 access unit exceeds size limit".to_owned(),
            ));
        }

        let decoded = self
            .decoder
            .decode(access_unit)
            .map_err(|error| RtcError::Media(format!("OpenH264 decode: {error}")))?;
        decoded
            .map(|image| copy_packed_i420(&image, rtp_timestamp).map(|frame| vec![frame]))
            .transpose()
            .map(Option::unwrap_or_default)
    }
}

fn copy_packed_i420(image: &impl YUVSource, rtp_timestamp: u32) -> Result<VideoFrame> {
    let (width, height) = image.dimensions();
    if width == 0
        || height == 0
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || width > MAX_H264_DECODE_DIMENSION
        || height > MAX_H264_DECODE_DIMENSION
        || width
            .checked_mul(height)
            .is_none_or(|pixels| pixels > MAX_H264_DECODE_PIXELS)
    {
        return Err(RtcError::Media(format!(
            "OpenH264 returned unsupported frame dimensions {width}x{height}"
        )));
    }

    let (y_stride, u_stride, v_stride) = image.strides();
    let chroma_width = width / 2;
    let chroma_height = height / 2;
    let planes = [
        (image.y(), y_stride, width, height),
        (image.u(), u_stride, chroma_width, chroma_height),
        (image.v(), v_stride, chroma_width, chroma_height),
    ];
    let width_u32 = u32::try_from(width)
        .map_err(|_| RtcError::Media("decoded H264 width does not fit u32".to_owned()))?;
    let height_u32 = u32::try_from(height)
        .map_err(|_| RtcError::Media("decoded H264 height does not fit u32".to_owned()))?;
    let mut data = Vec::with_capacity(i420_len(width_u32, height_u32));
    for (plane, stride, row_len, rows) in planes {
        if stride < row_len
            || rows
                .checked_mul(stride)
                .is_none_or(|required| plane.len() < required)
        {
            return Err(RtcError::Media(
                "OpenH264 returned an invalid I420 plane layout".to_owned(),
            ));
        }
        for row in 0..rows {
            let offset = row * stride;
            data.extend_from_slice(&plane[offset..offset + row_len]);
        }
    }
    Ok(VideoFrame {
        width: width_u32,
        height: height_u32,
        data,
        rtp_timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp_i420(width: u32, height: u32) -> Vec<u8> {
        let (width, height) = (width as usize, height as usize);
        let mut frame = Vec::with_capacity(width * height * 3 / 2);
        for _ in 0..height {
            for x in 0..width {
                frame.push((x * 219 / width.max(1) + 16) as u8);
            }
        }
        frame.extend(std::iter::repeat_n(128, width * height / 4));
        frame.extend(std::iter::repeat_n(128, width * height / 4));
        frame
    }

    fn round_trip(
        encoder: &mut H264Encoder,
        decoder: &mut H264Decoder,
        width: u32,
        height: u32,
        timestamp: u32,
    ) {
        let source = ramp_i420(width, height);
        let mut encoded = Vec::new();
        let keyframe = encoder
            .encode_into(&source, width, height, true, &mut encoded)
            .expect("encode H264");
        assert!(keyframe);
        assert!(!encoded.is_empty());
        let frames = decoder.decode(&encoded, timestamp).expect("decode H264");
        let frame = frames.first().expect("decoded H264 frame");
        assert_eq!((frame.width, frame.height), (width, height));
        assert_eq!(frame.data.len(), i420_len(width, height));
        assert_eq!(frame.rtp_timestamp, timestamp);
        let y = &frame.data[..(width * height) as usize];
        assert!(y[0] < 50);
        assert!(y[width as usize - 1] > 190);
    }

    #[test]
    fn h264_round_trips_packed_i420() {
        let mut encoder = H264Encoder::new(1_000_000).expect("encoder");
        let mut decoder = H264Decoder::new().expect("decoder");
        round_trip(&mut encoder, &mut decoder, 320, 240, 90_000);
    }

    #[test]
    fn h264_resolution_change_reinitializes_cleanly() {
        let mut encoder = H264Encoder::new(1_000_000).expect("encoder");
        let mut decoder = H264Decoder::new().expect("decoder");
        round_trip(&mut encoder, &mut decoder, 320, 240, 90_000);
        round_trip(&mut encoder, &mut decoder, 640, 360, 180_000);
        assert_eq!(encoder.dimensions(), Some((640, 360)));
    }

    #[test]
    fn malformed_h264_is_an_error_or_no_frame_not_a_panic() {
        let mut decoder = H264Decoder::new().expect("decoder");
        let result = decoder.decode(&[0, 0, 0, 1, 0x65, 0xff, 0xff, 0xff], 0);
        assert!(result.is_err() || result.is_ok_and(|frames| frames.is_empty()));
    }

    #[test]
    fn h264_decoder_restarts_after_malformed_input() {
        let source = ramp_i420(320, 240);
        let mut encoder = H264Encoder::new(1_000_000).expect("encoder");
        let mut encoded = Vec::new();
        encoder
            .encode_into(&source, 320, 240, true, &mut encoded)
            .expect("encode recovery keyframe");

        let mut decoder = H264Decoder::new().expect("decoder");
        let _ = decoder.decode(&[0, 0, 0, 1, 0x65, 0xff, 0xff, 0xff], 0);
        decoder.restart().expect("restart decoder");
        let frames = decoder
            .decode(&encoded, 90_000)
            .expect("decode after restart");
        assert_eq!(frames.len(), 1);
        assert_eq!(
            (frames[0].width, frames[0].height),
            (320, 240),
            "the restarted decoder must accept a fresh SPS/PPS/IDR access unit"
        );
    }

    #[test]
    fn invalid_i420_dimensions_and_lengths_are_rejected() {
        let mut encoder = H264Encoder::new(1_000_000).expect("encoder");
        let mut output = Vec::new();
        assert!(
            encoder
                .encode_into(&[], 321, 240, true, &mut output)
                .is_err()
        );
        assert!(
            encoder
                .encode_into(&[0; 10], 320, 240, true, &mut output)
                .is_err()
        );
    }

    #[test]
    fn level_3_1_encoder_rejects_frames_beyond_max_fs() {
        let mut encoder = H264Encoder::new(1_000_000).expect("encoder");
        let mut output = Vec::new();
        let error = encoder
            .encode_into(&[], 1_920, 1_080, true, &mut output)
            .expect_err("1080p exceeds level 3.1 MaxFS");
        assert!(error.to_string().contains("level 3.1"));
    }

    #[test]
    fn level_3_1_timing_uses_ceiling_division_at_30_fps_boundary() {
        assert_eq!(minimum_frame_duration_ns(), 33_333_334);
        assert_eq!(
            minimum_mbps_duration_ns(H264_LEVEL_3_1_MAX_FRAME_MACROBLOCKS),
            33_333_334
        );
        assert!(
            validate_h264_encode_request(1_280, 720, std::time::Duration::from_nanos(33_333_333))
                .is_err()
        );
        validate_h264_encode_request(1_280, 720, std::time::Duration::from_nanos(33_333_334))
            .expect("ceiling-rounded level 3.1 boundary");
    }

    #[test]
    fn idr_detection_accepts_three_and_four_byte_start_codes() {
        assert!(access_unit_has_idr(&[0, 0, 0, 1, 0x65, 1]));
        assert!(access_unit_has_idr(&[0, 0, 1, 0x65, 1]));
        assert!(!access_unit_has_idr(&[0, 0, 0, 1, 0x41, 1]));
        assert!(!access_unit_has_idr(&[0x65, 1]));
    }
}
