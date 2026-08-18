//! Slicing a [`PcmFrame`] into the shapes analysis and streaming APIs expect.
//!
//! Ported from stream-py's `PcmData.chunks` / `sliding_window` / `tail` / `head`
//! / `append`. Every length here is counted in **frames** (samples per channel),
//! not raw interleaved samples, so the same numbers hold for mono and stereo.

use std::time::Duration;

use super::PcmFrame;

/// Where [`PcmFrame::head`] and [`PcmFrame::tail`] place zeros when the frame is
/// shorter than the requested duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pad {
    /// Do not pad; return whatever is available.
    None,
    /// Prepend zeros, so existing audio ends at the boundary.
    Start,
    /// Append zeros, so existing audio starts at the boundary.
    End,
}

impl PcmFrame {
    /// Split into chunks of `chunk_frames` samples per channel, advancing by
    /// `chunk_frames - overlap_frames` each step.
    ///
    /// The final chunk is shorter than `chunk_frames` unless `pad_last` zero-fills
    /// it. An `overlap_frames` at or above `chunk_frames` would never advance, so
    /// the step is floored at one frame.
    ///
    /// ```
    /// use getstream::rtc::PcmFrame;
    ///
    /// let pcm = PcmFrame::mono((0..10).collect(), 16_000);
    /// // [0..4], [2..6], [4..8], [6..10], [8..10]
    /// assert_eq!(pcm.chunks(4, 2, false).count(), 5);
    /// ```
    pub fn chunks(
        &self,
        chunk_frames: usize,
        overlap_frames: usize,
        pad_last: bool,
    ) -> impl Iterator<Item = PcmFrame> + '_ {
        let channels = self.channels.max(1) as usize;
        let total = self.frames();
        let chunk_frames = chunk_frames.max(1);
        let step = chunk_frames.saturating_sub(overlap_frames).max(1);

