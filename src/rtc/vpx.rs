//! Minimal libvpx VP8/VP9 encoder for the outbound video path.
//!
//! [`LocalVideoTrack`](super::local_track::LocalVideoTrack) needs to turn raw
//! I420 frames into encoded VP8/VP9 for the SFU. The `vpx-encode` crate is too
//! restrictive for realtime streaming — it hardcodes the encoder config, so it
//! cannot set `g_lag_in_frames = 0` (VP9 otherwise buffers frames and emits
//! nothing per call) or force keyframes (the backend publisher can't answer a
//! subscriber's PLI, so a static image would only ever produce one keyframe
//! that late subscribers miss). This module binds `libvpx` directly (via
//! `env-libvpx-sys`, exposed as `vpx_sys`) with the correct realtime config.
//!
//! The libvpx C API is inherently unsafe; every FFI call is wrapped here and the
//! module surface is safe. libvpx encoder contexts are single-threaded but not
//! thread-*affine*, so [`VpxEncoder`] is `Send` (accessed under a mutex by the
//! caller) but not `Sync`.

use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_uint, c_ulong, c_void};
use std::ptr;

use vpx_sys::vp8e_enc_control_id::{
    VP8E_SET_CPUUSED, VP9E_GET_SVC_LAYER_ID, VP9E_REGISTER_CX_CALLBACK, VP9E_SET_ROW_MT,
    VP9E_SET_SVC, VP9E_SET_SVC_INTER_LAYER_PRED, VP9E_SET_SVC_PARAMETERS,
};
use vpx_sys::vp9e_temporal_layering_mode::{
    VP9E_TEMPORAL_LAYERING_MODE_0101, VP9E_TEMPORAL_LAYERING_MODE_0212,
    VP9E_TEMPORAL_LAYERING_MODE_NOLAYERING,
};
use vpx_sys::vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT;
use vpx_sys::vpx_img_fmt::VPX_IMG_FMT_I420;
use vpx_sys::vpx_rc_mode::VPX_CBR;
use vpx_sys::*;

use super::error::{Result, RtcError};

/// Which VPx codec a [`VpxEncoder`] produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VpxCodec {
    Vp8,
    Vp9,
}

/// One encoded frame plus whether it is a keyframe.
pub(crate) struct EncodedFrame {
    pub data: Vec<u8>,
    pub key: bool,
    pub spatial_id: u8,
    pub temporal_id: u8,
    pub width: u16,
    pub height: u16,
}

/// A WebRTC VP9 scalability mode supported by libvpx's fixed temporal patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Vp9SvcMode {
    spatial_layers: u8,
    temporal_layers: u8,
}

impl Vp9SvcMode {
    pub(crate) fn new(spatial_layers: u8, temporal_layers: u8) -> Result<Self> {
        if !(1..=3).contains(&spatial_layers) || !(1..=3).contains(&temporal_layers) {
            return Err(RtcError::Media(format!(
                "VP9 SVC supports one to three spatial and temporal layers (got L{spatial_layers}T{temporal_layers})"
            )));
        }
        Ok(Self {
            spatial_layers,
            temporal_layers,
        })
    }

    pub(crate) fn spatial_layers(self) -> u8 {
        self.spatial_layers
    }

    pub(crate) fn temporal_layers(self) -> u8 {
        self.temporal_layers
    }
}

/// A libvpx encoder configured for realtime single-frame streaming.
pub(crate) struct VpxEncoder {
    ctx: vpx_codec_ctx_t,
    width: u32,
    height: u32,
}

struct SvcCallbackBuffer {
    frames: Vec<EncodedFrame>,
    force_key: bool,
    failed: bool,
}

/// VP9 SVC encoder producing one owned compressed buffer per spatial layer.
pub(crate) struct VpxSvcEncoder {
    ctx: vpx_codec_ctx_t,
    width: u32,
    height: u32,
    mode: Vp9SvcMode,
    callback: Box<SvcCallbackBuffer>,
}

