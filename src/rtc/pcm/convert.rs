//! Converting a [`PcmFrame`] to and from the representations other systems want.
//!
//! Ported from stream-py's `PcmData.to_float32` / `to_int16` / `from_bytes` /
//! `to_bytes` / `to_wav_bytes` / `from_g711` / `g711_bytes`, with the same
//! scaling constants so both SDKs produce identical bytes for identical input.
//!
//! Scaling is asymmetric on purpose, matching stream-py and the wider audio
//! ecosystem: s16 → f32 divides by 32768 so the range fits inside `[-1, 1)`,
//! while f32 → s16 clamps to `[-1, 1]` and multiplies by 32767 so `1.0` maps to
//! [`i16::MAX`] rather than wrapping.

use super::PcmFrame;

/// Which G.711 companding law to use.
///
/// μ-law is standard in North America and Japan and is what Twilio Media
/// Streams sends; A-law is standard in Europe and most of the rest of the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum G711Mapping {
    /// μ-law (ITU-T G.711 μ), the North American and Japanese standard.
    Mulaw,
    /// A-law (ITU-T G.711 A), the European and international standard.
    Alaw,
}

/// The sample rate G.711 is defined at. Telephony audio arrives at this rate.
pub const G711_SAMPLE_RATE: u32 = 8_000;

impl PcmFrame {
    /// Convert samples to 32-bit float in `[-1, 1)`, the range most model APIs
    /// and DSP libraries expect.
    pub fn to_f32(&self) -> Vec<f32> {
        self.samples
            .iter()
            .map(|&s| f32::from(s) / 32_768.0)
            .collect()
    }

    /// Build a frame from interleaved 32-bit float samples in `[-1, 1]`.
    ///
    /// Values outside the range are clamped rather than wrapped, so a hot signal
    /// clips instead of inverting polarity.
    pub fn from_f32(samples: &[f32], sample_rate: u32, channels: u16) -> Self {
        let s16 = samples
            .iter()
            .map(|&v| (v.clamp(-1.0, 1.0) * 32_767.0) as i16)
            .collect();
        Self::new(s16, sample_rate, channels)
    }

    /// Interleaved little-endian `s16` bytes — the wire format for most
    /// streaming speech APIs.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.samples.len() * 2);
        for &s in &self.samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    /// Build a frame from interleaved little-endian `s16` bytes.
    ///
    /// A trailing odd byte cannot form a sample and is dropped, so a buffer
    /// split mid-sample by a transport does not shift every following sample by
    /// one byte.
    pub fn from_bytes(bytes: &[u8], sample_rate: u32, channels: u16) -> Self {
        let samples = bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        Self::new(samples, sample_rate, channels)
    }

