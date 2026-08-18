//! Thin, safe wrapper over the maintained [`opusic-sys`] libopus binding.
//!
//! The SDK only needs a narrow slice of libopus: a mono 48 kHz VoIP encoder for
//! the publish path and a mono 48 kHz decoder for the subscribe path. This module
//! owns the raw `OpusEncoder`/`OpusDecoder` handles behind RAII guards so the C
//! resources are always released, and turns libopus's negative status codes into
//! descriptive errors. It replaces the unmaintained `audiopus_sys`
//! (RUSTSEC-2026-0150) with a binding that tracks upstream libopus.
//!
//! [`opusic-sys`]: https://crates.io/crates/opusic-sys

use core::ffi::{CStr, c_int};

use opusic_sys as sys;

/// The single sample rate the SDK's Opus path uses (matches the SFU codec and
/// [`super::pcm::OPUS_SAMPLE_RATE`]). Kept local so this wrapper is self-contained
/// and can be compiled standalone by the benches.
const OPUS_SAMPLE_RATE_HZ: sys::opus_int32 = 48_000;

/// Render a libopus status code as a human-readable string.
fn opus_strerror(code: c_int) -> String {
    // SAFETY: `opus_strerror` returns a pointer to a static, NUL-terminated C
    // string for any input, so the pointer is valid and lives for the program.
    let ptr = unsafe { sys::opus_strerror(code) };
    if ptr.is_null() {
        return format!("opus error {code}");
    }
    // SAFETY: `ptr` is non-null and points at a static NUL-terminated string.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn frame_size_arg(samples: usize, what: &str) -> Result<c_int, String> {
    c_int::try_from(samples).map_err(|_| format!("opus {what} frame too large: {samples} samples"))
}

fn max_bytes_arg(bytes: usize) -> Result<i32, String> {
    i32::try_from(bytes).map_err(|_| format!("opus output buffer too large: {bytes} bytes"))
}

/// A mono 48 kHz Opus encoder configured for interactive voice.
///
/// libopus mutates encoder state on every `encode`, so the methods take `&mut
/// self`; callers serialize access (the media path holds it in a mutex).
pub struct Encoder {
    raw: *mut sys::OpusEncoder,
}

// SAFETY: `OpusEncoder` is a self-contained heap allocation with no thread
// affinity; libopus permits moving an instance between threads. It is *not*
// `Sync` (concurrent `opus_encode` on one instance is UB), so we deliberately do
// not implement `Sync` — callers guard shared access with a mutex.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Create a mono 48 kHz encoder tuned for VoIP (matches the SFU Opus codec).
    ///
    /// # Errors
    ///
    /// Returns the libopus status string if the encoder cannot be created.
    pub fn new_voip_mono() -> Result<Self, String> {
        let mut error: c_int = sys::OPUS_OK;
        // SAFETY: all scalar args are valid libopus inputs; `error` is a valid
        // out-pointer. On success the returned pointer owns a fresh encoder that
        // we free in `Drop`.
        let raw = unsafe {
            sys::opus_encoder_create(
                OPUS_SAMPLE_RATE_HZ,
                1,
                sys::OPUS_APPLICATION_VOIP,
                &raw mut error,
            )
        };
        if error != sys::OPUS_OK || raw.is_null() {
            return Err(format!("opus encoder init: {}", opus_strerror(error)));
        }
        Ok(Self { raw })
    }

    /// Encode one mono frame of 16-bit PCM into `output`, returning the byte
    /// length of the Opus packet.
    ///
    /// `pcm.len()` must be a valid Opus frame size for 48 kHz (e.g. 960 for the
    /// 20 ms media frame).
    ///
    /// # Errors
    ///
    /// Returns the libopus status string if encoding fails or the frame size is
    /// not representable.
    pub fn encode(&mut self, pcm: &[i16], output: &mut [u8]) -> Result<usize, String> {
        let frame_size = frame_size_arg(pcm.len(), "encode")?;
        let max_bytes = max_bytes_arg(output.len())?;
        // SAFETY: `self.raw` is a live encoder; `pcm`/`output` are valid slices
        // whose lengths we pass as the frame size and capacity, so libopus reads
        // exactly `frame_size` samples and writes at most `max_bytes`.
        let written = unsafe {
            sys::opus_encode(
                self.raw,
                pcm.as_ptr(),
                frame_size,
                output.as_mut_ptr(),
                max_bytes,
            )
        };
        if written < 0 {
            return Err(format!("opus encode: {}", opus_strerror(written)));
        }
        Ok(written as usize)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `self.raw` was created by `opus_encoder_create` and is freed
        // exactly once here; no other code frees or uses it afterwards.
        unsafe { sys::opus_encoder_destroy(self.raw) };
    }
}