// SAFETY: `vpx_codec_ctx_t` holds raw pointers into libvpx internal state, which
// is why the auto `Send` impl is withheld. libvpx does not pin a context to the
// thread that created it — it only forbids *concurrent* use. Callers always hold
// the encoder behind a `std::sync::Mutex`, which serializes access, so moving
// the value across threads is sound.
unsafe impl Send for VpxEncoder {}

// SAFETY: the raw pointers in `ctx` and the callback registration are only
// accessed by libvpx during a synchronous `encode` call. The callback storage
// is heap allocated (so moving this wrapper does not invalidate `user_priv`),
// and callers serialize mutable access under a `std::sync::Mutex`.
unsafe impl Send for VpxSvcEncoder {}

unsafe extern "C" fn collect_svc_packet(pkt: *mut vpx_codec_cx_pkt_t, user_data: *mut c_void) {
    if pkt.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: `user_data` is the stable pointer to this encoder's boxed
    // `SvcCallbackBuffer`, registered during construction and kept alive until
    // after `vpx_codec_destroy`. libvpx invokes this callback synchronously and
    // never concurrently for a single encoder context.
    let callback = unsafe { &mut *user_data.cast::<SvcCallbackBuffer>() };
    // SAFETY: libvpx supplies a valid packet pointer for the duration of this
    // callback. Reading the frame union member is valid only for frame packets.
    let packet = unsafe { &*pkt };
    if packet.kind != VPX_CODEC_CX_FRAME_PKT {
        return;
    }
    // SAFETY: the active union member is `frame`, established by `kind` above.
    let frame = unsafe { packet.data.frame };
    let Some(spatial_id) = frame
        .spatial_layer_encoded
        .iter()
        // libvpx accumulates these flags as it invokes the callback in
        // increasing spatial order; the highest encoded flag identifies the
        // buffer supplied by this invocation.
        .rposition(|encoded| *encoded != 0)
    else {
        return;
    };
    // libvpx may invoke the callback with an empty enhancement-layer packet
    // when rate control drops that layer. It is not an allocation/FFI failure.
    if frame.sz == 0 {
        return;
    }
    if frame.buf.is_null() || callback.frames.try_reserve(1).is_err() {
        callback.failed = true;
        return;
    }
    let mut data = Vec::new();
    if data.try_reserve_exact(frame.sz).is_err() {
        callback.failed = true;
        return;
    }
    // SAFETY: libvpx guarantees `frame.buf` is readable for `frame.sz` bytes
    // until this callback returns. We immediately copy it into Rust-owned
    // storage and retain no C pointer.
    let encoded = unsafe { std::slice::from_raw_parts(frame.buf.cast::<u8>(), frame.sz) };
    data.extend_from_slice(encoded);
    callback.frames.push(EncodedFrame {
        data,
        key: callback.force_key || (frame.flags & VPX_FRAME_IS_KEY) != 0,
        spatial_id: u8::try_from(spatial_id).unwrap_or(u8::MAX),
        // Replaced with the codec-reported temporal id after encode returns.
        temporal_id: 0,
        width: u16::try_from(frame.width[spatial_id]).unwrap_or(u16::MAX),
        height: u16::try_from(frame.height[spatial_id]).unwrap_or(u16::MAX),
    });
}

/// Turn a libvpx `vpx_codec_err_t` into a `Result`. The enum is `repr(i32)`, so
/// a non-zero discriminant is an error (`VPX_CODEC_OK == 0`).
pub(super) fn check(res: vpx_codec_err_t, what: &str) -> Result<()> {
    let code: i32 = res as i32;
    if code == 0 {
        Ok(())
    } else {
        Err(RtcError::Media(format!(
            "libvpx {what} failed (code {code})"
        )))
    }
}