        (0..total).step_by(step).map_while(move |start| {
            let end = (start + chunk_frames).min(total);
            if start >= end {
                return None;
            }
            let mut samples = self.samples[start * channels..end * channels].to_vec();
            if pad_last {
                samples.resize(chunk_frames * channels, 0);
            }
            Some(PcmFrame::new(samples, self.sample_rate, self.channels))
        })
    }

    /// Sliding windows for analysis, sized and stepped in milliseconds.
    ///
    /// A 25 ms window with a 10 ms hop is the usual shape for feature extraction
    /// and voice-activity detection.
    pub fn sliding_windows(
        &self,
        window: Duration,
        hop: Duration,
        pad_last: bool,
    ) -> impl Iterator<Item = PcmFrame> + '_ {
        let window_frames = self.frames_in(window).max(1);
        let hop_frames = self.frames_in(hop).max(1);
        let overlap = window_frames.saturating_sub(hop_frames);
        self.chunks(window_frames, overlap, pad_last)
    }

    /// Keep the first `duration` of audio.
    ///
    /// When the frame is shorter than `duration`, `pad` decides whether the
    /// result is zero-filled to the full length and at which end.
    pub fn head(&self, duration: Duration, pad: Pad) -> PcmFrame {
        let target = self.frames_in(duration);
        let take = target.min(self.frames());
        let channels = self.channels.max(1) as usize;
        self.padded(self.samples[..take * channels].to_vec(), target, pad)
    }

    /// Keep the last `duration` of audio.
    ///
    /// When the frame is shorter than `duration`, `pad` decides whether the
    /// result is zero-filled to the full length and at which end. Padding at
    /// [`Pad::Start`] keeps the audio aligned to the end of the window, which is
    /// what a rolling "last N seconds" buffer wants.
    pub fn tail(&self, duration: Duration, pad: Pad) -> PcmFrame {
        let target = self.frames_in(duration);
        let frames = self.frames();
        let skip = frames.saturating_sub(target);
        let channels = self.channels.max(1) as usize;
        self.padded(self.samples[skip * channels..].to_vec(), target, pad)
    }

    /// Zero-fill `samples` up to `target` frames at the requested end.
    fn padded(&self, samples: Vec<i16>, target: usize, pad: Pad) -> PcmFrame {
        let channels = self.channels.max(1) as usize;
        let want = target * channels;
        let samples = match pad {
            Pad::None => samples,
            Pad::End => {
                let mut s = samples;
                s.resize(want, 0);
                s
            }
            Pad::Start => {
                if samples.len() >= want {
                    samples
                } else {
                    let mut s = vec![0; want - samples.len()];
                    s.extend_from_slice(&samples);
                    s
                }
            }
        };
        PcmFrame::new(samples, self.sample_rate, self.channels)
    }

    /// Append `other`, converting it to this frame's rate and channel count
    /// first if they differ.
    ///
    /// Conversion runs per block, so appending a long run of short blocks that
    /// need resampling will not track phase across them — feed those through a
    /// [`StreamResampler`](super::StreamResampler) instead and append the result.
    pub fn append(&mut self, other: &PcmFrame) -> &mut Self {
        if other.samples.is_empty() {
            return self;
        }
        if self.samples.is_empty() {
            self.samples = other.samples.clone();
            self.sample_rate = other.sample_rate;
            self.channels = other.channels;
            return self;
        }
        if other.sample_rate == self.sample_rate && other.channels == self.channels {
            self.samples.extend_from_slice(&other.samples);
        } else {
            let converted = super::Resampler::new(self.sample_rate, self.channels).resample(other);
            self.samples.extend_from_slice(&converted.samples);
        }
        self
    }

    /// Concatenate `frames` into one block at the first frame's rate and channel
    /// count. Returns an empty 48 kHz mono frame when the input is empty.
    pub fn concat<'a>(frames: impl IntoIterator<Item = &'a PcmFrame>) -> PcmFrame {
        let mut iter = frames.into_iter();
        let Some(first) = iter.next() else {
            return PcmFrame::mono(Vec::new(), super::OPUS_SAMPLE_RATE);
        };
        let mut out = first.clone();
        for f in iter {
            out.append(f);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: i16) -> Vec<i16> {
        (0..n).collect()
    }

    #[test]
    fn chunks_with_overlap_match_the_python_example() {
        // stream-py: pcm.chunks(4, overlap=2) over 10 samples yields
        // [0:4], [2:6], [4:8], [6:10], [8:10].
        let pcm = PcmFrame::mono(ramp(10), 16_000);
        let got: Vec<Vec<i16>> = pcm.chunks(4, 2, false).map(|c| c.samples).collect();
        assert_eq!(
            got,
            vec![
                vec![0, 1, 2, 3],
                vec![2, 3, 4, 5],
                vec![4, 5, 6, 7],
                vec![6, 7, 8, 9],
                vec![8, 9],
            ]
        );
    }

    #[test]
    fn pad_last_zero_fills_the_short_tail() {
        let pcm = PcmFrame::mono(ramp(10), 16_000);
        let last = pcm.chunks(4, 0, true).last().unwrap();
        assert_eq!(last.samples, vec![8, 9, 0, 0]);
    }

    #[test]
    fn chunks_count_frames_not_interleaved_samples() {
        // 4 stereo frames chunked 2 at a time -> 2 chunks of 4 samples each.
        let pcm = PcmFrame::new(vec![1, -1, 2, -2, 3, -3, 4, -4], 48_000, 2);
        let got: Vec<Vec<i16>> = pcm.chunks(2, 0, false).map(|c| c.samples).collect();
        assert_eq!(got, vec![vec![1, -1, 2, -2], vec![3, -3, 4, -4]]);
        assert!(pcm.chunks(2, 0, false).all(|c| c.frames() == 2));
    }

    #[test]
    fn overlap_at_or_above_chunk_size_still_advances() {
        let pcm = PcmFrame::mono(ramp(6), 16_000);
        // step floors at 1 frame rather than looping forever.
        assert_eq!(pcm.chunks(3, 5, false).count(), 6);
    }

    #[test]
    fn sliding_windows_match_the_python_example() {
        // stream-py: 800 samples @16k, 25 ms window (400), 10 ms hop (160) -> 5.
        let pcm = PcmFrame::mono(vec![0; 800], 16_000);
        let windows: Vec<PcmFrame> = pcm
            .sliding_windows(Duration::from_millis(25), Duration::from_millis(10), false)
            .collect();
        assert_eq!(windows.len(), 5);
        assert_eq!(windows[0].frames(), 400);
    }

    #[test]
    fn head_and_tail_take_from_the_right_end() {
        let pcm = PcmFrame::mono(ramp(10), 10); // 10 Hz -> 1 sample per 100 ms
        let head = pcm.head(Duration::from_millis(300), Pad::None);
        let tail = pcm.tail(Duration::from_millis(300), Pad::None);
        assert_eq!(head.samples, vec![0, 1, 2]);
        assert_eq!(tail.samples, vec![7, 8, 9]);
    }

    #[test]
    fn tail_pads_at_the_start_to_keep_audio_flush_with_the_window_end() {
        let pcm = PcmFrame::mono(vec![5, 6], 10);
        let padded = pcm.tail(Duration::from_millis(500), Pad::Start);
        assert_eq!(padded.samples, vec![0, 0, 0, 5, 6]);
        assert_eq!(padded.duration(), Duration::from_millis(500));
    }

    #[test]
    fn head_pads_at_the_end() {
        let pcm = PcmFrame::mono(vec![5, 6], 10);
        let padded = pcm.head(Duration::from_millis(500), Pad::End);
        assert_eq!(padded.samples, vec![5, 6, 0, 0, 0]);
    }

    #[test]
    fn unpadded_short_frame_stays_short() {
        let pcm = PcmFrame::mono(vec![5, 6], 10);
        assert_eq!(
            pcm.tail(Duration::from_secs(9), Pad::None).samples,
            vec![5, 6]
        );
    }

    #[test]
    fn append_concatenates_matching_layouts() {
        let mut a = PcmFrame::mono(vec![1, 2], 16_000);
        a.append(&PcmFrame::mono(vec![3, 4], 16_000));
        assert_eq!(a.samples, vec![1, 2, 3, 4]);
    }

    #[test]
    fn append_converts_a_mismatched_block_to_the_target_layout() {
        let mut a = PcmFrame::mono(vec![0; 480], 48_000);
        a.append(&PcmFrame::new(vec![0; 320], 16_000, 1));
        // 320 frames at 16k become 960 at 48k.
        assert_eq!(a.frames(), 480 + 960);
        assert_eq!(a.sample_rate, 48_000);
    }

    #[test]
    fn appending_to_an_empty_frame_adopts_the_incoming_layout() {
        let mut a = PcmFrame::mono(Vec::new(), 8_000);
        a.append(&PcmFrame::new(vec![1, 2, 3, 4], 48_000, 2));
        assert_eq!(a.sample_rate, 48_000);
        assert_eq!(a.channels, 2);
        assert_eq!(a.samples, vec![1, 2, 3, 4]);
    }

    #[test]
    fn concat_joins_a_run_of_blocks() {
        let blocks = [
            PcmFrame::mono(vec![1, 2], 16_000),
            PcmFrame::mono(vec![3], 16_000),
            PcmFrame::mono(vec![4, 5], 16_000),
        ];
        assert_eq!(PcmFrame::concat(&blocks).samples, vec![1, 2, 3, 4, 5]);
        assert!(PcmFrame::concat(std::iter::empty()).is_empty());
    }
}
