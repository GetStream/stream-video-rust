//! Opus encoder and decoder over the static libopus that `build.rs` links.
//!
//! The API is the one of the `opus` crate, which the SDK used before.

use std::ffi::{CStr, c_char, c_int};
use std::fmt;

/// 120 ms at 48 kHz, the longest Opus frame for one channel. `opus_decode`
/// keeps a stack array of `frame_size` samples for each channel, so a larger
/// `frame_size` can overflow the stack.
const MAX_FRAME_SAMPLES: usize = 5_760;

const OPUS_OK: c_int = 0;
const OPUS_BAD_ARG: c_int = -1;
const OPUS_AUTO: c_int = -1000;
const OPUS_BITRATE_MAX: c_int = -1;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_SET_INBAND_FEC_REQUEST: c_int = 4012;
const OPUS_SET_PACKET_LOSS_PERC_REQUEST: c_int = 4014;
const OPUS_SET_DTX_REQUEST: c_int = 4016;

#[repr(C)]
struct OpusEncoder {
    _private: [u8; 0],
}

#[repr(C)]
struct OpusDecoder {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn opus_encoder_create(
        fs: i32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut OpusEncoder;
    fn opus_encoder_destroy(st: *mut OpusEncoder);
    fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
    fn opus_encode(
        st: *mut OpusEncoder,
        pcm: *const i16,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: i32,
    ) -> i32;
    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
    fn opus_decoder_destroy(st: *mut OpusDecoder);
    fn opus_decode(
        st: *mut OpusDecoder,
        data: *const u8,
        len: i32,
        pcm: *mut i16,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    safe fn opus_strerror(error: c_int) -> *const c_char;
}

// The SDK uses only `Mono`, `Voip` and `Bits`. The other variants keep the API
// of the `opus` crate.

/// The channel count of an encoder or decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Channels {
    Mono = 1,
    #[allow(dead_code)]
    Stereo = 2,
}

/// The coding mode of an encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Application {
    Voip = 2048,
    #[allow(dead_code)]
    Audio = 2049,
    #[allow(dead_code)]
    LowDelay = 2051,
}

/// The bitrate of an encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bitrate {
    /// Bits per second.
    Bits(i32),
    /// The highest bitrate that the packet size allows.
    #[allow(dead_code)]
    Max,
    /// The encoder chooses the bitrate.
    #[allow(dead_code)]
    Auto,
}

impl Bitrate {
    fn raw(self) -> i32 {
        match self {
            Bitrate::Bits(bits) => bits,
            Bitrate::Max => OPUS_BITRATE_MAX,
            Bitrate::Auto => OPUS_AUTO,
        }
    }
}

/// A libopus error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Error(c_int);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: libopus returns a static NUL-terminated string for every code.
        let message = unsafe { CStr::from_ptr(opus_strerror(self.0)) };
        f.write_str(&message.to_string_lossy())
    }
}

/// Maps a libopus return value to its non-negative result or an [`Error`].
fn check(code: c_int) -> Result<usize, Error> {
    usize::try_from(code).map_err(|_| Error(code))
}

/// Converts a length or a sample rate to the C integer that libopus takes.
fn c_int_of(value: impl TryInto<c_int>) -> Result<c_int, Error> {
    value.try_into().map_err(|_| Error(OPUS_BAD_ARG))
}

pub(crate) struct Encoder {
    state: *mut OpusEncoder,
    channels: Channels,
}

// SAFETY: an encoder state has no thread affinity, and every call takes
// `&mut self`.
unsafe impl Send for Encoder {}

impl Encoder {
    pub(crate) fn new(
        sample_rate: u32,
        channels: Channels,
        mode: Application,
    ) -> Result<Self, Error> {
        let sample_rate = c_int_of(sample_rate)?;
        let mut error = OPUS_OK;
        // SAFETY: the arguments are plain integers, and `error` outlives the call.
        let state = unsafe {
            opus_encoder_create(sample_rate, channels as c_int, mode as c_int, &mut error)
        };
        check(error)?;
        Ok(Self { state, channels })
    }

    pub(crate) fn set_bitrate(&mut self, value: Bitrate) -> Result<(), Error> {
        self.ctl(OPUS_SET_BITRATE_REQUEST, value.raw())
    }

    pub(crate) fn set_inband_fec(&mut self, enabled: bool) -> Result<(), Error> {
        self.ctl(OPUS_SET_INBAND_FEC_REQUEST, i32::from(enabled))
    }

    pub(crate) fn set_packet_loss_perc(&mut self, percent: i32) -> Result<(), Error> {
        self.ctl(OPUS_SET_PACKET_LOSS_PERC_REQUEST, percent)
    }

    pub(crate) fn set_dtx(&mut self, enabled: bool) -> Result<(), Error> {
        self.ctl(OPUS_SET_DTX_REQUEST, i32::from(enabled))
    }

    /// Encodes one frame of interleaved PCM into `output` and returns the
    /// packet length.
    pub(crate) fn encode(&mut self, input: &[i16], output: &mut [u8]) -> Result<usize, Error> {
        let frame_size = c_int_of(input.len() / self.channels as usize)?;
        let max_data_bytes = c_int_of(output.len())?;
        // SAFETY: `self.state` is a live encoder, libopus reads `frame_size`
        // samples for each channel (at most `input.len()`), and the pointers
        // come from live slices.
        check(unsafe {
            opus_encode(
                self.state,
                input.as_ptr(),
                frame_size,
                output.as_mut_ptr(),
                max_data_bytes,
            )
        })
    }