impl VpxEncoder {
    /// Create an encoder for `width`x`height` (both must be even) at
    /// `bitrate_kbps`, configured for realtime, low-latency, per-frame output.
    pub(crate) fn new(codec: VpxCodec, width: u32, height: u32, bitrate_kbps: u32) -> Result<Self> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(RtcError::Media(format!(
                "video dimensions must be non-zero and even (got {width}x{height})"
            )));
        }

        // SAFETY: `iface` is a static libvpx interface pointer. `cfg` and `ctx`
        // are left uninitialized and populated by `vpx_codec_enc_config_default`
        // / `vpx_codec_enc_init_ver` before `assume_init` reads them (the config
        // struct holds niche enums like `vpx_bit_depth`, so it must never be
        // constructed by zeroing). All pointers are valid for each call's scope.
        unsafe {
            let iface = match codec {
                VpxCodec::Vp8 => vpx_codec_vp8_cx(),
                VpxCodec::Vp9 => vpx_codec_vp9_cx(),
            };
            if iface.is_null() {
                return Err(RtcError::Media(
                    "libvpx codec interface unavailable".to_owned(),
                ));
            }

            let mut cfg = MaybeUninit::<vpx_codec_enc_cfg_t>::uninit();
            check(
                vpx_codec_enc_config_default(iface, cfg.as_mut_ptr(), 0),
                "enc_config_default",
            )?;
            let mut cfg = cfg.assume_init();

            cfg.g_w = width;
            cfg.g_h = height;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = 1_000; // milliseconds
            cfg.rc_target_bitrate = bitrate_kbps;
            // Emit a packet for every input frame (no alt-ref lookahead buffer).
            // This is the key setting `vpx-encode` cannot express.
            cfg.g_lag_in_frames = 0;
            cfg.g_threads = 4;
            cfg.g_error_resilient = VPX_ERROR_RESILIENT_DEFAULT;

            let mut ctx = MaybeUninit::<vpx_codec_ctx_t>::uninit();
            check(
                vpx_codec_enc_init_ver(
                    ctx.as_mut_ptr(),
                    iface,
                    &cfg,
                    0,
                    VPX_ENCODER_ABI_VERSION as c_int,
                ),
                "enc_init",
            )?;
            let mut ctx = ctx.assume_init();

            // Fastest realtime speed setting (CPUUSED is 0..=9 for VP9, higher is
            // faster / lower quality — fine for a solid backend frame).
            check(
                vpx_codec_control_(&mut ctx, VP8E_SET_CPUUSED as c_int, 8 as c_int),
                "set_cpuused",
            )?;
            if codec == VpxCodec::Vp9 {
                // Row-based multithreading; ignore errors on builds without it.
                let _ = vpx_codec_control_(&mut ctx, VP9E_SET_ROW_MT as c_int, 1 as c_int);
            }

            Ok(Self { ctx, width, height })
        }
    }

    /// Encode one packed I420 frame. `pts`/`duration` are in the encoder
    /// timebase (milliseconds). When `force_key` is set libvpx emits a keyframe.
    pub(crate) fn encode(
        &mut self,
        i420: &[u8],
        pts: i64,
        duration: i64,
        force_key: bool,
    ) -> Result<Vec<EncodedFrame>> {
        let expected = (self.width as usize) * (self.height as usize) * 3 / 2;
        if i420.len() < expected {
            return Err(RtcError::Media(format!(
                "i420 buffer too small: {} bytes for {}x{} (need {expected})",
                i420.len(),
                self.width,
                self.height
            )));
        }

        let flags: vpx_enc_frame_flags_t = if force_key {
            VPX_EFLAG_FORCE_KF as vpx_enc_frame_flags_t
        } else {
            0
        };

        // SAFETY: `image` is populated by `vpx_img_wrap` pointing at `i420`
        // (valid, `expected` bytes, not mutated by libvpx during encode) before
        // `assume_init` reads it. `vpx_codec_encode` and the cx-data drain below
        // operate on our owned context. The returned packet buffers are copied
        // into owned `Vec`s before the borrow ends.
        unsafe {
            let mut image = MaybeUninit::<vpx_image_t>::uninit();
            let wrapped = vpx_img_wrap(
                image.as_mut_ptr(),
                VPX_IMG_FMT_I420,
                self.width as c_uint,
                self.height as c_uint,
                1,
                i420.as_ptr() as *mut u8,
            );
            if wrapped.is_null() {
                return Err(RtcError::Media("vpx_img_wrap failed".to_owned()));
            }
            let image = image.assume_init();

            check(
                vpx_codec_encode(
                    &mut self.ctx,
                    &image,
                    pts,
                    duration as c_ulong,
                    flags,
                    VPX_DL_REALTIME as c_ulong,
                ),
                "encode",
            )?;

            let mut frames = Vec::new();
            let mut iter: vpx_codec_iter_t = ptr::null();
            loop {
                let pkt = vpx_codec_get_cx_data(&mut self.ctx, &mut iter);
                if pkt.is_null() {
                    break;
                }
                if (*pkt).kind == VPX_CODEC_CX_FRAME_PKT {
                    let frame = &(*pkt).data.frame;
                    let data =
                        std::slice::from_raw_parts(frame.buf as *const u8, frame.sz as usize)
                            .to_vec();
                    let key = (frame.flags & VPX_FRAME_IS_KEY) != 0;
                    frames.push(EncodedFrame {
                        data,
                        key,
                        spatial_id: 0,
                        temporal_id: 0,
                        width: u16::try_from(self.width).unwrap_or(u16::MAX),
                        height: u16::try_from(self.height).unwrap_or(u16::MAX),
                    });
                }
            }
            Ok(frames)
        }
    }
}

