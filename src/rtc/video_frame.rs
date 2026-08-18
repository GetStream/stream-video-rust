//! Decoded video frames ([`VideoFrame`]) produced by
//! [`RemoteTrack::next_video_frame`](super::remote_track::RemoteTrack::next_video_frame).
//!
//! Frames are packed I420 (YUV 4:2:0 planar) — the same layout
//! [`LocalVideoTrack::write_i420`](super::local_track::LocalVideoTrack::write_i420)
//! consumes, so a decode → transform → re-encode bridge needs no conversion in
//! between.
//!
//! The conversions here are dependency-free pure Rust on purpose: image codecs
//! (JPEG/PNG) and resampling filters are an application concern and stay out of
//! this crate. [`VideoFrame::to_rgb8`] and [`VideoFrame::downscale`] cover what a
//! bot needs to hand a frame to a vision model or shrink it before re-encoding.

/// One decoded video frame in packed I420.
///
/// `data` layout, for `w = width` and `h = height`:
///
/// | plane | offset            | size                        |
/// |-------|-------------------|-----------------------------|
/// | Y     | `0`               | `w * h`                     |
/// | U     | `w * h`           | `w.div_ceil(2) * h.div_ceil(2)` |
/// | V     | `w * h + chroma`  | `w.div_ceil(2) * h.div_ceil(2)` |
///
/// For the even dimensions every real encoder emits, that is `w * h * 3 / 2`
/// bytes total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Packed I420 planes (see the type docs for the layout).
    pub data: Vec<u8>,
    /// The RTP timestamp (90 kHz video clock) of the sample this was decoded
    /// from. Use it to order or pace frames; it is not a wall clock.
    pub rtp_timestamp: u32,
}

/// Chroma plane dimensions for a `width`x`height` I420 frame.
fn chroma_dims(width: u32, height: u32) -> (usize, usize) {
    ((width as usize).div_ceil(2), (height as usize).div_ceil(2))
}

/// The exact packed-I420 buffer length for `width`x`height`.
pub(crate) fn i420_len(width: u32, height: u32) -> usize {
    let (cw, ch) = chroma_dims(width, height);
    (width as usize) * (height as usize) + 2 * cw * ch
}

impl VideoFrame {
    /// Split `data` into the Y, U, and V planes.
    fn planes(&self) -> (&[u8], &[u8], &[u8]) {
        let y_size = (self.width as usize) * (self.height as usize);
        let (cw, ch) = chroma_dims(self.width, self.height);
        let c_size = cw * ch;
        let (y, rest) = self.data.split_at(y_size);
        let (u, v) = rest.split_at(c_size);
        (y, u, &v[..c_size])
    }

    /// Convert to packed 8-bit RGB (`width * height * 3` bytes), BT.601
    /// limited-range — the color space WebRTC video is delivered in.
    pub fn to_rgb8(&self) -> Vec<u8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let (y_plane, u_plane, v_plane) = self.planes();
        let (cw, _ch) = chroma_dims(self.width, self.height);

        let mut rgb = vec![0u8; w * h * 3];
        for row in 0..h {
            let c_row = row / 2;
            for col in 0..w {
                let y = i32::from(y_plane[row * w + col]);
                let c_idx = c_row * cw + col / 2;
                let u = i32::from(u_plane[c_idx]);
                let v = i32::from(v_plane[c_idx]);

                // Integer BT.601: 298/409/100/208/516 are the standard 8.8
                // fixed-point coefficients.
                let c = y - 16;
                let d = u - 128;
                let e = v - 128;
                let out = &mut rgb[(row * w + col) * 3..][..3];
                out[0] = clamp_u8((298 * c + 409 * e + 128) >> 8);
                out[1] = clamp_u8((298 * c - 100 * d - 208 * e + 128) >> 8);
                out[2] = clamp_u8((298 * c + 516 * d + 128) >> 8);
            }
        }
        rgb
    }

    /// Box-downscale to `width`x`height`, averaging each source region.
    ///
    /// Both targets are rounded **down** to an even number (I420 needs even
    /// dimensions) with a floor of 2, and are clamped to the current size —
    /// this only ever shrinks.
    pub fn downscale(&self, width: u32, height: u32) -> VideoFrame {
        let dw = width.min(self.width).max(2) & !1;
        let dh = height.min(self.height).max(2) & !1;
        if dw == self.width && dh == self.height {
            return self.clone();
        }

        let (y_plane, u_plane, v_plane) = self.planes();
        let (scw, sch) = chroma_dims(self.width, self.height);
        let (dcw, dch) = chroma_dims(dw, dh);

        let mut data = box_downscale(
            y_plane,
            self.width as usize,
            self.height as usize,
            dw as usize,
            dh as usize,
        );
        data.extend(box_downscale(u_plane, scw, sch, dcw, dch));
        data.extend(box_downscale(v_plane, scw, sch, dcw, dch));

        VideoFrame {
            width: dw,
            height: dh,
            data,
            rtp_timestamp: self.rtp_timestamp,
        }
    }

    /// Downscale so the longest edge is at most `max_edge`, preserving aspect
    /// ratio. Returns a clone when the frame already fits.
    pub fn downscale_to_fit(&self, max_edge: u32) -> VideoFrame {
        let longest = self.width.max(self.height);
        if max_edge == 0 || longest <= max_edge {
            return self.clone();
        }
        let scale = f64::from(max_edge) / f64::from(longest);
        let w = (f64::from(self.width) * scale).round() as u32;
        let h = (f64::from(self.height) * scale).round() as u32;
        self.downscale(w, h)
    }
}

fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Average each destination pixel over its source region (a box filter). `src`
/// is a tightly packed `sw`x`sh` 8-bit plane.
fn box_downscale(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dh];
    for dy in 0..dh {
        let y0 = dy * sh / dh;
        let y1 = (((dy + 1) * sh).div_ceil(dh)).min(sh).max(y0 + 1);
        for dx in 0..dw {
            let x0 = dx * sw / dw;
            let x1 = (((dx + 1) * sw).div_ceil(dw)).min(sw).max(x0 + 1);
            let mut sum = 0u32;
            let mut count = 0u32;
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += u32::from(src[y * sw + x]);
                    count += 1;
                }
            }
            out[dy * dw + dx] = (sum / count.max(1)) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid-color I420 frame (BT.601 limited-range blue is Y=41,U=240,V=110).
    fn solid(width: u32, height: u32, y: u8, u: u8, v: u8) -> VideoFrame {
        let (cw, ch) = chroma_dims(width, height);
        let mut data = vec![y; (width as usize) * (height as usize)];
        data.extend(std::iter::repeat_n(u, cw * ch));
        data.extend(std::iter::repeat_n(v, cw * ch));
        VideoFrame {
            width,
            height,
            data,
            rtp_timestamp: 900,
        }
    }

    #[test]
    fn i420_len_matches_three_halves_for_even_dimensions() {
        assert_eq!(i420_len(640, 360), 640 * 360 * 3 / 2);
        assert_eq!(solid(64, 48, 0, 0, 0).data.len(), i420_len(64, 48));
    }

    #[test]
    fn blue_frame_converts_to_blue_rgb() {
        let rgb = solid(8, 8, 41, 240, 110).to_rgb8();
        assert_eq!(rgb.len(), 8 * 8 * 3);
        let (r, g, b) = (rgb[0], rgb[1], rgb[2]);
        assert!(b > 200, "expected a blue-dominant pixel, got ({r},{g},{b})");
        assert!(b > r && b > g, "blue must dominate, got ({r},{g},{b})");
    }

    #[test]
    fn white_and_black_survive_the_round_trip() {
        let white = solid(4, 4, 235, 128, 128).to_rgb8();
        assert!(white[..3].iter().all(|&c| c > 250), "{:?}", &white[..3]);
        let black = solid(4, 4, 16, 128, 128).to_rgb8();
        assert!(black[..3].iter().all(|&c| c == 0), "{:?}", &black[..3]);
    }

    #[test]
    fn downscale_halves_and_preserves_a_solid_color() {
        let small = solid(64, 48, 41, 240, 110).downscale(32, 24);
        assert_eq!((small.width, small.height), (32, 24));
        assert_eq!(small.data.len(), i420_len(32, 24));
        assert!(small.data[..32 * 24].iter().all(|&y| y == 41));
        assert_eq!(small.rtp_timestamp, 900);
    }

    #[test]
    fn downscale_rounds_targets_down_to_even() {
        let small = solid(64, 48, 128, 128, 128).downscale(31, 23);
        assert_eq!((small.width, small.height), (30, 22));
        assert_eq!(small.data.len(), i420_len(30, 22));
    }

    #[test]
    fn downscale_never_upscales() {
        let frame = solid(32, 24, 128, 128, 128);
        let same = frame.downscale(640, 480);
        assert_eq!((same.width, same.height), (32, 24));
    }

    #[test]
    fn downscale_to_fit_preserves_aspect_ratio() {
        let small = solid(1280, 720, 128, 128, 128).downscale_to_fit(512);
        assert_eq!(small.width, 512);
        assert_eq!(small.height, 288);
        assert_eq!(small.data.len(), i420_len(512, 288));
    }

    #[test]
    fn downscale_to_fit_is_a_no_op_when_already_small() {
        let frame = solid(320, 240, 128, 128, 128);
        assert_eq!(frame.downscale_to_fit(512), frame);
    }

    /// A box filter must average, not point-sample: a 2x2 checkerboard collapsed
    /// to 1 pixel is the mean of its four luma values.
    #[test]
    fn box_filter_averages_rather_than_samples() {
        let averaged = box_downscale(&[10, 100, 200, 255], 2, 2, 1, 1);
        assert_eq!(averaged, vec![((10 + 100 + 200 + 255) / 4) as u8]);
    }
}
