//! Minimal libvpx VP8/VP9 decoder for the inbound video path.
//!
//! [`RemoteTrack::next_video_frame`](super::remote_track::RemoteTrack::next_video_frame)
//! needs raw frames, not RTP: a bot that wants to *see* the call has to turn
//! reassembled VP8/VP9 samples into pixels. This is the mirror of
//! [`vpx`](super::vpx), which binds the encoder for the outbound path — the
//! decoder symbols come from the same `env-libvpx-sys` bindings, so supporting
//! inbound video adds no new native dependency.
//!
//! The libvpx C API is inherently unsafe; every FFI call is wrapped here and the
//! module surface is safe. libvpx decoder contexts are not thread-*affine*, so
//! [`VpxDecoder`] is `Send` (accessed under a mutex by the caller) but not
//! `Sync`.

use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_uint};
use std::ptr;

use vpx_sys::vpx_img_fmt::VPX_IMG_FMT_I420;
use vpx_sys::*;

use super::error::{Result, RtcError};
use super::video_frame::{VideoFrame, i420_len};
use super::vpx::{VpxCodec, check};

/// Decoder worker threads. Two is enough to keep up with the 360p–720p a bot
/// subscribes to without competing with the rest of the runtime for cores.
const DECODE_THREADS: c_uint = 2;

/// A libvpx VP8/VP9 decoder producing packed I420 [`VideoFrame`]s.
pub(crate) struct VpxDecoder {
    ctx: vpx_codec_ctx_t,
}

// SAFETY: `vpx_codec_ctx_t` holds raw pointers into libvpx internal state, which
// is why the auto `Send` impl is withheld. libvpx does not pin a context to the
// thread that created it — it only forbids *concurrent* use. Callers always hold
// the decoder behind a `std::sync::Mutex`, which serializes access, so moving
// the value across threads is sound.
unsafe impl Send for VpxDecoder {}

impl VpxDecoder {
    /// Create a decoder for `codec`. Frame size is learned from the bitstream,
    /// so no dimensions are needed up front (`w`/`h` of 0).
    pub(crate) fn new(codec: VpxCodec) -> Result<Self> {
        // SAFETY: `iface` is a static libvpx interface pointer. `cfg` is fully
        // initialized by us before the call, and `ctx` is left uninitialized and
        // populated by `vpx_codec_dec_init_ver` before `assume_init` reads it.
        // All pointers are valid for the call's scope.
        unsafe {
            let iface = match codec {
                VpxCodec::Vp8 => vpx_codec_vp8_dx(),
                VpxCodec::Vp9 => vpx_codec_vp9_dx(),
            };
            if iface.is_null() {
                return Err(RtcError::Media(
                    "libvpx decoder interface unavailable".to_owned(),
                ));
            }

            let cfg = vpx_codec_dec_cfg_t {
                threads: DECODE_THREADS,
                w: 0,
                h: 0,
            };

            let mut ctx = MaybeUninit::<vpx_codec_ctx_t>::uninit();
            check(
                vpx_codec_dec_init_ver(
                    ctx.as_mut_ptr(),
                    iface,
                    &cfg,
                    VPX_CODEC_USE_FRAME_THREADING as vpx_codec_flags_t,
                    VPX_DECODER_ABI_VERSION as c_int,
                ),
                "dec_init",
            )?;
            Ok(Self {
                ctx: ctx.assume_init(),
            })
        }
    }

    /// Decode one reassembled VP8/VP9 frame, returning every picture libvpx
    /// produced. `rtp_timestamp` is copied onto each frame.
    ///
    /// An empty result is normal, not an error: with frame threading libvpx may
    /// buffer briefly at the start of a stream, and a frame that only updates
    /// reference state yields no picture.
    pub(crate) fn decode(&mut self, data: &[u8], rtp_timestamp: u32) -> Result<Vec<VideoFrame>> {
        if data.is_empty() {
            return Ok(Vec::new());
        }

        // SAFETY: `data` is a valid slice libvpx only reads for the duration of
        // the call. The iterator-driven drain below operates on our owned
        // context, and each `vpx_image_t` is copied into an owned buffer before
        // the next call can recycle it.
        unsafe {
            check(
                vpx_codec_decode(
                    &mut self.ctx,
                    data.as_ptr(),
                    data.len() as c_uint,
                    ptr::null_mut(),
                    0,
                ),
                "decode",
            )?;

            let mut frames = Vec::new();
            let mut iter: vpx_codec_iter_t = ptr::null();
            loop {
                let img = vpx_codec_get_frame(&mut self.ctx, &mut iter);
                if img.is_null() {
                    break;
                }
                match copy_i420(&*img, rtp_timestamp) {
                    Some(frame) => frames.push(frame),
                    None => tracing::debug!(
                        fmt = ?(*img).fmt,
                        "stream.rtc.vpx.unsupported_decoded_format"
                    ),
                }
            }
            Ok(frames)
        }
    }
}

impl Drop for VpxDecoder {
    fn drop(&mut self) {
        // SAFETY: `ctx` was successfully initialized in `new` (we only construct
        // `Self` on success) and is destroyed exactly once here.
        unsafe {
            let _ = vpx_codec_destroy(&mut self.ctx);
        }
    }
}