impl VpxSvcEncoder {
    /// Create a one-SSRC VP9 K-SVC encoder. Spatial resolutions use a 2:1
    /// ratio; temporal layers use libvpx's fixed `0101`/`0212` patterns.
    pub(crate) fn new(
        width: u32,
        height: u32,
        bitrate_kbps: u32,
        mode: Vp9SvcMode,
    ) -> Result<Self> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(RtcError::Media(format!(
                "video dimensions must be non-zero and even (got {width}x{height})"
            )));
        }

        let callback = Box::new(SvcCallbackBuffer {
            frames: Vec::with_capacity(usize::from(mode.spatial_layers())),
            force_key: true,
            failed: false,
        });

        // SAFETY: the libvpx interface pointer is static. `cfg` and `ctx` are
        // initialized by their respective C constructors before being read.
        // The callback's `user_priv` points into a Box whose allocation remains
        // stable until after this context is destroyed. Every C call is
        // synchronous and all pointer arguments remain live for its duration.
        unsafe {
            let iface = vpx_codec_vp9_cx();
            if iface.is_null() {
                return Err(RtcError::Media(
                    "libvpx VP9 codec interface unavailable".to_owned(),
                ));
            }
            let mut cfg = MaybeUninit::<vpx_codec_enc_cfg_t>::uninit();
            check(
                vpx_codec_enc_config_default(iface, cfg.as_mut_ptr(), 0),
                "SVC enc_config_default",
            )?;
            let mut cfg = cfg.assume_init();
            cfg.g_w = width;
            cfg.g_h = height;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = 1_000;
            cfg.g_lag_in_frames = 0;
            cfg.g_threads = 4;
            cfg.g_error_resilient = VPX_ERROR_RESILIENT_DEFAULT;
            cfg.rc_end_usage = VPX_CBR;
            cfg.rc_target_bitrate = bitrate_kbps.max(1);
            cfg.rc_resize_allowed = 0;
            cfg.rc_min_quantizer = 2;
            cfg.rc_max_quantizer = 56;
            cfg.rc_undershoot_pct = 50;
            cfg.rc_overshoot_pct = 50;
            cfg.rc_buf_initial_sz = 500;
            cfg.rc_buf_optimal_sz = 600;
            cfg.rc_buf_sz = 1_000;
            cfg.ss_number_layers = c_uint::from(mode.spatial_layers());
            cfg.ts_number_layers = c_uint::from(mode.temporal_layers());

            let temporal_mode = match mode.temporal_layers() {
                1 => VP9E_TEMPORAL_LAYERING_MODE_NOLAYERING,
                2 => VP9E_TEMPORAL_LAYERING_MODE_0101,
                _ => VP9E_TEMPORAL_LAYERING_MODE_0212,
            };
            cfg.temporal_layering_mode = temporal_mode as c_int;
            let temporal_fractions: &[u32] = match mode.temporal_layers() {
                1 => &[100],
                2 => &[67, 100],
                _ => &[60, 80, 100],
            };
            for temporal_id in 0..usize::from(mode.temporal_layers()) {
                cfg.ts_rate_decimator[temporal_id] =
                    1_u32 << (usize::from(mode.temporal_layers()) - temporal_id - 1);
            }
            let spatial_weights: &[u32] = match mode.spatial_layers() {
                1 => &[1],
                2 => &[1, 2],
                _ => &[1, 2, 4],
            };
            let weight_total: u32 = spatial_weights.iter().sum();
            let mut assigned = 0_u32;
            for (spatial_id, weight) in spatial_weights.iter().enumerate() {
                let spatial_bitrate = if spatial_id + 1 == spatial_weights.len() {
                    cfg.rc_target_bitrate.saturating_sub(assigned).max(1)
                } else {
                    cfg.rc_target_bitrate.saturating_mul(*weight) / weight_total
                };
                assigned = assigned.saturating_add(spatial_bitrate);
                cfg.ss_target_bitrate[spatial_id] = spatial_bitrate;
                for (temporal_id, fraction) in temporal_fractions.iter().enumerate() {
                    let index = spatial_id * usize::from(mode.temporal_layers()) + temporal_id;
                    cfg.layer_target_bitrate[index] =
                        spatial_bitrate.saturating_mul(*fraction) / 100;
                }
            }

            let mut ctx = MaybeUninit::<vpx_codec_ctx_t>::uninit();
            check(
                vpx_codec_enc_init_ver(
                    ctx.as_mut_ptr(),
                    iface,
                    &cfg,
                    0,
                    VPX_ENCODER_ABI_VERSION as c_int,
                ),
                "SVC enc_init",
            )?;
            let ctx = ctx.assume_init();
            // Build the owner immediately after successful initialization so
            // every later `?` destroys the native context on the error path.
            let mut encoder = Self {
                ctx,
                width,
                height,
                mode,
                callback,
            };
            check(
                vpx_codec_control_(&mut encoder.ctx, VP8E_SET_CPUUSED as c_int, 8 as c_int),
                "SVC set_cpuused",
            )?;
            let _ = vpx_codec_control_(&mut encoder.ctx, VP9E_SET_ROW_MT as c_int, 1 as c_int);
            check(
                vpx_codec_control_(&mut encoder.ctx, VP9E_SET_SVC as c_int, 1 as c_int),
                "enable SVC",
            )?;

            let mut parameters = vpx_svc_parameters {
                max_quantizers: [56; 12],
                min_quantizers: [2; 12],
                scaling_factor_num: [1; 12],
                scaling_factor_den: [1; 12],
                speed_per_layer: [8; 12],
                temporal_layering_mode: temporal_mode as c_int,
                loopfilter_ctrl: [0; 12],
            };
            for spatial_id in 0..usize::from(mode.spatial_layers()) {
                parameters.scaling_factor_num[spatial_id] = 1;
                parameters.scaling_factor_den[spatial_id] =
                    1_i32 << (usize::from(mode.spatial_layers()) - spatial_id - 1);
            }
            check(
                vpx_codec_control_(
                    &mut encoder.ctx,
                    VP9E_SET_SVC_PARAMETERS as c_int,
                    &mut parameters as *mut vpx_svc_parameters,
                ),
                "set SVC parameters",
            )?;
            // WebRTC's `_KEY` modes use lower spatial layers only as references
            // for key pictures (libvpx value 2: disabled on non-key frames).
            check(
                vpx_codec_control_(
                    &mut encoder.ctx,
                    VP9E_SET_SVC_INTER_LAYER_PRED as c_int,
                    2_u32,
                ),
                "set SVC inter-layer prediction",
            )?;
            let mut pair = vpx_codec_enc_output_cx_cb_pair {
                output_cx_pkt: Some(collect_svc_packet),
                user_priv: (&mut *encoder.callback as *mut SvcCallbackBuffer).cast::<c_void>(),
            };
            check(
                vpx_codec_control_(
                    &mut encoder.ctx,
                    VP9E_REGISTER_CX_CALLBACK as c_int,
                    &mut pair as *mut vpx_codec_enc_output_cx_cb_pair,
                ),
                "register SVC output callback",
            )?;

            Ok(encoder)
        }
    }

    pub(crate) fn mode(&self) -> Vp9SvcMode {
        self.mode
    }

    /// Encode one picture and return its spatial frames in increasing SID order.
    pub(crate) fn encode(
        &mut self,
        i420: &[u8],
        pts: i64,
        duration: i64,
        force_key: bool,
    ) -> Result<Vec<EncodedFrame>> {
        let expected = usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_add(pixels / 2))
            .ok_or_else(|| RtcError::Media("VP9 SVC frame size overflow".to_owned()))?;
        if i420.len() < expected {
            return Err(RtcError::Media(format!(
                "i420 buffer too small: {} bytes for {}x{} (need {expected})",
                i420.len(),
                self.width,
                self.height
            )));
        }
        self.callback.frames.clear();
        self.callback.force_key = force_key;
        self.callback.failed = false;
        let flags = if force_key {
            VPX_EFLAG_FORCE_KF as vpx_enc_frame_flags_t
        } else {
            0
        };

        // SAFETY: `image` is initialized by `vpx_img_wrap` over a validated
        // packed I420 slice. libvpx only borrows the slice during this
        // synchronous call; the registered callback copies all packet buffers
        // before control returns to C.
        unsafe {
            let mut image = MaybeUninit::<vpx_image_t>::uninit();
            if vpx_img_wrap(
                image.as_mut_ptr(),
                VPX_IMG_FMT_I420,
                self.width,
                self.height,
                1,
                i420.as_ptr() as *mut u8,
            )
            .is_null()
            {
                return Err(RtcError::Media("vpx_img_wrap failed".to_owned()));
            }
            let image = image.assume_init();
            check(
                vpx_codec_encode(
                    &mut self.ctx,
                    &image,
                    pts,
                    c_ulong::try_from(duration.max(1)).unwrap_or(c_ulong::MAX),
                    flags,
                    VPX_DL_REALTIME as c_ulong,
                ),
                "SVC encode",
            )?;
        }
        if self.callback.failed {
            return Err(RtcError::Media(
                "failed to copy libvpx SVC callback output".to_owned(),
            ));
        }
        let mut frames = std::mem::take(&mut self.callback.frames);
        let mut layer_id = vpx_svc_layer_id_t {
            spatial_layer_id: 0,
            temporal_layer_id: 0,
            temporal_layer_id_per_spatial: [0; 5],
        };
        // SAFETY: the context is live and exclusively borrowed, and libvpx
        // writes the current layer identifiers into this stack value before
        // returning. No pointer escapes the call.
        unsafe {
            check(
                vpx_codec_control_(
                    &mut self.ctx,
                    VP9E_GET_SVC_LAYER_ID as c_int,
                    &mut layer_id as *mut vpx_svc_layer_id_t,
                ),
                "get SVC layer id",
            )?;
        }
        let temporal_id = u8::try_from(layer_id.temporal_layer_id)
            .ok()
            .filter(|temporal_id| *temporal_id < self.mode.temporal_layers())
            .ok_or_else(|| {
                RtcError::Media(format!(
                    "libvpx returned invalid temporal layer {} for T{}",
                    layer_id.temporal_layer_id,
                    self.mode.temporal_layers()
                ))
            })?;
        for frame in &mut frames {
            frame.temporal_id = temporal_id;
        }
        frames.sort_unstable_by_key(|frame| frame.spatial_id);
        Ok(frames)
    }
}

