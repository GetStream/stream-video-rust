//! Sample-rate and channel-count conversion.
//!
//! Two resamplers, for two different jobs:
//!
//! - [`Resampler`] treats each block independently. Output length is fixed by
//!   the rate ratio alone, so a 20 ms block in is a 20 ms block out — what a
//!   model API expecting exact frame sizes needs. Ported from stream-py's
//!   `Resampler`.
//! - [`StreamResampler`] carries a fractional read position and one sample of
//!   history between calls, so a continuous stream joins without clicks at block
//!   boundaries. This is what the publish pacer uses.
//!
//! Both interpolate linearly. That is accurate enough for voice and for the
//! synthesized test tones, and keeps the dependency surface small; a
//! higher-order kernel can replace either one without an API change.

use super::OPUS_SAMPLE_RATE;
use super::PcmFrame;

/// Convert one PCM block to a target sample rate and channel count.
///
/// Each call is independent — no state carries between blocks. The output
/// length depends only on the rate ratio, so identical inputs always produce
/// identical outputs, and a 20 ms input block stays a 20 ms output block.
///
/// ```
/// use getstream::rtc::{PcmFrame, Resampler};
///
/// // A 20 ms block at 16 kHz is 320 frames; at 48 kHz it is exactly 960.
/// let r = Resampler::new(48_000, 1);
/// let out = r.resample(&PcmFrame::mono(vec![0; 320], 16_000));
/// assert_eq!(out.frames(), 960);
/// assert_eq!(out.sample_rate, 48_000);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resampler {
    sample_rate: u32,
    channels: u16,
}

impl Resampler {
    /// A resampler targeting `sample_rate` and `channels`.
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
        }
    }

    /// A resampler targeting 48 kHz mono, the SFU's native layout.
    pub fn to_opus_mono() -> Self {
        Self::new(OPUS_SAMPLE_RATE, 1)
    }

    /// The target sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The target channel count.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Convert `frame` to this resampler's rate and channel count.
    ///
    /// Rate conversion runs per channel before any downmix, so stereo content
    /// is not smeared across channels by the interpolation. Channel conversion
    /// duplicates mono into every output channel and averages multi-channel
    /// input down to mono.
    pub fn resample(&self, frame: &PcmFrame) -> PcmFrame {
        let in_channels = frame.channels.max(1);
        let in_rate = frame.sample_rate.max(1);

        if frame.samples.is_empty() {
            return PcmFrame::new(Vec::new(), self.sample_rate, self.channels);
        }

        // Deinterleave, resample each channel on its own, then reinterleave at
        // the target channel count.
        let planes: Vec<Vec<i16>> = (0..in_channels as usize)
            .map(|ch| {
                let plane: Vec<i16> = frame
                    .samples
                    .iter()
                    .skip(ch)
                    .step_by(in_channels as usize)
                    .copied()
                    .collect();
                resample_plane(&plane, in_rate, self.sample_rate)
            })
            .collect();

        PcmFrame::new(
            interleave(&planes, self.channels),
            self.sample_rate,
            self.channels,
        )
    }
}

/// Resample one non-interleaved channel with linear interpolation.
///
/// Output length is `round(len * to / from)`, and the first and last input
/// samples map exactly onto the first and last output samples. Anchoring both
/// endpoints is what keeps block lengths exact (320 @ 16 kHz → 960 @ 48 kHz)
/// and matches stream-py's `Resampler._resample_1d`.
fn resample_plane(samples: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate || samples.is_empty() {
        return samples.to_vec();
    }

    let in_len = samples.len();
    let out_len = (in_len as f64 * f64::from(to_rate) / f64::from(from_rate)).round() as usize;
    match out_len {
        0 => return Vec::new(),
        // A single output sample has no span to interpolate across.
        1 => return vec![samples[0]],
        _ => {}
    }
    // A single input sample carries no slope; hold it for the whole output.
    if in_len == 1 {
        return vec![samples[0]; out_len];
    }

    let step = (in_len - 1) as f64 / (out_len - 1) as f64;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * step;
            let idx = pos.floor() as usize;
            // The endpoint lands exactly on the last input sample, which has no
            // successor to interpolate toward.
            if idx + 1 >= in_len {
                return samples[in_len - 1];
            }
            let frac = pos - idx as f64;
            let a = f64::from(samples[idx]);
            let b = f64::from(samples[idx + 1]);
            clamp_to_i16(a + (b - a) * frac)
        })
        .collect()
}