/// Copy a libvpx image into a packed I420 [`VideoFrame`].
///
/// libvpx hands back plane pointers with their own row strides, which are
/// padded well past the visible width — copying `w * h` bytes straight off
/// `planes[0]` produces the classic sheared/garbled frame. Every row is copied
/// individually at `stride[plane]`.
///
/// # Safety
///
/// `img` must be a live `vpx_image_t` returned by `vpx_codec_get_frame`, whose
/// planes stay valid until the next `vpx_codec_decode` call.
unsafe fn copy_i420(img: &vpx_image_t, rtp_timestamp: u32) -> Option<VideoFrame> {
    if img.fmt != VPX_IMG_FMT_I420 {
        return None;
    }
    let (width, height) = (img.d_w, img.d_h);
    if width == 0 || height == 0 {
        return None;
    }

    let mut data = Vec::with_capacity(i420_len(width, height));
    // I420: chroma planes are half resolution in both axes (the shifts are 1,
    // but read them from the image rather than assuming).
    let cw = (width as usize).div_ceil(1 << img.x_chroma_shift);
    let ch = (height as usize).div_ceil(1 << img.y_chroma_shift);
    let planes = [
        (VPX_PLANE_Y as usize, width as usize, height as usize),
        (VPX_PLANE_U as usize, cw, ch),
        (VPX_PLANE_V as usize, cw, ch),
    ];

    for (plane, row_len, rows) in planes {
        let base = img.planes[plane];
        let stride = img.stride[plane];
        if base.is_null() || stride < 0 || (stride as usize) < row_len {
            return None;
        }
        for row in 0..rows {
            // SAFETY: libvpx guarantees `rows` rows of at least `stride` bytes
            // from `base`, and `row_len <= stride` is checked above.
            let start = unsafe { base.add(row * stride as usize) };
            data.extend_from_slice(unsafe { std::slice::from_raw_parts(start, row_len) });
        }
    }

    Some(VideoFrame {
        width,
        height,
        data,
        rtp_timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtc::vpx::VpxEncoder;

    /// A frame with a horizontal luma ramp: a stride bug shears or mangles the
    /// gradient, so this catches the classic packed-vs-strided copy mistake in a
    /// way a solid color cannot.
    fn ramp_i420(width: u32, height: u32) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let mut buf = Vec::with_capacity(i420_len(width, height));
        for _ in 0..h {
            for x in 0..w {
                buf.push((x * 255 / w.max(1)) as u8);
            }
        }
        buf.extend(std::iter::repeat_n(128u8, (w / 2) * (h / 2)));
        buf.extend(std::iter::repeat_n(128u8, (w / 2) * (h / 2)));
        buf
    }

    /// Round-trip through the real encoder and decoder: a keyframe in must come
    /// back out as a frame of the same size with recognisable content.
    fn round_trip(codec: VpxCodec, width: u32, height: u32) {
        let mut enc = VpxEncoder::new(codec, width, height, 1_000).expect("encoder");
        let mut dec = VpxDecoder::new(codec).expect("decoder");
        let source = ramp_i420(width, height);

        let packets = enc.encode(&source, 0, 33, true).expect("encode");
        assert!(!packets.is_empty(), "encoder produced no packet");

        let mut decoded = Vec::new();
        for packet in &packets {
            decoded.extend(dec.decode(&packet.data, 12_345).expect("decode"));
        }
        let frame = decoded.first().expect("decoder produced no frame");

        assert_eq!((frame.width, frame.height), (width, height));
        assert_eq!(frame.data.len(), i420_len(width, height));
        assert_eq!(frame.rtp_timestamp, 12_345);

        // The luma ramp must survive: left edge dark, right edge bright, and
        // every row identical (a stride bug breaks all three).
        let w = width as usize;
        let y = &frame.data[..w * height as usize];
        assert!(y[0] < 40, "left edge should be dark, got {}", y[0]);
        assert!(
            y[w - 1] > 215,
            "right edge should be bright, got {}",
            y[w - 1]
        );
        let mid_row = &y[(height as usize / 2) * w..][..w];
        assert!(
            mid_row[0] < 40 && mid_row[w - 1] > 215,
            "the ramp is not consistent across rows (stride handling)"
        );
    }

    #[test]
    fn vp8_round_trips_a_luma_ramp() {
        round_trip(VpxCodec::Vp8, 320, 240);
    }

    #[test]
    fn vp9_round_trips_a_luma_ramp() {
        round_trip(VpxCodec::Vp9, 320, 240);
    }

    /// Odd widths make libvpx's stride padding differ most from the visible
    /// width, so this is the strongest stride regression guard.
    #[test]
    fn vp9_round_trips_a_non_multiple_of_16_size() {
        round_trip(VpxCodec::Vp9, 322, 178);
    }

    #[test]
    fn empty_input_yields_no_frames() {
        let mut dec = VpxDecoder::new(VpxCodec::Vp9).expect("decoder");
        assert!(dec.decode(&[], 0).expect("decode empty").is_empty());
    }

    #[test]
    fn garbage_input_is_an_error_not_a_panic() {
        let mut dec = VpxDecoder::new(VpxCodec::Vp9).expect("decoder");
        assert!(dec.decode(&[0xff; 32], 0).is_err());
    }
}
