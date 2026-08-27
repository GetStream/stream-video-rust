//! Native local and remote media track classes.

use std::num::NonZeroU8;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use napi::bindgen_prelude::{Buffer, Env, PromiseRaw};
use napi_derive::napi;
use serde::Deserialize;
use serde_json::json;

use getstream::rtc::{
    LocalAudioTrack, LocalVideoTrack, LocalVideoTrackConfig, RemoteTrack, VideoLayering,
};

use crate::error::{illegal_state, invalid_argument, rtc_error};
use crate::types::{
    NativePcmFrame, NativeRtpPacket, NativeVideoFrame, duration_from_ms, pcm_from_le_bytes,
};

#[napi]
pub struct NativeLocalAudioTrack {
    pub(crate) inner: LocalAudioTrack,
}

#[napi]
impl NativeLocalAudioTrack {
    #[napi(factory)]
    pub fn opus() -> napi::Result<Self> {
        Ok(Self {
            inner: LocalAudioTrack::opus().map_err(rtc_error)?,
        })
    }

    #[napi]
    pub fn write_pcm<'env>(
        &self,
        env: &'env Env,
        data: Buffer,
        sample_rate: u32,
        channels: u32,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let bytes = data.to_vec();
        let frame = pcm_from_le_bytes(&bytes, sample_rate, channels)?;
        let track = self.inner.clone();
        env.spawn_future(async move { track.write_pcm(frame).await.map_err(rtc_error) })
    }

    #[napi]
    pub fn write_encoded<'env>(
        &self,
        env: &'env Env,
        data: Buffer,
        duration_ms: f64,
        audio_level: Option<u32>,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let bytes = data.to_vec();
        let duration = duration_from_ms(duration_ms)?;
        let level = validate_audio_level(audio_level)?;
        let track = self.inner.clone();
        env.spawn_future(async move {
            match level {
                Some(level) => track.write_sample_with_level(&bytes, duration, level).await,
                None => track.write_sample(&bytes, duration).await,
            }
            .map_err(rtc_error)
        })
    }

    #[napi]
    pub fn write_rtp<'env>(
        &self,
        env: &'env Env,
        packet: NativeRtpPacket,
        audio_level: Option<u32>,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let packet = packet.try_into()?;
        let level = validate_audio_level(audio_level)?;
        let track = self.inner.clone();
        env.spawn_future(async move {
            match level {
                Some(level) => track.write_rtp_with_level(packet, level).await,
                None => track.write_rtp(packet).await,
            }
            .map_err(rtc_error)
        })
    }

    #[napi]
    pub fn flush(&self) {
        self.inner.flush();
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VideoOptions {
    target_bitrate_bps: Option<u32>,
    allow_frame_skipping: Option<bool>,
    layering: Option<LayeringOptions>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
enum LayeringOptions {
    Single,
    ServerManaged {
        #[serde(rename = "maxSpatialLayers")]
        max_spatial_layers: Option<u8>,
        #[serde(rename = "maxTemporalLayers")]
        max_temporal_layers: Option<u8>,
    },
}

#[napi]
pub struct NativeLocalVideoTrack {
    pub(crate) inner: LocalVideoTrack,
}

#[napi]
impl NativeLocalVideoTrack {
    #[napi(factory)]
    pub fn vp8(options_json: Option<String>) -> napi::Result<Self> {
        Self::create("vp8", options_json)
    }

    #[napi(factory)]
    pub fn vp9(options_json: Option<String>) -> napi::Result<Self> {
        Self::create("vp9", options_json)
    }

    #[napi(factory)]
    pub fn h264(options_json: Option<String>) -> napi::Result<Self> {
        Self::create("h264", options_json)
    }

    #[napi]
    pub fn write_i420<'env>(
        &self,
        env: &'env Env,
        data: Buffer,
        width: u32,
        height: u32,
        duration_ms: f64,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let bytes = data.to_vec();
        let duration = duration_from_ms(duration_ms)?;
        let track = self.inner.clone();
        env.spawn_future(async move {
            track
                .write_i420_vec(bytes, width, height, duration)
                .await
                .map_err(rtc_error)
        })
    }

    #[napi]
    pub fn write_encoded<'env>(
        &self,
        env: &'env Env,
        data: Buffer,
        duration_ms: f64,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let bytes = data.to_vec();
        let duration = duration_from_ms(duration_ms)?;
        let track = self.inner.clone();
        env.spawn_future(async move {
            track
                .write_sample(&bytes, duration)
                .await
                .map_err(rtc_error)
        })
    }

    #[napi]
    pub fn write_rtp<'env>(
        &self,
        env: &'env Env,
        packet: NativeRtpPacket,
    ) -> napi::Result<PromiseRaw<'env, ()>> {
        let packet = packet.try_into()?;
        let track = self.inner.clone();
        env.spawn_future(async move { track.write_rtp(packet).await.map_err(rtc_error) })
    }
}