    /// Encode as a RIFF/WAVE container: a 44-byte header followed by the same
    /// bytes [`PcmFrame::to_bytes`] produces.
    ///
    /// Useful for batch speech-to-text endpoints that take a file upload, and
    /// for capturing a fixture while debugging a media path.
    pub fn to_wav_bytes(&self) -> Vec<u8> {
        const HEADER_LEN: u32 = 36;
        const PCM_FORMAT: u16 = 1;
        const BITS_PER_SAMPLE: u16 = 16;

        let channels = self.channels.max(1);
        let block_align = channels * BITS_PER_SAMPLE / 8;
        let byte_rate = self.sample_rate * u32::from(block_align);
        let data = self.to_bytes();
        // A WAV chunk length is a u32; saturate rather than wrap on absurd input.
        let data_len = u32::try_from(data.len()).unwrap_or(u32::MAX);

        let mut out = Vec::with_capacity(44 + data.len());
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&HEADER_LEN.saturating_add(data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");

        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk length
        out.extend_from_slice(&PCM_FORMAT.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());

        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    /// Decode G.711 companded bytes, one byte per sample.
    ///
    /// Telephony carries 8 kHz audio, which is the default `sample_rate`
    /// argument's usual value ([`G711_SAMPLE_RATE`]). Sources that deliver
    /// base64 (Twilio Media Streams, for one) should decode that layer first.
    pub fn from_g711(bytes: &[u8], sample_rate: u32, channels: u16, mapping: G711Mapping) -> Self {
        let decode = match mapping {
            G711Mapping::Mulaw => mulaw_to_linear,
            G711Mapping::Alaw => alaw_to_linear,
        };
        Self::new(
            bytes.iter().copied().map(decode).collect(),
            sample_rate,
            channels,
        )
    }

    /// Encode to G.711 companded bytes, one byte per sample.
    ///
    /// Samples are companded as they are: resample to [`G711_SAMPLE_RATE`] with
    /// [`Resampler`](super::Resampler) first if the frame is not already at the
    /// telephony rate.
    pub fn to_g711(&self, mapping: G711Mapping) -> Vec<u8> {
        let encode = match mapping {
            G711Mapping::Mulaw => linear_to_mulaw,
            G711Mapping::Alaw => linear_to_alaw,
        };
        self.samples.iter().copied().map(encode).collect()
    }
}

/// Bias added to the linear code before μ-law segmentation (ITU-T G.711).
const MULAW_BIAS: i32 = 0x84;

/// The inversion mask each law applies to its code word: μ-law flips every bit,
/// A-law flips the sign bit and alternating magnitude bits.
const MULAW_MASK: u8 = 0xFF;
const ALAW_MASK: u8 = 0xD5;

/// ITU-T G.711 μ-law expansion: one companded byte to 16-bit linear.
const fn mulaw_to_linear(byte: u8) -> i16 {
    let val = !byte;
    let t = (((val & 0x0F) as i32) << 3) + MULAW_BIAS;
    let t = t << (((val & 0x70) >> 4) as u32);
    // Peak magnitude is 32124, so the result always fits in i16.
    if val & 0x80 != 0 {
        (MULAW_BIAS - t) as i16
    } else {
        (t - MULAW_BIAS) as i16
    }
}

/// ITU-T G.711 A-law expansion: one companded byte to 16-bit linear.
///
/// The `2t + 1` form places each code at the centre of its quantization
/// interval rather than its lower edge.
const fn alaw_to_linear(byte: u8) -> i16 {
    let val = byte ^ 0x55;
    let t = (val & 0x0F) as i32;
    let seg = ((val & 0x70) >> 4) as u32;
    let t = if seg > 0 {
        (t + t + 1 + 32) << (seg + 2)
    } else {
        (t + t + 1) << 3
    };
    // Peak magnitude is 32256, so the result always fits in i16.
    if val & 0x80 != 0 { t as i16 } else { -t as i16 }
}

/// Compression tables mapping a 14-bit linear code to its companded byte.
///
/// Compression is the inverse of expansion: each linear value takes the code
/// whose *expanded* value is nearest, with segment boundaries at the midpoint
/// between neighbouring codes. That is not the same as the truncating
/// `linear2ulaw` in the ITU-T reference — it differs at ~1% of codes, all at
/// segment boundaries — and it is what FFmpeg's `pcm_mulaw` / `pcm_alaw` do.
/// Matching FFmpeg means this SDK and stream-py (which companders through PyAV)
/// emit byte-identical G.711 for identical input.
///
/// Built at compile time, indexed by `(sample + 32768) >> 2`.
const MULAW_COMPRESS: [u8; 16384] = build_compress_table(MULAW_MASK, Law::Mulaw);
const ALAW_COMPRESS: [u8; 16384] = build_compress_table(ALAW_MASK, Law::Alaw);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Law {
    Mulaw,
    Alaw,
}

const fn expand(byte: u8, law: Law) -> i32 {
    match law {
        Law::Mulaw => mulaw_to_linear(byte) as i32,
        Law::Alaw => alaw_to_linear(byte) as i32,
    }
}

/// Walk the 127 positive codes, and for each pair of neighbours fill every
/// linear value below their midpoint with the lower code. Values past the last
/// midpoint saturate to code 127.
const fn build_compress_table(mask: u8, law: Law) -> [u8; 16384] {
    let mut table = [0u8; 16384];
    table[8192] = mask;

    let mut j: i32 = 1;
    let mut i: i32 = 0;
    while i < 127 {
        let v1 = expand((i as u8) ^ mask, law);
        let v2 = expand(((i + 1) as u8) ^ mask, law);
        // Midpoint of the two expanded values, in the >> 2 index domain.
        let boundary = (v1 + v2 + 4) >> 3;
        while j < boundary {
            table[(8192 - j) as usize] = (i as u8) ^ (mask ^ 0x80);
            table[(8192 + j) as usize] = (i as u8) ^ mask;
            j += 1;
        }
        i += 1;
    }
    while j < 8192 {
        table[(8192 - j) as usize] = 127u8 ^ (mask ^ 0x80);
        table[(8192 + j) as usize] = 127u8 ^ mask;
        j += 1;
    }
    table[0] = table[1];
    table
}

/// ITU-T G.711 μ-law compression: 16-bit linear to one companded byte.
fn linear_to_mulaw(pcm: i16) -> u8 {
    MULAW_COMPRESS[((i32::from(pcm) + 32_768) >> 2) as usize]
}

/// ITU-T G.711 A-law compression: 16-bit linear to one companded byte.
fn linear_to_alaw(pcm: i16) -> u8 {
    ALAW_COMPRESS[((i32::from(pcm) + 32_768) >> 2) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// μ-law's second encoding of zero. Compression never emits it, but a
    /// decoder must accept it.
    const MULAW_NEGATIVE_ZERO: u8 = 0x7F;

    #[test]
    fn f32_round_trip_preserves_samples_within_one_step() {
        let frame = PcmFrame::mono(vec![0, 1, -1, 1000, -1000, i16::MAX, i16::MIN], 48_000);
        let back = PcmFrame::from_f32(&frame.to_f32(), 48_000, 1);
        for (a, b) in frame.samples.iter().zip(&back.samples) {
            assert!((a - b).abs() <= 1, "{a} != {b}");
        }
    }

    #[test]
    fn f32_conversion_uses_the_documented_asymmetric_scaling() {
        // s16 -> f32 divides by 32768; f32 -> s16 multiplies by 32767.
        assert_eq!(
            PcmFrame::mono(vec![32_767], 8_000).to_f32()[0],
            32_767.0 / 32_768.0
        );
        assert_eq!(PcmFrame::from_f32(&[1.0], 8_000, 1).samples[0], i16::MAX);
        assert_eq!(PcmFrame::from_f32(&[-1.0], 8_000, 1).samples[0], -32_767);
    }

    #[test]
    fn from_f32_clamps_out_of_range_input() {
        let f = PcmFrame::from_f32(&[9.0, -9.0], 8_000, 1);
        assert_eq!(f.samples, vec![i16::MAX, -32_767]);
    }

    #[test]
    fn bytes_round_trip_little_endian() {
        let frame = PcmFrame::new(vec![1, -2, 300, -400], 48_000, 2);
        let bytes = frame.to_bytes();
        assert_eq!(&bytes[..2], &1i16.to_le_bytes());
        let back = PcmFrame::from_bytes(&bytes, 48_000, 2);
        assert_eq!(back, frame);
    }

    #[test]
    fn from_bytes_drops_a_trailing_partial_sample() {
        // Three bytes carry one whole sample; the stray byte must not shift it.
        let f = PcmFrame::from_bytes(&[0x01, 0x00, 0x7F], 8_000, 1);
        assert_eq!(f.samples, vec![1]);
    }

    #[test]
    fn wav_header_describes_the_payload() {
        let frame = PcmFrame::new(vec![0; 200], 16_000, 2);
        let wav = frame.to_wav_bytes();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + 400);

        let field = |at: usize| u32::from_le_bytes(wav[at..at + 4].try_into().unwrap());
        let field16 = |at: usize| u16::from_le_bytes(wav[at..at + 2].try_into().unwrap());
        assert_eq!(field(4) as usize, wav.len() - 8); // RIFF size excludes its own 8 bytes
        assert_eq!(field16(20), 1); // PCM
        assert_eq!(field16(22), 2); // channels
        assert_eq!(field(24), 16_000); // sample rate
        assert_eq!(field(28), 16_000 * 4); // byte rate = rate * channels * 2
        assert_eq!(field16(32), 4); // block align
        assert_eq!(field16(34), 16); // bits per sample
        assert_eq!(field(40) as usize, 400); // data size
    }

    /// Every companded byte must survive expand-then-compress unchanged.
    /// Decoding lands on a segment's quantization point, so re-encoding has to
    /// return the same byte — this pins both directions against each other.
    ///
    /// μ-law's `0x7F` is the one exception, covered by the test below.
    #[test]
    fn g711_decode_encode_is_stable_for_every_byte() {
        for byte in 0..=u8::MAX {
            if byte != MULAW_NEGATIVE_ZERO {
                assert_eq!(
                    linear_to_mulaw(mulaw_to_linear(byte)),
                    byte,
                    "mulaw byte {byte:#04x}"
                );
            }
            assert_eq!(
                linear_to_alaw(alaw_to_linear(byte)),
                byte,
                "alaw byte {byte:#04x}"
            );
        }
    }

    /// μ-law encodes zero twice: `0xFF` and `0x7F` both expand to linear 0, so
    /// compression cannot round-trip both. The encoder emits `0xFF`, matching
    /// the ITU-T reference implementation and FFmpeg's `pcm_mulaw`. A-law has no
    /// such pair — its negative branch maps through `-val - 1`.
    #[test]
    fn mulaw_has_a_negative_zero_that_encodes_to_positive_zero() {
        assert_eq!(mulaw_to_linear(MULAW_NEGATIVE_ZERO), 0);
        assert_eq!(mulaw_to_linear(0xFF), 0);
        assert_eq!(linear_to_mulaw(0), 0xFF);
        assert_ne!(alaw_to_linear(0xD5), alaw_to_linear(0x55));
    }

    #[test]
    fn g711_round_trip_stays_within_companding_error() {
        // G.711 is lossy; the guarantee is bounded relative error, not equality.
        for law in [G711Mapping::Mulaw, G711Mapping::Alaw] {
            let input: Vec<i16> = (-32_000..32_000).step_by(97).collect();
            let frame = PcmFrame::mono(input.clone(), G711_SAMPLE_RATE);
            let back = PcmFrame::from_g711(&frame.to_g711(law), G711_SAMPLE_RATE, 1, law);
            assert_eq!(back.samples.len(), input.len());
            for (a, b) in input.iter().zip(&back.samples) {
                let err = (i32::from(*a) - i32::from(*b)).abs();
                let tolerance = (i32::from(a.abs()) / 16).max(256);
                assert!(err <= tolerance, "{law:?}: {a} -> {b} (err {err})");
            }
        }
    }

    #[test]
    fn g711_anchors_match_the_itu_tables() {
        // Fixed points from the ITU-T G.711 reference tables.
        assert_eq!(mulaw_to_linear(0xFF), 0);
        assert_eq!(mulaw_to_linear(0x7F), 0);
        assert_eq!(mulaw_to_linear(0x00), -32_124);
        assert_eq!(mulaw_to_linear(0x80), 32_124);
        assert_eq!(alaw_to_linear(0xD5), 8);
        assert_eq!(alaw_to_linear(0x55), -8);
        assert_eq!(alaw_to_linear(0x2A), -32_256);
        assert_eq!(alaw_to_linear(0xAA), 32_256);
    }

    #[test]
    fn g711_encodes_one_byte_per_sample() {
        let frame = PcmFrame::new(vec![0; 320], G711_SAMPLE_RATE, 2);
        assert_eq!(frame.to_g711(G711Mapping::Mulaw).len(), 320);
        assert_eq!(frame.to_g711(G711Mapping::Alaw).len(), 320);
    }

    #[test]
    fn silence_companded_and_expanded_stays_quiet() {
        let quiet = PcmFrame::mono(vec![0; 160], G711_SAMPLE_RATE);
        for law in [G711Mapping::Mulaw, G711Mapping::Alaw] {
            let back = PcmFrame::from_g711(&quiet.to_g711(law), G711_SAMPLE_RATE, 1, law);
            assert!(back.rms() < 0.001, "{law:?} introduced noise into silence");
        }
    }
}