/// A mono 48 kHz Opus decoder.
pub struct Decoder {
    raw: *mut sys::OpusDecoder,
}

// SAFETY: see `Encoder` — an `OpusDecoder` may move between threads but is not
// safe for concurrent use, so it is `Send` but not `Sync`.
unsafe impl Send for Decoder {}

impl Decoder {
    /// Create a mono 48 kHz decoder.
    ///
    /// # Errors
    ///
    /// Returns the libopus status string if the decoder cannot be created.
    pub fn new_mono() -> Result<Self, String> {
        let mut error: c_int = sys::OPUS_OK;
        // SAFETY: valid scalar args and a valid `error` out-pointer; a non-null
        // result owns a decoder freed in `Drop`.
        let raw = unsafe { sys::opus_decoder_create(OPUS_SAMPLE_RATE_HZ, 1, &raw mut error) };
        if error != sys::OPUS_OK || raw.is_null() {
            return Err(format!("opus decoder init: {}", opus_strerror(error)));
        }
        Ok(Self { raw })
    }

    /// Decode one Opus packet into `output`, returning the number of mono samples
    /// written. `output.len()` bounds the samples produced.
    ///
    /// # Errors
    ///
    /// Returns the libopus status string if decoding fails or the frame size is
    /// not representable.
    pub fn decode(
        &mut self,
        packet: &[u8],
        output: &mut [i16],
        fec: bool,
    ) -> Result<usize, String> {
        let frame_size = frame_size_arg(output.len(), "decode")?;
        let len = max_bytes_arg(packet.len())?;
        // SAFETY: `self.raw` is a live decoder; `packet`/`output` are valid
        // slices whose lengths bound the read and the write. libopus writes at
        // most `frame_size` samples into `output`.
        let samples = unsafe {
            sys::opus_decode(
                self.raw,
                packet.as_ptr(),
                len,
                output.as_mut_ptr(),
                frame_size,
                c_int::from(fec),
            )
        };
        if samples < 0 {
            return Err(format!("opus decode: {}", opus_strerror(samples)));
        }
        Ok(samples as usize)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: `self.raw` was created by `opus_decoder_create` and is freed
        // exactly once here.
        unsafe { sys::opus_decoder_destroy(self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn tone_20ms() -> Vec<i16> {
        let frame = OPUS_SAMPLE_RATE_HZ as usize / 50;
        (0..frame)
            .map(|i| {
                let t = i as f64 / f64::from(OPUS_SAMPLE_RATE_HZ);
                (12_000.0 * (2.0 * PI * 440.0 * t).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn encode_then_decode_round_trips_a_tone() {
        let mut encoder = Encoder::new_voip_mono().expect("encoder");
        let mut decoder = Decoder::new_mono().expect("decoder");
        let pcm = tone_20ms();
        let mut packet = vec![0u8; 4_000];

        let bytes = encoder.encode(&pcm, &mut packet).expect("encode");
        assert!(bytes > 0 && bytes <= packet.len(), "packet length {bytes}");

        let mut out = vec![0i16; 5_760];
        let samples = decoder
            .decode(&packet[..bytes], &mut out, false)
            .expect("decode");
        assert_eq!(samples, pcm.len(), "decoded sample count matches the frame");
    }

    #[test]
    fn encode_rejects_a_bogus_frame_size() {
        let mut encoder = Encoder::new_voip_mono().expect("encoder");
        let mut packet = vec![0u8; 4_000];
        // 137 samples is not a valid 48 kHz Opus frame size.
        let err = encoder
            .encode(&vec![0i16; 137], &mut packet)
            .expect_err("bogus frame size must be rejected");
        assert!(err.contains("opus encode"), "error was: {err}");
    }
}