impl NativeLocalVideoTrack {
    fn create(codec: &str, options_json: Option<String>) -> napi::Result<Self> {
        let options = options_json
            .map(|value| serde_json::from_str::<VideoOptions>(&value))
            .transpose()
            .map_err(|error| invalid_argument(format!("invalid video options: {error}")))?
            .unwrap_or_default();
        let mut config = options
            .target_bitrate_bps
            .map(LocalVideoTrackConfig::new)
            .unwrap_or_default();
        if let Some(allow_frame_skipping) = options.allow_frame_skipping {
            config.allow_frame_skipping = allow_frame_skipping;
        }
        config.layering = match options.layering {
            None | Some(LayeringOptions::Single) => VideoLayering::Single,
            Some(LayeringOptions::ServerManaged {
                max_spatial_layers,
                max_temporal_layers,
            }) => VideoLayering::ServerManaged {
                max_spatial_layers: non_zero_layer(max_spatial_layers, "maxSpatialLayers")?,
                max_temporal_layers: non_zero_layer(max_temporal_layers, "maxTemporalLayers")?,
            },
        };
        let inner = match codec {
            "vp8" => LocalVideoTrack::vp8_with_config(config),
            "vp9" => LocalVideoTrack::vp9_with_config(config),
            "h264" => LocalVideoTrack::h264_with_config(config),
            _ => return Err(invalid_argument(format!("unsupported video codec {codec}"))),
        }
        .map_err(rtc_error)?;
        Ok(Self { inner })
    }
}

fn non_zero_layer(value: Option<u8>, field: &str) -> napi::Result<Option<NonZeroU8>> {
    value
        .map(|value| {
            if value > 3 {
                return Err(invalid_argument(format!("{field} cannot exceed 3")));
            }
            NonZeroU8::new(value)
                .ok_or_else(|| invalid_argument(format!("{field} must be greater than zero")))
        })
        .transpose()
}

fn validate_audio_level(value: Option<u32>) -> napi::Result<Option<u8>> {
    value
        .map(|value| {
            u8::try_from(value)
                .ok()
                .filter(|value| *value <= 127)
                .ok_or_else(|| invalid_argument("audioLevel must be between 0 and 127"))
        })
        .transpose()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ReadMode {
    Decoded = 1,
    Rtp = 2,
}

impl ReadMode {
    fn name(self) -> &'static str {
        match self {
            Self::Decoded => "decoded",
            Self::Rtp => "rtp",
        }
    }
}

#[derive(Default)]
struct ReadModeOwner {
    selected: AtomicU8,
}

impl ReadModeOwner {
    fn claim(&self, requested: ReadMode) -> napi::Result<()> {
        let requested_value = requested as u8;
        match self.selected.compare_exchange(
            0,
            requested_value,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(current) if current == requested_value => Ok(()),
            Err(current) => {
                let active = match current {
                    value if value == ReadMode::Decoded as u8 => ReadMode::Decoded.name(),
                    value if value == ReadMode::Rtp as u8 => ReadMode::Rtp.name(),
                    _ => "unknown",
                };
                Err(illegal_state(
                    "remote tracks cannot switch between decoded and RTP reads",
                    json!({
                        "activeMode": active,
                        "requestedMode": requested.name(),
                    }),
                ))
            }
        }
    }
}

#[napi]
pub struct NativeRemoteTrack {
    inner: Arc<RemoteTrack>,
    read_mode: ReadModeOwner,
}

impl NativeRemoteTrack {
    pub(crate) fn new(inner: RemoteTrack) -> Self {
        Self {
            inner: Arc::new(inner),
            read_mode: ReadModeOwner::default(),
        }
    }
}

