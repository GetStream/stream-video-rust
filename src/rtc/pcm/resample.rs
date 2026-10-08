//! Sample-rate and channel-count conversion.
//!
//! Two resamplers, for two different jobs:
//!
//! - [`Resampler`] treats each block independently. Output length is fixed by
//!   the rate ratio alone, so a 20 ms block in is a 20 ms block out — what a
//!   model API expecting exact frame sizes needs. Ported from stream-py's
//!   `Resampler`.
//! - [`StreamResampler`] runs a windowed-sinc filter (rubato) whose state carries
//!   between calls, so a continuous stream joins without clicks at block
//!   boundaries and does not alias. This is what the publish pacer uses.
//!
//! [`Resampler`] interpolates linearly, so it aliases when it downsamples.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler as _, Resizable as _, SincInterpolationParameters, WindowFunction,
};

use super::OPUS_SAMPLE_RATE;
use super::PcmFrame;
use crate::rtc::error::{Result, RtcError};

/// Sinc filter length in input frames. The filter delays audio by half of it.
const SINC_LEN: usize = 256;
/// Zero frames that push all held input out of the filter. An output sample
/// reads `SINC_LEN + 1` input frames from a fractional position.
const FLUSH_FRAMES: usize = SINC_LEN + 4;

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

/// A streaming sinc resampler to a fixed output rate and channel count. The
/// first pushed frame sets the input rate, and later frames must keep it.
///
/// Feed input blocks of any length with [`StreamResampler::push`]; the filter
/// state carries across calls, so consecutive blocks resample continuously. The
/// filter delays audio by 128 input frames (8 ms at 16 kHz) and holds them until
/// more input or [`flush`](Self::flush) pushes them out. Input already at the
/// output rate passes through unfiltered. Use this for a live stream, where
/// block boundaries are arbitrary; use [`Resampler`] when each block must
/// convert to an exact length on its own.
///
/// ```
/// use getstream::rtc::{PcmFrame, StreamResampler};
///
/// let mut r = StreamResampler::new(48_000, 1);
/// let mut out = r.push(&PcmFrame::mono(vec![0; 320], 16_000))?;
/// out.append(&r.flush()?);
/// assert_eq!(out.sample_rate, 48_000);
/// # Ok::<(), getstream::rtc::RtcError>(())
/// ```
#[derive(Debug)]
pub struct StreamResampler {
    out_rate: u32,
    channels: u16,
    /// The rate of the first pushed frame.
    in_rate: Option<u32>,
    /// `None` before the first frame and when the input rate equals the output
    /// rate.
    filter: Option<Async<f32>>,
    /// Interleaved input of the current call.
    input: Vec<f32>,
    /// Interleaved filter output, `output_frames_max` frames long.
    output: Vec<f32>,
    /// Set when input went into the filter after the last flush.
    holds_audio: bool,
}

impl StreamResampler {
    /// A resampler to `out_rate` for `channels` interleaved channels.
    pub fn new(out_rate: u32, channels: u16) -> Self {
        Self {
            out_rate: out_rate.max(1),
            channels: channels.max(1),
            in_rate: None,
            filter: None,
            input: Vec::new(),
            output: Vec::new(),
            holds_audio: false,
        }
    }

    /// Resample `frame` to the output rate. The first frame sets the input
    /// rate. The newest 128 input frames stay in the filter until the next call
    /// or a [`flush`](Self::flush).
    ///
    /// # Errors
    ///
    /// Returns [`RtcError::PcmRateMismatch`] if `frame` has another rate than
    /// the first frame, [`RtcError::IllegalState`] if it has another channel
    /// count than the resampler, and [`RtcError::Media`] if the filter rejects
    /// the rate or a buffer.
    pub fn push(&mut self, frame: &PcmFrame) -> Result<PcmFrame> {
        let in_rate = self.in_rate.unwrap_or(frame.sample_rate);
        if frame.sample_rate != in_rate {
            return Err(RtcError::PcmRateMismatch {
                expected: in_rate,
                actual: frame.sample_rate,
            });
        }
        if frame.channels != self.channels {
            return Err(RtcError::IllegalState(format!(
                "pcm frame with {} channels, resampler input has {} channels",
                frame.channels, self.channels
            )));
        }
        if self.in_rate.is_none() {
            self.start(in_rate)?;
        }
        let samples = frame.frames() * usize::from(self.channels);
        self.input.clear();
        self.input
            .extend(frame.samples[..samples].iter().map(|&s| f32::from(s)));
        self.run()
    }

