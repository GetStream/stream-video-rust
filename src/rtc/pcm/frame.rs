//! The [`PcmFrame`] interchange type and the SFU's native audio constants.

use std::time::Duration;

/// The SFU's native audio sample rate (Opus internal clock).
pub const OPUS_SAMPLE_RATE: u32 = 48_000;
/// Samples per channel in a 20 ms frame at 48 kHz (the Opus frame we pace on).
pub const FRAME_SAMPLES_20MS: usize = (OPUS_SAMPLE_RATE as usize) / 50;

/// A block of interleaved 16-bit PCM samples.
///
/// `samples` is interleaved when `channels > 1` (L, R, L, R, …). This is the
/// public type produced by [`RemoteTrack::next_pcm`](crate::rtc::RemoteTrack)
/// and consumed by [`LocalAudioTrack::write_pcm`](crate::rtc::LocalAudioTrack).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmFrame {
    /// Interleaved 16-bit samples.
    pub samples: Vec<i16>,
    /// Samples per second (per channel).
    pub sample_rate: u32,
    /// Channel count (1 = mono, 2 = stereo).
    pub channels: u16,
}

impl PcmFrame {
    /// Build a frame from interleaved samples.
    pub fn new(samples: Vec<i16>, sample_rate: u32, channels: u16) -> Self {
        Self {
            samples,
            sample_rate,
            channels: channels.max(1),
        }
    }

    /// Build a mono frame.
    pub fn mono(samples: Vec<i16>, sample_rate: u32) -> Self {
        Self::new(samples, sample_rate, 1)
    }

    /// A silent frame of `frames` samples per channel.
    pub fn silence(frames: usize, sample_rate: u32, channels: u16) -> Self {
        let channels = channels.max(1);
        Self::new(vec![0; frames * channels as usize], sample_rate, channels)
    }

    /// Number of samples per channel.
    pub fn frames(&self) -> usize {
        self.samples.len() / (self.channels.max(1) as usize)
    }

    /// Whether the frame carries no samples.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Whether the frame carries two channels.
    pub fn is_stereo(&self) -> bool {
        self.channels == 2
    }

    /// The playback duration of this block.
    pub fn duration(&self) -> Duration {
        if self.sample_rate == 0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(self.frames() as f64 / f64::from(self.sample_rate))
    }

    /// The playback duration of this block in fractional milliseconds.
    pub fn duration_ms(&self) -> f64 {
        self.duration().as_secs_f64() * 1000.0
    }

    /// Root-mean-square amplitude across all samples, normalized to `[0, 1]`.
    ///
    /// Handy for asserting a republished stream is non-silent (energy above a
    /// small threshold) without pulling in a DSP crate.
    pub fn rms(&self) -> f64 {
        rms_i16(&self.samples)
    }

    /// Number of samples per channel a `duration` of audio occupies at this
    /// frame's sample rate.
    pub(crate) fn frames_in(&self, duration: Duration) -> usize {
        (duration.as_secs_f64() * f64::from(self.sample_rate)) as usize
    }
}

/// Root-mean-square amplitude of `samples`, normalized to `[0, 1]`.
///
/// The slice form lets the outbound pacer measure the 20 ms block it is about
/// to encode without building a [`PcmFrame`] around it.
pub(crate) fn rms_i16(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples
        .iter()
        .map(|&s| {
            let v = f64::from(s) / f64::from(i16::MAX);
            v * v
        })
        .sum();
    (sum_sq / samples.len() as f64).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_silence_is_zero_and_tone_is_positive() {
        assert_eq!(PcmFrame::mono(vec![0; 480], 48_000).rms(), 0.0);
        let tone: Vec<i16> = (0..480)
            .map(|i| ((i as f64 * 0.1).sin() * 10_000.0) as i16)
            .collect();
        assert!(PcmFrame::mono(tone, 48_000).rms() > 0.05);
    }

    #[test]
    fn duration_of_20ms_frame() {
        let f = PcmFrame::mono(vec![0; FRAME_SAMPLES_20MS], OPUS_SAMPLE_RATE);
        assert_eq!(f.duration(), Duration::from_millis(20));
        assert_eq!(f.duration_ms(), 20.0);
    }

    #[test]
    fn frames_counts_per_channel_not_total_samples() {
        let stereo = PcmFrame::new(vec![1, 2, 3, 4, 5, 6], 48_000, 2);
        assert_eq!(stereo.frames(), 3);
        assert!(stereo.is_stereo());
        assert_eq!(stereo.duration_ms(), 3.0 / 48.0);
    }

    #[test]
    fn silence_is_sized_per_channel() {
        let s = PcmFrame::silence(480, 48_000, 2);
        assert_eq!(s.samples.len(), 960);
        assert_eq!(s.frames(), 480);
        assert_eq!(s.rms(), 0.0);
    }
}