/// Interleave per-channel planes into `out_channels`, duplicating mono into
/// every channel and averaging multi-channel input down to mono.
fn interleave(planes: &[Vec<i16>], out_channels: u16) -> Vec<i16> {
    let out_channels = out_channels.max(1) as usize;
    let frames = planes.first().map_or(0, Vec::len);
    let mut out = Vec::with_capacity(frames * out_channels);

    for f in 0..frames {
        if out_channels == 1 && planes.len() > 1 {
            let sum: f64 = planes.iter().map(|p| f64::from(p[f])).sum();
            out.push(clamp_to_i16(sum / planes.len() as f64));
            continue;
        }
        for ch in 0..out_channels {
            // Fewer input channels than requested: repeat the last one, so mono
            // fans out to every output channel.
            let plane = planes.get(ch).unwrap_or(&planes[planes.len() - 1]);
            out.push(plane[f]);
        }
    }
    out
}

/// Round to the nearest integer and saturate, so interpolation overshoot cannot
/// wrap a loud sample to the opposite polarity.
fn clamp_to_i16(v: f64) -> i16 {
    v.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

/// A streaming linear resampler that downmixes to mono and converts to a fixed
/// output rate (48 kHz by default).
///
/// Feed input blocks with [`StreamResampler::push`]; it retains a one-sample
/// history and a fractional read position so consecutive blocks resample
/// continuously. Use this for a live stream, where block boundaries are
/// arbitrary; use [`Resampler`] when each block must convert to an exact length
/// on its own.
#[derive(Debug)]
pub struct StreamResampler {
    out_rate: u32,
    in_rate: u32,
    in_channels: u16,
    /// Carried mono input (f32), including one history sample at index 0.
    inbuf: Vec<f32>,
    /// Fractional read position within `inbuf`.
    pos: f64,
}

impl StreamResampler {
    /// A resampler targeting 48 kHz mono.
    pub fn to_opus_mono() -> Self {
        Self::new(OPUS_SAMPLE_RATE)
    }

    /// A resampler targeting `out_rate` mono.
    pub fn new(out_rate: u32) -> Self {
        Self {
            out_rate: out_rate.max(1),
            in_rate: 0,
            in_channels: 1,
            inbuf: Vec::new(),
            pos: 0.0,
        }
    }

    /// Resample and downmix `frame`, returning mono s16 samples at the target
    /// rate. Returns an empty vector when there is not yet enough input to emit
    /// a sample (the remainder is buffered for the next call).
    pub fn push(&mut self, frame: &PcmFrame) -> Vec<i16> {
        let in_rate = frame.sample_rate.max(1);
        let in_channels = frame.channels.max(1);

        // A format change invalidates the carried history; restart cleanly.
        if in_rate != self.in_rate || in_channels != self.in_channels {
            self.in_rate = in_rate;
            self.in_channels = in_channels;
            self.inbuf.clear();
            self.pos = 0.0;
        }

        // Downmix the incoming block to mono f32 and append.
        let ch = in_channels as usize;
        self.inbuf.reserve(frame.frames());
        for chunk in frame.samples.chunks_exact(ch) {
            let sum: f32 = chunk.iter().map(|&s| f32::from(s)).sum();
            self.inbuf.push(sum / ch as f32);
        }

        // Fast path: identical rate → no interpolation, just emit and reset.
        if in_rate == self.out_rate {
            let out: Vec<i16> = self
                .inbuf
                .drain(..)
                .map(|v| v.round().clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16)
                .collect();
            self.pos = 0.0;
            return out;
        }

        let step = f64::from(in_rate) / f64::from(self.out_rate);
        let mut out = Vec::new();
        // Need `floor(pos) + 1` to exist to interpolate.
        while self.pos + 1.0 < self.inbuf.len() as f64 {
            let idx = self.pos.floor() as usize;
            let frac = (self.pos - idx as f64) as f32;
            let a = self.inbuf[idx];
            let b = self.inbuf[idx + 1];
            let v = a + (b - a) * frac;
            out.push(v.round().clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16);
            self.pos += step;
        }

        // Drop fully-consumed input, keeping one sample of history so the next
        // block interpolates across the boundary.
        let consumed = self.pos.floor() as usize;
        if consumed > 0 {
            let keep_from = consumed.min(self.inbuf.len());
            self.inbuf.drain(..keep_from);
            self.pos -= consumed as f64;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_length_follows_the_rate_ratio_exactly() {
        // The property model APIs depend on: a 20 ms block stays 20 ms.
        for (from, to, frames_in, frames_out) in [
            (16_000, 48_000, 320, 960),
            (48_000, 16_000, 960, 320),
            (8_000, 48_000, 160, 960),
            (44_100, 48_000, 882, 960),
            (24_000, 48_000, 480, 960),
        ] {
            let out = Resampler::new(to, 1).resample(&PcmFrame::mono(vec![0; frames_in], from));
            assert_eq!(out.frames(), frames_out, "{from} -> {to}");
        }
    }

    #[test]
    fn identical_rate_and_channels_is_a_passthrough() {
        let frame = PcmFrame::new(vec![100, 200, 300, 400], 48_000, 2);
        assert_eq!(Resampler::new(48_000, 2).resample(&frame), frame);
    }

    #[test]
    fn endpoints_are_preserved_across_rate_change() {
        let input: Vec<i16> = vec![1000, 2000, 3000, 4000, 5000];
        let out = Resampler::new(48_000, 1).resample(&PcmFrame::mono(input.clone(), 16_000));
        assert_eq!(out.samples[0], input[0]);
        assert_eq!(*out.samples.last().unwrap(), *input.last().unwrap());
    }

    #[test]
    fn stereo_downmix_averages_channels() {
        let stereo = PcmFrame::new(vec![100, 200, 300, 400], 48_000, 2);
        let out = Resampler::new(48_000, 1).resample(&stereo);
        assert_eq!(out.samples, vec![150, 350]);
        assert_eq!(out.channels, 1);
    }

    #[test]
    fn mono_upmix_duplicates_into_both_channels() {
        let mono = PcmFrame::mono(vec![100, 200], 48_000);
        let out = Resampler::new(48_000, 2).resample(&mono);
        assert_eq!(out.samples, vec![100, 100, 200, 200]);
        assert_eq!(out.frames(), 2);
    }

    #[test]
    fn channels_are_resampled_independently_not_smeared() {
        // Hard-panned content: left is loud, right is silent. If the two
        // channels were interpolated as one interleaved run, energy would leak
        // between them.
        let stereo = PcmFrame::new(vec![10_000, 0, 10_000, 0, 10_000, 0, 10_000, 0], 24_000, 2);
        let out = Resampler::new(48_000, 2).resample(&stereo);
        let right_max = out
            .samples
            .iter()
            .skip(1)
            .step_by(2)
            .copied()
            .max()
            .unwrap();
        assert_eq!(
            right_max, 0,
            "silent channel picked up energy from the left"
        );
    }

    #[test]
    fn empty_input_yields_an_empty_frame_at_the_target_layout() {
        let out = Resampler::new(48_000, 2).resample(&PcmFrame::mono(Vec::new(), 16_000));
        assert!(out.is_empty());
        assert_eq!(out.sample_rate, 48_000);
        assert_eq!(out.channels, 2);
    }

    #[test]
    fn single_input_sample_is_held_across_the_output() {
        let out = Resampler::new(48_000, 1).resample(&PcmFrame::mono(vec![1234], 16_000));
        assert_eq!(out.samples, vec![1234; 3]);
    }

    #[test]
    fn full_scale_input_never_wraps_polarity() {
        // Linear interpolation is a weighted average, so every output must land
        // inside the input's range. A wrapping `as i16` cast would break that by
        // flipping a full-scale sample to the opposite rail.
        let loud = PcmFrame::mono(vec![i16::MIN, i16::MAX, i16::MIN, i16::MAX], 16_000);
        let out = Resampler::new(48_000, 1).resample(&loud);
        let lo = *loud.samples.iter().min().unwrap();
        let hi = *loud.samples.iter().max().unwrap();
        assert!(
            out.samples.iter().all(|s| (lo..=hi).contains(s)),
            "interpolation escaped the input range"
        );
        assert_eq!(out.samples[0], i16::MIN);
        assert_eq!(*out.samples.last().unwrap(), i16::MAX);
    }

    #[test]
    fn stream_identity_rate_passes_through_downmixed() {
        let mut r = StreamResampler::to_opus_mono();
        // Stereo 48k -> mono 48k: averages channels, same length per channel.
        let frame = PcmFrame::new(vec![100, 200, 300, 400], 48_000, 2);
        let out = r.push(&frame);
        assert_eq!(out, vec![150, 350]);
    }

    #[test]
    fn stream_downsample_halves_sample_count() {
        // 96k mono -> 48k mono ≈ half as many samples.
        let mut r = StreamResampler::to_opus_mono();
        let input: Vec<i16> = (0..960).map(|i| (i % 100) as i16).collect();
        let frame = PcmFrame::mono(input, 96_000);
        let out = r.push(&frame);
        // ~480 output samples (± a couple for boundary handling).
        assert!(
            (475..=480).contains(&out.len()),
            "unexpected out len {}",
            out.len()
        );
    }

    #[test]
    fn stream_upsample_is_continuous_across_blocks() {
        // 24k -> 48k across two blocks should roughly double total samples and
        // not panic on the boundary.
        let mut r = StreamResampler::to_opus_mono();
        let block: Vec<i16> = (0..240).map(|i| i as i16).collect();
        let a = r.push(&PcmFrame::mono(block.clone(), 24_000));
        let b = r.push(&PcmFrame::mono(block, 24_000));
        assert!(!a.is_empty() && !b.is_empty());
        // 480 input samples @2x ≈ 960 output (allow slack for warm-up).
        let total = a.len() + b.len();
        assert!((950..=960).contains(&total), "unexpected total {total}");
    }
}