impl Drop for VpxEncoder {
    fn drop(&mut self) {
        // SAFETY: `ctx` was successfully initialized in `new` (we only construct
        // `Self` on success) and is destroyed exactly once here.
        unsafe {
            let _ = vpx_codec_destroy(&mut self.ctx);
        }
    }
}

impl Drop for VpxSvcEncoder {
    fn drop(&mut self) {
        // SAFETY: `ctx` was initialized successfully before `Self` was built and
        // is destroyed exactly once. Destruction also unregisters the callback
        // before its boxed user storage is dropped.
        unsafe {
            let _ = vpx_codec_destroy(&mut self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blue_i420(width: u32, height: u32) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let mut buf = vec![41u8; w * h];
        buf.extend(std::iter::repeat_n(240u8, (w / 2) * (h / 2)));
        buf.extend(std::iter::repeat_n(110u8, (w / 2) * (h / 2)));
        buf
    }

    /// Realtime config must emit a packet for the first frame (a keyframe), not
    /// buffer it — the regression that made `vpx-encode` unusable here.
    #[test]
    fn vp9_emits_keyframe_on_first_frame() {
        let mut enc = VpxEncoder::new(VpxCodec::Vp9, 320, 240, 800).expect("vp9 encoder");
        let frame = blue_i420(320, 240);
        let out = enc.encode(&frame, 0, 100, true).expect("encode");
        assert!(!out.is_empty(), "expected at least one encoded packet");
        assert!(
            out.iter().any(|f| f.key),
            "first forced frame must be a keyframe"
        );
        assert!(
            out.iter().any(|f| !f.data.is_empty()),
            "packet payload was empty"
        );
    }

    #[test]
    fn vp8_emits_packets() {
        let mut enc = VpxEncoder::new(VpxCodec::Vp8, 160, 120, 400).expect("vp8 encoder");
        let frame = blue_i420(160, 120);
        let out = enc.encode(&frame, 0, 100, true).expect("encode");
        assert!(!out.is_empty());
    }

    #[test]
    fn vp9_svc_emits_one_owned_frame_per_spatial_layer() {
        let mode = Vp9SvcMode::new(3, 3).expect("L3T3 mode");
        let mut encoder = VpxSvcEncoder::new(320, 240, 900, mode).expect("VP9 SVC encoder");
        let input = blue_i420(320, 240);

        let frames = encoder.encode(&input, 0, 33, true).expect("encode");

        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames
                .iter()
                .map(|frame| (frame.spatial_id, frame.temporal_id))
                .collect::<Vec<_>>(),
            vec![(0, 0), (1, 0), (2, 0)]
        );
        assert_eq!(
            frames
                .iter()
                .map(|frame| (frame.width, frame.height))
                .collect::<Vec<_>>(),
            vec![(80, 60), (160, 120), (320, 240)]
        );
        assert!(frames.iter().all(|frame| frame.key));
        assert!(frames.iter().all(|frame| !frame.data.is_empty()));
    }

    #[test]
    fn vp9_svc_supports_every_advertised_layer_count() {
        let input = blue_i420(160, 120);
        for spatial_layers in 1..=3 {
            for temporal_layers in 1..=3 {
                let mode = Vp9SvcMode::new(spatial_layers, temporal_layers).expect("mode");
                let mut encoder = VpxSvcEncoder::new(160, 120, 900, mode).expect("VP9 SVC encoder");
                let frames = encoder.encode(&input, 0, 33, true).expect("encode");
                assert_eq!(frames.len(), usize::from(spatial_layers));
                assert!(frames.iter().all(|frame| frame.temporal_id == 0));
            }
        }
    }

    #[test]
    fn vp9_svc_uses_codec_reported_0212_temporal_pattern_and_resets_on_key() {
        let mode = Vp9SvcMode::new(1, 3).expect("L1T3 mode");
        let mut encoder = VpxSvcEncoder::new(160, 120, 500, mode).expect("VP9 SVC encoder");
        let input = blue_i420(160, 120);
        let mut temporal_ids = Vec::new();

        for frame_index in 0..5 {
            let frames = encoder
                .encode(&input, i64::from(frame_index) * 33, 33, frame_index == 0)
                .expect("encode temporal picture");
            temporal_ids.push(frames[0].temporal_id);
        }
        let key = encoder.encode(&input, 165, 33, true).expect("forced key");
        temporal_ids.push(key[0].temporal_id);

        assert_eq!(temporal_ids, [0, 2, 1, 2, 0, 0]);
    }
}