    /// Push the audio the filter still holds out with silence and return it.
    /// The filter then holds only silence. Returns an empty frame when no audio
    /// went in since the last flush, or when the input runs at the output rate.
    ///
    /// # Errors
    ///
    /// Returns [`RtcError::Media`] if the filter rejects a buffer.
    pub fn flush(&mut self) -> Result<PcmFrame> {
        if !self.holds_audio {
            return Ok(PcmFrame::new(Vec::new(), self.out_rate, self.channels));
        }
        self.input.clear();
        self.input
            .resize(FLUSH_FRAMES * usize::from(self.channels), 0.0);
        let out = self.run()?;
        self.holds_audio = false;
        Ok(out)
    }

    /// Build the filter for `in_rate` and fix the input rate. A rate the filter
    /// rejects leaves the input rate unset.
    fn start(&mut self, in_rate: u32) -> Result<()> {
        if in_rate != self.out_rate {
            let params =
                SincInterpolationParameters::new(SINC_LEN, WindowFunction::BlackmanHarris2);
            // The largest block one filter call takes; `run` splits longer input.
            let max_chunk = (in_rate as usize / 10).max(1);
            let filter = Async::new_sinc(
                f64::from(self.out_rate) / f64::from(in_rate),
                1.0,
                &params,
                max_chunk,
                usize::from(self.channels),
                FixedAsync::Input,
            )
            .map_err(|e| RtcError::Media(e.to_string()))?;
            self.output = vec![0.0; filter.output_frames_max() * usize::from(self.channels)];
            self.filter = Some(filter);
        }
        self.in_rate = Some(in_rate);
        Ok(())
    }