    fn ctl(&mut self, request: c_int, value: i32) -> Result<(), Error> {
        // SAFETY: `self.state` is a live encoder, and every request here takes
        // one `opus_int32`.
        check(unsafe { opus_encoder_ctl(self.state, request, value) }).map(drop)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `self.state` came from `opus_encoder_create` and is freed only
        // here.
        unsafe { opus_encoder_destroy(self.state) }
    }
}

pub(crate) struct Decoder {
    state: *mut OpusDecoder,
    channels: Channels,
}

// SAFETY: a decoder state has no thread affinity, and every call takes
// `&mut self`.
unsafe impl Send for Decoder {}

impl Decoder {
    pub(crate) fn new(sample_rate: u32, channels: Channels) -> Result<Self, Error> {
        let sample_rate = c_int_of(sample_rate)?;
        let mut error = OPUS_OK;
        // SAFETY: the arguments are plain integers, and `error` outlives the call.
        let state = unsafe { opus_decoder_create(sample_rate, channels as c_int, &mut error) };
        check(error)?;
        Ok(Self { state, channels })
    }

    /// Decodes `input` into interleaved `output` and returns the number of
    /// samples for each channel, at most 120 ms. An empty `input` conceals a
    /// lost packet; `fec` decodes the in-band FEC copy of the previous frame
    /// that `input` carries.
    pub(crate) fn decode(
        &mut self,
        input: &[u8],
        output: &mut [i16],
        fec: bool,
    ) -> Result<usize, Error> {
        let len = c_int_of(input.len())?;
        let frame_size = c_int_of((output.len() / self.channels as usize).min(MAX_FRAME_SAMPLES))?;
        // SAFETY: `self.state` is a live decoder, the pointers come from live
        // slices, libopus writes at most `frame_size` samples for each channel
        // (at most `output.len()`), and `frame_size` is at most 120 ms.
        check(unsafe {
            opus_decode(
                self.state,
                input.as_ptr(),
                len,
                output.as_mut_ptr(),
                frame_size,
                c_int::from(fec),
            )
        })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: `self.state` came from `opus_decoder_create` and is freed only
        // here.
        unsafe { opus_decoder_destroy(self.state) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 20 ms of a 440 Hz tone at 48 kHz.
    fn tone_20ms() -> Vec<i16> {
        (0..960)
            .map(|i| {
                ((f64::from(i) * 440.0 * std::f64::consts::TAU / 48_000.0).sin() * 12_000.0) as i16
            })
            .collect()
    }

    /// The length of the tenth packet: the encoder needs some frames to reach
    /// the target bitrate.
    fn tenth_packet_len(bitrate: i32) -> usize {
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::Voip).expect("encoder");
        encoder
            .set_bitrate(Bitrate::Bits(bitrate))
            .expect("set bitrate");
        let pcm = tone_20ms();
        let mut packet = [0u8; 1_500];
        (0..10)
            .map(|_| encoder.encode(&pcm, &mut packet).expect("encode"))
            .last()
            .expect("ten packets")
    }

    #[test]
    fn a_lower_bitrate_gives_smaller_packets() {
        assert!(tenth_packet_len(8_000) < tenth_packet_len(64_000));
    }

    #[test]
    fn decode_into_a_long_buffer_gives_at_most_120_ms() {
        let mut decoder = Decoder::new(48_000, Channels::Mono).expect("decoder");
        // A multiple of 2.5 ms, larger than a test thread stack as floats.
        let mut output = vec![0i16; 1_000_080];

        let samples = decoder.decode(&[], &mut output, false).expect("conceal");

        assert!(samples <= 5_760);
    }

    #[test]
    fn a_stereo_frame_decodes_to_samples_per_channel() {
        let mut encoder =
            Encoder::new(48_000, Channels::Stereo, Application::Audio).expect("encoder");
        let mut decoder = Decoder::new(48_000, Channels::Stereo).expect("decoder");
        let pcm: Vec<i16> = tone_20ms().into_iter().flat_map(|s| [s, s]).collect();
        let mut packet = [0u8; 1_500];
        let mut output = vec![0i16; 2 * 5_760];

        let len = encoder.encode(&pcm, &mut packet).expect("encode");
        let samples = decoder
            .decode(&packet[..len], &mut output, false)
            .expect("decode");

        assert_eq!(samples, 960);
    }

    #[test]
    fn every_application_creates_an_encoder() {
        for application in [Application::Voip, Application::Audio, Application::LowDelay] {
            assert!(Encoder::new(48_000, Channels::Mono, application).is_ok());
        }
    }

    #[test]
    fn every_bitrate_setting_is_accepted() {
        let mut encoder = Encoder::new(48_000, Channels::Mono, Application::Voip).expect("encoder");
        for bitrate in [Bitrate::Bits(32_000), Bitrate::Max, Bitrate::Auto] {
            assert!(encoder.set_bitrate(bitrate).is_ok());
        }
    }
}
