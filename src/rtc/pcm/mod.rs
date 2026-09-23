//! Raw PCM audio ([`PcmFrame`]) and the audio utilities that surround it.
//!
//! [`PcmFrame`] is the public interchange type for the PCM republish path — the
//! Rust analog of stream-py's `PcmData` (`samples`, `sample_rate`, `channels`).
//! Samples are interleaved signed 16-bit, the format Opus decodes to and the
//! SFU speaks natively, so the common path never converts.
//!
//! The rest of the module covers what an agent pipeline needs on either side of
//! that path:
//!
//! - [`convert`] — 32-bit float conversion for model APIs that want `[-1, 1]`,
//!   raw little-endian bytes, WAV containers, and G.711 μ-law / A-law.
//! - [`resample`] — [`Resampler`] converts one independent block to a target
//!   rate and channel count; [`StreamResampler`] carries state across blocks for
//!   a continuous stream.
//! - [`chunk`] — fixed-size chunking with overlap, millisecond sliding windows,
//!   [`PcmFrame::head`] / [`PcmFrame::tail`], and concatenation.
//!
//! These are ported from stream-py's `getstream/video/rtc/track_util.py`; the
//! numeric behavior matches it, including exact resampled block lengths.
//!
//! # Preparing track audio for another service
//!
//! ```
//! use std::time::Duration;
//!
//! use getstream::rtc::{G711Mapping, Pad, PcmFrame, Resampler};
//!
//! // A 20 ms stereo block off a remote track, at the SFU's native rate.
//! let track_audio = PcmFrame::new(vec![0; 960 * 2], 48_000, 2);
//!
//! // Rate and channel conversion. Each block converts on its own, so 20 ms in
//! // is 20 ms out: 960 frames at 48 kHz become exactly 320 at 16 kHz.
//! let frame = Resampler::new(16_000, 1).resample(&track_audio);
//! assert_eq!(frame.frames(), 320);
//!
//! // Whatever representation the far side wants.
//! let floats: Vec<f32> = frame.to_f32();
//! let wav: Vec<u8> = frame.to_wav_bytes();
//! let telephony: Vec<u8> = frame.to_g711(G711Mapping::Mulaw);
//! assert_eq!(floats.len(), 320);
//! assert_eq!(wav.len(), 44 + 320 * 2);
//! assert_eq!(telephony.len(), 320);
//!
//! // Fixed-size chunks, or overlapping windows for VAD and feature extraction.
//! assert_eq!(frame.chunks(160, 0, true).count(), 2);
//! let windows = frame.sliding_windows(
//!     Duration::from_millis(25),
//!     Duration::from_millis(10),
//!     false,
//! );
//! assert_eq!(windows.count(), 2);
//!
//! // A rolling "last N seconds" buffer, zero-padded at the front while it fills.
//! let recent = frame.tail(Duration::from_secs(1), Pad::Start);
//! assert_eq!(recent.duration(), Duration::from_secs(1));
//! ```
//!
//! [`Resampler`] converts one block at a time and is the right choice when a
//! provider expects exact frame sizes. For a continuous stream whose block
//! boundaries are arbitrary, [`StreamResampler`] carries interpolation state
//! across calls so blocks join without clicks; that is what the publish path
//! uses internally.
//!
//! G.711 companding matches FFmpeg's `pcm_mulaw` and `pcm_alaw` byte for byte,
//! so a Rust agent and a Python agent emit identical output for identical input.
//! Resample to [`convert::G711_SAMPLE_RATE`] first — companding does not change
//! the rate.

pub mod chunk;
pub mod convert;
mod frame;
pub mod resample;

pub use convert::G711Mapping;
pub(crate) use frame::rms_i16;
pub use frame::{FRAME_SAMPLES_20MS, OPUS_SAMPLE_RATE, PcmFrame};
pub use resample::{Resampler, StreamResampler};