    /// Pass `self.input` through the filter.
    fn run(&mut self) -> Result<PcmFrame> {
        let ch = usize::from(self.channels);
        let mut out = Vec::new();
        if let Some(filter) = self.filter.as_mut() {
            for piece in self.input.chunks(filter.input_frames_max() * ch) {
                let frames = piece.len() / ch;
                filter
                    .set_chunk_size(frames)
                    .map_err(|e| RtcError::Media(e.to_string()))?;
                let input = InterleavedSlice::new(piece, ch, frames)
                    .map_err(|e| RtcError::Media(e.to_string()))?;
                let capacity = self.output.len() / ch;
                let mut output = InterleavedSlice::new_mut(&mut self.output[..], ch, capacity)
                    .map_err(|e| RtcError::Media(e.to_string()))?;
                let (_, written) = filter
                    .process_into_buffer(&input, &mut output, None)
                    .map_err(|e| RtcError::Media(e.to_string()))?;
                out.extend(
                    self.output[..written * ch]
                        .iter()
                        .map(|&v| clamp_to_i16(f64::from(v))),
                );
                self.holds_audio = true;
            }
        } else {
            out.extend(self.input.iter().map(|&v| clamp_to_i16(f64::from(v))));
        }
        Ok(PcmFrame::new(out, self.out_rate, self.channels))
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

    /// A silent 20 ms block at `rate` whose last 2 ms carry a loud 1 kHz tone.
    fn loud_end(rate: u32) -> PcmFrame {
        let frames = rate as usize / 50;
        let quiet = frames - rate as usize / 500;
        let samples = (0..frames)
            .map(|i| {
                if i < quiet {
                    return 0;
                }
                let time = i as f64 / f64::from(rate);
                (20_000.0 * (std::f64::consts::TAU * 1_000.0 * time).sin()) as i16
            })
            .collect();
        PcmFrame::mono(samples, rate)
    }

    fn peak(samples: &[i16]) -> i16 {
        samples
            .iter()
            .map(|s| s.saturating_abs())
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn stream_flush_returns_the_audio_the_filter_still_holds() {
        let mut r = StreamResampler::new(48_000, 1);

        let pushed = r.push(&loud_end(16_000)).expect("push");
        let flushed = r.flush().expect("flush");

        assert!(
            peak(&pushed.samples) < 1_000,
            "the loud end left before the flush"
        );
        assert!(
            peak(&flushed.samples) > 15_000,
            "the flush did not return the loud end"
        );
    }

    #[test]
    fn stream_flush_leaves_no_old_audio_for_the_next_block() {
        for rate in [8_000, 16_000, 22_050, 24_000, 32_000, 44_100, 96_000] {
            let mut r = StreamResampler::new(48_000, 1);
            r.push(&loud_end(rate)).expect("push");
            r.flush().expect("flush");

            let next = r
                .push(&PcmFrame::silence(rate as usize / 10, rate, 1))
                .expect("push");

            assert_eq!(
                peak(&next.samples),
                0,
                "{rate} Hz: old audio after the flush"
            );
        }
    }

    #[test]
    fn stream_keeps_the_rate_of_the_first_frame_and_its_channels() {
        let mut r = StreamResampler::new(48_000, 1);
        r.push(&PcmFrame::silence(320, 16_000, 1)).expect("push");

        let rate = r
            .push(&PcmFrame::silence(480, 24_000, 1))
            .expect_err("rate differs");
        let channels = r
            .push(&PcmFrame::silence(320, 16_000, 2))
            .expect_err("channels differ");

        assert!(
            matches!(
                rate,
                RtcError::PcmRateMismatch {
                    expected: 16_000,
                    actual: 24_000
                }
            ),
            "error was: {rate}"
        );
        assert!(
            matches!(channels, RtcError::IllegalState(_)),
            "error was: {channels}"
        );
    }

    #[test]
    fn stream_at_the_same_rate_copies_every_channel_and_holds_nothing() {
        let mut r = StreamResampler::new(48_000, 2);
        let frame = PcmFrame::new(vec![100, -200, 300, -400], 48_000, 2);

        let out = r.push(&frame).expect("push");

        assert_eq!(out, frame);
        assert!(r.flush().expect("flush").is_empty());
    }

    #[test]
    fn stream_resamples_each_channel_on_its_own() {
        let mut r = StreamResampler::new(48_000, 2);
        let left = loud_end(16_000).samples;
        let stereo: Vec<i16> = left.iter().flat_map(|&l| [l, 0]).collect();

        let mut out = r.push(&PcmFrame::new(stereo, 16_000, 2)).expect("push");
        out.append(&r.flush().expect("flush"));

        assert_eq!((out.sample_rate, out.channels), (48_000, 2));
        let right: Vec<i16> = out.samples.iter().skip(1).step_by(2).copied().collect();
        assert!(
            peak(&out.samples) > 15_000,
            "the left channel lost its audio"
        );
        assert_eq!(peak(&right), 0, "the silent right channel picked up audio");
    }

    #[test]
    fn stream_downsample_halves_sample_count() {
        // 96k mono -> 48k mono ≈ half as many samples.
        let mut r = StreamResampler::new(48_000, 1);
        let input: Vec<i16> = (0..960).map(|i| (i % 100) as i16).collect();
        let frame = PcmFrame::mono(input, 96_000);
        let out = r.push(&frame).expect("push");
        // ~480 output samples (± a couple for boundary handling).
        assert!(
            (475..=480).contains(&out.frames()),
            "unexpected out len {}",
            out.frames()
        );
    }

    #[test]
    fn stream_upsample_is_continuous_across_blocks() {
        // 24k -> 48k across two blocks should roughly double total samples and
        // not panic on the boundary.
        let mut r = StreamResampler::new(48_000, 1);
        let block: Vec<i16> = (0..240).map(|i| i as i16).collect();
        let a = r
            .push(&PcmFrame::mono(block.clone(), 24_000))
            .expect("push");
        let b = r.push(&PcmFrame::mono(block, 24_000)).expect("push");
        assert!(!a.is_empty() && !b.is_empty());
        // 480 input samples @2x ≈ 960 output (allow slack for warm-up).
        let total = a.frames() + b.frames();
        assert!((950..=960).contains(&total), "unexpected total {total}");
    }
}
