//! Pure planning for server-managed simulcast publications.
//!
//! The RID order and bitrate/dimension progression mirror the current Stream
//! web publisher (`q`, `h`, `f`, low to high). The SFU publish option remains
//! authoritative; local limits may only reduce its requested layer count.

use std::num::NonZeroU8;

use super::proto::models::{PublishOption, VideoDimension, VideoLayer, VideoQuality};

pub(crate) const SIMULCAST_RIDS: [&str; 3] = ["q", "h", "f"];

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlannedVideoLayer {
    pub(crate) rid: &'static str,
    pub(crate) dimension: VideoDimension,
    pub(crate) bitrate_bps: u32,
    pub(crate) max_framerate: u32,
    pub(crate) scale_resolution_down_by: f32,
    pub(crate) initially_active: bool,
    pub(crate) quality: VideoQuality,
}

impl PlannedVideoLayer {
    pub(crate) fn as_proto(&self) -> VideoLayer {
        VideoLayer {
            rid: self.rid.to_owned(),
            video_dimension: Some(self.dimension),
            bitrate: self.bitrate_bps,
            fps: self.max_framerate,
            quality: self.quality as i32,
        }
    }
}

pub(crate) fn single_layer(option: &PublishOption) -> PlannedVideoLayer {
    let dimension = option.video_dimension.unwrap_or(VideoDimension {
        width: 1280,
        height: 720,
    });
    PlannedVideoLayer {
        rid: "q",
        dimension,
        bitrate_bps: u32::try_from(option.bitrate).unwrap_or_default(),
        max_framerate: u32::try_from(option.fps).unwrap_or_default(),
        scale_resolution_down_by: 1.0,
        initially_active: true,
        quality: VideoQuality::High,
    }
}

pub(crate) fn simulcast_layers(
    option: &PublishOption,
    local_max_spatial_layers: Option<NonZeroU8>,
) -> Vec<PlannedVideoLayer> {
    let server_count = if option.max_spatial_layers <= 0 {
        3
    } else {
        option.max_spatial_layers.clamp(1, 3) as u8
    };
    let count = local_max_spatial_layers
        .map(NonZeroU8::get)
        .unwrap_or(3)
        .min(server_count)
        .clamp(1, 3);
    let full = option.video_dimension.unwrap_or(VideoDimension {
        width: 1280,
        height: 720,
    });
    let full_bitrate = u32::try_from(option.bitrate).unwrap_or_default();
    let fps = u32::try_from(option.fps).unwrap_or_default();

    (0..count)
        .map(|index| {
            let power = u32::from(count - index - 1);
            let scale = 1_u32 << power;
            let quality = match index {
                0 => VideoQuality::LowUnspecified,
                1 => VideoQuality::Mid,
                _ => VideoQuality::High,
            };
            let fallback = match scale {
                1 => 1_250_000,
                2 => 750_000,
                _ => 300_000,
            };
            PlannedVideoLayer {
                rid: SIMULCAST_RIDS[usize::from(index)],
                dimension: VideoDimension {
                    width: even_dimension(full.width / scale),
                    height: even_dimension(full.height / scale),
                },
                bitrate_bps: full_bitrate
                    .checked_div(scale)
                    .filter(|v| *v > 0)
                    .unwrap_or(fallback),
                max_framerate: fps,
                scale_resolution_down_by: scale as f32,
                initially_active: !option.use_single_layer || index + 1 == count,
                quality,
            }
        })
        .collect()
}

fn even_dimension(value: u32) -> u32 {
    value.max(2) & !1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option() -> PublishOption {
        PublishOption {
            bitrate: 1_200_000,
            fps: 30,
            max_spatial_layers: 3,
            video_dimension: Some(VideoDimension {
                width: 1280,
                height: 720,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn plans_three_simulcast_layers_low_to_high() {
        let layers = simulcast_layers(&option(), None);
        assert_eq!(
            layers.iter().map(|l| l.rid).collect::<Vec<_>>(),
            ["q", "h", "f"]
        );
        assert_eq!(
            layers.iter().map(|l| l.dimension.width).collect::<Vec<_>>(),
            [320, 640, 1280]
        );
        assert_eq!(
            layers.iter().map(|l| l.bitrate_bps).collect::<Vec<_>>(),
            [300_000, 600_000, 1_200_000]
        );
        assert!(layers.iter().all(|l| l.initially_active));
    }

    #[test]
    fn local_limit_reduces_server_layer_count_and_remaps_rids() {
        let layers = simulcast_layers(&option(), NonZeroU8::new(2));
        assert_eq!(layers.iter().map(|l| l.rid).collect::<Vec<_>>(), ["q", "h"]);
        assert_eq!(
            layers.iter().map(|l| l.dimension.width).collect::<Vec<_>>(),
            [640, 1280]
        );
    }

    #[test]
    fn single_layer_hint_keeps_only_highest_encoding_active() {
        let mut option = option();
        option.use_single_layer = true;
        let layers = simulcast_layers(&option, None);
        assert_eq!(
            layers
                .iter()
                .map(|l| l.initially_active)
                .collect::<Vec<_>>(),
            [false, false, true]
        );
    }
}