#[napi]
impl NativeRemoteTrack {
    #[napi(getter)]
    pub fn user_id(&self) -> String {
        self.inner.participant().user_id.clone()
    }

    #[napi(getter)]
    pub fn session_id(&self) -> String {
        self.inner.participant().session_id.clone()
    }

    #[napi(getter)]
    pub fn track_lookup_prefix(&self) -> String {
        self.inner.participant().track_lookup_prefix.clone()
    }

    #[napi(getter)]
    pub fn track_type(&self) -> &'static str {
        super::track_type_name(self.inner.track_type())
    }

    #[napi(getter)]
    pub fn mime_type(&self) -> String {
        self.inner.codec().mime_type.clone()
    }

    #[napi(getter)]
    pub fn payload_type(&self) -> u32 {
        u32::from(self.inner.codec().payload_type)
    }

    #[napi(getter)]
    pub fn clock_rate(&self) -> u32 {
        self.inner.codec().clock_rate
    }

    #[napi(getter)]
    pub fn channels(&self) -> u32 {
        u32::from(self.inner.codec().channels)
    }

    #[napi(getter)]
    pub fn ssrc(&self) -> u32 {
        self.inner.ssrc()
    }

    #[napi]
    pub async fn next_pcm(&self) -> napi::Result<Option<NativePcmFrame>> {
        self.read_mode.claim(ReadMode::Decoded)?;
        Ok(self.inner.next_pcm().await.map(Into::into))
    }

    #[napi]
    pub async fn next_video_frame(&self) -> napi::Result<Option<NativeVideoFrame>> {
        self.read_mode.claim(ReadMode::Decoded)?;
        Ok(self.inner.next_video_frame().await.map(Into::into))
    }

    #[napi]
    pub async fn read_rtp(&self) -> napi::Result<Option<NativeRtpPacket>> {
        self.read_mode.claim(ReadMode::Rtp)?;
        Ok(self.inner.read_rtp().await.map(Into::into))
    }

    #[napi]
    pub async fn drain_rtp(&self) -> napi::Result<bool> {
        self.read_mode.claim(ReadMode::Rtp)?;
        Ok(self.inner.drain_rtp().await)
    }

    #[napi]
    pub async fn request_keyframe(&self) -> napi::Result<()> {
        self.inner.request_keyframe().await.map_err(rtc_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn decode(error: napi::Error) -> Value {
        serde_json::from_str(&error.reason).expect("errors cross the boundary as JSON")
    }

    #[test]
    fn audio_level_accepts_the_rfc_6464_range_only() {
        assert_eq!(validate_audio_level(None).expect("absent level"), None);
        assert_eq!(validate_audio_level(Some(0)).expect("loudest"), Some(0));
        assert_eq!(validate_audio_level(Some(127)).expect("silence"), Some(127));
        assert!(validate_audio_level(Some(128)).is_err());
        assert!(validate_audio_level(Some(u32::MAX)).is_err());
    }

    #[test]
    fn read_mode_is_owned_for_the_track_lifetime() {
        let owner = ReadModeOwner::default();
        owner.claim(ReadMode::Decoded).expect("choose decoded");
        owner.claim(ReadMode::Decoded).expect("repeat decoded read");

        let error = owner
            .claim(ReadMode::Rtp)
            .expect_err("mode changes must fail");
        let value = decode(error);
        assert_eq!(value["code"], "RTC_ILLEGAL_STATE");
        assert_eq!(value["details"]["activeMode"], "decoded");
        assert_eq!(value["details"]["requestedMode"], "rtp");
    }

    #[test]
    fn video_options_reject_unknown_fields() {
        assert!(
            serde_json::from_str::<VideoOptions>(
                r#"{"targetBitrateBps":500000,"unexpected":true}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<VideoOptions>(
                r#"{"layering":{"mode":"server-managed","unexpected":1}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn video_options_accept_frame_skipping_policy() {
        let options = serde_json::from_str::<VideoOptions>(
            r#"{"targetBitrateBps":3000000,"allowFrameSkipping":false}"#,
        )
        .expect("valid options");

        assert_eq!(options.allow_frame_skipping, Some(false));
    }
}
